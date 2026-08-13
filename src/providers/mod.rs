mod ai_action;
mod alias;
mod command_help;
mod command_spec;
mod filesystem;
mod git;
mod history;
mod network_interface;
mod path_command;
mod process;
mod project;
mod python_module;
mod session_command;
mod ssh;
mod toolchain;

pub use ai_action::{AiActionProvider, ai_error_candidate, ai_result_candidates};
pub use alias::AliasProvider;
pub use command_help::{CommandHelpCache, CommandHelpProvider};
pub use command_spec::CommandSpecProvider;
pub use filesystem::FilesystemProvider;
pub use git::GitProvider;
pub use history::HistoryProvider;
pub use network_interface::NetworkInterfaceProvider;
pub use path_command::PathCommandProvider;
pub use process::ProcessProvider;
pub use project::ProjectProvider;
pub use python_module::PythonModuleProvider;
pub use session_command::SessionCommandProvider;
pub use ssh::SshHostProvider;
pub use toolchain::ToolchainProvider;

use crate::{completion::CompletionContext, parser::TokenKind};

/// Builtins and reserved words are shell-specific. Exact and prefix checks
/// use the same selected table so history filtering and natural-language
/// detection cannot disagree, without treating zsh-only words as Bash
/// commands or missing Fish's command-oriented builtins.
const BASH_BUILTINS_AND_KEYWORDS: &[&str] = &[
    "!",
    ".",
    ":",
    "[",
    "[[",
    "alias",
    "bg",
    "bind",
    "break",
    "builtin",
    "caller",
    "case",
    "cd",
    "command",
    "compgen",
    "complete",
    "compopt",
    "continue",
    "coproc",
    "declare",
    "dirs",
    "disown",
    "do",
    "done",
    "echo",
    "elif",
    "else",
    "enable",
    "esac",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "fi",
    "for",
    "function",
    "getopts",
    "hash",
    "help",
    "history",
    "if",
    "in",
    "jobs",
    "kill",
    "let",
    "local",
    "logout",
    "mapfile",
    "popd",
    "printf",
    "pushd",
    "pwd",
    "read",
    "readarray",
    "readonly",
    "return",
    "select",
    "set",
    "shift",
    "shopt",
    "source",
    "suspend",
    "test",
    "then",
    "time",
    "times",
    "trap",
    "true",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "until",
    "wait",
    "while",
];

const ZSH_BUILTINS_AND_KEYWORDS: &[&str] = &[
    "!",
    ".",
    ":",
    "[",
    "[[",
    "alias",
    "autoload",
    "bg",
    "bindkey",
    "break",
    "builtin",
    "bye",
    "case",
    "cd",
    "chdir",
    "command",
    "compcall",
    "compctl",
    "compdef",
    "compdescribe",
    "compfiles",
    "compgroups",
    "compquote",
    "comptags",
    "comptry",
    "compvalues",
    "continue",
    "coproc",
    "declare",
    "dirs",
    "disable",
    "disown",
    "do",
    "done",
    "echo",
    "echotc",
    "echoti",
    "elif",
    "else",
    "emulate",
    "enable",
    "end",
    "esac",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "fi",
    "float",
    "for",
    "foreach",
    "function",
    "functions",
    "getln",
    "getopts",
    "hash",
    "history",
    "if",
    "in",
    "integer",
    "jobs",
    "kill",
    "let",
    "limit",
    "local",
    "log",
    "logout",
    "nocorrect",
    "noglob",
    "popd",
    "print",
    "printf",
    "private",
    "pushd",
    "pushln",
    "pwd",
    "r",
    "read",
    "readonly",
    "rehash",
    "repeat",
    "return",
    "sched",
    "select",
    "set",
    "setopt",
    "shift",
    "source",
    "stat",
    "suspend",
    "test",
    "then",
    "time",
    "times",
    "trap",
    "true",
    "ttyctl",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unfunction",
    "unhash",
    "unlimit",
    "unset",
    "unsetopt",
    "until",
    "vared",
    "wait",
    "whence",
    "where",
    "which",
    "while",
    "zcompile",
    "zformat",
    "zmodload",
    "zparseopts",
    "zregexparse",
    "zstyle",
];

const FISH_BUILTINS_AND_KEYWORDS: &[&str] = &[
    "!",
    ".",
    "[",
    "_",
    "abbr",
    "and",
    "argparse",
    "begin",
    "bg",
    "bind",
    "block",
    "break",
    "breakpoint",
    "builtin",
    "case",
    "cd",
    "command",
    "commandline",
    "complete",
    "contains",
    "continue",
    "count",
    "disown",
    "echo",
    "else",
    "emit",
    "end",
    "eval",
    "exec",
    "exit",
    "false",
    "fg",
    "fish_add_path",
    "for",
    "function",
    "functions",
    "history",
    "if",
    "jobs",
    "math",
    "not",
    "or",
    "path",
    "printf",
    "pwd",
    "random",
    "read",
    "realpath",
    "return",
    "set",
    "set_color",
    "source",
    "status",
    "string",
    "switch",
    "test",
    "time",
    "trap",
    "true",
    "type",
    "ulimit",
    "umask",
    "wait",
    "while",
];

const BASH_KEYWORDS: &[&str] = &[
    "!", "[[", "case", "coproc", "do", "done", "elif", "else", "esac", "fi", "for", "function",
    "if", "in", "select", "then", "time", "until", "while",
];

const ZSH_KEYWORDS: &[&str] = &[
    "!",
    "[[",
    "case",
    "coproc",
    "do",
    "done",
    "elif",
    "else",
    "end",
    "esac",
    "fi",
    "for",
    "foreach",
    "function",
    "if",
    "in",
    "nocorrect",
    "repeat",
    "select",
    "then",
    "time",
    "until",
    "while",
];

const FISH_KEYWORDS: &[&str] = &[
    "!", "and", "begin", "case", "else", "end", "for", "function", "if", "not", "or", "switch",
    "time", "while",
];

const ZSH_NON_BUILTIN_COMMANDS: &[&str] = &["compdef", "stat"];
const FISH_NON_BUILTIN_COMMANDS: &[&str] = &["_", "fish_add_path"];

fn shell_builtins_and_keywords(shell: crate::shell::ShellKind) -> &'static [&'static str] {
    match shell {
        crate::shell::ShellKind::Bash => BASH_BUILTINS_AND_KEYWORDS,
        crate::shell::ShellKind::Zsh => ZSH_BUILTINS_AND_KEYWORDS,
        crate::shell::ShellKind::Fish => FISH_BUILTINS_AND_KEYWORDS,
    }
}

fn shell_keywords(shell: crate::shell::ShellKind) -> &'static [&'static str] {
    match shell {
        crate::shell::ShellKind::Bash => BASH_KEYWORDS,
        crate::shell::ShellKind::Zsh => ZSH_KEYWORDS,
        crate::shell::ShellKind::Fish => FISH_KEYWORDS,
    }
}

pub(crate) fn is_shell_builtin_or_keyword(shell: crate::shell::ShellKind, word: &str) -> bool {
    shell_builtins_and_keywords(shell).contains(&word)
}

pub(crate) fn is_shell_callable(shell: crate::shell::ShellKind, word: &str) -> bool {
    is_shell_builtin_or_keyword(shell, word) && !shell_keywords(shell).contains(&word)
}

pub(crate) fn is_shell_builtin(shell: crate::shell::ShellKind, word: &str) -> bool {
    is_shell_callable(shell, word)
        && match shell {
            crate::shell::ShellKind::Zsh => !ZSH_NON_BUILTIN_COMMANDS.contains(&word),
            crate::shell::ShellKind::Fish => !FISH_NON_BUILTIN_COMMANDS.contains(&word),
            crate::shell::ShellKind::Bash => true,
        }
}

pub(crate) fn shell_builtin_has_prefix(shell: crate::shell::ShellKind, prefix: &str) -> bool {
    shell_builtins_and_keywords(shell)
        .iter()
        .any(|name| is_shell_builtin(shell, name) && name.starts_with(prefix))
}

pub(crate) fn shell_symbol_has_prefix(shell: crate::shell::ShellKind, prefix: &str) -> bool {
    shell_builtins_and_keywords(shell)
        .iter()
        .any(|name| name.starts_with(prefix))
}

/// Cursor progress past the effective command token: the cooked words of the
/// active segment from the effective command up to the cursor, plus the
/// zero-based index of the argument being completed (0 = first argument after
/// the effective command). Leading assignments and wrapper words (`sudo`,
/// `env`, …) are skipped, so `sudo git checkout ` measures from `git`.
/// `None` while the cursor is still on the effective command token itself.
/// Shared by the filesystem and command-help providers so both agree on what
/// "first argument" means.
pub(crate) fn argument_progress(context: &CompletionContext) -> Option<(Vec<&str>, usize)> {
    if redirect_target(context) {
        return None;
    }
    let word_tokens = segment_word_tokens(context);
    let cooked: Vec<&str> = word_tokens
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let command_index = crate::parser::effective_command_index_for_shell(&cooked, context.shell)?;
    let command_token = word_tokens[command_index];
    if context.buffer.cursor <= command_token.range.end {
        return None;
    }
    let words: Vec<&str> = cooked[command_index..].to_vec();
    // The active word is the token the cursor sits on. When the cursor sits
    // exactly at a word's start (the preceding char is whitespace) that word
    // is still the ACTIVE word — an empty prefix — not a completed one, so it
    // must not count as finished.
    let on_active_word = word_tokens.last().is_some_and(|token| {
        context.buffer.cursor >= token.range.start && context.buffer.cursor <= token.range.end
    });
    let position = if on_active_word {
        words.len().saturating_sub(2)
    } else {
        words.len().saturating_sub(1)
    };
    Some((words, position))
}

/// True while the cursor is still on the (effective) command word: either no
/// effective command exists yet (empty line, or only assignments/wrappers so
/// far — `sudo `, `FOO=bar `) or the cursor sits within/at the end of the
/// effective command word. Past it (`ls `, an argument position) command-name
/// completion must not fire. Shared by the PATH and alias providers.
pub(crate) fn command_position_open(context: &CompletionContext) -> bool {
    if context.parsed.current_prefix.contains('/') || redirect_target(context) {
        return false;
    }
    let word_tokens = segment_word_tokens(context);
    let cooked: Vec<&str> = word_tokens
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let last_word_active = word_tokens.last().is_some_and(|token| {
        context.buffer.cursor >= token.range.start && context.buffer.cursor <= token.range.end
    });
    match crate::parser::effective_command_state_for_shell(&cooked, last_word_active, context.shell)
    {
        crate::parser::EffectiveCommandState::AwaitingCommand => true,
        crate::parser::EffectiveCommandState::AwaitingWrapperValue => false,
        crate::parser::EffectiveCommandState::Found(index)
        | crate::parser::EffectiveCommandState::WrapperCommand(index)
        | crate::parser::EffectiveCommandState::IndeterminateWrapper(index) => {
            let token = word_tokens[index];
            context.buffer.cursor >= token.range.start && context.buffer.cursor <= token.range.end
        }
    }
}

