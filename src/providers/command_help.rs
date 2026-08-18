use std::{
    collections::{HashMap, HashSet},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, SlotKind, TextEdit,
    },
    platform::CommandPathCache,
    providers::argument_progress,
    specs::SpecRegistry,
    terminal::RiskLevel,
};

// `ps`/`ifconfig`-style probe precedent: the `man` binary is a fixed trusted
// program, receives no shell and no user-controlled argv beyond the command
// name (placed after `--`), reads null stdin, and is bounded in time and
// output. macOS `man` always runs the troff pipeline: warm `man -P cat cp`
// measures 120-150 ms here and up to ~550 ms under full-suite test load, so
// a ~150 ms cap negative-caches most cold fetches. 1200 ms stays bounded
// while covering loaded machines; the fetch runs at most once per command
// per session on a background thread, outside the interactive query path.
const MAN_TIMEOUT: Duration = Duration::from_millis(1200);
// `--help` fallback for modern CLIs without a (useful) man page: the binary
// itself is a fixed program resolved on PATH, receives no shell, a single
// literal `--help` argument, null stdin, and is bounded in time and output —
// the same discipline as the `man` probe. 800 ms covers warm `kubectl
// --help`-style runs without letting a cold binary stall the applies pass.
const HELP_TIMEOUT: Duration = Duration::from_millis(800);
const MAN_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_ENTRIES: usize = 200;
const MAX_DESCRIPTION_CHARS: usize = 72;
const MAX_CONCURRENT_FETCHES: usize = 4;
// Nested CLIs are common (`hokan config ai`, `git remote add`, ...). Keep
// recursive probing bounded, but do not stop at an arbitrary three levels.
const MAX_HELP_SCOPE_DEPTH: usize = 8;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandHelp {
    pub flags: Vec<HelpEntry>,
    pub subcommands: Vec<HelpEntry>,
    /// Accepted aliases parsed from command rows. They validate history and
    /// exact input but are not emitted as additional recommendation rows.
    pub subcommand_aliases: Vec<String>,
    /// The root invocation also accepts a free positional argument (for
    /// example Codex/Claude prompts), so an unknown first word is not by
    /// itself proof of an invalid subcommand.
    pub accepts_positionals: bool,
    /// True only when `<command> --help` exposed a parseable, untruncated
    /// Commands section. Man pages and partial parses remain non-exhaustive
    /// because commands such as Git can be extended by aliases or external
    /// helpers.
    pub subcommands_exhaustive: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpEntry {
    pub name: String,
    pub description: String,
    pub takes_value: bool,
}

impl CommandHelp {
    #[must_use]
    pub fn has_subcommands(&self) -> bool {
        !self.subcommands.is_empty()
    }
}

/// Session-scoped command → parsed help cache. Negative results (man failed,
/// page unparsable, `--help` fallback empty) are cached as empty entries so a
/// missing or slow page costs at most one bounded fetch per command per
/// session. Shared between the help provider (which fetches) and the
/// filesystem provider (which only peeks) so the suppression check never
/// spawns a subprocess.
type CommandHelpEntries = HashMap<String, (Option<PathBuf>, Arc<CommandHelp>)>;

#[derive(Default)]
pub struct CommandHelpCache {
    entries: Mutex<CommandHelpEntries>,
    pending: Mutex<HashMap<String, Option<PathBuf>>>,
    fetches: AtomicUsize,
    revision: AtomicU64,
}

impl CommandHelpCache {
    /// Cached entry only; never runs `man`. Cheap enough for `applies`-time
    /// suppression checks in other providers.
    #[must_use]
    pub fn peek(&self, command: &str) -> Option<Arc<CommandHelp>> {
        lock(&self.entries)
            .get(command)
            .map(|(_, help)| Arc::clone(help))
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn is_pending(&self, command: &str) -> bool {
        lock(&self.pending).contains_key(command)
    }

    fn peek_scope(&self, command: &str, scope: &[String]) -> Option<Arc<CommandHelp>> {
        if scope.is_empty() {
            self.peek(command)
        } else {
            self.peek(&scope_cache_key(command, scope))
        }
    }

    fn scope_is_pending(&self, command: &str, scope: &[String]) -> bool {
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

    fn request_scope(
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
                .is_some_and(|(cached_path, _)| cache_path_matches(cached_path, &executable))
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
            .is_some_and(|(cached_path, _)| cache_path_matches(cached_path, &executable))
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
                    if entries.get(&command).is_some_and(|(cached_path, _)| {
                        cache_path_matches(cached_path, &executable)
                    }) {
                        false
                    } else {
                        entries.insert(command.clone(), (executable.clone(), Arc::new(fetched)));
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

    fn get_with(&self, command: &str, fetch: impl Fn(&str) -> CommandHelp) -> Arc<CommandHelp> {
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

fn cache_path_matches(cached: &Option<PathBuf>, requested: &Option<PathBuf>) -> bool {
    cached.is_none() || cached == requested
}

pub struct CommandHelpProvider {
    specs: Arc<SpecRegistry>,
    commands: Arc<CommandPathCache>,
    cache: Arc<CommandHelpCache>,
}

impl CommandHelpProvider {
    #[must_use]
    pub fn new(
        specs: Arc<SpecRegistry>,
        commands: Arc<CommandPathCache>,
        cache: Arc<CommandHelpCache>,
    ) -> Self {
        Self {
            specs,
            commands,
            cache,
        }
    }
}

impl CandidateProvider for CommandHelpProvider {
    fn id(&self) -> &'static str {
        "command_help"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        let Some(command) = context.command() else {
            return false;
        };
        // Specs are curated and win; never run `man` for a command the user
        // cannot execute, and never for the command token itself. Package
        // managers have a dedicated state machine with project-aware scripts;
        // probing their generic help would delay and duplicate those rows.
        if self.specs.get(command).is_some()
            || !crate::providers::effective_command_accepts_external(context)
        {
            return false;
        }
        let executable = crate::providers::resolved_executable_path(context, &self.commands);
        if executable.is_none() {
            return false;
        }
        let request_missing = help_probe_allowed(command, executable.as_deref());
        if argument_progress(context).is_none() && !bare_command_position(context, command) {
            return false;
        }
        !matches!(
            lookup_help_scope(context, &self.cache, command, executable, request_missing,),
            HelpLookup::None
        )
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Some(command) = context.command() else {
            return ProviderOutput::default();
        };
        let executable = crate::providers::resolved_executable_path(context, &self.commands);
        let HelpLookup::Ready(target) =
            lookup_help_scope(context, &self.cache, command, executable, false)
        else {
            return ProviderOutput::default();
        };
        let help = target.help;
        let position = target.position;
        if let HelpPosition::Values(flag_index) = position {
            return complete_help_values(context, command, &target.scope, &help, flag_index);
        }
        let flags_position = position == HelpPosition::Flags;
        let bare_subcommands = position == HelpPosition::BareSubcommands;
        let entries = if flags_position {
            &help.flags
        } else {
            &help.subcommands
        };
        let query = if bare_subcommands {
            ""
        } else {
            context.parsed.current_prefix.as_str()
        };
        let folded_query = query.to_lowercase();
        let exact = entries.iter().any(|entry| entry.name == query)
            || (!flags_position && help.subcommand_aliases.iter().any(|alias| alias == query));
        if exact {
            return ProviderOutput::default();
        }
        let candidates = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                query.is_empty() || entry.name.to_lowercase().starts_with(&folded_query)
            })
            .map(|(index, entry)| {
                let replacement = if bare_subcommands {
                    format!("{command} {}", entry.name)
                } else {
                    entry.name.clone()
                };
                let display = crate::parser::apply_edit(
                    &context.buffer.text,
                    context.parsed.replacement.clone(),
                    &replacement,
                )
                .map(|result| result.trim_end().to_owned())
                .unwrap_or_else(|_| format!("{command} {}", entry.name));
                let mut candidate = Candidate::new(
                    context.query_id,
                    display,
                    entry.description.as_str(),
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement,
                        cursor_after: CursorPlacement::End,
                    }),
                    if flags_position {
                        CandidateAction::Insert
                    } else {
                        CandidateAction::InsertAndContinue {
                            next_slot: SlotKind::Path,
                        }
                    },
                    CandidateSource::CommandHelp,
                    CandidateKind::Command,
                    // Man-derived rows are never auto-executed: Enter degrades
                    // to a fill, exactly like an incomplete spec recipe.
                    Completeness::NeedsInput {
                        slot: if flags_position {
                            SlotKind::Value
                        } else {
                            SlotKind::Path
                        },
                    },
                    RiskLevel::Low,
                    if target.scope.is_empty() {
                        format!("help:{command}")
                    } else {
                        format!("help:{command}:{}", target.scope.join(" "))
                    },
                );
                // CLI authors generally put the most useful commands and
                // flags first. Preserve that signal so short, obscure man-page
                // entries cannot outrank the documented common workflow.
                candidate.score.spec_priority =
                    i16::try_from(MAX_ENTRIES.saturating_sub(index)).unwrap_or_default();
                candidate
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

fn complete_help_values(
    context: &CompletionContext,
    command: &str,
    scope: &[String],
    help: &CommandHelp,
    flag_index: usize,
) -> ProviderOutput {
    let Some(entry) = help.flags.get(flag_index) else {
        return ProviderOutput::default();
    };
    let choices = documented_value_choices(&entry.description);
    if choices.is_empty() {
        return ProviderOutput::default();
    }

    let prefix = context.parsed.current_prefix.as_str();
    let mut edit_prefix = String::new();
    let mut query = prefix;
    if let Some(rest) = prefix.strip_prefix(&entry.name) {
        if let Some(value) = rest.strip_prefix('=') {
            edit_prefix = format!("{}=", entry.name);
            query = value;
        } else if entry.name.len() == 2 && !rest.is_empty() {
            edit_prefix = entry.name.clone();
            query = rest;
        }
    }
    let folded_query = query.to_ascii_lowercase();
    if choices
        .iter()
        .any(|choice| choice.eq_ignore_ascii_case(query))
    {
        return ProviderOutput::default();
    }

    let candidates = choices
        .into_iter()
        .enumerate()
        .filter(|(_, choice)| {
            folded_query.is_empty() || choice.to_ascii_lowercase().starts_with(&folded_query)
        })
        .map(|(index, choice)| {
            let replacement = format!("{edit_prefix}{choice}");
            let display = crate::parser::apply_edit(
                &context.buffer.text,
                context.parsed.replacement.clone(),
                &replacement,
            )
            .map(|result| result.trim_end().to_owned())
            .unwrap_or_else(|_| format!("{command} {replacement}"));
            let scope = if scope.is_empty() {
                String::new()
            } else {
                format!(":{}", scope.join(" "))
            };
            let mut candidate = Candidate::new(
                context.query_id,
                display,
                format!("{} 的文档可选值", entry.name),
                Some(TextEdit {
                    range: context.parsed.replacement.clone(),
                    replacement,
                    cursor_after: CursorPlacement::End,
                }),
                CandidateAction::Insert,
                CandidateSource::CommandHelp,
                CandidateKind::Recipe,
                Completeness::NeedsInput {
                    slot: SlotKind::Value,
                },
                RiskLevel::Low,
                format!("help:{command}{scope}:{}:{choice}", entry.name),
            );
            candidate.score.spec_priority =
                i16::try_from(MAX_ENTRIES.saturating_sub(index)).unwrap_or_default();
            candidate
        })
        .collect();
    ProviderOutput {
        candidates,
        diagnostics: Vec::new(),
    }
}

fn documented_value_choices(description: &str) -> Vec<String> {
    let folded = description.to_ascii_lowercase();
    let Some((start, marker_len)) = [
        "valid values are:",
        "possible values:",
        "valid values:",
        "values are:",
        "values:",
    ]
    .iter()
    .filter_map(|marker| folded.find(marker).map(|start| (start, marker.len())))
    .min_by_key(|(start, _)| *start) else {
        return Vec::new();
    };
    let tail = description[start + marker_len..].trim_start();
    let end = tail.find([']', ')', ';']).unwrap_or(tail.len());
    let list = tail[..end].trim();
    if !list.contains([',', '|']) {
        return Vec::new();
    }

    let mut choices = Vec::new();
    for raw in list.split([',', '|']) {
        let choice = raw
            .trim()
            .trim_matches(['`', '\'', '"', '<', '>', '[', ']', '(', ')']);
        if choice.is_empty()
            || choice.len() > 64
            || choice.contains(char::is_whitespace)
            || !choice.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || matches!(character, '-' | '_' | '+' | '.' | '/' | ':')
            })
        {
            continue;
        }
        if !choices.iter().any(|existing: &String| existing == choice) {
            choices.push(choice.to_owned());
        }
    }
    if choices.len() >= 2 {
        choices
    } else {
        Vec::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HelpPosition {
    Flags,
    Subcommands,
    BareSubcommands,
    Values(usize),
}

struct HelpTarget {
    help: Arc<CommandHelp>,
    position: HelpPosition,
    scope: Vec<String>,
}

enum HelpLookup {
    Ready(HelpTarget),
    Pending,
    None,
}

enum CompletedScope<'a> {
    Subcommand { word: &'a str, consumed: usize },
    None,
    Blocked,
}

fn lookup_help_scope(
    context: &CompletionContext,
    cache: &Arc<CommandHelpCache>,
    command: &str,
    executable: Option<PathBuf>,
    request_missing: bool,
) -> HelpLookup {
    let Some(mut help) = cache.peek(command) else {
        if request_missing {
            cache.request(command, executable);
            return HelpLookup::Pending;
        }
        return HelpLookup::None;
    };
    let Some((words, position)) = argument_progress(context) else {
        return (help.has_subcommands() && bare_command_position(context, command))
            .then_some(HelpTarget {
                help,
                position: HelpPosition::BareSubcommands,
                scope: Vec::new(),
            })
            .map_or(HelpLookup::None, HelpLookup::Ready);
    };
    let before = words.get(1..=position).unwrap_or_default();
    let mut scope = Vec::new();
    let mut consumed = 0;
    loop {
        match completed_scope(command, scope.is_empty(), &help, &before[consumed..]) {
            CompletedScope::Subcommand {
                word,
                consumed: scope_consumed,
            } => {
                if !supports_scoped_help(command) || scope.len() >= MAX_HELP_SCOPE_DEPTH {
                    return HelpLookup::None;
                }
                consumed += scope_consumed;
                scope.push(word.to_owned());
                if let Some(scoped) = cache.peek_scope(command, &scope) {
                    help = scoped;
                    continue;
                }
                if request_missing {
                    cache.request_scope(command, executable, scope);
                    return HelpLookup::Pending;
                }
                return if cache.scope_is_pending(command, &scope) {
                    HelpLookup::Pending
                } else {
                    HelpLookup::None
                };
            }
            CompletedScope::None => {
                let Some(position) = help_position_for_arguments(
                    command,
                    scope.is_empty(),
                    &help,
                    &before[consumed..],
                    &context.parsed.current_prefix,
                ) else {
                    return HelpLookup::None;
                };
                return HelpLookup::Ready(HelpTarget {
                    help,
                    position,
                    scope,
                });
            }
            CompletedScope::Blocked => return HelpLookup::None,
        }
    }
}

fn completed_scope<'a>(
    command: &str,
    allow_toolchain_selector: bool,
    help: &CommandHelp,
    before: &'a [&'a str],
) -> CompletedScope<'a> {
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if word == "--" {
            return CompletedScope::Blocked;
        }
        if allow_toolchain_selector && toolchain_selector(command, word) {
            index += 1;
            continue;
        }
        if word.starts_with('-') && word != "-" {
            let Some((entry, attached_value)) = help_flag_usage(help, word) else {
                return CompletedScope::Blocked;
            };
            index += 1;
            if entry.takes_value && !attached_value {
                if index >= before.len() {
                    return CompletedScope::None;
                }
                index += 1;
            }
            continue;
        }
        if help.subcommands.iter().any(|entry| entry.name == word)
            || help.subcommand_aliases.iter().any(|alias| alias == word)
        {
            return CompletedScope::Subcommand {
                word,
                consumed: index + 1,
            };
        }
        return CompletedScope::Blocked;
    }
    CompletedScope::None
}

fn help_position_for_arguments(
    command: &str,
    allow_toolchain_selector: bool,
    help: &CommandHelp,
    before: &[&str],
    current_prefix: &str,
) -> Option<HelpPosition> {
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if word == "--" {
            return None;
        }
        if allow_toolchain_selector && toolchain_selector(command, word) {
            index += 1;
            continue;
        }
        if word.starts_with('-') && word != "-" {
            let (entry_index, attached_value) = help_flag_usage_index(help, word)?;
            let entry = &help.flags[entry_index];
            index += 1;
            if entry.takes_value && !attached_value {
                if index >= before.len() {
                    return Some(HelpPosition::Values(entry_index));
                }
                index += 1;
            }
            continue;
        }
        return None;
    }

    if current_prefix.starts_with('-')
        && let Some((entry_index, attached_value)) = help_flag_usage_index(help, current_prefix)
        && attached_value
    {
        return Some(HelpPosition::Values(entry_index));
    }
    if current_prefix.starts_with('-') && !current_prefix.contains('=') {
        Some(HelpPosition::Flags)
    } else if !current_prefix.starts_with('-') {
        Some(HelpPosition::Subcommands)
    } else {
        None
    }
}

fn toolchain_selector(command: &str, word: &str) -> bool {
    matches!(
        command_basename(command),
        "cargo" | "rustc" | "rustdoc" | "rustup"
    ) && word
        .strip_prefix('+')
        .is_some_and(|selector| !selector.is_empty() && !selector.contains('/'))
}

fn bare_command_position(context: &CompletionContext, command: &str) -> bool {
    (crate::providers::command_position_open(context)
        || crate::providers::explicit_executable_path_position(context))
        && context.parsed.current_prefix == command
}

pub(crate) fn dynamic_help_owns_position(
    context: &CompletionContext,
    cache: &Arc<CommandHelpCache>,
) -> bool {
    let Some(command) = context.command() else {
        return false;
    };
    match lookup_help_scope(context, cache, command, None, false) {
        HelpLookup::Ready(target) => match target.position {
            HelpPosition::Flags | HelpPosition::Values(_) => true,
            HelpPosition::Subcommands | HelpPosition::BareSubcommands => {
                target.help.has_subcommands()
                    && (!hybrid_subcommand_path_command(command, &target.scope)
                        || context.parsed.current_prefix.is_empty()
                        || target.help.subcommands.iter().any(|entry| {
                            entry
                                .name
                                .to_ascii_lowercase()
                                .starts_with(&context.parsed.current_prefix.to_ascii_lowercase())
                        }))
            }
        },
        HelpLookup::Pending => true,
        HelpLookup::None => false,
    }
}

fn hybrid_subcommand_path_command(command: &str, scope: &[String]) -> bool {
    scope.is_empty() && command_basename(command) == "swift"
}

fn supports_scoped_help(command: &str) -> bool {
    super::is_pip_command(command)
        || matches!(
            command_basename(command),
            "ansible"
                | "apt"
                | "aws"
                | "az"
                | "brew"
                | "cargo"
                | "claude"
                | "codex"
                | "composer"
                | "conan"
                | "consul"
                | "diskutil"
                | "dnf"
                | "docker"
                | "docker-compose"
                | "dotnet"
                | "eksctl"
                | "firebase"
                | "flyctl"
                | "gcloud"
                | "gem"
                | "git"
                | "gh"
                | "glab"
                | "go"
                | "helm"
                | "heroku"
                | "hokan"
                | "istioctl"
                | "kubectl"
                | "launchctl"
                | "mise"
                | "meson"
                | "nerdctl"
                | "nix"
                | "nomad"
                | "oc"
                | "ollama"
                | "openssl"
                | "pacman"
                | "pip"
                | "pip3"
                | "pipx"
                | "pnpm"
                | "podman"
                | "poetry"
                | "railway"
                | "rustup"
                | "security"
                | "snap"
                | "svn"
                | "swift"
                | "systemctl"
                | "terraform"
                | "tofu"
                | "uv"
                | "vagrant"
                | "vault"
                | "vcpkg"
                | "vercel"
                | "wrangler"
                | "npm"
                | "yarn"
                | "bun"
                | "deno"
        )
}

fn help_flag_usage<'a>(help: &'a CommandHelp, word: &str) -> Option<(&'a HelpEntry, bool)> {
    let (index, attached) = help_flag_usage_index(help, word)?;
    Some((&help.flags[index], attached))
}

