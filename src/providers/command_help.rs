use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
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
// per session, from the applies pass, outside the engine's local budget.
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandHelp {
    pub flags: Vec<HelpEntry>,
    pub subcommands: Vec<HelpEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpEntry {
    pub name: String,
    pub description: String,
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
#[derive(Default)]
pub struct CommandHelpCache {
    entries: Mutex<HashMap<String, Arc<CommandHelp>>>,
    fetches: AtomicUsize,
}

impl CommandHelpCache {
    /// Cached entry only; never runs `man`. Cheap enough for `applies`-time
    /// suppression checks in other providers.
    #[must_use]
    pub fn peek(&self, command: &str) -> Option<Arc<CommandHelp>> {
        lock(&self.entries).get(command).cloned()
    }

    /// Cache-first lookup; on a cold miss fetches synchronously once
    /// (bounded `man`, with a bounded `--help` fallback when the page yields
    /// nothing) and caches the outcome, including failures. The entries lock
    /// is held across the fetch so concurrent cold misses for the same
    /// command spawn a single fetch instead of one per caller.
    pub fn get(&self, command: &str) -> Arc<CommandHelp> {
        self.get_with(command, fetch_command_help)
    }

    fn get_with(&self, command: &str, fetch: impl Fn(&str) -> CommandHelp) -> Arc<CommandHelp> {
        let mut entries = lock(&self.entries);
        if let Some(help) = entries.get(command) {
            return help.clone();
        }
        self.fetches.fetch_add(1, Ordering::Relaxed);
        let fetched = Arc::new(fetch(command));
        entries.entry(command.to_owned()).or_insert(fetched).clone()
    }

    #[cfg(test)]
    pub(crate) fn seed(&self, command: &str, help: CommandHelp) {
        lock(&self.entries).insert(command.to_owned(), Arc::new(help));
    }

    #[cfg(test)]
    pub(crate) fn fetch_count(&self) -> usize {
        self.fetches.load(Ordering::Relaxed)
    }
}

fn lock(
    entries: &Mutex<HashMap<String, Arc<CommandHelp>>>,
) -> MutexGuard<'_, HashMap<String, Arc<CommandHelp>>> {
    entries.lock().unwrap_or_else(PoisonError::into_inner)
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
        // cannot execute, and never for the command token itself.
        if self.specs.get(command).is_some() || !self.commands.contains(command) {
            return false;
        }
        let Some((_words, position)) = argument_progress(context) else {
            return false;
        };
        if !context.parsed.current_prefix.starts_with('-') && position != 0 {
            return false;
        }
        // Warm the cache here, not in `complete`: the engine only starts the
        // `local_timeout_ms` budget after the applies pass, so the single
        // bounded `man` run on a cold miss cannot starve the providers that
        // follow (a cold fetch inside `complete` eats the whole ~100 ms
        // budget and the filesystem batch is skipped for that query).
        self.cache.get(command);
        true
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Some(command) = context.command() else {
            return ProviderOutput::default();
        };
        let Some((_words, position)) = argument_progress(context) else {
            return ProviderOutput::default();
        };
        let flags_position = context.parsed.current_prefix.starts_with('-');
        if !flags_position && position != 0 {
            return ProviderOutput::default();
        }
        let help = self.cache.get(command);
        let entries = if flags_position {
            &help.flags
        } else {
            &help.subcommands
        };
        let candidates = entries
            .iter()
            .map(|entry| {
                // The edit inserts only the bare word, but the row displays
                // the full command line (`kimi export`) so the list reads as
                // arguments of THIS command — bare words (`export`) look like
                // unrelated commands next to history rows.
                Candidate::new(
                    context.query_id,
                    format!("{command} {}", entry.name),
                    entry.description.as_str(),
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement: entry.name.clone(),
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
                    format!("man:{command}"),
                )
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
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

/// Full cold-miss fetch: try the man page, and only when it yields no flags
/// and no subcommands (kubectl has no man page at all; docker's page has no
/// COMMANDS section) fall back to a single bounded `<cmd> --help` run. An
/// empty fallback result is returned as-is so the negative-cache path still
/// guarantees at most one fetch per command per session.
fn fetch_command_help(command: &str) -> CommandHelp {
    fetch_with_fallback(command, fetch_man_page, fetch_help_output)
}

fn fetch_with_fallback(
    command: &str,
    man: impl Fn(&str) -> CommandHelp,
    help: impl Fn(&str) -> CommandHelp,
) -> CommandHelp {
    let parsed = man(command);
    if !parsed.flags.is_empty() || !parsed.subcommands.is_empty() {
        return parsed;
    }
    help(command)
}

/// Modern-CLI fallback: `<cmd> --help`, bounded exactly like the man probe —
/// the command resolves on PATH (the provider only fires for commands the
/// user can execute), gets no shell and no user-controlled argv beyond the
/// literal `--help`, reads null stdin, and dies on timeout. A failing,
/// hanging, or empty run degrades to an empty `CommandHelp`.
fn fetch_help_output(command: &str) -> CommandHelp {
    let Ok(output) =
        crate::platform::run_bounded(command, ["--help"], HELP_TIMEOUT, MAN_MAX_OUTPUT_BYTES)
    else {
        return CommandHelp::default();
    };
    if !output.status.success() || output.stdout.is_empty() {
        return CommandHelp::default();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_help_output(&text)
}

/// Conservative heuristics over `--help` text (kubectl/docker/cobra, cargo
/// clap, and similar two-column layouts). Same philosophy as the man parser:
/// skip anything unrecognized rather than guess.
fn parse_help_output(text: &str) -> CommandHelp {
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut help = CommandHelp::default();
    let mut seen_flags = HashSet::new();
    let mut seen_subcommands = HashSet::new();
    let mut in_commands = false;
    for (index, line) in lines.iter().enumerate() {
        if is_commands_header(line) {
            in_commands = true;
            continue;
        }
        if is_help_section_header(line) {
            in_commands = false;
            continue;
        }
        if let Some((names, rest)) = parse_flag_line(line) {
            let description = inline_description(&rest)
                .or_else(|| block_description(&lines, index, indent_of(line)))
                .map_or_else(String::new, |text| shorten(&text));
            for name in names {
                if seen_flags.insert(name.clone()) && help.flags.len() < MAX_ENTRIES {
                    help.flags.push(HelpEntry {
                        name,
                        description: description.clone(),
                    });
                }
            }
            continue;
        }
        if !in_commands {
            continue;
        }
        if let Some((name, description)) = help_subcommand_row(line)
            && seen_subcommands.insert(name.clone())
            && help.subcommands.len() < MAX_ENTRIES
        {
            help.subcommands.push(HelpEntry {
                name,
                description: shorten(&description),
            });
        }
    }
    help
}

/// Flush-left `Commands:`-family headers used by cobra/clap-style help:
/// `Commands:`, `Available Commands:`, `Management Commands:`, and the
/// parenthesized `Commands (…)` variants.
fn is_commands_header(line: &str) -> bool {
    if line.starts_with(char::is_whitespace) {
        return false;
    }
    let head = line
        .trim()
        .trim_end_matches(':')
        .split('(')
        .next()
        .unwrap_or_default()
        .trim_end();
    head == "Commands" || head.ends_with(" Commands")
}

/// Any other flush-left `Something:` line ends a commands section (`Flags:`,
/// `Options:`, `Global Flags:`, …). The commands header itself is matched
/// first by the caller.
fn is_help_section_header(line: &str) -> bool {
    !line.starts_with(char::is_whitespace) && line.trim_end().ends_with(':')
}

/// Two-column `name   description` rows inside a `--help` commands section.
/// Unlike the man-page variant, hyphenated names (`api-versions`) and
/// clap-style alias lists (`build, b`) are accepted — the alias after the
/// comma is dropped.
fn help_subcommand_row(line: &str) -> Option<(String, String)> {
    if line.starts_with('-') || !line.starts_with(char::is_whitespace) {
        return None;
    }
    let trimmed = line.trim_start();
    let name_end = trimmed.find(char::is_whitespace)?;
    let name_token = &trimmed[..name_end];
    let name = name_token.trim_end_matches(',');
    if !is_entry_name(name) {
        return None;
    }
    let mut rest = &trimmed[name_end..];
    // Alias rows (`build, b    Compile…`): skip the single-word alias so the
    // column gap is measured after it.
    if name_token.ends_with(',') {
        let alias = rest.trim_start();
        let alias_end = alias.find(char::is_whitespace)?;
        if !alias[..alias_end]
            .chars()
            .all(|c| c.is_ascii_alphanumeric())
        {
            return None;
        }
        rest = &alias[alias_end..];
    }
    let gap = rest
        .chars()
        .take_while(|character| *character == ' ')
        .count();
    if gap < 2 {
        return None;
    }
    let description = rest.trim_start();
    if description.is_empty() {
        return None;
    }
    Some((name.to_owned(), description.to_owned()))
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
            in_commands = line.trim().contains("COMMAND");
            continue;
        }
        if let Some((names, rest)) = parse_flag_line(line) {
            let description = inline_description(&rest)
                .or_else(|| block_description(&lines, index, indent_of(line)))
                .map_or_else(String::new, |text| shorten(&text));
            for name in names {
                if seen_flags.insert(name.clone()) && help.flags.len() < MAX_ENTRIES {
                    help.flags.push(HelpEntry {
                        name,
                        description: description.clone(),
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
                help.subcommands.push(HelpEntry { name, description });
            }
        } else if let Some((name, description)) = two_column_subcommand(line)
            && seen_subcommands.insert(name.clone())
            && help.subcommands.len() < MAX_ENTRIES
        {
            help.subcommands.push(HelpEntry {
                name,
                description: shorten(&description),
            });
        }
    }
    help
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

fn is_entry_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
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
    use std::{ffi::OsString, fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};

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

    #[test]
    fn parses_kubectl_style_help() {
        let help = parse_help_output(KUBECTL_HELP);
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
        let help = parse_help_output(DOCKER_HELP);
        let names: Vec<&str> = help
            .subcommands
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["builder", "container", "attach", "build"]);
        let flags: Vec<&str> = help.flags.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(flags, ["--config", "-D", "--debug"]);
        assert_eq!(help.flags[0].description, "Location of client config files");
    }

    #[test]
    fn parses_cargo_style_help_with_aliases() {
        let help = parse_help_output(CARGO_HELP);
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
    }

    #[test]
    fn help_garbage_yields_no_entries() {
        assert_eq!(parse_help_output(""), CommandHelp::default());
        assert_eq!(
            parse_help_output("just some prose\n"),
            CommandHelp::default()
        );
        // A commands header with no parseable rows still yields no
        // subcommands; rows outside any commands section are ignored.
        let orphan_rows = "  start   Start the service.\nCommands:\n\nFlags:\n";
        let help = parse_help_output(orphan_rows);
        assert!(help.subcommands.is_empty());
    }

    #[test]
    fn help_fallback_only_runs_when_the_man_page_is_empty() {
        use std::cell::Cell;

        let help_calls = Cell::new(0);
        let counting_help = |_: &str| {
            help_calls.set(help_calls.get() + 1);
            CommandHelp {
                flags: Vec::new(),
                subcommands: vec![HelpEntry {
                    name: "apply".into(),
                    description: String::new(),
                }],
            }
        };
        let man_with_flags = |_: &str| CommandHelp {
            flags: vec![HelpEntry {
                name: "-x".into(),
                description: String::new(),
            }],
            subcommands: Vec::new(),
        };
        // A non-empty man parse wins; `--help` is never spawned.
        let result = fetch_with_fallback("demo", man_with_flags, counting_help);
        assert_eq!(result.flags.len(), 1);
        assert_eq!(help_calls.get(), 0);
        // An empty man parse falls back to `--help` exactly once.
        let result = fetch_with_fallback("demo", |_| CommandHelp::default(), counting_help);
        assert_eq!(result.subcommands.len(), 1);
        assert_eq!(help_calls.get(), 1);
        // Both empty: the negative result is returned for caching.
        let result = fetch_with_fallback(
            "demo",
            |_| CommandHelp::default(),
            |_| CommandHelp::default(),
        );
        assert_eq!(result, CommandHelp::default());
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
        let provider = CommandHelpProvider::new(
            Arc::new(SpecRegistry::load(None)),
            commands,
            Arc::new(CommandHelpCache::default()),
        );
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
                flags: vec![HelpEntry {
                    name: "--paginate".into(),
                    description: "Pipe output into less.".into(),
                }],
                subcommands: vec![HelpEntry {
                    name: "checkout".into(),
                    description: "Switch branches.".into(),
                }],
            },
        );
        let provider = CommandHelpProvider::new(
            Arc::new(SpecRegistry::load(None)),
            commands,
            Arc::clone(&cache),
        );
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(provider);

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

        // Past the first argument without a dash the provider stays silent.
        let output = engine.complete(&context("git checkout ", 3));
        assert!(output.candidates.is_empty());
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
