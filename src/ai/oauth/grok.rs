//! xAI standard RFC 8628 device flow.

use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode};
use serde::Deserialize;

use super::{
    DEFAULT_EXPIRES_IN, DEFAULT_POLL_INTERVAL, DEVICE_FLOW_TIMEOUT, DevicePrompt,
    MAX_POLL_INTERVAL, OAuthEndpoints, OAuthError, TokenResponse, block_on_flow, http_client,
    into_tokens, parse_json, send_form, status_error,
};
use crate::config::OAuthTokens;

const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

pub(super) const GROK_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const GROK_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const GROK_DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// xAI standard RFC 8628 device flow: request a device code, tell the user
/// where to authorize, then poll until granted, denied, or expired.
pub fn run_grok_device_flow(sink: &mut dyn FnMut(DevicePrompt)) -> Result<OAuthTokens, OAuthError> {
    let endpoints = OAuthEndpoints::grok();
    let client = http_client()?;
    block_on_flow(grok_device_flow(&client, &endpoints, sink))
}

async fn grok_device_flow(
    client: &Client,
    endpoints: &OAuthEndpoints,
    sink: &mut dyn FnMut(DevicePrompt),
) -> Result<OAuthTokens, OAuthError> {
    let reply = send_form(
        client,
        &endpoints.device_code,
        &[("client_id", GROK_CLIENT_ID), ("scope", GROK_SCOPE)],
    )
    .await?;
    if !reply.status.is_success() {
        return Err(status_error(reply.status));
    }
    let device: DeviceCodeResponse = parse_json(&reply.body)?;
    sink(DevicePrompt {
        verification_uri: device
            .verification_uri_complete
            .unwrap_or(device.verification_uri),
        user_code: device.user_code,
    });

    let deadline = Instant::now()
        + Duration::from_secs(device.expires_in.unwrap_or(DEFAULT_EXPIRES_IN))
            .min(DEVICE_FLOW_TIMEOUT);
    let mut interval = device
        .interval
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_POLL_INTERVAL)
        .min(MAX_POLL_INTERVAL);
    loop {
        if Instant::now() >= deadline {
            return Err(OAuthError::Expired);
        }
        tokio::time::sleep(interval).await;
        let reply = send_form(
            client,
            &endpoints.token,
            &[
                ("grant_type", GROK_DEVICE_GRANT),
                ("device_code", &device.device_code),
                ("client_id", GROK_CLIENT_ID),
            ],
        )
        .await?;
        if reply.status.is_success() {
            let parsed: TokenResponse = parse_json(&reply.body)?;
            return into_tokens(parsed, None);
        }
        if reply.status == StatusCode::BAD_REQUEST
            && let Ok(error) = serde_json::from_str::<DevicePollError>(&reply.body)
        {
            match error.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    interval = next_interval(interval);
                    continue;
                }
                "expired_token" => return Err(OAuthError::Expired),
                "access_denied" => return Err(OAuthError::Denied),
                _ => return Err(OAuthError::ServerRejected(reply.status.as_u16())),
            }
        }
        return Err(status_error(reply.status));
    }
}

