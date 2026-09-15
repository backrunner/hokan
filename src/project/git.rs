//! Git repository context for intent-aware `git` completion: where the cwd
//! sits relative to a repository and what the repository's state is. The
//! status probe runs `git status --porcelain` through the bounded platform
//! runner on a background thread and is cached briefly so a burst of
//! keystrokes costs at most one subprocess — and the completion worker
//! never waits on it.

use std::{
    collections::{HashMap, HashSet},
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

/// How long a cached repository context is trusted. Short enough that a
/// commit made in another terminal shows up quickly, long enough that typing
/// a full command never needs a second `git status` probe.
const STATUS_TTL: Duration = Duration::from_millis(2_000);
/// `git status` on a huge or NFS-backed repository must never stall the
/// completion worker; on timeout the context degrades to "unknown".
const STATUS_TIMEOUT: Duration = Duration::from_millis(800);
const STATUS_MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Matches the workspace probe's walk-up bound.
const MAX_WALK_UP: usize = 8;
/// Same TTL as the status cache: a burst of keystrokes costs at most one
/// `for-each-ref` run, while a branch created elsewhere shows up quickly.
const REFS_TTL: Duration = Duration::from_millis(2_000);
const REFS_TIMEOUT: Duration = Duration::from_millis(800);
const REFS_MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// A stale entry is still served while a background probe refreshes it, but
/// only up to this bound: returning to a long-idle session must not reuse
/// repository state captured hours ago.
const STALE_SERVE_MAX: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitContext {
    /// No `.git` found at or above the cwd.
    NotARepository,
    /// Inside a repository but the status probe failed or timed out: the
    /// provider falls back to state-independent rows.
    RepositoryUnknown,
    /// Inside a repository with a fresh status.
    Repository(GitStatus),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitStatus {
    pub branch: Option<String>,
    /// Commits the branch is ahead of its upstream (0 also when there is no
    /// upstream — nothing to push *to*).
    pub ahead: u32,
    pub behind: u32,
    pub has_upstream: bool,
    /// Staged, unstaged, or untracked changes present.
    pub has_changes: bool,
}

#[derive(Debug, Default)]
pub struct GitStatusCache {
    entries: Mutex<HashMap<PathBuf, CacheEntry>>,
    /// Directories with an in-flight probe, so a burst of keystrokes (or a
    /// probe slower than the typing cadence) never stacks up `git status`
    /// subprocesses.
    pending: Mutex<HashSet<PathBuf>>,
}

#[derive(Debug)]
struct CacheEntry {
    at: Instant,
    context: GitContext,
}

enum Cached<T> {
    Fresh(T),
    /// Trusted data past its TTL: still served, but a background refresh is
    /// requested so the next lookup sees current state.
    Stale(T),
}

fn classify<T>(entry: Option<(&Instant, &T)>, ttl: Duration) -> Option<Cached<T>>
where
    T: Clone,
{
    match entry {
        Some((at, value)) if at.elapsed() < ttl => Some(Cached::Fresh(value.clone())),
        Some((at, value)) if at.elapsed() < STALE_SERVE_MAX => Some(Cached::Stale(value.clone())),
        _ => None,
    }
}

impl GitStatusCache {
    /// Non-blocking repository context: fresh entries are returned as-is,
    /// stale entries are served while a background probe refreshes them, and
    /// a cold or long-stale lookup requests a probe and degrades to
    /// `RepositoryUnknown`. A `git status` run must never stall the
    /// completion worker.
    pub fn context_for(self: &Arc<Self>, cwd: &Path) -> GitContext {
        let hit = self.entries.lock().ok().and_then(|entries| {
            classify(
                entries.get(cwd).map(|entry| (&entry.at, &entry.context)),
                STATUS_TTL,
            )
        });
        match hit {
            Some(Cached::Fresh(context)) => context,
            Some(Cached::Stale(context)) => {
                self.request_probe(cwd);
                context
            }
            None => {
                self.request_probe(cwd);
                GitContext::RepositoryUnknown
            }
        }
    }

    /// Queue a background status probe unless one is already in flight.
    /// `GitProvider::applies` calls this so the probe usually lands before
    /// the first `complete` that needs it.
    pub fn request_probe(self: &Arc<Self>, cwd: &Path) {
        {
            let Ok(mut pending) = self.pending.lock() else {
                return;
            };
            if !pending.insert(cwd.to_owned()) {
                return;
            }
        }
        let cache = Arc::clone(self);
        let directory = cwd.to_owned();
        let spawned = thread::Builder::new()
            .name("hokan-git-status".into())
            .spawn(move || {
                // A panicking probe must still clear `pending`, otherwise the
                // directory would never be retried.
                let context = catch_unwind(AssertUnwindSafe(|| probe(&directory)))
                    .unwrap_or(GitContext::RepositoryUnknown);
                if let Ok(mut entries) = cache.entries.lock() {
                    entries.insert(
                        directory.clone(),
                        CacheEntry {
                            at: Instant::now(),
                            context,
                        },
                    );
                }
                if let Ok(mut pending) = cache.pending.lock() {
                    pending.remove(&directory);
                }
            });
        if spawned.is_err()
            && let Ok(mut pending) = self.pending.lock()
        {
            pending.remove(cwd);
        }
    }

    /// Run the probe synchronously — used by tests that need a repository
    /// context on the very first `complete` call.
    #[cfg(test)]
    pub(crate) fn probe_now(&self, cwd: &Path) {
        let context = probe(cwd);
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(
                cwd.to_owned(),
                CacheEntry {
                    at: Instant::now(),
                    context,
                },
            );
        }
    }
}

fn probe(cwd: &Path) -> GitContext {
    let Some(root) = find_repo_root(cwd) else {
        return GitContext::NotARepository;
    };
    let Ok(output) = crate::platform::run_bounded(
        "git",
        [
            "-C",
            root.to_str().unwrap_or("."),
            "status",
            "--porcelain=v1",
            "--branch",
            "--ahead-behind",
        ],
        STATUS_TIMEOUT,
        STATUS_MAX_OUTPUT_BYTES,
    ) else {
        return GitContext::RepositoryUnknown;
    };
    if !output.status.success() {
        return GitContext::RepositoryUnknown;
    }
    GitContext::Repository(parse_status(&String::from_utf8_lossy(&output.stdout)))
}

/// A recent commit for revision slots (`git cherry-pick <hash>` and
/// friends).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommit {
    /// Abbreviated object name — what the user types.
    pub hash: String,
    /// Subject line, shown as the row description.
    pub subject: String,
}

