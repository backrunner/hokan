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
mod ssh;

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
pub use ssh::SshHostProvider;

use crate::{completion::CompletionContext, parser::TokenKind};

/// Cursor progress past the effective command token: the cooked words of the
/// active segment from the effective command up to the cursor, plus the
/// zero-based index of the argument being completed (0 = first argument after
/// the effective command). Leading assignments and wrapper words (`sudo`,
/// `env`, …) are skipped, so `sudo git checkout ` measures from `git`.
/// `None` while the cursor is still on the effective command token itself.
/// Shared by the filesystem and command-help providers so both agree on what
/// "first argument" means.
pub(crate) fn argument_progress(context: &CompletionContext) -> Option<(Vec<&str>, usize)> {
    let word_tokens = segment_word_tokens(context);
    let cooked: Vec<&str> = word_tokens
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let command_index = crate::parser::effective_command_index(&cooked)?;
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
    if context.parsed.current_prefix.contains('/') {
        return false;
    }
    let word_tokens = segment_word_tokens(context);
    let cooked: Vec<&str> = word_tokens
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    match crate::parser::effective_command_index(&cooked) {
        None => true,
        Some(index) => {
            let token = word_tokens[index];
            context.buffer.cursor >= token.range.start && context.buffer.cursor <= token.range.end
        }
    }
}

/// Word tokens of the active pipeline segment up to the cursor.
fn segment_word_tokens(context: &CompletionContext) -> Vec<&crate::parser::Token> {
    context
        .parsed
        .tokens
        .iter()
        .filter(|token| {
            token.kind == TokenKind::Word
                && token.range.start >= context.parsed.active_segment.start
                && token.range.start <= context.buffer.cursor
        })
        .collect()
}

/// Cooked words of the active pipeline segment up to the cursor.
pub(crate) fn segment_words(context: &CompletionContext) -> Vec<&str> {
    segment_word_tokens(context)
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect()
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
];

pub(crate) const MANAGERS: &[ManagerSpec] = &[
    ManagerSpec {
        name: "pnpm",
        keyword: None,
        subcommands: &[],
    },
    ManagerSpec {
        name: "yarn",
        keyword: None,
        subcommands: &[],
    },
    ManagerSpec {
        name: "bun",
        keyword: None,
        subcommands: &[],
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
    /// Bare prefix after the manager (or the manager word itself).
    Bare { on_manager_word: bool },
}

pub(crate) fn manager_position(
    context: &CompletionContext,
) -> Option<(&'static ManagerSpec, Position)> {
    let words = segment_words(context);
    let spec = MANAGERS
        .iter()
        .find(|spec| Some(spec.name) == words.first().copied())?;
    let trailing_space = context.buffer.text[..context.buffer.cursor]
        .chars()
        .next_back()
        .is_some_and(char::is_whitespace);
    match words.as_slice() {
        [_, second, ..] if Some(*second) == spec.keyword && (trailing_space || words.len() > 2) => {
            Some((spec, Position::ScriptToken))
        }
        // pnpm/yarn/bun also accept the explicit `run` form.
        [_, "run", ..] if spec.keyword.is_none() && (trailing_space || words.len() > 2) => {
            Some((spec, Position::ScriptToken))
        }
        [_, second] if Some(*second) == spec.keyword => Some((spec, Position::KeywordWord)),
        [_, "run"] if spec.keyword.is_none() => Some((spec, Position::KeywordWord)),
        [_] if trailing_space => Some((
            spec,
            Position::Bare {
                on_manager_word: false,
            },
        )),
        [_] => Some((
            spec,
            Position::Bare {
                on_manager_word: true,
            },
        )),
        [_, _] => Some((
            spec,
            Position::Bare {
                on_manager_word: false,
            },
        )),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FilterPosition {
    /// `pnpm --filter <prefix>`: complete the member name.
    Value,
    /// `pnpm --filter <member> <prefix>` (or `… run <prefix>`): the member's
    /// own scripts.
    MemberScripts { member: String, keyword: bool },
}

/// pnpm workspace `--filter` positions. Only pnpm for now — npm/yarn use
/// different workspace flags (`--workspace`, `yarn workspace`).
pub(crate) fn filter_position(context: &CompletionContext) -> Option<FilterPosition> {
    let words = segment_words(context);
    if words.first().copied() != Some("pnpm") || words.get(1).copied() != Some("--filter") {
        return None;
    }
    let trailing_space = context.buffer.text[..context.buffer.cursor]
        .chars()
        .next_back()
        .is_some_and(char::is_whitespace);
    let member = |keyword: bool| {
        Some(FilterPosition::MemberScripts {
            member: words[2].to_owned(),
            keyword,
        })
    };
    match words.len() {
        // `pnpm --filter ` or the member name being typed.
        2 if trailing_space => Some(FilterPosition::Value),
        3 if !trailing_space => Some(FilterPosition::Value),
        // After the member: its scripts; `run` keeps the keyword form.
        3 if trailing_space => member(false),
        4 if !trailing_space => member(words[3] == "run"),
        _ if words.len() > 4 => member(false),
        _ => None,
    }
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
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            BufferSnapshot::new(text, cursor, BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context")
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

        // Still on the effective command word: no progress yet.
        assert!(argument_progress(&context("sudo vim", 8)).is_none());
        // Only a wrapper so far: no effective command, no progress.
        assert!(argument_progress(&context("sudo ", 5)).is_none());
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
        // Argument positions: command-name completion must not fire.
        assert!(!open("ls "));
        assert!(!open("sudo ls "));
        assert!(!open("FOO=bar ls "));
        assert!(!open("git checkout "));
        // A path prefix is never a command name.
        assert!(!open("./l"));

        // Cursor mid-word on the effective command still counts.
        assert!(command_position_open(&context("sudo vim", 7)));
        // At the end of the command word it is open; inside an argument it is
        // not.
        assert!(command_position_open(&context("ls -la", 2)));
        assert!(!command_position_open(&context("ls -la", 4)));
    }
}
