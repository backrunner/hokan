//! Self-update: TTL-cached release checks and verified, atomic upgrades.
//!
//! One implementation serves both the manual `hokan upgrade` command and the
//! detached `--auto` background check: query GitHub Releases (with a TTL
//! cache so background checks stay under the unauthenticated rate limit),
//! verify the archive against the published SHA256SUMS, smoke-test the new
//! binary, and atomically rename it over the current executable.
//!
//! Every entry point returns `Result`; nothing in this module panics or
//! exits the process, so `--auto` callers can log and drop failures quietly.

mod api;
mod cache;
mod install;
#[cfg(test)]
pub(crate) mod test_support;

use std::{fmt, path::PathBuf, time::Duration};

use semver::Version;
use thiserror::Error;

pub use api::{ReleaseInfo, fetch_latest};

/// Production GitHub API base; injectable through [`UpgradePaths`] for tests.
pub const DEFAULT_API_BASE: &str = "https://api.github.com";
/// Repository that publishes the release archives.
pub const DEFAULT_REPO: &str = "backrunner/hokan";
/// Default check interval; matches `[update].interval_secs` in the config.
pub const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(1_800);

/// Release channel to track.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Channel {
    Stable,
    Beta,
}

impl Channel {
    pub fn parse(value: &str) -> Result<Self, UpdateError> {
        match value {
            "stable" => Ok(Self::Stable),
            "beta" => Ok(Self::Beta),
            _ => Err(UpdateError::InvalidChannel),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Options for one upgrade run, shared by manual and `--auto` invocations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpgradeOptions {
    pub channel: Channel,
    /// Only report what is available; never download or install.
    pub check_only: bool,
    /// Reinstall even when the latest version equals the current one, and
    /// ignore the TTL cache.
    pub force: bool,
    /// Detached background check: fresh TTL cache exits immediately and the
    /// caller logs failures quietly instead of surfacing them.
    pub auto: bool,
    /// TTL for the "nothing newer" cache short-circuit, from
    /// `[update].interval_secs` (see [`DEFAULT_CHECK_INTERVAL`]).
    pub interval_secs: u64,
}

/// Filesystem and network locations an upgrade run operates on. Tests point
/// every field at a tempdir and a loopback mock server.
#[derive(Clone, Debug)]
pub struct UpgradePaths {
    /// The running executable to replace.
    pub current_exe: PathBuf,
    /// XDG state directory; the TTL cache lives at `update-check.json` inside.
    pub state_dir: PathBuf,
    /// XDG cache directory; downloads stage under `downloads/` inside.
    pub cache_dir: PathBuf,
    /// GitHub API base URL (`https://api.github.com` in production).
    pub api_base: String,
    /// `owner/name` of the releases repository.
    pub repo: String,
}

impl UpgradePaths {
    /// Production endpoints; the directories come from `ConfigPaths`.
    #[must_use]
    pub fn production(current_exe: PathBuf, state_dir: PathBuf, cache_dir: PathBuf) -> Self {
        Self {
            current_exe,
            state_dir,
            cache_dir,
            api_base: DEFAULT_API_BASE.to_owned(),
            repo: DEFAULT_REPO.to_owned(),
        }
    }
}

/// Snapshot of the TTL check cache, regardless of freshness; `hokan doctor`
/// reports what the last check (background or manual) recorded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedCheck {
    pub channel: String,
    pub latest_known: String,
}

/// Reads the last recorded check from the cache file in `state_dir`; a
/// missing or corrupt file is simply absent.
#[must_use]
pub fn read_cached_check(state_dir: &std::path::Path) -> Option<CachedCheck> {
    let entry = cache::CheckCache::load(&state_dir.join("update-check.json"))?;
    Some(CachedCheck {
        channel: entry.channel,
        latest_known: entry.latest_known,
    })
}

pub(crate) use install::directory_writable;

/// What an upgrade run did or found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpgradeOutcome {
    /// Nothing newer exists (or a fresh cache already said so).
    AlreadyCurrent { version: Version },
    /// `--check` report: no download happened.
    Checked { current: Version, latest: Version },
    /// The executable was replaced.
    Upgraded { from: Version, to: Version },
    /// The executable's directory is not writable (package-manager install);
    /// the user must upgrade through their package manager.
    NotWritable { path: PathBuf },
}

/// Update failures with stable `HK-UPD-*` codes. `Display` messages never
/// embed tokens or credential-bearing URLs.
#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("unknown update channel (expected stable or beta)")]
    InvalidChannel,
    #[error("update request failed")]
    Network,
    #[error("update request timed out")]
    Timeout,
    #[error("update server rejected the request (HTTP {0})")]
    Http(u16),
    #[error("update server response was invalid")]
    InvalidResponse,
    #[error("release does not provide an archive for this platform")]
    MissingAsset,
    #[error("downloaded archive failed the SHA256 checksum")]
    ChecksumMismatch,
    #[error("downloaded binary failed the smoke test")]
    SmokeTest,
    #[error("this platform has no release archive naming scheme")]
    UnsupportedPlatform,
    #[error("I/O error during update: {0}")]
    Io(#[from] std::io::Error),
}

impl UpdateError {
    /// Stable machine-readable code for logs and doctor output.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidChannel => "HK-UPD-CHANNEL",
            Self::Network => "HK-UPD-NET",
            Self::Timeout => "HK-UPD-TIMEOUT",
            Self::Http(_) => "HK-UPD-HTTP",
            Self::InvalidResponse => "HK-UPD-JSON",
            Self::MissingAsset => "HK-UPD-ASSET",
            Self::ChecksumMismatch => "HK-UPD-HASH",
            Self::SmokeTest => "HK-UPD-SMOKE",
            Self::UnsupportedPlatform => "HK-UPD-PLATFORM",
            Self::Io(_) => "HK-UPD-IO",
        }
    }
}