/// Branch/remote/tag listing of a repository, used by ref completion
/// (`git checkout <…>` and friends).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitRefs {
    /// The checked-out local branch, if any (detached HEAD → `None`).
    pub current: Option<String>,
    /// Local branch short names.
    pub locals: Vec<String>,
    /// Remote-tracking refs (`origin/main`), symbolic `*/HEAD` excluded.
    pub remotes: Vec<String>,
    /// Bare remote names derived from the remote-tracking refs (`origin`).
    pub remote_names: Vec<String>,
    pub tags: Vec<String>,
    /// Recent commits across all refs, newest first — for `cherry-pick`,
    /// `revert`, `reset`, `rebase --onto`, and similar revision slots.
    pub commits: Vec<GitCommit>,
}

/// Short-TTL cache of ref listings keyed by repository root, mirroring
/// [`GitStatusCache`]: one bounded `for-each-ref` run per burst of
/// keystrokes, executed on a background thread so the completion worker
/// never waits on it.
#[derive(Debug, Default)]
pub struct GitRefsCache {
    entries: Mutex<HashMap<PathBuf, RefsEntry>>,
    /// Repository roots with an in-flight probe — same dedupe as the status
    /// cache.
    pending: Mutex<HashSet<PathBuf>>,
}

#[derive(Debug)]
struct RefsEntry {
    at: Instant,
    /// `None` is a cached probe failure: a missing or broken `git` must not
    /// respawn `for-each-ref` on every keystroke.
    refs: Option<Arc<GitRefs>>,
}

