//! Loopback HTTP mock and release-fixture builders shared by the `update`
//! tests. Mirrors the hand-rolled `TcpListener` pattern from
//! `ai::oauth::test_support`; response bodies may embed the `{BASE}`
//! placeholder, which the server rewrites to its actual loopback address so
//! release JSON can carry absolute download URLs.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    thread,
    time::Duration,
};

use sha2::{Digest, Sha256};

use super::api;

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

pub(crate) fn raw_reply(status: &'static str, body: Vec<u8>) -> MockReply {
    MockReply { status, body }
}

/// Serves exactly `requests` connections, routing each request path through
/// `handler`. Returns the loopback base URL and the server thread.
pub(crate) fn spawn_server(
    requests: usize,
    mut handler: impl FnMut(&str) -> MockReply + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
    let address = listener.local_addr().expect("server address");
    let base = format!("http://{address}");
    let thread_base = base.clone();
    let join = thread::spawn(move || {
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().expect("accept request");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            let path = read_request_path(&mut stream);
            let reply = handler(&path);
            let body = replace_base(&reply.body, &thread_base);
            write!(
                stream,
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.status,
                body.len()
            )
            .expect("write headers");
            stream.write_all(&body).expect("write body");
            stream.flush().expect("flush response");
        }
    });
    (base, join)
}

fn read_request_path(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).expect("read request");
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&request)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_owned()
}

fn replace_base(body: &[u8], base: &str) -> Vec<u8> {
    let Ok(text) = String::from_utf8(body.to_vec()) else {
        return body.to_vec();
    };
    text.replace("{BASE}", base).into_bytes()
}

/// Archive file name this build expects in a release (`hokan-{v}-{target}.tar.gz`).
pub(crate) fn archive_asset(version: &str) -> String {
    api::archive_name(&semver::Version::parse(version).expect("fixture version"))
        .expect("test targets have an archive name")
}

/// Release JSON in the GitHub API shape; asset download URLs use the
/// `{BASE}` placeholder the mock server rewrites.
pub(crate) fn release_json(tag: &str, assets: &[String]) -> serde_json::Value {
    let assets: Vec<serde_json::Value> = assets
        .iter()
        .map(|name| {
            serde_json::json!({
                "name": name,
                "browser_download_url": format!("{{BASE}}/download/{name}"),
            })
        })
        .collect();
    serde_json::json!({
        "tag_name": tag,
        "prerelease": tag.contains('-'),
        "assets": assets,
    })
}

/// Builds a real tar.gz release archive containing `bin/hokan` (mode 0755)
/// with the given content.
pub(crate) fn build_archive(binary: &str) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    {
        let mut builder = tar::Builder::new(&mut encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(binary.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "bin/hokan", binary.as_bytes())
            .expect("append binary");
        builder.finish().expect("finish archive");
    }
    encoder.finish().expect("finish gzip")
}

/// Renders a SHA256SUMS file from `(hex_digest, file_name)` pairs.
pub(crate) fn sha256sums_for(entries: &[(&str, &str)]) -> String {
    entries
        .iter()
        .map(|(hash, name)| format!("{hash}  {name}\n"))
        .collect()
}

/// Serves a full release download: the archive followed by its SHA256SUMS.
pub(crate) fn serve_release(version: &str, archive: Vec<u8>) -> (String, thread::JoinHandle<()>) {
    let sums = sha256sums_for(&[(
        &format!("{:x}", Sha256::digest(&archive)),
        &archive_asset(version),
    )]);
    serve_release_with(version, archive, sums.into_bytes())
}

/// Same as [`serve_release`] but with caller-supplied sums content (for
/// checksum-mismatch tests).
pub(crate) fn serve_release_with(
    version: &str,
    archive: Vec<u8>,
    sums: Vec<u8>,
) -> (String, thread::JoinHandle<()>) {
    let archive_name = archive_asset(version);
    spawn_server(2, move |path| {
        if path == format!("/download/{archive_name}") {
            raw_reply("200 OK", archive.clone())
        } else if path == "/download/SHA256SUMS" {
            raw_reply("200 OK", sums.clone())
        } else {
            raw_reply("404 Not Found", Vec::new())
        }
    })
}

/// Writes an executable stub file (mode 0755) at `path`.
pub(crate) fn write_stub_binary(path: &Path, content: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, content).expect("write stub binary");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod stub binary");
}