/// Runs one upgrade pass: consult the TTL cache, resolve the latest release
/// for the configured channel, and install it when it is newer.
///
/// Never downgrades. A fresh cache that already recorded "nothing newer"
/// short-circuits before any network traffic; `--check` and `--force`
/// always hit the network.
pub fn run_upgrade(
    options: &UpgradeOptions,
    paths: &UpgradePaths,
) -> Result<UpgradeOutcome, UpdateError> {
    let current =
        Version::parse(env!("CARGO_PKG_VERSION")).map_err(|_| UpdateError::InvalidResponse)?;
    let cache_path = paths.state_dir.join("update-check.json");

    if !options.force
        && !options.check_only
        && let Some(entry) =
            cache::CheckCache::read_fresh(&cache_path, Duration::from_secs(options.interval_secs))
        && entry.channel == options.channel.as_str()
        && let Ok(latest_known) = Version::parse(&entry.latest_known)
        && latest_known <= current
    {
        return Ok(UpgradeOutcome::AlreadyCurrent { version: current });
    }

    let release = fetch_latest(options.channel, &paths.api_base, &paths.repo)?;

    // Best-effort: a failed cache write must not fail the upgrade itself.
    let _ = cache::CheckCache {
        last_check_epoch: cache::now_epoch_secs(),
        channel: options.channel.as_str().to_owned(),
        latest_known: release.version.to_string(),
    }
    .write(&cache_path);

    let latest = release.version.clone();
    if options.check_only {
        return Ok(UpgradeOutcome::Checked { current, latest });
    }
    if latest < current || (latest == current && !options.force) {
        return Ok(UpgradeOutcome::AlreadyCurrent { version: current });
    }
    install::download_and_install(&release, paths, &current)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::update::test_support::{archive_asset, json_reply, release_json, spawn_server};

    fn paths(root: &Path, api_base: &str) -> UpgradePaths {
        UpgradePaths {
            current_exe: root.join("bin/hokan"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            api_base: api_base.to_owned(),
            repo: "backrunner/hokan".to_owned(),
        }
    }

    fn options() -> UpgradeOptions {
        UpgradeOptions {
            channel: Channel::Stable,
            check_only: false,
            force: false,
            auto: false,
            interval_secs: DEFAULT_CHECK_INTERVAL.as_secs(),
        }
    }

    #[test]
    fn channel_parse_display_roundtrip() {
        assert_eq!(Channel::parse("stable").ok(), Some(Channel::Stable));
        assert_eq!(Channel::parse("beta").ok(), Some(Channel::Beta));
        assert!(matches!(
            Channel::parse("nightly"),
            Err(UpdateError::InvalidChannel)
        ));
        assert_eq!(Channel::Stable.to_string(), "stable");
        assert_eq!(Channel::Beta.to_string(), "beta");
        assert_eq!(UpdateError::InvalidChannel.code(), "HK-UPD-CHANNEL");
    }

    #[test]
    fn check_only_reports_latest_without_installing() {
        let root = tempfile::tempdir().expect("tempdir");
        let (base, join) = spawn_server(1, move |path| {
            assert!(path.starts_with("/repos/backrunner/hokan/releases/latest"));
            json_reply(
                "200 OK",
                release_json("v9.9.9", &[archive_asset("9.9.9"), "SHA256SUMS".to_owned()]),
            )
        });
        let mut opts = options();
        opts.check_only = true;
        let outcome = run_upgrade(&opts, &paths(root.path(), &base)).expect("check run");
        assert_eq!(
            outcome,
            UpgradeOutcome::Checked {
                current: Version::parse("0.1.0").expect("current"),
                latest: Version::parse("9.9.9").expect("latest"),
            }
        );
        join.join().expect("server thread");
        // The successful fetch refreshed the TTL cache.
        let cache = std::fs::read_to_string(root.path().join("state/update-check.json"))
            .expect("cache file");
        assert!(cache.contains("\"latest_known\":\"9.9.9\""));
        // --check must not have touched the (absent) executable.
        assert!(!root.path().join("bin").exists());
    }

    #[test]
    fn older_latest_release_reports_already_current() {
        let root = tempfile::tempdir().expect("tempdir");
        let (base, join) = spawn_server(1, move |_| {
            json_reply(
                "200 OK",
                release_json("v0.0.1", &[archive_asset("0.0.1"), "SHA256SUMS".to_owned()]),
            )
        });
        let outcome = run_upgrade(&options(), &paths(root.path(), &base)).expect("run");
        assert_eq!(
            outcome,
            UpgradeOutcome::AlreadyCurrent {
                version: Version::parse("0.1.0").expect("current"),
            }
        );
        join.join().expect("server thread");
    }

    #[test]
    fn fresh_cache_short_circuits_without_network() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("state")).expect("state dir");
        std::fs::write(
            root.path().join("state/update-check.json"),
            format!(
                "{{\"last_check_epoch\":{},\"channel\":\"stable\",\"latest_known\":\"0.0.1\"}}",
                cache::now_epoch_secs()
            ),
        )
        .expect("seed cache");
        // No server: any network traffic would fail the run. Auto mode with a
        // fresh cache exits quietly the same way.
        let mut auto = options();
        auto.auto = true;
        let outcome =
            run_upgrade(&auto, &paths(root.path(), "http://127.0.0.1:1")).expect("cached run");
        assert!(matches!(outcome, UpgradeOutcome::AlreadyCurrent { .. }));

        // A fresh cache for another channel must not short-circuit.
        let mut opts = options();
        opts.channel = Channel::Beta;
        assert!(run_upgrade(&opts, &paths(root.path(), "http://127.0.0.1:1")).is_err());
    }

    #[test]
    fn stale_cache_is_ignored() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("state")).expect("state dir");
        // An expired cache never short-circuits; the run must hit the network.
        std::fs::write(
            root.path().join("state/update-check.json"),
            "{\"last_check_epoch\":1,\"channel\":\"stable\",\"latest_known\":\"0.0.1\"}",
        )
        .expect("seed stale cache");
        assert!(run_upgrade(&options(), &paths(root.path(), "http://127.0.0.1:1")).is_err());
    }
}