pub(crate) fn explicit_executable_path_position(context: &CompletionContext) -> bool {
    if !context.parsed.current_prefix.contains('/') || redirect_target(context) {
        return false;
    }
    let word_tokens = segment_word_tokens(context);
    let cooked: Vec<&str> = word_tokens
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let last_word_active = word_tokens.last().is_some_and(|token| {
        context.buffer.cursor >= token.range.start && context.buffer.cursor <= token.range.end
    });
    let analysis = crate::parser::effective_command_analysis_for_shell(
        &cooked,
        last_word_active,
        context.shell,
    );
    if analysis.kind == crate::parser::EffectiveCommandKind::Builtin {
        return false;
    }
    let crate::parser::EffectiveCommandState::Found(index) = analysis.state else {
        return false;
    };
    let token = word_tokens[index];
    context.buffer.cursor >= token.range.start && context.buffer.cursor <= token.range.end
}

pub(crate) fn executable_position_open(context: &CompletionContext) -> bool {
    if command_position_open(context) {
        return true;
    }
    !context.parsed.current_prefix.contains('/') && executable_argument_slot(context)
}

pub(crate) fn explicit_executable_argument_path_position(context: &CompletionContext) -> bool {
    context.parsed.current_prefix.contains('/')
        && context.command() != Some("corepack")
        && executable_argument_slot(context)
}

fn executable_argument_slot(context: &CompletionContext) -> bool {
    if context.parsed.current_prefix.starts_with('-') || redirect_target(context) {
        return false;
    }
    if context.command() == Some("which") {
        return argument_progress(context).is_some();
    }
    if shell_symbol_argument_position(context) {
        return true;
    }
    if context.command() == Some("command") {
        return false;
    }
    if find_exec_command_position(context) {
        return true;
    }
    if rustup_run_executable_position(context) {
        return true;
    }
    if delegated_executable_slot(context).is_some() {
        return true;
    }
    if context.command() == Some("npx") {
        let Some((words, position)) = argument_progress(context) else {
            return false;
        };
        return npx_executable_position(&words, position);
    }
    if context.command() == Some("corepack") {
        return argument_progress(context).is_some_and(|(_, position)| position == 0);
    }
    let Some(ManagerScanResult::Ready(scan)) = scan_manager(context) else {
        return false;
    };
    let Some(command) = scan.words.get(scan.command_index).copied() else {
        return false;
    };
    match (scan.spec.name, command) {
        ("npm", "exec") if scan.command_index < scan.active_index => {
            package_exec_executable_position(
                scan.words
                    .get(scan.command_index + 1..scan.active_index)
                    .unwrap_or_default(),
            )
        }
        ("pnpm" | "yarn", "exec") => {
            scan.command_index + 1 == scan.active_index
                || (scan.words.get(scan.command_index + 1).copied() == Some("--")
                    && scan.command_index + 2 == scan.active_index)
        }
        _ => false,
    }
}

fn rustup_run_executable_position(context: &CompletionContext) -> bool {
    if context
        .command()
        .is_none_or(|command| executable_basename(command) != "rustup")
    {
        return false;
    }
    let Some((words, position)) = argument_progress(context) else {
        return false;
    };
    let before = words.get(1..=position).unwrap_or_default();
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if word.starts_with('+') || matches!(word, "-v" | "--verbose" | "-q" | "--quiet") {
            index += 1;
            continue;
        }
        break;
    }
    if before.get(index).copied() != Some("run") {
        return false;
    }
    index += 1;
    if before.get(index).copied() == Some("--install") {
        index += 1;
    }
    before
        .get(index)
        .is_some_and(|toolchain| !toolchain.starts_with('-'))
        && index + 1 == before.len()
}

/// The executable word inside a `find -exec`, `-execdir`, `-ok`, or `-okdir`
/// action. The active word itself is excluded by `argument_progress`, so an
/// empty nested slice or a wrapper chain still awaiting its command identifies
/// the executable slot.
pub(crate) fn find_exec_command_position(context: &CompletionContext) -> bool {
    if context.command() != Some("find") || redirect_target(context) {
        return false;
    }
    let Some((words, position)) = argument_progress(context) else {
        return false;
    };
    let before = words.get(1..=position).unwrap_or_default();
    let mut command_start = None;
    for (index, word) in before.iter().copied().enumerate() {
        if is_find_exec_action(word) {
            command_start = Some(index + 1);
        } else if command_start.is_some() && matches!(word, ";" | "+") {
            command_start = None;
        }
    }
    let Some(command_start) = command_start else {
        return false;
    };
    let nested = &before[command_start..];
    matches!(
        crate::parser::effective_external_command_state(nested, false),
        crate::parser::EffectiveCommandState::AwaitingCommand
    )
}

pub(crate) fn find_exec_working_directory(
    context: &CompletionContext,
) -> Option<std::path::PathBuf> {
    if !find_exec_command_position(context) {
        return None;
    }
    let (words, position) = argument_progress(context)?;
    let before = words.get(1..=position).unwrap_or_default();
    let marker = before.iter().rposition(|word| is_find_exec_action(word))?;
    if is_find_execdir_action(before[marker]) {
        return None;
    }
    let command_start = marker + 1;
    let nested = &before[command_start..];
    Some(
        crate::parser::wrapper_working_directories(nested, nested.len())
            .into_iter()
            .fold(invocation_working_directory(context), |directory, value| {
                resolve_directory(&directory, value)
            }),
    )
}

fn is_find_exec_action(word: &str) -> bool {
    matches!(word, "-exec" | "-execdir" | "-ok" | "-okdir")
}

fn is_find_execdir_action(word: &str) -> bool {
    matches!(word, "-execdir" | "-okdir")
}

pub(crate) fn shell_symbol_argument_position(context: &CompletionContext) -> bool {
    let shell_symbols_allowed =
        command_resolution_kind(context) != crate::parser::EffectiveCommandKind::External;
    if shell_symbols_allowed
        && context.command().is_some_and(|command| {
            command == "type"
                || (matches!(command, "whence" | "where" | "which")
                    && context.shell == crate::shell::ShellKind::Zsh)
        })
    {
        return argument_progress(context).is_some();
    }
    if shell_symbols_allowed && context.command() == Some("command") {
        let Some((words, position)) = argument_progress(context) else {
            return false;
        };
        let prior = words.get(1..=position).unwrap_or_default();
        return prior
            .iter()
            .position(|word| crate::parser::command_query_option(word))
            .is_some_and(|mode| prior[..mode].iter().all(|word| word.starts_with('-')));
    }
    false
}

fn npx_executable_position(words: &[&str], position: usize) -> bool {
    let before = words.get(1..=position).unwrap_or_default();
    package_exec_executable_position(before)
}

fn package_exec_executable_position(before: &[&str]) -> bool {
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if word == "--" {
            return index + 1 == before.len();
        }
        if let Some((flag, _)) = word.split_once('=') {
            if matches!(flag, "-c" | "--call") {
                return false;
            }
            if matches!(
                flag,
                "-y" | "--yes"
                    | "--no"
                    | "--workspaces"
                    | "--include-workspace-root"
                    | "--ignore-scripts"
                    | "--foreground-scripts"
                    | "--offline"
                    | "--prefer-offline"
            ) {
                index += 1;
                continue;
            }
            if matches!(
                flag,
                "-p" | "--package"
                    | "-w"
                    | "--workspace"
                    | "--prefix"
                    | "--npm"
                    | "--node-arg"
                    | "--userconfig"
                    | "--cache"
                    | "--registry"
                    | "--location"
                    | "--script-shell"
            ) {
                index += 1;
                continue;
            }
            return false;
        }
        if word.len() > 2 && word.starts_with("-c") {
            return false;
        }
        if word.len() > 2 && word.starts_with("-p") {
            index += 1;
            continue;
        }
        if word.len() > 2 && word.starts_with("-w") && !word.starts_with("--") {
            index += 1;
            continue;
        }
        if matches!(word, "-c" | "--call") {
            return false;
        }
        if matches!(
            word,
            "-p" | "--package"
                | "-w"
                | "--workspace"
                | "--prefix"
                | "--npm"
                | "--node-arg"
                | "--userconfig"
                | "--cache"
                | "--registry"
                | "--location"
                | "--script-shell"
        ) {
            if index + 1 >= before.len() {
                return false;
            }
            index += 2;
            continue;
        }
        if matches!(
            word,
            "-y" | "--yes"
                | "-q"
                | "--quiet"
                | "--no"
                | "--ignore-existing"
                | "--workspaces"
                | "--include-workspace-root"
                | "--ignore-scripts"
                | "--foreground-scripts"
                | "--offline"
                | "--prefer-offline"
        ) {
            index += 1;
            continue;
        }
        return false;
    }
    true
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DelegatedExecutableKind {
    UvRun,
    PoetryRun,
    PipenvRun,
    BundleExec,
}

impl DelegatedExecutableKind {
    const fn action(self) -> &'static str {
        match self {
            Self::UvRun | Self::PoetryRun | Self::PipenvRun => "run",
            Self::BundleExec => "exec",
        }
    }
}

fn delegated_executable_kind(command: &str) -> Option<DelegatedExecutableKind> {
    match executable_basename(command) {
        "uv" => Some(DelegatedExecutableKind::UvRun),
        "poetry" => Some(DelegatedExecutableKind::PoetryRun),
        "pipenv" => Some(DelegatedExecutableKind::PipenvRun),
        "bundle" | "bundler" => Some(DelegatedExecutableKind::BundleExec),
        _ => None,
    }
}

fn delegated_executable_slot(context: &CompletionContext) -> Option<std::path::PathBuf> {
    let kind = delegated_executable_kind(context.command()?)?;
    let (words, position) = argument_progress(context)?;
    let before = words.get(1..=position).unwrap_or_default();
    let mut directory = invocation_working_directory(context);
    let mut action_seen = false;
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if !action_seen && word == kind.action() {
            action_seen = true;
            index += 1;
            continue;
        }
        if word == "--" {
            return (action_seen && index + 1 == before.len()).then_some(directory);
        }
        if delegated_terminal_option(kind, word)
            || action_seen && delegated_non_executable_mode(kind, word)
        {
            return None;
        }
        if let Some((flag, value)) = word.split_once('=') {
            if !delegated_value_option(kind, action_seen, flag) {
                return None;
            }
            apply_delegated_directory(kind, flag, value, &mut directory);
            index += 1;
            continue;
        }
        if let Some((flag, value)) = delegated_attached_short_value(kind, action_seen, word) {
            apply_delegated_directory(kind, flag, value, &mut directory);
            index += 1;
            continue;
        }
        if delegated_value_option(kind, action_seen, word) {
            let value = before.get(index + 1).copied()?;
            apply_delegated_directory(kind, word, value, &mut directory);
            index += 2;
            continue;
        }
        if delegated_boolean_option(kind, action_seen, word) {
            index += 1;
            continue;
        }
        return None;
    }
    action_seen.then_some(directory)
}

