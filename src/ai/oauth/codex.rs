//! OpenAI proprietary Codex device flow.

use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::{
    DEFAULT_POLL_INTERVAL, DEVICE_FLOW_TIMEOUT, DevicePrompt, MAX_POLL_INTERVAL, OAuthEndpoints,
    OAuthError, TokenResponse, block_on_flow, http_client, into_tokens, parse_json, send_form,
    send_json, status_error,
};
use crate::config::OAuthTokens;

const CODEX_USERCODE_MAX_RETRIES: u32 = 3;

pub(super) const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_DEVICE_URL: &str = "https://auth.openai.com/codex/device";
const CODEX_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";

/// OpenAI proprietary Codex device flow: request a user code, poll for the
/// server-issued authorization code and PKCE verifier, then exchange them.
pub fn run_codex_device_flow(
    sink: &mut dyn FnMut(DevicePrompt),
) -> Result<OAuthTokens, OAuthError> {
    let endpoints = OAuthEndpoints::codex();
    let client = http_client()?;
    block_on_flow(codex_device_flow(&client, &endpoints, sink))
}

async fn codex_device_flow(
    client: &Client,
    endpoints: &OAuthEndpoints,
    sink: &mut dyn FnMut(DevicePrompt),
) -> Result<OAuthTokens, OAuthError> {
    // Step a: request the user code, retrying rate limits briefly.
    let mut attempt = 0_u32;
    let user_code: CodexUserCodeResponse = loop {
        let reply = send_json(
            client,
            &endpoints.codex_user_code,
            &CodexUserCodeRequest {
                client_id: CODEX_CLIENT_ID,
            },
        )
        .await?;
        if reply.status == StatusCode::TOO_MANY_REQUESTS && attempt < CODEX_USERCODE_MAX_RETRIES {
            let wait = reply
                .retry_after
                .unwrap_or(1_u64 << attempt)
                .min(MAX_POLL_INTERVAL.as_secs());
            tokio::time::sleep(Duration::from_secs(wait)).await;
            attempt += 1;
            continue;
        }
        if !reply.status.is_success() {
            return Err(status_error(reply.status));
        }
        break parse_json(&reply.body)?;
    };
    sink(DevicePrompt {
        verification_uri: CODEX_DEVICE_URL.to_owned(),
        user_code: user_code.user_code.clone(),
    });

    // Step b: poll until the user authorizes; the SERVER issues the PKCE
    // verifier alongside the authorization code.
    let deadline = Instant::now() + DEVICE_FLOW_TIMEOUT;
    let interval = user_code
        .interval
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_POLL_INTERVAL)
        .min(MAX_POLL_INTERVAL);
    let grant: CodexDeviceTokenResponse = loop {
        if Instant::now() >= deadline {
            return Err(OAuthError::Expired);
        }
        tokio::time::sleep(interval).await;
        let reply = send_json(
            client,
            &endpoints.codex_device_token,
            &CodexDeviceTokenRequest {
                device_auth_id: &user_code.device_auth_id,
                user_code: &user_code.user_code,
            },
        )
        .await?;
        match reply.status {
            StatusCode::OK => break parse_json(&reply.body)?,
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => continue,
            status => return Err(status_error(status)),
        }
    };

    // Step c: exchange the authorization code with the server-issued verifier.
    let reply = send_form(
        client,
        &endpoints.token,
        &[
            ("grant_type", "authorization_code"),
            ("code", &grant.authorization_code),
            ("redirect_uri", CODEX_REDIRECT_URI),
            ("client_id", CODEX_CLIENT_ID),
            ("code_verifier", grant.code_verifier.as_str()),
        ],
    )
    .await?;
    if !reply.status.is_success() {
        return Err(status_error(reply.status));
    }
    let parsed: TokenResponse = parse_json(&reply.body)?;
    let account_id = parsed
        .id_token
        .as_deref()
        .and_then(|token| jwt_account_id(token.as_str()))
        .or_else(|| jwt_account_id(parsed.access_token.as_str()));
    into_tokens(parsed, account_id)
}

/// Reads `chatgpt_account_id` from a JWT payload WITHOUT verifying the
/// signature (the token came straight from the provider's TLS endpoint). The
/// claim sits either at the top level or under `https://api.openai.com/auth`.
fn jwt_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    json.get("chatgpt_account_id")
        .or_else(|| {
            json.get("https://api.openai.com/auth")?
                .get("chatgpt_account_id")
        })
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

#[derive(Serialize)]
struct CodexUserCodeRequest<'a> {
    client_id: &'a str,
}

#[derive(Deserialize)]
struct CodexUserCodeResponse {
    user_code: String,
    device_auth_id: String,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Serialize)]
struct CodexDeviceTokenRequest<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

#[derive(Deserialize)]
struct CodexDeviceTokenResponse {
    authorization_code: String,
    code_verifier: Zeroizing<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::oauth::test_support::*;

