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
const ASSET_REDIRECT_LIMIT: usize = 5;
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
            } else if attempt
                .previous()
                .last()
                .is_some_and(|previous| previous.scheme() != attempt.url().scheme())
            {
                attempt.error("release asset redirect changed URL scheme")
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
    let client = api_client()?;
    let base = base.trim_end_matches('/');
    block_on(async {
        let mut best = None;
        for page in 1.. {
            let url = format!("{base}/repos/{repo}/releases?per_page=20&page={page}");
            let mut releases: Vec<Release> = get_json(&client, &url).await?;
            let last_page = releases.len() < 20;
            releases.extend(best.take());
            best = select_highest(releases, channel);
            if last_page {
                break;
            }
        }
        best.map(release_info).transpose()
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