pub(crate) fn delegated_command_working_directory(
    context: &CompletionContext,
) -> Option<std::path::PathBuf> {
    let kind = delegated_executable_kind(context.command()?)?;
    let (words, position) = argument_progress(context)?;
    let before = words.get(1..=position).unwrap_or_default();
    let mut directory = invocation_working_directory(context);
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if let Some((flag, value)) = word.split_once('=') {
            apply_delegated_directory(kind, flag, value, &mut directory);
            index += 1;
            continue;
        }
        if kind == DelegatedExecutableKind::PoetryRun && word.len() > 2 && word.starts_with("-C") {
            apply_delegated_directory(kind, "-C", &word[2..], &mut directory);
            index += 1;
            continue;
        }
        if word == "--directory" || kind == DelegatedExecutableKind::PoetryRun && word == "-C" {
            let value = before.get(index + 1).copied()?;
            apply_delegated_directory(kind, word, value, &mut directory);
            index += 2;
            continue;
        }
        index += 1;
    }
    Some(directory)
}

fn delegated_terminal_option(kind: DelegatedExecutableKind, word: &str) -> bool {
    matches!(word, "-h" | "--help" | "--version")
        || word == "-V" && kind != DelegatedExecutableKind::BundleExec
        || kind == DelegatedExecutableKind::PipenvRun
            && matches!(
                word,
                "--where" | "--venv" | "--py" | "--envs" | "--rm" | "--man" | "--support"
            )
}

fn delegated_non_executable_mode(kind: DelegatedExecutableKind, word: &str) -> bool {
    kind == DelegatedExecutableKind::UvRun
        && (matches!(word, "-m" | "--module" | "-s" | "--script" | "--gui-script")
            || word.starts_with("--module=")
            || word.starts_with("--script=")
            || word.starts_with("--gui-script=")
            || word.len() > 2 && matches!(&word[..2], "-m" | "-s"))
}

fn delegated_value_option(kind: DelegatedExecutableKind, action_seen: bool, word: &str) -> bool {
    let global = match kind {
        DelegatedExecutableKind::UvRun => matches!(
            word,
            "--color" | "--allow-insecure-host" | "--directory" | "--project" | "--config-file"
        ),
        DelegatedExecutableKind::PoetryRun => {
            matches!(word, "-C" | "--directory" | "-P" | "--project")
        }
        DelegatedExecutableKind::PipenvRun => matches!(
            word,
            "--python" | "--pypi-mirror" | "--categories" | "--extra-pip-args"
        ),
        DelegatedExecutableKind::BundleExec => matches!(
            word,
            "--gemfile" | "--path" | "-j" | "--jobs" | "-r" | "--retry"
        ),
    };
    global
        || action_seen
            && kind == DelegatedExecutableKind::UvRun
            && matches!(
                word,
                "--extra"
                    | "--no-extra"
                    | "--group"
                    | "--no-group"
                    | "--only-group"
                    | "--no-editable-package"
                    | "--env-file"
                    | "-w"
                    | "--with"
                    | "--with-editable"
                    | "--with-requirements"
                    | "--package"
                    | "--python-platform"
                    | "--index"
                    | "--default-index"
                    | "-i"
                    | "--index-url"
                    | "--extra-index-url"
                    | "-f"
                    | "--find-links"
                    | "--index-strategy"
                    | "--keyring-provider"
                    | "-P"
                    | "--upgrade-package"
                    | "--upgrade-group"
                    | "--resolution"
                    | "--prerelease"
                    | "--fork-strategy"
                    | "--exclude-newer"
                    | "--exclude-newer-package"
                    | "--no-sources-package"
                    | "--reinstall-package"
                    | "--link-mode"
                    | "-C"
                    | "--config-setting"
                    | "--config-settings-package"
                    | "--no-build-isolation-package"
                    | "--no-build-package"
                    | "--no-binary-package"
                    | "--cache-dir"
                    | "--refresh-package"
                    | "-p"
                    | "--python"
            )
}

fn delegated_boolean_option(kind: DelegatedExecutableKind, action_seen: bool, word: &str) -> bool {
    if matches!(
        kind,
        DelegatedExecutableKind::UvRun | DelegatedExecutableKind::PoetryRun
    ) && word.len() >= 2
        && word.starts_with('-')
        && word[1..]
            .chars()
            .all(|character| matches!(character, 'q' | 'v'))
    {
        return true;
    }
    let global = match kind {
        DelegatedExecutableKind::UvRun => matches!(
            word,
            "--system-certs" | "--offline" | "--no-progress" | "--no-config"
        ),
        DelegatedExecutableKind::PoetryRun => matches!(
            word,
            "-n" | "--no-interaction" | "--ansi" | "--no-ansi" | "--no-plugins" | "--no-cache"
        ),
        DelegatedExecutableKind::PipenvRun => matches!(
            word,
            "--bare" | "--site-packages" | "--clear" | "--quiet" | "--verbose"
        ),
        DelegatedExecutableKind::BundleExec => matches!(
            word,
            "--keep-file-descriptors"
                | "--no-keep-file-descriptors"
                | "--no-color"
                | "-V"
                | "--verbose"
                | "--no-verbose"
        ),
    };
    global
        || action_seen
            && kind == DelegatedExecutableKind::UvRun
            && matches!(
                word,
                "--all-extras"
                    | "--no-dev"
                    | "--no-default-groups"
                    | "--all-groups"
                    | "--only-dev"
                    | "--no-editable"
                    | "--exact"
                    | "--no-env-file"
                    | "--isolated"
                    | "--active"
                    | "--no-sync"
                    | "--locked"
                    | "--frozen"
                    | "--all-packages"
                    | "--no-project"
                    | "--no-index"
                    | "-U"
                    | "--upgrade"
                    | "--no-sources"
                    | "--reinstall"
                    | "--compile-bytecode"
                    | "--no-build-isolation"
                    | "--no-build"
                    | "--no-binary"
                    | "-n"
                    | "--no-cache"
                    | "--refresh"
                    | "--managed-python"
                    | "--no-managed-python"
                    | "--no-python-downloads"
            )
}

fn delegated_attached_short_value(
    kind: DelegatedExecutableKind,
    action_seen: bool,
    word: &str,
) -> Option<(&'static str, &str)> {
    let flags: &[&str] = match kind {
        DelegatedExecutableKind::UvRun if action_seen => &["-w", "-i", "-f", "-P", "-C", "-p"],
        DelegatedExecutableKind::PoetryRun => &["-C", "-P"],
        DelegatedExecutableKind::PipenvRun => &[],
        DelegatedExecutableKind::BundleExec => &["-j", "-r"],
        DelegatedExecutableKind::UvRun => &[],
    };
    flags.iter().find_map(|flag| {
        (word.len() > flag.len() && word.starts_with(flag)).then_some((*flag, &word[flag.len()..]))
    })
}

fn apply_delegated_directory(
    kind: DelegatedExecutableKind,
    flag: &str,
    value: &str,
    directory: &mut std::path::PathBuf,
) {
    let changes_directory =
        flag == "--directory" || kind == DelegatedExecutableKind::PoetryRun && flag == "-C";
    if changes_directory {
        *directory = resolve_directory(directory, value);
    }
}

pub(crate) fn path_executable_name_allowed(context: &CompletionContext, name: &str) -> bool {
    context.command() != Some("corepack")
        || matches!(name, "npm" | "npx" | "pnpm" | "pnpx" | "yarn" | "yarnpkg")
}

pub(crate) fn command_resolution_kind(
    context: &CompletionContext,
) -> crate::parser::EffectiveCommandKind {
    let word_tokens = segment_word_tokens(context);
    let cooked: Vec<&str> = word_tokens
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let last_word_active = word_tokens.last().is_some_and(|token| {
        context.buffer.cursor >= token.range.start && context.buffer.cursor <= token.range.end
    });
    crate::parser::effective_command_analysis_for_shell(&cooked, last_word_active, context.shell)
        .kind
}

/// Shell aliases/functions expand only at the syntactic command word. An
/// executable argument behind `sudo`/`env` is a PATH slot, but it is not an
/// alias slot in normal shell parsing.
pub(crate) fn shell_command_position_open(context: &CompletionContext) -> bool {
    if context.parsed.current_prefix.contains('/') || redirect_target(context) {
        return false;
    }
    let word_tokens = segment_word_tokens(context);
    let cooked: Vec<&str> = word_tokens
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let last_word_active = word_tokens.last().is_some_and(|token| {
        context.buffer.cursor >= token.range.start && context.buffer.cursor <= token.range.end
    });
    let analysis = crate::parser::effective_command_analysis_for_shell(
        &cooked,
        last_word_active,
        context.shell,
    );
    if analysis.kind != crate::parser::EffectiveCommandKind::Shell {
        return false;
    }
    match analysis.state {
        crate::parser::EffectiveCommandState::AwaitingCommand => true,
        crate::parser::EffectiveCommandState::AwaitingWrapperValue => false,
        crate::parser::EffectiveCommandState::Found(index)
        | crate::parser::EffectiveCommandState::WrapperCommand(index)
        | crate::parser::EffectiveCommandState::IndeterminateWrapper(index) => {
            let token = word_tokens[index];
            context.buffer.cursor >= token.range.start && context.buffer.cursor <= token.range.end
        }
    }
}

pub(crate) fn effective_command_is_shell_command(context: &CompletionContext) -> bool {
    command_resolution_kind(context) == crate::parser::EffectiveCommandKind::Shell
}

pub(crate) fn effective_command_accepts_external(context: &CompletionContext) -> bool {
    command_resolution_kind(context) != crate::parser::EffectiveCommandKind::Builtin
}

/// Word tokens of the active pipeline segment up to the cursor.
fn segment_word_tokens(context: &CompletionContext) -> Vec<&crate::parser::Token> {
    crate::parser::semantic_word_tokens(&context.parsed.tokens, &context.parsed.active_segment)
        .into_iter()
        .filter(|token| token.range.start <= context.buffer.cursor)
        .collect()
}

/// True while the active word is the target of a shell redirect, including
/// an empty target immediately after `>`. Redirect targets are always path
/// slots and must not wake command, history-shape, or semantic providers.
pub(crate) fn redirect_target(context: &CompletionContext) -> bool {
    redirect_operator(context).is_some()
}

pub(crate) fn redirect_path_target(context: &CompletionContext) -> bool {
    let Some(operator) = redirect_operator(context) else {
        return false;
    };
    let raw = &context.buffer.text[operator.range.clone()];
    let prefix = context.parsed.current_prefix.as_str();
    if matches!(raw, "<<" | "<<-" | "<<<") {
        return false;
    }
    let fd_designator = context.parsed.tokens.iter().any(|token| {
        token.kind == TokenKind::Word
            && token.range.end == operator.range.start
            && token
                .cooked_prefix
                .chars()
                .all(|character| character.is_ascii_digit())
    });
    if fd_designator && raw.contains('&') {
        return false;
    }
    if raw == "<&" {
        return false;
    }
    if raw.ends_with("&")
        && (prefix == "-"
            || (!prefix.is_empty() && prefix.chars().all(|character| character.is_ascii_digit())))
    {
        return false;
    }
    true
}

