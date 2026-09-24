//! Help text parsing; input bytes and subprocess time are bounded by probing.
mod modern;
use super::{CommandHelp, HelpEntry};
#[cfg(test)]
pub(super) use modern::parse_help_output;
pub(crate) use modern::parse_help_output_for_scope;
use modern::split_subcommand_names;
use std::collections::HashSet;
pub(in crate::providers::command_help) const MAX_DESCRIPTION_CHARS: usize = 72;

/// Conservative heuristics over `man -P cat` output. Anything unrecognized is
/// skipped rather than guessed: a partial flag list is useful, a wrong one is
/// not.
pub(in crate::providers::command_help) fn parse_man_page(command: &str, text: &str) -> CommandHelp {
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
                if seen_flags.insert(name.clone()) {
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
            if seen_subcommands.insert(name.clone()) {
                help.subcommands.push(HelpEntry {
                    name,
                    description,
                    takes_value: false,
                });
            }
        } else if let Some((name, description)) = two_column_subcommand(line)
            && seen_subcommands.insert(name.clone())
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
            if seen_subcommands.insert(name.clone()) {
                help.subcommands.push(HelpEntry {
                    name,
                    description: shorten(&description),
                    takes_value: false,
                });
            }
            for alias in names {
                if seen_subcommands.insert(alias.clone()) {
                    help.subcommand_aliases.push(alias);
                }
            }
        }
    }
    help
}

pub(in crate::providers::command_help) fn is_command_section_header(line: &str) -> bool {
    let head = line.trim().split('(').next().unwrap_or_default().trim_end();
    matches!(head, "COMMANDS" | "SUBCOMMANDS" | "COMMAND LIST")
        || head.ends_with(" COMMANDS")
        || head.ends_with(" SUBCOMMANDS")
        || head.ends_with(" COMMAND LIST")
}

/// `man -P cat` output keeps troff overstrike markup: bold is `X\bX` and
/// underline is `_\bX`. Collapse each overprinted pair to the visible glyph.
pub(in crate::providers::command_help) fn strip_overstrike(text: &str) -> String {
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
pub(in crate::providers::command_help) fn is_section_header(line: &str) -> bool {
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
pub(in crate::providers::command_help) fn parse_flag_line(
    line: &str,
) -> Option<(Vec<String>, String)> {
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

pub(in crate::providers::command_help) fn is_flag_token(token: &str) -> bool {
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
pub(in crate::providers::command_help) fn inline_description(rest: &str) -> Option<String> {
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
pub(in crate::providers::command_help) fn flag_takes_separate_value(rest: &str) -> bool {
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

pub(in crate::providers::command_help) fn is_placeholder(word: &str) -> bool {
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
pub(in crate::providers::command_help) fn block_description(
    lines: &[String],
    entry_index: usize,
    entry_indent: usize,
) -> Option<String> {
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

pub(in crate::providers::command_help) fn indent_of(line: &str) -> usize {
    line.chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .count()
}

/// git(1) style: a `git-add(1)` reference line inside a COMMANDS section.
pub(in crate::providers::command_help) fn git_style_subcommand(
    command: &str,
    line: &str,
) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix(command)?.strip_prefix('-')?;
    let name = rest.strip_suffix("(1)")?;
    (is_entry_name(name)).then(|| name.to_owned())
}

/// Two-column `name  description` entries inside a COMMANDS section.
pub(in crate::providers::command_help) fn two_column_subcommand(
    line: &str,
) -> Option<(String, String)> {
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
pub(in crate::providers::command_help) fn man_signature_subcommand(
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

pub(in crate::providers::command_help) fn is_entry_name(name: &str) -> bool {
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
pub(in crate::providers::command_help) fn shorten(text: &str) -> String {
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
