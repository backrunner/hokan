use std::ops::Range;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum QuoteContext {
    #[default]
    Unquoted,
    Single,
    Double,
    Opaque,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenKind {
    Word,
    Whitespace,
    Pipe,
    AndIf,
    OrIf,
    Separator,
    Redirect,
    Comment,
    Opaque,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub range: Range<usize>,
    pub cooked_prefix: String,
    pub quote: QuoteContext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedLine {
    pub tokens: Vec<Token>,
    pub active_segment: Range<usize>,
    pub replacement: Range<usize>,
    pub quote: QuoteContext,
    pub command: Option<String>,
    /// Token range of the effective command word. Callers replacing whole
    /// lines must start the edit here so a wrapper/assignment prefix
    /// (`sudo …`, `FOO=bar …`) is preserved.
    pub command_range: Option<Range<usize>>,
    pub current_prefix: String,
}

pub fn parse_line(text: &str, cursor: usize) -> Result<ParsedLine, crate::Error> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return Err(crate::Error::Parse(
            "cursor does not fall on a UTF-8 boundary".into(),
        ));
    }
    let tokens = lex(text);
    let segment_start = tokens
        .iter()
        .filter(|token| token.range.end <= cursor && is_segment_boundary(token.kind))
        .map(|token| token.range.end)
        .next_back()
        .unwrap_or(0);
    let segment_end = tokens
        .iter()
        .find(|token| token.range.start >= cursor && is_segment_boundary(token.kind))
        .map_or(text.len(), |token| token.range.start);
    let active_segment = segment_start..segment_end;

    let current = tokens.iter().find(|token| {
        token.kind == TokenKind::Word
            && token.range.start <= cursor
            && cursor <= token.range.end
            && token.range.start >= active_segment.start
            && token.range.end <= active_segment.end
    });
    let (replacement, quote, current_prefix) = current.map_or_else(
        || (cursor..cursor, quote_at(text, cursor), String::new()),
        |token| {
            let prefix = cook_word(&text[token.range.start..cursor]);
            (token.range.clone(), quote_at(text, cursor), prefix)
        },
    );
    let command_token = {
        let words = semantic_word_tokens(&tokens, &active_segment);
        let cooked: Vec<&str> = words
            .iter()
            .map(|token| token.cooked_prefix.as_str())
            .collect();
        effective_command_index(&cooked).map(|index| words[index])
    };
    let command = command_token.map(|token| token.cooked_prefix.clone());
    let command_range = command_token.map(|token| token.range.clone());

    Ok(ParsedLine {
        tokens,
        active_segment,
        replacement,
        quote,
        command,
        command_range,
        current_prefix,
    })
}

