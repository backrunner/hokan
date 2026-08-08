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
        if command.opaque || command.indeterminate {
            raise_with_reason(
                &mut level,
                RiskLevel::Unknown,
                &mut reasons,
                RiskReason::OpaqueSyntax,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EffectiveCommandInfo {
    pub(crate) word: String,
    pub(crate) kind: crate::parser::EffectiveCommandKind,
    pub(crate) indeterminate: bool,
}

/// The effective command of the first segment, after peeling assignments and
/// wrappers exactly as completion and the risk classifier do. The raw word is
/// preserved for explicit-path checks; `kind` records whether the surrounding
/// syntax accepts aliases/builtins or requires a PATH executable.
#[must_use]
pub(crate) fn effective_command_info_for_shell(
    command: &str,
    shell: crate::shell::ShellKind,
) -> Option<EffectiveCommandInfo> {
    let parsed = crate::parser::parse_line(command, command.len()).ok()?;
    if parsed
        .tokens
        .iter()
        .any(|token| token.quote == crate::parser::QuoteContext::Opaque)
    {
        return None;
    }
    let segments = command_segments(&parsed.tokens);
    let words = segments.first()?.as_slice();
    let analysis = crate::parser::effective_command_analysis_for_shell(words, false, shell);
    let (index, indeterminate) = match analysis.state {
        crate::parser::EffectiveCommandState::Found(index)
        | crate::parser::EffectiveCommandState::WrapperCommand(index) => (index, false),
        crate::parser::EffectiveCommandState::IndeterminateWrapper(index) => (index, true),
        crate::parser::EffectiveCommandState::AwaitingCommand
        | crate::parser::EffectiveCommandState::AwaitingWrapperValue => return None,
    };
    Some(EffectiveCommandInfo {
        word: words[index].to_owned(),
        kind: analysis.kind,
        indeterminate,
    })
}

#[must_use]
pub(crate) fn effective_command_word_for_shell(
    command: &str,
    shell: crate::shell::ShellKind,
) -> Option<String> {
    effective_command_info_for_shell(command, shell).map(|info| info.word)
}

#[derive(Clone, Copy)]
struct CommandView<'a> {
    name: &'a str,
    args: &'a [&'a str],
    privileged: bool,
    opaque: bool,
    indeterminate: bool,
}

fn command_segments(tokens: &[crate::parser::Token]) -> Vec<Vec<&str>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for token in tokens {
        if matches!(
            token.kind,
            TokenKind::Pipe | TokenKind::AndIf | TokenKind::OrIf | TokenKind::Separator
        ) {
            ranges.push(start..token.range.start);
            start = token.range.end;
        }
    }
    let end = tokens.last().map_or(start, |token| token.range.end);
    ranges.push(start..end);
    ranges
        .iter()
        .map(|range| {
            crate::parser::semantic_word_tokens(tokens, range)
                .into_iter()
                .map(|token| token.cooked_prefix.as_str())
                .collect()
        })
        .collect()
}