fn help_flag_usage_index(help: &CommandHelp, word: &str) -> Option<(usize, bool)> {
    if let Some((index, _)) = help
        .flags
        .iter()
        .enumerate()
        .find(|(_, entry)| entry.name == word)
    {
        return Some((index, false));
    }
    if let Some((name, _)) = word.split_once('=') {
        return help
            .flags
            .iter()
            .enumerate()
            .find(|(_, entry)| entry.name == name)
            .map(|(index, _)| (index, true));
    }
    help.flags
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.takes_value && entry.name.len() == 2)
        .find(|(_, entry)| word.len() > entry.name.len() && word.starts_with(&entry.name))
        .map(|(index, _)| (index, true))
}

/// Validate the top-level argument portion of a recorded invocation against
/// cached command help. A parseable `--help` Commands section is closed only
/// when the root has no free positional and the CLI is not extensible. Hybrid
/// prompt CLIs and man-derived lists reject spelling-near misses but preserve
/// unknown positional text and proven external command extensions.
pub(crate) fn history_arguments_are_plausible(
    help: &CommandHelp,
    arguments: &[&str],
    known_non_failure: bool,
    allows_external_subcommands: bool,
) -> bool {
    if help.subcommands.is_empty() && help.flags.is_empty() {
        return true;
    }

    let mut index = 0;
    while let Some(word) = arguments.get(index).copied() {
        if has_dynamic_shell_syntax(word) || word == "--" {
            return true;
        }
        if word.starts_with('-') && word != "-" {
            let Some((entry, attached_value)) = help_flag_usage(help, word) else {
                // An unknown flag may itself consume the following word. Do
                // not guess that the next token is a subcommand, but reject
                // an obvious misspelling of a documented top-level flag.
                let name = word.split_once('=').map_or(word, |(name, _)| name);
                return known_non_failure
                    || !help
                        .flags
                        .iter()
                        .any(|entry| one_edit_or_adjacent_transposition(name, &entry.name));
            };
            index += 1;
            if entry.takes_value && !attached_value {
                if index >= arguments.len() {
                    return known_non_failure;
                }
                index += 1;
            }
            continue;
        }

        if help.subcommands.iter().any(|entry| entry.name == word)
            || help.subcommand_aliases.iter().any(|alias| alias == word)
        {
            return true;
        }
        if help.subcommands_exhaustive && !help.accepts_positionals && !allows_external_subcommands
        {
            return known_non_failure;
        }
        if known_non_failure && !help.accepts_positionals {
            return true;
        }
        return !help_subcommand_typo(help, word);
    }
    true
}

/// Validate recorded arguments through each confirmed subcommand scope. A
/// missing scoped help entry is requested asynchronously and returns `None`,
/// allowing history to stay hidden until it can be checked instead of
/// flashing a likely typo for one completion frame.
pub(crate) fn scoped_history_arguments_are_plausible(
    cache: &Arc<CommandHelpCache>,
    command: &str,
    executable: Option<PathBuf>,
    root: Arc<CommandHelp>,
    arguments: &[&str],
    known_non_failure: bool,
    allows_external_subcommands: bool,
) -> Option<bool> {
    let mut help = root;
    let mut remaining = arguments;
    let mut scope = Vec::new();
    loop {
        match history_help_step(
            &help,
            remaining,
            known_non_failure,
            scope.is_empty() && allows_external_subcommands,
        ) {
            HistoryHelpStep::Done(plausible) => return Some(plausible),
            HistoryHelpStep::Subcommand { word, consumed } => {
                remaining = &remaining[consumed..];
                if remaining.is_empty() || known_non_failure {
                    return Some(true);
                }
                if !supports_scoped_help(command) || scope.len() >= MAX_HELP_SCOPE_DEPTH {
                    return Some(true);
                }
                scope.push(word.to_owned());
                if let Some(scoped) = cache.peek_scope(command, &scope) {
                    help = scoped;
                    continue;
                }
                if !cache.scope_is_pending(command, &scope) {
                    if !help_probe_allowed(command, executable.as_deref()) {
                        return Some(true);
                    }
                    cache.request_scope(command, executable.clone(), scope.clone());
                }
                return None;
            }
        }
    }
}

enum HistoryHelpStep<'a> {
    Subcommand { word: &'a str, consumed: usize },
    Done(bool),
}

fn history_help_step<'a>(
    help: &CommandHelp,
    arguments: &'a [&'a str],
    known_non_failure: bool,
    allows_external_subcommands: bool,
) -> HistoryHelpStep<'a> {
    if help.subcommands.is_empty() && help.flags.is_empty() {
        return HistoryHelpStep::Done(true);
    }

    let mut index = 0;
    while let Some(word) = arguments.get(index).copied() {
        if has_dynamic_shell_syntax(word) || word == "--" {
            return HistoryHelpStep::Done(true);
        }
        if word.starts_with('-') && word != "-" {
            let Some((entry, attached_value)) = help_flag_usage(help, word) else {
                let name = word.split_once('=').map_or(word, |(name, _)| name);
                return HistoryHelpStep::Done(
                    known_non_failure
                        || !help
                            .flags
                            .iter()
                            .any(|entry| one_edit_or_adjacent_transposition(name, &entry.name)),
                );
            };
            index += 1;
            if entry.takes_value && !attached_value {
                if index >= arguments.len() {
                    return HistoryHelpStep::Done(known_non_failure);
                }
                index += 1;
            }
            continue;
        }

        if help.subcommands.iter().any(|entry| entry.name == word)
            || help.subcommand_aliases.iter().any(|alias| alias == word)
        {
            return HistoryHelpStep::Subcommand {
                word,
                consumed: index + 1,
            };
        }
        if help.subcommands_exhaustive && !help.accepts_positionals && !allows_external_subcommands
        {
            return HistoryHelpStep::Done(known_non_failure);
        }
        if known_non_failure && !help.accepts_positionals {
            return HistoryHelpStep::Done(true);
        }
        return HistoryHelpStep::Done(!help_subcommand_typo(help, word));
    }
    HistoryHelpStep::Done(true)
}

