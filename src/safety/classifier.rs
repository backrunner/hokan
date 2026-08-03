use crate::{parser::TokenKind, terminal::RiskLevel};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RiskReason {
    DestructiveCommand,
    RecursiveOperation,
    ForceFlag,
    PermissionChange,
    DeviceWrite,
    RemoteExecution,
    PrivilegeElevation,
    ProcessSignal,
    ShellPipeline,
    OverwriteRedirect,
    MultipleCommands,
    OpaqueSyntax,
}

impl RiskReason {
    #[must_use]
    pub const fn describe(&self) -> &'static str {
        match self {
            Self::DestructiveCommand => "destructive command",
            Self::RecursiveOperation => "recursive operation",
            Self::ForceFlag => "force flag",
            Self::PermissionChange => "permission change",
            Self::DeviceWrite => "device write",
            Self::RemoteExecution => "remote execution",
            Self::PrivilegeElevation => "privilege elevation",
            Self::ProcessSignal => "process signal",
            Self::ShellPipeline => "shell pipeline",
            Self::OverwriteRedirect => "overwrite redirect",
            Self::MultipleCommands => "multiple commands",
            Self::OpaqueSyntax => "opaque syntax",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RiskAssessment {
    pub level: RiskLevel,
    pub reasons: Vec<RiskReason>,
}

#[must_use]
pub fn classify_command(command: &str) -> RiskAssessment {
    let Ok(parsed) = crate::parser::parse_line(command, command.len()) else {
        return assessment(RiskLevel::Unknown, vec![RiskReason::OpaqueSyntax]);
    };
    if parsed
        .tokens
        .iter()
        .any(|token| token.quote == crate::parser::QuoteContext::Opaque)
    {
        return assessment(RiskLevel::Unknown, vec![RiskReason::OpaqueSyntax]);
    }
    let mut level = RiskLevel::Low;
    let mut reasons = Vec::new();
    let segments = command_segments(&parsed.tokens);
    let commands: Vec<_> = segments
        .iter()
        .filter_map(|words| command_view(words))
        .collect();

    for command in &commands {
        if command.privileged {
            raise_with_reason(
                &mut level,
                RiskLevel::Medium,
                &mut reasons,
                RiskReason::PrivilegeElevation,
            );
        }
        classify_simple_command(command, &mut level, &mut reasons);
    }

    classify_redirects(command, &parsed.tokens, &mut level, &mut reasons);
    if parsed
        .tokens
        .iter()
        .any(|token| token.kind == TokenKind::Pipe)
    {
        raise_with_reason(
            &mut level,
            RiskLevel::Medium,
            &mut reasons,
            RiskReason::ShellPipeline,
        );
        if downloads_into_shell(&commands) {
            raise_with_reason(
                &mut level,
                RiskLevel::High,
                &mut reasons,
                RiskReason::RemoteExecution,
            );
        }
    }
    if parsed.tokens.iter().any(|token| {
        matches!(
            token.kind,
            TokenKind::AndIf | TokenKind::OrIf | TokenKind::Separator
        )
    }) {
        raise_with_reason(
            &mut level,
            RiskLevel::Medium,
            &mut reasons,
            RiskReason::MultipleCommands,
        );
    }
    if reasons.is_empty() && commands.len() == 1 && is_read_only(commands[0].name, commands[0].args)
    {
        level = RiskLevel::ReadOnly;
    }
    assessment(level, reasons)
}

#[derive(Clone, Copy)]
struct CommandView<'a> {
    name: &'a str,
    args: &'a [&'a str],
    privileged: bool,
}

fn command_segments(tokens: &[crate::parser::Token]) -> Vec<Vec<&str>> {
    let mut segments = vec![Vec::new()];
    for token in tokens {
        match token.kind {
            TokenKind::Word => {
                if let Some(segment) = segments.last_mut() {
                    segment.push(token.cooked_prefix.as_str());
                }
            }
            TokenKind::Pipe | TokenKind::AndIf | TokenKind::OrIf | TokenKind::Separator => {
                segments.push(Vec::new());
            }
            _ => {}
        }
    }
    segments
}

