mod edit;
mod lexer;
mod quote;

pub use edit::{EditError, apply_edit};
pub(crate) use lexer::effective_command_index;
pub use lexer::{ParsedLine, QuoteContext, Token, TokenKind, parse_line};
pub use quote::escape_for_shell;