fn help_subcommand_typo(help: &CommandHelp, word: &str) -> bool {
    help.subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .chain(help.subcommand_aliases.iter().map(String::as_str))
        .any(|name| {
            one_edit_or_adjacent_transposition(word, name)
                || common_subcommand_variant(name).is_some_and(|variant| {
                    word.eq_ignore_ascii_case(variant)
                        || one_edit_or_adjacent_transposition(word, variant)
                })
        })
}

fn common_subcommand_variant(command: &str) -> Option<&'static str> {
    match command {
        "update" => Some("upgrade"),
        "upgrade" => Some("update"),
        _ => None,
    }
}

fn has_dynamic_shell_syntax(word: &str) -> bool {
    word.chars()
        .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
}

pub(crate) fn one_edit_or_adjacent_transposition(left: &str, right: &str) -> bool {
    if left == right {
        return false;
    }
    let left: Vec<char> = left.chars().flat_map(char::to_lowercase).collect();
    let right: Vec<char> = right.chars().flat_map(char::to_lowercase).collect();
    if left.len().abs_diff(right.len()) > 1 {
        return false;
    }
    if left.len() == right.len() {
        let differences: Vec<usize> = left
            .iter()
            .zip(&right)
            .enumerate()
            .filter_map(|(index, (left, right))| (left != right).then_some(index))
            .collect();
        return differences.len() == 1
            || (differences.len() == 2
                && differences[1] == differences[0] + 1
                && left[differences[0]] == right[differences[1]]
                && left[differences[1]] == right[differences[0]]);
    }

    let (shorter, longer) = if left.len() < right.len() {
        (&left, &right)
    } else {
        (&right, &left)
    };
    let mut short_index = 0;
    let mut long_index = 0;
    let mut skipped = false;
    while short_index < shorter.len() && long_index < longer.len() {
        if shorter[short_index] == longer[long_index] {
            short_index += 1;
            long_index += 1;
        } else if skipped {
            return false;
        } else {
            skipped = true;
            long_index += 1;
        }
    }
    true
}

fn command_basename(command: &str) -> &str {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
}

fn help_probe_allowed(command: &str, executable: Option<&Path>) -> bool {
    executable.is_some()
        && !command.contains('/')
        // Wrapper scripts can download runtimes and execute project-owned
        // bootstrap logic even for `--help`. Their static completion surfaces
        // are handled without launching the wrapper.
        && !matches!(command_basename(command), "gradlew" | "mvnw")
}

fn fetch_man_page(command: &str) -> CommandHelp {
    let Ok(output) = crate::platform::run_bounded(
        "man",
        ["-P", "cat", "--", command],
        MAN_TIMEOUT,
        MAN_MAX_OUTPUT_BYTES,
    ) else {
        return CommandHelp::default();
    };
    if !output.status.success() || output.stdout.is_empty() {
        return CommandHelp::default();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_man_page(command, &text)
}

/// Full cold-miss fetch: try the man page, and when it yields no subcommands
/// (kubectl has no man page at all; some pages document only flags) merge a
/// single bounded `<cmd> --help` run. Homebrew is also augmented because its
/// man page omits common dispatcher commands that `brew --help` exposes.
fn fetch_command_help(command: &str) -> CommandHelp {
    let help_command = command_basename(command);
    fetch_with_fallback(help_command, fetch_man_page, |_| fetch_help_output(command))
}

fn fetch_command_help_from(command: &str, executable: Option<&Path>) -> CommandHelp {
    let help_command = command_basename(command);
    let parsed = fetch_man_page(help_command);
    if !parsed.subcommands.is_empty() && !help_augments_man(help_command) {
        return parsed;
    }
    let fallback = executable.map_or_else(
        || fetch_help_output(command),
        |path| fetch_help_program(help_command, path.as_os_str()),
    );
    merge_help_sources(help_command, parsed, fallback)
}

fn fetch_scoped_help_from(
    command: &str,
    executable: Option<&Path>,
    scope: &[String],
) -> CommandHelp {
    let help_command = command_basename(command);
    executable.map_or_else(
        || fetch_help_program_for_scope(help_command, std::ffi::OsStr::new(command), scope),
        |path| fetch_help_program_for_scope(help_command, path.as_os_str(), scope),
    )
}

fn fetch_with_fallback(
    command: &str,
    man: impl Fn(&str) -> CommandHelp,
    help: impl Fn(&str) -> CommandHelp,
) -> CommandHelp {
    let parsed = man(command);
    if !parsed.subcommands.is_empty() && !help_augments_man(command) {
        return parsed;
    }
    merge_help_sources(command, parsed, help(command))
}

fn help_augments_man(command: &str) -> bool {
    // Homebrew's generated man page exposes a broad command surface but omits
    // a few common dispatcher commands such as `update`; `brew --help` lists
    // those canonical invocations. The two sources are complementary.
    matches!(command, "brew")
}

fn merge_help_sources(command: &str, man: CommandHelp, help: CommandHelp) -> CommandHelp {
    if help_augments_man(command) {
        // Homebrew's concise help output is explicitly ordered around common
        // workflows; keep it first and append the broader man-page surface.
        merge_help(help, man)
    } else {
        merge_help(man, help)
    }
}

fn merge_help(mut primary: CommandHelp, fallback: CommandHelp) -> CommandHelp {
    let mut seen_flags: HashSet<String> = primary
        .flags
        .iter()
        .map(|entry| entry.name.clone())
        .collect();
    for flag in fallback.flags {
        if primary.flags.len() >= MAX_ENTRIES {
            break;
        }
        if seen_flags.insert(flag.name.clone()) {
            primary.flags.push(flag);
        }
    }
    let mut seen_subcommands: HashSet<String> = primary
        .subcommands
        .iter()
        .map(|entry| entry.name.clone())
        .chain(primary.subcommand_aliases.iter().cloned())
        .collect();
    let mut truncated = false;
    for subcommand in fallback.subcommands {
        if !seen_subcommands.insert(subcommand.name.clone()) {
            continue;
        }
        if primary.subcommands.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        primary.subcommands.push(subcommand);
    }
    for alias in fallback.subcommand_aliases {
        if !seen_subcommands.insert(alias.clone()) {
            continue;
        }
        if primary.subcommand_aliases.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        primary.subcommand_aliases.push(alias);
    }
    primary.subcommands_exhaustive =
        (primary.subcommands_exhaustive || fallback.subcommands_exhaustive) && !truncated;
    primary.accepts_positionals |= fallback.accepts_positionals;
    primary
}

/// Modern-CLI fallback: `<cmd> --help`, bounded exactly like the man probe —
/// the command resolves on PATH (the provider only fires for commands the
/// user can execute), gets no shell and no user-controlled argv beyond the
/// literal `--help`, reads null stdin, and dies on timeout. A failing,
/// hanging, or empty run degrades to an empty `CommandHelp`.
fn fetch_help_output(command: &str) -> CommandHelp {
    fetch_help_program(command_basename(command), std::ffi::OsStr::new(command))
}

fn fetch_help_program(command: &str, program: &std::ffi::OsStr) -> CommandHelp {
    fetch_help_program_for_scope(command, program, &[])
}

fn fetch_help_program_for_scope(
    command: &str,
    program: &std::ffi::OsStr,
    scope: &[String],
) -> CommandHelp {
    for arguments in help_probe_arguments(command, scope) {
        let Ok(output) =
            crate::platform::run_bounded(program, &arguments, HELP_TIMEOUT, MAN_MAX_OUTPUT_BYTES)
        else {
            continue;
        };
        let mut parsed = CommandHelp::default();
        for bytes in [&output.stdout, &output.stderr] {
            if bytes.is_empty() {
                continue;
            }
            let text = String::from_utf8_lossy(bytes);
            if output.status.success() || looks_like_help_output(&text) {
                parsed = merge_help(parsed, parse_help_output_for_scope(command, scope, &text));
            }
        }
        if !parsed.flags.is_empty() || !parsed.subcommands.is_empty() {
            return parsed;
        }
    }
    CommandHelp::default()
}

fn help_probe_arguments(command: &str, scope: &[String]) -> Vec<Vec<String>> {
    let command = command_basename(command);
    let prefixed_help = || {
        let mut arguments = Vec::with_capacity(scope.len() + 1);
        arguments.push("help".to_owned());
        arguments.extend(scope.iter().cloned());
        arguments
    };
    let suffixed = |flag: &str| {
        let mut arguments = Vec::with_capacity(scope.len() + 1);
        arguments.extend(scope.iter().cloned());
        arguments.push(flag.to_owned());
        arguments
    };
    match command {
        "defaults" | "diskutil" | "go" | "launchctl" | "openssl" | "security" => {
            vec![prefixed_help()]
        }
        "brew" | "gem" | "svn" if !scope.is_empty() => vec![prefixed_help()],
        "swift" | "vcpkg" if !scope.is_empty() => {
            vec![prefixed_help(), suffixed("--help")]
        }
        "pnpm" if scope.is_empty() => {
            vec![vec!["help".to_owned(), "-a".to_owned()], suffixed("--help")]
        }
        "git" if !scope.is_empty() => vec![suffixed("-h")],
        "terraform" | "tofu" if !scope.is_empty() => {
            vec![suffixed("-help"), suffixed("--help")]
        }
        "ffmpeg" | "ffplay" | "ffprobe" | "perl" | "tmux" if scope.is_empty() => {
            vec![vec!["-h".to_owned()]]
        }
        "networksetup" | "sqlite3" | "xcodebuild" if scope.is_empty() => {
            vec![vec!["-help".to_owned()]]
        }
        _ => vec![suffixed("--help")],
    }
}

fn looks_like_help_output(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        line.starts_with("usage:")
            || line == "usage"
            || line.starts_with("commands:")
            || line.starts_with("available commands:")
            || line.starts_with("subcommands:")
            || line == "options:"
            || line == "flags:"
            || line == "standard commands"
            || line == "the commands are:"
    })
}

/// Conservative heuristics over `--help` text (kubectl/docker/cobra, cargo
/// clap, and similar two-column layouts). Same philosophy as the man parser:
/// skip anything unrecognized rather than guess.
#[cfg(test)]
fn parse_help_output(command: &str, text: &str) -> CommandHelp {
    parse_help_output_for_scope(command, &[], text)
}