fn redirect_operator(context: &CompletionContext) -> Option<&crate::parser::Token> {
    let tokens: Vec<_> = context
        .parsed
        .tokens
        .iter()
        .filter(|token| {
            token.range.start >= context.parsed.active_segment.start
                && token.range.end <= context.parsed.active_segment.end
        })
        .collect();
    if let Some(current) = tokens.iter().position(|token| {
        token.kind == TokenKind::Word
            && token.range.start <= context.buffer.cursor
            && context.buffer.cursor <= token.range.end
    }) {
        return tokens[..current]
            .iter()
            .rev()
            .find(|token| token.kind != TokenKind::Whitespace)
            .copied()
            .filter(|token| token.kind == TokenKind::Redirect);
    }
    tokens
        .iter()
        .filter(|token| token.range.end <= context.buffer.cursor)
        .rev()
        .find(|token| token.kind != TokenKind::Whitespace)
        .copied()
        .filter(|token| token.kind == TokenKind::Redirect)
}

/// Cooked words from the effective command through the cursor. Leading
/// assignments and wrappers are removed so every contextual provider sees
/// the same command-relative shape (`sudo pnpm dev` -> `pnpm dev`).
pub(crate) fn segment_words(context: &CompletionContext) -> Vec<&str> {
    let words: Vec<&str> = segment_word_tokens(context)
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let Some(command_index) =
        crate::parser::effective_command_index_for_shell(&words, context.shell)
    else {
        return Vec::new();
    };
    words[command_index..].to_vec()
}

pub(crate) fn corepack_dispatch(context: &CompletionContext) -> bool {
    let word_tokens = segment_word_tokens(context);
    let words: Vec<&str> = word_tokens
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let Some(command_index) =
        crate::parser::effective_command_index_for_shell(&words, context.shell)
    else {
        return false;
    };
    command_index
        .checked_sub(1)
        .and_then(|index| words.get(index))
        .is_some_and(|word| *word == "corepack")
}

/// Directory inherited by the effective command after outer wrappers have
/// applied their own chdir options. Relative changes from nested wrappers are
/// resolved in execution order.
pub(crate) fn invocation_working_directory(context: &CompletionContext) -> std::path::PathBuf {
    let words: Vec<&str> = segment_word_tokens(context)
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let Some(command_index) =
        crate::parser::effective_command_index_for_shell(&words, context.shell)
    else {
        return context.cwd.as_ref().clone();
    };
    wrapper_working_directory_before(context, &words, command_index)
}

/// Resolve the effective external command to a file that is executable by
/// the current user. PATH names use the shared command snapshot; explicit
/// paths are resolved relative to wrapper-adjusted cwd and checked directly.
pub(crate) fn resolved_executable_path(
    context: &CompletionContext,
    commands: &crate::platform::CommandPathCache,
) -> Option<std::path::PathBuf> {
    let command = context.command()?;
    if let Some(path) = commands.path(command) {
        return Some(path);
    }
    if !command.contains('/')
        || command_resolution_kind(context) == crate::parser::EffectiveCommandKind::Builtin
    {
        return None;
    }
    let path = resolve_directory(&invocation_working_directory(context), command);
    crate::platform::is_executable(&path).then_some(path)
}

pub(crate) fn executable_basename(command: &str) -> &str {
    std::path::Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
}

pub(crate) fn is_c_family_compiler(command: &str) -> bool {
    c_family_compiler_driver(command).is_some()
}

fn c_family_compiler_driver(command: &str) -> Option<&'static str> {
    const COMPILERS: &[&str] = &[
        "clang-cl", "clang++", "clang", "g++", "gcc", "c++", "cc", "cpp",
    ];
    let without_version = command.rsplit_once('-').map_or(command, |(stem, suffix)| {
        if suffix.chars().any(|character| character.is_ascii_digit())
            && suffix
                .chars()
                .all(|character| character.is_ascii_digit() || character == '.')
        {
            stem
        } else {
            command
        }
    });
    COMPILERS.iter().copied().find(|compiler| {
        without_version == *compiler
            || without_version
                .strip_suffix(compiler)
                .is_some_and(|prefix| prefix.ends_with('-'))
    })
}

pub(crate) fn is_pip_command(command: &str) -> bool {
    let name = executable_basename(command);
    let Some(suffix) = name.strip_prefix("pip") else {
        return false;
    };
    suffix.is_empty()
        || suffix.split('.').all(|component| {
            !component.is_empty()
                && component
                    .chars()
                    .all(|character| character.is_ascii_digit())
        })
}

pub(crate) fn wrapper_working_directory_before(
    context: &CompletionContext,
    words: &[&str],
    limit: usize,
) -> std::path::PathBuf {
    crate::parser::wrapper_working_directories_for_shell(words, limit, context.shell)
        .into_iter()
        .fold(context.cwd.as_ref().clone(), |directory, value| {
            resolve_directory(&directory, value)
        })
}

/// Node package managers and how they run scripts: pnpm, yarn, and bun
/// execute them directly (`pnpm dev`); npm needs `run`; deno needs `task`.
/// Managers with a keyword lead with their own subcommands at the bare
/// position and only offer scripts behind the keyword.
pub(crate) struct ManagerSpec {
    pub(crate) name: &'static str,
    pub(crate) keyword: Option<&'static str>,
    pub(crate) subcommands: &'static [(&'static str, &'static str)],
}

pub(crate) const NPM_SUBCOMMANDS: &[(&str, &str)] = &[
    ("install", "安装全部依赖"),
    ("run", "运行 package.json 脚本"),
    ("run-script", "运行 package.json 脚本"),
    ("test", "运行 test 脚本"),
    ("start", "运行 start 脚本"),
    ("ci", "按 lockfile 干净安装"),
    ("update", "更新依赖"),
    ("uninstall", "移除依赖"),
    ("exec", "执行包提供的命令"),
    ("init", "初始化 package.json"),
    ("audit", "依赖安全审计"),
    ("outdated", "检查过期依赖"),
    ("publish", "发布包"),
    ("cache", "管理 npm 缓存"),
    ("config", "读取或修改 npm 配置"),
    ("dedupe", "减少重复依赖"),
    ("doctor", "检查 npm 环境"),
    ("explain", "解释依赖安装原因"),
    ("fund", "查看依赖资助信息"),
    ("ls", "列出已安装依赖"),
    ("pack", "创建包归档"),
    ("ping", "检查 registry 连通性"),
    ("prune", "移除多余依赖"),
    ("rebuild", "重新构建依赖"),
    ("search", "搜索 registry 包"),
    ("view", "查看包元数据"),
    ("version", "修改包版本"),
    ("whoami", "显示当前 registry 用户"),
    ("login", "登录 registry"),
    ("logout", "退出 registry"),
    ("link", "链接本地包"),
    ("pkg", "管理 package.json 字段"),
    ("prefix", "显示 npm 前缀目录"),
    ("query", "查询依赖选择器"),
    ("root", "显示 node_modules 目录"),
    ("token", "管理访问令牌"),
    ("unpublish", "撤下已发布版本"),
];

pub(crate) const PNPM_SUBCOMMANDS: &[(&str, &str)] = &[
    ("install", "安装项目依赖"),
    ("add", "添加依赖"),
    ("remove", "移除依赖"),
    ("update", "更新依赖"),
    ("run", "显式运行 package.json 脚本"),
    ("exec", "执行依赖提供的命令"),
    ("dlx", "临时下载并执行包"),
    ("create", "从模板创建项目"),
    ("init", "初始化 package.json"),
    ("list", "列出已安装依赖"),
    ("why", "解释依赖来源"),
    ("outdated", "检查过期依赖"),
    ("audit", "依赖安全审计"),
    ("publish", "发布包"),
    ("pack", "创建发布归档"),
    ("clean", "清理 workspace 的 node_modules"),
    ("dedupe", "减少 lockfile 中的重复依赖"),
    ("fetch", "预取 lockfile 中的依赖"),
    ("import", "从 npm lockfile 生成 pnpm lockfile"),
    ("link", "链接本地包"),
    ("prune", "移除多余依赖"),
    ("rebuild", "重新构建依赖"),
    ("unlink", "取消本地包链接"),
    ("patch", "准备依赖补丁"),
    ("patch-commit", "提交依赖补丁"),
    ("patch-remove", "移除依赖补丁"),
    ("licenses", "检查依赖许可证"),
    ("approve-builds", "批准依赖构建脚本"),
    ("ignored-builds", "列出被阻止的构建脚本"),
    ("start", "运行 start 脚本"),
    ("test", "运行 test 脚本"),
    ("bin", "显示依赖可执行文件目录"),
    ("config", "管理 pnpm 配置"),
    ("deploy", "部署 workspace 包"),
    ("root", "显示有效 node_modules 目录"),
    ("stage", "暂存待发布包"),
    ("runtime", "管理 JavaScript 运行时"),
    ("self-update", "更新 pnpm"),
    ("store", "管理 pnpm store"),
    ("cache", "管理包元数据缓存"),
];

pub(crate) const YARN_SUBCOMMANDS: &[(&str, &str)] = &[
    ("install", "安装项目依赖"),
    ("add", "添加依赖"),
    ("remove", "移除依赖"),
    ("up", "升级依赖"),
    ("upgrade", "升级依赖"),
    ("run", "显式运行 package.json 脚本"),
    ("exec", "执行依赖提供的命令"),
    ("dlx", "临时下载并执行包"),
    ("workspace", "在单个 workspace 中运行命令"),
    ("workspaces", "管理多个 workspaces"),
    ("why", "解释依赖来源"),
    ("info", "查看包信息"),
    ("config", "管理 Yarn 配置"),
    ("cache", "管理下载缓存"),
    ("set", "更新 Yarn 或项目配置"),
    ("create", "从模板创建项目"),
    ("init", "初始化项目"),
    ("link", "链接本地包"),
    ("unlink", "取消本地包链接"),
    ("pack", "创建包归档"),
    ("publish", "发布包"),
    ("version", "管理包版本"),
];

pub(crate) const BUN_SUBCOMMANDS: &[(&str, &str)] = &[
    ("install", "安装项目依赖"),
    ("add", "添加依赖"),
    ("remove", "移除依赖"),
    ("update", "更新依赖"),
    ("run", "显式运行 package.json 脚本"),
    ("x", "执行包提供的命令"),
    ("exec", "执行 shell 脚本"),
    ("repl", "启动 Bun REPL"),
    ("create", "从模板创建项目"),
    ("init", "初始化项目"),
    ("test", "运行 Bun 测试"),
    ("build", "构建入口文件"),
    ("pm", "管理 Bun 包管理器状态"),
    ("outdated", "检查过期依赖"),
    ("patch", "准备依赖补丁"),
    ("publish", "发布包"),
    ("link", "链接本地包"),
    ("unlink", "移除本地包链接"),
    ("upgrade", "升级 Bun"),
];

