//! Gemini Code Assist transport (`gemini-oauth`): lazy `loadCodeAssist`
//! project resolution (with `onboardUser` fallback) followed by
//! `generateContent`. Wire shapes mirror gemini-cli
//! `packages/core/src/code_assist/{server,converter,setup,types}.ts`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::ai::client::{
    AiClient, AiClientError, PreparedAuth, SYSTEM_PROMPT, send_and_read, validate_endpoint_url,
};

/// Polling cadence for the onboard long-running operation (gemini-cli uses
/// 5s; a shorter interval keeps interactive latency acceptable).
const OPERATION_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Cap on operation polls before giving up (~60s).
const MAX_OPERATION_POLLS: u32 = 30;
/// `UserTierId::LEGACY`, setup.ts's fallback when no tier is the default.
const LEGACY_TIER_ID: &str = "legacy-tier";

/// The Code Assist base URL, used verbatim for `v1internal:<method>` calls.
pub(crate) fn normalize_endpoint(endpoint: &str) -> Result<String, AiClientError> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    validate_endpoint_url(endpoint)?;
    Ok(endpoint.to_owned())
}

pub(crate) async fn request(
    client: &AiClient,
    auth: &PreparedAuth,
    user_content: &str,
    cancel: &CancellationToken,
) -> Result<Vec<crate::ai::AiCommand>, AiClientError> {
    let project = resolve_project(client, auth, cancel).await?;
    let body = CaGenerateContentRequest {
        model: &client.model,
        project: &project,
        request: VertexGenerateContentRequest {
            contents: vec![CaContent {
                role: "user",
                parts: vec![CaPart { text: user_content }],
            }],
            system_instruction: CaContent {
                role: "user",
                parts: vec![CaPart {
                    text: SYSTEM_PROMPT,
                }],
            },
            generation_config: GenerationConfig {
                temperature: 0.1,
                max_output_tokens: 500,
            },
        },
    };
    let url = format!("{}/v1internal:generateContent", client.endpoint);
    let body = send_and_read(
        client
            .client
            .post(&url)
            .bearer_auth(auth.bearer.as_str())
            .json(&body),
        cancel,
    )
    .await?;
    parse_generate_response(&body)
}

/// Resolves the Code Assist project id once per process: `loadCodeAssist`
/// first, then the setup.ts project rules, then `onboardUser` as a fallback.
async fn resolve_project(
    client: &AiClient,
    auth: &PreparedAuth,
    cancel: &CancellationToken,
) -> Result<String, AiClientError> {
    if let Ok(guard) = client.code_assist_project.lock()
        && let Some(project) = guard.as_ref()
    {
        return Ok(project.clone());
    }
    let url = format!("{}/v1internal:loadCodeAssist", client.endpoint);
    let body = send_and_read(
        client
            .client
            .post(&url)
            .bearer_auth(auth.bearer.as_str())
            .json(&LoadCodeAssistRequest {
                metadata: ClientMetadata::core(),
            }),
        cancel,
    )
    .await?;
    let load: LoadCodeAssistResponse =
        serde_json::from_slice(&body).map_err(|_| AiClientError::InvalidResponse)?;
    // setup.ts: a server-managed project wins; an already-onboarded account
    // (currentTier) without one needs a user-supplied GCP project, which
    // hokan has no support for, so this arm stays `None` and fails below
    // with `CodeAssistProject`. Only tier-less accounts onboard here.
    let project = if let Some(project) = load.cloudaicompanion_project {
        Some(project)
    } else if load.current_tier.is_some() {
        None
    } else {
        onboard(client, auth, &load, cancel).await?
    };
    let Some(project) = project else {
        return Err(AiClientError::CodeAssistProject);
    };
    if let Ok(mut guard) = client.code_assist_project.lock() {
        *guard = Some(project.clone());
    }
    Ok(project)
}

