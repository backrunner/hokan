//! Codex Responses transport (`openai-oauth`): OpenAI's Responses API behind
//! `chatgpt.com/backend-api/codex`, carrying the ChatGPT account id header.

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::ai::client::{
    AiClient, AiClientError, PreparedAuth, SYSTEM_PROMPT, send_and_read, validate_endpoint_url,
};

/// `{endpoint}/responses`, keeping an already-suffixed endpoint as-is.
pub(crate) fn normalize_endpoint(endpoint: &str) -> Result<String, AiClientError> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    let url = if endpoint.ends_with("/responses") {
        endpoint.to_owned()
    } else {
        format!("{endpoint}/responses")
    };
    validate_endpoint_url(&url)?;
    Ok(url)
}

pub(crate) async fn request(
    client: &AiClient,
    auth: &PreparedAuth,
    user_content: &str,
    cancel: &CancellationToken,
) -> Result<Vec<crate::ai::AiCommand>, AiClientError> {
    let request = ResponsesRequest {
        model: &client.model,
        instructions: SYSTEM_PROMPT,
        input: vec![ResponsesInput {
            role: "user",
            content: vec![ResponsesInputContent {
                kind: "input_text",
                text: user_content,
            }],
        }],
        max_output_tokens: 500,
        store: false,
    };
    let mut builder = client
        .client
        .post(&client.endpoint)
        .bearer_auth(auth.bearer.as_str())
        .header("originator", "hokan")
        .header(
            reqwest::header::USER_AGENT,
            concat!("hokan/", env!("CARGO_PKG_VERSION")),
        );
    if let Some(account_id) = &auth.account_id {
        builder = builder.header("ChatGPT-Account-Id", account_id);
    }
    let body = send_and_read(builder.json(&request), cancel).await?;
    parse_response(&body)
}

/// Concatenates every `output[].content[]` item of type `output_text`, then
/// falls back to the top-level `output_text` convenience field.
fn parse_response(body: &[u8]) -> Result<Vec<crate::ai::AiCommand>, AiClientError> {
    let response: ResponsesReply =
        serde_json::from_slice(body).map_err(|_| AiClientError::InvalidResponse)?;
    let mut text = String::new();
    for item in &response.output {
        for content in &item.content {
            if content.kind == "output_text"
                && let Some(part) = &content.text
            {
                text.push_str(part);
            }
        }
    }
    if text.is_empty()
        && let Some(convenience) = response.output_text
    {
        text = convenience;
    }
    if text.is_empty() {
        return Err(AiClientError::InvalidResponse);
    }
    crate::ai::parse_ai_commands(&text).map_err(|_| AiClientError::InvalidResponse)
}

#[derive(Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    instructions: &'a str,
    input: Vec<ResponsesInput<'a>>,
    max_output_tokens: u32,
    store: bool,
}

#[derive(Serialize)]
struct ResponsesInput<'a> {
    role: &'a str,
    content: Vec<ResponsesInputContent<'a>>,
}

#[derive(Serialize)]
struct ResponsesInputContent<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    text: &'a str,
}

#[derive(Deserialize)]
struct ResponsesReply {
    #[serde(default)]
    output: Vec<ResponsesOutputItem>,
    #[serde(default)]
    output_text: Option<String>,
}

#[derive(Deserialize)]
struct ResponsesOutputItem {
    #[serde(default)]
    content: Vec<ResponsesContentItem>,
}

#[derive(Deserialize)]
struct ResponsesContentItem {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use super::*;
    use crate::{
        ai::{
            AiContext,
            test_support::{
                FAR_FUTURE, json_reply, request_body, request_path, spawn_mock_server,
                write_oauth_credential,
            },
        },
        config::AiConfig,
        shell::ShellKind,
    };

    fn codex_client(base: &str, credential_path: &std::path::Path, timeout_ms: u64) -> AiClient {
        let config = AiConfig {
            enabled: true,
            provider: "openai-oauth".into(),
            auth: crate::config::AiAuth::OAuth,
            endpoint: base.to_owned(),
            model: "gpt-test".into(),
            api_key_env: String::new(),
            timeout_ms,
            ..AiConfig::default()
        };
        AiClient::new(&config, credential_path).expect("client")
    }