pub(crate) const DENO_SUBCOMMANDS: &[(&str, &str)] = &[
    ("task", "运行 deno.json / package.json 脚本"),
    ("run", "运行一个程序"),
    ("install", "安装依赖"),
    ("add", "添加依赖"),
    ("test", "运行测试"),
    ("fmt", "格式化代码"),
    ("lint", "静态检查"),
    ("compile", "编译为可执行文件"),
    ("eval", "求值一段代码"),
    ("init", "初始化项目"),
    ("bench", "运行基准测试"),
    ("check", "类型检查模块"),
    ("coverage", "生成测试覆盖率报告"),
    ("doc", "显示模块文档"),
    ("info", "显示模块依赖信息"),
    ("jupyter", "启动 Jupyter kernel"),
    ("lsp", "启动语言服务器"),
    ("outdated", "检查过期依赖"),
    ("publish", "发布 JSR 包"),
    ("remove", "移除依赖"),
    ("repl", "启动交互式 REPL"),
    ("serve", "运行 HTTP 服务"),
    ("uninstall", "卸载已安装命令"),
    ("upgrade", "升级 Deno"),
];

pub(crate) const MANAGERS: &[ManagerSpec] = &[
    ManagerSpec {
        name: "pnpm",
        keyword: None,
        subcommands: PNPM_SUBCOMMANDS,
    },
    ManagerSpec {
        name: "yarn",
        keyword: None,
        subcommands: YARN_SUBCOMMANDS,
    },
    ManagerSpec {
        name: "bun",
        keyword: None,
        subcommands: BUN_SUBCOMMANDS,
    },
    ManagerSpec {
        name: "npm",
        keyword: Some("run"),
        subcommands: NPM_SUBCOMMANDS,
    },
    ManagerSpec {
        name: "deno",
        keyword: Some("task"),
        subcommands: DENO_SUBCOMMANDS,
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Position {
    /// `npm run <prefix>` / `deno task <prefix>` / `pnpm run <prefix>`:
    /// replace only the script token.
    ScriptToken,
    /// The keyword word itself is active (`npm run`, `deno task`): it
    /// matches no script name, so the fill keeps the keyword.
    KeywordWord,
    /// Cursor is still on the manager word itself (`pnpm`).
    ManagerWord,
    /// The first word after the manager is empty or active (`pnpm de`).
    /// Direct-script managers mix native commands and non-conflicting scripts
    /// here; npm/deno expose native commands only.
    CommandToken,
}

pub(crate) struct ManagerPosition {
    pub(crate) spec: &'static ManagerSpec,
    pub(crate) position: Position,
    pub(crate) project_dir: std::path::PathBuf,
    pub(crate) workspace_root: bool,
    pub(crate) recursive: bool,
    pub(crate) include_workspace_root: bool,
    pub(crate) if_present: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct ManagerOptionPosition {
    pub(crate) spec: &'static ManagerSpec,
    pub(crate) after_script_keyword: bool,
}

pub(crate) fn manager_option_position(
    context: &CompletionContext,
) -> Option<ManagerOptionPosition> {
    let ManagerScanResult::OptionName {
        spec,
        after_script_keyword,
    } = scan_manager(context)?
    else {
        return None;
    };
    Some(ManagerOptionPosition {
        spec,
        after_script_keyword,
    })
}

pub(crate) fn manager_position(context: &CompletionContext) -> Option<ManagerPosition> {
    let ManagerScanResult::Ready(scan) = scan_manager(context)? else {
        return None;
    };
    if scan.selector.is_some()
        || scan.multiple_selectors
        || (scan.spec.name == "yarn"
            && scan.words.get(scan.command_index).copied() == Some("workspace"))
    {
        return None;
    }
    let position = if scan.active_index == 0 {
        Position::ManagerWord
    } else if scan.command_index == scan.active_index {
        let active = scan.words.get(scan.active_index).copied();
        if active.is_some_and(|word| word.starts_with('-')) {
            return None;
        }
        if active.is_some_and(|word| is_primary_script_keyword(scan.spec, word)) {
            Position::KeywordWord
        } else {
            Position::CommandToken
        }
    } else {
        let command = scan.words.get(scan.command_index).copied()?;
        let active = scan.words.get(scan.active_index).copied();
        if active.is_some_and(|word| word.starts_with('-')) {
            return None;
        }
        if is_script_keyword(scan.spec, command) && scan.operand_index == scan.active_index {
            Position::ScriptToken
        } else {
            return None;
        }
    };
    Some(ManagerPosition {
        spec: scan.spec,
        position,
        project_dir: scan.project_dir,
        workspace_root: scan.workspace_root,
        recursive: scan.recursive,
        include_workspace_root: scan.include_workspace_root,
        if_present: scan.if_present,
    })
}

pub(crate) fn manager_command(context: &CompletionContext) -> Option<&str> {
    let ManagerScanResult::Ready(scan) = scan_manager(context)? else {
        return None;
    };
    scan.words.get(scan.command_index).copied()
}

pub(crate) fn manager_project_dir(context: &CompletionContext) -> Option<std::path::PathBuf> {
    match scan_manager(context)? {
        ManagerScanResult::Ready(scan) => {
            if (scan.workspace_root || scan.recursive) && matches!(scan.spec.name, "pnpm" | "npm") {
                Some(
                    crate::project::discover_node_workspace(&scan.project_dir)
                        .map_or(scan.project_dir, |workspace| workspace.root),
                )
            } else {
                Some(scan.project_dir)
            }
        }
        ManagerScanResult::WorkspaceValue { project_dir, .. } => Some(project_dir),
        ManagerScanResult::OptionName { .. } | ManagerScanResult::Blocked => None,
    }
}

pub(crate) fn package_manager(context: &CompletionContext) -> Option<&'static ManagerSpec> {
    let command = context.command()?;
    let command = executable_basename(command);
    MANAGERS.iter().find(|spec| spec.name == command)
}

#[must_use]
pub(crate) fn is_package_manager(command: &str) -> bool {
    let command = executable_basename(command);
    MANAGERS.iter().any(|spec| spec.name == command)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceStyle {
    PnpmFilter,
    NpmWorkspace,
    YarnWorkspace,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FilterPosition {
    Value {
        style: WorkspaceStyle,
        project_dir: std::path::PathBuf,
        edit_prefix: String,
    },
    MemberCommand {
        style: WorkspaceStyle,
        project_dir: std::path::PathBuf,
        member: String,
    },
    MemberScripts {
        style: WorkspaceStyle,
        project_dir: std::path::PathBuf,
        member: String,
        on_keyword: bool,
        explicit_run: bool,
    },
}

/// Workspace-selection positions shared by `pnpm --filter`,
/// `npm --workspace`, and `yarn workspace`.
pub(crate) fn filter_position(context: &CompletionContext) -> Option<FilterPosition> {
    match scan_manager(context)? {
        ManagerScanResult::WorkspaceValue {
            style,
            project_dir,
            edit_prefix,
        } => Some(FilterPosition::Value {
            style,
            project_dir,
            edit_prefix,
        }),
        ManagerScanResult::Blocked => None,
        ManagerScanResult::OptionName { .. } => None,
        ManagerScanResult::Ready(scan) => {
            if scan.multiple_selectors {
                return None;
            }
            if let Some((style, member)) = scan.selector.clone() {
                return workspace_command_position(&scan, style, member, scan.command_index);
            }
            if scan.spec.name != "yarn"
                || scan.words.get(scan.command_index).copied() != Some("workspace")
            {
                return None;
            }
            let member_index = scan.command_index + 1;
            if member_index == scan.active_index {
                return Some(FilterPosition::Value {
                    style: WorkspaceStyle::YarnWorkspace,
                    project_dir: scan.project_dir,
                    edit_prefix: String::new(),
                });
            }
            let member = scan.words.get(member_index)?.to_string();
            workspace_command_position(
                &scan,
                WorkspaceStyle::YarnWorkspace,
                member,
                member_index + 1,
            )
        }
    }
}

struct ManagerScan<'a> {
    spec: &'static ManagerSpec,
    words: Vec<&'a str>,
    active_index: usize,
    command_index: usize,
    operand_index: usize,
    project_dir: std::path::PathBuf,
    selector: Option<(WorkspaceStyle, String)>,
    workspace_root: bool,
    recursive: bool,
    include_workspace_root: bool,
    if_present: bool,
    multiple_selectors: bool,
}

enum ManagerScanResult<'a> {
    Ready(ManagerScan<'a>),
    WorkspaceValue {
        style: WorkspaceStyle,
        project_dir: std::path::PathBuf,
        edit_prefix: String,
    },
    OptionName {
        spec: &'static ManagerSpec,
        after_script_keyword: bool,
    },
    Blocked,
}

fn scan_manager(context: &CompletionContext) -> Option<ManagerScanResult<'_>> {
    if redirect_target(context) || !effective_command_accepts_external(context) {
        return None;
    }
    let words = segment_words(context);
    let manager = words.first().map(|word| executable_basename(word))?;
    let spec = MANAGERS.iter().find(|spec| spec.name == manager)?;
    let active_word = context.parsed.replacement.start < context.parsed.replacement.end
        && context.parsed.replacement.start <= context.buffer.cursor
        && context.buffer.cursor <= context.parsed.replacement.end;
    let active_index = if active_word {
        words.len().saturating_sub(1)
    } else {
        words.len()
    };
    let invocation_dir = invocation_working_directory(context);
    let mut project_dir = invocation_dir.clone();
    let mut selector = None;
    let mut selector_count = 0;
    let mut workspace_root = false;
    let mut recursive = false;
    let mut include_workspace_root = false;
    let mut if_present = false;
    let mut index = 1;
    let mut options_ended = false;

    while index < active_index {
        let word = words[index];
        if word == "--" {
            index += 1;
            options_ended = true;
            break;
        }
        if let Some((kind, value)) = attached_manager_value(spec.name, word) {
            apply_manager_value(
                kind,
                value,
                &invocation_dir,
                &mut project_dir,
                &mut selector,
                &mut selector_count,
            );
            index += 1;
            continue;
        }
        if let Some((flag, enabled)) = attached_manager_boolean(spec.name, word) {
            apply_manager_boolean(
                spec.name,
                flag,
                enabled,
                &mut workspace_root,
                &mut recursive,
                &mut include_workspace_root,
                &mut if_present,
            );
            index += 1;
            continue;
        }
        if let Some(kind) = manager_value_option(spec.name, word) {
            if index + 1 >= active_index {
                return Some(match kind {
                    ManagerValue::Workspace(style) => ManagerScanResult::WorkspaceValue {
                        style,
                        project_dir,
                        edit_prefix: String::new(),
                    },
                    _ => ManagerScanResult::Blocked,
                });
            }
            let value = words[index + 1];
            apply_manager_value(
                kind,
                value,
                &invocation_dir,
                &mut project_dir,
                &mut selector,
                &mut selector_count,
            );
            index += 2;
            continue;
        }
        if word.starts_with('-') {
            if !manager_flag_without_value(spec.name, word) {
                return Some(ManagerScanResult::Blocked);
            }
            apply_manager_flag(
                spec.name,
                word,
                &mut workspace_root,
                &mut recursive,
                &mut include_workspace_root,
                &mut if_present,
            );
            index += 1;
            continue;
        }
        break;
    }
    // Resolve preceding global options before interpreting an attached
    // workspace selector. Otherwise `pnpm -C repo --filter=web` scans the
    // original cwd instead of `repo`. An attached word after `--` or after a
    // command token is an operand, not a manager-global selector.
    if !options_ended
        && index == active_index
        && let Some(active) = words.get(active_index).copied()
    {
        if let Some((style, edit_prefix)) = attached_workspace_style(spec.name, active) {
            return Some(ManagerScanResult::WorkspaceValue {
                style,
                project_dir,
                edit_prefix,
            });
        }
        if active.starts_with('-') {
            return Some(
                if active.contains('=') || attached_manager_value(spec.name, active).is_some() {
                    ManagerScanResult::Blocked
                } else {
                    ManagerScanResult::OptionName {
                        spec,
                        after_script_keyword: false,
                    }
                },
            );
        }
    }
    let command_index = index;
    let mut operand_index = command_index.saturating_add(1);
    if matches!(spec.name, "npm" | "pnpm")
        && words
            .get(command_index)
            .is_some_and(|command| is_script_keyword(spec, command))
        && command_index < active_index
    {
        let mut tail = command_index + 1;
        while tail < active_index {
            let word = words[tail];
            if word == "--" {
                return Some(ManagerScanResult::Blocked);
            }
            if let Some((kind, value)) = attached_manager_value(spec.name, word) {
                apply_manager_value(
                    kind,
                    value,
                    &invocation_dir,
                    &mut project_dir,
                    &mut selector,
                    &mut selector_count,
                );
                tail += 1;
                continue;
            }
            if let Some((flag, enabled)) = attached_manager_boolean(spec.name, word) {
                apply_manager_boolean(
                    spec.name,
                    flag,
                    enabled,
                    &mut workspace_root,
                    &mut recursive,
                    &mut include_workspace_root,
                    &mut if_present,
                );
                tail += 1;
                continue;
            }
            if let Some(kind) = manager_value_option(spec.name, word) {
                if tail + 1 >= active_index {
                    return Some(match kind {
                        ManagerValue::Workspace(style) => ManagerScanResult::WorkspaceValue {
                            style,
                            project_dir,
                            edit_prefix: String::new(),
                        },
                        _ => ManagerScanResult::Blocked,
                    });
                }
                let value = words[tail + 1];
                apply_manager_value(
                    kind,
                    value,
                    &invocation_dir,
                    &mut project_dir,
                    &mut selector,
                    &mut selector_count,
                );
                tail += 2;
                continue;
            }
            if word.starts_with('-') {
                if !manager_flag_without_value(spec.name, word) {
                    return Some(ManagerScanResult::Blocked);
                }
                apply_manager_flag(
                    spec.name,
                    word,
                    &mut workspace_root,
                    &mut recursive,
                    &mut include_workspace_root,
                    &mut if_present,
                );
                tail += 1;
                continue;
            }
            break;
        }
        if tail == active_index
            && let Some(active) = words.get(active_index).copied()
        {
            if let Some((style, edit_prefix)) = attached_workspace_style(spec.name, active) {
                return Some(ManagerScanResult::WorkspaceValue {
                    style,
                    project_dir,
                    edit_prefix,
                });
            }
            if active.starts_with('-') {
                return Some(
                    if active.contains('=') || attached_manager_value(spec.name, active).is_some() {
                        ManagerScanResult::Blocked
                    } else {
                        ManagerScanResult::OptionName {
                            spec,
                            after_script_keyword: true,
                        }
                    },
                );
            }
        }
        operand_index = tail;
    }
    Some(ManagerScanResult::Ready(ManagerScan {
        spec,
        words,
        active_index,
        command_index,
        operand_index,
        project_dir,
        selector,
        workspace_root,
        recursive,
        include_workspace_root,
        if_present,
        multiple_selectors: selector_count > 1,
    }))
}

#[derive(Clone, Copy)]
enum ManagerValue {
    Directory,
    Workspace(WorkspaceStyle),
    Other,
}

fn manager_value_option(manager: &str, option: &str) -> Option<ManagerValue> {
    match (manager, option) {
        ("pnpm", "-C" | "--dir") | ("npm", "--prefix") | ("yarn", "--cwd") | ("bun", "--cwd") => {
            Some(ManagerValue::Directory)
        }
        ("pnpm", "-F" | "--filter") => Some(ManagerValue::Workspace(WorkspaceStyle::PnpmFilter)),
        ("npm", "-w" | "--workspace") => {
            Some(ManagerValue::Workspace(WorkspaceStyle::NpmWorkspace))
        }
        (
            "pnpm",
            "--reporter"
            | "--workspace-concurrency"
            | "--store-dir"
            | "--global-dir"
            | "--global-bin-dir"
            | "--state-dir"
            | "--cache-dir"
            | "--virtual-store-dir"
            | "--lockfile-dir"
            | "--config-dir"
            | "--package-import-method"
            | "--network-concurrency"
            | "--fetch-retries"
            | "--fetch-retry-factor"
            | "--fetch-retry-mintimeout"
            | "--fetch-retry-maxtimeout"
            | "--fetch-timeout"
            | "--loglevel"
            | "--resume-from"
            | "--changed-files-ignore-pattern"
            | "--test-pattern",
        )
        | (
            "npm",
            "--location" | "--userconfig" | "--registry" | "--cache" | "--otp" | "--script-shell",
        )
        | ("yarn", "--mutex" | "--cache-folder" | "--modules-folder" | "--use-yarnrc")
        | ("bun", "--config" | "--backend")
        | (
            "deno",
            "--config" | "--cert" | "--location" | "--v8-flags" | "--import-map" | "--lock"
            | "--env-file" | "--node-modules-dir",
        ) => Some(ManagerValue::Other),
        _ => None,
    }
}

fn attached_manager_value<'a>(manager: &str, word: &'a str) -> Option<(ManagerValue, &'a str)> {
    if let Some((flag, value)) = word.split_once('=') {
        return manager_value_option(manager, flag).map(|kind| (kind, value));
    }
    if matches!(manager, "pnpm") && word.len() > 2 && word.starts_with("-C") {
        return Some((ManagerValue::Directory, &word[2..]));
    }
    if manager == "pnpm" && word.len() > 2 && word.starts_with("-F") {
        return Some((
            ManagerValue::Workspace(WorkspaceStyle::PnpmFilter),
            &word[2..],
        ));
    }
    if manager == "npm" && word.len() > 2 && word.starts_with("-w") && !word.starts_with("--") {
        return Some((
            ManagerValue::Workspace(WorkspaceStyle::NpmWorkspace),
            &word[2..],
        ));
    }
    None
}

fn attached_manager_boolean<'a>(manager: &str, word: &'a str) -> Option<(&'a str, bool)> {
    let (flag, value) = word.split_once('=')?;
    if !manager_flag_without_value(manager, flag) {
        return None;
    }
    match value {
        "true" => Some((flag, true)),
        "false" => Some((flag, false)),
        _ => None,
    }
}