/// Wrappers whose eventual positional argument is another executable. Their
/// common options are parsed below so completion can still find `ls` in
/// `sudo -u root ls`, `env -i ls`, or `watch -n 1 ls`.
const COMMAND_WRAPPERS: &[&str] = &[
    "!",
    "sudo",
    "doas",
    "command",
    "builtin",
    "nohup",
    "time",
    "watch",
    "env",
    "exec",
    "nice",
    "timeout",
    "xargs",
    "stdbuf",
    "setsid",
    "noglob",
    "nocorrect",
    "not",
    "and",
    "or",
    "corepack",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EffectiveCommandState {
    Found(usize),
    AwaitingCommand,
    AwaitingWrapperValue,
    WrapperCommand(usize),
    IndeterminateWrapper(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EffectiveCommandKind {
    /// Normal shell command position: aliases, functions, builtins, and PATH
    /// executables are all meaningful.
    Shell,
    /// An external wrapper will resolve the nested word through PATH.
    External,
    /// `command` bypasses aliases/functions but accepts builtins or PATH.
    ExternalOrBuiltin,
    /// `builtin` accepts a shell builtin name only.
    Builtin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EffectiveCommandAnalysis {
    pub(crate) state: EffectiveCommandState,
    pub(crate) privileged: bool,
    pub(crate) opaque: bool,
    pub(crate) kind: EffectiveCommandKind,
}

/// Index of the effective command word within `words` (the cooked semantic
/// words of a segment in order). Redirection targets must already have been
/// removed by [`semantic_word_tokens`].
pub(crate) fn effective_command_index(words: &[&str]) -> Option<usize> {
    match effective_command_state(words, false) {
        EffectiveCommandState::Found(index)
        | EffectiveCommandState::WrapperCommand(index)
        | EffectiveCommandState::IndeterminateWrapper(index) => Some(index),
        EffectiveCommandState::AwaitingCommand | EffectiveCommandState::AwaitingWrapperValue => {
            None
        }
    }
}

pub(crate) fn effective_command_index_for_shell(
    words: &[&str],
    shell: crate::shell::ShellKind,
) -> Option<usize> {
    match effective_command_state_for_shell(words, false, shell) {
        EffectiveCommandState::Found(index)
        | EffectiveCommandState::WrapperCommand(index)
        | EffectiveCommandState::IndeterminateWrapper(index) => Some(index),
        EffectiveCommandState::AwaitingCommand | EffectiveCommandState::AwaitingWrapperValue => {
            None
        }
    }
}

/// Effective-command parsing with awareness of whether the final word is
/// still active at the cursor. That distinction keeps `sudo -u root` on the
/// username value slot, while `sudo -u root ` opens executable completion.
pub(crate) fn effective_command_state(
    words: &[&str],
    last_word_active: bool,
) -> EffectiveCommandState {
    effective_command_analysis(words, last_word_active).state
}

pub(crate) fn effective_command_state_for_shell(
    words: &[&str],
    last_word_active: bool,
    shell: crate::shell::ShellKind,
) -> EffectiveCommandState {
    effective_command_analysis_for_shell(words, last_word_active, shell).state
}

pub(crate) fn effective_command_analysis(
    words: &[&str],
    last_word_active: bool,
) -> EffectiveCommandAnalysis {
    effective_command_analysis_impl(words, last_word_active, None)
}

pub(crate) fn effective_command_analysis_for_shell(
    words: &[&str],
    last_word_active: bool,
    shell: crate::shell::ShellKind,
) -> EffectiveCommandAnalysis {
    effective_command_analysis_impl(words, last_word_active, Some(shell))
}

/// Effective-command parsing for argv positions that are resolved directly
/// by an external process (for example `find -exec`). Shell-only dispatchers
/// such as `command`, `builtin`, and `!`, plus assignment-looking words, are
/// ordinary executable words there.
pub(crate) fn effective_external_command_state(
    words: &[&str],
    last_word_active: bool,
) -> EffectiveCommandState {
    effective_command_analysis_with_policy(
        words,
        last_word_active,
        None,
        EffectiveCommandKind::External,
        false,
    )
    .state
}

fn effective_command_analysis_impl(
    words: &[&str],
    last_word_active: bool,
    shell: Option<crate::shell::ShellKind>,
) -> EffectiveCommandAnalysis {
    effective_command_analysis_with_policy(
        words,
        last_word_active,
        shell,
        EffectiveCommandKind::Shell,
        true,
    )
}

fn effective_command_analysis_with_policy(
    words: &[&str],
    last_word_active: bool,
    shell: Option<crate::shell::ShellKind>,
    initial_kind: EffectiveCommandKind,
    initial_shell_syntax_allowed: bool,
) -> EffectiveCommandAnalysis {
    let mut index = 0;
    let mut privileged = false;
    let mut opaque = false;
    let mut kind = initial_kind;
    let mut shell_syntax_allowed = initial_shell_syntax_allowed;
    while shell_syntax_allowed
        && words
            .get(index)
            .is_some_and(|word| is_assignment_word(word))
    {
        if last_word_active && index + 1 == words.len() {
            return EffectiveCommandAnalysis {
                state: EffectiveCommandState::AwaitingWrapperValue,
                privileged,
                opaque,
                kind,
            };
        }
        index += 1;
    }
    loop {
        let Some(word) = words.get(index).copied() else {
            return EffectiveCommandAnalysis {
                state: EffectiveCommandState::AwaitingCommand,
                privileged,
                opaque,
                kind,
            };
        };
        let wrapper = basename(word);
        if !COMMAND_WRAPPERS.contains(&wrapper) || !wrapper_supported(wrapper, shell) {
            return EffectiveCommandAnalysis {
                state: EffectiveCommandState::Found(index),
                privileged,
                opaque,
                kind,
            };
        }
        let wrapper_kind = wrapper_kind(wrapper);
        if (word.contains('/') && wrapper_kind.is_shell_only())
            || (!shell_syntax_allowed && wrapper_kind.is_shell_only())
        {
            return EffectiveCommandAnalysis {
                state: EffectiveCommandState::Found(index),
                privileged,
                opaque,
                kind,
            };
        }
        privileged |= matches!(wrapper, "sudo" | "doas");
        opaque |= wrapper == "exec";
        let nested_shell_context =
            shell_syntax_allowed && !(wrapper_kind == WrapperKind::Time && word.contains('/'));
        let shell_wrapper = nested_shell_context.then_some(shell).flatten();
        match scan_wrapper(words, index, wrapper, last_word_active, shell_wrapper) {
            WrapperScan::Next(next) => {
                let (nested_kind, nested_shell_syntax) =
                    wrapper_kind.nested_policy(nested_shell_context);
                kind = nested_kind;
                shell_syntax_allowed = nested_shell_syntax;
                let nested_dispatcher = words.get(next).is_some_and(|word| {
                    !word.contains('/') && matches!(basename(word), "command" | "exec" | "builtin")
                });
                if matches!(wrapper_kind, WrapperKind::Command | WrapperKind::Builtin)
                    && nested_dispatcher
                {
                    index = next;
                    shell_syntax_allowed = true;
                    continue;
                }
                if wrapper_kind == WrapperKind::Builtin {
                    return EffectiveCommandAnalysis {
                        state: EffectiveCommandState::Found(next),
                        privileged,
                        opaque,
                        kind,
                    };
                }
                index = next;
            }
            WrapperScan::AwaitingCommand => {
                let (kind, _) = wrapper_kind.nested_policy(nested_shell_context);
                return EffectiveCommandAnalysis {
                    state: EffectiveCommandState::AwaitingCommand,
                    privileged,
                    opaque,
                    kind,
                };
            }
            WrapperScan::AwaitingValue => {
                return EffectiveCommandAnalysis {
                    state: EffectiveCommandState::AwaitingWrapperValue,
                    privileged,
                    opaque,
                    kind,
                };
            }
            WrapperScan::OwnCommand => {
                return EffectiveCommandAnalysis {
                    state: EffectiveCommandState::WrapperCommand(index),
                    privileged,
                    opaque,
                    kind,
                };
            }
            WrapperScan::Indeterminate => {
                return EffectiveCommandAnalysis {
                    state: EffectiveCommandState::IndeterminateWrapper(index),
                    privileged,
                    opaque,
                    kind,
                };
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WrapperKind {
    ShellModifier,
    External,
    Command,
    Builtin,
    Exec,
    Time,
}

impl WrapperKind {
    const fn is_shell_only(self) -> bool {
        matches!(
            self,
            Self::ShellModifier | Self::Command | Self::Builtin | Self::Exec
        )
    }

    const fn nested_policy(self, shell_syntax_allowed: bool) -> (EffectiveCommandKind, bool) {
        match self {
            Self::ShellModifier => (EffectiveCommandKind::Shell, true),
            Self::External => (EffectiveCommandKind::External, false),
            Self::Command => (EffectiveCommandKind::ExternalOrBuiltin, false),
            Self::Builtin => (EffectiveCommandKind::Builtin, false),
            Self::Exec => (EffectiveCommandKind::External, false),
            Self::Time if shell_syntax_allowed => (EffectiveCommandKind::Shell, true),
            Self::Time => (EffectiveCommandKind::External, false),
        }
    }
}

fn wrapper_kind(wrapper: &str) -> WrapperKind {
    match wrapper {
        "!" | "noglob" | "nocorrect" | "not" | "and" | "or" => WrapperKind::ShellModifier,
        "command" => WrapperKind::Command,
        "builtin" => WrapperKind::Builtin,
        "exec" => WrapperKind::Exec,
        "time" => WrapperKind::Time,
        _ => WrapperKind::External,
    }
}

fn wrapper_supported(wrapper: &str, shell: Option<crate::shell::ShellKind>) -> bool {
    match wrapper {
        "not" | "and" | "or" => shell == Some(crate::shell::ShellKind::Fish),
        "nocorrect" => shell == Some(crate::shell::ShellKind::Zsh),
        "noglob" => shell.is_none_or(|shell| shell == crate::shell::ShellKind::Zsh),
        "!" => shell.is_none_or(|shell| shell != crate::shell::ShellKind::Fish),
        _ => true,
    }
}

#[derive(Clone, Copy)]
enum WrapperScan {
    Next(usize),
    AwaitingCommand,
    AwaitingValue,
    OwnCommand,
    Indeterminate,
}

fn scan_wrapper(
    words: &[&str],
    wrapper_index: usize,
    wrapper: &str,
    last_word_active: bool,
    shell_wrapper: Option<crate::shell::ShellKind>,
) -> WrapperScan {
    if wrapper == "corepack" {
        return words
            .get(wrapper_index + 1)
            .copied()
            .filter(|command| matches!(*command, "npm" | "pnpm" | "yarn"))
            .map_or(WrapperScan::OwnCommand, |_| {
                WrapperScan::Next(wrapper_index + 1)
            });
    }
    let mut index = wrapper_index + 1;
    loop {
        let Some(word) = words.get(index).copied() else {
            return WrapperScan::AwaitingCommand;
        };
        if matches!(wrapper, "env" | "sudo") && is_assignment_word(word) {
            if last_word_active && index + 1 == words.len() {
                return WrapperScan::AwaitingValue;
            }
            index += 1;
            continue;
        }
        // A bare shell `time` is not the GNU/BSD executable. Bash's reserved
        // word accepts only `-p`; zsh and fish treat every following word as
        // the command itself. External forms (`/usr/bin/time`, `command time`,
        // `sudo time`) continue through the normal option table below.
        if wrapper == "time"
            && let Some(shell) = shell_wrapper
        {
            if shell == crate::shell::ShellKind::Bash && word == "-p" {
                index += 1;
                continue;
            }
            break;
        }
        if wrapper == "builtin"
            && shell_wrapper == Some(crate::shell::ShellKind::Zsh)
            && word == "--"
        {
            break;
        }
        if word == "--" {
            index += 1;
            break;
        }
        if !word.starts_with('-') || word == "-" {
            break;
        }
        if matches!(word, "--help" | "--version") {
            return WrapperScan::OwnCommand;
        }
        if (wrapper == "command" && command_query_option(word))
            || (wrapper == "sudo" && sudo_own_operation(word))
        {
            return WrapperScan::OwnCommand;
        }
        if wrapper == "env"
            && matches!(
                word.split_once('=').map_or(word, |(flag, _)| flag),
                "-S" | "--split-string"
            )
        {
            if option_has_attached_value(wrapper, word) {
                return WrapperScan::Indeterminate;
            }
            let value = index + 1;
            if value >= words.len() || (last_word_active && value + 1 == words.len()) {
                return WrapperScan::AwaitingValue;
            }
            return WrapperScan::Indeterminate;
        }
        if option_takes_value(wrapper, word) {
            if option_has_attached_value(wrapper, word) {
                index += 1;
                continue;
            }
            let value = index + 1;
            if value >= words.len() || (last_word_active && value + 1 == words.len()) {
                return WrapperScan::AwaitingValue;
            }
            index = value + 1;
            continue;
        }
        if option_without_value(wrapper, word) {
            index += 1;
            continue;
        }
        // Unknown wrapper options are intentionally conservative: treating
        // their following value as an executable would create unrelated PATH
        // rows and misclassify the command family.
        return WrapperScan::Indeterminate;
    }

    // `env -- NAME=value command` (and sudo's equivalent) still treats
    // assignment words as environment changes after option parsing ends.
    while matches!(wrapper, "env" | "sudo")
        && words
            .get(index)
            .is_some_and(|word| is_assignment_word(word))
    {
        if last_word_active && index + 1 == words.len() {
            return WrapperScan::AwaitingValue;
        }
        index += 1;
    }

    if wrapper == "timeout" {
        if index >= words.len() || (last_word_active && index + 1 == words.len()) {
            return WrapperScan::AwaitingValue;
        }
        index += 1; // duration
    }
    if index >= words.len() {
        WrapperScan::AwaitingCommand
    } else {
        WrapperScan::Next(index)
    }
}

/// Directory changes applied by command wrappers before `command_index`.
/// One value is returned per wrapper (the last option wins within a single
/// wrapper), while nested wrappers remain ordered so callers can resolve
/// relative paths successively: `sudo -D app env -C sub git` yields
/// `["app", "sub"]`.
pub(crate) fn wrapper_working_directories<'a>(
    words: &'a [&'a str],
    command_index: usize,
) -> Vec<&'a str> {
    wrapper_working_directories_impl(words, command_index, None)
}

pub(crate) fn wrapper_working_directories_for_shell<'a>(
    words: &'a [&'a str],
    command_index: usize,
    shell: crate::shell::ShellKind,
) -> Vec<&'a str> {
    wrapper_working_directories_impl(words, command_index, Some(shell))
}

fn wrapper_working_directories_impl<'a>(
    words: &'a [&'a str],
    command_index: usize,
    shell: Option<crate::shell::ShellKind>,
) -> Vec<&'a str> {
    let mut directories = Vec::new();
    let mut index = 0;
    while index < command_index
        && words
            .get(index)
            .is_some_and(|word| is_assignment_word(word))
    {
        index += 1;
    }

    while index < command_index {
        let wrapper = basename(words[index]);
        if !COMMAND_WRAPPERS.contains(&wrapper) || !wrapper_supported(wrapper, shell) {
            break;
        }
        index += 1;
        let mut directory = None;
        while index < command_index {
            let word = words[index];
            if matches!(wrapper, "env" | "sudo") && is_assignment_word(word) {
                index += 1;
                continue;
            }
            if word == "--" {
                index += 1;
                break;
            }
            if !word.starts_with('-') || word == "-" {
                break;
            }
            if wrapper == "env"
                && matches!(
                    word.split_once('=').map_or(word, |(flag, _)| flag),
                    "-S" | "--split-string"
                )
            {
                return directories;
            }
            if option_takes_value(wrapper, word) {
                if option_has_attached_value(wrapper, word) {
                    if let Some(value) = wrapper_directory_value(wrapper, word) {
                        directory = Some(value);
                    }
                    index += 1;
                    continue;
                }
                let Some(value) = words.get(index + 1).copied() else {
                    return directories;
                };
                if wrapper_directory_option(wrapper, word) {
                    directory = Some(value);
                }
                index += 2;
                continue;
            }
            if option_without_value(wrapper, word) {
                index += 1;
                continue;
            }
            return directories;
        }
        if let Some(directory) = directory {
            directories.push(directory);
        }

        while index < command_index
            && matches!(wrapper, "env" | "sudo")
            && words
                .get(index)
                .is_some_and(|word| is_assignment_word(word))
        {
            index += 1;
        }
        if wrapper == "timeout" && index < command_index {
            index += 1;
        }
    }
    directories
}

