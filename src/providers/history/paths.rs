//! Live existence checks for standalone history commands with one path operand.
//!
//! Creation commands and compound shell lines are deliberately excluded: an
//! earlier command or redirection may create the target before it is used.
use std::{io::ErrorKind, path::PathBuf};

use crate::{
    completion::CompletionContext,
    parser::{EffectiveCommandKind, EffectiveCommandState, QuoteContext, TokenKind},
    shell::ShellAliases,
};

pub(super) fn single_path_is_plausible(
    context: &CompletionContext,
    command: &str,
    words: &[String],
    aliases: &ShellAliases,
) -> bool {
    let cooked = words.iter().map(String::as_str).collect::<Vec<_>>();
    let analysis =
        crate::parser::effective_command_analysis_for_shell(&cooked, false, context.shell);
    let EffectiveCommandState::Found(command_index) = analysis.state else {
        return true;
    };
    let word = cooked[command_index];
    // A function or alias can change argument semantics. External wrappers
    // bypass shell definitions, so `sudo rm …` can still be checked.
    if analysis.opaque
        || (analysis.kind == EffectiveCommandKind::Shell && aliases.contains(word))
        || cooked[..command_index].iter().any(|word| {
            matches!(
                crate::providers::executable_basename(word),
                "xargs" | "watch"
            )
        })
    {
        return true;
    }
    let name = crate::providers::executable_basename(word);
    let Some(options) = path_options(name) else {
        return true;
    };
    let Some(operand_index) = single_operand(&cooked, command_index + 1, &options) else {
        return true;
    };
    let value = cooked[operand_index];
    // Directory-stack selectors and stdin are not filesystem operands.
    if value == "-"
        || (name == "pushd"
            && value.starts_with(['+', '-'])
            && value[1..].chars().all(|c| c.is_ascii_digit()))
    {
        return true;
    }
    // Bare cd targets may be resolved through the shell's directory search path.
    if matches!(name, "cd" | "pushd")
        && !value.contains('/')
        && ["CDPATH", "cdpath"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
    {
        return true;
    }
    let Ok(parsed) = crate::parser::parse_line(command, command.len()) else {
        return true;
    };
    if parsed.quote != QuoteContext::Unquoted
        || parsed
            .tokens
            .iter()
            .any(|token| token.kind == TokenKind::Redirect)
    {
        return true;
    }
    let tokens = crate::parser::semantic_word_tokens(&parsed.tokens, &(0..command.len()));
    let Some(token) = tokens.get(operand_index) else {
        return true;
    };
    let raw = &command[token.range.clone()];
    let mut base = context.cwd.as_ref().clone();
    // Resolve wrapper cwd changes only when their spelling is unambiguous.
    // In particular, never turn `env -C "$PROJECT"` into a literal directory.
    for directory in
        crate::parser::wrapper_working_directories_for_shell(&cooked, command_index, context.shell)
    {
        if directory.starts_with('~') {
            return true;
        }
        let Some(resolved) = literal_path(&base, directory, directory) else {
            return true;
        };
        base = resolved;
    }
    let Some(target) = literal_path(&base, value, raw) else {
        return true;
    };
    // A dangling symlink is still a valid rm/readlink operand. Permission and
    // I/O errors do not prove absence and must not hide otherwise valid history.
    let metadata = if matches!(name, "cd" | "pushd" | "rmdir") {
        std::fs::metadata(target)
    } else {
        std::fs::symlink_metadata(target)
    };
    !matches!(metadata, Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory))
}

fn literal_path(base: &std::path::Path, value: &str, raw: &str) -> Option<PathBuf> {
    // Keep dynamic shell expressions and URLs. No expansion or subprocess is
    // ever executed while deciding whether to offer a history row.
    if value.is_empty()
        || raw.starts_with('=')
        || value
            .chars()
            .any(|c| matches!(c, '$' | '`' | '*' | '?' | '[' | '{' | '(' | ')'))
        || value.contains("://")
    {
        return None;
    }
    if raw.starts_with('~') {
        if value == "~" && raw == "~" {
            return std::env::home_dir();
        }
        if let Some(rest) = value.strip_prefix("~/")
            && raw.starts_with("~/")
        {
            return std::env::home_dir().map(|home| home.join(rest));
        }
        return None;
    }
    // Quoted/escaped tildes are literal names relative to cwd.
    Some(base.join(value))
}

struct PathOptions {
    short: &'static str,
    long: &'static [&'static str],
}

fn path_options(command: &str) -> Option<PathOptions> {
    // Only known valueless flags are consumed. Unknown/value-taking options
    // keep the row rather than misidentifying an option value as a path.
    let (short, long): (_, &[_]) = match command {
        "rm" => (
            "dfiIPRrvW",
            &[
                "force",
                "interactive",
                "recursive",
                "dir",
                "verbose",
                "one-file-system",
                "preserve-root",
                "no-preserve-root",
            ],
        ),
        "rmdir" => ("pv", &["parents", "verbose", "ignore-fail-on-non-empty"]),
        "cd" => ("LP", &[]),
        "pushd" => ("nLPqs", &[]),
        "ls" => (
            "1ABCFGHLOPRSUabcdfghiklmnopqrstuvx",
            &[
                "all",
                "almost-all",
                "directory",
                "human-readable",
                "recursive",
                "reverse",
                "color",
                "classify",
                "group-directories-first",
                "numeric-uid-gid",
            ],
        ),
        "du" => (
            "aHhLkPscmx",
            &[
                "all",
                "human-readable",
                "summarize",
                "total",
                "one-file-system",
                "apparent-size",
                "bytes",
            ],
        ),
        "tree" => (
            "adfFlpugDsihnNQqACrx",
            &["dirsfirst", "noreport", "inodes", "device"],
        ),
        "find" => ("HLPXdxs", &[]),
        "stat" => ("Llnqrsx", &["dereference", "terse"]),
        "file" => (
            "bchikLnNprsvzZ",
            &[
                "brief",
                "dereference",
                "mime",
                "mime-type",
                "mime-encoding",
                "keep-going",
                "no-pad",
                "special-files",
                "uncompress",
            ],
        ),
        "readlink" => (
            "nqsv",
            &[
                "canonicalize-existing",
                "no-newline",
                "quiet",
                "silent",
                "verbose",
            ],
        ),
        "open" => ("RnW", &[]),
        "xdg-open" => ("", &[]),
        _ => return None,
    };
    Some(PathOptions { short, long })
}

fn single_operand(words: &[&str], start: usize, options: &PathOptions) -> Option<usize> {
    let mut operand = None;
    let mut flags = true;
    for (index, word) in words.iter().enumerate().skip(start) {
        if flags && *word == "--" {
            flags = false;
        } else if flags && word.starts_with("--") {
            let flag = word[2..]
                .split_once('=')
                .map_or(&word[2..], |(flag, _)| flag);
            if !options.long.contains(&flag) {
                return None;
            }
        } else if flags && word.starts_with('-') && *word != "-" {
            if !word[1..].chars().all(|flag| options.short.contains(flag)) {
                return None;
            }
        } else if operand.replace(index).is_some() {
            return None;
        }
    }
    operand
}
