//! Session cache, background fetch deduplication, and executable invalidation.
use super::{
    CommandHelp,
    probe::{
        fetch_command_help, fetch_command_help_from, fetch_scoped_help_from, help_probe_allowed,
    },
};
use std::{
    collections::HashMap,
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
};
const MAX_CONCURRENT_FETCHES: usize = 4;

/// Fingerprint of the binary a help entry was probed from. Checked whenever
/// an entry is read or replaced so an upgraded executable (same path, new
/// build) invalidates its cached help — not just a PATH resolution change.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ExecutableStamp {
    /// The resolution the entry was probed for; also serves as the cache's
    /// path-matching key so a PATH change still invalidates.
    path: PathBuf,
    /// The file's contents fingerprint at fetch time. `None` when the file
    /// could not be stat'd then — a binary appearing later at the same path
    /// (previously a permanent stale negative entry) now invalidates too.
    file: Option<FileStamp>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileStamp {
    length: u64,
    modified_ns: u128,
}

impl FileStamp {
    fn for_path(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        Some(Self {
            length: metadata.len(),
            modified_ns: metadata
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
        })
    }
}

impl ExecutableStamp {
    fn resolved(path: PathBuf) -> Self {
        Self {
            file: FileStamp::for_path(&path),
            path,
        }
    }

    /// One `stat` to confirm the recorded file is still what was probed —
    /// or still absent when `file` is `None`. Cheap compared to a
    /// `man`/`--help` fetch, so staleness is caught at read time instead of
    /// only when a new request happens to fire.
    fn is_current(&self) -> bool {
        FileStamp::for_path(&self.path) == self.file
    }
}

/// Session-scoped command → parsed help cache. Negative results (man failed,
/// page unparsable, `--help` fallback empty) are cached as empty entries so a
/// missing or slow page costs at most one bounded fetch per command per
/// session. Shared between the help provider (which fetches) and the
/// filesystem provider (which only peeks) so the suppression check never
/// spawns a subprocess.
type CommandHelpEntries = HashMap<String, (Option<ExecutableStamp>, Arc<CommandHelp>)>;

#[derive(Default)]
pub struct CommandHelpCache {
    entries: Mutex<CommandHelpEntries>,
    pending: Mutex<HashMap<String, Option<PathBuf>>>,
    fetches: AtomicUsize,
    revision: AtomicU64,
}