fn wrapper_directory_option(wrapper: &str, option: &str) -> bool {
    let option = option.split_once('=').map_or(option, |(name, _)| name);
    matches!(
        (wrapper, option),
        ("env", "-C" | "--chdir") | ("sudo", "-D" | "--chdir")
    )
}

fn wrapper_directory_value<'a>(wrapper: &str, option: &'a str) -> Option<&'a str> {
    if let Some((name, value)) = option.split_once('=') {
        return wrapper_directory_option(wrapper, name).then_some(value);
    }
    match wrapper {
        "env" => option.strip_prefix("-C").filter(|value| !value.is_empty()),
        "sudo" => option.strip_prefix("-D").filter(|value| !value.is_empty()),
        _ => None,
    }
}

fn option_takes_value(wrapper: &str, option: &str) -> bool {
    let option = option.split_once('=').map_or(option, |(name, _)| name);
    let exact = match wrapper {
        "sudo" => matches!(
            option,
            "-u" | "--user"
                | "-g"
                | "--group"
                | "-h"
                | "--host"
                | "-p"
                | "--prompt"
                | "-C"
                | "--close-from"
                | "-R"
                | "--chroot"
                | "-D"
                | "--chdir"
                | "-T"
                | "--command-timeout"
        ),
        "doas" => matches!(option, "-a" | "-C" | "-u"),
        "env" => matches!(
            option,
            "-u" | "--unset" | "-C" | "--chdir" | "-S" | "--split-string" | "-a" | "--argv0"
        ),
        "watch" => matches!(option, "-n" | "--interval" | "-q" | "--equexit"),
        "time" => matches!(option, "-f" | "--format" | "-o" | "--output"),
        "exec" => option == "-a",
        "nice" => matches!(option, "-n" | "--adjustment"),
        "timeout" => matches!(option, "-k" | "--kill-after" | "-s" | "--signal"),
        "xargs" => matches!(
            option,
            "-a" | "--arg-file"
                | "-E"
                | "-e"
                | "-I"
                | "-L"
                | "-n"
                | "-P"
                | "-s"
                | "-S"
                | "-d"
                | "--eof"
                | "--replace"
                | "--max-lines"
                | "--max-args"
                | "--max-procs"
                | "--max-chars"
                | "--delimiter"
                | "--process-slot-var"
        ),
        "stdbuf" => matches!(
            option,
            "-i" | "--input" | "-o" | "--output" | "-e" | "--error"
        ),
        _ => false,
    };
    exact
        || short_value_options(wrapper)
            .iter()
            .any(|name| option.len() > name.len() && option.starts_with(name))
}