pub(crate) fn parse_help_output_for_scope(
    command: &str,
    scope: &[String],
    text: &str,
) -> CommandHelp {
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut help = CommandHelp::default();
    let mut seen_flags = HashSet::new();
    let mut seen_subcommands = HashSet::new();
    let mut in_commands = false;
    let mut in_command_grid = false;
    let mut in_argument_choices = false;
    let mut in_invocations = false;
    let mut in_usage = false;
    let mut command_row_indent = None;
    let mut saw_commands = false;
    let mut truncated = false;
    for (index, line) in lines.iter().enumerate() {
        if let Some(signature) = usage_signature(line) {
            in_commands = false;
            in_command_grid = false;
            in_argument_choices = false;
            in_invocations = false;
            in_usage = true;
            command_row_indent = None;
            help.accepts_positionals |= usage_has_free_positional(command, scope, signature);
            if let Some((name, description)) = help_invocation_subcommand(command, scope, signature)
            {
                push_subcommand(
                    &mut help,
                    &mut seen_subcommands,
                    name,
                    description,
                    &mut truncated,
                );
            }
            continue;
        }
        if in_usage {
            help.accepts_positionals |= usage_has_free_positional(command, scope, line);
            if let Some((name, description)) = help_invocation_subcommand(command, scope, line) {
                push_subcommand(
                    &mut help,
                    &mut seen_subcommands,
                    name,
                    description,
                    &mut truncated,
                );
                continue;
            }
            if !line.trim().is_empty() && !line.starts_with(char::is_whitespace) {
                in_usage = false;
            }
        }
        if let Some(standard_commands) = openssl_command_group(command, line) {
            in_commands = standard_commands;
            in_command_grid = standard_commands;
            in_argument_choices = false;
            in_invocations = false;
            command_row_indent = None;
            saw_commands |= standard_commands;
            continue;
        }
        if is_commands_header(command, line) {
            in_commands = true;
            in_command_grid = false;
            in_argument_choices = false;
            in_invocations = false;
            command_row_indent = None;
            saw_commands = true;
            continue;
        }
        if is_invocation_section_header(line) {
            in_commands = false;
            in_command_grid = false;
            in_argument_choices = false;
            in_invocations = true;
            command_row_indent = None;
            continue;
        }
        if is_arguments_header(line) {
            in_commands = false;
            in_command_grid = false;
            in_argument_choices = true;
            in_invocations = false;
            command_row_indent = None;
            help.accepts_positionals = true;
            continue;
        }
        if is_help_section_header(line) {
            in_commands = false;
            in_command_grid = false;
            in_argument_choices = false;
            in_invocations = false;
            command_row_indent = None;
            continue;
        }
        if let Some((names, rest)) = parse_flag_line(line) {
            let takes_value = flag_takes_separate_value(&rest);
            let description = inline_description(&rest)
                .or_else(|| block_description(&lines, index, indent_of(line)))
                .map_or_else(String::new, |text| shorten(&text));
            for name in names {
                if seen_flags.insert(name.clone()) && help.flags.len() < MAX_ENTRIES {
                    help.flags.push(HelpEntry {
                        name,
                        description: description.clone(),
                        takes_value,
                    });
                }
            }
            continue;
        }
        if (in_commands || !scope.is_empty())
            && line.starts_with(char::is_whitespace)
            && let Some((name, description)) = help_invocation_subcommand(command, scope, line)
        {
            push_subcommand(
                &mut help,
                &mut seen_subcommands,
                name,
                description,
                &mut truncated,
            );
            continue;
        }
        if in_invocations {
            if let Some((name, description)) = help_invocation_subcommand(command, scope, line) {
                push_subcommand(
                    &mut help,
                    &mut seen_subcommands,
                    name,
                    description,
                    &mut truncated,
                );
            }
            continue;
        }
        if in_argument_choices {
            if let Some((names, description)) = help_argument_choices(line) {
                saw_commands = true;
                for name in names {
                    push_subcommand(
                        &mut help,
                        &mut seen_subcommands,
                        name,
                        description.clone(),
                        &mut truncated,
                    );
                }
            }
            continue;
        }
        if !in_commands {
            continue;
        }
        if in_command_grid {
            if let Some(names) = help_command_grid_row(line) {
                for name in names {
                    push_subcommand(
                        &mut help,
                        &mut seen_subcommands,
                        name,
                        String::new(),
                        &mut truncated,
                    );
                }
            }
            continue;
        }
        if let Some(names) = help_comma_command_row(line) {
            for name in names {
                push_subcommand(
                    &mut help,
                    &mut seen_subcommands,
                    name,
                    String::new(),
                    &mut truncated,
                );
            }
            continue;
        }
        if let Some((mut names, description)) = help_subcommand_row(line) {
            let indent = indent_of(line);
            if command_row_indent.is_some_and(|expected| indent > expected) {
                continue;
            }
            command_row_indent = Some(command_row_indent.map_or(indent, |value| value.min(indent)));
            let description = match block_description(&lines, index, indent_of(line)) {
                Some(continuation) => format!("{description} {continuation}"),
                None => description,
            };
            names.extend(description_aliases(&description));
            let mut names = names.into_iter();
            let Some(name) = names.next() else {
                continue;
            };
            push_subcommand(
                &mut help,
                &mut seen_subcommands,
                name,
                shorten(&description),
                &mut truncated,
            );
            for alias in names {
                if seen_subcommands.insert(alias.clone()) {
                    if help.subcommand_aliases.len() < MAX_ENTRIES {
                        help.subcommand_aliases.push(alias);
                    } else {
                        truncated = true;
                    }
                }
            }
        }
    }
    help.subcommands_exhaustive = saw_commands && !help.subcommands.is_empty() && !truncated;
    help
}

fn push_subcommand(
    help: &mut CommandHelp,
    seen: &mut HashSet<String>,
    name: String,
    description: String,
    truncated: &mut bool,
) {
    if !seen.insert(name.clone()) {
        return;
    }
    if help.subcommands.len() < MAX_ENTRIES {
        help.subcommands.push(HelpEntry {
            name,
            description,
            takes_value: false,
        });
    } else {
        *truncated = true;
    }
}

/// Flush-left `Commands:`-family headers used by cobra/clap-style help:
/// `Commands:`, `Available Commands:`, `Management Commands:`, and the
/// parenthesized `Commands (…)` variants.
fn is_commands_header(command: &str, line: &str) -> bool {
    if line.starts_with(char::is_whitespace) {
        return false;
    }
    let head = line
        .trim()
        .trim_end_matches(':')
        .split('(')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_lowercase();
    matches!(
        head.as_str(),
        "commands" | "subcommands" | "the commands are"
    ) || head.ends_with(" commands")
        || head.starts_with("these are common git commands")
        || (command_basename(command) == "pnpm"
            && matches!(
                head.as_str(),
                "manage your dependencies"
                    | "patch your dependencies"
                    | "review your dependencies"
                    | "run your scripts"
                    | "other"
                    | "manage your engines"
                    | "inspect your store"
                    | "manage your store"
                    | "manage your cache"
            ))
}

fn help_comma_command_row(line: &str) -> Option<Vec<String>> {
    if !line.starts_with(char::is_whitespace) || !line.contains(',') {
        return None;
    }
    let names: Vec<_> = line
        .trim()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    (names.len() >= 2 && names.iter().all(|name| is_entry_name(name))).then_some(names)
}

fn is_arguments_header(line: &str) -> bool {
    if line.starts_with(char::is_whitespace) {
        return false;
    }
    matches!(
        line.trim()
            .trim_end_matches(':')
            .to_ascii_lowercase()
            .as_str(),
        "arguments" | "positionals" | "positional arguments"
    )
}

fn usage_signature(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.eq_ignore_ascii_case("usage") {
        return Some("");
    }
    let (header, signature) = trimmed.split_once(':')?;
    header
        .trim()
        .eq_ignore_ascii_case("usage")
        .then_some(signature.trim_start())
}

fn usage_has_free_positional(command: &str, scope: &[String], line: &str) -> bool {
    let command = command_basename(command);
    let trimmed = line.trim();
    let signature = split_help_columns(trimmed).map_or(trimmed, |(signature, _)| signature);
    let mut words = signature.split_whitespace();
    if words.next() != Some(command) {
        return false;
    }
    for expected in scope {
        if words.next() != Some(expected.as_str()) {
            return false;
        }
    }
    words.any(|word| {
        if word.starts_with('-') || word.contains('|') {
            return false;
        }
        let normalized = word
            .trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && !matches!(character, '_' | '-' | '.')
            })
            .to_ascii_lowercase();
        matches!(
            normalized.as_str(),
            "arg"
                | "args"
                | "argument"
                | "arguments"
                | "directory"
                | "expression"
                | "file"
                | "filename"
                | "host"
                | "module"
                | "name"
                | "package"
                | "packages"
                | "path"
                | "pattern"
                | "port"
                | "prompt"
                | "requirement"
                | "requirements"
                | "script"
                | "script.js"
                | "target"
                | "url"
                | "value"
        )
    })
}

/// Homebrew-style help groups executable examples under prose-oriented
/// headings instead of a formal Commands section. Only exact
/// `<command> <subcommand>` invocation rows are accepted from these groups.
fn is_invocation_section_header(line: &str) -> bool {
    if line.starts_with(char::is_whitespace) {
        return false;
    }
    matches!(
        line.trim()
            .trim_end_matches(':')
            .to_ascii_lowercase()
            .as_str(),
        "example usage" | "troubleshooting" | "contributing" | "further help"
    )
}

fn help_invocation_subcommand(
    command: &str,
    scope: &[String],
    line: &str,
) -> Option<(String, String)> {
    let command = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command);
    let trimmed = line.trim();
    let (signature, prose) = split_help_columns(trimmed).unwrap_or((trimmed, ""));
    let mut words = signature.split_whitespace();
    if words.next()? != command {
        return None;
    }
    for expected in scope {
        if words.next()? != expected {
            return None;
        }
    }
    let name = words.next()?;
    if !is_entry_name(name) {
        return None;
    }
    let syntax = words.collect::<Vec<_>>().join(" ");
    let description = if prose.is_empty() { &syntax } else { prose };
    Some((name.to_owned(), shorten(description)))
}

/// Any other flush-left `Something:` line ends a commands section (`Flags:`,
/// `Options:`, `Global Flags:`, …). The commands header itself is matched
/// first by the caller.
fn is_help_section_header(line: &str) -> bool {
    if line.starts_with(char::is_whitespace) {
        return false;
    }
    line.trim_end().ends_with(':') || is_all_caps_help_heading(line.trim())
}

/// Two-column `name   description` rows inside a `--help` commands section.
/// Unlike the man-page variant, hyphenated names (`api-versions`) and
/// clap/commander-style alias lists (`build, b`, `update|upgrade`) are
/// accepted and retained for validation without creating duplicate rows.
fn help_subcommand_row(line: &str) -> Option<(Vec<String>, String)> {
    if line.starts_with('-') || !line.starts_with(char::is_whitespace) {
        return None;
    }
    let trimmed = line.trim_start();
    let (signature, description) = split_help_columns(trimmed)?;
    let mut signature_words = signature.split_whitespace();
    let name_token = signature_words.next()?;
    let comma_aliases = name_token.ends_with(',');
    let mut names = split_subcommand_names(name_token.trim_end_matches([',', ':']))?;
    if comma_aliases {
        let alias = signature_words.next()?.trim_end_matches([',', ':']);
        names.extend(split_subcommand_names(alias)?);
        if let Some((canonical, _)) = names.iter().enumerate().max_by_key(|(_, name)| name.len())
            && canonical != 0
        {
            names.swap(0, canonical);
        }
    }
    if description.is_empty() {
        return None;
    }
    names.extend(description_aliases(description));
    let mut seen = HashSet::new();
    names.retain(|name| seen.insert(name.clone()));
    Some((names, description.to_owned()))
}

fn split_help_columns(line: &str) -> Option<(&str, &str)> {
    let mut gap_start = None;
    let mut spaces = 0;
    for (index, character) in line.char_indices() {
        if character == '\t' {
            let start = gap_start.unwrap_or(index);
            let description = line[index + character.len_utf8()..].trim_start();
            if !description.is_empty() {
                return Some((line[..start].trim_end(), description));
            }
            gap_start = None;
            spaces = 0;
        } else if character == ' ' {
            gap_start.get_or_insert(index);
            spaces += 1;
            if spaces >= 2 {
                let start = gap_start?;
                let description = line[index + 1..].trim_start();
                if !description.is_empty() {
                    return Some((line[..start].trim_end(), description));
                }
            }
        } else {
            gap_start = None;
            spaces = 0;
        }
    }
    None
}

fn is_all_caps_help_heading(line: &str) -> bool {
    let mut letters = 0;
    for character in line.chars().filter(char::is_ascii_alphabetic) {
        if !character.is_ascii_uppercase() {
            return false;
        }
        letters += 1;
    }
    letters >= 2
}

fn openssl_command_group(command: &str, line: &str) -> Option<bool> {
    if command_basename(command) != "openssl" || line.starts_with(char::is_whitespace) {
        return None;
    }
    let heading = line.trim().to_ascii_lowercase();
    (heading == "standard commands"
        || heading.starts_with("message digest commands")
        || heading.starts_with("cipher commands"))
    .then_some(heading == "standard commands")
}

fn help_command_grid_row(line: &str) -> Option<Vec<String>> {
    let names: Vec<_> = line.split_whitespace().map(str::to_owned).collect();
    (!names.is_empty() && names.iter().all(|name| is_entry_name(name))).then_some(names)
}

fn help_argument_choices(line: &str) -> Option<(Vec<String>, String)> {
    if !line.starts_with(char::is_whitespace) {
        return None;
    }
    let trimmed = line.trim_start();
    let choices = trimmed.strip_prefix('{')?;
    let end = choices.find('}')?;
    let names: Vec<_> = choices[..end]
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    if names.len() < 2 || !names.iter().all(|name| is_entry_name(name)) {
        return None;
    }
    Some((names, shorten(choices[end + 1..].trim_start())))
}

fn split_subcommand_names(token: &str) -> Option<Vec<String>> {
    let names: Vec<_> = token
        .split(['|', ','])
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    (!names.is_empty() && names.iter().all(|name| is_entry_name(name))).then_some(names)
}

fn description_aliases(description: &str) -> Vec<String> {
    let Some(start) = description
        .find("[aliases:")
        .or_else(|| description.find("[alias:"))
    else {
        return Vec::new();
    };
    let Some(end) = description[start..].find(']') else {
        return Vec::new();
    };
    let block = &description[start + 1..start + end];
    let Some((_, aliases)) = block.split_once(':') else {
        return Vec::new();
    };
    aliases
        .split([',', ' ', '|'])
        .map(str::trim)
        .filter(|alias| is_entry_name(alias))
        .map(str::to_owned)
        .collect()
}