fn next_interval(current: Duration) -> Duration {
    current
        .saturating_add(SLOW_DOWN_STEP)
        .min(MAX_POLL_INTERVAL)
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct DevicePollError {
    error: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::oauth::{now_epoch_secs, test_support::*};

    #[test]
    fn grok_flow_pending_twice_then_success() {
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let poll_count = polls.clone();
        let (base, requests, join) = spawn_server(4, move |request| {
            if request_path(request) == "/oauth2/device/code" {
                grok_device_code_reply()
            } else {
                match poll_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 | 1 => json_reply(
                        "400 Bad Request",
                        serde_json::json!({"error": "authorization_pending"}),
                    ),
                    _ => grok_token_reply(),
                }
            }
        });
        let endpoints = test_endpoints(&base);
        let client = http_client().expect("client");
        let mut prompts = Vec::new();
        let before = now_epoch_secs();
        let tokens = runtime()
            .block_on(grok_device_flow(&client, &endpoints, &mut |prompt| {
                prompts.push(prompt);
            }))
            .expect("device flow");
        let after = now_epoch_secs();

        assert_eq!(prompts.len(), 1);
        // The complete URL embedding the code is preferred.
        assert_eq!(
            prompts[0].verification_uri,
            "https://x.ai/device?code=ABCD-EFGH"
        );
        assert_eq!(prompts[0].user_code, "ABCD-EFGH");
        assert_eq!(tokens.access_token.as_str(), "grok-access");
        assert_eq!(tokens.refresh_token.as_str(), "grok-refresh");
        assert!(tokens.expires_at >= before + 3600 && tokens.expires_at <= after + 3600);
        assert_eq!(tokens.account_id, None);

        let device_request =
            String::from_utf8(requests.recv().expect("device request")).expect("UTF-8");
        assert!(request_path(device_request.as_bytes()) == "/oauth2/device/code");
        assert!(
            request_body(device_request.as_bytes())
                .contains("client_id=b1a00492-073a-47ea-816f-4c329264a828")
        );
        let poll_request =
            String::from_utf8(requests.recv().expect("poll request")).expect("UTF-8");
        let poll_body = request_body(poll_request.as_bytes());
        assert!(
            poll_body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
        );
        assert!(poll_body.contains("device_code=device-code-1"));
        join.join().expect("server");
    }

    #[test]
    fn grok_flow_slow_down_then_success() {
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let poll_count = polls.clone();
        let (base, _requests, join) = spawn_server(3, move |request| {
            if request_path(request) == "/oauth2/device/code" {
                grok_device_code_reply()
            } else {
                match poll_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => json_reply("400 Bad Request", serde_json::json!({"error": "slow_down"})),
                    _ => grok_token_reply(),
                }
            }
        });
        let endpoints = test_endpoints(&base);
        let client = http_client().expect("client");
        let tokens = runtime()
            .block_on(grok_device_flow(&client, &endpoints, &mut |_| {}))
            .expect("device flow");
        assert_eq!(tokens.access_token.as_str(), "grok-access");
        join.join().expect("server");
    }

    #[test]
    fn slow_down_grows_interval_by_five_seconds_capped_at_thirty() {
        assert_eq!(
            next_interval(Duration::from_secs(0)),
            Duration::from_secs(5)
        );
        assert_eq!(
            next_interval(Duration::from_secs(5)),
            Duration::from_secs(10)
        );
        assert_eq!(
            next_interval(Duration::from_secs(28)),
            Duration::from_secs(30)
        );
        assert_eq!(
            next_interval(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn grok_flow_clamps_server_controlled_expires_in() {
        // A huge server-controlled expires_in must be clamped before the
        // Instant arithmetic instead of overflowing and panicking.
        let (base, _requests, join) = spawn_server(2, move |request| {
            if request_path(request) == "/oauth2/device/code" {
                json_reply(
                    "200 OK",
                    serde_json::json!({
                        "device_code": "device-code-1",
                        "user_code": "ABCD-EFGH",
                        "verification_uri": "https://x.ai/device",
                        "expires_in": u64::MAX,
                        "interval": 0
                    }),
                )
            } else {
                json_reply(
                    "400 Bad Request",
                    serde_json::json!({"error": "expired_token"}),
                )
            }
        });
        let endpoints = test_endpoints(&base);
        let client = http_client().expect("client");
        let error = runtime()
            .block_on(grok_device_flow(&client, &endpoints, &mut |_| {}))
            .expect_err("expired_token poll must fail");
        assert_eq!(error, OAuthError::Expired);
        join.join().expect("server");
    }

    #[test]
    fn grok_flow_terminal_errors() {
        for (error_code, expected) in [
            ("expired_token", OAuthError::Expired),
            ("access_denied", OAuthError::Denied),
            ("server_error", OAuthError::ServerRejected(400)),
        ] {
            let (base, _requests, join) = spawn_server(2, move |request| {
                if request_path(request) == "/oauth2/device/code" {
                    grok_device_code_reply()
                } else {
                    // Body carries attacker-controlled noise; it must never
                    // leak into the returned error message.
                    json_reply(
                        "400 Bad Request",
                        serde_json::json!({"error": error_code, "error_description": "grok-access zzz"}),
                    )
                }
            });
            let endpoints = test_endpoints(&base);
            let client = http_client().expect("client");
            let error = runtime()
                .block_on(grok_device_flow(&client, &endpoints, &mut |_| {}))
                .expect_err("terminal poll error");
            assert_eq!(error, expected);
            let rendered = format!("{error}");
            assert!(!rendered.contains("zzz"));
            join.join().expect("server");
        }
    }
}
