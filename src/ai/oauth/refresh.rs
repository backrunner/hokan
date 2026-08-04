//! Token refresh shared by the OAuth providers.

use reqwest::Client;
use zeroize::Zeroizing;

use super::{
    OAuthEndpoints, OAuthError, TokenResponse, block_on_flow,
    codex::CODEX_CLIENT_ID,
    expiry_from_now,
    gemini::{GEMINI_CLIENT_ID, GEMINI_CLIENT_SECRET},
    grok::GROK_CLIENT_ID,
    http_client, parse_json, send_form, status_error,
};
use crate::config::OAuthTokens;

/// Runs the refresh_token grant for an OAuth provider. `account_id` is always
/// preserved; when the response omits `refresh_token` the old one is kept
/// (rotating vs static refresh tokens differ per provider).
pub fn refresh_tokens(slug: &str, tokens: &OAuthTokens) -> Result<OAuthTokens, OAuthError> {
    let client = http_client()?;
    block_on_flow(refresh_tokens_async(&client, slug, tokens, None))
}

/// Async variant of [`refresh_tokens`] for callers already inside a tokio
/// runtime (`AiClient::request`); the sync entry point builds its own runtime
/// and would panic there. `token_url_override` points tests at a loopback
/// mock; production callers pass `None`.
pub(crate) async fn refresh_tokens_async(
    client: &Client,
    slug: &str,
    tokens: &OAuthTokens,
    token_url_override: Option<&str>,
) -> Result<OAuthTokens, OAuthError> {
    let mut endpoints = OAuthEndpoints::for_slug(slug).ok_or(OAuthError::InvalidResponse)?;
    if let Some(url) = token_url_override {
        endpoints.token = url.to_owned();
    }
    refresh_with(client, &endpoints, slug, tokens).await
}

/// How long before `expires_at` each provider's token should be refreshed:
/// OpenAI 120s, xAI 300s, Gemini 300s. The skew must stay well below the
/// one-hour `expires_in` fallback (xAI's real TTL is ~6h, so 300s is ample):
/// a skew >= the fallback would mark every refreshed token as already
/// expiring and prepend a refresh round trip to every request.
#[must_use]
pub fn refresh_skew_secs(slug: &str) -> u64 {
    match slug {
        "openai-oauth" => 120,
        _ => 300,
    }
}

#[must_use]
pub fn expires_soon(tokens: &OAuthTokens, now_epoch_secs: u64, skew: u64) -> bool {
    tokens.expires_at <= now_epoch_secs.saturating_add(skew)
}

