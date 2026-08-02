use std::{path::Path, time::Duration};

use reqwest::{Client, StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    ai::protocol::AiContext,
    config::{AiConfig, CredentialError, load_api_key},
};

const RESPONSE_MAX_BYTES: usize = 128 * 1024;

pub struct AiClient {
    client: Client,
    endpoint: String,
    model: String,
    config: AiConfig,
    default_credential_path: std::path::PathBuf,
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
        let endpoint = normalize_endpoint(&config.endpoint)?;
        Ok(Self {
            client,
            endpoint,
            model: config.model.clone(),
            config: config.clone(),
            default_credential_path: default_credential_path.to_owned(),
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
        let api_key =
            load_api_key(&self.config, &self.default_credential_path).map_err(|error| {
                if matches!(error, CredentialError::Missing) {
                    AiClientError::MissingCredential
                } else {
                    AiClientError::CredentialRejected
                }
            })?;
        let user_content = user_prompt(context);
        let request = ChatRequest {
            model: &self.model,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: "Return only JSON: {\"commands\":[{\"command\":\"...\",\"explanation\":\"...\"}]}. Provide 1-5 single-line shell commands. Never use Markdown.",
                },
                ChatMessage {
                    role: "user",
                    content: &user_content,
                },
            ],
            temperature: 0.1,
            max_tokens: 500,
        };
        let request = self
            .client
            .post(&self.endpoint)
            .bearer_auth(api_key.as_str())
            .json(&request);
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
        let response: ChatResponse =
            serde_json::from_slice(&body).map_err(|_| AiClientError::InvalidResponse)?;
        let content = response
            .choices
            .first()
            .map(|choice| choice.message.content.as_str())
            .ok_or(AiClientError::InvalidResponse)?;
        crate::ai::parse_ai_commands(content).map_err(|_| AiClientError::InvalidResponse)
    }
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
    let parsed = reqwest::Url::parse(&url).map_err(|_| AiClientError::Configuration)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(AiClientError::Configuration);
    }
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
    Http(u16),
}

impl std::fmt::Display for AiClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Configuration => "AI endpoint or client configuration is invalid",
            Self::MissingCredential => "AI credential environment variable is not set",
            Self::CredentialRejected => {
                "AI credential file was rejected; run `hokann config ai` for diagnostics"
            }
            Self::Unauthorized => "AI endpoint rejected the credential",
            Self::RateLimited => "AI endpoint rate limit was reached",
            Self::Timeout => "AI request timed out",
            Self::Network => "AI network request failed",
            Self::ResponseTooLarge => "AI response exceeded the size limit",
            Self::InvalidResponse => "AI response did not contain valid command JSON",
            Self::Cancelled => "AI request was cancelled",
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
            Self::Http(_) => "HK-AI-HTTP",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        path::PathBuf,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::{config::write_api_key, shell::ShellKind};

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
}
