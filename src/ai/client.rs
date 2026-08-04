use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{Client, RequestBuilder, StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::{
    ai::{
        code_assist, codex,
        oauth::{OAuthError, expires_soon, refresh_skew_secs, refresh_tokens_async},
        protocol::AiContext,
    },
    config::{
        AiAuth, AiConfig, CredentialError, OAuthTokens, ProviderCredential, load_api_key,
        read_credential, resolve_credential_path, write_credential,
    },
};

pub(crate) const RESPONSE_MAX_BYTES: usize = 128 * 1024;

/// Identical system prompt for every transport.
pub(crate) const SYSTEM_PROMPT: &str = "Return only JSON: {\"commands\":[{\"command\":\"...\",\"explanation\":\"...\"}]}. Provide 1-5 single-line shell commands. Never use Markdown.";

pub struct AiClient {
    pub(crate) client: Client,
    /// Fully normalized per transport (chat completions URL, Codex
    /// `/responses` URL, or the Code Assist base URL).
    pub(crate) endpoint: String,
    pub(crate) model: String,
    /// Code Assist project id resolved by `loadCodeAssist`, cached so it is
    /// resolved once per process.
    pub(crate) code_assist_project: Mutex<Option<String>>,
    config: AiConfig,
    default_credential_path: std::path::PathBuf,
    transport: Transport,
    /// Test hook overriding the production OAuth token endpoint used during
    /// credential refresh; always `None` in production.
    refresh_endpoint: Option<String>,
}

/// Inference protocol, chosen from `ai.provider`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Transport {
    ChatCompletions,
    CodexResponses,
    GeminiCodeAssist,
}

impl Transport {
    fn for_provider(provider: &str) -> Self {
        match provider {
            "openai-oauth" => Self::CodexResponses,
            "gemini-oauth" => Self::GeminiCodeAssist,
            _ => Self::ChatCompletions,
        }
    }
}

/// Bearer credential resolved for one request, plus the ChatGPT account id
/// the Codex transport needs (OAuth tokens first, `ai.account_id` fallback).
pub(crate) struct PreparedAuth {
    pub bearer: Zeroizing<String>,
    pub account_id: Option<String>,
}

impl AiClient {
    pub fn new(config: &AiConfig, default_credential_path: &Path) -> Result<Self, AiClientError> {
        let timeout = Duration::from_millis(config.timeout_ms);
        let client = Client::builder()
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .timeout(timeout)
            .redirect(Policy::none())
            .build()
            .map_err(|_| AiClientError::Configuration)?;
        let transport = Transport::for_provider(config.provider.trim());
        let endpoint = match transport {
            Transport::ChatCompletions => normalize_endpoint(&config.endpoint)?,
            Transport::CodexResponses => codex::normalize_endpoint(&config.endpoint)?,
            Transport::GeminiCodeAssist => code_assist::normalize_endpoint(&config.endpoint)?,
        };
        Ok(Self {
            client,
            endpoint,
            model: config.model.clone(),
            code_assist_project: Mutex::new(None),
            config: config.clone(),
            default_credential_path: default_credential_path.to_owned(),
            transport,
            refresh_endpoint: None,
        })
    }

    pub async fn request(
        &self,
        context: &AiContext,
        cancel: &CancellationToken,
    ) -> Result<Vec<crate::ai::AiCommand>, AiClientError> {
        if cancel.is_cancelled() {
            return Err(AiClientError::Cancelled);
        }
        let auth = self.prepare_credential(cancel).await?;
        let user_content = user_prompt(context);
        match self.transport {
            Transport::ChatCompletions => self.chat_completions(&auth, &user_content, cancel).await,
            Transport::CodexResponses => codex::request(self, &auth, &user_content, cancel).await,
            Transport::GeminiCodeAssist => {
                code_assist::request(self, &auth, &user_content, cancel).await
            }
        }
    }

