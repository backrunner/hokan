mod edit;
mod lexer;
mod quote;

pub use edit::{EditError, apply_edit};
pub(crate) use lexer::{
    EffectiveCommandKind, EffectiveCommandState, EnvironmentChange, command_query_option,
    effective_command_analysis, effective_command_analysis_for_shell,
    effective_command_index_for_shell, effective_command_state_for_shell,
    effective_external_command_state, semantic_word_tokens, wrapper_environment_changes_for_shell,
    wrapper_working_directories, wrapper_working_directories_for_shell,
};
pub use lexer::{ParsedLine, QuoteContext, Token, TokenKind, parse_line};
pub use quote::escape_for_shell;
