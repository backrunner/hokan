use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use super::*;
use crate::{
    ai::test_support::{
        self, FAR_FUTURE, chat_success_body, request_path, spawn_mock_server,
        write_oauth_credential,
    },
    config::write_api_key,
    shell::ShellKind,
};

struct TestClient {
    _directory: tempfile::TempDir,
    client: AiClient,
}

fn test_client(endpoint: &str, timeout: Duration) -> TestClient {
    let directory = tempfile::tempdir().expect("credential directory");
    let credential_path = directory.path().join("credentials.toml");
    write_api_key(&credential_path, "test-secret").expect("write credential");
    let config = AiConfig {
        enabled: true,
        endpoint: endpoint.to_owned(),
        model: "test-model".into(),
        api_key_file: Some(PathBuf::from("credentials.toml")),
        timeout_ms: timeout.as_millis() as u64,
        ..AiConfig::default()
    };
    let client = AiClient::new(&config, &credential_path).expect("client");
    TestClient {
        _directory: directory,
        client,
    }
}

/// OAuth client for the chat transport (`grok-oauth`); the credential is
/// written by the caller before this is invoked.
fn oauth_chat_client(endpoint: &str, credential_path: &Path, timeout: Duration) -> AiClient {
    let config = AiConfig {
        enabled: true,
        provider: "grok-oauth".into(),
        auth: AiAuth::OAuth,
        endpoint: endpoint.to_owned(),
        model: "grok-test".into(),
        api_key_env: String::new(),
        timeout_ms: timeout.as_millis() as u64,
        ..AiConfig::default()
    };
    AiClient::new(&config, credential_path).expect("client")
}

fn context() -> AiContext {
    AiContext {
        request: "find recently modified Rust files".into(),
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

fn spawn_server<F>(handler: F) -> (String, mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>)
where
    F: FnOnce(&mut TcpStream) + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
    let address = listener.local_addr().expect("server address");
    let (request_sender, request_receiver) = mpsc::channel();
    let join = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        let request = read_http_request(&mut stream);
        let _ = request_sender.send(request);
        handler(&mut stream);
    });
    (format!("http://{address}/v1"), request_receiver, join)
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut expected = None;
    loop {
        let count = stream.read(&mut buffer).expect("read request");
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);
        if expected.is_none()
            && let Some(header_end) = find_bytes(&request, b"\r\n\r\n")
        {
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            expected = Some(header_end + 4 + content_length);
        }
        if expected.is_some_and(|length| request.len() >= length) {
            break;
        }
    }
    request
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_response(stream: &mut TcpStream, status: &str, body: &[u8], extra: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    )
    .expect("write headers");
    stream.write_all(body).expect("write body");
    stream.flush().expect("flush response");
}

fn success_body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
            "choices": [{
                "message": {
                    "content": "{\"commands\":[{\"command\":\"find . -name '*.rs'\",\"explanation\":\"find Rust files\"}]}"
                }
            }]
        }))
        .expect("response JSON")
}

#[test]
fn sends_minimal_request_and_parses_success() {
    let body = success_body();
    let (endpoint, request_receiver, join) = spawn_server(move |stream| {
        write_response(
            stream,
            "200 OK",
            &body,
            "Content-Type: application/json\r\n",
        );
    });
    let test = test_client(&endpoint, Duration::from_secs(1));
    let commands = runtime()
        .block_on(test.client.request(&context(), &CancellationToken::new()))
        .expect("AI response");
    assert_eq!(commands[0].command, "find . -name '*.rs'");

    let request =
        String::from_utf8(request_receiver.recv().expect("request")).expect("UTF-8 request");
    assert!(request.starts_with("POST /v1/chat/completions "));
    assert!(request.contains("authorization: Bearer test-secret"));
    assert!(request.contains("find recently modified Rust files"));
    assert!(!request.contains("history"));
    assert!(!request.contains("/full/path/to/project"));
    join.join().expect("server");
}

#[test]
fn maps_http_errors_without_exposing_credentials() {
    for (status, expected) in [
        ("401 Unauthorized", AiClientError::Unauthorized),
        ("403 Forbidden", AiClientError::Unauthorized),
        ("404 Not Found", AiClientError::Http(404)),
        ("429 Too Many Requests", AiClientError::RateLimited),
        ("500 Server Error", AiClientError::Http(500)),
    ] {
        let (endpoint, _, join) = spawn_server(move |stream| {
            write_response(stream, status, b"ignored", "");
        });
        let test = test_client(&endpoint, Duration::from_secs(1));
        let error = runtime()
            .block_on(test.client.request(&context(), &CancellationToken::new()))
            .expect_err("HTTP status must fail");
        assert_eq!(error, expected);
        assert!(!format!("{error:?} {error}").contains("test-secret"));
        join.join().expect("server");
    }
}

