//! Modern CLI command tables, usage signatures, aliases, and flags.
use super::super::probe::command_basename;
use super::{
    CommandHelp, HashSet, HelpEntry, block_description, flag_takes_separate_value, indent_of,
    inline_description, is_entry_name, parse_flag_line, shorten,
};
use std::path::Path;

/// Conservative heuristics over `--help` text (kubectl/docker/cobra, cargo
/// clap, and similar two-column layouts). Same philosophy as the man parser:
/// skip anything unrecognized rather than guess.
#[cfg(test)]
pub(in crate::providers::command_help) fn parse_help_output(
    command: &str,
    text: &str,
) -> CommandHelp {
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
                push_subcommand(&mut help, &mut seen_subcommands, name, description);
            }
            continue;
        }
        if in_usage {
            help.accepts_positionals |= usage_has_free_positional(command, scope, line);
            if let Some((name, description)) = help_invocation_subcommand(command, scope, line) {
                push_subcommand(&mut help, &mut seen_subcommands, name, description);
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
        if (in_commands || !scope.is_empty())
            && line.starts_with(char::is_whitespace)
            && let Some((name, description)) = help_invocation_subcommand(command, scope, line)
        {
            push_subcommand(&mut help, &mut seen_subcommands, name, description);
            continue;
        }
        if in_invocations {
            if let Some((name, description)) = help_invocation_subcommand(command, scope, line) {
                push_subcommand(&mut help, &mut seen_subcommands, name, description);
            }
            continue;
        }
        if in_argument_choices {
            if let Some((names, description)) = help_argument_choices(line) {
                saw_commands = true;
                for name in names {
                    push_subcommand(&mut help, &mut seen_subcommands, name, description.clone());
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
                    push_subcommand(&mut help, &mut seen_subcommands, name, String::new());
                }
            }
            continue;
        }
        if let Some(names) = help_comma_command_row(line) {
            for name in names {
                push_subcommand(&mut help, &mut seen_subcommands, name, String::new());
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
            );
            for alias in names {
                if seen_subcommands.insert(alias.clone()) {
                    help.subcommand_aliases.push(alias);
                }
            }
        }
    }
    help.subcommands_exhaustive = saw_commands && !help.subcommands.is_empty();
    help
}

pub(in crate::providers::command_help) fn push_subcommand(
    help: &mut CommandHelp,
    seen: &mut HashSet<String>,
    name: String,
    description: String,
) {
    if !seen.insert(name.clone()) {
        return;
    }
    help.subcommands.push(HelpEntry {
        name,
        description,
        takes_value: false,
    });
}

/// Flush-left `Commands:`-family headers used by cobra/clap-style help:
/// `Commands:`, `Available Commands:`, `Management Commands:`, and the
/// parenthesized `Commands (…)` variants.
pub(in crate::providers::command_help) fn is_commands_header(command: &str, line: &str) -> bool {
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

pub(in crate::providers::command_help) fn help_comma_command_row(
    line: &str,
) -> Option<Vec<String>> {
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

pub(in crate::providers::command_help) fn is_arguments_header(line: &str) -> bool {
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

pub(in crate::providers::command_help) fn usage_signature(line: &str) -> Option<&str> {
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

pub(in crate::providers::command_help) fn usage_has_free_positional(
    command: &str,
    scope: &[String],
    line: &str,
) -> bool {
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
pub(in crate::providers::command_help) fn is_invocation_section_header(line: &str) -> bool {
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

pub(in crate::providers::command_help) fn help_invocation_subcommand(
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
pub(in crate::providers::command_help) fn is_help_section_header(line: &str) -> bool {
    if line.starts_with(char::is_whitespace) {
        return false;
    }
    line.trim_end().ends_with(':') || is_all_caps_help_heading(line.trim())
}

/// Two-column `name   description` rows inside a `--help` commands section.
/// Unlike the man-page variant, hyphenated names (`api-versions`) and
/// clap/commander-style alias lists (`build, b`, `update|upgrade`) are
/// accepted and retained for validation without creating duplicate rows.
pub(in crate::providers::command_help) fn help_subcommand_row(
    line: &str,
) -> Option<(Vec<String>, String)> {
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

pub(in crate::providers::command_help) fn split_help_columns(line: &str) -> Option<(&str, &str)> {
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

pub(in crate::providers::command_help) fn is_all_caps_help_heading(line: &str) -> bool {
    let mut letters = 0;
    for character in line.chars().filter(char::is_ascii_alphabetic) {
        if !character.is_ascii_uppercase() {
            return false;
        }
        letters += 1;
    }
    letters >= 2
}

pub(in crate::providers::command_help) fn openssl_command_group(
    command: &str,
    line: &str,
) -> Option<bool> {
    if command_basename(command) != "openssl" || line.starts_with(char::is_whitespace) {
        return None;
    }
    let heading = line.trim().to_ascii_lowercase();
    (heading == "standard commands"
        || heading.starts_with("message digest commands")
        || heading.starts_with("cipher commands"))
    .then_some(heading == "standard commands")
}

pub(in crate::providers::command_help) fn help_command_grid_row(line: &str) -> Option<Vec<String>> {
    let names: Vec<_> = line.split_whitespace().map(str::to_owned).collect();
    (!names.is_empty() && names.iter().all(|name| is_entry_name(name))).then_some(names)
}

pub(in crate::providers::command_help) fn help_argument_choices(
    line: &str,
) -> Option<(Vec<String>, String)> {
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

pub(in crate::providers::command_help) fn split_subcommand_names(
    token: &str,
) -> Option<Vec<String>> {
    let names: Vec<_> = token
        .split(['|', ','])
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    (!names.is_empty() && names.iter().all(|name| is_entry_name(name))).then_some(names)
}

pub(in crate::providers::command_help) fn description_aliases(description: &str) -> Vec<String> {
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