fn attached_workspace_style(manager: &str, word: &str) -> Option<(WorkspaceStyle, String)> {
    if let Some((flag, _)) = word.split_once('=')
        && let Some(ManagerValue::Workspace(style)) = manager_value_option(manager, flag)
    {
        return Some((style, word[..flag.len() + 1].to_owned()));
    }
    if manager == "pnpm" && word.len() > 2 && word.starts_with("-F") {
        return Some((WorkspaceStyle::PnpmFilter, "-F".to_owned()));
    }
    if manager == "npm" && word.len() > 2 && word.starts_with("-w") && !word.starts_with("--") {
        return Some((WorkspaceStyle::NpmWorkspace, "-w".to_owned()));
    }
    None
}

fn apply_manager_value(
    kind: ManagerValue,
    value: &str,
    cwd: &std::path::Path,
    project_dir: &mut std::path::PathBuf,
    selector: &mut Option<(WorkspaceStyle, String)>,
    selector_count: &mut usize,
) {
    match kind {
        ManagerValue::Directory => {
            *project_dir = resolve_directory(cwd, value);
        }
        ManagerValue::Workspace(style) => {
            *selector_count = selector_count.saturating_add(1);
            *selector = Some((style, value.to_owned()));
        }
        ManagerValue::Other => {}
    }
}

