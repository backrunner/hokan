//! Loopback HTTP mock shared by the `ai` transport tests. Mirrors the
//! hand-rolled `TcpListener` pattern already used in `client`/`oauth` tests.
//! Response writes are best-effort so cancellation tests can hang up early.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::mpsc,
    thread,
    time::Duration,
};

/// Far-future token expiry (year 2128) that no provider skew triggers on.
pub(crate) const FAR_FUTURE: u64 = 4_000_000_000;

pub(crate) struct MockReply {
    status: &'static str,
    body: Vec<u8>,
}

pub(crate) fn json_reply(status: &'static str, body: serde_json::Value) -> MockReply {
    MockReply {
        status,
        body: serde_json::to_vec(&body).expect("reply JSON"),
    }
}

/// Chat-completions success envelope shared by the OAuth credential tests.
pub(crate) fn chat_success_body() -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {
                "content": "{\"commands\":[{\"command\":\"ls\",\"explanation\":\"list files\"}]}"
            }
        }]
    })
}

/// Serves exactly `requests` connections, routing each raw request through
/// `handler`; every request is forwarded to the returned channel.
pub(crate) fn spawn_mock_server(
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
            let _ = write!(
                stream,
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.status,
                reply.body.len()
            );
            let _ = stream.write_all(&reply.body);
            let _ = stream.flush();
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

pub(crate) fn request_path(request: &[u8]) -> String {
    String::from_utf8_lossy(request)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_owned()
}

pub(crate) fn request_body(request: &[u8]) -> String {
    let start = find_bytes(request, b"\r\n\r\n").expect("header end") + 4;
    String::from_utf8_lossy(&request[start..]).into_owned()
}

/// Writes OAuth tokens for `slug` with a fixed refresh token and account id.
pub(crate) fn write_oauth_credential(path: &Path, slug: &str, access_token: &str, expires_at: u64) {
    crate::config::write_credential(
        path,
        slug,
        &crate::config::ProviderCredential::OAuth(crate::config::OAuthTokens {
            access_token: zeroize::Zeroizing::new(access_token.to_owned()),
            refresh_token: zeroize::Zeroizing::new("refresh-token".to_owned()),
            expires_at,
            account_id: Some("acct-1".to_owned()),
        }),
    )
    .expect("write oauth credential");
}