    async fn chat_completions(
        &self,
        auth: &PreparedAuth,
        user_content: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<crate::ai::AiCommand>, AiClientError> {
        let request = ChatRequest {
            model: &self.model,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: SYSTEM_PROMPT,
                },
                ChatMessage {
                    role: "user",
                    content: user_content,
                },
            ],
            temperature: 0.1,
            max_tokens: 500,
        };
        let body = send_and_read(
            self.client
                .post(&self.endpoint)
                .bearer_auth(auth.bearer.as_str())
                .json(&request),
            cancel,
        )
        .await?;
        let response: ChatResponse =
            serde_json::from_slice(&body).map_err(|_| AiClientError::InvalidResponse)?;
        let content = response
            .choices
            .first()
            .map(|choice| choice.message.content.as_str())
            .ok_or(AiClientError::InvalidResponse)?;
        crate::ai::parse_ai_commands(content).map_err(|_| AiClientError::InvalidResponse)
    }

    /// Resolves the bearer credential for one request, dispatched on
    /// `ai.auth`. OAuth tokens are refreshed transparently when they expire
    /// within the provider's skew.
    async fn prepare_credential(
        &self,
        cancel: &CancellationToken,
    ) -> Result<PreparedAuth, AiClientError> {
        match self.config.auth {
            AiAuth::ApiKey => {
                let bearer = match load_api_key(&self.config, &self.default_credential_path) {
                    Ok(key) => key,
                    // Ollama ignores the bearer token but expects the header;
                    // a placeholder keeps key-less local setups working.
                    Err(CredentialError::Missing) if self.provider_slug() == "ollama" => {
                        Zeroizing::new("ollama".to_owned())
                    }
                    Err(CredentialError::Missing) => return Err(AiClientError::MissingCredential),
                    Err(_) => return Err(AiClientError::CredentialRejected),
                };
                Ok(PreparedAuth {
                    bearer,
                    account_id: self.config.account_id.clone(),
                })
            }
            AiAuth::OAuth => {
                let tokens = match read_credential(&self.credential_path(), self.provider_slug()) {
                    Ok(ProviderCredential::OAuth(tokens)) => tokens,
                    // An API key stored under an OAuth slug is a config /
                    // store mismatch; report it as a missing OAuth credential.
                    Ok(ProviderCredential::ApiKey(_)) | Err(CredentialError::Missing) => {
                        return Err(AiClientError::MissingCredential);
                    }
                    Err(_) => return Err(AiClientError::CredentialRejected),
                };
                let tokens = self.refresh_if_due(tokens, cancel).await?;
                Ok(PreparedAuth {
                    account_id: tokens
                        .account_id
                        .clone()
                        .or_else(|| self.config.account_id.clone()),
                    bearer: tokens.access_token,
                })
            }
        }
    }

    fn provider_slug(&self) -> &str {
        self.config.provider.trim()
    }

    /// Credential store path for OAuth reads and writes: the configured
    /// `ai.api_key_file` when set (matching `configured_credential_available`),
    /// else the default credentials file.
    fn credential_path(&self) -> PathBuf {
        resolve_credential_path(&self.config, &self.default_credential_path)
            .unwrap_or_else(|| self.default_credential_path.clone())
    }

    /// Refreshes `tokens` once when they expire within the provider's skew.
    /// The refresh runs on the caller's runtime via the async variant: the
    /// sync `refresh_tokens` builds its own runtime and would panic inside
    /// `request` ("Cannot start a runtime from within a runtime"). A
    /// transiently failed refresh keeps the stale tokens so the server
    /// answers 401 instead of failing locally; rotated tokens are persisted
    /// best-effort.
    async fn refresh_if_due(
        &self,
        tokens: OAuthTokens,
        cancel: &CancellationToken,
    ) -> Result<OAuthTokens, AiClientError> {
        if !expires_soon(
            &tokens,
            now_epoch_secs(),
            refresh_skew_secs(self.provider_slug()),
        ) {
            return Ok(tokens);
        }
        let Some(refreshed) = await_refresh(
            refresh_tokens_async(
                &self.client,
                self.provider_slug(),
                &tokens,
                self.refresh_endpoint.as_deref(),
            ),
            cancel,
        )
        .await
        else {
            return Err(AiClientError::Cancelled);
        };
        match refreshed {
            Ok(refreshed) => {
                // Best-effort persist: when the write fails (e.g. a read-only
                // config directory) the rotated tokens still authorize this
                // request and the next one simply refreshes again. AiClient
                // holds no debug-log channel, so the failure is deliberately
                // swallowed here; a lost rotation stays observable through
                // the repeated refresh traffic in `hokan doctor`.
                let _ = write_credential(
                    &self.credential_path(),
                    self.provider_slug(),
                    &ProviderCredential::OAuth(refreshed.clone()),
                );
                Ok(refreshed)
            }
            // The refresh token was revoked (`invalid_grant` arrives as a
            // 400): every following request would pay a doomed refresh round
            // trip, so fail fast and send the user back to `hokan ai setup`.
            Err(OAuthError::ServerRejected(400)) | Err(OAuthError::Denied) => {
                Err(AiClientError::CredentialRejected)
            }
            // Transient failure: keep the stale tokens so the server answers
            // 401 instead of failing locally.
            Err(_) => Ok(tokens),
        }
    }
}