async fn refresh_with(
    client: &Client,
    endpoints: &OAuthEndpoints,
    slug: &str,
    tokens: &OAuthTokens,
) -> Result<OAuthTokens, OAuthError> {
    let client_id = match slug {
        "grok-oauth" => GROK_CLIENT_ID,
        "openai-oauth" => CODEX_CLIENT_ID,
        _ => GEMINI_CLIENT_ID,
    };
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", tokens.refresh_token.as_str()),
        ("client_id", client_id),
    ];
    if slug == "gemini-oauth" {
        form.push(("client_secret", GEMINI_CLIENT_SECRET));
    }
    let reply = send_form(client, &endpoints.token, &form).await?;
    if !reply.status.is_success() {
        return Err(status_error(reply.status));
    }
    let parsed: TokenResponse = parse_json(&reply.body)?;
    Ok(OAuthTokens {
        access_token: parsed.access_token,
        // Static-refresh providers omit the field; keep the old token.
        refresh_token: parsed
            .refresh_token
            .unwrap_or_else(|| Zeroizing::new(tokens.refresh_token.as_str().to_owned())),
        expires_at: expiry_from_now(parsed.expires_in),
        account_id: tokens.account_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::oauth::{DEFAULT_EXPIRES_IN, now_epoch_secs, test_support::*};

    #[test]
    fn refresh_rotates_and_preserves_refresh_token_per_provider() {
        use super::super::gemini::{GEMINI_CLIENT_ID, GEMINI_CLIENT_SECRET};

        for (slug, client_id, expect_secret) in [
            ("openai-oauth", "app_EMoamEEZ73f0CkXaXp7hrann", false),
            ("grok-oauth", "b1a00492-073a-47ea-816f-4c329264a828", false),
            ("gemini-oauth", GEMINI_CLIENT_ID, true),
        ] {
            // Rotating: the server issues a new refresh token.
            let (base, requests, join) = spawn_server(1, |_| {
                json_reply(
                    "200 OK",
                    serde_json::json!({
                        "access_token": "new-access",
                        "refresh_token": "new-refresh",
                        "expires_in": 120
                    }),
                )
            });
            let endpoints = test_endpoints(&base);
            let client = http_client().expect("client");
            let before = now_epoch_secs();
            let refreshed = runtime()
                .block_on(refresh_with(
                    &client,
                    &endpoints,
                    slug,
                    &sample_tokens("old-refresh"),
                ))
                .expect("refresh");
            assert_eq!(refreshed.access_token.as_str(), "new-access");
            assert_eq!(refreshed.refresh_token.as_str(), "new-refresh");
            assert!(refreshed.expires_at >= before + 120);
            assert_eq!(refreshed.account_id.as_deref(), Some("acct-keep"));
            let body = request_body(&requests.recv().expect("refresh request"));
            assert!(body.contains("grant_type=refresh_token"));
            assert!(body.contains("refresh_token=old-refresh"));
            assert!(body.contains(&format!("client_id={client_id}")));
            assert_eq!(
                body.contains(&format!("client_secret={GEMINI_CLIENT_SECRET}")),
                expect_secret
            );
            join.join().expect("server");

            // Static: response omits refresh_token, the old one is kept, and a
            // missing expires_in falls back to one hour.
            let (base, _requests, join) = spawn_server(1, |_| {
                json_reply(
                    "200 OK",
                    serde_json::json!({"access_token": "new-access-2"}),
                )
            });
            let endpoints = test_endpoints(&base);
            let before = now_epoch_secs();
            let refreshed = runtime()
                .block_on(refresh_with(
                    &client,
                    &endpoints,
                    slug,
                    &sample_tokens("old-refresh"),
                ))
                .expect("refresh without rotation");
            assert_eq!(refreshed.access_token.as_str(), "new-access-2");
            assert_eq!(refreshed.refresh_token.as_str(), "old-refresh");
            assert!(refreshed.expires_at >= before + 3600);
            join.join().expect("server");
        }
    }

    #[test]
    fn refresh_errors_on_failure_status_and_unknown_slug() {
        let (base, _requests, join) = spawn_server(1, |_| {
            json_reply(
                "400 Bad Request",
                serde_json::json!({"error": "invalid_grant"}),
            )
        });
        let endpoints = test_endpoints(&base);
        let client = http_client().expect("client");
        let error = runtime()
            .block_on(refresh_with(
                &client,
                &endpoints,
                "grok-oauth",
                &sample_tokens("r"),
            ))
            .expect_err("400 must fail");
        assert_eq!(error, OAuthError::ServerRejected(400));
        join.join().expect("server");

        assert_eq!(
            refresh_tokens("deepseek", &sample_tokens("r")).expect_err("non-OAuth slug"),
            OAuthError::InvalidResponse
        );
    }

    #[test]
    fn oversize_token_response_is_rejected() {
        // A valid-JSON body over the 64 KiB cap must be rejected instead of
        // being buffered unboundedly (before the cap this refresh succeeded).
        let (base, _requests, join) = spawn_server(1, |_| {
            json_reply(
                "200 OK",
                serde_json::json!({
                    "access_token": "a".repeat(70 * 1024),
                    "refresh_token": "new-refresh",
                    "expires_in": 3600
                }),
            )
        });
        let endpoints = test_endpoints(&base);
        let client = http_client().expect("client");
        let error = runtime()
            .block_on(refresh_with(
                &client,
                &endpoints,
                "grok-oauth",
                &sample_tokens("r"),
            ))
            .expect_err("oversize body must fail");
        assert_eq!(error, OAuthError::InvalidResponse);
        join.join().expect("server");
    }

    #[test]
    fn expires_soon_boundary_and_skews() {
        let tokens = OAuthTokens {
            access_token: Zeroizing::new("a".to_owned()),
            refresh_token: Zeroizing::new("r".to_owned()),
            expires_at: 1_000,
            account_id: None,
        };
        assert!(
            expires_soon(&tokens, 880, 120),
            "now + skew == expires_at is soon"
        );
        assert!(!expires_soon(&tokens, 879, 120));
        assert!(expires_soon(&tokens, 1_001, 0), "already expired is soon");
        assert!(!expires_soon(&tokens, 999, 0));

        assert_eq!(refresh_skew_secs("openai-oauth"), 120);
        assert_eq!(refresh_skew_secs("grok-oauth"), 300);
        assert_eq!(refresh_skew_secs("gemini-oauth"), 300);
    }

    #[test]
    fn grok_default_expiry_is_not_immediately_due_for_refresh() {
        // When the server omits expires_in the fallback is one hour; with the
        // grok skew the refreshed token must NOT already count as expiring,
        // or every request would prepend another refresh round trip.
        let now = now_epoch_secs();
        let tokens = OAuthTokens {
            access_token: Zeroizing::new("a".to_owned()),
            refresh_token: Zeroizing::new("r".to_owned()),
            expires_at: now + DEFAULT_EXPIRES_IN,
            account_id: None,
        };
        assert!(!expires_soon(&tokens, now, refresh_skew_secs("grok-oauth")));
    }
}