#[test]
fn never_follows_redirects_with_authorization() {
    let target = TcpListener::bind("127.0.0.1:0").expect("redirect target");
    target.set_nonblocking(true).expect("nonblocking target");
    let target_url = format!(
        "http://{}/capture",
        target.local_addr().expect("target address")
    );
    let (endpoint, _, join) = spawn_server(move |stream| {
        write_response(
            stream,
            "302 Found",
            b"redirect",
            &format!("Location: {target_url}\r\n"),
        );
    });
    let test = test_client(&endpoint, Duration::from_secs(1));
    let error = runtime()
        .block_on(test.client.request(&context(), &CancellationToken::new()))
        .expect_err("redirect must not be followed");
    assert_eq!(error, AiClientError::Http(302));
    thread::sleep(Duration::from_millis(20));
    assert_eq!(
        target
            .accept()
            .expect_err("target must receive no request")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    join.join().expect("server");
}

#[test]
fn enforces_body_limit_and_json_shape() {
    let (endpoint, _, join) = spawn_server(|stream| {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            RESPONSE_MAX_BYTES + 1
        )
        .expect("oversize headers");
        stream.flush().expect("flush headers");
    });
    let test = test_client(&endpoint, Duration::from_secs(1));
    assert_eq!(
        runtime()
            .block_on(test.client.request(&context(), &CancellationToken::new()))
            .expect_err("oversize body"),
        AiClientError::ResponseTooLarge
    );
    join.join().expect("server");

    let (endpoint, _, join) = spawn_server(|stream| {
        write_response(stream, "200 OK", b"{not-json", "");
    });
    let test = test_client(&endpoint, Duration::from_secs(1));
    assert_eq!(
        runtime()
            .block_on(test.client.request(&context(), &CancellationToken::new()))
            .expect_err("invalid JSON"),
        AiClientError::InvalidResponse
    );
    join.join().expect("server");
}

#[test]
fn cancellation_interrupts_slow_headers() {
    let (endpoint, _, join) = spawn_server(|stream| {
        thread::sleep(Duration::from_millis(150));
        let _ = write_response_after_cancel(stream);
    });
    let test = test_client(&endpoint, Duration::from_secs(2));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let started = Instant::now();
    let error = runtime().block_on(async {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });
        test.client.request(&context(), &cancel).await
    });
    assert_eq!(error.expect_err("cancel request"), AiClientError::Cancelled);
    assert!(started.elapsed() < Duration::from_millis(120));
    join.join().expect("server");
}

fn write_response_after_cancel(stream: &mut TcpStream) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    )
}

#[test]
fn timeout_applies_while_reading_body() {
    let (endpoint, _, join) = spawn_server(|stream| {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx"
        )
        .expect("partial response");
        stream.flush().expect("flush partial response");
        thread::sleep(Duration::from_millis(150));
    });
    let test = test_client(&endpoint, Duration::from_millis(40));
    assert_eq!(
        runtime()
            .block_on(test.client.request(&context(), &CancellationToken::new()))
            .expect_err("slow body must time out"),
        AiClientError::Timeout
    );
    join.join().expect("server");
}

#[test]
fn timeout_applies_while_waiting_for_headers() {
    let (endpoint, _, join) = spawn_server(|stream| {
        thread::sleep(Duration::from_millis(150));
        let _ = write_response_after_cancel(stream);
    });
    let test = test_client(&endpoint, Duration::from_millis(40));
    assert_eq!(
        runtime()
            .block_on(test.client.request(&context(), &CancellationToken::new()))
            .expect_err("slow headers must time out"),
        AiClientError::Timeout
    );
    join.join().expect("server");
}

#[test]
fn reports_connection_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral address");
    let endpoint = format!("http://{}/v1", listener.local_addr().expect("address"));
    drop(listener);
    let test = test_client(&endpoint, Duration::from_millis(100));
    assert_eq!(
        runtime()
            .block_on(test.client.request(&context(), &CancellationToken::new()))
            .expect_err("connection must fail"),
        AiClientError::Network
    );
}