pub(crate) fn resolve_directory(base: &std::path::Path, value: &str) -> std::path::PathBuf {
    if value == "~"
        && let Some(home) = std::env::home_dir()
    {
        return home;
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    let value = std::path::PathBuf::from(value);
    if value.is_absolute() {
        value
    } else {
        base.join(value)
    }
}

fn manager_flag_without_value(manager: &str, option: &str) -> bool {
    match manager {
        "pnpm" => matches!(
            option,
            "-w" | "--workspace-root"
                | "--no-workspace-root"
                | "-r"
                | "--recursive"
                | "--no-recursive"
                | "--include-workspace-root"
                | "--no-include-workspace-root"
                | "-y"
                | "--yes"
                | "--if-present"
                | "--no-if-present"
                | "--no-bail"
                | "--no-color"
                | "--fail-if-no-match"
                | "--report-summary"
                | "--reporter-hide-prefix"
                | "--sequential"
                | "--use-stderr"
                | "--silent"
                | "--stream"
                | "--parallel"
                | "--aggregate-output"
                | "--offline"
                | "--prefer-offline"
        ),
        "npm" => matches!(
            option,
            "-g" | "--global"
                | "--include-workspace-root"
                | "--no-include-workspace-root"
                | "--workspaces"
                | "--no-workspaces"
                | "--if-present"
                | "--no-if-present"
                | "--ignore-scripts"
                | "--foreground-scripts"
                | "--json"
                | "--silent"
                | "--verbose"
        ),
        "yarn" => matches!(
            option,
            "--verbose" | "--json" | "--silent" | "--offline" | "--immutable"
        ),
        "bun" => matches!(option, "--silent"),
        "deno" => matches!(option, "-q" | "--quiet" | "--no-config" | "--unstable"),
        _ => false,
    }
}

fn apply_manager_flag(
    manager: &str,
    option: &str,
    workspace_root: &mut bool,
    recursive: &mut bool,
    include_workspace_root: &mut bool,
    if_present: &mut bool,
) {
    apply_manager_boolean(
        manager,
        option,
        true,
        workspace_root,
        recursive,
        include_workspace_root,
        if_present,
    );
}

fn apply_manager_boolean(
    manager: &str,
    option: &str,
    enabled: bool,
    workspace_root: &mut bool,
    recursive: &mut bool,
    include_workspace_root: &mut bool,
    if_present: &mut bool,
) {
    let (option, enabled) = match (manager, option) {
        ("pnpm", "--no-workspace-root") => ("--workspace-root", !enabled),
        ("pnpm", "--no-recursive") => ("--recursive", !enabled),
        ("npm", "--no-workspaces") => ("--workspaces", !enabled),
        ("pnpm" | "npm", "--no-include-workspace-root") => ("--include-workspace-root", !enabled),
        ("pnpm" | "npm", "--no-if-present") => ("--if-present", !enabled),
        _ => (option, enabled),
    };
    match (manager, option) {
        ("pnpm", "-w" | "--workspace-root") => *workspace_root = enabled,
        ("pnpm", "-r" | "--recursive") | ("npm", "--workspaces") => {
            *recursive = enabled;
        }
        ("pnpm" | "npm", "--include-workspace-root") => {
            *include_workspace_root = enabled;
        }
        ("pnpm" | "npm", "--if-present") => *if_present = enabled,
        _ => {}
    }
}

fn is_script_keyword(spec: &ManagerSpec, word: &str) -> bool {
    is_primary_script_keyword(spec, word) || spec.name == "npm" && word == "run-script"
}

fn is_primary_script_keyword(spec: &ManagerSpec, word: &str) -> bool {
    Some(word) == spec.keyword || (spec.keyword.is_none() && word == "run")
}

fn workspace_command_position(
    scan: &ManagerScan<'_>,
    style: WorkspaceStyle,
    member: String,
    command_index: usize,
) -> Option<FilterPosition> {
    if command_index == scan.active_index {
        let active = scan.words.get(command_index).copied();
        if active.is_some_and(|word| word.starts_with('-')) {
            return None;
        }
        if active == Some("run") {
            return Some(FilterPosition::MemberScripts {
                style,
                project_dir: scan.project_dir.clone(),
                member,
                on_keyword: true,
                explicit_run: true,
            });
        }
        return Some(FilterPosition::MemberCommand {
            style,
            project_dir: scan.project_dir.clone(),
            member,
        });
    }
    let script_index = if command_index == scan.command_index {
        scan.operand_index
    } else {
        command_index + 1
    };
    if scan.words.get(command_index).copied() == Some("run") && script_index == scan.active_index {
        return Some(FilterPosition::MemberScripts {
            style,
            project_dir: scan.project_dir.clone(),
            member,
            on_keyword: false,
            explicit_run: true,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        completion::{BufferSnapshot, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    fn context(text: &str, cursor: usize) -> CompletionContext {
        context_for_shell(text, cursor, ShellKind::Zsh)
    }

    fn context_for_shell(text: &str, cursor: usize, shell: ShellKind) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(1),
            shell,
            PathBuf::from("/tmp"),
            BufferSnapshot::new(text, cursor, BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context")
    }

    #[test]
    fn shell_specific_modifiers_open_only_their_shell_command_slot() {
        for (shell, text) in [
            (ShellKind::Fish, "not cod"),
            (ShellKind::Fish, "and cod"),
            (ShellKind::Fish, "or cod"),
            (ShellKind::Zsh, "nocorrect cod"),
        ] {
            let context = context_for_shell(text, text.len(), shell);
            assert_eq!(
                context.command(),
                Some("cod"),
                "effective command for {text:?}"
            );
            assert!(command_position_open(&context), "command slot for {text:?}");
            assert!(
                shell_command_position_open(&context),
                "shell slot for {text:?}"
            );
            assert_eq!(segment_words(&context), ["cod"], "words for {text:?}");
        }

        for (shell, text, command) in [
            (ShellKind::Bash, "not cod", "not"),
            (ShellKind::Zsh, "and cod", "and"),
            (ShellKind::Fish, "sudo not cod", "not"),
            (ShellKind::Zsh, "sudo nocorrect cod", "nocorrect"),
        ] {
            let context = context_for_shell(text, text.len(), shell);
            assert_eq!(
                context.command(),
                Some(command),
                "effective command for {text:?}"
            );
            assert!(
                !command_position_open(&context),
                "argument slot for {text:?}"
            );
        }
    }

    #[test]
    fn argument_progress_measures_from_the_effective_command() {
        // Wrappers and assignments are skipped: position 0 is the first
        // argument after the effective command.
        let sudo_vim = context("sudo vim ", 9);
        let (words, position) = argument_progress(&sudo_vim).expect("progress");
        assert_eq!(words, ["vim"]);
        assert_eq!(position, 0);

        let prefixed = context("FOO=bar git checkout ", 21);
        let (words, position) = argument_progress(&prefixed).expect("progress");
        assert_eq!(words, ["git", "checkout"]);
        assert_eq!(position, 1);

        for text in [
            "sudo -u root vim ",
            "env -i FOO=bar vim ",
            "watch -n1 vim ",
            "timeout 2 vim ",
        ] {
            let wrapper = context(text, text.len());
            let (words, position) = argument_progress(&wrapper).expect("wrapper progress");
            assert_eq!(words, ["vim"], "effective words for {text:?}");
            assert_eq!(position, 0, "argument position for {text:?}");
        }

        // Still on the effective command word: no progress yet.
        assert!(argument_progress(&context("sudo vim", 8)).is_none());
        // Only a wrapper so far: no effective command, no progress.
        assert!(argument_progress(&context("sudo ", 5)).is_none());
    }

    #[test]
    fn pip_detection_supports_versioned_and_explicit_names() {
        for command in ["pip", "pip3", "pip3.14", "/opt/bin/pip3.13"] {
            assert!(is_pip_command(command));
        }
        for command in ["pipeline", "pipx", "pip3.", "pip3x"] {
            assert!(!is_pip_command(command));
        }
    }

    #[test]
    fn cursor_at_a_word_start_makes_that_word_the_active_one() {
        // `ls -la |foo` (cursor at the start of `foo`, whitespace before it):
        // `foo` is the ACTIVE word with an empty prefix, so the word before
        // the slot is `-la` — position must not count `foo` as finished.
        let at_word_start = context("ls -la foo", 7);
        let (words, position) = argument_progress(&at_word_start).expect("progress");
        assert_eq!(words, ["ls", "-la", "foo"]);
        assert_eq!(position, 1, "active word `foo` is slot 1, not 2");
        assert_eq!(words[position], "-la");

        // Same for the first argument: `git |checkout` — checkout is active.
        let first_argument = context("git checkout", 4);
        let (words, position) = argument_progress(&first_argument).expect("progress");
        assert_eq!(words, ["git", "checkout"]);
        assert_eq!(position, 0);
    }

    #[test]
    fn command_position_open_only_on_the_effective_command_word() {
        let open = |text: &str| command_position_open(&context(text, text.len()));
        // No effective command yet, or cursor on the command word.
        assert!(open(""));
        assert!(open("l"));
        assert!(open("ls"));
        assert!(open("sudo "));
        assert!(open("sudo l"));
        assert!(open("FOO=bar "));
        assert!(open("FOO=bar l"));
        assert!(open("sudo -u root "));
        assert!(open("env -i FOO=bar "));
        assert!(open("watch -n 1 "));
        // Argument positions: command-name completion must not fire.
        assert!(!open("ls "));
        assert!(!open("sudo ls "));
        assert!(!open("FOO=bar ls "));
        assert!(!open("git checkout "));
        assert!(!open("sudo -u root"));
        assert!(!open("env -i FOO=bar"));
        assert!(!open("watch -n 1"));
        // A path prefix is never a command name.
        assert!(!open("./l"));

        // Cursor mid-word on the effective command still counts.
        assert!(command_position_open(&context("sudo vim", 7)));
        // At the end of the command word it is open; inside an argument it is
        // not.
        assert!(command_position_open(&context("ls -la", 2)));
        assert!(!command_position_open(&context("ls -la", 4)));
    }

    #[test]
    fn package_manager_positions_stop_after_a_completed_command() {
        let position = |text: &str| {
            manager_position(&context(text, text.len())).map(|position| position.position)
        };
        assert_eq!(position("pnpm"), Some(Position::ManagerWord));
        assert_eq!(position("pnpm "), Some(Position::CommandToken));
        assert_eq!(position("pnpm de"), Some(Position::CommandToken));
        assert_eq!(position("pnpm run"), Some(Position::KeywordWord));
        assert_eq!(position("pnpm run "), Some(Position::ScriptToken));
        assert_eq!(position("pnpm run de"), Some(Position::ScriptToken));
        assert_eq!(
            position("pnpm run --if-present de"),
            Some(Position::ScriptToken)
        );
        assert_eq!(position("pnpm dev "), None);
        assert_eq!(position("pnpm install "), None);
        assert_eq!(position("npm ru"), Some(Position::CommandToken));
        assert_eq!(position("npm run"), Some(Position::KeywordWord));
        assert_eq!(position("npm run "), Some(Position::ScriptToken));
        assert_eq!(position("npm run-script"), Some(Position::CommandToken));
        assert_eq!(position("npm run-script "), Some(Position::ScriptToken));
        assert_eq!(position("npm run-script bu"), Some(Position::ScriptToken));
        assert_eq!(
            position("npm run --if-present bu"),
            Some(Position::ScriptToken)
        );
        assert_eq!(
            position("npm run --script-shell /bin/sh bu"),
            Some(Position::ScriptToken)
        );
        assert_eq!(position("npm run --unknown value"), None);
        assert_eq!(position("npm run -- bu"), None);
        assert_eq!(position("npm install "), None);
        assert_eq!(position("sudo pnpm de"), Some(Position::CommandToken));
        assert_eq!(position("corepack pnpm de"), Some(Position::CommandToken));
        assert_eq!(position("FOO=bar npm run bu"), Some(Position::ScriptToken));
        assert_eq!(position("pnpm -C app run bu"), Some(Position::ScriptToken));
        assert_eq!(position("pnpm --dir=app de"), Some(Position::CommandToken));
        assert_eq!(
            position("npm --prefix app run bu"),
            Some(Position::ScriptToken)
        );
        assert_eq!(position("yarn --cwd app de"), Some(Position::CommandToken));
        assert_eq!(position("pnpm -C "), None);
        assert_eq!(position("pnpm --reporter "), None);
        assert_eq!(position("pnpm --unknown value"), None);
        assert_eq!(position("pnpm > "), None);
        assert_eq!(position("pnpm > output"), None);
        assert_eq!(position("builtin pnpm "), None);

        let at_command_start = context("pnpm build", "pnpm ".len());
        assert_eq!(
            manager_position(&at_command_start).map(|position| position.position),
            Some(Position::CommandToken)
        );
        let at_script_start = context("npm run dev", "npm run ".len());
        assert_eq!(
            manager_position(&at_script_start).map(|position| position.position),
            Some(Position::ScriptToken)
        );

        let manager = manager_position(&context("pnpm -C app run bu", "pnpm -C app run bu".len()))
            .expect("manager position");
        assert_eq!(manager.project_dir, PathBuf::from("/tmp/app"));
    }

    #[test]
    fn working_directory_values_follow_shell_path_semantics() {
        let base = PathBuf::from("/tmp/project");
        assert_eq!(resolve_directory(&base, "app"), base.join("app"));
        assert_eq!(
            resolve_directory(&base, "/var/tmp/app"),
            PathBuf::from("/var/tmp/app")
        );
        if let Some(home) = std::env::home_dir() {
            assert_eq!(resolve_directory(&base, "~"), home);
            assert_eq!(resolve_directory(&base, "~/app"), home.join("app"));
        }
    }

    #[test]
    fn wrapper_working_directories_propagate_to_nested_providers() {
        assert_eq!(
            invocation_working_directory(&context(
                "sudo -D app env --chdir=sub cat ",
                "sudo -D app env --chdir=sub cat ".len(),
            )),
            PathBuf::from("/tmp/app/sub")
        );
        let manager = manager_position(&context(
            "env -C app pnpm run bu",
            "env -C app pnpm run bu".len(),
        ))
        .expect("manager behind env chdir");
        assert_eq!(manager.project_dir, PathBuf::from("/tmp/app"));
    }

    #[test]
    fn package_manager_workspace_flags_preserve_their_scope() {
        let root = manager_position(&context("pnpm -w run bu", "pnpm -w run bu".len()))
            .expect("workspace-root manager");
        assert!(root.workspace_root);
        assert!(!root.recursive);

        let recursive = manager_position(&context(
            "pnpm -r --include-workspace-root run bu",
            "pnpm -r --include-workspace-root run bu".len(),
        ))
        .expect("recursive manager");
        assert!(recursive.recursive);
        assert!(recursive.include_workspace_root);

        let npm = manager_position(&context(
            "npm --workspaces run bu",
            "npm --workspaces run bu".len(),
        ))
        .expect("npm workspaces manager");
        assert!(npm.recursive);
        assert!(!npm.if_present);

        let npm_after_command = manager_position(&context(
            "npm run --workspaces --include-workspace-root bu",
            "npm run --workspaces --include-workspace-root bu".len(),
        ))
        .expect("npm run trailing workspace flags");
        assert!(npm_after_command.recursive);
        assert!(npm_after_command.include_workspace_root);

        let optional = manager_position(&context(
            "npm --workspaces --if-present run bu",
            "npm --workspaces --if-present run bu".len(),
        ))
        .expect("npm optional workspace scripts");
        assert!(optional.if_present);

        let optional_last_value_wins = manager_position(&context(
            "npm --if-present --no-if-present --no-if-present=false --workspaces run bu",
            "npm --if-present --no-if-present --no-if-present=false --workspaces run bu".len(),
        ))
        .expect("npm if-present boolean ordering");
        assert!(optional_last_value_wins.if_present);

        let disabled = manager_position(&context(
            "pnpm --recursive=false run bu",
            "pnpm --recursive=false run bu".len(),
        ))
        .expect("disabled recursive flag");
        assert!(!disabled.recursive);

        let last_value_wins = manager_position(&context(
            "pnpm -r --include-workspace-root --recursive=false --include-workspace-root=false run bu",
            "pnpm -r --include-workspace-root --recursive=false --include-workspace-root=false run bu"
                .len(),
        ))
        .expect("last boolean values");
        assert!(!last_value_wins.recursive);
        assert!(!last_value_wins.include_workspace_root);

        let reenabled = manager_position(&context(
            "npm --workspaces=false --workspaces run bu",
            "npm --workspaces=false --workspaces run bu".len(),
        ))
        .expect("reenabled workspaces flag");
        assert!(reenabled.recursive);

        let negative_flags = manager_position(&context(
            "pnpm -w -r --include-workspace-root --no-workspace-root --no-recursive --no-include-workspace-root run bu",
            "pnpm -w -r --include-workspace-root --no-workspace-root --no-recursive --no-include-workspace-root run bu"
                .len(),
        ))
        .expect("negative workspace flags");
        assert!(!negative_flags.workspace_root);
        assert!(!negative_flags.recursive);
        assert!(!negative_flags.include_workspace_root);

        assert!(
            manager_position(&context(
                "pnpm --no-definitely-unknown run bu",
                "pnpm --no-definitely-unknown run bu".len(),
            ))
            .is_none(),
            "unknown --no-* flags must not be treated as modeled booleans"
        );

        assert_eq!(
            manager_position(&context(
                "pnpm --cache-dir cache run bu",
                "pnpm --cache-dir cache run bu".len(),
            ))
            .map(|position| position.position),
            Some(Position::ScriptToken)
        );
    }

    #[test]
    fn workspace_positions_stop_after_the_script_token() {
        let position = |text: &str| filter_position(&context(text, text.len()));
        assert!(matches!(
            position("pnpm --filter "),
            Some(FilterPosition::Value {
                style: WorkspaceStyle::PnpmFilter,
                edit_prefix,
                ..
            }) if edit_prefix.is_empty()
        ));
        assert!(matches!(
            position("pnpm --filter=we"),
            Some(FilterPosition::Value {
                style: WorkspaceStyle::PnpmFilter,
                edit_prefix,
                ..
            }) if edit_prefix == "--filter="
        ));
        assert!(matches!(
            position("pnpm -Fwe"),
            Some(FilterPosition::Value {
                style: WorkspaceStyle::PnpmFilter,
                edit_prefix,
                ..
            }) if edit_prefix == "-F"
        ));
        assert!(matches!(
            position("pnpm -C repo --filter=we"),
            Some(FilterPosition::Value {
                style: WorkspaceStyle::PnpmFilter,
                project_dir,
                edit_prefix,
            }) if project_dir.as_path() == std::path::Path::new("/tmp/repo")
                && edit_prefix == "--filter="
        ));
        assert!(matches!(
            position("pnpm --filter web "),
            Some(FilterPosition::MemberCommand {
                style: WorkspaceStyle::PnpmFilter,
                member,
                ..
            }) if member == "web"
        ));
        assert!(matches!(
            position("pnpm -C repo --filter web run"),
            Some(FilterPosition::MemberScripts {
                style: WorkspaceStyle::PnpmFilter,
                member,
                on_keyword: true,
                explicit_run: true,
                project_dir,
            }) if member == "web"
                && project_dir.as_path() == std::path::Path::new("/tmp/repo")
        ));
        assert!(matches!(
            position("pnpm --filter web run "),
            Some(FilterPosition::MemberScripts {
                style: WorkspaceStyle::PnpmFilter,
                member,
                on_keyword: false,
                explicit_run: true,
                ..
            }) if member == "web"
        ));
        assert!(matches!(
            position("pnpm run -F web bu"),
            Some(FilterPosition::MemberScripts {
                style: WorkspaceStyle::PnpmFilter,
                member,
                explicit_run: true,
                ..
            }) if member == "web"
        ));
        assert_eq!(position("pnpm --filter web run dev "), None);
        assert!(matches!(
            position("sudo pnpm --filter web "),
            Some(FilterPosition::MemberCommand {
                style: WorkspaceStyle::PnpmFilter,
                member,
                ..
            }) if member == "web"
        ));

        assert!(matches!(
            position("npm --workspace "),
            Some(FilterPosition::Value {
                style: WorkspaceStyle::NpmWorkspace,
                ..
            })
        ));
        assert!(matches!(
            position("npm -wwe"),
            Some(FilterPosition::Value {
                style: WorkspaceStyle::NpmWorkspace,
                edit_prefix,
                ..
            }) if edit_prefix == "-w"
        ));
        assert!(matches!(
            position("npm -w=we"),
            Some(FilterPosition::Value {
                style: WorkspaceStyle::NpmWorkspace,
                edit_prefix,
                ..
            }) if edit_prefix == "-w="
        ));
        assert!(matches!(
            position("npm --prefix repo --workspace=we"),
            Some(FilterPosition::Value {
                style: WorkspaceStyle::NpmWorkspace,
                project_dir,
                edit_prefix,
            }) if project_dir.as_path() == std::path::Path::new("/tmp/repo")
                && edit_prefix == "--workspace="
        ));
        assert!(matches!(
            position("npm -w web ru"),
            Some(FilterPosition::MemberCommand {
                style: WorkspaceStyle::NpmWorkspace,
                member,
                ..
            }) if member == "web"
        ));
        assert!(matches!(
            position("npm -w web run bu"),
            Some(FilterPosition::MemberScripts {
                style: WorkspaceStyle::NpmWorkspace,
                member,
                ..
            }) if member == "web"
        ));
        assert!(matches!(
            position("npm run --workspace "),
            Some(FilterPosition::Value {
                style: WorkspaceStyle::NpmWorkspace,
                ..
            })
        ));
        assert!(matches!(
            position("npm run --workspace=we"),
            Some(FilterPosition::Value {
                style: WorkspaceStyle::NpmWorkspace,
                edit_prefix,
                ..
            }) if edit_prefix == "--workspace="
        ));
        assert!(matches!(
            position("npm run --workspace web bu"),
            Some(FilterPosition::MemberScripts {
                style: WorkspaceStyle::NpmWorkspace,
                member,
                explicit_run: true,
                ..
            }) if member == "web"
        ));
        assert!(matches!(
            position("npm --workspace web run --if-present bu"),
            Some(FilterPosition::MemberScripts {
                style: WorkspaceStyle::NpmWorkspace,
                member,
                explicit_run: true,
                ..
            }) if member == "web"
        ));
        assert!(matches!(
            position("yarn workspace "),
            Some(FilterPosition::Value {
                style: WorkspaceStyle::YarnWorkspace,
                ..
            })
        ));
        assert!(matches!(
            position("yarn workspace web bu"),
            Some(FilterPosition::MemberCommand {
                style: WorkspaceStyle::YarnWorkspace,
                member,
                ..
            }) if member == "web"
        ));
        assert!(matches!(
            position("yarn workspace web run bu"),
            Some(FilterPosition::MemberScripts {
                style: WorkspaceStyle::YarnWorkspace,
                member,
                explicit_run: true,
                ..
            }) if member == "web"
        ));
        assert_eq!(position("pnpm -- --filter=we"), None);
    }

    #[test]
    fn manager_option_prefixes_are_distinct_from_values_and_repeated_selectors() {
        let option = |text: &str| manager_option_position(&context(text, text.len()));
        assert!(matches!(
            option("pnpm --f"),
            Some(ManagerOptionPosition {
                after_script_keyword: false,
                ..
            })
        ));
        assert!(matches!(
            option("pnpm run --if"),
            Some(ManagerOptionPosition {
                after_script_keyword: true,
                ..
            })
        ));
        assert!(option("pnpm --filter=we").is_none());
        assert!(option("pnpm -Capp").is_none());

        for text in [
            "pnpm --filter api --filter web de",
            "npm -w api --workspace web run de",
        ] {
            let context = context(text, text.len());
            assert!(manager_position(&context).is_none(), "manager: {text:?}");
            assert!(filter_position(&context).is_none(), "filter: {text:?}");
        }
    }

    #[test]
    fn redirects_are_path_slots_and_do_not_change_argument_progress() {
        for text in ["git > lo", "git 2> lo", "git checkout >& lo"] {
            let context = context(text, text.len());
            assert!(redirect_target(&context), "redirect target for {text:?}");
            assert!(argument_progress(&context).is_none());
            assert!(!command_position_open(&context));
        }

        let after = context("git checkout > log ma", "git checkout > log ma".len());
        let (words, position) = argument_progress(&after).expect("progress after redirect");
        assert_eq!(words, ["git", "checkout", "ma"]);
        assert_eq!(position, 1);

        let trailing = context("git > log ", "git > log ".len());
        let (words, position) = argument_progress(&trailing).expect("subcommand after redirect");
        assert_eq!(words, ["git"]);
        assert_eq!(position, 0);
    }
}
