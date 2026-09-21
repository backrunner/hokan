//! Exercise the updater's real client with process-local proxy settings.
//! Child processes avoid mutating the environment of parallel Rust tests.

use std::{
    env,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::Command,
    thread,
    time::{Duration, Instant},
};

const BASE_ENV: &str = "HOKAN_TEST_PROXY_BASE";

#[test]
fn proxy_worker_process() {
    let Ok(base) = env::var(BASE_ENV) else {
        return;
    };
    assert!(
        hokan::update::fetch_latest(hokan::update::Channel::Beta, &base, "backrunner/hokan")
            .expect("request through configured proxy policy")
            .is_none()
    );
}

#[test]
fn update_requests_follow_http_and_socks_proxies_and_no_proxy() {
    for mode in ["http", "lowercase", "socks5h", "bypass"] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let base = if mode == "bypass" {
            format!("http://{address}")
        } else {
            // Must never resolve or connect to this host directly.
            "http://hokan-update.invalid".to_owned()
        };
        let proxy = match mode {
            "socks5h" => format!("socks5h://{address}"),
            "bypass" => "http://127.0.0.1:1".to_owned(),
            _ => format!("http://{address}"),
        };
        let mut command = Command::new(env::current_exe().expect("test binary"));
        command.args(["--exact", "proxy_worker_process", "--nocapture"]);
        for key in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
            "REQUEST_METHOD",
        ] {
            command.env_remove(key);
        }
        command
            .env(BASE_ENV, base)
            .env(
                if mode == "lowercase" {
                    "http_proxy"
                } else {
                    "HTTP_PROXY"
                },
                proxy,
            )
            .env("NO_PROXY", if mode == "bypass" { "127.0.0.1" } else { "" });

        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let mut stream = accept_bounded(&listener, deadline);
                let mut first = [0];
                if stream.peek(&mut first).expect("protocol byte") == 0 {
                    continue;
                }
                let socks = mode == "socks5h" && first[0] == 5;
                if socks {
                    socks_handshake(&mut stream);
                }
                let request = read_request(&mut stream);
                // Developer services may probe any newly opened localhost port,
                // including a SOCKS listener. Do not consume the worker's turn.
                if request.starts_with("GET / HTTP/1.1\r\n") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }
                assert_eq!(socks, mode == "socks5h", "expected proxy protocol");
                let target = if matches!(mode, "http" | "lowercase") {
                    "http://hokan-update.invalid/repos/backrunner/hokan/releases?per_page=20&page=1"
                } else {
                    "/repos/backrunner/hokan/releases?per_page=20&page=1"
                };
                assert!(
                    request.starts_with(&format!("GET {target} HTTP/1.1\r\n")),
                    "{request}"
                );
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]",
                    )
                    .expect("reply");
                break;
            }
        });
        // Reproduce an unrelated port probe deterministically for every mode.
        let mut probe = TcpStream::connect(address).expect("probe");
        probe
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("probe timeout");
        probe
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("probe request");
        let mut reply = String::new();
        probe.read_to_string(&mut reply).expect("probe reply");
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
        let output = command.output().expect("proxy worker");
        server.join().expect("proxy server");
        assert!(
            output.status.success(),
            "{mode}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut byte = [0];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("request");
        request.push(byte[0]);
        assert!(request.len() < 8192, "request too large");
    }
    String::from_utf8(request).expect("HTTP request")
}

fn accept_bounded(listener: &TcpListener, deadline: Instant) -> TcpStream {
    listener.set_nonblocking(true).expect("nonblocking");
    loop {
        assert!(Instant::now() < deadline, "proxy worker timed out");
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).expect("blocking stream");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("read timeout");
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("write timeout");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "client did not reach the proxy or bypass destination"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept: {error}"),
        }
    }
}

fn socks_handshake(stream: &mut TcpStream) {
    let mut greeting = [0; 2];
    stream.read_exact(&mut greeting).expect("SOCKS greeting");
    assert_eq!(greeting[0], 5);
    let mut methods = vec![0; usize::from(greeting[1])];
    stream.read_exact(&mut methods).expect("SOCKS methods");
    assert!(methods.contains(&0), "anonymous proxy");
    stream.write_all(&[5, 0]).expect("SOCKS method");
    let mut connect = [0; 5];
    stream.read_exact(&mut connect).expect("SOCKS CONNECT");
    assert_eq!(&connect[..4], &[5, 1, 0, 3], "remote DNS");
    let mut host = vec![0; usize::from(connect[4])];
    stream.read_exact(&mut host).expect("SOCKS host");
    assert_eq!(host, b"hokan-update.invalid");
    let mut port = [0; 2];
    stream.read_exact(&mut port).expect("SOCKS port");
    assert_eq!(u16::from_be_bytes(port), 80);
    stream
        .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
        .expect("SOCKS connected");
}