impl GitRefsCache {
    /// `None` outside a repository, while a cold probe is still in flight,
    /// or when the probe keeps failing — ref completion stays silent rather
    /// than guessing or blocking.
    pub fn refs_for(self: &Arc<Self>, cwd: &Path) -> Option<Arc<GitRefs>> {
        let root = find_repo_root(cwd)?;
        let hit = self.entries.lock().ok().and_then(|entries| {
            classify(
                entries.get(&root).map(|entry| (&entry.at, &entry.refs)),
                REFS_TTL,
            )
        });
        match hit {
            Some(Cached::Fresh(refs)) => refs,
            Some(Cached::Stale(refs)) => {
                self.request_probe(root);
                refs
            }
            None => {
                self.request_probe(root);
                None
            }
        }
    }

    fn request_probe(self: &Arc<Self>, root: PathBuf) {
        {
            let Ok(mut pending) = self.pending.lock() else {
                return;
            };
            if !pending.insert(root.clone()) {
                return;
            }
        }
        let cache = Arc::clone(self);
        let directory = root.clone();
        let spawned = thread::Builder::new()
            .name("hokan-git-refs".into())
            .spawn(move || {
                let refs =
                    catch_unwind(AssertUnwindSafe(|| probe_refs(&directory))).unwrap_or(None);
                if let Ok(mut entries) = cache.entries.lock() {
                    entries.insert(
                        directory.clone(),
                        RefsEntry {
                            at: Instant::now(),
                            refs,
                        },
                    );
                }
                if let Ok(mut pending) = cache.pending.lock() {
                    pending.remove(&directory);
                }
            });
        if spawned.is_err()
            && let Ok(mut pending) = self.pending.lock()
        {
            pending.remove(&root);
        }
    }

    /// Run the probe synchronously — used by tests that need ref rows on
    /// the very first `complete` call.
    #[cfg(test)]
    pub(crate) fn probe_now(&self, cwd: &Path) {
        let Some(root) = find_repo_root(cwd) else {
            return;
        };
        let refs = probe_refs(&root);
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(
                root,
                RefsEntry {
                    at: Instant::now(),
                    refs,
                },
            );
        }
    }
}

fn probe_refs(root: &Path) -> Option<Arc<GitRefs>> {
    let output = crate::platform::run_bounded(
        "git",
        [
            "-C",
            root.to_str().unwrap_or("."),
            "for-each-ref",
            // %(HEAD) marks the checked-out branch, so a single run carries
            // everything ref completion needs.
            "--format=%(HEAD)%09%(refname)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
        REFS_TIMEOUT,
        REFS_MAX_OUTPUT_BYTES,
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut refs = parse_refs(&String::from_utf8_lossy(&output.stdout));
    refs.commits = probe_commits(root);
    Some(Arc::new(refs))
}

/// Recent commits across every ref. `--all` matters for `cherry-pick`: the
/// commit the user wants usually lives on another branch. An unborn HEAD
/// (fresh repository) fails the `log` probe — that is not a ref failure, so
/// commits degrade to an empty list instead of poisoning the whole entry.
fn probe_commits(root: &Path) -> Vec<GitCommit> {
    let Ok(output) = crate::platform::run_bounded(
        "git",
        [
            "-C",
            root.to_str().unwrap_or("."),
            "log",
            "--all",
            "--format=%h%x09%s",
            "--max-count=200",
            "--no-show-signature",
        ],
        REFS_TIMEOUT,
        REFS_MAX_OUTPUT_BYTES,
    ) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_commits(&String::from_utf8_lossy(&output.stdout))
}

fn parse_commits(text: &str) -> Vec<GitCommit> {
    text.lines()
        .filter_map(|line| {
            // Subjects may themselves contain tabs; an empty subject leaves
            // the line as a bare hash, which still completes fine.
            let (hash, subject) = line.split_once('\t').unwrap_or((line, ""));
            (!hash.is_empty()).then(|| GitCommit {
                hash: hash.to_owned(),
                subject: subject.to_owned(),
            })
        })
        .collect()
}

/// Parses `%(HEAD)%09%(refname)` lines into short names grouped by kind.
fn parse_refs(text: &str) -> GitRefs {
    let mut refs = GitRefs::default();
    for line in text.lines() {
        let (head, refname) = line.split_once('\t').unwrap_or((" ", line));
        if let Some(local) = refname.strip_prefix("refs/heads/") {
            if head.trim() == "*" {
                refs.current = Some(local.to_owned());
            }
            refs.locals.push(local.to_owned());
        } else if let Some(remote) = refname.strip_prefix("refs/remotes/") {
            // Symbolic refs such as `origin/HEAD` duplicate the default
            // branch row.
            if remote.ends_with("/HEAD") {
                continue;
            }
            if let Some((name, _)) = remote.split_once('/')
                && !refs.remote_names.iter().any(|known| known == name)
            {
                refs.remote_names.push(name.to_owned());
            }
            refs.remotes.push(remote.to_owned());
        } else if let Some(tag) = refname.strip_prefix("refs/tags/") {
            refs.tags.push(tag.to_owned());
        }
    }
    refs
}

/// Nearest ancestor (or the directory itself) containing `.git` — a
/// directory, or a file for worktrees and submodules.
fn find_repo_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .take(MAX_WALK_UP)
        .find(|directory| fs::symlink_metadata(directory.join(".git")).is_ok())
        .map(Path::to_owned)
}

/// Parses `git status --porcelain=v1 --branch --ahead-behind`. The first
/// `##` line carries the branch and tracking counters; every other line is
/// a changed or untracked path.
fn parse_status(text: &str) -> GitStatus {
    let mut status = GitStatus::default();
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("## ") {
            parse_branch_header(header, &mut status);
        } else if !line.trim().is_empty() {
            status.has_changes = true;
        }
    }
    status
}