#[test]
fn ollama_falls_back_to_placeholder_bearer() {
    let body = success_body();
    let (endpoint, request_receiver, join) = spawn_server(move |stream| {
        write_response(stream, "200 OK", &body, "");
    });
    let directory = tempfile::tempdir().expect("credential directory");
    let credential_path = directory.path().join("credentials.toml");
    let config = AiConfig {
        enabled: true,
        provider: "ollama".into(),
        endpoint: endpoint.clone(),
        model: "llama-test".into(),
        // No credential source at all: no file, and an env var that is
        // never set in this process.
        api_key_file: None,
        api_key_env: "HOKAN_TEST_NEVER_SET_KEY".into(),
        timeout_ms: 1_000,
        ..AiConfig::default()
    };
    let client = AiClient::new(&config, &credential_path).expect("client");
    let commands = runtime()
        .block_on(client.request(&context(), &CancellationToken::new()))
        .expect("ollama request must not need a credential");
    assert_eq!(commands[0].command, "find . -name '*.rs'");

    let request =
        String::from_utf8(request_receiver.recv().expect("request")).expect("UTF-8 request");
    assert!(request.contains("authorization: Bearer ollama"));
    join.join().expect("server");
}

#[test]
fn missing_key_stays_an_error_for_non_ollama_providers() {
    let directory = tempfile::tempdir().expect("credential directory");
    let credential_path = directory.path().join("credentials.toml");
    let config = AiConfig {
        enabled: true,
        provider: "deepseek".into(),
        endpoint: "http://127.0.0.1:1/v1".into(),
        model: "deepseek-chat".into(),
        api_key_file: None,
        api_key_env: "HOKAN_TEST_NEVER_SET_KEY".into(),
        timeout_ms: 1_000,
        ..AiConfig::default()
    };
    let client = AiClient::new(&config, &credential_path).expect("client");
    assert_eq!(
        runtime()
            .block_on(client.request(&context(), &CancellationToken::new()))
            .expect_err("missing key must fail"),
        AiClientError::MissingCredential
    );
}

#[test]
fn oauth_tokens_authorize_chat_transport() {
    let body = success_body();
    let (endpoint, request_receiver, join) = spawn_server(move |stream| {
        write_response(stream, "200 OK", &body, "");
    });
    let directory = tempfile::tempdir().expect("credential directory");
    let credential_path = directory.path().join("credentials.toml");
    write_oauth_credential(&credential_path, "grok-oauth", "grok-access", FAR_FUTURE);
    let client = oauth_chat_client(&endpoint, &credential_path, Duration::from_secs(1));
    let commands = runtime()
        .block_on(client.request(&context(), &CancellationToken::new()))
        .expect("oauth request");
    assert_eq!(commands[0].command, "find . -name '*.rs'");

    let request =
        String::from_utf8(request_receiver.recv().expect("request")).expect("UTF-8 request");
    assert!(request.contains("authorization: Bearer grok-access"));
    join.join().expect("server");
}

#[test]
fn api_key_entry_under_oauth_slug_is_missing_credential() {
    let directory = tempfile::tempdir().expect("credential directory");
    let credential_path = directory.path().join("credentials.toml");
    write_credential(
        &credential_path,
        "grok-oauth",
        &ProviderCredential::ApiKey(Zeroizing::new("api-key".to_owned())),
    )
    .expect("write api key credential");
    // The endpoint would fail to connect; the credential error must fire
    // before any request is attempted.
    let client = oauth_chat_client(
        "http://127.0.0.1:1/v1",
        &credential_path,
        Duration::from_secs(1),
    );
    assert_eq!(
        runtime()
            .block_on(client.request(&context(), &CancellationToken::new()))
            .expect_err("api key under oauth slug"),
        AiClientError::MissingCredential
    );
}

#[test]
fn expired_oauth_token_is_refreshed_and_persisted() {
    let directory = tempfile::tempdir().expect("credential directory");
    let credential_path = directory.path().join("credentials.toml");
    // expires_at = 1: long past, so any skew triggers the refresh.
    write_oauth_credential(&credential_path, "grok-oauth", "old-access", 1);

    let (refresh_base, refresh_requests, refresh_join) = spawn_mock_server(1, |_| {
        test_support::json_reply(
            "200 OK",
            serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "expires_in": 3600
            }),
        )
    });
    let body = success_body();
    let (endpoint, request_receiver, join) = spawn_server(move |stream| {
        write_response(stream, "200 OK", &body, "");
    });

    let mut client = oauth_chat_client(&endpoint, &credential_path, Duration::from_secs(1));
    client.refresh_endpoint = Some(format!("{refresh_base}/oauth/token"));
    let commands = runtime()
        .block_on(client.request(&context(), &CancellationToken::new()))
        .expect("refreshed request");
    assert_eq!(commands[0].command, "find . -name '*.rs'");

    let refresh_request = String::from_utf8(refresh_requests.recv().expect("refresh request"))
        .expect("UTF-8 request");
    assert_eq!(request_path(refresh_request.as_bytes()), "/oauth/token");
    assert!(refresh_request.contains("grant_type=refresh_token"));
    assert!(refresh_request.contains("refresh_token=refresh-token"));

    let request =
        String::from_utf8(request_receiver.recv().expect("request")).expect("UTF-8 request");
    assert!(request.contains("authorization: Bearer new-access"));

    // The rotated tokens were written back to the credentials file.
    match read_credential(&credential_path, "grok-oauth").expect("rotated credential") {
        ProviderCredential::OAuth(tokens) => {
            assert_eq!(tokens.access_token.as_str(), "new-access");
            assert_eq!(tokens.refresh_token.as_str(), "new-refresh");
        }
        ProviderCredential::ApiKey(_) => panic!("entry must stay OAuth tokens"),
    }
    join.join().expect("server");
    refresh_join.join().expect("refresh server");
}