/// Conservative heuristics over `man -P cat` output. Anything unrecognized is
/// skipped rather than guessed: a partial flag list is useful, a wrong one is
/// not.
fn parse_man_page(command: &str, text: &str) -> CommandHelp {
    let lines: Vec<String> = text.lines().map(strip_overstrike).collect();
    let mut help = CommandHelp::default();
    let mut seen_flags = HashSet::new();
    let mut seen_subcommands = HashSet::new();
    let mut in_commands = false;
    for (index, line) in lines.iter().enumerate() {
        if is_section_header(line) {
            // COMMANDS / SUBCOMMANDS / "COMMAND LIST" style sections only;
            // e.g. git(1) has "GIT COMMANDS" and "HIGH-LEVEL COMMANDS
            // (PORCELAIN)".
            in_commands = is_command_section_header(line);
            continue;
        }
        if let Some((names, rest)) = parse_flag_line(line) {
            let takes_value = flag_takes_separate_value(&rest);
            let description = inline_description(&rest)
                .or_else(|| block_description(&lines, index, indent_of(line)))
                .map_or_else(String::new, |text| shorten(&text));
            for name in names {
                if seen_flags.insert(name.clone()) && help.flags.len() < MAX_ENTRIES {
                    help.flags.push(HelpEntry {
                        name,
                        description: description.clone(),
                        takes_value,
                    });
                }
            }
            continue;
        }
        if !in_commands {
            continue;
        }
        if let Some(name) = git_style_subcommand(command, line) {
            let description = block_description(&lines, index, indent_of(line))
                .map_or_else(String::new, |text| shorten(&text));
            if seen_subcommands.insert(name.clone()) && help.subcommands.len() < MAX_ENTRIES {
                help.subcommands.push(HelpEntry {
                    name,
                    description,
                    takes_value: false,
                });
            }
        } else if let Some((name, description)) = two_column_subcommand(line)
            && seen_subcommands.insert(name.clone())
            && help.subcommands.len() < MAX_ENTRIES
        {
            help.subcommands.push(HelpEntry {
                name,
                description: shorten(&description),
                takes_value: false,
            });
        } else if let Some((names, description)) = man_signature_subcommand(command, &lines, index)
        {
            let mut names = names.into_iter();
            let Some(name) = names.next() else {
                continue;
            };
            if seen_subcommands.insert(name.clone()) && help.subcommands.len() < MAX_ENTRIES {
                help.subcommands.push(HelpEntry {
                    name,
                    description: shorten(&description),
                    takes_value: false,
                });
            }
            for alias in names {
                if seen_subcommands.insert(alias.clone())
                    && help.subcommand_aliases.len() < MAX_ENTRIES
                {
                    help.subcommand_aliases.push(alias);
                }
            }
        }
    }
    help
}

fn is_command_section_header(line: &str) -> bool {
    let head = line.trim().split('(').next().unwrap_or_default().trim_end();
    matches!(head, "COMMANDS" | "SUBCOMMANDS" | "COMMAND LIST")
        || head.ends_with(" COMMANDS")
        || head.ends_with(" SUBCOMMANDS")
        || head.ends_with(" COMMAND LIST")
}

/// `man -P cat` output keeps troff overstrike markup: bold is `X\bX` and
/// underline is `_\bX`. Collapse each overprinted pair to the visible glyph.
fn strip_overstrike(text: &str) -> String {
    let mut output: Vec<char> = Vec::with_capacity(text.len());
    let mut overstrike = false;
    for character in text.chars() {
        if character == '\u{8}' {
            overstrike = true;
        } else if overstrike {
            output.pop();
            output.push(character);
            overstrike = false;
        } else {
            output.push(character);
        }
    }
    output.into_iter().collect()
}

/// A flush-left ALL-CAPS line such as `OPTIONS` or `GIT COMMANDS`. The
/// running page header (`TAR(1)   General Commands Manual   TAR(1)`) contains
/// lowercase words and is rejected.
fn is_section_header(line: &str) -> bool {
    if line.starts_with(char::is_whitespace) {
        return false;
    }
    let letters = line.chars().filter(char::is_ascii_alphabetic);
    let mut count = 0;
    for letter in letters {
        if !letter.is_ascii_uppercase() {
            return false;
        }
        count += 1;
    }
    count >= 2
}

/// Leading flag tokens of a line, e.g. `-a, --auto-compress` or `--color`
/// from `--color=when`. Returns the flags plus the unparsed remainder.
fn parse_flag_line(line: &str) -> Option<(Vec<String>, String)> {
    let mut rest = line.trim_start();
    if !rest.starts_with('-') {
        return None;
    }
    let mut flags = Vec::new();
    loop {
        if !rest.starts_with('-') {
            break;
        }
        let token_len = rest
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '-')
            .map(char::len_utf8)
            .sum::<usize>();
        let token = &rest[..token_len];
        if !is_flag_token(token) {
            break;
        }
        flags.push(token.to_owned());
        rest = rest[token_len..].trim_start();
        if rest.starts_with(',') || rest.starts_with('|') {
            rest = rest[1..].trim_start();
            continue;
        }
        break;
    }
    if flags.is_empty() {
        return None;
    }
    Some((flags, rest.to_owned()))
}

fn is_flag_token(token: &str) -> bool {
    let stripped = token.strip_prefix('-').unwrap_or(token);
    let stripped = stripped.strip_prefix('-').unwrap_or(stripped);
    token.starts_with('-')
        && !stripped.is_empty()
        && stripped
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && stripped
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Same-line description after the flag tokens, e.g. the `Create a new
/// archive` in `-c      Create a new archive`. A single trailing word is an
/// argument placeholder (`-D format`), not a description.
fn inline_description(rest: &str) -> Option<String> {
    let trimmed = rest.trim_start();
    if trimmed.is_empty() || trimmed.starts_with(['=', '<', '[']) {
        return None;
    }
    let first_end = trimmed.find(char::is_whitespace)?;
    let after = trimmed[first_end..].trim_start();
    if after.is_empty() {
        return None;
    }
    // `-f format      Do the thing`: a placeholder-looking first word
    // separated by a wide gap belongs to the flag, not the description.
    let gap = trimmed[first_end..]
        .chars()
        .take_while(|character| *character == ' ')
        .count();
    let first = &trimmed[..first_end];
    if gap >= 2 && is_placeholder(first) {
        return Some(after.to_owned());
    }
    Some(trimmed.to_owned())
}

/// Whether the syntax following a parsed flag names a required, separately
/// supplied value. Attached-only forms such as `--color=WHEN` do not consume
/// the next argv word; `<FILE>`, `FILE`, and `string  Description` do.
fn flag_takes_separate_value(rest: &str) -> bool {
    let trimmed = rest.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('=') || trimmed.starts_with("[=") {
        return false;
    }
    if trimmed.starts_with('<') {
        return true;
    }
    if trimmed.starts_with('[') {
        let token = trimmed.split_whitespace().next().unwrap_or_default();
        if token.starts_with("[<") && token.ends_with(">]") {
            return true;
        }
        return token
            .strip_prefix('[')
            .and_then(|token| token.strip_suffix(']'))
            .is_some_and(is_placeholder);
    }
    let Some(first_end) = trimmed.find(char::is_whitespace) else {
        return is_placeholder(trimmed.trim_end_matches([',', ';', ':']));
    };
    let first = trimmed[..first_end].trim_end_matches([',', ';', ':']);
    let gap = trimmed[first_end..]
        .chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .count();
    gap >= 2 && is_placeholder(first)
}

fn is_placeholder(word: &str) -> bool {
    if word.is_empty() || word.len() > 12 {
        return false;
    }
    // ALL-CAPS (`FILE`, `TAG`) or lowercase (`format`, `when`) argument
    // placeholders; a capitalized word starts a real description.
    let lowercase = word
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    let uppercase = word
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    lowercase || uppercase
}

/// First lines of the deeper-indented block following an entry line (man
/// description bodies sit one indent level below their option/command name).
fn block_description(lines: &[String], entry_index: usize, entry_indent: usize) -> Option<String> {
    let mut collected = String::new();
    for line in lines.iter().skip(entry_index + 1).take(3) {
        if line.trim().is_empty() || indent_of(line) <= entry_indent {
            break;
        }
        if !collected.is_empty() {
            collected.push(' ');
        }
        collected.push_str(line.trim());
    }
    (!collected.is_empty()).then_some(collected)
}

fn indent_of(line: &str) -> usize {
    line.chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .count()
}

/// git(1) style: a `git-add(1)` reference line inside a COMMANDS section.
fn git_style_subcommand(command: &str, line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix(command)?.strip_prefix('-')?;
    let name = rest.strip_suffix("(1)")?;
    (is_entry_name(name)).then(|| name.to_owned())
}

/// Two-column `name  description` entries inside a COMMANDS section.
fn two_column_subcommand(line: &str) -> Option<(String, String)> {
    if line.starts_with('-') || !line.starts_with(char::is_whitespace) {
        return None;
    }
    let trimmed = line.trim_start();
    let name_end = trimmed.find(char::is_whitespace)?;
    let name = &trimmed[..name_end];
    if !is_entry_name(name) || name.contains('-') {
        return None;
    }
    let gap = trimmed[name_end..]
        .chars()
        .take_while(|character| *character == ' ')
        .count();
    if gap < 2 {
        return None;
    }
    let description = trimmed[name_end..].trim_start();
    if description.is_empty() {
        return None;
    }
    Some((name.to_owned(), description.to_owned()))
}

/// Command headings in BSD-style man pages often use
/// `subcommand [arguments]` on one line with the description in the deeper
/// indented block below. Homebrew documents its complete command surface in
/// this form rather than as two-column rows.
fn man_signature_subcommand(
    command: &str,
    lines: &[String],
    index: usize,
) -> Option<(Vec<String>, String)> {
    let line = lines.get(index)?;
    if line.starts_with('-') || !line.starts_with(char::is_whitespace) {
        return None;
    }
    let trimmed = line.trim_start();
    let name_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let name_token = &trimmed[..name_end];
    let mut names = split_subcommand_names(name_token.trim_end_matches(','))?;
    if names.first().is_some_and(|name| name == command) {
        return None;
    }
    let mut usage = &trimmed[name_end..];
    if name_token.ends_with(',') {
        let alias = usage.trim_start();
        let alias_end = alias.find(char::is_whitespace)?;
        names.extend(split_subcommand_names(&alias[..alias_end])?);
        usage = &alias[alias_end..];
    }
    let description = block_description(lines, index, indent_of(line))?;
    let usage = usage.trim();
    Some((
        names,
        if usage.is_empty() {
            description
        } else {
            format!("{usage}: {description}")
        },
    ))
}

fn is_entry_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && !name.ends_with(':')
        && !name.contains("::")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | ':'))
}

/// Collapse all whitespace, drop control characters, and truncate with `…`.
fn shorten(text: &str) -> String {
    let collapsed: String = text
        .split_whitespace()
        .filter(|word| !word.chars().any(char::is_control))
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = collapsed.chars();
    let mut shortened: String = chars.by_ref().take(MAX_DESCRIPTION_CHARS).collect();
    if chars.next().is_some() {
        while shortened.ends_with(char::is_whitespace) {
            shortened.pop();
        }
        shortened.push('…');
    }
    shortened
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString, fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration,
    };

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    fn bold(text: &str) -> String {
        text.chars()
            .flat_map(|character| [character, '\u{8}', character])
            .collect()
    }

    #[test]
    fn parses_tar_style_flags_with_inline_and_block_descriptions() {
        let page = [
            bold("NAME"),
            String::new(),
            bold("DESCRIPTION"),
            format!(
                "     {}      Create a new archive containing the specified items.",
                bold("-c")
            ),
            String::new(),
            "     In other modes, files are added in order.".to_owned(),
            String::new(),
            format!("     {}", bold("-r")),
            "             Like -c, but entries are appended to the".to_owned(),
            "             archive.".to_owned(),
            String::new(),
            format!("     {} {}", bold("-f"), bold("format")),
            "             Use the given format for the archive.".to_owned(),
            String::new(),
            format!("     {}", bold("--null")),
            "             Read null-terminated names.".to_owned(),
        ]
        .join("\n");
        let help = parse_man_page("tar", &page);
        let flags: Vec<(&str, &str)> = help
            .flags
            .iter()
            .map(|entry| (entry.name.as_str(), entry.description.as_str()))
            .collect();
        assert_eq!(
            flags,
            [
                ("-c", "Create a new archive containing the specified items."),
                ("-r", "Like -c, but entries are appended to the archive."),
                ("-f", "Use the given format for the archive."),
                ("--null", "Read null-terminated names."),
            ]
        );
        assert!(!help.flags[0].takes_value);
        assert!(help.flags[2].takes_value);
        assert!(help.subcommands.is_empty());
    }

    #[test]
    fn parses_comma_joined_and_equals_style_flags() {
        let page = format!(
            "{}\n     {}, {}\n             Use the archive suffix.\n\n     {}=_\n             Colorize output.\n",
            bold("OPTIONS"),
            bold("-a"),
            bold("--auto-compress"),
            bold("--color"),
        );
        let help = parse_man_page("ls", &page);
        let names: Vec<&str> = help.flags.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["-a", "--auto-compress", "--color"]);
        assert_eq!(help.flags[0].description, "Use the archive suffix.");
        assert_eq!(help.flags[2].description, "Colorize output.");
    }

    #[test]
    fn parses_git_style_subcommands_only_inside_command_sections() {
        let page = [
            bold("NAME"),
            format!("     {} - the stupid content tracker", bold("git")),
            String::new(),
            bold("GIT COMMANDS"),
            "     We divide Git into groups.".to_owned(),
            String::new(),
            "   Main porcelain commands".to_owned(),
            format!("     {}(1)", bold("git-add")),
            "         Add file contents to the index.".to_owned(),
            String::new(),
            format!("     {}(1)", bold("git-commit")),
            "         Record changes to the repository.".to_owned(),
            String::new(),
            bold("SEE ALSO"),
            format!("     {}(1)", bold("git-web--browse")),
            "         Not a subcommand section.".to_owned(),
        ]
        .join("\n");
        let help = parse_man_page("git", &page);
        let names: Vec<&str> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["add", "commit"]);
        assert_eq!(
            help.subcommands[0].description,
            "Add file contents to the index."
        );
    }

    #[test]
    fn parses_two_column_subcommand_tables() {
        let page = [
            bold("NAME"),
            "  demo - demo tool".to_owned(),
            String::new(),
            bold("COMMANDS"),
            "  start   Start the service.".to_owned(),
            "  stop    Stop the service.".to_owned(),
            "  list".to_owned(),
            String::new(),
            bold("EXIT STATUS"),
            "  text here".to_owned(),
        ]
        .join("\n");
        let help = parse_man_page("demo", &page);
        let names: Vec<&str> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["start", "stop"]);
        assert_eq!(help.subcommands[1].description, "Stop the service.");
    }

    #[test]
    fn parses_man_page_command_signatures_with_block_descriptions() {
        let page = [
            bold("COMMANDS"),
            "   install formula|cask...".to_owned(),
            "     Install a formula or cask.".to_owned(),
            String::new(),
            "   update".to_owned(),
            "     Fetch the newest package metadata.".to_owned(),
            String::new(),
            "   doctor, dr [--list-checks]".to_owned(),
            "     Check the system for potential problems.".to_owned(),
            String::new(),
            "     descriptive prose continues here".to_owned(),
        ]
        .join("\n");
        let help = parse_man_page("brew", &page);
        let names: Vec<_> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["install", "update", "doctor"]);
        assert_eq!(help.subcommand_aliases, ["dr"]);
        assert_eq!(
            help.subcommands[0].description,
            "formula|cask...: Install a formula or cask."
        );
    }

    #[test]
    fn command_named_non_subcommand_sections_do_not_leak_rows() {
        let page = [
            bold("COMMAND LINE OPTIONS"),
            "  output   This is prose laid out in two columns.".to_owned(),
            bold("COMMAND EXECUTION"),
            "  worker   This is also not a subcommand.".to_owned(),
            bold("SUBCOMMANDS"),
            "  deploy   Ship the service.".to_owned(),
        ]
        .join("\n");
        let help = parse_man_page("demo", &page);
        let names: Vec<_> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["deploy"]);
    }

    #[test]
    fn garbage_input_yields_no_entries() {
        assert_eq!(parse_man_page("x", ""), CommandHelp::default());
        assert_eq!(
            parse_man_page("x", "random prose\n- not a flag line\n--\n-@notaflag\n"),
            CommandHelp::default()
        );
    }

    const KUBECTL_HELP: &str = "\
kubectl controls the Kubernetes cluster manager.

