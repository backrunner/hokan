//! Validate historical arguments against cached documentation and known scopes.
use super::{
    CommandHelp, CommandHelpCache,
    probe::help_probe_allowed,
    scope::{MAX_HELP_SCOPE_DEPTH, help_flag_usage, supports_scoped_help},
};
use std::{path::PathBuf, sync::Arc};

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
                if !supports_scoped_help(command, &help) || scope.len() >= MAX_HELP_SCOPE_DEPTH {
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

pub(super) enum HistoryHelpStep<'a> {
    Subcommand { word: &'a str, consumed: usize },
    Done(bool),
}

pub(super) fn history_help_step<'a>(
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

pub(super) fn help_subcommand_typo(help: &CommandHelp, word: &str) -> bool {
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

pub(super) fn common_subcommand_variant(command: &str) -> Option<&'static str> {
    match command {
        "update" => Some("upgrade"),
        "upgrade" => Some("update"),
        _ => None,
    }
}

pub(super) fn has_dynamic_shell_syntax(word: &str) -> bool {
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
