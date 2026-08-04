//! OAuth sign-in flows for the OAuth-capable providers (`openai-oauth`,
//! `gemini-oauth`, `grok-oauth`).
//!
//! The setup wizard is synchronous CLI code, so every public entry point
//! drives its future on a private current-thread runtime (same pattern as the
//! `hokan-ai` thread in `app::runtime`). Endpoint URLs live in
//! [`OAuthEndpoints`] so tests can point every flow at a loopback mock server;
//! production entry points use the provider constants verified from
//! hermes-agent / opencode / gemini-cli.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Client, StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::config::OAuthTokens;

mod codex;
mod gemini;
mod grok;
mod refresh;
#[cfg(test)]
mod test_support;

pub use codex::run_codex_device_flow;
pub use gemini::run_gemini_manual_flow;
pub use grok::run_grok_device_flow;
pub(crate) use refresh::refresh_tokens_async;
pub use refresh::{expires_soon, refresh_skew_secs, refresh_tokens};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on OAuth endpoint response bodies; real device-code, token, and
/// refresh replies are a few hundred bytes of JSON.
const RESPONSE_BODY_MAX_BYTES: usize = 64 * 1024;
/// Overall cap for interactive device sign-in; matches the server-side device
/// code lifetime of both device-flow providers.
const DEVICE_FLOW_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Fallback token lifetime when the server omits `expires_in`.
const DEFAULT_EXPIRES_IN: u64 = 3600;

/// What the user must do to authorize a device sign-in; the wizard renders it.
pub struct DevicePrompt {
    /// URL the user opens; the complete (code-embedded) URL when offered.
    pub verification_uri: String,
    /// Code the user types at the verification page.
    pub user_code: String,
}

/// Endpoint set for one provider. Production constructors hold the verified
/// constants; tests construct loopback URLs directly.
struct OAuthEndpoints {
    /// RFC 8628 device authorization endpoint (xAI).
    device_code: String,
    /// OpenAI proprietary user-code endpoint (Codex step a).
    codex_user_code: String,
    /// OpenAI proprietary device-auth poll endpoint (Codex step b).
    codex_device_token: String,
    /// Token endpoint shared by exchange and refresh (all providers).
    token: String,
    /// Authorize endpoint for the manual-code flow (Gemini).
    authorize: String,
}

impl OAuthEndpoints {
    fn grok() -> Self {
        Self {
            device_code: "https://auth.x.ai/oauth2/device/code".to_owned(),
            codex_user_code: String::new(),
            codex_device_token: String::new(),
            token: "https://auth.x.ai/oauth2/token".to_owned(),
            authorize: String::new(),
        }
    }

    fn codex() -> Self {
        Self {
            device_code: String::new(),
            codex_user_code: "https://auth.openai.com/api/accounts/deviceauth/usercode".to_owned(),
            codex_device_token: "https://auth.openai.com/api/accounts/deviceauth/token".to_owned(),
            token: "https://auth.openai.com/oauth/token".to_owned(),
            authorize: String::new(),
        }
    }

    fn gemini() -> Self {
        Self {
            device_code: String::new(),
            codex_user_code: String::new(),
            codex_device_token: String::new(),
            token: "https://oauth2.googleapis.com/token".to_owned(),
            authorize: "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
        }
    }