impl CommandHelpCache {
    /// Cached entry only; never runs `man`. Cheap enough for `applies`-time
    /// suppression checks in other providers. Entries stamped from a binary
    /// are revalidated against the file so an in-place upgrade drops the
    /// entry instead of serving stale help for the rest of the session.
    #[must_use]
    pub fn peek(&self, command: &str) -> Option<Arc<CommandHelp>> {
        let mut entries = lock(&self.entries);
        let stale = matches!(
            entries.get(command),
            Some((Some(stamp), _)) if !stamp.is_current()
        );
        if stale {
            entries.remove(command);
            return None;
        }
        entries.get(command).map(|(_, help)| Arc::clone(help))
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn is_pending(&self, command: &str) -> bool {
        lock(&self.pending).contains_key(command)
    }

    pub(super) fn peek_scope(&self, command: &str, scope: &[String]) -> Option<Arc<CommandHelp>> {
        if scope.is_empty() {
            self.peek(command)
        } else {
            self.peek(&scope_cache_key(command, scope))
        }
    }

    pub(super) fn scope_is_pending(&self, command: &str, scope: &[String]) -> bool {
        if scope.is_empty() {
            self.is_pending(command)
        } else {
            self.is_pending(&scope_cache_key(command, scope))
        }
    }

    /// Schedule one background fetch for a cold command. Completion queries
    /// never wait for `man` or `<command> --help`; the populated cache is used
    /// on the next keystroke. Pending and negative results are both deduped.
    pub fn request(self: &Arc<Self>, command: &str, executable: Option<PathBuf>) {
        if !help_probe_allowed(command, executable.as_deref()) {
            return;
        }
        let fetch_path = executable.clone();
        self.request_with_path(command, executable, move |command| {
            fetch_command_help_from(command, fetch_path.as_deref())
        });
    }

    pub(super) fn request_scope(
        self: &Arc<Self>,
        command: &str,
        executable: Option<PathBuf>,
        scope: Vec<String>,
    ) {
        if !help_probe_allowed(command, executable.as_deref()) {
            return;
        }
        if scope.is_empty() {
            self.request(command, executable);
            return;
        }
        let key = scope_cache_key(command, &scope);
        let command = command.to_owned();
        let fetch_path = executable.clone();
        self.request_with_path(&key, executable, move |_| {
            fetch_scoped_help_from(&command, fetch_path.as_deref(), &scope)
        });
    }

    #[cfg(test)]
    pub(crate) fn request_with(
        self: &Arc<Self>,
        command: &str,
        fetch: impl FnOnce(&str) -> CommandHelp + Send + 'static,
    ) {
        self.request_with_path(command, None, fetch);
    }

    pub(crate) fn request_with_path(
        self: &Arc<Self>,
        command: &str,
        executable: Option<PathBuf>,
        fetch: impl FnOnce(&str) -> CommandHelp + Send + 'static,
    ) {
        {
            let mut entries = lock(&self.entries);
            if entries
                .get(command)
                .is_some_and(|entry| entry_satisfies(entry, &executable))
            {
                return;
            }
            entries.remove(command);
        }
        {
            let mut pending = lock(&self.pending);
            if pending.get(command) == Some(&executable) {
                return;
            }
            if !pending.contains_key(command) && pending.len() >= MAX_CONCURRENT_FETCHES {
                return;
            }
            pending.insert(command.to_owned(), executable.clone());
        }
        // Close the small race with a synchronous cache fill between the
        // first lookup and the pending insertion.
        if lock(&self.entries)
            .get(command)
            .is_some_and(|entry| entry_satisfies(entry, &executable))
        {
            let mut pending = lock(&self.pending);
            if pending.get(command) == Some(&executable) {
                pending.remove(command);
            }
            return;
        }

        self.fetches.fetch_add(1, Ordering::Relaxed);
        let cache = Arc::clone(self);
        let command = command.to_owned();
        let pending_command = command.clone();
        let pending_path = executable.clone();
        let spawned = thread::Builder::new()
            .name("hokan-command-help".into())
            .spawn(move || {
                let fetched =
                    catch_unwind(AssertUnwindSafe(|| fetch(&command))).unwrap_or_default();
                let mut pending = lock(&cache.pending);
                let current = pending.get(&command) == Some(&executable);
                let inserted = current && {
                    let mut entries = lock(&cache.entries);
                    if entries
                        .get(&command)
                        .is_some_and(|entry| entry_satisfies(entry, &executable))
                    {
                        false
                    } else {
                        let stamp = executable.clone().map(ExecutableStamp::resolved);
                        entries.insert(command.clone(), (stamp, Arc::new(fetched)));
                        true
                    }
                };
                if current {
                    pending.remove(&command);
                }
                drop(pending);
                if inserted {
                    cache.revision.fetch_add(1, Ordering::Release);
                }
            });
        if spawned.is_err() {
            let mut pending = lock(&self.pending);
            if pending.get(pending_command.as_str()) == Some(&pending_path) {
                pending.remove(pending_command.as_str());
            }
        }
    }

    /// Synchronous cache-first lookup used by focused tests and callers that
    /// explicitly opt into waiting. Interactive completion uses `request`.
    pub fn get(&self, command: &str) -> Arc<CommandHelp> {
        self.get_with(command, fetch_command_help)
    }

    pub(super) fn get_with(
        &self,
        command: &str,
        fetch: impl Fn(&str) -> CommandHelp,
    ) -> Arc<CommandHelp> {
        let mut entries = lock(&self.entries);
        if let Some((_, help)) = entries.get(command) {
            return Arc::clone(help);
        }
        self.fetches.fetch_add(1, Ordering::Relaxed);
        let fetched = Arc::new(fetch(command));
        let fetched = entries
            .entry(command.to_owned())
            .or_insert_with(|| (None, fetched))
            .1
            .clone();
        self.revision.fetch_add(1, Ordering::Release);
        fetched
    }

    #[cfg(test)]
    pub(crate) fn seed(&self, command: &str, help: CommandHelp) {
        lock(&self.entries).insert(command.to_owned(), (None, Arc::new(help)));
        self.revision.fetch_add(1, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn seed_scope(&self, command: &str, scope: &[&str], help: CommandHelp) {
        let scope: Vec<String> = scope.iter().map(|word| (*word).to_owned()).collect();
        lock(&self.entries).insert(scope_cache_key(command, &scope), (None, Arc::new(help)));
        self.revision.fetch_add(1, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn fetch_count(&self) -> usize {
        self.fetches.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn bump_revision(&self) {
        self.revision.fetch_add(1, Ordering::Release);
    }
}

fn scope_cache_key(command: &str, scope: &[String]) -> String {
    let mut key = String::from("\0help-scope");
    key.push('\0');
    key.push_str(command);
    for word in scope {
        key.push('\0');
        key.push_str(word);
    }
    key
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An entry is usable for `requested` when it was probed from the same
/// binary path (or carries no stamp at all — test seeds and explicit `get`
/// fills) and that binary is still on disk unchanged.
fn entry_satisfies(
    entry: &(Option<ExecutableStamp>, Arc<CommandHelp>),
    requested: &Option<PathBuf>,
) -> bool {
    let (stamp, _) = entry;
    (stamp.is_none() || stamp.as_ref().map(|stamp| &stamp.path) == requested.as_ref())
        && stamp.as_ref().is_none_or(ExecutableStamp::is_current)
}
