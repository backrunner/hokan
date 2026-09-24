//! GitHub Releases queries for update checks.
//!
//! The base URL is injectable so tests run against a loopback mock server.
//! The reqwest client is async (matching the rest of the crate), so every
//! public entry point drives its future on a private current-thread runtime,
//! the same pattern as `ai::oauth`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Client, StatusCode, header::HeaderMap, redirect::Policy};
use semver::Version;
use serde::Deserialize;

use super::{
    Channel, DEFAULT_API_BASE, DEFAULT_API_MIRROR_TEMPLATES, DEFAULT_MIRROR_TEMPLATES, MIRRORS_ENV,
    UpdateError,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const ASSET_REDIRECT_LIMIT: usize = 5;
const MAX_RELEASE_PAGES: usize = 100;
/// Cap on release-metadata bodies; real replies are a few KiB of JSON.
const RESPONSE_BODY_MAX_BYTES: usize = 1024 * 1024;

/// One resolved release: version plus the download URLs for this platform.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseInfo {
    pub version: Version,
    pub tag: String,
    pub archive_url: String,
    pub checksums_url: String,
    pub signature_url: String,
}

/// Runs `future` on a private current-thread runtime (same pattern as
/// `ai::oauth::block_on_flow`): update checks are synchronous code.
pub(crate) fn block_on<F>(future: F) -> Result<F::Output, UpdateError>
where
    F: std::future::Future,
{
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| UpdateError::Network)?
        .block_on(future))
}

/// GitHub API client: metadata requests never need redirects, so keep them
/// disabled and fail closed if an unexpected redirect is returned.
pub(crate) fn api_client() -> Result<Client, UpdateError> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("hokan/", env!("CARGO_PKG_VERSION")))
        .redirect(Policy::none())
        .build()
        .map_err(|_| UpdateError::Network)
}

/// Release assets normally redirect from GitHub to its asset storage. Follow
/// a small number of redirects, but never allow a scheme change (including an
/// HTTPS downgrade). Asset requests carry no credentials.
pub(crate) fn download_client() -> Result<Client, UpdateError> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("hokan/", env!("CARGO_PKG_VERSION")))
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() >= ASSET_REDIRECT_LIMIT {
                attempt.error("too many release asset redirects")
            } else if !attempt.url().username().is_empty()
                || attempt.url().password().is_some()
                || attempt
                    .previous()
                    .last()
                    .is_some_and(|previous| previous.scheme() != attempt.url().scheme())
            {
                attempt.error("unsafe release asset redirect")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|_| UpdateError::Network)
}

/// Resolve the highest semver within the requested channel. The release list
/// distinguishes an empty channel from a missing repository (HTTP 404).
/// Paginate because publication order need not match semantic version order.
pub fn fetch_latest(
    channel: Channel,
    base: &str,
    repo: &str,
) -> Result<Option<ReleaseInfo>, UpdateError> {
    fetch_latest_with_mirrors(channel, base, repo, &mirror_templates(base))
}