/// Drives `refresh` to completion unless `cancel` fires first, returning
/// `None` when cancelled. The refresh branch is polled first so that, when
/// both complete in the same tick, the rotated tokens still win and can be
/// persisted: the server has already invalidated the old refresh token, and
/// dropping the result would silently sign the user out for good.
async fn await_refresh<F>(
    refresh: F,
    cancel: &CancellationToken,
) -> Option<Result<OAuthTokens, OAuthError>>
where
    F: std::future::Future<Output = Result<OAuthTokens, OAuthError>>,
{
    tokio::select! {
        biased;
        refreshed = refresh => Some(refreshed),
        () = cancel.cancelled() => None,
    }
}

/// Shared HTTP plumbing for every transport: send with cancellation, map the
/// status, then read the body in chunks under `RESPONSE_MAX_BYTES`.
pub(crate) async fn send_and_read(
    request: RequestBuilder,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, AiClientError> {
    let mut response = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(AiClientError::Cancelled),
        response = request.send() => response.map_err(map_reqwest_error)?,
    };
    let status = response.status();
    if !status.is_success() {
        return Err(status_error(status));
    }
    if response
        .content_length()
        .is_some_and(|length| length > RESPONSE_MAX_BYTES as u64)
    {
        return Err(AiClientError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(AiClientError::Cancelled),
            chunk = response.chunk() => chunk.map_err(map_reqwest_error)?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        if body.len().saturating_add(chunk.len()) > RESPONSE_MAX_BYTES {
            return Err(AiClientError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// URL hygiene shared by all endpoint normalizers: absolute http(s), no
/// credentials, query, or fragment.
pub(crate) fn validate_endpoint_url(url: &str) -> Result<(), AiClientError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| AiClientError::Configuration)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(AiClientError::Configuration);
    }
    Ok(())
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn map_reqwest_error(error: reqwest::Error) -> AiClientError {
    if error.is_timeout() {
        AiClientError::Timeout
    } else {
        AiClientError::Network
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    max_tokens: u32,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
}

fn user_prompt(context: &AiContext) -> String {
    format!(
        "request: {}\nos: {}\narchitecture: {}\nshell: {}\ncwd_basename: {}\nproject_kinds: {}",
        context.request,
        context.os,
        context.architecture,
        context.shell,
        context.cwd_basename.as_deref().unwrap_or("not-shared"),
        context.project_kinds.join(",")
    )
}

fn normalize_endpoint(endpoint: &str) -> Result<String, AiClientError> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    let url = if endpoint.ends_with("/chat/completions") {
        endpoint.to_owned()
    } else {
        format!("{endpoint}/chat/completions")
    };
    validate_endpoint_url(&url)?;
    Ok(url)
}

fn status_error(status: StatusCode) -> AiClientError {
    match status.as_u16() {
        401 | 403 => AiClientError::Unauthorized,
        429 => AiClientError::RateLimited,
        code => AiClientError::Http(code),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AiClientError {
    Configuration,
    MissingCredential,
    CredentialRejected,
    Unauthorized,
    RateLimited,
    Timeout,
    Network,
    ResponseTooLarge,
    InvalidResponse,
    Cancelled,
    CodeAssistProject,
    Http(u16),
}

impl std::fmt::Display for AiClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Configuration => "AI endpoint or client configuration is invalid",
            Self::MissingCredential => "AI credential environment variable is not set",
            Self::CredentialRejected => {
                "AI credential file was rejected; run `hokan config ai` for diagnostics"
            }
            Self::Unauthorized => "AI endpoint rejected the credential",
            Self::RateLimited => "AI endpoint rate limit was reached",
            Self::Timeout => "AI request timed out",
            Self::Network => "AI network request failed",
            Self::ResponseTooLarge => "AI response exceeded the size limit",
            Self::InvalidResponse => "AI response did not contain valid command JSON",
            Self::Cancelled => "AI request was cancelled",
            Self::CodeAssistProject => {
                "Gemini Code Assist did not return a Google Cloud project; enable Gemini Code Assist for your Google account and sign in again"
            }
            Self::Http(code) => return write!(formatter, "AI endpoint returned HTTP {code}"),
        };
        formatter.write_str(message)
    }
}

impl AiClientError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Configuration => "HK-AI-CFG",
            Self::MissingCredential => "HK-AI-CRED",
            Self::CredentialRejected => "HK-AI-CRED",
            Self::Unauthorized => "HK-AI-401",
            Self::RateLimited => "HK-AI-429",
            Self::Timeout => "HK-AI-TIMEOUT",
            Self::Network => "HK-AI-NET",
            Self::ResponseTooLarge => "HK-AI-SIZE",
            Self::InvalidResponse => "HK-AI-JSON",
            Self::Cancelled => "HK-AI-CANCEL",
            Self::CodeAssistProject => "HK-AI-PROJECT",
            Self::Http(_) => "HK-AI-HTTP",
        }
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod client_tests;