fn option_has_attached_value(wrapper: &str, option: &str) -> bool {
    if option.contains('=') {
        return true;
    }
    short_value_options(wrapper)
        .iter()
        .any(|name| option.len() > name.len() && option.starts_with(name))
}

fn short_value_options(wrapper: &str) -> &'static [&'static str] {
    match wrapper {
        "sudo" => &["-u", "-g", "-h", "-p", "-C", "-R", "-D", "-T"],
        "doas" => &["-a", "-C", "-u"],
        "env" => &["-u", "-C", "-S", "-a"],
        "watch" => &["-n", "-q"],
        "time" => &["-f", "-o"],
        "exec" => &["-a"],
        "nice" => &["-n"],
        "timeout" => &["-k", "-s"],
        "xargs" => &["-a", "-E", "-e", "-I", "-L", "-n", "-P", "-s", "-S", "-d"],
        "stdbuf" => &["-i", "-o", "-e"],
        _ => &[],
    }
}

fn option_without_value(wrapper: &str, option: &str) -> bool {
    let option = option.split_once('=').map_or(option, |(name, _)| name);
    let exact = match wrapper {
        "sudo" => matches!(
            option,
            "-A" | "--askpass"
                | "-b"
                | "--background"
                | "-E"
                | "--preserve-env"
                | "-e"
                | "--edit"
                | "-H"
                | "--set-home"
                | "-i"
                | "--login"
                | "-K"
                | "-k"
                | "-n"
                | "--non-interactive"
                | "-P"
                | "--preserve-groups"
                | "-S"
                | "--stdin"
                | "-s"
                | "--shell"
        ),
        "doas" => matches!(option, "-L" | "-n" | "-s"),
        "command" => matches!(option, "-p" | "-v" | "-V"),
        "builtin" => false,
        "nohup" => false,
        "env" => matches!(
            option,
            "-i" | "--ignore-environment" | "-0" | "--null" | "-v" | "--debug"
        ),
        "watch" => matches!(
            option,
            "-d" | "--differences"
                | "-g"
                | "--chgexit"
                | "-e"
                | "--errexit"
                | "-b"
                | "--beep"
                | "-t"
                | "--no-title"
                | "-x"
                | "--exec"
                | "-c"
                | "--color"
                | "-p"
                | "--precise"
                | "-r"
                | "--no-rerun"
                | "-w"
                | "--no-wrap"
        ),
        "time" => matches!(
            option,
            "-a" | "--append" | "-p" | "--portability" | "-v" | "--verbose" | "-q" | "--quiet"
        ),
        "exec" => matches!(option, "-c" | "-l"),
        "nice" => {
            option.len() > 1
                && option[1..]
                    .chars()
                    .all(|character| character.is_ascii_digit())
        }
        "timeout" => matches!(
            option,
            "--foreground" | "--preserve-status" | "-v" | "--verbose"
        ),
        "xargs" => matches!(
            option,
            "-0" | "--null"
                | "-r"
                | "--no-run-if-empty"
                | "-t"
                | "--verbose"
                | "-p"
                | "--interactive"
                | "-x"
                | "--exit"
                | "-o"
                | "--open-tty"
                | "--show-limits"
        ),
        "stdbuf" => false,
        "setsid" => matches!(option, "-c" | "--ctty" | "-f" | "--fork" | "-w" | "--wait"),
        _ => false,
    };
    exact
        || match wrapper {
            "sudo" => short_flag_cluster(option, "AbEHiKknPSs"),
            "doas" => short_flag_cluster(option, "ns"),
            "env" => short_flag_cluster(option, "i0v"),
            "watch" => short_flag_cluster(option, "dgebtxcprw"),
            "time" => short_flag_cluster(option, "apvq"),
            "exec" => short_flag_cluster(option, "cl"),
            "timeout" => short_flag_cluster(option, "v"),
            "xargs" => short_flag_cluster(option, "0rtpxo"),
            "setsid" => short_flag_cluster(option, "cfw"),
            _ => false,
        }
}

fn short_flag_cluster(option: &str, allowed: &str) -> bool {
    option
        .strip_prefix('-')
        .filter(|flags| flags.len() > 1 && !flags.starts_with('-'))
        .is_some_and(|flags| flags.chars().all(|flag| allowed.contains(flag)))
}