fn fetch_latest_with_mirrors(
    channel: Channel,
    base: &str,
    repo: &str,
    mirrors: &[String],
) -> Result<Option<ReleaseInfo>, UpdateError> {
    let client = api_client()?;
    let base = base.trim_end_matches('/');
    block_on(async {
        let mut first_error = None;
        // Retry the complete listing on one source. Never mix pagination from
        // GitHub and a stale mirror, which could skip releases between pages.
        for template in std::iter::once(None).chain(mirrors.iter().map(Some)) {
            match fetch_from_source(&client, channel, base, repo, template).await {
                Ok(release) => return Ok(release),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        Err(first_error.unwrap_or(UpdateError::Network))
    })?
}

async fn fetch_from_source(
    client: &Client,
    channel: Channel,
    base: &str,
    repo: &str,
    template: Option<&String>,
) -> Result<Option<ReleaseInfo>, UpdateError> {
    let mut best = None;
    for page in 1..=MAX_RELEASE_PAGES {
        let primary = format!("{base}/repos/{repo}/releases?per_page=20&page={page}");
        let url = match template {
            Some(template) => mirror_url(&primary, template).ok_or(UpdateError::InvalidResponse)?,
            None => primary,
        };
        let mut releases: Vec<Release> = get_json(client, &url).await?;
        let last_page = releases.len() < 20;
        releases.extend(best.take());
        best = select_highest(releases, channel);
        if last_page {
            return best
                .map(|release| release_info(release, base, repo))
                .transpose();
        }
    }
    Err(UpdateError::InvalidResponse)
}

/// Parses a release tag (`v0.2.0` or `0.2.0-beta.1`) as semver; unparseable
/// tags (nightly aliases and the like) return `None` and are skipped.
pub(crate) fn parse_tag_version(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

/// The release-archive target triple for this build, matching the naming
/// scheme of `scripts/package-release.sh`.
pub(crate) fn target_triple() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        ("x86_64", "macos") => Some("x86_64-apple-darwin"),
        ("x86_64", "linux") => Some("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Some("aarch64-unknown-linux-gnu"),
        _ => None,
    }
}

/// Canonical archive file name for `version` on this platform.
pub(crate) fn archive_name(version: &Version) -> Result<String, UpdateError> {
    let target = target_triple().ok_or(UpdateError::UnsupportedPlatform)?;
    Ok(format!("hokan-{version}-{target}.tar.gz"))
}

/// Downloads a body with a hard size cap (mirrors the chunked read in
/// `ai::oauth` so a broken server cannot make us buffer unbounded data).
pub(crate) async fn download(
    client: &Client,
    url: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, UpdateError> {
    send(client.get(url), max_bytes).await
}

/// Download from GitHub first and then from configured relay templates. A
/// relay is only a transport: the caller must still verify the signed
/// SHA256SUMS before installing anything.
pub(crate) async fn download_with_fallback(
    client: &Client,
    url: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, UpdateError> {
    download_from_candidates(client, url, max_bytes, &mirror_templates(url)).await
}

async fn download_from_candidates(
    client: &Client,
    url: &str,
    max_bytes: usize,
    mirrors: &[String],
) -> Result<Vec<u8>, UpdateError> {
    let mut first_error = None;
    for candidate in candidate_urls(url, mirrors) {
        match download(client, &candidate, max_bytes).await {
            Ok(body) => return Ok(body),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    // Keep GitHub's rate-limit and proxy diagnostics if every relay fails.
    Err(first_error.unwrap_or(UpdateError::Network))
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &Client,
    url: &str,
) -> Result<T, UpdateError> {
    let body = send(
        client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json"),
        RESPONSE_BODY_MAX_BYTES,
    )
    .await?;
    serde_json::from_slice(&body).map_err(|_| UpdateError::InvalidResponse)
}

fn mirror_templates(primary: &str) -> Vec<String> {
    let Ok(url) = reqwest::Url::parse(primary) else {
        return Vec::new();
    };
    // Do not relay arbitrary/test endpoints, which may contain private data.
    if url.scheme() != "https" || !matches!(url.host_str(), Some("api.github.com" | "github.com")) {
        return Vec::new();
    }
    match std::env::var(MIRRORS_ENV) {
        Ok(value) => value
            .split([',', '\n'])
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|value| mirror_url(primary, value).is_some())
            .take(4)
            .map(str::to_owned)
            .collect(),
        Err(_) => (if url.host_str() == Some("api.github.com") {
            DEFAULT_API_MIRROR_TEMPLATES
        } else {
            DEFAULT_MIRROR_TEMPLATES
        })
        .iter()
        .map(|value| (*value).to_owned())
        .collect(),
    }
}

fn mirror_url(primary: &str, template: &str) -> Option<String> {
    let source = reqwest::Url::parse(primary).ok()?;
    let path = match source.query() {
        Some(query) => format!("{}?{query}", source.path()),
        None => source.path().to_owned(),
    };
    let candidate = if template.contains("{url}") {
        template.replace("{url}", primary)
    } else if template.contains("{path}") {
        template.replace("{path}", &path)
    } else {
        return None;
    };
    let url = reqwest::Url::parse(&candidate).ok()?;
    // A production HTTPS request can never fall back to HTTP. HTTP is only
    // used by callers injecting an HTTP base (local test servers).
    if url.scheme() != source.scheme()
        || !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(candidate)
}

fn candidate_urls(primary: &str, mirrors: &[String]) -> Vec<String> {
    let mut candidates = vec![primary.to_owned()];
    for template in mirrors.iter().take(4) {
        if let Some(candidate) = mirror_url(primary, template)
            && !candidates.contains(&candidate)
        {
            candidates.push(candidate);
        }
    }
    candidates
}

async fn send(request: reqwest::RequestBuilder, max_bytes: usize) -> Result<Vec<u8>, UpdateError> {
    let mut response = request.send().await.map_err(map_reqwest_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(http_error(status, response.headers()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(UpdateError::InvalidResponse);
    }
    let mut body = Vec::new();
    loop {
        let Some(chunk) = response.chunk().await.map_err(map_reqwest_error)? else {
            break;
        };
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(UpdateError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn http_error(status: StatusCode, headers: &HeaderMap) -> UpdateError {
    // Only retain numeric metadata, never response bodies or arbitrary header
    // text: an error may come from a proxy and contain credential-bearing URLs.
    let seconds = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
    };
    let retry_after = seconds("retry-after");
    let exhausted = seconds("x-ratelimit-remaining") == Some(0);
    if status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN && (exhausted || retry_after.is_some()))
    {
        let retry_after_secs = retry_after.or_else(|| {
            let reset = seconds("x-ratelimit-reset")?;
            let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
            Some(reset.saturating_sub(now))
        });
        UpdateError::RateLimited {
            status: status.as_u16(),
            retry_after_secs,
        }
    } else {
        UpdateError::Http(status.as_u16())
    }
}

fn map_reqwest_error(error: reqwest::Error) -> UpdateError {
    if error.is_timeout() {
        UpdateError::Timeout
    } else {
        UpdateError::Network
    }
}

/// Picks the release with the highest semver tag; prereleases order below
/// their release (`0.2.0` > `0.2.0-beta.2` > `0.2.0-beta.1`).
fn select_highest(releases: Vec<Release>, channel: Channel) -> Option<Release> {
    releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| parse_tag_version(&release.tag_name).map(|v| (v, release)))
        .filter(|(version, release)| {
            Channel::for_version(version) == channel
                && release.prerelease == (channel == Channel::Beta)
        })
        .max_by(|(left, _), (right, _)| left.cmp_precedence(right))
        .map(|(_, release)| release)
}

/// Resolves the platform archive and SHA256SUMS assets of one release.
fn release_info(release: Release, base: &str, repo: &str) -> Result<ReleaseInfo, UpdateError> {
    let version = parse_tag_version(&release.tag_name).ok_or(UpdateError::InvalidResponse)?;
    let archive = archive_name(&version)?;
    let find = |name: &str| {
        let mut matches = release.assets.iter().filter(|asset| asset.name == name);
        let asset = matches.next().ok_or(UpdateError::MissingAsset)?;
        if matches.next().is_some() {
            return Err(UpdateError::InvalidResponse);
        }
        if base == DEFAULT_API_BASE {
            let expected = format!(
                "https://github.com/{repo}/releases/download/{}/{name}",
                release.tag_name
            );
            if asset.browser_download_url != expected {
                return Err(UpdateError::InvalidResponse);
            }
        } else {
            // Injected bases can only serve assets from the same origin.
            let url = reqwest::Url::parse(&asset.browser_download_url)
                .map_err(|_| UpdateError::InvalidResponse)?;
            let origin = reqwest::Url::parse(base).map_err(|_| UpdateError::InvalidResponse)?;
            if url.origin() != origin.origin()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err(UpdateError::InvalidResponse);
            }
        }
        Ok(asset.browser_download_url.clone())
    };
    // SHA256 alone authenticates only the checksum response. Require the
    // detached signature asset before a release can become installable.
    let signature_url = find("SHA256SUMS.sig")?;
    Ok(ReleaseInfo {
        version,
        tag: release.tag_name.clone(),
        archive_url: find(&archive)?,
        checksums_url: find("SHA256SUMS")?,
        signature_url,
    })
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::test_support::{
        archive_asset, json_reply, raw_reply, redirect_reply, release_json, spawn_server,
    };

    #[test]
    fn release_asset_download_follows_same_scheme_redirects() {
        let (base, join) = spawn_server(2, move |path| match path {
            "/asset" => redirect_reply("{BASE}/download/asset"),
            "/download/asset" => raw_reply("200 OK", b"release asset".to_vec()),
            _ => raw_reply("404 Not Found", Vec::new()),
        });
        let client = download_client().expect("download client");
        let body = block_on(download(&client, &format!("{base}/asset"), 1024))
            .expect("runtime")
            .expect("redirected download");
        assert_eq!(body, b"release asset");
        join.join().expect("server thread");
    }

    #[test]
    fn mirror_templates_keep_direct_github_first_and_rewrite_the_full_url() {
        let candidates = candidate_urls(
            "https://api.github.com/repos/backrunner/hokan/releases?page=1",
            &[
                "https://mirror.example/{url}".to_owned(),
                "https://api-mirror.example{path}".to_owned(),
            ],
        );
        assert_eq!(
            candidates,
            vec![
                "https://api.github.com/repos/backrunner/hokan/releases?page=1",
                "https://mirror.example/https://api.github.com/repos/backrunner/hokan/releases?page=1",
                "https://api-mirror.example/repos/backrunner/hokan/releases?page=1",
            ]
        );
    }

    #[test]
    fn failed_check_tries_mirrors_in_order_and_direct_success_stops() {
        let (base, join) = spawn_server(3, |path| {
            if path.starts_with("/repos/") {
                raw_reply("503 Service Unavailable", Vec::new())
            } else if path.starts_with("/one/repos/") {
                raw_reply("200 OK", b"proxy login page".to_vec())
            } else {
                assert!(path.starts_with("/two/repos/"));
                json_reply("200 OK", serde_json::json!([fixture("v9.9.9")]))
            }
        });
        let mirrors = vec![format!("{base}/one{{path}}"), format!("{base}/two{{path}}")];
        let info = fetch_latest_with_mirrors(Channel::Stable, &base, "backrunner/hokan", &mirrors)
            .expect("fallback")
            .expect("release");
        assert_eq!(info.version, Version::new(9, 9, 9));
        join.join().expect("server");

        let (base, join) = spawn_server(1, |path| {
            assert!(path.starts_with("/repos/"), "must not contact mirror");
            json_reply("200 OK", serde_json::json!([]))
        });
        assert!(
            fetch_latest_with_mirrors(
                Channel::Stable,
                &base,
                "backrunner/hokan",
                &[format!("{base}/unused{{path}}")]
            )
            .expect("direct")
            .is_none()
        );
        join.join().expect("server");
    }

    #[test]
    fn failed_asset_download_tries_next_mirror_with_body_limit() {
        let (base, join) = spawn_server(3, |path| match path {
            "/asset" => raw_reply("502 Bad Gateway", Vec::new()),
            "/one/asset" => raw_reply("200 OK", vec![0; 1025]),
            "/two/asset" => raw_reply("200 OK", b"archive bytes".to_vec()),
            _ => panic!("unexpected request"),
        });
        let client = download_client().expect("client");
        let mirrors = vec![format!("{base}/one{{path}}"), format!("{base}/two{{path}}")];
        assert_eq!(
            block_on(download_from_candidates(
                &client,
                &format!("{base}/asset"),
                1024,
                &mirrors
            ))
            .expect("runtime")
            .expect("fallback"),
            b"archive bytes"
        );
        join.join().expect("server");
    }

    #[test]
    fn all_mirrors_failing_preserves_original_rate_limit() {
        let (base, join) = spawn_server(2, |path| {
            if path.starts_with("/repos/") {
                raw_reply("429 Too Many Requests", Vec::new()).with_header("Retry-After", "123")
            } else {
                raw_reply("404 Not Found", Vec::new())
            }
        });
        assert!(matches!(
            fetch_latest_with_mirrors(
                Channel::Beta,
                &base,
                "backrunner/hokan",
                &[format!("{base}/mirror{{path}}")]
            ),
            Err(UpdateError::RateLimited {
                retry_after_secs: Some(123),
                ..
            })
        ));
        join.join().expect("server");
    }

    #[test]
    fn metadata_cannot_redirect_downloads_to_another_repository_or_host() {
        for bad in [
            "http://github.com/backrunner/hokan/releases/download/v9.9.9/SHA256SUMS",
            "https://github.com/attacker/hokan/releases/download/v9.9.9/SHA256SUMS",
            "https://127.0.0.1/private",
            "https://github.com.evil.invalid/backrunner/hokan/releases/download/v9.9.9/SHA256SUMS",
        ] {
            let mut release = fixture("v9.9.9");
            for asset in release["assets"].as_array_mut().expect("assets") {
                let name = asset["name"].as_str().expect("name");
                asset["browser_download_url"] = serde_json::json!(format!(
                    "https://github.com/backrunner/hokan/releases/download/v9.9.9/{name}"
                ));
            }
            release["assets"][1]["browser_download_url"] = serde_json::json!(bad);
            let release: Release = serde_json::from_value(release).expect("release");
            assert!(matches!(
                release_info(release, DEFAULT_API_BASE, "backrunner/hokan"),
                Err(UpdateError::InvalidResponse)
            ));
        }
    }

    #[test]
    fn unsafe_mirror_templates_are_ignored_without_tls_downgrade() {
        let primary = "https://api.github.com/repos/backrunner/hokan/releases";
        for template in [
            "http://127.0.0.1/{url}",
            "http://mirror.example/{url}",
            "https://user:secret@mirror.example/{url}",
            "https://mirror.example/#fragment",
            "file:///tmp/{url}",
            "https://mirror.example",
        ] {
            assert!(mirror_url(primary, template).is_none());
        }
    }

    fn fixture(tag: &str) -> serde_json::Value {
        let version = tag.trim_start_matches('v');
        release_json(tag, &[archive_asset(version), "SHA256SUMS".to_owned()])
    }

    #[test]
    fn each_channel_selects_its_highest_semver_without_crossing_channels() {
        for (channel, expected) in [
            (Channel::Stable, "v0.2.0"),
            (Channel::Beta, "v0.2.0-beta.10"),
        ] {
            let (base, join) = spawn_server(1, |path| {
                assert_eq!(path, "/repos/backrunner/hokan/releases?per_page=20&page=1");
                let mut draft = fixture("v9.9.9-beta.1");
                draft["draft"] = serde_json::json!(true);
                let mut mislabeled = fixture("v9.9.9");
                mislabeled["prerelease"] = serde_json::json!(true);
                json_reply(
                    "200 OK",
                    serde_json::json!([
                        fixture("v0.2.0-beta.2"),
                        fixture("v0.1.0"),
                        fixture("v0.2.0-beta.10"),
                        fixture("v0.2.0"),
                        release_json("nightly", &[]),
                        draft,
                        mislabeled,
                    ]),
                )
            });
            let info = fetch_latest(channel, &base, "backrunner/hokan")
                .expect("lookup")
                .expect("release");
            assert_eq!(info.tag, expected);
            assert!(
                info.archive_url
                    .ends_with(&archive_asset(expected.trim_start_matches('v')))
            );
            assert!(info.checksums_url.ends_with("SHA256SUMS"));
            join.join().expect("server thread");
        }
    }

    #[test]
    fn empty_channel_is_not_an_http_error_or_a_cross_channel_fallback() {
        for (channel, releases) in [
            (Channel::Stable, vec![fixture("v0.2.0-beta.1")]),
            (Channel::Beta, vec![fixture("v0.2.0")]),
            (Channel::Stable, vec![]),
        ] {
            let (base, join) = spawn_server(1, move |_| {
                json_reply("200 OK", serde_json::json!(releases))
            });
            assert_eq!(
                fetch_latest(channel, &base, "backrunner/hokan").expect("lookup"),
                None
            );
            join.join().expect("server thread");
        }
    }

    #[test]
    fn selection_checks_later_pages_instead_of_assuming_publication_order() {
        for (channel, expected) in [
            (Channel::Stable, "v2.0.0"),
            (Channel::Beta, "v3.0.0-beta.1"),
        ] {
            let (base, join) = spawn_server(2, |path| {
                if path.ends_with("page=1") {
                    json_reply(
                        "200 OK",
                        serde_json::json!(vec![fixture("v1.0.0-beta.1"); 20]),
                    )
                } else {
                    assert!(path.ends_with("page=2"));
                    json_reply(
                        "200 OK",
                        serde_json::json!([fixture("v2.0.0"), fixture("v3.0.0-beta.1")]),
                    )
                }
            });
            assert_eq!(
                fetch_latest(channel, &base, "backrunner/hokan")
                    .expect("lookup")
                    .expect("release")
                    .tag,
                expected
            );
            join.join().expect("server thread");
        }
    }

    #[test]
    fn missing_archive_asset_is_an_error() {
        let (base, join) = spawn_server(1, |_| {
            json_reply(
                "200 OK",
                serde_json::json!([release_json("v0.2.0", &["SHA256SUMS".to_owned()])]),
            )
        });
        let error = fetch_latest(Channel::Stable, &base, "backrunner/hokan")
            .expect_err("missing archive must fail");
        assert_eq!(error.code(), "HK-UPD-ASSET");
        join.join().expect("server thread");
    }

    #[test]
    fn http_error_status_is_reported() {
        let (base, join) = spawn_server(1, move |_| {
            json_reply("404 Not Found", serde_json::json!({}))
        });
        let error =
            fetch_latest(Channel::Stable, &base, "backrunner/hokan").expect_err("404 must fail");
        assert!(matches!(error, UpdateError::Http(404)));
        assert_eq!(error.code(), "HK-UPD-HTTP");
        join.join().expect("server thread");

        let (base, join) = spawn_server(1, move |_| {
            json_reply("500 Internal Server Error", serde_json::json!({}))
        });
        let error =
            fetch_latest(Channel::Stable, &base, "backrunner/hokan").expect_err("500 must fail");
        assert!(matches!(error, UpdateError::Http(500)));
        join.join().expect("server thread");
    }

    #[test]
    fn rate_limits_are_distinguished_from_gateway_denials() {
        let reset = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs()
            + 3600;
        let (base, join) = spawn_server(1, move |_| {
            raw_reply("403 Forbidden", b"private response body".to_vec())
                .with_header("X-RateLimit-Remaining", "0")
                .with_header("X-RateLimit-Reset", reset.to_string())
        });
        let error = fetch_latest(Channel::Beta, &base, "backrunner/hokan")
            .expect_err("GitHub primary rate limit");
        assert!(matches!(
            error,
            UpdateError::RateLimited {
                status: 403,
                retry_after_secs: Some(1..=3600),
            }
        ));
        assert_eq!(error.code(), "HK-UPD-HTTP");
        assert!(!error.to_string().contains("private"));
        join.join().expect("server");

        for status in ["403 Forbidden", "429 Too Many Requests"] {
            let (base, join) = spawn_server(1, move |_| {
                raw_reply(status, Vec::new())
                    .with_header("Retry-After", "120")
                    .with_header("X-RateLimit-Reset", "0")
            });
            let error = fetch_latest(Channel::Beta, &base, "backrunner/hokan")
                .expect_err("secondary rate limit");
            assert!(matches!(
                error,
                UpdateError::RateLimited {
                    retry_after_secs: Some(120),
                    ..
                }
            ));
            join.join().expect("server");
        }

        let (base, join) = spawn_server(1, |_| {
            raw_reply(
                "403 Forbidden",
                b"https://user:secret@proxy.invalid".to_vec(),
            )
            .with_header("Retry-After", "https://user:secret@proxy.invalid")
            .with_header("X-RateLimit-Remaining", "invalid")
        });
        let error = fetch_latest(Channel::Beta, &base, "backrunner/hokan")
            .expect_err("gateway refusal without rate-limit evidence");
        assert!(matches!(error, UpdateError::Http(403)));
        assert!(!format!("{error:?}").contains("secret"));
        join.join().expect("server");
    }

    #[test]
    fn rate_limit_metadata_is_optional_and_expired_resets_do_not_underflow() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-reset", "invalid".parse().expect("header"));
        assert!(matches!(
            http_error(StatusCode::TOO_MANY_REQUESTS, &headers),
            UpdateError::RateLimited {
                retry_after_secs: None,
                ..
            }
        ));
        headers.insert("x-ratelimit-remaining", "0".parse().expect("header"));
        headers.insert("x-ratelimit-reset", "0".parse().expect("header"));
        assert!(matches!(
            http_error(StatusCode::FORBIDDEN, &headers),
            UpdateError::RateLimited {
                retry_after_secs: Some(0),
                ..
            }
        ));
        // Rate-limit headers on unrelated HTTP errors are not evidence of throttling.
        assert!(matches!(
            http_error(StatusCode::NOT_FOUND, &headers),
            UpdateError::Http(404)
        ));
    }

    #[test]
    fn tag_versions_parse_and_order_as_semver() {
        assert_eq!(
            parse_tag_version("v0.2.0-beta.2"),
            Version::parse("0.2.0-beta.2").ok()
        );
        assert_eq!(parse_tag_version("0.1.0"), Version::parse("0.1.0").ok());
        assert_eq!(parse_tag_version("nightly"), None);
        assert_eq!(parse_tag_version("v1"), None);

        let beta1 = Version::parse("0.2.0-beta.1").expect("beta1");
        let beta2 = Version::parse("0.2.0-beta.2").expect("beta2");
        let stable = Version::parse("0.2.0").expect("stable");
        assert!(beta2 > beta1, "0.2.0-beta.2 > 0.2.0-beta.1");
        assert!(stable > beta2, "0.2.0 > 0.2.0-beta.N");
    }

    #[test]
    fn draft_releases_are_not_selected() {
        let released: Release =
            serde_json::from_value(release_json("v1.0.0-beta.1", &[])).expect("release");
        let mut draft = release_json("v9.9.9", &[]);
        draft["draft"] = serde_json::json!(true);
        let draft = serde_json::from_value(draft).expect("draft");
        assert_eq!(
            select_highest(vec![released, draft], Channel::Beta)
                .expect("published release")
                .tag_name,
            "v1.0.0-beta.1"
        );
    }

    #[test]
    fn archive_name_matches_release_packaging_scheme() {
        let version = Version::parse("0.2.0").expect("version");
        let name = archive_name(&version).expect("archive name");
        assert!(name.starts_with("hokan-0.2.0-"));
        assert!(name.ends_with(".tar.gz"));
        assert!(target_triple().is_some(), "test targets are supported");
    }
}