/// setup.ts's onboarding path: the default allowed tier, or the legacy tier
/// when none is marked; the operation is polled until done.
async fn onboard(
    client: &AiClient,
    auth: &PreparedAuth,
    load: &LoadCodeAssistResponse,
    cancel: &CancellationToken,
) -> Result<Option<String>, AiClientError> {
    // The free tier manages its own project, and hokan has no user-defined
    // GCP project to send for the other tiers (no GOOGLE_CLOUD_PROJECT
    // support), so `cloudaicompanionProject` is always omitted here.
    let tier_id = load
        .allowed_tiers
        .iter()
        .find(|tier| tier.is_default)
        .and_then(|tier| tier.id.clone())
        .unwrap_or_else(|| LEGACY_TIER_ID.to_owned());
    let url = format!("{}/v1internal:onboardUser", client.endpoint);
    let body = send_and_read(
        client
            .client
            .post(&url)
            .bearer_auth(auth.bearer.as_str())
            .json(&OnboardUserRequest {
                tier_id: &tier_id,
                metadata: ClientMetadata::core(),
            }),
        cancel,
    )
    .await?;
    let mut operation: Operation =
        serde_json::from_slice(&body).map_err(|_| AiClientError::InvalidResponse)?;
    let mut polls = 0_u32;
    while !operation.done {
        let Some(name) = operation.name.clone() else {
            return Ok(None);
        };
        if polls >= MAX_OPERATION_POLLS {
            return Err(AiClientError::Timeout);
        }
        polls += 1;
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(AiClientError::Cancelled),
            () = tokio::time::sleep(OPERATION_POLL_INTERVAL) => {}
        }
        let url = format!("{}/v1internal/{name}", client.endpoint);
        let body = send_and_read(
            client.client.get(&url).bearer_auth(auth.bearer.as_str()),
            cancel,
        )
        .await?;
        operation = serde_json::from_slice(&body).map_err(|_| AiClientError::InvalidResponse)?;
    }
    Ok(operation
        .response
        .and_then(|response| response.cloudaicompanion_project)
        .and_then(|project| project.id))
}

fn parse_generate_response(body: &[u8]) -> Result<Vec<crate::ai::AiCommand>, AiClientError> {
    let parsed: CaGenerateContentResponse =
        serde_json::from_slice(body).map_err(|_| AiClientError::InvalidResponse)?;
    let mut text = String::new();
    if let Some(candidate) = parsed
        .response
        .and_then(|response| response.candidates.into_iter().next())
        && let Some(content) = candidate.content
    {
        for part in content.parts {
            if let Some(part_text) = part.text {
                text.push_str(&part_text);
            }
        }
    }
    if text.is_empty() {
        return Err(AiClientError::InvalidResponse);
    }
    crate::ai::parse_ai_commands(&text).map_err(|_| AiClientError::InvalidResponse)
}

#[derive(Serialize)]
struct LoadCodeAssistRequest<'a> {
    metadata: ClientMetadata<'a>,
}

/// setup.ts's `coreClientMetadata`; no `cloudaicompanionProject` /
/// `duetProject` is sent because hokan has no user-configured GCP project.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientMetadata<'a> {
    ide_type: &'a str,
    platform: &'a str,
    plugin_type: &'a str,
}

impl<'a> ClientMetadata<'a> {
    fn core() -> Self {
        Self {
            ide_type: "IDE_UNSPECIFIED",
            platform: "PLATFORM_UNSPECIFIED",
            plugin_type: "GEMINI",
        }
    }
}

#[derive(Deserialize)]
struct LoadCodeAssistResponse {
    #[serde(default, rename = "cloudaicompanionProject")]
    cloudaicompanion_project: Option<String>,
    #[serde(default, rename = "currentTier")]
    current_tier: Option<serde_json::Value>,
    #[serde(default, rename = "allowedTiers")]
    allowed_tiers: Vec<Tier>,
}

#[derive(Deserialize)]
struct Tier {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "isDefault")]
    is_default: bool,
}

#[derive(Serialize)]
struct OnboardUserRequest<'a> {
    #[serde(rename = "tierId")]
    tier_id: &'a str,
    metadata: ClientMetadata<'a>,
}

#[derive(Deserialize)]
struct Operation {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    response: Option<OnboardResponse>,
}

#[derive(Deserialize)]
struct OnboardResponse {
    #[serde(default, rename = "cloudaicompanionProject")]
    cloudaicompanion_project: Option<ProjectReference>,
}

#[derive(Deserialize)]
struct ProjectReference {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Serialize)]
struct CaGenerateContentRequest<'a> {
    model: &'a str,
    project: &'a str,
    request: VertexGenerateContentRequest<'a>,
}

#[derive(Serialize)]
struct VertexGenerateContentRequest<'a> {
    contents: Vec<CaContent<'a>>,
    #[serde(rename = "systemInstruction")]
    system_instruction: CaContent<'a>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

#[derive(Serialize)]
struct CaContent<'a> {
    role: &'a str,
    parts: Vec<CaPart<'a>>,
}

#[derive(Serialize)]
struct CaPart<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct GenerationConfig {
    temperature: f32,
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
}