pub(crate) fn command_query_option(option: &str) -> bool {
    matches!(option, "-v" | "-V")
        || option
            .strip_prefix('-')
            .filter(|flags| flags.len() > 1 && !flags.starts_with('-'))
            .is_some_and(|flags| {
                flags.chars().all(|flag| matches!(flag, 'p' | 'v' | 'V'))
                    && flags.chars().any(|flag| matches!(flag, 'v' | 'V'))
            })
}

fn sudo_own_operation(option: &str) -> bool {
    let option = option.split_once('=').map_or(option, |(flag, _)| flag);
    matches!(
        option,
        "-e" | "--edit" | "-l" | "--list" | "-V" | "--version" | "-v" | "--validate"
    ) || option
        .strip_prefix('-')
        .filter(|flags| flags.len() > 1 && !flags.starts_with('-'))
        .is_some_and(|flags| {
            flags.chars().all(|flag| "AbEeHiKklnPSsVv".contains(flag))
                && flags
                    .chars()
                    .any(|flag| matches!(flag, 'e' | 'l' | 'V' | 'v'))
        })
}

fn basename(command: &str) -> &str {
    command.rsplit('/').next().unwrap_or(command)
}

/// Shell words that participate in argv semantics for one segment. Redirect
/// operators, their targets, and an adjacent numeric fd designator are left
/// out, so `git 2>log checkout` has the same command words as
/// `git checkout`.
pub(crate) fn semantic_word_tokens<'a>(
    tokens: &'a [Token],
    segment: &Range<usize>,
) -> Vec<&'a Token> {
    let mut words: Vec<&Token> = Vec::new();
    let mut redirect_target = false;
    for token in tokens
        .iter()
        .filter(|token| token.range.start >= segment.start && token.range.end <= segment.end)
    {
        match token.kind {
            TokenKind::Redirect => {
                if words.last().is_some_and(|word| {
                    word.range.end == token.range.start
                        && word
                            .cooked_prefix
                            .chars()
                            .all(|character| character.is_ascii_digit())
                }) {
                    words.pop();
                }
                redirect_target = true;
            }
            TokenKind::Word => {
                if redirect_target {
                    redirect_target = false;
                } else {
                    words.push(token);
                }
            }
            TokenKind::Opaque if redirect_target => redirect_target = false,
            TokenKind::Comment => break,
            _ => {}
        }
    }
    words
}

/// A leading `NAME=value` environment assignment word.
fn is_assignment_word(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn lex(text: &str) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        if matches!(bytes[index], b'\n' | b'\r') {
            index += 1;
            if bytes[start] == b'\r' && bytes.get(index) == Some(&b'\n') {
                index += 1;
            }
            push_token(
                &mut tokens,
                TokenKind::Separator,
                start..index,
                text,
                QuoteContext::Unquoted,
            );
            continue;
        }
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            push_token(
                &mut tokens,
                TokenKind::Whitespace,
                start..index,
                text,
                QuoteContext::Unquoted,
            );
            continue;
        }
        if matches!(bytes[index..], [b'<', b'(', ..] | [b'>', b'(', ..]) {
            index = consume_opaque(bytes, index + 2);
            push_token(
                &mut tokens,
                TokenKind::Opaque,
                start..index,
                text,
                QuoteContext::Opaque,
            );
            continue;
        }
        let (kind, width) = match bytes[index..] {
            [b'&', b'&', ..] => (Some(TokenKind::AndIf), 2),
            // `&>` redirects both streams; lexing it as `&` + `>` would
            // invent a background separator that is not there.
            [b'&', b'>', b'>', ..] => (Some(TokenKind::Redirect), 3),
            [b'&', b'>', ..] => (Some(TokenKind::Redirect), 2),
            [b'&', ..] => (Some(TokenKind::Separator), 1),
            [b'|', b'|', ..] => (Some(TokenKind::OrIf), 2),
            [b'|', ..] => (Some(TokenKind::Pipe), 1),
            [b';', ..] => (Some(TokenKind::Separator), 1),
            // `>&` (fd duplication / both-streams redirect) and `>>&` are
            // single operators, not `>` followed by a background `&`.
            [b'>', b'>', b'&', ..] => (Some(TokenKind::Redirect), 3),
            [b'<', b'<', b'<', ..] | [b'<', b'<', b'-', ..] => (Some(TokenKind::Redirect), 3),
            [b'>', b'>', ..] | [b'<', b'<', ..] => (Some(TokenKind::Redirect), 2),
            [b'>', b'&', ..] => (Some(TokenKind::Redirect), 2),
            [b'<', b'&', ..] | [b'<', b'>', ..] | [b'>', b'|', ..] => {
                (Some(TokenKind::Redirect), 2)
            }
            [b'<', ..] | [b'>', ..] => (Some(TokenKind::Redirect), 1),
            [b'#', ..] => (Some(TokenKind::Comment), bytes.len() - index),
            _ => (None, 0),
        };
        if let Some(kind) = kind {
            index += width;
            push_token(
                &mut tokens,
                kind,
                start..index,
                text,
                QuoteContext::Unquoted,
            );
            continue;
        }

        let mut quote = QuoteContext::Unquoted;
        let mut token_quote = QuoteContext::Unquoted;
        while index < bytes.len() {
            let byte = bytes[index];
            match quote {
                QuoteContext::Unquoted => match byte {
                    b'\'' => {
                        quote = QuoteContext::Single;
                        if token_quote != QuoteContext::Opaque {
                            token_quote = QuoteContext::Single;
                        }
                        index += 1;
                    }
                    b'"' => {
                        quote = QuoteContext::Double;
                        if token_quote != QuoteContext::Opaque {
                            token_quote = QuoteContext::Double;
                        }
                        index += 1;
                    }
                    b'\\' => index = (index + 2).min(bytes.len()),
                    b'$' if bytes.get(index + 1) == Some(&b'(') => {
                        index = consume_opaque(bytes, index + 2);
                        token_quote = QuoteContext::Opaque;
                    }
                    b'$' if is_zsh_eval_expansion(bytes, index) => {
                        index += 1;
                        token_quote = QuoteContext::Opaque;
                    }
                    b'`' => {
                        index = consume_backticks(bytes, index + 1);
                        token_quote = QuoteContext::Opaque;
                    }
                    b if b.is_ascii_whitespace() || b"|;&<>".contains(&b) => break,
                    _ => index += utf8_width_at(bytes, index),
                },
                QuoteContext::Single => {
                    index += utf8_width_at(bytes, index);
                    if byte == b'\'' {
                        quote = QuoteContext::Unquoted;
                    }
                }
                QuoteContext::Double => match byte {
                    b'"' => {
                        quote = QuoteContext::Unquoted;
                        index += 1;
                    }
                    b'\\' => index = (index + 2).min(bytes.len()),
                    b'$' if bytes.get(index + 1) == Some(&b'(') => {
                        index = consume_opaque(bytes, index + 2);
                        token_quote = QuoteContext::Opaque;
                    }
                    b'$' if is_zsh_eval_expansion(bytes, index) => {
                        index += 1;
                        token_quote = QuoteContext::Opaque;
                    }
                    b'`' => {
                        index = consume_backticks(bytes, index + 1);
                        token_quote = QuoteContext::Opaque;
                    }
                    _ => index += utf8_width_at(bytes, index),
                },
                QuoteContext::Opaque => index += utf8_width_at(bytes, index),
            }
        }
        push_token(
            &mut tokens,
            TokenKind::Word,
            start..index,
            text,
            token_quote,
        );
    }
    tokens
}