    fn context() -> AiContext {
        AiContext {
            request: "list the current directory".into(),
            os: "test-os",
            architecture: "test-arch",
            shell: ShellKind::Zsh,
            cwd_basename: Some("project".into()),
            project_kinds: vec!["rust"],
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// Response body whose two `output_text` parts concatenate into one valid
    /// command envelope.
    fn two_part_reply() -> serde_json::Value {
        serde_json::json!({
            "output": [{
                "type": "message",
                "content": [
                    {"type": "output_text", "text": "{\"commands\":[{\"command\":\"ls\","},
                    {"type": "output_text", "text": "\"explanation\":\"list files\"}]}"}
                ]
            }]
        })
    }

    #[test]
    fn sends_responses_request_and_parses_output_text_parts() {
        let directory = tempfile::tempdir().expect("credential directory");
        let credential_path = directory.path().join("credentials.toml");
        write_oauth_credential(&credential_path, "openai-oauth", "codex-access", FAR_FUTURE);

        let (base, requests, join) =
            spawn_mock_server(1, |_| json_reply("200 OK", two_part_reply()));
        let client = codex_client(&base, &credential_path, 1_000);
        let commands = runtime()
            .block_on(client.request(&context(), &CancellationToken::new()))
            .expect("codex response");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command, "ls");
        assert_eq!(commands[0].explanation, "list files");

        let request = String::from_utf8(requests.recv().expect("request")).expect("UTF-8");
        assert_eq!(request_path(request.as_bytes()), "/responses");
        assert!(request.contains("authorization: Bearer codex-access"));
        assert!(request.contains("chatgpt-account-id: acct-1"));
        assert!(request.contains("originator: hokan"));
        assert!(request.contains(&format!("user-agent: hokan/{}", env!("CARGO_PKG_VERSION"))));

        let body: serde_json::Value =
            serde_json::from_str(&request_body(request.as_bytes())).expect("request JSON");
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["instructions"], SYSTEM_PROMPT);
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert!(
            body["input"][0]["content"][0]["text"]
                .as_str()
                .expect("prompt text")
                .contains("list the current directory")
        );
        assert_eq!(body["max_output_tokens"], 500);
        assert_eq!(body["store"], false);
        join.join().expect("server");
    }

    #[test]
    fn accepts_top_level_output_text_convenience_field() {
        let directory = tempfile::tempdir().expect("credential directory");
        let credential_path = directory.path().join("credentials.toml");
        write_oauth_credential(&credential_path, "openai-oauth", "codex-access", FAR_FUTURE);

        let (base, _requests, join) = spawn_mock_server(1, |_| {
            json_reply(
                "200 OK",
                serde_json::json!({
                    "output_text": "{\"commands\":[{\"command\":\"pwd\",\"explanation\":\"print directory\"}]}"
                }),
            )
        });
        let client = codex_client(&base, &credential_path, 1_000);
        let commands = runtime()
            .block_on(client.request(&context(), &CancellationToken::new()))
            .expect("codex response");
        assert_eq!(commands[0].command, "pwd");
        join.join().expect("server");
    }

    #[test]
    fn maps_401_to_unauthorized() {
        let directory = tempfile::tempdir().expect("credential directory");
        let credential_path = directory.path().join("credentials.toml");
        write_oauth_credential(&credential_path, "openai-oauth", "codex-access", FAR_FUTURE);

        let (base, _requests, join) =
            spawn_mock_server(1, |_| json_reply("401 Unauthorized", serde_json::json!({})));
        let client = codex_client(&base, &credential_path, 1_000);
        let error = runtime()
            .block_on(client.request(&context(), &CancellationToken::new()))
            .expect_err("401 must fail");
        assert_eq!(error, AiClientError::Unauthorized);
        join.join().expect("server");
    }

    #[test]
    fn enforces_response_size_limit() {
        let directory = tempfile::tempdir().expect("credential directory");
        let credential_path = directory.path().join("credentials.toml");
        write_oauth_credential(&credential_path, "openai-oauth", "codex-access", FAR_FUTURE);

        let oversized = "x".repeat(crate::ai::client::RESPONSE_MAX_BYTES + 1);
        let (base, _requests, join) = spawn_mock_server(1, move |_| {
            json_reply("200 OK", serde_json::json!({"output_text": oversized}))
        });
        let client = codex_client(&base, &credential_path, 1_000);
        assert_eq!(
            runtime()
                .block_on(client.request(&context(), &CancellationToken::new()))
                .expect_err("oversize body"),
            AiClientError::ResponseTooLarge
        );
        join.join().expect("server");
    }

    #[test]
    fn cancellation_interrupts_slow_response() {
        let directory = tempfile::tempdir().expect("credential directory");
        let credential_path = directory.path().join("credentials.toml");
        write_oauth_credential(&credential_path, "openai-oauth", "codex-access", FAR_FUTURE);

        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let (release_sender, release_receiver) = mpsc::channel();
        let (base, _requests, join) = spawn_mock_server(1, move |_| {
            trigger.cancel();
            release_receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("cancelled request must return before response");
            json_reply("200 OK", two_part_reply())
        });
        let client = codex_client(&base, &credential_path, 2_000);
        let error = runtime().block_on(client.request(&context(), &cancel));
        release_sender.send(()).expect("release server");
        assert_eq!(error.expect_err("cancel request"), AiClientError::Cancelled);
        join.join().expect("server");
    }
}