#[derive(Deserialize)]
struct CaGenerateContentResponse {
    #[serde(default)]
    response: Option<VertexGenerateContentResponse>,
}

#[derive(Deserialize)]
struct VertexGenerateContentResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
}

#[derive(Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<CandidateContent>,
}

#[derive(Deserialize)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<CandidatePart>,
}

#[derive(Deserialize)]
struct CandidatePart {
    #[serde(default)]
    text: Option<String>,
}

#[cfg(test)]
mod tests {
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

    fn code_assist_client(base: &str, credential_path: &std::path::Path) -> AiClient {
        let config = AiConfig {
            enabled: true,
            provider: "gemini-oauth".into(),
            auth: crate::config::AiAuth::OAuth,
            endpoint: base.to_owned(),
            model: "gemini-test".into(),
            api_key_env: String::new(),
            timeout_ms: 2_000,
            ..AiConfig::default()
        };
        AiClient::new(&config, credential_path).expect("client")
    }

    fn context() -> AiContext {
        AiContext {
            request: "print the working directory".into(),
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

    fn generate_reply() -> serde_json::Value {
        serde_json::json!({
            "response": {
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [
                            {"text": "{\"commands\":[{\"command\":\"pwd\","},
                            {"text": "\"explanation\":\"print directory\"}]}"}
                        ]
                    }
                }]
            }
        })
    }

    #[test]
    fn loads_project_once_then_generates_content() {
        let directory = tempfile::tempdir().expect("credential directory");
        let credential_path = directory.path().join("credentials.toml");
        write_oauth_credential(
            &credential_path,
            "gemini-oauth",
            "gemini-access",
            FAR_FUTURE,
        );

        // Three requests: loadCodeAssist, generateContent, generateContent.
        let (base, requests, join) = spawn_mock_server(3, |request| {
            if request_path(request).ends_with(":loadCodeAssist") {
                json_reply(
                    "200 OK",
                    serde_json::json!({"cloudaicompanionProject": "proj-1"}),
                )
            } else {
                json_reply("200 OK", generate_reply())
            }
        });
        let client = code_assist_client(&base, &credential_path);
        let runtime = runtime();
        for _ in 0..2 {
            let commands = runtime
                .block_on(client.request(&context(), &CancellationToken::new()))
                .expect("code assist response");
            assert_eq!(commands[0].command, "pwd");
        }

        let load = String::from_utf8(requests.recv().expect("load request")).expect("UTF-8");
        assert_eq!(request_path(load.as_bytes()), "/v1internal:loadCodeAssist");
        assert!(load.contains("authorization: Bearer gemini-access"));
        let load_body: serde_json::Value =
            serde_json::from_str(&request_body(load.as_bytes())).expect("load JSON");
        assert_eq!(load_body["metadata"]["ideType"], "IDE_UNSPECIFIED");
        assert_eq!(load_body["metadata"]["platform"], "PLATFORM_UNSPECIFIED");
        assert_eq!(load_body["metadata"]["pluginType"], "GEMINI");

        let generate =
            String::from_utf8(requests.recv().expect("generate request")).expect("UTF-8");
        assert_eq!(
            request_path(generate.as_bytes()),
            "/v1internal:generateContent"
        );
        let generate_body: serde_json::Value =
            serde_json::from_str(&request_body(generate.as_bytes())).expect("generate JSON");
        assert_eq!(generate_body["model"], "gemini-test");
        assert_eq!(generate_body["project"], "proj-1");
        assert_eq!(generate_body["request"]["contents"][0]["role"], "user");
        assert!(
            generate_body["request"]["contents"][0]["parts"][0]["text"]
                .as_str()
                .expect("prompt text")
                .contains("print the working directory")
        );
        assert_eq!(
            generate_body["request"]["systemInstruction"]["parts"][0]["text"],
            SYSTEM_PROMPT
        );
        assert_eq!(
            generate_body["request"]["generationConfig"]["temperature"],
            0.1
        );
        assert_eq!(
            generate_body["request"]["generationConfig"]["maxOutputTokens"],
            500
        );

        // The second request skips loadCodeAssist: the project is cached.
        let second = String::from_utf8(requests.recv().expect("second generate")).expect("UTF-8");
        assert_eq!(
            request_path(second.as_bytes()),
            "/v1internal:generateContent"
        );
        join.join().expect("server");
    }

    #[test]
    fn onboards_default_free_tier_when_no_project_is_returned() {
        let directory = tempfile::tempdir().expect("credential directory");
        let credential_path = directory.path().join("credentials.toml");
        write_oauth_credential(
            &credential_path,
            "gemini-oauth",
            "gemini-access",
            FAR_FUTURE,
        );

        let (base, requests, join) = spawn_mock_server(3, |request| {
            let path = request_path(request);
            if path.ends_with(":loadCodeAssist") {
                json_reply(
                    "200 OK",
                    serde_json::json!({
                        "allowedTiers": [{"id": "free-tier", "isDefault": true}]
                    }),
                )
            } else if path.ends_with(":onboardUser") {
                json_reply(
                    "200 OK",
                    serde_json::json!({
                        "name": "operations/1",
                        "done": true,
                        "response": {"cloudaicompanionProject": {"id": "proj-free"}}
                    }),
                )
            } else {
                json_reply("200 OK", generate_reply())
            }
        });
        let client = code_assist_client(&base, &credential_path);
        let commands = runtime()
            .block_on(client.request(&context(), &CancellationToken::new()))
            .expect("onboarded response");
        assert_eq!(commands[0].command, "pwd");

        let _load = requests.recv().expect("load request");
        let onboard = String::from_utf8(requests.recv().expect("onboard request")).expect("UTF-8");
        assert_eq!(request_path(onboard.as_bytes()), "/v1internal:onboardUser");
        let onboard_body: serde_json::Value =
            serde_json::from_str(&request_body(onboard.as_bytes())).expect("onboard JSON");
        assert_eq!(onboard_body["tierId"], "free-tier");
        // The free tier manages its own project; none is sent.
        assert!(onboard_body.get("cloudaicompanionProject").is_none());
        assert_eq!(onboard_body["metadata"]["pluginType"], "GEMINI");

        let generate =
            String::from_utf8(requests.recv().expect("generate request")).expect("UTF-8");
        let generate_body: serde_json::Value =
            serde_json::from_str(&request_body(generate.as_bytes())).expect("generate JSON");
        assert_eq!(generate_body["project"], "proj-free");
        join.join().expect("server");
    }

    #[test]
    fn missing_project_after_onboarding_surfaces_clear_error() {
        let directory = tempfile::tempdir().expect("credential directory");
        let credential_path = directory.path().join("credentials.toml");
        write_oauth_credential(
            &credential_path,
            "gemini-oauth",
            "gemini-access",
            FAR_FUTURE,
        );

        // loadCodeAssist returns nothing usable; onboarding completes but
        // still yields no project.
        let (base, requests, join) = spawn_mock_server(2, |request| {
            if request_path(request).ends_with(":loadCodeAssist") {
                json_reply("200 OK", serde_json::json!({}))
            } else {
                json_reply(
                    "200 OK",
                    serde_json::json!({"name": "operations/2", "done": true, "response": {}}),
                )
            }
        });
        let client = code_assist_client(&base, &credential_path);
        let error = runtime()
            .block_on(client.request(&context(), &CancellationToken::new()))
            .expect_err("missing project must fail");
        assert_eq!(error, AiClientError::CodeAssistProject);
        assert_eq!(error.code(), "HK-AI-PROJECT");
        assert!(format!("{error}").contains("enable Gemini Code Assist"));

        let _load = requests.recv().expect("load request");
        let onboard = String::from_utf8(requests.recv().expect("onboard request")).expect("UTF-8");
        let onboard_body: serde_json::Value =
            serde_json::from_str(&request_body(onboard.as_bytes())).expect("onboard JSON");
        // No default tier: the legacy-tier fallback is used (setup.ts).
        assert_eq!(onboard_body["tierId"], "legacy-tier");
        join.join().expect("server");
    }

    #[test]
    fn invalid_generate_response_is_rejected() {
        let directory = tempfile::tempdir().expect("credential directory");
        let credential_path = directory.path().join("credentials.toml");
        write_oauth_credential(
            &credential_path,
            "gemini-oauth",
            "gemini-access",
            FAR_FUTURE,
        );

        let (base, _requests, join) = spawn_mock_server(2, |request| {
            if request_path(request).ends_with(":loadCodeAssist") {
                json_reply(
                    "200 OK",
                    serde_json::json!({"cloudaicompanionProject": "proj-1"}),
                )
            } else {
                json_reply(
                    "200 OK",
                    serde_json::json!({"response": {"candidates": []}}),
                )
            }
        });
        let client = code_assist_client(&base, &credential_path);
        assert_eq!(
            runtime()
                .block_on(client.request(&context(), &CancellationToken::new()))
                .expect_err("empty candidates must fail"),
            AiClientError::InvalidResponse
        );
        join.join().expect("server");
    }
}