fn push_token(
    tokens: &mut Vec<Token>,
    kind: TokenKind,
    range: Range<usize>,
    text: &str,
    quote: QuoteContext,
) {
    tokens.push(Token {
        kind,
        cooked_prefix: if kind == TokenKind::Word {
            cook_word(&text[range.clone()])
        } else {
            String::new()
        },
        range,
        quote,
    });
}

fn quote_at(text: &str, cursor: usize) -> QuoteContext {
    let prefix = &text[..cursor];
    let mut quote = QuoteContext::Unquoted;
    let mut escaped = false;
    for character in prefix.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, character) {
            (QuoteContext::Unquoted | QuoteContext::Double, '\\') => escaped = true,
            (QuoteContext::Unquoted, '\'') => quote = QuoteContext::Single,
            (QuoteContext::Unquoted, '"') => quote = QuoteContext::Double,
            (QuoteContext::Single, '\'') | (QuoteContext::Double, '"') => {
                quote = QuoteContext::Unquoted;
            }
            _ => {}
        }
    }
    quote
}

fn cook_word(raw: &str) -> String {
    let mut output = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut quote = QuoteContext::Unquoted;
    while let Some(character) = chars.next() {
        match quote {
            QuoteContext::Unquoted => match character {
                '\'' => quote = QuoteContext::Single,
                '"' => quote = QuoteContext::Double,
                '\\' => {
                    if let Some(next) = chars.next()
                        && next != '\n'
                    {
                        output.push(next);
                    }
                }
                _ => output.push(character),
            },
            QuoteContext::Single => {
                if character == '\'' {
                    quote = QuoteContext::Unquoted;
                } else {
                    output.push(character);
                }
            }
            QuoteContext::Double => match character {
                '"' => quote = QuoteContext::Unquoted,
                '\\' => match chars.peek().copied() {
                    Some(next @ ('$' | '`' | '"' | '\\')) => {
                        chars.next();
                        output.push(next);
                    }
                    Some('\n') => {
                        chars.next();
                    }
                    _ => output.push('\\'),
                },
                _ => output.push(character),
            },
            QuoteContext::Opaque => output.push(character),
        }
    }
    output
}

const fn is_segment_boundary(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Pipe | TokenKind::AndIf | TokenKind::OrIf | TokenKind::Separator
    )
}

fn consume_opaque(bytes: &[u8], mut index: usize) -> usize {
    let mut depth = 1_u32;
    while index < bytes.len() && depth > 0 {
        match bytes[index] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'\\' => index = (index + 1).min(bytes.len()),
            _ => {}
        }
        index += 1;
    }
    index
}

fn consume_backticks(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() {
        match bytes[index] {
            b'`' => return index + 1,
            b'\\' => index = (index + 2).min(bytes.len()),
            _ => index += 1,
        }
    }
    index
}

fn is_zsh_eval_expansion(bytes: &[u8], index: usize) -> bool {
    let Some(flags) = bytes.get(index.saturating_add(3)..) else {
        return false;
    };
    if bytes.get(index..index.saturating_add(3)) != Some(b"${(".as_slice()) {
        return false;
    }
    let Some(end) = flags.iter().position(|byte| *byte == b')') else {
        return false;
    };
    flags[..end].contains(&b'e')
}

