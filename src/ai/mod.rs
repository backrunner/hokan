mod client;
mod detector;
mod protocol;

pub use client::{AiClient, AiClientError};
pub use detector::{NaturalLanguageScore, detect_natural_language};
pub use protocol::{AiCommand, AiContext, build_context, parse_ai_commands};