    #[test]
    fn codex_flow_three_steps_with_server_issued_verifier() {
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let poll_count = polls.clone();
        let (base, requests, join) =
            spawn_server(4, move |request| match request_path(request).as_str() {
                "/api/accounts/deviceauth/usercode" => json_reply(
                    "200 OK",
                    serde_json::json!({
                        "user_code": "WXYZ-1234",
                        "device_auth_id": "auth-id-1",
                        "interval": 0
                    }),
                ),
                "/api/accounts/deviceauth/token" => {
                    match poll_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => MockReply {
                            status: "403 Forbidden",
                            body: b"pending".to_vec(),
                            extra_headers: String::new(),
                        },
                        _ => json_reply(
                            "200 OK",
                            serde_json::json!({
                                "authorization_code": "codex-auth-code",
                                "code_verifier": "server-issued-verifier"
                            }),
                        ),
                    }
                }
                _ => json_reply(
                    "200 OK",
                    serde_json::json!({
                        "access_token": "codex-access",
                        "refresh_token": "codex-refresh",
                        "expires_in": 1800,
                        "id_token": unsigned_jwt(serde_json::json!({
                            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-nested-1"}
                        }))
                    }),
                ),
            });
        let endpoints = test_endpoints(&base);
        let client = http_client().expect("client");
        let mut prompts = Vec::new();
        let tokens = runtime()
            .block_on(codex_device_flow(&client, &endpoints, &mut |prompt| {
                prompts.push(prompt);
            }))
            .expect("codex flow");

        assert_eq!(prompts.len(), 1);
        assert_eq!(
            prompts[0].verification_uri,
            "https://auth.openai.com/codex/device"
        );
        assert_eq!(prompts[0].user_code, "WXYZ-1234");
        assert_eq!(tokens.access_token.as_str(), "codex-access");
        assert_eq!(tokens.refresh_token.as_str(), "codex-refresh");
        assert_eq!(tokens.account_id.as_deref(), Some("acct-nested-1"));

        let usercode =
            String::from_utf8(requests.recv().expect("usercode request")).expect("UTF-8");
        assert_eq!(
            request_path(usercode.as_bytes()),
            "/api/accounts/deviceauth/usercode"
        );
        assert!(
            request_body(usercode.as_bytes())
                .contains(r#""client_id":"app_EMoamEEZ73f0CkXaXp7hrann""#)
        );
        let poll = String::from_utf8(requests.recv().expect("poll request")).expect("UTF-8");
        let poll_body = request_body(poll.as_bytes());
        assert!(poll_body.contains(r#""device_auth_id":"auth-id-1""#));
        assert!(poll_body.contains(r#""user_code":"WXYZ-1234""#));
        let _pending_poll = requests.recv().expect("second poll request");
        let exchange =
            String::from_utf8(requests.recv().expect("exchange request")).expect("UTF-8");
        assert_eq!(request_path(exchange.as_bytes()), "/oauth/token");
        let exchange_body = request_body(exchange.as_bytes());
        assert!(exchange_body.contains("grant_type=authorization_code"));
        assert!(exchange_body.contains("code=codex-auth-code"));
        assert!(exchange_body.contains("code_verifier=server-issued-verifier"));
        assert!(
            exchange_body
                .contains("redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback")
        );
        assert!(exchange_body.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        join.join().expect("server");
    }

    #[test]
    fn codex_usercode_retries_429_then_surfaces_failure() {
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempt_count = attempts.clone();
        let (base, requests, join) = spawn_server(2, move |_| {
            if attempt_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                MockReply {
                    status: "429 Too Many Requests",
                    body: b"rate limited".to_vec(),
                    extra_headers: "Retry-After: 0\r\n".to_owned(),
                }
            } else {
                json_reply("500 Server Error", serde_json::json!({"error": "down"}))
            }
        });
        let endpoints = test_endpoints(&base);
        let client = http_client().expect("client");
        let error = runtime()
            .block_on(codex_device_flow(&client, &endpoints, &mut |_| {}))
            .expect_err("second attempt fails");
        assert_eq!(error, OAuthError::ServerRejected(500));
        for _ in 0..2 {
            let request =
                String::from_utf8(requests.recv().expect("usercode request")).expect("UTF-8");
            assert_eq!(
                request_path(request.as_bytes()),
                "/api/accounts/deviceauth/usercode"
            );
        }
        join.join().expect("server");
    }

    #[test]
    fn jwt_account_id_reads_direct_and_nested_claims() {
        let direct = unsigned_jwt(serde_json::json!({"chatgpt_account_id": "acct-direct-1"}));
        assert_eq!(jwt_account_id(&direct).as_deref(), Some("acct-direct-1"));
        let nested = unsigned_jwt(serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-nested-2"}
        }));
        assert_eq!(jwt_account_id(&nested).as_deref(), Some("acct-nested-2"));
        assert_eq!(jwt_account_id("not-a-jwt"), None);
        assert_eq!(
            jwt_account_id(&unsigned_jwt(serde_json::json!({"sub": "x"}))),
            None
        );
    }
}