Usage:
  kubectl [flags] [options]

Available Commands:
  apply         Apply a configuration to a resource by file name or stdin
  api-versions  Print the supported API versions on the server
  create        Create a resource from a file or from stdin
  get           Display one or many resources

Flags:
  -h, --help              help for kubectl
      --kubeconfig string  Path to the kubeconfig file

Use \"kubectl <command> --help\" for more information about a given command.
";

    const DOCKER_HELP: &str = "\
Usage:  docker [OPTIONS] COMMAND

A self-sufficient runtime for containers

Management Commands:
  builder     Manage builds
  container   Manage containers

Commands:
  attach      Attach local standard input, output, and error streams to a running container
  build       Build an image from a Dockerfile

Global Flags:
      --config string   Location of client config files
  -D, --debug           Enable debug mode
";

    const CARGO_HELP: &str = "\
Rust's package manager

Usage: cargo [OPTIONS] [COMMAND]

Commands:
  build, b    Compile the current package
  check, c    Analyze the current package and report errors
  run         Run a binary or example of the local package

Options:
  -V, --version   Print version
";

    const AI_CLI_HELP: &str = "\
Usage: ai [OPTIONS] <COMMAND>

Commands:
  exec            Run non-interactively [aliases: e]
  apply           Apply the latest patch to the local
                  working tree [aliases: a]
  update|upgrade  Install the latest version

Arguments:
  [PROMPT]        Optional prompt to start a session

Options:
  -h, --help      Print help
";

    const BREW_HELP: &str = "\
Example usage:
  brew search TEXT|/REGEX/
  brew info [FORMULA|CASK...]
  brew install FORMULA|CASK...
  brew update
  brew upgrade [FORMULA|CASK...]
  brew uninstall FORMULA|CASK...
  brew list [FORMULA|CASK...]

Troubleshooting:
  brew config
  brew doctor
  brew install --verbose --debug FORMULA|CASK

Contributing:
  brew create URL [--no-fetch]
  brew edit [FORMULA|CASK...]

Further help:
  brew commands
  brew help [COMMAND]
  man brew
  https://docs.brew.sh
";

    const GO_HELP: &str = "\
Usage:
\tgo <command> [arguments]

The commands are:
\tbug         start a bug report
\tbuild       compile packages and dependencies
\tmod         module maintenance

Additional help topics:
\tbuildconstraint build constraints
";

    const GH_HELP: &str = "\
USAGE
  gh <command> <subcommand> [flags]

CORE COMMANDS
  auth:          Authenticate gh and git with GitHub
  pr:            Manage pull requests

HELP TOPICS
  accessibility: Learn about accessibility

FLAGS
  --help      Show help for command
";

    const GH_PR_HELP: &str = "\
USAGE
  gh pr <command> [flags]

GENERAL COMMANDS
  create:        Create a pull request
  list:          List pull requests

FLAGS
  --help   Show help for command
";

    const OPENSSL_HELP: &str = "\
help:

Standard commands
asn1parse         ca                ciphers           x509