fn parse_branch_header(header: &str, status: &mut GitStatus) {
    // "## main...origin/main [ahead 2, behind 1]" — but also
    // "## No commits yet on main" and "## HEAD (no branch)".
    let (branch_part, counters) = match header.split_once(" [") {
        Some((branch, counters)) => (branch, counters.trim_end_matches(']')),
        None => (header, ""),
    };
    let branch = branch_part
        .strip_prefix("No commits yet on ")
        .unwrap_or(branch_part);
    status.branch = match branch.split_once("...") {
        Some((local, upstream)) => {
            status.has_upstream = !upstream.is_empty();
            Some(local.to_owned())
        }
        None => (branch != "HEAD (no branch)").then(|| branch.to_owned()),
    };
    for counter in counters.split(", ") {
        if let Some(ahead) = counter.strip_prefix("ahead ") {
            status.ahead = ahead.parse().unwrap_or(0);
        } else if let Some(behind) = counter.strip_prefix("behind ") {
            status.behind = behind.parse().unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ahead_behind_and_changes() {
        let status = parse_status(
            "## main...origin/main [ahead 2, behind 1]\n M src/lib.rs\n?? notes.txt\n",
        );
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert!(status.has_upstream);
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 1);
        assert!(status.has_changes);
    }

    #[test]
    fn parses_clean_branch_without_upstream() {
        let status = parse_status("## work\n");
        assert_eq!(status.branch.as_deref(), Some("work"));
        assert!(!status.has_upstream);
        assert_eq!(status.ahead, 0);
        assert!(!status.has_changes);
    }

    #[test]
    fn parses_no_commits_yet_and_detached_head() {
        let empty = parse_status("## No commits yet on main\n?? a\n");
        assert_eq!(empty.branch.as_deref(), Some("main"));
        assert!(empty.has_changes);

        let detached = parse_status("## HEAD (no branch)\n");
        assert_eq!(detached.branch, None);
    }

    #[test]
    fn finds_repo_root_upwards_and_reports_plain_directories() {
        let root = tempfile::tempdir().expect("root");
        let repository = root.path().join("repo/src/deep");
        fs::create_dir_all(&repository).expect("nested");
        assert_eq!(find_repo_root(&repository), None);
        fs::create_dir_all(root.path().join("repo/.git")).expect("git dir");
        assert_eq!(
            find_repo_root(&repository).as_deref(),
            Some(root.path().join("repo").as_path())
        );
    }

    #[test]
    fn parses_ref_lines_into_kinds_and_marks_current() {
        let refs = parse_refs(
            "*\trefs/heads/main\n \trefs/heads/feature/mars\n \trefs/remotes/origin/main\n \trefs/remotes/origin/HEAD\n \trefs/tags/v1\n",
        );
        assert_eq!(refs.current.as_deref(), Some("main"));
        assert_eq!(refs.locals, ["main", "feature/mars"]);
        assert_eq!(refs.remotes, ["origin/main"]);
        assert_eq!(refs.remote_names, ["origin"]);
        assert_eq!(refs.tags, ["v1"]);
    }

    #[test]
    fn parses_commit_lines_and_keeps_tabbed_or_empty_subjects() {
        let commits = parse_commits("a1b2c3\tfix login\tagain\nd4e5f6\tplain subject\n789abc\n\n");
        assert_eq!(commits.len(), 3);
        assert_eq!(commits[0].hash, "a1b2c3");
        assert_eq!(commits[0].subject, "fix login\tagain");
        assert_eq!(commits[1].subject, "plain subject");
        assert_eq!(commits[2].hash, "789abc");
        assert_eq!(commits[2].subject, "");
    }

    fn git_available() -> bool {
        crate::platform::run_bounded("git", ["--version"], Duration::from_secs(2), 1024)
            .is_ok_and(|output| output.status.success())
    }

    fn git(directory: &Path, args: &[&str]) {
        // `commit.gpgsign=false`: a developer machine with global signing on
        // must not hang the fixture commit on a pinentry prompt.
        let mut command = vec![
            "-C",
            directory.to_str().expect("utf-8 path"),
            "-c",
            "commit.gpgsign=false",
        ];
        command.extend_from_slice(args);
        let output =
            crate::platform::run_bounded("git", command, Duration::from_secs(10), 1024 * 1024)
                .expect("git run");
        assert!(output.status.success(), "git {args:?} failed");
    }

    #[test]
    fn refs_cache_lists_and_caches_a_real_repository() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().expect("repo");
        git(root.path(), &["init", "-q", "-b", "main"]);
        fs::write(root.path().join("a.txt"), b"a").expect("file");
        git(root.path(), &["add", "-A"]);
        git(
            root.path(),
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ],
        );
        git(root.path(), &["branch", "tmp"]);

        let cache = Arc::new(GitRefsCache::default());
        cache.probe_now(root.path());
        let first = cache.refs_for(root.path()).expect("refs");
        assert_eq!(first.current.as_deref(), Some("main"));
        assert!(first.locals.iter().any(|name| name == "tmp"));

        // Within the TTL a deleted branch is still served from the cache —
        // no second `for-each-ref` run per keystroke burst.
        git(root.path(), &["branch", "-qD", "tmp"]);
        let second = cache.refs_for(root.path()).expect("cached refs");
        assert!(second.locals.iter().any(|name| name == "tmp"));
        assert!(Arc::ptr_eq(&first, &second));

        // Outside a repository there is nothing to offer.
        let plain = tempfile::tempdir().expect("plain");
        assert!(cache.refs_for(plain.path()).is_none());
    }

    #[test]
    fn cold_lookups_never_block_and_are_filled_by_a_background_probe() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().expect("repo");
        git(root.path(), &["init", "-q", "-b", "main"]);
        fs::write(root.path().join("a.txt"), b"a").expect("file");
        git(root.path(), &["add", "-A"]);
        git(
            root.path(),
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ],
        );

        let status = Arc::new(GitStatusCache::default());
        let refs = Arc::new(GitRefsCache::default());

        // Cold caches return immediately with the degraded values instead of
        // running `git` on the completion worker.
        assert_eq!(
            status.context_for(root.path()),
            GitContext::RepositoryUnknown
        );
        assert!(refs.refs_for(root.path()).is_none());

        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(status.context_for(root.path()), GitContext::Repository(_))
            || refs.refs_for(root.path()).is_none()
        {
            assert!(
                Instant::now() < deadline,
                "background probes never filled the caches"
            );
            thread::yield_now();
        }
        assert_eq!(
            refs.refs_for(root.path()).expect("refs").current.as_deref(),
            Some("main")
        );
    }
}