#[test]
fn failed_refresh_falls_back_to_stale_token_and_surfaces_401() {
    let directory = tempfile::tempdir().expect("credential directory");
    let credential_path = directory.path().join("credentials.toml");
    write_oauth_credential(&credential_path, "grok-oauth", "old-access", 1);

    // A transient server failure keeps the stale tokens...
    let (refresh_base, _refresh_requests, refresh_join) = spawn_mock_server(1, |_| {
        test_support::json_reply(
            "500 Server Error",
            serde_json::json!({"error": "temporarily_unavailable"}),
        )
    });
    let (endpoint, request_receiver, join) = spawn_server(move |stream| {
        write_response(stream, "401 Unauthorized", b"stale", "");
    });

    let mut client = oauth_chat_client(&endpoint, &credential_path, Duration::from_secs(1));
    client.refresh_endpoint = Some(format!("{refresh_base}/oauth/token"));
    let error = runtime()
        .block_on(client.request(&context(), &CancellationToken::new()))
        .expect_err("stale token must be rejected by the server");
    assert_eq!(error, AiClientError::Unauthorized);

    let request =
        String::from_utf8(request_receiver.recv().expect("request")).expect("UTF-8 request");
    assert!(request.contains("authorization: Bearer old-access"));
    join.join().expect("server");
    refresh_join.join().expect("refresh server");
}

#[test]
fn chat_success_shape_matches_shared_test_body() {
    // Guards the shared helper used by the other transports' tests.
    let body = serde_json::to_vec(&chat_success_body()).expect("body JSON");
    let parsed: ChatResponse = serde_json::from_slice(&body).expect("chat response");
    assert!(parsed.choices[0].message.content.contains("commands"));
}

#[test]
fn revoked_refresh_token_fails_as_credential_rejected() {
    let directory = tempfile::tempdir().expect("credential directory");
    let credential_path = directory.path().join("credentials.toml");
    write_oauth_credential(&credential_path, "grok-oauth", "old-access", 1);

    // invalid_grant arrives as a 400: the refresh token is permanently
    // revoked, so the client must fail fast instead of retrying the doomed
    // refresh before every request.
    let (refresh_base, _refresh_requests, refresh_join) = spawn_mock_server(1, |_| {
        test_support::json_reply(
            "400 Bad Request",
            serde_json::json!({"error": "invalid_grant"}),
        )
    });

    // Unroutable endpoint: the failure must happen before any request is
    // attempted with the stale access token.
    let mut client = oauth_chat_client(
        "http://127.0.0.1:1/v1",
        &credential_path,
        Duration::from_secs(1),
    );
    client.refresh_endpoint = Some(format!("{refresh_base}/oauth/token"));
    assert_eq!(
        runtime()
            .block_on(client.request(&context(), &CancellationToken::new()))
            .expect_err("revoked refresh token must fail fast"),
        AiClientError::CredentialRejected
    );
    refresh_join.join().expect("refresh server");
}

#[test]
fn same_tick_cancellation_still_yields_completed_refresh() {
    let tokens = || OAuthTokens {
        access_token: Zeroizing::new("new-access".to_owned()),
        refresh_token: Zeroizing::new("new-refresh".to_owned()),
        expires_at: FAR_FUTURE,
        account_id: None,
    };
    // Both branches ready on the first poll: the completed refresh must win
    // so its rotated tokens get persisted instead of dropped while the
    // server has already invalidated the old refresh token.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let refreshed = runtime()
        .block_on(await_refresh(std::future::ready(Ok(tokens())), &cancel))
        .expect("completed refresh beats a same-tick cancel")
        .expect("refresh result");
    assert_eq!(refreshed.access_token.as_str(), "new-access");

    // A pending refresh is still interrupted by the cancellation.
    let pending = runtime().block_on(await_refresh(
        std::future::pending::<Result<OAuthTokens, OAuthError>>(),
        &cancel,
    ));
    assert!(pending.is_none());
}