Message Digest commands (see the `dgst' command for more details)
md5               sha1              sha256

Cipher commands (see the `enc' command for more details)
aes-128-cbc       aes-256-cbc
";

    const SIGNATURE_HELP: &str = "\
Commands:
  agents [options]                      Manage background agents
  i, install                            Install dependencies
  plugin|plugins                        Manage plugins
";

    #[test]
    fn parses_go_prose_header_without_leaking_help_topics() {
        let help = parse_help_output("go", GO_HELP);
        let names: Vec<_> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["bug", "build", "mod"]);
        assert!(help.subcommands_exhaustive);
    }

    #[test]
    fn parses_git_native_help_command_groups() {
        let help = parse_help_output(
            "git",
            r#"These are common Git commands used in various situations:

start a working area
   clone      Clone a repository
   init       Create an empty repository

work on the current change
   add        Add file contents to the index
"#,
        );
        let names: Vec<_> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["clone", "init", "add"]);
    }

    #[test]
    fn parses_uppercase_colon_command_groups_without_help_topics() {
        let help = parse_help_output("gh", GH_HELP);
        let names: Vec<_> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["auth", "pr"]);
        assert!(!names.contains(&"accessibility"));
    }

    #[test]
    fn parses_openssl_standard_command_grid_only() {
        let help = parse_help_output("openssl", OPENSSL_HELP);
        let names: Vec<_> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["asn1parse", "ca", "ciphers", "x509"]);
        assert!(!names.contains(&"sha256"));
        assert!(!names.contains(&"aes-128-cbc"));
    }

    #[test]
    fn parses_usage_signatures_and_prefers_full_comma_aliases() {
        let help = parse_help_output("demo", SIGNATURE_HELP);
        let names: Vec<_> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["agents", "install", "plugin"]);
        assert_eq!(help.subcommand_aliases, ["i", "plugins"]);
    }

    #[test]
    fn parses_argparse_positional_command_choices() {
        let help = parse_help_output(
            "conda",
            "usage: conda [-h] COMMAND ...\n\npositional arguments:\n  {activate,clean,create,install}\n                        Command to run\n\noptions:\n  -h, --help            show this help message\n",
        );
        let names: Vec<_> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["activate", "clean", "create", "install"]);
        assert!(help.accepts_positionals);
        assert!(help.subcommands_exhaustive);
    }

    #[test]
    fn parses_pnpm_categorized_and_npm_comma_command_lists() {
        let pnpm = parse_help_output(
            "pnpm",
            "Manage your dependencies:\n  i, install       Install dependencies\n  clean            Remove node_modules\n\nManage your store:\n  store add        Add packages\n\nOptions:\n  -r, --recursive  Run recursively\n",
        );
        assert_eq!(
            pnpm.subcommands
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["install", "clean", "store"]
        );
        assert_eq!(pnpm.subcommand_aliases, ["i"]);
        assert!(pnpm.subcommands_exhaustive);

        let npm = parse_help_output(
            "npm",
            "All commands:\n    access, adduser, audit, cache, config, install, run\n\nOptions:\n  -h, --help  Show help\n",
        );
        assert_eq!(
            npm.subcommands
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            [
                "access", "adduser", "audit", "cache", "config", "install", "run"
            ]
        );
        assert!(npm.subcommands_exhaustive);
    }

    #[test]
    fn parses_scoped_usage_and_literal_invocation_rows() {
        let npm_scope = vec!["cache".to_owned()];
        let npm = parse_help_output_for_scope(
            "npm",
            &npm_scope,
            "Usage:\nnpm cache add <package-spec>\nnpm cache clean [<key>]\nnpm cache ls [<name>]\nnpm cache verify\n\nOptions:\n  --cache <path>\n",
        );
        assert_eq!(
            npm.subcommands
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["add", "clean", "ls", "verify"]
        );
        assert!(!npm.subcommands_exhaustive);

        let bun_scope = vec!["pm".to_owned()];
        let bun = parse_help_output_for_scope(
            "bun",
            &bun_scope,
            "bun pm: Package manager utilities\n\n  bun pm pack       create a tarball\n  bun pm bin        print the bin folder\n  bun pm cache      print the cache folder\n  bun pm cache rm   clear the cache\n",
        );
        assert_eq!(
            bun.subcommands
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["pack", "bin", "cache"]
        );

        let bun_cache_scope = vec!["pm".to_owned(), "cache".to_owned()];
        let bun_cache = parse_help_output_for_scope(
            "bun",
            &bun_cache_scope,
            "  bun pm cache      print the cache folder\n  bun pm cache rm   clear the cache\n",
        );
        assert_eq!(
            bun_cache
                .subcommands
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["rm"]
        );
    }

    #[test]
    fn pnpm_root_help_probes_the_complete_command_list_first() {
        assert_eq!(
            help_probe_arguments("pnpm", &[]),
            [
                vec!["help".to_owned(), "-a".to_owned()],
                vec!["--help".to_owned()],
            ]
        );
        assert_eq!(
            help_probe_arguments("pnpm", &["store".to_owned()]),
            [vec!["store".to_owned(), "--help".to_owned()]]
        );
        assert_eq!(
            help_probe_arguments("swift", &["package".to_owned()]),
            [
                vec!["help".to_owned(), "package".to_owned()],
                vec!["package".to_owned(), "--help".to_owned()],
            ]
        );
    }

    #[test]
    fn bracketed_flag_arguments_and_documented_values_are_recognized() {
        assert!(flag_takes_separate_value("[<SPEC>]  Select a package"));
        assert!(flag_takes_separate_value("[DIRECTORY]  Change directory"));
        assert!(!flag_takes_separate_value(
            "[=<WHEN>]  Optional attached value"
        ));
        assert_eq!(
            documented_value_choices("Coloring [possible values: auto, always, never]"),
            ["auto", "always", "never"]
        );
        assert_eq!(
            documented_value_choices("Build mode (values: debug, release; default: debug)"),
            ["debug", "release"]
        );
        assert!(documented_value_choices("Set an arbitrary string value").is_empty());
    }

    #[test]
    fn parses_kubectl_style_help() {
        let help = parse_help_output("kubectl", KUBECTL_HELP);
        assert!(help.subcommands_exhaustive);
        let names: Vec<&str> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["apply", "api-versions", "create", "get"]);
        assert_eq!(
            help.subcommands[1].description,
            "Print the supported API versions on the server"
        );
        let flags: Vec<(&str, &str)> = help
            .flags
            .iter()
            .map(|entry| (entry.name.as_str(), entry.description.as_str()))
            .collect();
        assert_eq!(
            flags,
            [
                ("-h", "help for kubectl"),
                ("--help", "help for kubectl"),
                ("--kubeconfig", "Path to the kubeconfig file"),
            ]
        );
    }

    #[test]
    fn parses_docker_style_help_with_management_commands() {
        let help = parse_help_output("docker", DOCKER_HELP);
        assert!(help.subcommands_exhaustive);
        let names: Vec<&str> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["builder", "container", "attach", "build"]);
        let flags: Vec<&str> = help.flags.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(flags, ["--config", "-D", "--debug"]);
        assert_eq!(help.flags[0].description, "Location of client config files");
        assert!(help.flags[0].takes_value);
        assert!(!help.flags[1].takes_value);
    }

    #[test]
    fn parses_cargo_style_help_with_aliases() {
        let help = parse_help_output("cargo", CARGO_HELP);
        assert!(help.subcommands_exhaustive);
        let names: Vec<&str> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["build", "check", "run"]);
        assert_eq!(
            help.subcommands[0].description,
            "Compile the current package"
        );
        let flags: Vec<&str> = help.flags.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(flags, ["-V", "--version"]);
        assert_eq!(help.subcommand_aliases, ["b", "c"]);
    }

    #[test]
    fn parses_pipe_and_description_subcommand_aliases() {
        let help = parse_help_output("ai", AI_CLI_HELP);
        let names: Vec<_> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["exec", "apply", "update"]);
        assert_eq!(help.subcommand_aliases, ["e", "a", "upgrade"]);
        assert!(help.subcommands_exhaustive);
        assert!(help.accepts_positionals);
        for alias in ["e", "a", "upgrade"] {
            assert!(history_arguments_are_plausible(
                &help,
                &[alias],
                false,
                false
            ));
        }
        assert!(!history_arguments_are_plausible(
            &help,
            &["upgrad"],
            false,
            false
        ));
        assert!(history_arguments_are_plausible(
            &help,
            &["fix", "this", "bug"],
            false,
            false
        ));
    }

    #[test]
    fn parses_homebrew_invocation_sections_without_claiming_exhaustiveness() {
        let help = parse_help_output("brew", BREW_HELP);
        let names: Vec<_> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "search",
                "info",
                "install",
                "update",
                "upgrade",
                "uninstall",
                "list",
                "config",
                "doctor",
                "create",
                "edit",
                "commands",
                "help",
            ]
        );
        assert_eq!(help.subcommands[2].description, "FORMULA|CASK...");
        assert!(!help.subcommands_exhaustive);
        assert!(!help.accepts_positionals);
    }

    #[test]
    fn proven_hidden_subcommands_survive_a_closed_help_surface() {
        let help = CommandHelp {
            flags: Vec::new(),
            subcommands: vec![HelpEntry {
                name: "deploy".into(),
                description: String::new(),
                takes_value: false,
            }],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: true,
        };
        assert!(!history_arguments_are_plausible(
            &help,
            &["hidden"],
            false,
            false
        ));
        assert!(history_arguments_are_plausible(
            &help,
            &["hidden"],
            true,
            false
        ));
    }

    #[test]
    fn help_garbage_yields_no_entries() {
        assert_eq!(parse_help_output("demo", ""), CommandHelp::default());
        assert_eq!(
            parse_help_output("demo", "just some prose\n"),
            CommandHelp::default()
        );
        // A commands header with no parseable rows still yields no
        // subcommands; rows outside any commands section are ignored.
        let orphan_rows = "  start   Start the service.\nCommands:\n\nFlags:\n";
        let help = parse_help_output("demo", orphan_rows);
        assert!(help.subcommands.is_empty());
        assert!(!help.subcommands_exhaustive);
    }

    #[test]
    fn help_fallback_fills_a_man_page_without_subcommands() {
        use std::cell::Cell;

        let help_calls = Cell::new(0);
        let counting_help = |_: &str| {
            help_calls.set(help_calls.get() + 1);
            CommandHelp {
                flags: Vec::new(),
                subcommands: vec![HelpEntry {
                    name: "apply".into(),
                    description: String::new(),
                    takes_value: false,
                }],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: false,
            }
        };
        let man_with_flags = |_: &str| CommandHelp {
            flags: vec![HelpEntry {
                name: "-x".into(),
                description: String::new(),
                takes_value: false,
            }],
            subcommands: Vec::new(),
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: false,
        };
        // Preserve parsed man flags while filling its missing command list.
        let result = fetch_with_fallback("demo", man_with_flags, counting_help);
        assert_eq!(result.flags.len(), 1);
        assert_eq!(result.subcommands.len(), 1);
        assert_eq!(help_calls.get(), 1);
        // An empty man parse falls back to `--help` exactly once.
        let result = fetch_with_fallback("demo", |_| CommandHelp::default(), counting_help);
        assert_eq!(result.subcommands.len(), 1);
        assert_eq!(help_calls.get(), 2);
        // Both empty: the negative result is returned for caching.
        let result = fetch_with_fallback(
            "demo",
            |_| CommandHelp::default(),
            |_| CommandHelp::default(),
        );
        assert_eq!(result, CommandHelp::default());
    }

    #[test]
    fn homebrew_help_augments_a_nonempty_man_command_list() {
        let entry = |name: &str| HelpEntry {
            name: name.into(),
            description: String::new(),
            takes_value: false,
        };
        let result = fetch_with_fallback(
            "brew",
            |_| CommandHelp {
                flags: Vec::new(),
                subcommands: vec![entry("install"), entry("doctor")],
                subcommand_aliases: vec!["dr".into()],
                accepts_positionals: false,
                subcommands_exhaustive: false,
            },
            |_| CommandHelp {
                flags: Vec::new(),
                subcommands: vec![entry("install"), entry("update")],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: false,
            },
        );
        let names: Vec<_> = result
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["install", "update", "doctor"]);
        assert_eq!(result.subcommand_aliases, ["dr"]);
    }

    #[test]
    fn failing_or_hanging_help_degrades_to_empty() {
        let directory = tempfile::tempdir().expect("script directory");
        let failing = directory.path().join("failing");
        fs::write(&failing, "#!/bin/sh\nexit 1\n").expect("failing script");
        fs::set_permissions(&failing, fs::Permissions::from_mode(0o700)).expect("failing mode");
        let hanging = directory.path().join("hanging");
        // Note: `run_bounded` joins its output readers after killing the
        // direct child, so a grandchild holding the pipe keeps the fetch
        // blocked until it exits — keep this sleep short.
        fs::write(&hanging, "#!/bin/sh\nsleep 2\n").expect("hanging script");
        fs::set_permissions(&hanging, fs::Permissions::from_mode(0o700)).expect("hanging mode");
        assert_eq!(
            fetch_help_output(failing.to_str().expect("failing path")),
            CommandHelp::default()
        );
        assert_eq!(
            fetch_help_output(hanging.to_str().expect("hanging path")),
            CommandHelp::default()
        );
    }

    #[test]
    fn successful_help_script_is_parsed_end_to_end() {
        let directory = tempfile::tempdir().expect("script directory");
        let tool = directory.path().join("demotool");
        fs::write(
            &tool,
            "#!/bin/sh\nprintf 'Commands:\\n  deploy   Ship it.\\n'\n",
        )
        .expect("help script");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).expect("script mode");
        let help = fetch_help_output(tool.to_str().expect("script path"));
        assert_eq!(help.subcommands.len(), 1);
        assert_eq!(help.subcommands[0].name, "deploy");
        assert_eq!(help.subcommands[0].description, "Ship it.");
    }

    #[test]
    fn known_help_entrypoints_and_stderr_help_are_accepted() {
        let directory = tempfile::tempdir().expect("script directory");
        let go = directory.path().join("go");
        fs::write(
            &go,
            "#!/bin/sh\n[ \"$1\" = help ] || exit 3\nprintf 'The commands are:\\n  build   Compile packages.\\n' >&2\n",
        )
        .expect("help script");
        fs::set_permissions(&go, fs::Permissions::from_mode(0o700)).expect("script mode");
        assert!(looks_like_help_output(
            "error: help requested\nThe commands are:\n"
        ));
        let help = fetch_help_program("go", go.as_os_str());
        assert_eq!(help.subcommands.len(), 1);
        assert_eq!(help.subcommands[0].name, "build");
    }

    #[test]
    fn descriptions_are_collapsed_and_truncated() {
        assert_eq!(shorten("  a   b\tc\n"), "a b c");
        let long = "word ".repeat(40);
        let shortened = shorten(&long);
        assert!(shortened.ends_with('…'));
        assert!(shortened.chars().count() <= MAX_DESCRIPTION_CHARS + 1);
    }

    #[test]
    fn overstrike_markup_collapses_to_visible_glyphs() {
        assert_eq!(strip_overstrike(&bold("NAME")), "NAME");
        assert_eq!(strip_overstrike("_\u{8}f_\u{8}i_\u{8}l_\u{8}e"), "file");
        assert_eq!(strip_overstrike("plain"), "plain");
    }

    #[test]
    fn applies_gating_skips_specs_non_executables_and_the_command_token() {
        let directory = tempfile::tempdir().expect("command directory");
        for command in ["ls", "git"] {
            let path = directory.path().join(command);
            fs::write(&path, b"#!/bin/sh\n").expect("fake command");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("command mode");
        }
        let path = OsString::from(directory.path());
        let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
        let cache = Arc::new(CommandHelpCache::default());
        cache.seed("git", CommandHelp::default());
        let provider =
            CommandHelpProvider::new(Arc::new(SpecRegistry::load(None)), commands, cache);
        // `ls` has spec coverage: skipped even at flag/subcommand positions.
        assert!(!provider.applies(&context("ls ", 1)));
        assert!(!provider.applies(&context("ls -", 2)));
        // Not an executable on PATH: skipped.
        assert!(!provider.applies(&context("nosuchcmd ", 3)));
        // Cursor still on the command token: skipped.
        assert!(!provider.applies(&context("gi", 4)));
        // First-argument and flag positions: applies.
        assert!(provider.applies(&context("git ", 5)));
        assert!(provider.applies(&context("git ch", 6)));
        assert!(provider.applies(&context("git -", 7)));
        // Past the first argument without a dash: no.
        assert!(!provider.applies(&context("git add ", 8)));
        // `--` ends flag parsing; a dash-prefixed path after it is not a flag.
        assert!(!provider.applies(&context("git -- -path", 9)));
    }

    #[test]
    fn negative_results_are_cached_after_one_fetch() {
        let cache = CommandHelpCache::default();
        let missing = "hokan-definitely-missing-command";
        let first = cache.get(missing);
        assert!(first.flags.is_empty() && first.subcommands.is_empty());
        assert_eq!(cache.fetch_count(), 1);
        let second = cache.get(missing);
        assert_eq!(cache.fetch_count(), 1);
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn concurrent_cold_misses_share_a_single_fetch() {
        let cache = Arc::new(CommandHelpCache::default());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        // Slow enough that both callers overlap on a cold miss without the
        // cross-fetch lock.
        let fetch = |_: &str| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            CommandHelp::default()
        };
        std::thread::scope(|scope| {
            let worker_cache = Arc::clone(&cache);
            let worker_barrier = Arc::clone(&barrier);
            let fetch = &fetch;
            let worker = scope.spawn(move || {
                worker_barrier.wait();
                worker_cache.get_with("shared-missing", fetch)
            });
            barrier.wait();
            let main = cache.get_with("shared-missing", fetch);
            let spawned = worker.join().expect("worker thread");
            assert!(Arc::ptr_eq(&main, &spawned));
        });
        assert_eq!(cache.fetch_count(), 1);
    }

    #[test]
    fn background_requests_return_immediately_and_dedupe() {
        let cache = Arc::new(CommandHelpCache::default());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        cache.request_with("slow-tool", move |_| {
            started_tx.send(()).expect("started");
            release_rx.recv().expect("released");
            CommandHelp {
                flags: Vec::new(),
                subcommands: vec![HelpEntry {
                    name: "deploy".into(),
                    description: "Ship it".into(),
                    takes_value: false,
                }],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: false,
            }
        });
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "request must not wait for the fetch"
        );
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("background fetch started");
        assert!(cache.is_pending("slow-tool"));
        cache.request_with("slow-tool", |_| panic!("duplicate fetch"));
        assert_eq!(cache.fetch_count(), 1);
        assert!(cache.peek("slow-tool").is_none());

        release_tx.send(()).expect("release fetch");
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while cache.peek("slow-tool").is_none() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        let help = cache.peek("slow-tool").expect("cached help");
        assert!(!cache.is_pending("slow-tool"));
        assert_eq!(help.subcommands[0].name, "deploy");
    }

    #[test]
    fn resolved_executable_change_invalidates_cached_help() {
        let cache = Arc::new(CommandHelpCache::default());
        let first_path = PathBuf::from("/toolchains/one/demo");
        cache.request_with_path("demo", Some(first_path.clone()), |_| CommandHelp {
            flags: Vec::new(),
            subcommands: vec![HelpEntry {
                name: "first".into(),
                description: String::new(),
                takes_value: false,
            }],
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: false,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while cache.peek("demo").is_none() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(
            cache.peek("demo").expect("first help").subcommands[0].name,
            "first"
        );

        // Same resolution is a cache hit; a different executable path gets a
        // fresh parse for the identically named command.
        cache.request_with_path("demo", Some(first_path), |_| panic!("duplicate fetch"));
        cache.request_with_path("demo", Some(PathBuf::from("/toolchains/two/demo")), |_| {
            CommandHelp {
                flags: Vec::new(),
                subcommands: vec![HelpEntry {
                    name: "second".into(),
                    description: String::new(),
                    takes_value: false,
                }],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: false,
            }
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while cache
            .peek("demo")
            .is_none_or(|help| help.subcommands[0].name != "second")
            && std::time::Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert_eq!(
            cache.peek("demo").expect("second help").subcommands[0].name,
            "second"
        );
        assert_eq!(cache.fetch_count(), 2);
    }

    #[test]
    fn engine_emits_seeded_subcommands_and_flags() {
        let directory = tempfile::tempdir().expect("command directory");
        let path = directory.path().join("git");
        fs::write(&path, b"#!/bin/sh\n").expect("fake command");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
        let path_var = OsString::from(directory.path());
        let commands = Arc::new(CommandPathCache::from_path(Some(&path_var)));
        let cache = Arc::new(CommandHelpCache::default());
        cache.seed(
            "git",
            CommandHelp {
                flags: vec![
                    HelpEntry {
                        name: "--paginate".into(),
                        description: "Pipe output into less.".into(),
                        takes_value: false,
                    },
                    HelpEntry {
                        name: "--config".into(),
                        description: "Read configuration from a file.".into(),
                        takes_value: true,
                    },
                    HelpEntry {
                        name: "--color".into(),
                        description: "Coloring [possible values: auto, always, never]".into(),
                        takes_value: true,
                    },
                ],
                subcommands: vec![
                    HelpEntry {
                        name: "checkout".into(),
                        description: "Switch branches.".into(),
                        takes_value: false,
                    },
                    HelpEntry {
                        name: "checkout-index".into(),
                        description: "Copy files from the index.".into(),
                        takes_value: false,
                    },
                ],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: false,
            },
        );
        let provider = CommandHelpProvider::new(
            Arc::new(SpecRegistry::load(None)),
            commands,
            Arc::clone(&cache),
        );
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(provider);

        let output = engine.complete(&context("git", 24));
        let checkout = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "git checkout")
            .expect("bare subcommand candidate");
        let edit = checkout.edit.as_ref().expect("bare edit");
        assert_eq!(edit.range, 0..3);
        assert_eq!(edit.replacement, "git checkout");

        assert!(
            engine
                .complete(&context("sudo git", 25))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "sudo git checkout")
        );

        let output = engine.complete(&context("git ", 1));
        let checkout = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "git checkout")
            .expect("checkout candidate");
        assert_eq!(checkout.source, CandidateSource::CommandHelp);
        assert!(matches!(
            checkout.action,
            CandidateAction::InsertAndContinue { .. }
        ));
        assert!(matches!(
            checkout.completeness,
            Completeness::NeedsInput { .. }
        ));
        assert_eq!(checkout.edit.as_ref().expect("edit").range, 4..4);

        let output = engine.complete(&context("git --p", 2));
        let flag = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "git --paginate")
            .expect("flag candidate");
        assert!(matches!(flag.action, CandidateAction::Insert));
        assert_eq!(flag.edit.as_ref().expect("edit").range, 4..7);

        let wrapped = engine.complete(&context("sudo git --p", 20));
        assert!(
            wrapped
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "sudo git --paginate")
        );

        let after_global_value = engine.complete(&context("git --config cfg ch", 21));
        let checkout = after_global_value
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "git --config cfg checkout")
            .expect("subcommand after a separated global flag value");
        assert_eq!(
            checkout.edit.as_ref().expect("edit").replacement,
            "checkout"
        );
        assert!(
            engine
                .complete(&context("git --config cf", 22))
                .candidates
                .is_empty(),
            "a required flag value must not be treated as a subcommand"
        );
        let separated = engine.complete(&context("git --color a", 28));
        assert_eq!(separated.candidates.len(), 2);
        assert_eq!(
            separated.candidates[0]
                .edit
                .as_ref()
                .expect("separated enum edit")
                .replacement,
            "auto"
        );
        let attached = engine.complete(&context("git --color=a", 29));
        assert!(attached.candidates.iter().any(|candidate| {
            candidate
                .edit
                .as_ref()
                .is_some_and(|edit| edit.replacement == "--color=auto")
        }));
        assert!(
            engine
                .complete(&context("git --color auto", 30))
                .candidates
                .is_empty(),
            "an exact documented value must stay quiet"
        );
        assert!(
            engine
                .complete(&context("git checkout", 23))
                .candidates
                .is_empty(),
            "an exact completed subcommand must not leave fuzzy siblings"
        );
        assert!(
            engine
                .complete(&context("git ckt", 26))
                .candidates
                .is_empty(),
            "subcommands require a real token prefix"
        );

        assert!(
            engine
                .complete(&context("git checkout -", 3))
                .candidates
                .is_empty(),
            "top-level flags must not leak into a recognized subcommand"
        );

        // Past the first argument without a dash the provider stays silent.
        let output = engine.complete(&context("git checkout ", 4));
        assert!(output.candidates.is_empty());
    }

    #[test]
    fn cached_hokan_help_precedes_the_session_command_at_the_budget_boundary() {
        let directory = tempfile::tempdir().expect("command directory");
        let path = directory.path().join("hokan");
        fs::write(&path, b"#!/bin/sh\n").expect("fake command");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
            directory.path(),
        ))));
        let cache = Arc::new(CommandHelpCache::default());
        cache.seed(
            "hokan",
            parse_help_output("hokan", "Commands:\n  config   Configure Hokan\n"),
        );
        cache.seed_scope(
            "hokan",
            &["config"],
            parse_help_output_for_scope(
                "hokan",
                &["config".to_owned()],
                "Commands:\n  ai   Configure AI\n",
            ),
        );

        // With a zero local budget the next provider is intentionally not
        // allowed to run once a useful row exists. The semantic help provider
        // must therefore precede the matching `hokan-leave` session row.
        let mut engine = CompletionEngine::new(100, 20).with_local_timeout(Duration::ZERO);
        engine.register(CommandHelpProvider::new(
            Arc::new(SpecRegistry::load(None)),
            Arc::clone(&commands),
            Arc::clone(&cache),
        ));
        engine.register(crate::providers::SessionCommandProvider);
        engine.register(crate::providers::PathCommandProvider::new(commands));

        let rows: Vec<_> = engine
            .complete(&context("hokan", 1))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["hokan config"]);

        let rows: Vec<_> = engine
            .complete(&context("hokan config a", 2))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["hokan config ai"]);
    }

    #[test]
    fn standalone_prompt_clis_offer_help_rows_without_a_space() {
        let directory = tempfile::tempdir().expect("command directory");
        let path = directory.path().join("codex");
        fs::write(&path, b"#!/bin/sh\n").expect("fake command");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
            directory.path(),
        ))));
        let cache = Arc::new(CommandHelpCache::default());
        cache.seed(
            "codex",
            parse_help_output("codex", "Commands:\n  exec   Run non-interactively\n"),
        );
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(CommandHelpProvider::new(
            Arc::new(SpecRegistry::load(None)),
            commands,
            cache,
        ));

        let bare_rows: Vec<_> = engine
            .complete(&context("codex", 27))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(bare_rows, ["codex exec"]);
        let rows: Vec<_> = engine
            .complete(&context("codex ", 28))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["codex exec"]);
    }

    #[test]
    fn swift_repl_command_offers_subcommands_without_a_space() {
        let directory = tempfile::tempdir().expect("command directory");
        let path = directory.path().join("swift");
        fs::write(&path, b"#!/bin/sh\n").expect("fake command");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
            directory.path(),
        ))));
        let cache = Arc::new(CommandHelpCache::default());
        cache.seed(
            "swift",
            parse_help_output(
                "swift",
                "Subcommands:\n  swift build   Build packages\n  swift run     Run a product\n",
            ),
        );
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(CommandHelpProvider::new(
            Arc::new(SpecRegistry::load(None)),
            commands,
            cache,
        ));

        let bare_rows: Vec<_> = engine
            .complete(&context("swift", 31))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(bare_rows, ["swift run", "swift build"]);
        let rows: Vec<_> = engine
            .complete(&context("swift ", 32))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["swift build", "swift run"]);
    }

    #[test]
    fn engine_descends_through_confirmed_help_subcommands() {
        let directory = tempfile::tempdir().expect("command directory");
        let path = directory.path().join("gh");
        fs::write(&path, b"#!/bin/sh\n").expect("fake command");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
            directory.path(),
        ))));
        let cache = Arc::new(CommandHelpCache::default());
        cache.seed("gh", parse_help_output("gh", GH_HELP));
        cache.seed_scope(
            "gh",
            &["pr"],
            parse_help_output_for_scope("gh", &["pr".to_owned()], GH_PR_HELP),
        );
        cache.seed_scope(
            "gh",
            &["pr", "create"],
            parse_help_output_for_scope(
                "gh",
                &["pr".to_owned(), "create".to_owned()],
                "Commands:\n  deep     Continue into a fourth level\nOptions:\n  --fill     Use commit information for title and body\n",
            ),
        );
        cache.seed_scope(
            "gh",
            &["pr", "create", "deep"],
            parse_help_output_for_scope(
                "gh",
                &["pr".to_owned(), "create".to_owned(), "deep".to_owned()],
                "Options:\n  --final     Finish the nested operation\n",
            ),
        );
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(CommandHelpProvider::new(
            Arc::new(SpecRegistry::load(None)),
            commands,
            cache,
        ));

        let rows: Vec<_> = engine
            .complete(&context("gh pr ", 40))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["gh pr create", "gh pr list"]);

        let rows: Vec<_> = engine
            .complete(&context("gh pr cr", 41))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["gh pr create"]);
        assert!(
            engine
                .complete(&context("gh pr zzz", 42))
                .candidates
                .is_empty()
        );

        let rows: Vec<_> = engine
            .complete(&context("gh pr create --f", 43))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["gh pr create --fill"]);

        let rows: Vec<_> = engine
            .complete(&context("gh pr create deep --f", 44))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["gh pr create deep --final"]);
    }

    #[test]
    fn explicit_executable_paths_receive_dynamic_help() {
        let directory = tempfile::tempdir().expect("command directory");
        let path = directory.path().join("demotool");
        fs::write(&path, b"#!/bin/sh\n").expect("fake command");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
        let commands = Arc::new(CommandPathCache::default());
        let cache = Arc::new(CommandHelpCache::default());
        cache.seed(
            "./demotool",
            parse_help_output("demotool", "Commands:\n  deploy   Ship it\n"),
        );
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(CommandHelpProvider::new(
            Arc::new(SpecRegistry::load(None)),
            commands,
            cache,
        ));
        let mut spaced = context("./demotool ", 44);
        spaced.cwd = Arc::new(directory.path().to_owned());
        let rows: Vec<_> = engine
            .complete(&spaced)
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["./demotool deploy"]);

        let mut bare = context("./demotool", 45);
        bare.cwd = Arc::new(directory.path().to_owned());
        let rows: Vec<_> = engine
            .complete(&bare)
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["./demotool deploy"]);
    }

    #[test]
    fn cold_help_probe_never_executes_project_paths_or_build_wrappers() {
        let directory = tempfile::tempdir().expect("command directory");
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        for name in ["gradlew", "mvnw"] {
            let path = bin.join(name);
            fs::write(&path, b"#!/bin/sh\nexit 99\n").expect("wrapper");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("wrapper mode");
        }
        let local = directory.path().join("demotool");
        fs::write(&local, b"#!/bin/sh\nexit 99\n").expect("local executable");
        fs::set_permissions(&local, fs::Permissions::from_mode(0o700)).expect("local mode");
        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(&bin))));
        let cache = Arc::new(CommandHelpCache::default());
        let provider = CommandHelpProvider::new(
            Arc::new(SpecRegistry::load(None)),
            commands,
            Arc::clone(&cache),
        );

        for text in ["gradlew ", "mvnw ", "./demotool "] {
            let mut current = context(text, 46);
            current.cwd = Arc::new(directory.path().to_owned());
            assert!(!provider.applies(&current), "{text:?} must stay cache-only");
        }
        assert_eq!(cache.fetch_count(), 0);
    }

    #[test]
    fn rustc_toolchain_selector_does_not_block_documented_flags() {
        let help = CommandHelp {
            flags: vec![HelpEntry {
                name: "--edition".into(),
                description: String::new(),
                takes_value: true,
            }],
            ..CommandHelp::default()
        };
        assert_eq!(
            help_position_for_arguments("rustc", true, &help, &["+nightly"], "--ed"),
            Some(HelpPosition::Flags)
        );
    }

    #[test]
    fn homebrew_help_completes_bare_space_and_strict_prefix_positions() {
        let directory = tempfile::tempdir().expect("command directory");
        let path = directory.path().join("brew");
        fs::write(&path, b"#!/bin/sh\n").expect("fake command");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("command mode");
        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
            directory.path(),
        ))));
        let cache = Arc::new(CommandHelpCache::default());
        cache.seed("brew", parse_help_output("brew", BREW_HELP));
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(CommandHelpProvider::new(
            Arc::new(SpecRegistry::load(None)),
            commands,
            cache,
        ));

        for (text, query) in [("brew", 30), ("brew ", 31)] {
            let rows: Vec<_> = engine
                .complete(&context(text, query))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect();
            assert!(rows.contains(&"brew install".to_owned()), "rows: {rows:?}");
            assert!(rows.contains(&"brew update".to_owned()), "rows: {rows:?}");
        }

        let rows: Vec<_> = engine
            .complete(&context("brew in", 32))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["brew info", "brew install"]);
        assert!(
            engine
                .complete(&context("brew zzz", 33))
                .candidates
                .is_empty()
        );
    }

    fn context(text: &str, query: u64) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(query),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            BufferSnapshot::new(
                text,
                text.len(),
                BufferRevision::new(query),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context")
    }
}
