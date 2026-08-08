//! Static inference of a shell function's first-argument slot. A positional
//! parameter only becomes a completion source when its use has clear path or
//! executable semantics; guards and ordinary string uses are ignored.

use std::{ops::Range, path::PathBuf};

use super::ShellKind;
use crate::{
    completion::SlotKind,
    parser::{
        Token, TokenKind, effective_command_index_for_shell, parse_line, semantic_word_tokens,
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionSlot {
    pub kind: SlotKind,
    /// Literal base directory the argument is joined onto
    /// (`cd ~/projects/$1` -> `~/projects`); `None` for a bare `$1`.
    pub base: Option<PathBuf>,
}

pub(crate) fn infer_function_slot(shell: ShellKind, body: &str) -> Option<FunctionSlot> {
    let mut fallback = None;
    for line in body.lines() {
        let Ok(parsed) = parse_line(line, line.len()) else {
            continue;
        };
        let mut segment_start = 0;
        for token in &parsed.tokens {
            if token.kind == TokenKind::Comment {
                if let Some(slot) = inspect_segment(
                    shell,
                    line,
                    &parsed.tokens,
                    segment_start..token.range.start,
                    &mut fallback,
                ) {
                    return Some(slot);
                }
                break;
            }
            if is_segment_boundary(token.kind) {
                if let Some(slot) = inspect_segment(
                    shell,
                    line,
                    &parsed.tokens,
                    segment_start..token.range.start,
                    &mut fallback,
                ) {
                    return Some(slot);
                }
                segment_start = token.range.end;
            }
        }
        if let Some(slot) = inspect_segment(
            shell,
            line,
            &parsed.tokens,
            segment_start..line.len(),
            &mut fallback,
        ) {
            return Some(slot);
        }
    }
    fallback
}

fn inspect_segment(
    shell: ShellKind,
    line: &str,
    tokens: &[Token],
    range: Range<usize>,
    fallback: &mut Option<FunctionSlot>,
) -> Option<FunctionSlot> {
    let words = semantic_word_tokens(tokens, &range);
    let start = words
        .iter()
        .position(|word| !is_control_prefix(&word.cooked_prefix))?;
    let words = &words[start..];
    let cooked: Vec<&str> = words
        .iter()
        .map(|word| word.cooked_prefix.as_str())
        .collect();
    let command_index = effective_command_index_for_shell(&cooked, shell)?;
    let command_token = words[command_index];
    let command_raw = &line[command_token.range.clone()];

    if let Some(prefix) = active_parameter_prefix(command_raw, shell)
        && let Some(slot) = slot_with_prefix(SlotKind::Executable, &prefix)
    {
        return Some(slot);
    }

    let command = command_token
        .cooked_prefix
        .rsplit('/')
        .next()
        .unwrap_or(command_token.cooked_prefix.as_str());
    let semantics = command_semantics(command);
    for argument in words.iter().skip(command_index + 1) {
        let raw = &line[argument.range.clone()];
        let Some(prefix) = active_parameter_prefix(raw, shell) else {
            continue;
        };
        match semantics {
            CommandSemantics::Slot(kind) => {
                if let Some(slot) = slot_with_prefix(kind, &prefix) {
                    return Some(slot);
                }
            }
            CommandSemantics::NonPath => {}
            CommandSemantics::Unknown => {
                if let Some(base) = literal_base(&prefix)
                    && fallback.is_none()
                {
                    *fallback = Some(FunctionSlot {
                        kind: SlotKind::Path,
                        base: Some(base),
                    });
                }
            }
        }
    }
    None
}

const fn is_segment_boundary(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Pipe | TokenKind::AndIf | TokenKind::OrIf | TokenKind::Separator
    )
}

fn is_control_prefix(word: &str) -> bool {
    matches!(word, "then" | "else" | "do" | "{" | "(")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandSemantics {
    Slot(SlotKind),
    NonPath,
    Unknown,
}

fn command_semantics(command: &str) -> CommandSemantics {
    match command {
        "cd" | "pushd" | "rmdir" => CommandSemantics::Slot(SlotKind::Directory),
        "mkdir" => CommandSemantics::Slot(SlotKind::NewFile),
        "ls" | "cat" | "bat" | "less" | "more" | "head" | "tail" | "wc" | "sort" | "uniq"
        | "file" | "stat" | "du" | "cp" | "mv" | "rm" | "touch" | "ln" | "readlink"
        | "realpath" | "dirname" | "basename" | "open" | "xdg-open" | "vi" | "vim" | "nvim"
        | "nano" | "emacs" | "code" | "diff" | "cmp" | "tee" | "zip" | "unzip" | "rsync"
        | "scp" | "gzip" | "gunzip" | "bzip2" | "bunzip2" | "xz" | "unxz" | "sha1sum"
        | "sha256sum" | "md5" | "md5sum" | "gcc" | "clang" | "cc" | "g++" | "clang++"
        | "source" | "." | "bash" | "zsh" | "sh" | "python" | "python3" | "ruby" | "perl"
        | "node" => CommandSemantics::Slot(SlotKind::Path),
        // These consume control expressions or ordinary values. A path-like
        // string inside one must not override a later, meaningful path use.
        "if" | "elif" | "while" | "until" | "case" | "for" | "select" | "[" | "[[" | "test"
        | "echo" | "printf" | "print" | "read" | "return" | "exit" | "export" | "typeset"
        | "local" | "declare" | "readonly" | "unset" | "set" | "shift" | "let" | "eval"
        | "trap" | "true" | "false" | "sleep" => CommandSemantics::NonPath,
        _ => CommandSemantics::Unknown,
    }
}

fn slot_with_prefix(kind: SlotKind, prefix: &str) -> Option<FunctionSlot> {
    if prefix.is_empty() {
        return Some(FunctionSlot { kind, base: None });
    }
    literal_base(prefix).map(|base| FunctionSlot {
        kind,
        base: Some(base),
    })
}

/// Return the cooked portion before the first active first-parameter
/// expansion. Single-quoted and escaped dollar signs are literals.
fn active_parameter_prefix(raw: &str, shell: ShellKind) -> Option<String> {
    let index = active_parameter_index(raw, shell)?;
    parse_line(raw, index)
        .ok()
        .map(|parsed| parsed.current_prefix)
}

fn active_parameter_index(raw: &str, shell: ShellKind) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut index = 0;
    let mut quote = None;
    while index < bytes.len() {
        match (quote, bytes[index]) {
            (Some(b'\''), b'\'') => quote = None,
            (Some(b'\''), _) => {}
            (Some(b'"'), b'"') => quote = None,
            (Some(b'"'), b'\\') => {
                index = (index + 2).min(bytes.len());
                continue;
            }
            (Some(b'"'), b'$') if parameter_width(&bytes[index..], shell).is_some() => {
                return Some(index);
            }
            (Some(b'"'), _) => {}
            (None, b'\'' | b'"') => quote = Some(bytes[index]),
            (None, b'\\') => {
                index = (index + 2).min(bytes.len());
                continue;
            }
            (None, b'$') if parameter_width(&bytes[index..], shell).is_some() => {
                return Some(index);
            }
            (None, _) => {}
            (Some(_), _) => {}
        }
        index += 1;
    }
    None
}

fn parameter_width(bytes: &[u8], shell: ShellKind) -> Option<usize> {
    match shell {
        ShellKind::Fish => bytes.starts_with(b"$argv[1]").then_some(8),
        ShellKind::Zsh | ShellKind::Bash => {
            if bytes.starts_with(b"${1}") {
                Some(4)
            } else if bytes.starts_with(b"$1") && !bytes.get(2).is_some_and(u8::is_ascii_digit) {
                Some(2)
            } else {
                None
            }
        }
    }
}

/// Resolve a static directory prefix. Dynamic variables, globs, URL-shaped
/// values, and filename-only prefixes are deliberately left uninterpreted.
fn literal_base(prefix: &str) -> Option<PathBuf> {
    if prefix.is_empty()
        || !prefix.contains('/')
        || prefix.contains("//")
        || prefix
            .chars()
            .any(|character| matches!(character, '`' | '*' | '?' | '['))
    {
        return None;
    }
    let mut prefix = prefix.to_owned();
    while prefix.len() > 1 && prefix.ends_with('/') {
        prefix.pop();
    }
    let expanded = if prefix == "~" {
        std::env::home_dir()
    } else if let Some(rest) = prefix.strip_prefix("~/") {
        std::env::home_dir().map(|home| home.join(rest))
    } else if prefix == "$HOME" || prefix == "${HOME}" {
        std::env::home_dir()
    } else if let Some(rest) = prefix
        .strip_prefix("$HOME/")
        .or_else(|| prefix.strip_prefix("${HOME}/"))
    {
        std::env::home_dir().map(|home| home.join(rest))
    } else if prefix == "$PWD" || prefix == "${PWD}" {
        Some(PathBuf::new())
    } else if let Some(rest) = prefix
        .strip_prefix("$PWD/")
        .or_else(|| prefix.strip_prefix("${PWD}/"))
    {
        Some(PathBuf::from(rest))
    } else if prefix.contains('$') || prefix.starts_with('~') {
        None
    } else {
        Some(PathBuf::from(prefix))
    }?;
    Some(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cd_with_literal_base_gives_a_directory_slot() {
        let slot = infer_function_slot(ShellKind::Zsh, "cd ~/projects/$1").expect("slot");
        assert_eq!(slot.kind, SlotKind::Directory);
        assert_eq!(
            slot.base,
            std::env::home_dir().map(|home| home.join("projects"))
        );
    }

    #[test]
    fn guarded_real_world_proj_function_uses_the_cd_path() {
        let slot = infer_function_slot(
            ShellKind::Zsh,
            r#"if [ -n "$1" ]; then
  cd "/Volumes/BRData/projects/$1"
else
  cd /Volumes/BRData/projects
fi"#,
        )
        .expect("slot");
        assert_eq!(slot.kind, SlotKind::Directory);
        assert_eq!(slot.base, Some(PathBuf::from("/Volumes/BRData/projects")));
    }

    #[test]
    fn same_line_guards_and_boolean_chains_do_not_hide_path_uses() {
        let slot = infer_function_slot(
            ShellKind::Bash,
            r#"if [ -n "$1" ]; then cd "/tmp/my projects/$1"; fi"#,
        )
        .expect("slot");
        assert_eq!(slot.kind, SlotKind::Directory);
        assert_eq!(slot.base, Some(PathBuf::from("/tmp/my projects")));

        let chained = infer_function_slot(ShellKind::Zsh, r#"echo ready && cd ./work/$1"#)
            .expect("chained slot");
        assert_eq!(chained.kind, SlotKind::Directory);
        assert_eq!(chained.base, Some(PathBuf::from("./work")));
    }

    #[test]
    fn quoted_and_braced_parameters_are_found() {
        let slot = infer_function_slot(ShellKind::Zsh, "cd \"~/projects/${1}\"").expect("slot");
        assert_eq!(slot.kind, SlotKind::Directory);
        assert!(slot.base.is_some());
    }

    #[test]
    fn braced_home_and_pwd_bases_are_resolved() {
        let home = infer_function_slot(ShellKind::Zsh, "cd ${HOME}/projects/$1")
            .expect("braced HOME slot");
        assert_eq!(
            home.base,
            std::env::home_dir().map(|home| home.join("projects"))
        );

        let pwd =
            infer_function_slot(ShellKind::Bash, "cd $PWD/projects/$1").expect("PWD-relative slot");
        assert_eq!(pwd.base, Some(PathBuf::from("projects")));
    }

    #[test]
    fn known_path_commands_accept_bare_params_and_command_params_are_executables() {
        let slot = infer_function_slot(ShellKind::Bash, "vim $1").expect("slot");
        assert_eq!(slot.kind, SlotKind::Path);
        assert_eq!(slot.base, None);

        let executable = infer_function_slot(ShellKind::Zsh, r#"command "$1" --version"#)
            .expect("executable slot");
        assert_eq!(executable.kind, SlotKind::Executable);
        assert_eq!(executable.base, None);
    }

    #[test]
    fn directory_builtins_and_mkdir_use_precise_slots() {
        assert_eq!(
            infer_function_slot(ShellKind::Zsh, "pushd ~/projects/$1")
                .expect("pushd slot")
                .kind,
            SlotKind::Directory
        );
        assert_eq!(
            infer_function_slot(ShellKind::Bash, "rmdir ./tmp/$1")
                .expect("rmdir slot")
                .kind,
            SlotKind::Directory
        );
        assert_eq!(
            infer_function_slot(ShellKind::Fish, "mkdir -p work/$argv[1]")
                .expect("mkdir slot")
                .kind,
            SlotKind::NewFile
        );
    }

    #[test]
    fn fish_argv_parameter_is_supported() {
        let slot = infer_function_slot(ShellKind::Fish, "mkdir -p $argv[1]; and cd $argv[1]")
            .expect("slot");
        assert_eq!(slot.kind, SlotKind::NewFile);
    }

    #[test]
    fn non_path_and_literal_parameter_uses_do_not_invent_filesystem_slots() {
        assert!(infer_function_slot(ShellKind::Zsh, r#"echo "$1"; printf '%s' "$1""#).is_none());
        assert!(infer_function_slot(ShellKind::Zsh, r#"echo '$1'; echo \$1"#).is_none());
        assert!(infer_function_slot(ShellKind::Zsh, "git checkout $1").is_none());
        assert!(infer_function_slot(ShellKind::Zsh, "tool report-$1").is_none());

        let fallback = infer_function_slot(ShellKind::Zsh, "custom-tool ./inputs/$1")
            .expect("explicit path fallback");
        assert_eq!(fallback.kind, SlotKind::Path);
        assert_eq!(fallback.base, Some(PathBuf::from("./inputs")));
    }

    #[test]
    fn unresolved_dynamic_bases_are_not_treated_as_cwd_paths() {
        assert!(infer_function_slot(ShellKind::Zsh, "cd $PROJECTS/$1").is_none());
        assert!(infer_function_slot(ShellKind::Zsh, "cd https://example.test/$1").is_none());
    }

    #[test]
    fn multi_line_bodies_use_the_first_meaningful_parameter_segment() {
        let slot = infer_function_slot(
            ShellKind::Zsh,
            "echo $1\necho preparing\ncd ~/work/$1 && ls",
        )
        .expect("slot");
        assert_eq!(slot.kind, SlotKind::Directory);
        assert_eq!(
            slot.base,
            std::env::home_dir().map(|home| home.join("work"))
        );
    }

    #[test]
    fn unclear_bodies_yield_no_slot() {
        assert!(infer_function_slot(ShellKind::Zsh, "echo done").is_none());
        assert!(infer_function_slot(ShellKind::Zsh, "echo $@").is_none());
    }
}
