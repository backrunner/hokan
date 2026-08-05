//! GitHub Releases queries for update checks.
//!
//! The base URL is injectable so tests run against a loopback mock server.
//! The reqwest client is async (matching the rest of the crate), so every
//! public entry point drives its future on a private current-thread runtime,
//! the same pattern as `ai::oauth`.

use std::time::Duration;

use reqwest::{Client, redirect::Policy};
use semver::Version;
use serde::Deserialize;

use super::{Channel, UpdateError};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on release-metadata bodies; real replies are a few KiB of JSON.
const RESPONSE_BODY_MAX_BYTES: usize = 1024 * 1024;

/// One resolved release: version plus the download URLs for this platform.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseInfo {
    pub version: Version,
    pub tag: String,
    pub archive_url: String,
    pub checksums_url: String,
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

/// Shared client for API calls and archive downloads: fixed timeouts, no
/// redirects, and a `User-Agent` identifying hokan (GitHub requires one).
pub(crate) fn http_client() -> Result<Client, UpdateError> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("hokan/", env!("CARGO_PKG_VERSION")))
        .redirect(Policy::none())
        .build()
        .map_err(|_| UpdateError::Network)
}

/// Resolves the newest release for `channel`. Beta takes the highest semver
/// across recent releases (prereleases included) and the latest stable, so
/// beta users never fall behind a newer stable release.
pub fn fetch_latest(channel: Channel, base: &str, repo: &str) -> Result<ReleaseInfo, UpdateError> {
    let client = http_client()?;
    let base = base.trim_end_matches('/');
    block_on(async {
        match channel {
            Channel::Stable => {
                let url = format!("{base}/repos/{repo}/releases/latest");
                let release: Release = get_json(&client, &url).await?;
                release_info(release)
            }
            Channel::Beta => {
                let url = format!("{base}/repos/{repo}/releases?per_page=20");
                let mut releases: Vec<Release> = get_json(&client, &url).await?;
                // Compare with the latest stable and keep the max. A failed
                // stable lookup degrades to beta-only instead of failing.
                let stable_url = format!("{base}/repos/{repo}/releases/latest");
                if let Ok(stable) = get_json::<Release>(&client, &stable_url).await {
                    releases.push(stable);
                }
                let best = select_highest(releases)?;
                release_info(best)
            }
        }
    })?
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

async fn send(request: reqwest::RequestBuilder, max_bytes: usize) -> Result<Vec<u8>, UpdateError> {
    let mut response = request.send().await.map_err(map_reqwest_error)?;
    let status = response.status();
    if !status.is_success() {
        return Err(UpdateError::Http(status.as_u16()));
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

fn map_reqwest_error(error: reqwest::Error) -> UpdateError {
    if error.is_timeout() {
        UpdateError::Timeout
    } else {
        UpdateError::Network
    }
}

/// Picks the release with the highest semver tag; prereleases order below
/// their release (`0.2.0` > `0.2.0-beta.2` > `0.2.0-beta.1`).
fn select_highest(releases: Vec<Release>) -> Result<Release, UpdateError> {
    releases
        .into_iter()
        .filter_map(|release| parse_tag_version(&release.tag_name).map(|v| (v, release)))
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, release)| release)
        .ok_or(UpdateError::InvalidResponse)
}

/// Resolves the platform archive and SHA256SUMS assets of one release.
fn release_info(release: Release) -> Result<ReleaseInfo, UpdateError> {
    let version = parse_tag_version(&release.tag_name).ok_or(UpdateError::InvalidResponse)?;
    let archive = archive_name(&version)?;
    let find = |name: &str| {
        release
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .map(|asset| asset.browser_download_url.clone())
            .ok_or(UpdateError::MissingAsset)
    };
    Ok(ReleaseInfo {
        version,
        tag: release.tag_name,
        archive_url: find(&archive)?,
        checksums_url: find("SHA256SUMS")?,
    })
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
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
    use crate::update::test_support::{archive_asset, json_reply, release_json, spawn_server};

    #[test]
    fn stable_channel_parses_latest_release() {
        let (base, join) = spawn_server(1, move |path| {
            assert_eq!(path, "/repos/backrunner/hokan/releases/latest");
            json_reply(
                "200 OK",
                release_json("v0.2.0", &[archive_asset("0.2.0"), "SHA256SUMS".to_owned()]),
            )
        });
        let info = fetch_latest(Channel::Stable, &base, "backrunner/hokan").expect("latest");
        assert_eq!(info.version, Version::parse("0.2.0").expect("version"));
        assert_eq!(info.tag, "v0.2.0");
        assert!(info.archive_url.ends_with(&archive_asset("0.2.0")));
        assert!(info.checksums_url.ends_with("SHA256SUMS"));
        join.join().expect("server thread");
    }

    #[test]
    fn beta_channel_picks_highest_semver_including_prereleases() {
        let (base, join) = spawn_server(2, move |path| {
            if path.starts_with("/repos/backrunner/hokan/releases?") {
                let beta2 = archive_asset("0.2.0-beta.2");
                json_reply(
                    "200 OK",
                    serde_json::json!([
                        release_json(
                            "v0.2.0-beta.1",
                            &[archive_asset("0.2.0-beta.1"), "SHA256SUMS".to_owned()]
                        ),
                        release_json("v0.2.0-beta.2", &[beta2, "SHA256SUMS".to_owned()]),
                        release_json("nightly", &[]),
                    ]),
                )
            } else {
                json_reply(
                    "200 OK",
                    release_json("v0.1.0", &[archive_asset("0.1.0"), "SHA256SUMS".to_owned()]),
                )
            }
        });
        let info = fetch_latest(Channel::Beta, &base, "backrunner/hokan").expect("beta latest");
        assert_eq!(info.tag, "v0.2.0-beta.2");
        assert_eq!(
            info.version,
            Version::parse("0.2.0-beta.2").expect("version")
        );
        join.join().expect("server thread");
    }

    #[test]
    fn beta_channel_falls_back_to_newer_stable() {
        let (base, join) = spawn_server(2, move |path| {
            if path.starts_with("/repos/backrunner/hokan/releases?") {
                json_reply(
                    "200 OK",
                    serde_json::json!([release_json(
                        "v0.2.0-beta.1",
                        &[archive_asset("0.2.0-beta.1"), "SHA256SUMS".to_owned()]
                    ),]),
                )
            } else {
                json_reply(
                    "200 OK",
                    release_json("v0.2.0", &[archive_asset("0.2.0"), "SHA256SUMS".to_owned()]),
                )
            }
        });
        let info = fetch_latest(Channel::Beta, &base, "backrunner/hokan").expect("beta latest");
        assert_eq!(info.tag, "v0.2.0");
        assert_eq!(info.version, Version::parse("0.2.0").expect("version"));
        join.join().expect("server thread");
    }

    #[test]
    fn missing_archive_asset_is_an_error() {
        let (base, join) = spawn_server(1, move |_| {
            json_reply(
                "200 OK",
                release_json(
                    "v0.2.0",
                    &["SHA256SUMS".to_owned(), "sbom.spdx.json".to_owned()],
                ),
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
    fn archive_name_matches_release_packaging_scheme() {
        let version = Version::parse("0.2.0").expect("version");
        let name = archive_name(&version).expect("archive name");
        assert!(name.starts_with("hokan-0.2.0-"));
        assert!(name.ends_with(".tar.gz"));
        assert!(target_triple().is_some(), "test targets are supported");
    }
}