#[test]
fn oauth_refresh_reads_and_writes_the_configured_api_key_file() {
    let directory = tempfile::tempdir().expect("credential directory");
    let default_path = directory.path().join("credentials.toml");
    let custom_path = directory.path().join("custom.toml");
    // expires_at = 1: long past, so any skew triggers the refresh.
    write_oauth_credential(&custom_path, "grok-oauth", "old-access", 1);

    let (refresh_base, _refresh_requests, refresh_join) = spawn_mock_server(1, |_| {
        test_support::json_reply(
            "200 OK",
            serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "expires_in": 3600
            }),
        )
    });
    let body = success_body();
    let (endpoint, request_receiver, join) = spawn_server(move |stream| {
        write_response(stream, "200 OK", &body, "");
    });

    let config = AiConfig {
        enabled: true,
        provider: "grok-oauth".into(),
        auth: AiAuth::OAuth,
        endpoint: endpoint.clone(),
        model: "grok-test".into(),
        api_key_env: String::new(),
        // A relative api_key_file resolves against the default path's parent.
        api_key_file: Some(PathBuf::from("custom.toml")),
        timeout_ms: 1_000,
        ..AiConfig::default()
    };
    let mut client = AiClient::new(&config, &default_path).expect("client");
    client.refresh_endpoint = Some(format!("{refresh_base}/oauth/token"));
    let commands = runtime()
        .block_on(client.request(&context(), &CancellationToken::new()))
        .expect("refreshed request");
    assert_eq!(commands[0].command, "find . -name '*.rs'");

    let request =
        String::from_utf8(request_receiver.recv().expect("request")).expect("UTF-8 request");
    assert!(request.contains("authorization: Bearer new-access"));

    // Read and write both used the configured file, not the default path.
    match read_credential(&custom_path, "grok-oauth").expect("rotated credential") {
        ProviderCredential::OAuth(tokens) => {
            assert_eq!(tokens.access_token.as_str(), "new-access");
            assert_eq!(tokens.refresh_token.as_str(), "new-refresh");
        }
        ProviderCredential::ApiKey(_) => panic!("entry must stay OAuth tokens"),
    }
    assert!(
        read_credential(&default_path, "grok-oauth").is_err(),
        "default credentials path must stay untouched"
    );
    join.join().expect("server");
    refresh_join.join().expect("refresh server");
}

#[test]
fn unwritable_credential_store_still_uses_refreshed_tokens() {
    let directory = tempfile::tempdir().expect("credential directory");
    let credential_path = directory.path().join("credentials.toml");
    write_oauth_credential(&credential_path, "grok-oauth", "old-access", 1);
    // A directory squatting on the lock path makes every write fail while
    // reads still succeed (the seed write left a lock FILE behind).
    let lock_path = directory.path().join("credentials.toml.lock");
    std::fs::remove_file(&lock_path).expect("remove the lock file");
    std::fs::create_dir(&lock_path).expect("squat the lock path");

    let (refresh_base, _refresh_requests, refresh_join) = spawn_mock_server(1, |_| {
        test_support::json_reply(
            "200 OK",
            serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "expires_in": 3600
            }),
        )
    });
    let body = success_body();
    let (endpoint, request_receiver, join) = spawn_server(move |stream| {
        write_response(stream, "200 OK", &body, "");
    });

    let mut client = oauth_chat_client(&endpoint, &credential_path, Duration::from_secs(1));
    client.refresh_endpoint = Some(format!("{refresh_base}/oauth/token"));
    // The failed write is swallowed: this request still uses the rotated
    // tokens, and the next request simply refreshes again.
    let commands = runtime()
        .block_on(client.request(&context(), &CancellationToken::new()))
        .expect("request must succeed despite the failed write");
    assert_eq!(commands[0].command, "find . -name '*.rs'");

    let request =
        String::from_utf8(request_receiver.recv().expect("request")).expect("UTF-8 request");
    assert!(request.contains("authorization: Bearer new-access"));

    // The write really failed: the store still holds the old tokens.
    match read_credential(&credential_path, "grok-oauth").expect("credential") {
        ProviderCredential::OAuth(tokens) => {
            assert_eq!(tokens.access_token.as_str(), "old-access");
        }
        ProviderCredential::ApiKey(_) => panic!("entry must stay OAuth tokens"),
    }
    join.join().expect("server");
    refresh_join.join().expect("refresh server");
}
