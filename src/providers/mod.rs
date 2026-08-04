mod ai_action;
mod command_help;
mod command_spec;
mod filesystem;
mod history;
mod network_interface;
mod path_command;
mod process;
mod project;

pub use ai_action::{AiActionProvider, ai_error_candidate, ai_result_candidates};
pub use command_help::{CommandHelpCache, CommandHelpProvider};
pub use command_spec::CommandSpecProvider;
pub use filesystem::FilesystemProvider;
pub use history::HistoryProvider;
pub use network_interface::NetworkInterfaceProvider;
pub use path_command::PathCommandProvider;
pub use process::ProcessProvider;
pub use project::ProjectProvider;

use crate::{completion::CompletionContext, parser::TokenKind};

/// Cursor progress past the command token: the cooked words of the active
/// segment up to the cursor, plus the zero-based index of the argument being
/// completed (0 = first argument after the command). `None` while the cursor
/// is still on the command token itself. Shared by the filesystem and
/// command-help providers so both agree on what "first argument" means.
pub(crate) fn argument_progress(context: &CompletionContext) -> Option<(Vec<&str>, usize)> {
    let command_token = context.parsed.tokens.iter().find(|token| {
        token.kind == TokenKind::Word && token.range.start >= context.parsed.active_segment.start
    })?;
    if context.buffer.cursor <= command_token.range.end
        && !context.buffer.text[..context.buffer.cursor].ends_with(char::is_whitespace)
    {
        return None;
    }
    let words: Vec<_> = context
        .parsed
        .tokens
        .iter()
        .filter(|token| {
            token.kind == TokenKind::Word
                && token.range.start >= context.parsed.active_segment.start
                && token.range.start <= context.buffer.cursor
        })
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let trailing_space = context.buffer.text[..context.buffer.cursor]
        .chars()
        .next_back()
        .is_some_and(char::is_whitespace);
    let position = if trailing_space {
        words.len().saturating_sub(1)
    } else {
        words.len().saturating_sub(2)
    };
    Some((words, position))
}