fn utf8_width_at(bytes: &[u8], index: usize) -> usize {
    let width = match bytes[index] {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    };
    width.min(bytes.len() - index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn finds_segment_command_and_replacement() {
        let text = "cat data | rg 'fo";
        let parsed = parse_line(text, text.len()).expect("line should parse");
        assert_eq!(&text[parsed.active_segment], " rg 'fo");
        assert_eq!(parsed.command.as_deref(), Some("rg"));
        assert_eq!(&text[parsed.replacement], "'fo");
        assert_eq!(parsed.current_prefix, "fo");
        assert_eq!(parsed.quote, QuoteContext::Single);
    }

    #[test]
    fn accepts_incomplete_unicode_quotes_and_opaque_substitutions() {
        let text = "echo \"中 $(date";
        let parsed = parse_line(text, text.len()).expect("incomplete input should parse");
        assert_eq!(parsed.command.as_deref(), Some("echo"));
        assert!(
            parsed
                .tokens
                .iter()
                .any(|token| token.quote == QuoteContext::Opaque)
        );
    }

    #[test]
    fn cooks_backslashes_according_to_the_active_quote_context() {
        let cases = [
            (r"cat 'dir\name/fi", r"dir\name/fi"),
            (r#"cat "dir\q/fi""#, r"dir\q/fi"),
            (r#"cat "dir\\name/fi""#, r"dir\name/fi"),
            (r"cat dir\ name/fi", "dir name/fi"),
        ];
        for (text, expected) in cases {
            let parsed = parse_line(text, text.len()).expect("quoted path");
            assert_eq!(parsed.current_prefix, expected, "prefix for {text:?}");
        }
    }

    #[test]
    fn marks_executable_substitutions_as_opaque_in_every_executable_context() {
        for text in [
            "echo $(rm -rf /)",
            "echo `rm -rf /`",
            "echo \"$(rm -rf /)\"",
            "echo \"`rm -rf /`\"",
            "cat <(rm -rf /)",
            "cat >(rm -rf /)",
            "echo ${(e)payload}",
            "echo \"${(Xe)payload}\"",
            "echo ${${(e)name}}",
        ] {
            let parsed = parse_line(text, text.len()).expect("line should parse");
            assert!(
                parsed
                    .tokens
                    .iter()
                    .any(|token| token.quote == QuoteContext::Opaque),
                "missing opaque token for {text:?}"
            );
        }
    }

    #[test]
    fn fd_duplication_redirects_lex_as_single_redirect_tokens() {
        for (text, redirect) in [
            ("echo hi 2>&1", ">&"),
            ("echo hi &> file", "&>"),
            ("echo hi &>> file", "&>>"),
            ("echo hi >>& file", ">>&"),
            ("cat <<-EOF", "<<-"),
        ] {
            let parsed = parse_line(text, text.len()).expect("line should parse");
            let redirects: Vec<_> = parsed
                .tokens
                .iter()
                .filter(|token| token.kind == TokenKind::Redirect)
                .map(|token| &text[token.range.clone()])
                .collect();
            assert!(
                redirects.contains(&redirect),
                "missing {redirect:?} redirect token in {text:?}: {redirects:?}"
            );
            assert!(
                !parsed
                    .tokens
                    .iter()
                    .any(|token| token.kind == TokenKind::Separator),
                "unexpected separator for {text:?}"
            );
        }
        // `&&` must still lex as AndIf, not `&` + `>&`.
        let parsed = parse_line("a && b", 5).expect("line should parse");
        assert!(
            parsed
                .tokens
                .iter()
                .any(|token| token.kind == TokenKind::AndIf)
        );
    }

    #[test]
    fn bare_background_operator_terminates_the_word() {
        // A lone `&` (background operator) must not wedge the lexer: it is a
        // separator, so `sleep 1 &` and a trailing `&` both tokenize.
        for text in ["sleep 1 &", "sleep 1 & wait", "echo &"] {
            let parsed = parse_line(text, text.len()).expect("line should parse");
            assert!(
                parsed
                    .tokens
                    .iter()
                    .any(|token| token.kind == TokenKind::Separator),
                "missing separator for {text:?}"
            );
        }
        let parsed = parse_line("sleep 1 & wait", 13).expect("line should parse");
        assert_eq!(parsed.command.as_deref(), Some("wait"));
    }

    #[test]
    fn unquoted_newlines_start_a_new_command_segment() {
        let text = "echo first\ncod";
        let parsed = parse_line(text, text.len()).expect("line should parse");
        assert_eq!(parsed.command.as_deref(), Some("cod"));
        assert_eq!(&text[parsed.active_segment], "cod");

        let quoted = "echo 'first\nsecond'";
        let parsed = parse_line(quoted, quoted.len()).expect("quoted newline");
        assert_eq!(parsed.command.as_deref(), Some("echo"));
        assert_eq!(
            parsed
                .tokens
                .iter()
                .filter(|token| token.kind == TokenKind::Separator)
                .count(),
            0
        );
    }

    #[test]
    fn effective_command_skips_assignments_and_wrappers() {
        let cases: &[(&str, Option<&str>)] = &[
            ("FOO=bar ls ", Some("ls")),
            ("FOO=bar BAZ=qux ls -la", Some("ls")),
            ("sudo vim f", Some("vim")),
            ("sudo git checkout ", Some("git")),
            ("env FOO=bar ls ", Some("ls")),
            ("env FOO=bar sudo ls ", Some("ls")),
            ("time ls -la", Some("ls")),
            ("nohup make ", Some("make")),
            ("sudo -u root ls ", Some("ls")),
            ("sudo --user=root ls ", Some("ls")),
            ("sudo -nE ls ", Some("ls")),
            ("sudo FOO=bar ls ", Some("ls")),
            ("sudo -- FOO=bar ls ", Some("ls")),
            ("doas -u root ls ", Some("ls")),
            ("env -i ls ", Some("ls")),
            ("env -- FOO=bar ls ", Some("ls")),
            ("env -i -- FOO=bar ls ", Some("ls")),
            ("env -u DEBUG ls ", Some("ls")),
            ("env -a custom ls ", Some("ls")),
            ("watch -n1 ls ", Some("ls")),
            ("watch -n 1 ls ", Some("ls")),
            ("watch -dx ls ", Some("ls")),
            ("watch -q 2 ls ", Some("ls")),
            ("timeout 2 ls ", Some("ls")),
            ("xargs -n1 ls ", Some("ls")),
            ("xargs -0r ls ", Some("ls")),
            ("xargs -a input ls ", Some("ls")),
            ("nice -n5 ls ", Some("ls")),
            ("nice -5 ls ", Some("ls")),
            ("stdbuf -oL ls ", Some("ls")),
            ("setsid -f ls ", Some("ls")),
            ("setsid -fw ls ", Some("ls")),
            ("corepack pnpm run ", Some("pnpm")),
            ("corepack npm run ", Some("npm")),
            ("corepack yarn build ", Some("yarn")),
            ("corepack enable ", Some("corepack")),
            ("sudo env rm ", Some("rm")),
            ("command env rm ", Some("rm")),
            ("command command rm ", Some("rm")),
            ("command exec rm ", Some("rm")),
            ("command builtin echo ", Some("echo")),
            ("time builtin echo ", Some("echo")),
            // Shell-only dispatchers lose that meaning once an external
            // wrapper owns the command argument.
            ("sudo builtin rm ", Some("builtin")),
            ("sudo command rm ", Some("command")),
            ("sudo exec rm ", Some("exec")),
            ("builtin env rm ", Some("env")),
            ("builtin command rm ", Some("rm")),
            ("builtin exec rm ", Some("rm")),
            ("builtin builtin echo ", Some("echo")),
            // Query/edit modes consume arguments without executing them as a
            // nested command.
            ("command -v rm", Some("command")),
            ("command -pv rm", Some("command")),
            ("sudo -e /etc/hosts", Some("sudo")),
            ("sudo -nl rm", Some("sudo")),
            ("env -S 'rm -rf /'", Some("env")),
            // Only wrappers/assignments so far: no effective command yet.
            ("sudo ", None),
            ("sudo", None),
            ("FOO=bar ", None),
            ("env FOO=bar ", None),
            ("sudo -u root ", None),
            ("env -i FOO=bar ", None),
            ("watch -n 1 ", None),
            // Unknown wrapper options stay conservative.
            ("sudo --not-a-real-option value ls ", Some("sudo")),
        ];
        for (text, expected) in cases {
            let parsed = parse_line(text, text.len()).expect("line should parse");
            assert_eq!(
                parsed.command.as_deref(),
                *expected,
                "effective command for {text:?}"
            );
            assert_eq!(
                parsed
                    .command_range
                    .as_ref()
                    .map(|range| &text[range.clone()]),
                expected.map(|command| {
                    let start = text.rfind(command).expect("command in text");
                    &text[start..start + command.len()]
                }),
                "command range for {text:?}"
            );
        }
    }

    #[test]
    fn effective_command_tracks_the_nested_resolution_domain() {
        let cases = [
            ("ls", EffectiveCommandKind::Shell),
            ("time ls", EffectiveCommandKind::Shell),
            ("/usr/bin/time ls", EffectiveCommandKind::External),
            ("sudo ls", EffectiveCommandKind::External),
            ("command ls", EffectiveCommandKind::ExternalOrBuiltin),
            (
                "command command ls",
                EffectiveCommandKind::ExternalOrBuiltin,
            ),
            ("command exec ls", EffectiveCommandKind::External),
            ("command builtin echo", EffectiveCommandKind::Builtin),
            ("builtin echo", EffectiveCommandKind::Builtin),
            (
                "builtin command ls",
                EffectiveCommandKind::ExternalOrBuiltin,
            ),
            ("builtin exec ls", EffectiveCommandKind::External),
            ("builtin builtin echo", EffectiveCommandKind::Builtin),
            ("sudo builtin echo", EffectiveCommandKind::External),
        ];
        for (text, expected) in cases {
            let parsed = parse_line(text, text.len()).expect("line should parse");
            let words: Vec<&str> = semantic_word_tokens(&parsed.tokens, &parsed.active_segment)
                .into_iter()
                .map(|token| token.cooked_prefix.as_str())
                .collect();
            assert_eq!(
                effective_command_analysis(&words, false).kind,
                expected,
                "resolution domain for {text:?}"
            );
        }
    }

    #[test]
    fn shell_time_options_do_not_borrow_the_external_time_grammar() {
        let cases: &[(&[&str], crate::shell::ShellKind, EffectiveCommandState)] = &[
            (
                &["time", "-p", "ls"],
                crate::shell::ShellKind::Bash,
                EffectiveCommandState::Found(2),
            ),
            (
                &["time", "-f", "fmt", "ls"],
                crate::shell::ShellKind::Bash,
                EffectiveCommandState::Found(1),
            ),
            (
                &["time", "-p", "ls"],
                crate::shell::ShellKind::Zsh,
                EffectiveCommandState::Found(1),
            ),
            (
                &["time", "--", "ls"],
                crate::shell::ShellKind::Zsh,
                EffectiveCommandState::Found(1),
            ),
            (
                &["/usr/bin/time", "-f", "fmt", "ls"],
                crate::shell::ShellKind::Zsh,
                EffectiveCommandState::Found(3),
            ),
            (
                &["sudo", "time", "-f", "fmt", "ls"],
                crate::shell::ShellKind::Zsh,
                EffectiveCommandState::Found(4),
            ),
        ];
        for (words, shell, expected) in cases {
            assert_eq!(
                effective_command_state_for_shell(words, false, *shell),
                *expected,
                "effective command for {words:?} in {shell:?}"
            );
        }
    }

    #[test]
    fn external_command_slots_do_not_apply_shell_only_dispatchers() {
        for words in [
            &["command"][..],
            &["builtin"][..],
            &["exec"][..],
            &["!"][..],
            &["noglob"][..],
            &["FOO=bar"][..],
        ] {
            assert_eq!(
                effective_external_command_state(words, false),
                EffectiveCommandState::Found(0),
                "external slot must stop at {words:?}"
            );
        }
        for words in [
            &["env"][..],
            &["env", "FOO=bar"][..],
            &["sudo", "-nE"][..],
            &["time", "-o", "report.txt"][..],
            &["xargs", "-0r"][..],
        ] {
            assert_eq!(
                effective_external_command_state(words, false),
                EffectiveCommandState::AwaitingCommand,
                "external wrapper must expose its command after {words:?}"
            );
        }
    }

    #[test]
    fn builtin_does_not_borrow_command_flags() {
        for shell in [crate::shell::ShellKind::Bash, crate::shell::ShellKind::Zsh] {
            assert_eq!(
                effective_command_state_for_shell(&["builtin", "-p", "echo"], false, shell),
                EffectiveCommandState::IndeterminateWrapper(0),
                "builtin -p in {shell:?}"
            );
        }
        assert_eq!(
            effective_command_state_for_shell(
                &["builtin", "--", "echo"],
                false,
                crate::shell::ShellKind::Bash,
            ),
            EffectiveCommandState::Found(2),
        );
        assert_eq!(
            effective_command_state_for_shell(
                &["builtin", "--", "echo"],
                false,
                crate::shell::ShellKind::Zsh,
            ),
            EffectiveCommandState::Found(1),
        );
    }

    #[test]
    fn wrapper_working_directories_follow_nested_wrapper_order() {
        let cases: &[(&[&str], &[&str])] = &[
            (&["env", "-C", "app", "cat"], &["app"]),
            (
                &["sudo", "-Dapp", "env", "--chdir=sub", "git"],
                &["app", "sub"],
            ),
            (&["sudo", "-C3", "env", "-C", "app", "git"], &["app"]),
            (&["env", "-C", "first", "-C", "second", "cat"], &["second"]),
            (&["sudo", "make", "-Dtarget"], &[]),
        ];
        for (words, expected) in cases {
            let command_index = effective_command_index(words).expect("effective command");
            assert_eq!(
                wrapper_working_directories(words, command_index),
                *expected,
                "wrapper directories for {words:?}"
            );
        }
    }

    #[test]
    fn redirects_do_not_become_command_or_argument_words() {
        let cases = [
            ("2>error.log git status", Some("git")),
            ("git 2>error.log status", Some("git")),
            ("<input.txt cat", Some("cat")),
            (">output.txt", None),
        ];
        for (text, command) in cases {
            let parsed = parse_line(text, text.len()).expect("line should parse");
            assert_eq!(parsed.command.as_deref(), command, "command for {text:?}");
            let words: Vec<_> = semantic_word_tokens(&parsed.tokens, &parsed.active_segment)
                .into_iter()
                .map(|token| token.cooked_prefix.as_str())
                .collect();
            assert!(
                !words
                    .iter()
                    .any(|word| word.contains(".log") || *word == "input.txt"),
                "redirect targets leaked into {words:?}"
            );
        }
    }

    proptest! {
        #[test]
        fn arbitrary_short_unicode_lines_parse_at_every_character_boundary(
            characters in proptest::collection::vec(any::<char>(), 0..64),
        ) {
            let text: String = characters.into_iter().collect();
            for cursor in text
                .char_indices()
                .map(|(index, _)| index)
                .chain(std::iter::once(text.len()))
            {
                let parsed = parse_line(&text, cursor).expect("valid UTF-8 boundary");
                prop_assert!(parsed.active_segment.start <= cursor);
                prop_assert!(cursor <= parsed.active_segment.end);
                prop_assert!(parsed.replacement.start <= parsed.replacement.end);
                prop_assert!(parsed.replacement.end <= text.len());
            }
        }
    }
}
