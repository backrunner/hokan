use std::{env, path::PathBuf};

use crate::shell::ShellKind;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedHistory {
    pub command: String,
    pub timestamp_ms: Option<i64>,
}

#[must_use]
pub fn parse_history(shell: ShellKind, bytes: &[u8]) -> Vec<ImportedHistory> {
    let text = String::from_utf8_lossy(bytes);
    match shell {
        ShellKind::Zsh => parse_zsh(&text),
        ShellKind::Bash => parse_bash(&text),
        ShellKind::Fish => parse_fish(&text),
    }
}

#[must_use]
pub fn default_history_path(shell: ShellKind) -> Option<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from)?;
    match shell {
        ShellKind::Zsh => Some(
            env::var_os("HISTFILE")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".zsh_history")),
        ),
        ShellKind::Bash => Some(
            env::var_os("HISTFILE")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".bash_history")),
        ),
        ShellKind::Fish => Some(
            env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"))
                .join("fish/fish_history"),
        ),
    }
}

fn parse_zsh(text: &str) -> Vec<ImportedHistory> {
    let mut entries = Vec::new();
    let mut current: Option<ImportedHistory> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(": ")
            && let Some((metadata, command)) = rest.split_once(';')
        {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            let timestamp = metadata
                .split(':')
                .next()
                .and_then(|value| value.parse::<i64>().ok())
                .map(|seconds| seconds.saturating_mul(1_000));
            current = Some(ImportedHistory {
                command: command.to_owned(),
                timestamp_ms: timestamp,
            });
        } else if let Some(entry) = current.as_mut() {
            entry.command.push('\n');
            entry.command.push_str(line);
        } else if !line.trim().is_empty() {
            entries.push(ImportedHistory {
                command: line.to_owned(),
                timestamp_ms: None,
            });
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries
}

fn parse_bash(text: &str) -> Vec<ImportedHistory> {
    let mut timestamp = None;
    let mut entries = Vec::new();
    for line in text.lines() {
        if let Some(seconds) = line
            .strip_prefix('#')
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|value| value.parse::<i64>().ok())
        {
            timestamp = Some(seconds.saturating_mul(1_000));
        } else if !line.trim().is_empty() {
            entries.push(ImportedHistory {
                command: line.to_owned(),
                timestamp_ms: timestamp.take(),
            });
        }
    }
    entries
}

fn parse_fish(text: &str) -> Vec<ImportedHistory> {
    let mut entries: Vec<ImportedHistory> = Vec::new();
    for line in text.lines() {
        if let Some(command) = line.trim_start().strip_prefix("- cmd: ") {
            entries.push(ImportedHistory {
                command: unescape_fish(command),
                timestamp_ms: None,
            });
        } else if let Some(timestamp) = line.trim_start().strip_prefix("when: ")
            && let Some(last) = entries.last_mut()
        {
            last.timestamp_ms = timestamp
                .parse::<i64>()
                .ok()
                .map(|seconds| seconds.saturating_mul(1_000));
        }
    }
    entries
}

fn unescape_fish(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character == '\\' {
            match chars.next() {
                Some('n') => output.push('\n'),
                Some('\\') => output.push('\\'),
                Some(next) => {
                    output.push('\\');
                    output.push(next);
                }
                None => output.push('\\'),
            }
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_shell_formats() {
        assert_eq!(
            parse_history(ShellKind::Zsh, b": 100:0;git status\n: 101:0;ls\n")[0],
            ImportedHistory {
                command: "git status".into(),
                timestamp_ms: Some(100_000)
            }
        );
        assert_eq!(
            parse_history(ShellKind::Bash, b"#100\necho ok\n")[0].timestamp_ms,
            Some(100_000)
        );
        assert_eq!(
            parse_history(ShellKind::Fish, b"- cmd: echo\\nnext\n  when: 100\n")[0],
            ImportedHistory {
                command: "echo\nnext".into(),
                timestamp_ms: Some(100_000)
            }
        );
    }
}