    fn for_slug(slug: &str) -> Option<Self> {
        match slug {
            "grok-oauth" => Some(Self::grok()),
            "openai-oauth" => Some(Self::codex()),
            "gemini-oauth" => Some(Self::gemini()),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OAuthError {
    Cancelled,
    Expired,
    Denied,
    Network,
    Timeout,
    InvalidResponse,
    ServerRejected(u16),
    TooManyRequests,
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Cancelled => "OAuth sign-in was cancelled",
            Self::Expired => "OAuth device code expired before sign-in completed",
            Self::Denied => "OAuth access was denied by the user or server",
            Self::Network => "OAuth network request failed",
            Self::Timeout => "OAuth request timed out",
            Self::InvalidResponse => "OAuth server response was invalid",
            Self::TooManyRequests => "OAuth server rate limit was reached",
            Self::ServerRejected(code) => {
                return write!(formatter, "OAuth server rejected the request (HTTP {code})");
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for OAuthError {}

impl OAuthError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Cancelled => "HK-AUTH-CANCEL",
            Self::Expired => "HK-AUTH-EXPIRED",
            Self::Denied => "HK-AUTH-DENIED",
            Self::Network => "HK-AUTH-NET",
            Self::Timeout => "HK-AUTH-TIMEOUT",
            Self::InvalidResponse => "HK-AUTH-JSON",
            Self::ServerRejected(_) => "HK-AUTH-HTTP",
            Self::TooManyRequests => "HK-AUTH-429",
        }
    }
}

fn block_on_flow<F>(future: F) -> Result<OAuthTokens, OAuthError>
where
    F: std::future::Future<Output = Result<OAuthTokens, OAuthError>>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| OAuthError::Network)?
        .block_on(future)
}

fn http_client() -> Result<Client, OAuthError> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        // Never follow redirects while carrying credentials.
        .redirect(Policy::none())
        .build()
        .map_err(|_| OAuthError::Network)
}

struct HttpReply {
    status: StatusCode,
    retry_after: Option<u64>,
    body: Zeroizing<String>,
}

async fn send_form(
    client: &Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<HttpReply, OAuthError> {
    send(client.post(url).form(form)).await
}

async fn send_json<T: Serialize + ?Sized>(
    client: &Client,
    url: &str,
    body: &T,
) -> Result<HttpReply, OAuthError> {
    send(client.post(url).json(body)).await
}

async fn send(request: reqwest::RequestBuilder) -> Result<HttpReply, OAuthError> {
    let mut response = request.send().await.map_err(map_reqwest_error)?;
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if response
        .content_length()
        .is_some_and(|length| length > RESPONSE_BODY_MAX_BYTES as u64)
    {
        return Err(OAuthError::InvalidResponse);
    }
    // Read in chunks under the cap: a malicious or broken server must not be
    // able to make the client buffer an unbounded body.
    let mut body = Vec::new();
    loop {
        let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? else {
            break;
        };
        if body.len().saturating_add(chunk.len()) > RESPONSE_BODY_MAX_BYTES {
            return Err(OAuthError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    let body = Zeroizing::new(String::from_utf8(body).map_err(|_| OAuthError::InvalidResponse)?);
    Ok(HttpReply {
        status,
        retry_after,
        body,
    })
}

fn parse_json<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, OAuthError> {
    serde_json::from_str(body).map_err(|_| OAuthError::InvalidResponse)
}

fn map_reqwest_error(error: reqwest::Error) -> OAuthError {
    if error.is_timeout() {
        OAuthError::Timeout
    } else {
        OAuthError::Network
    }
}

fn status_error(status: StatusCode) -> OAuthError {
    match status.as_u16() {
        429 => OAuthError::TooManyRequests,
        code => OAuthError::ServerRejected(code),
    }
}

fn into_tokens(
    parsed: TokenResponse,
    account_id: Option<String>,
) -> Result<OAuthTokens, OAuthError> {
    Ok(OAuthTokens {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token.ok_or(OAuthError::InvalidResponse)?,
        expires_at: expiry_from_now(parsed.expires_in),
        account_id,
    })
}

fn expiry_from_now(expires_in: Option<u64>) -> u64 {
    now_epoch_secs().saturating_add(expires_in.unwrap_or(DEFAULT_EXPIRES_IN))
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Token endpoint payload; token fields stay zeroized for their whole life.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: Zeroizing<String>,
    #[serde(default)]
    refresh_token: Option<Zeroizing<String>>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    id_token: Option<Zeroizing<String>>,
}
