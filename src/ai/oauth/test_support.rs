//! Loopback HTTP mock shared by the `oauth` flow tests. Distinct from
//! `crate::ai::test_support`: this variant supports extra response headers
//! (the Codex 429 test needs `Retry-After`) and fails hard on write errors.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use zeroize::Zeroizing;

use super::OAuthEndpoints;
use crate::config::OAuthTokens;

pub(super) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

pub(super) struct MockReply {
    pub(super) status: &'static str,
    pub(super) body: Vec<u8>,
    pub(super) extra_headers: String,
}

pub(super) fn json_reply(status: &'static str, body: serde_json::Value) -> MockReply {
    MockReply {
        status,
        body: serde_json::to_vec(&body).expect("reply JSON"),
        extra_headers: String::new(),
    }
}

/// Serves exactly `requests` connections, routing each raw request through
/// `handler`; every request is forwarded to the returned channel.
pub(super) fn spawn_server(
    requests: usize,
    mut handler: impl FnMut(&[u8]) -> MockReply + Send + 'static,
) -> (String, mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
    let address = listener.local_addr().expect("server address");
    let (request_sender, request_receiver) = mpsc::channel();
    let join = thread::spawn(move || {
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().expect("accept request");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            let request = read_http_request(&mut stream);
            let _ = request_sender.send(request.clone());
            let reply = handler(&request);
            write_response(&mut stream, reply.status, &reply.body, &reply.extra_headers);
        }
    });
    (format!("http://{address}"), request_receiver, join)
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

pub(super) fn request_path(request: &[u8]) -> String {
    String::from_utf8_lossy(request)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_owned()
}

pub(super) fn request_body(request: &[u8]) -> String {
    let start = find_bytes(request, b"\r\n\r\n").expect("header end") + 4;
    String::from_utf8_lossy(&request[start..]).into_owned()
}

pub(super) fn test_endpoints(base: &str) -> OAuthEndpoints {
    OAuthEndpoints {
        device_code: format!("{base}/oauth2/device/code"),
        codex_user_code: format!("{base}/api/accounts/deviceauth/usercode"),
        codex_device_token: format!("{base}/api/accounts/deviceauth/token"),
        token: format!("{base}/oauth/token"),
        authorize: format!("{base}/o/oauth2/v2/auth"),
    }
}

pub(super) fn sample_tokens(refresh_token: &str) -> OAuthTokens {
    OAuthTokens {
        access_token: Zeroizing::new("old-access".to_owned()),
        refresh_token: Zeroizing::new(refresh_token.to_owned()),
        expires_at: 1_000_000,
        account_id: Some("acct-keep".to_owned()),
    }
}

pub(super) fn unsigned_jwt(payload: serde_json::Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("payload JSON"));
    format!("{header}.{payload}.fakesig")
}

pub(super) fn grok_device_code_reply() -> MockReply {
    json_reply(
        "200 OK",
        serde_json::json!({
            "device_code": "device-code-1",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://x.ai/device",
            "verification_uri_complete": "https://x.ai/device?code=ABCD-EFGH",
            "expires_in": 900,
            "interval": 0
        }),
    )
}

pub(super) fn grok_token_reply() -> MockReply {
    json_reply(
        "200 OK",
        serde_json::json!({
            "access_token": "grok-access",
            "refresh_token": "grok-refresh",
            "expires_in": 3600,
            "token_type": "Bearer"
        }),
    )
}