fn command_view<'a>(words: &'a [&'a str]) -> Option<CommandView<'a>> {
    let analysis = crate::parser::effective_command_analysis(words, false);
    let (mut index, mut indeterminate) = match analysis.state {
        crate::parser::EffectiveCommandState::Found(index)
        | crate::parser::EffectiveCommandState::WrapperCommand(index) => (index, false),
        crate::parser::EffectiveCommandState::IndeterminateWrapper(index) => (index, true),
        crate::parser::EffectiveCommandState::AwaitingCommand
        | crate::parser::EffectiveCommandState::AwaitingWrapperValue => return None,
    };
    if let Some(dispatched) = dispatched_command_index(words, index) {
        index = dispatched;
    } else if dispatches_command(basename(words[index]), words.get(index + 1).copied()) {
        indeterminate = true;
    }
    Some(CommandView {
        name: basename(words[index]),
        args: &words[index + 1..],
        privileged: analysis.privileged,
        opaque: analysis.opaque,
        indeterminate,
    })
}

fn dispatches_command(command: &str, first_argument: Option<&str>) -> bool {
    match command {
        "pnpm" | "yarn" => matches!(first_argument, Some("exec" | "dlx")),
        "npm" => first_argument == Some("exec"),
        "bun" => first_argument == Some("x"),
        "npx" => true,
        _ => false,
    }
}

fn dispatched_command_index(words: &[&str], command_index: usize) -> Option<usize> {
    let command = basename(words[command_index]);
    let mut index = match command {
        "pnpm" | "yarn"
            if matches!(words.get(command_index + 1).copied(), Some("exec" | "dlx")) =>
        {
            command_index + 2
        }
        "npm" if words.get(command_index + 1).copied() == Some("exec") => command_index + 2,
        "bun" if words.get(command_index + 1).copied() == Some("x") => command_index + 2,
        "npx" => command_index + 1,
        _ => return None,
    };
    while words.get(index).copied() == Some("--") {
        index += 1;
    }
    words
        .get(index)
        .is_some_and(|word| !word.starts_with('-'))
        .then_some(index)
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
        let text = &source[token.range.clone()];
        if token.kind != TokenKind::Redirect || !(text.starts_with('>') || text.starts_with("&>")) {
            continue;
        }
        let target = tokens[index + 1..]
            .iter()
            .find(|candidate| candidate.kind != TokenKind::Whitespace)
            .filter(|candidate| candidate.kind == TokenKind::Word)
            .map(|candidate| candidate.cooked_prefix.as_str());
        // `2>&1` / `2>&-` duplicate a descriptor; nothing is overwritten.
        if target.is_some_and(|word| word == "-" || word.bytes().all(|byte| byte.is_ascii_digit()))
        {
            continue;
        }
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
            ("doas -u root rm -rf ./artifact", RiskLevel::High),
            ("timeout 2 rm -rf ./artifact", RiskLevel::High),
            ("watch -n 1 rm -rf ./artifact", RiskLevel::High),
            ("time -f %E rm ./artifact", RiskLevel::Medium),
            ("nice -n 5 rm ./artifact", RiskLevel::Medium),
            ("stdbuf -oL rm ./artifact", RiskLevel::Medium),
            ("setsid -f rm ./artifact", RiskLevel::Medium),
            ("noglob rm ./artifact", RiskLevel::Medium),
            ("! rm ./artifact", RiskLevel::Medium),
            ("builtin command rm -rf ./artifact", RiskLevel::High),
            ("command command rm -rf ./artifact", RiskLevel::High),
            ("pnpm exec rm -rf ./artifact", RiskLevel::High),
            ("npm exec -- rm ./artifact", RiskLevel::Medium),
            ("yarn exec rm ./artifact", RiskLevel::Medium),
            ("npx rm ./artifact", RiskLevel::Medium),
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
            ("command -v rm", RiskLevel::Low),
            ("sudo -e /etc/hosts", RiskLevel::Medium),
            ("sudo --not-a-real-option value rm", RiskLevel::Unknown),
            ("env -S 'rm -rf /'", RiskLevel::Unknown),
            ("source ./script.sh", RiskLevel::Unknown),
            (". ./script.sh", RiskLevel::Unknown),
            ("exec rm -rf /", RiskLevel::Unknown),
            ("echo ${(e)payload}", RiskLevel::Unknown),
            ("echo \"${(Xe)payload}\"", RiskLevel::Unknown),
            // Descriptor duplication is not an overwrite and not a background
            // separator; `&>file` still overwrites a file.
            ("echo hi 2>&1", RiskLevel::Low),
            ("echo hi 2>&-", RiskLevel::Low),
            ("echo hi &> file", RiskLevel::Medium),
            ("echo hi >> file 2>&1", RiskLevel::Medium),
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
    fn sees_through_xargs_to_the_wrapped_command() {
        assert_eq!(classify_command("xargs rm -rf /").level, RiskLevel::High);
        assert_eq!(classify_command("xargs rm file").level, RiskLevel::Medium);
        assert_eq!(
            classify_command("xargs -I{} rm {}").level,
            RiskLevel::Medium
        );
        assert_eq!(classify_command("xargs -0 rm -f").level, RiskLevel::High);
        assert_eq!(
            classify_command("xargs -n 1 rm -r dir").level,
            RiskLevel::High
        );
        assert_eq!(classify_command("xargs echo").level, RiskLevel::Low);
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

        let doas = classify_command("doas rm file");
        assert!(doas.reasons.contains(&RiskReason::PrivilegeElevation));
        assert!(doas.reasons.contains(&RiskReason::DestructiveCommand));
    }
}