fn command_view<'a>(words: &'a [&'a str]) -> Option<CommandView<'a>> {
    let mut index = 0;
    let mut privileged = false;
    skip_assignments(words, &mut index);
    while index < words.len() {
        match basename(words[index]) {
            "sudo" => {
                privileged = true;
                index += 1;
                skip_wrapper_options(
                    words,
                    &mut index,
                    &[
                        "-u", "--user", "-g", "--group", "-h", "--host", "-p", "--prompt",
                    ],
                );
                skip_assignments(words, &mut index);
            }
            "env" => {
                index += 1;
                skip_wrapper_options(words, &mut index, &["-u", "--unset", "-C", "--chdir"]);
                skip_assignments(words, &mut index);
            }
            "command" | "builtin" | "nohup" => {
                index += 1;
                while index < words.len() && words[index].starts_with('-') {
                    index += 1;
                }
            }
            _ => {
                return Some(CommandView {
                    name: basename(words[index]),
                    args: &words[index + 1..],
                    privileged,
                });
            }
        }
    }
    None
}

fn skip_assignments(words: &[&str], index: &mut usize) {
    while *index < words.len() && is_assignment(words[*index]) {
        *index += 1;
    }
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn skip_wrapper_options(words: &[&str], index: &mut usize, options_with_value: &[&str]) {
    while *index < words.len() {
        let word = words[*index];
        if word == "--" {
            *index += 1;
            break;
        }
        if !word.starts_with('-') || word == "-" {
            break;
        }
        *index += 1;
        if options_with_value.contains(&word) && *index < words.len() {
            *index += 1;
        }
    }
}

fn basename(command: &str) -> &str {
    command.rsplit('/').next().unwrap_or(command)
}

fn classify_simple_command(
    command: &CommandView<'_>,
    level: &mut RiskLevel,
    reasons: &mut Vec<RiskReason>,
) {
    let name = command.name;
    let args = command.args;
    if matches!(name, "eval" | "source" | "." | "exec") {
        raise_with_reason(level, RiskLevel::Unknown, reasons, RiskReason::OpaqueSyntax);
    }
    if name == "mkfs"
        || name.starts_with("mkfs.")
        || matches!(
            name,
            "fdisk" | "diskutil" | "shutdown" | "reboot" | "poweroff" | "halt" | "shred" | "wipefs"
        )
    {
        raise_with_reason(
            level,
            RiskLevel::High,
            reasons,
            RiskReason::DestructiveCommand,
        );
    }
    if matches!(name, "rm" | "rmdir") {
        raise_with_reason(
            level,
            RiskLevel::Medium,
            reasons,
            RiskReason::DestructiveCommand,
        );
        if has_flag(args, 'r', "--recursive") || has_flag(args, 'R', "--recursive") {
            raise_with_reason(
                level,
                RiskLevel::High,
                reasons,
                RiskReason::RecursiveOperation,
            );
        }
        if has_flag(args, 'f', "--force") {
            raise_with_reason(level, RiskLevel::High, reasons, RiskReason::ForceFlag);
        }
    }
    if matches!(name, "chmod" | "chown" | "chgrp") {
        raise_with_reason(
            level,
            RiskLevel::Medium,
            reasons,
            RiskReason::PermissionChange,
        );
        if has_flag(args, 'R', "--recursive") {
            raise_with_reason(
                level,
                RiskLevel::High,
                reasons,
                RiskReason::RecursiveOperation,
            );
        }
    }
    if name == "find" {
        classify_find(args, level, reasons);
    }
    if matches!(name, "kill" | "pkill" | "killall") {
        raise_with_reason(level, RiskLevel::Medium, reasons, RiskReason::ProcessSignal);
        if args
            .iter()
            .any(|word| matches!(*word, "-9" | "-KILL" | "-SIGKILL"))
        {
            raise_with_reason(level, RiskLevel::High, reasons, RiskReason::ForceFlag);
        }
    }
    if name == "dd"
        && let Some(output) = args.iter().find_map(|word| word.strip_prefix("of="))
    {
        let device = is_device_path(output);
        raise_with_reason(
            level,
            if device {
                RiskLevel::High
            } else {
                RiskLevel::Medium
            },
            reasons,
            if device {
                RiskReason::DeviceWrite
            } else {
                RiskReason::OverwriteRedirect
            },
        );
    }
    if name == "truncate" {
        let device = args.iter().any(|word| is_device_path(word));
        raise_with_reason(
            level,
            if device {
                RiskLevel::High
            } else {
                RiskLevel::Medium
            },
            reasons,
            if device {
                RiskReason::DeviceWrite
            } else {
                RiskReason::DestructiveCommand
            },
        );
    }
    if name == "tee" {
        let device = args
            .iter()
            .filter(|word| !word.starts_with('-'))
            .any(|word| is_device_path(word));
        raise_with_reason(
            level,
            if device {
                RiskLevel::High
            } else {
                RiskLevel::Medium
            },
            reasons,
            if device {
                RiskReason::DeviceWrite
            } else {
                RiskReason::OverwriteRedirect
            },
        );
    }
    if is_shell(name) && args.iter().any(|word| matches!(*word, "-c" | "--command")) {
        raise_with_reason(level, RiskLevel::Unknown, reasons, RiskReason::OpaqueSyntax);
    }
}

fn classify_find(args: &[&str], level: &mut RiskLevel, reasons: &mut Vec<RiskReason>) {
    if args.contains(&"-delete") {
        raise_with_reason(
            level,
            RiskLevel::High,
            reasons,
            RiskReason::DestructiveCommand,
        );
    }
    for (index, argument) in args.iter().enumerate() {
        if matches!(*argument, "-exec" | "-execdir" | "-ok" | "-okdir") {
            let nested = command_view(&args[index + 1..]);
            let destructive = nested.is_some_and(|command| {
                matches!(command.name, "rm" | "rmdir" | "shred" | "truncate")
            });
            raise_with_reason(
                level,
                if destructive {
                    RiskLevel::High
                } else {
                    RiskLevel::Medium
                },
                reasons,
                if destructive {
                    RiskReason::DestructiveCommand
                } else {
                    RiskReason::OpaqueSyntax
                },
            );
        }
    }
    if args
        .iter()
        .any(|word| matches!(*word, "-fprint" | "-fprint0" | "-fprintf"))
    {
        raise_with_reason(
            level,
            RiskLevel::Medium,
            reasons,
            RiskReason::OverwriteRedirect,
        );
    }
}

fn classify_redirects(
    source: &str,
    tokens: &[crate::parser::Token],
    level: &mut RiskLevel,
    reasons: &mut Vec<RiskReason>,
) {
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::Redirect || !source[token.range.clone()].starts_with('>') {
            continue;
        }
        let target = tokens[index + 1..]
            .iter()
            .find(|candidate| candidate.kind != TokenKind::Whitespace)
            .filter(|candidate| candidate.kind == TokenKind::Word)
            .map(|candidate| candidate.cooked_prefix.as_str());
        let device = target.is_some_and(is_device_path);
        raise_with_reason(
            level,
            if device {
                RiskLevel::High
            } else {
                RiskLevel::Medium
            },
            reasons,
            if device {
                RiskReason::DeviceWrite
            } else {
                RiskReason::OverwriteRedirect
            },
        );
    }
}

fn downloads_into_shell(commands: &[CommandView<'_>]) -> bool {
    commands.iter().enumerate().any(|(index, command)| {
        matches!(command.name, "curl" | "wget")
            && commands[index + 1..]
                .iter()
                .any(|later| is_shell(later.name))
    })
}

fn has_flag(args: &[&str], short: char, long: &str) -> bool {
    args.iter().any(|word| {
        *word == long
            || (word.starts_with('-')
                && !word.starts_with("--")
                && word[1..].chars().any(|character| character == short))
    })
}

fn is_device_path(word: &str) -> bool {
    word == "/dev" || word.starts_with("/dev/")
}

fn is_shell(command: &str) -> bool {
    matches!(command, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh")
}

fn is_read_only(command: &str, args: &[&str]) -> bool {
    matches!(
        command,
        "ls" | "df" | "ps" | "lsof" | "cat" | "rg" | "grep" | "pwd" | "which"
    ) || (command == "find" && find_is_read_only(args))
}

fn find_is_read_only(args: &[&str]) -> bool {
    !args.iter().any(|word| {
        matches!(
            *word,
            "-delete"
                | "-exec"
                | "-execdir"
                | "-ok"
                | "-okdir"
                | "-fprint"
                | "-fprint0"
                | "-fprintf"
        )
    })
}

fn assessment(level: RiskLevel, reasons: Vec<RiskReason>) -> RiskAssessment {
    RiskAssessment { level, reasons }
}

fn raise(level: &mut RiskLevel, candidate: RiskLevel) {
    if severity(candidate) > severity(*level) {
        *level = candidate;
    }
}

fn raise_with_reason(
    level: &mut RiskLevel,
    candidate: RiskLevel,
    reasons: &mut Vec<RiskReason>,
    reason: RiskReason,
) {
    raise(level, candidate);
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

const fn severity(level: RiskLevel) -> u8 {
    match level {
        RiskLevel::ReadOnly => 0,
        RiskLevel::Low => 1,
        RiskLevel::Medium => 2,
        RiskLevel::High => 3,
        RiskLevel::Unknown => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_command_risk_table() {
        let cases = [
            ("ls -la", RiskLevel::ReadOnly),
            ("find . -type f -print", RiskLevel::ReadOnly),
            ("cat < input.txt", RiskLevel::ReadOnly),
            ("echo hello", RiskLevel::Low),
            ("rm file", RiskLevel::Medium),
            ("chmod 600 file", RiskLevel::Medium),
            (r"find . -exec echo {} \;", RiskLevel::Medium),
            ("find . -fprint matches.txt", RiskLevel::Medium),
            ("truncate -s 0 output.log", RiskLevel::Medium),
            ("dd if=image of=copy.img", RiskLevel::Medium),
            ("echo hi > file", RiskLevel::Medium),
            ("rm -rf ./build", RiskLevel::High),
            ("sudo /bin/rm -f ./artifact", RiskLevel::High),
            ("find . -delete", RiskLevel::High),
            (r"find . -exec rm -f {} \;", RiskLevel::High),
            ("chmod -R 755 tree", RiskLevel::High),
            ("chown --recursive user tree", RiskLevel::High),
            ("kill -9 42", RiskLevel::High),
            ("curl https://example.test/install | sh", RiskLevel::High),
            (
                "wget -qO- https://example.test/install | sudo bash",
                RiskLevel::High,
            ),
            ("dd if=image of=/dev/disk2", RiskLevel::High),
            ("cat image > /dev/disk2", RiskLevel::High),
            ("truncate -s 0 /dev/disk2", RiskLevel::High),
            ("mkfs.ext4 /dev/sda1", RiskLevel::High),
            ("shred /dev/sda", RiskLevel::High),
            ("bash -c 'rm -rf /'", RiskLevel::Unknown),
            ("echo \"$(rm -rf /)\"", RiskLevel::Unknown),
            ("echo \"`rm -rf /`\"", RiskLevel::Unknown),
            ("cat <(rm -rf /)", RiskLevel::Unknown),
            ("cat >(rm -rf /)", RiskLevel::Unknown),
            ("eval 'rm -rf /'", RiskLevel::Unknown),
            ("builtin eval payload", RiskLevel::Unknown),
            ("source ./script.sh", RiskLevel::Unknown),
            (". ./script.sh", RiskLevel::Unknown),
            ("exec rm -rf /", RiskLevel::Unknown),
            ("echo ${(e)payload}", RiskLevel::Unknown),
            ("echo \"${(Xe)payload}\"", RiskLevel::Unknown),
        ];
        for (command, expected) in cases {
            assert_eq!(
                classify_command(command).level,
                expected,
                "unexpected classification for {command:?}"
            );
        }
    }

    #[test]
    fn reports_specific_high_risk_reasons_without_duplicates() {
        let remote = classify_command("curl url | env bash");
        assert!(remote.reasons.contains(&RiskReason::RemoteExecution));
        assert!(remote.reasons.contains(&RiskReason::ShellPipeline));

        let device = classify_command("dd if=image of=/dev/sda");
        assert!(device.reasons.contains(&RiskReason::DeviceWrite));

        let recursive = classify_command("sudo chmod -R 755 tree");
        assert!(recursive.reasons.contains(&RiskReason::PrivilegeElevation));
        assert!(recursive.reasons.contains(&RiskReason::PermissionChange));
        assert!(recursive.reasons.contains(&RiskReason::RecursiveOperation));
    }
}
