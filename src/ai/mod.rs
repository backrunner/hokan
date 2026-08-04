mod client;
mod code_assist;
mod codex;
mod detector;
mod oauth;
mod protocol;
pub mod providers;
#[cfg(test)]
mod test_support;

pub use client::{AiClient, AiClientError};
pub use detector::{NaturalLanguageScore, detect_natural_language};
pub use oauth::{
    DevicePrompt, OAuthError, expires_soon, refresh_skew_secs, refresh_tokens,
    run_codex_device_flow, run_gemini_manual_flow, run_grok_device_flow,
};
pub use protocol::{AiCommand, AiContext, build_context, parse_ai_commands};
