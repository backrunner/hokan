//! Static inference of a shell function's first-argument slot: where the
//! body uses `$1` (posix) or `$argv[1]` (fish), the command and any literal
//! path prefix around it decide what to complete there. Conservative — no
//! slot is returned when the usage is unclear.

use std::path::PathBuf;

use super::ShellKind;
use crate::completion::SlotKind;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionSlot {
    pub kind: SlotKind,
    /// Literal base directory the argument is joined onto
    /// (`cd ~/projects/$1` → `~/projects`); `None` for a bare `$1`.
    pub base: Option<PathBuf>,
}

pub(crate) fn infer_function_slot(shell: ShellKind, body: &str) -> Option<FunctionSlot> {
    // The first simple command that mentions the parameter decides.
    for segment in body.split([';', '\n']) {
        let segment = segment
            .split("&&")
            .next()
            .and_then(|part| part.split("||").next())
            .unwrap_or(segment);
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        let Some((command, argument, span)) = tokens.first().and_then(|command| {
            tokens.iter().skip(1).find_map(|argument| {
                param_span(argument, shell).map(|span| (*command, *argument, span))
            })
        }) else {
            continue;
        };
        let kind = match command {
            "cd" => SlotKind::Directory,
            "bash" | "zsh" | "sh" => SlotKind::Executable,
            "cat" | "less" | "bat" | "head" | "tail" | "vim" | "nvim" | "vi" | "nano" | "code" => {
                SlotKind::File
            }
            _ => SlotKind::Path,
        };
        let base = literal_base(&argument[..span.0]);
        return Some(FunctionSlot { kind, base });
    }
    None
}

/// Byte range of the first positional-parameter reference in a token:
/// `$1`/`${1}` (posix, excluding `$10`-style) or `$argv[1]` (fish).
fn param_span(token: &str, shell: ShellKind) -> Option<(usize, usize)> {
    match shell {
        ShellKind::Fish => token.find("$argv[1]").map(|index| (index, index + 8)),
        ShellKind::Zsh | ShellKind::Bash => {
            for pattern in ["${1}", "$1"] {
                if let Some(index) = token.find(pattern) {
                    let after = token.as_bytes().get(index + pattern.len());
                    if !after.is_some_and(u8::is_ascii_digit) {
                        return Some((index, index + pattern.len()));
                    }
                }
            }
            None
        }
    }
}

/// The literal path prefix joined before the parameter, with quotes and the
/// trailing slash stripped; `~`/`$HOME` expanded. `~/projects/` →
/// `~/projects`; empty → `None`.
fn literal_base(prefix: &str) -> Option<PathBuf> {
    let mut prefix = prefix.trim_matches(['\'', '"']).to_owned();
    while prefix.ends_with('/') {
        prefix.pop();
    }
    if prefix.is_empty() {
        return None;
    }
    let expanded = prefix
        .strip_prefix("~/")
        .and_then(|rest| std::env::home_dir().map(|home| home.join(rest)))
        .or_else(|| {
            prefix
                .strip_prefix("$HOME/")
                .and_then(|rest| std::env::home_dir().map(|home| home.join(rest)))
        })
        .unwrap_or_else(|| PathBuf::from(&prefix));
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
    fn quoted_and_braced_parameters_are_found() {
        let slot = infer_function_slot(ShellKind::Zsh, "cd \"~/projects/${1}\"").expect("slot");
        assert_eq!(slot.kind, SlotKind::Directory);
        assert!(slot.base.is_some());
    }

    #[test]
    fn editors_give_file_slots_and_bare_params_use_cwd() {
        let slot = infer_function_slot(ShellKind::Bash, "vim $1").expect("slot");
        assert_eq!(slot.kind, SlotKind::File);
        assert_eq!(slot.base, None);
    }

    #[test]
    fn fish_argv_parameter_is_supported() {
        let slot = infer_function_slot(ShellKind::Fish, "mkdir -p $argv[1]; and cd $argv[1]")
            .expect("slot");
        // First segment mentioning the parameter wins (mkdir → Path here,
        // because the mkdir segment precedes the cd segment after `;`).
        assert!(matches!(slot.kind, SlotKind::Path | SlotKind::Directory));
    }

    #[test]
    fn multi_line_bodies_use_the_first_parameter_segment() {
        let slot = infer_function_slot(ShellKind::Zsh, "echo preparing\ncd ~/work/$1 && ls")
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
