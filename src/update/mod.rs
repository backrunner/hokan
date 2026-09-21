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
mod local_io;
#[cfg(test)]
pub(crate) mod test_support;

use std::{fmt, path::PathBuf, time::Duration};

use semver::Version;
use thiserror::Error;

pub use api::{ReleaseInfo, fetch_latest};
pub(crate) use api::{block_on, download, download_client};

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
    /// The running binary owns the default channel, including after replacement.
    #[must_use]
    pub const fn current() -> Self {
        if env!("CARGO_PKG_VERSION_PRE").is_empty() {
            Self::Stable
        } else {
            Self::Beta
        }
    }

    #[must_use]
    pub fn for_version(version: &Version) -> Self {
        if version.pre.is_empty() {
            Self::Stable
        } else {
            Self::Beta
        }
    }

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

pub(crate) use install::{directory_writable, managed_install};

/// Build metadata does not change semver precedence. `force` permits an
/// equal-version reinstall, never a downgrade, including across channels.
pub(crate) fn should_install(current: &Version, target: &Version, force: bool) -> bool {
    match target.cmp_precedence(current) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Equal => force,
        std::cmp::Ordering::Less => false,
    }
}

/// What an upgrade run did or found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpgradeOutcome {
    /// The repository exists but this channel has no published release yet.
    NoRelease { channel: Channel },
    /// A background attempt is already running or its retry interval has not elapsed.
    Deferred,
    /// Nothing newer exists (or a fresh cache already said so).
    AlreadyCurrent { version: Version },
    /// `--check` report: no download happened.
    Checked { current: Version, latest: Version },
    /// The executable was replaced.
    Upgraded { from: Version, to: Version },
    /// The executable's directory is not writable (package-manager install);
    /// the user must upgrade through their package manager.
    NotWritable { path: PathBuf },
    /// Known package-manager layout, even when its files are user-writable.
    ManagedInstall { path: PathBuf },
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
    #[error("update server rate limit exceeded (HTTP {status})")]
    RateLimited {
        status: u16,
        retry_after_secs: Option<u64>,
    },
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
    #[error("another update is holding the installation lock; retry later")]
    Busy,
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
            Self::Http(_) | Self::RateLimited { .. } => "HK-UPD-HTTP",
            Self::InvalidResponse => "HK-UPD-JSON",
            Self::MissingAsset => "HK-UPD-ASSET",
            Self::ChecksumMismatch => "HK-UPD-HASH",
            Self::SmokeTest => "HK-UPD-SMOKE",
            Self::UnsupportedPlatform => "HK-UPD-PLATFORM",
            Self::Busy => "HK-UPD-BUSY",
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
        && !should_install(&current, &latest_known, false)
    {
        return Ok(UpgradeOutcome::AlreadyCurrent { version: current });
    }

    // Cache hits are not attempts: otherwise opening a shell just before
    // cache expiry postpones the next network check by another full interval.
    let _auto_lock = if options.auto && !options.force && !options.check_only {
        let Some(lock) = cache::begin_auto_attempt(paths, options, &current)? else {
            return Ok(UpgradeOutcome::Deferred);
        };
        Some(lock)
    } else {
        None
    };

    let Some(release) = check_release(options.channel, paths)? else {
        return Ok(UpgradeOutcome::NoRelease {
            channel: options.channel,
        });
    };

    let latest = release.version.clone();
    if options.check_only {
        return Ok(UpgradeOutcome::Checked { current, latest });
    }
    if !should_install(&current, &latest, options.force) {
        return Ok(UpgradeOutcome::AlreadyCurrent { version: current });
    }
    install::download_and_install(&release, paths, &current)
}

pub(crate) fn check_release(
    channel: Channel,
    paths: &UpgradePaths,
) -> Result<Option<ReleaseInfo>, UpdateError> {
    let Some(release) = fetch_latest(channel, &paths.api_base, &paths.repo)? else {
        return Ok(None);
    };
    // A cache write failure must not prevent a manual upgrade.
    let _ = cache::CheckCache {
        last_check_epoch: cache::now_epoch_secs(),
        channel: channel.as_str().to_owned(),
        latest_known: release.version.to_string(),
    }
    .write(&paths.state_dir.join("update-check.json"));
    Ok(Some(release))
}

pub(crate) use install::download_and_install as install_checked_release;

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
    fn cross_channel_upgrades_follow_semver_and_the_installed_version_channel() {
        for (from, to, newer, channel) in [
            ("0.1.0-beta.9", "0.1.0-beta.13", true, Channel::Beta),
            ("0.1.0-beta.13", "0.1.0", true, Channel::Stable),
            ("0.1.0", "0.2.0-beta.1", true, Channel::Beta),
            ("0.1.0", "0.1.0-beta.14", false, Channel::Beta),
            ("0.2.0-beta.1", "0.1.0", false, Channel::Stable),
        ] {
            let current = Version::parse(from).expect("from");
            let target = Version::parse(to).expect("to");
            assert_eq!(
                should_install(&current, &target, false),
                newer,
                "{from} -> {to}"
            );
            assert_eq!(
                should_install(&current, &target, true),
                newer,
                "force {from} -> {to}"
            );
            assert_eq!(Channel::for_version(&target), channel);
        }
        let current = Version::parse("1.0.0+one").expect("current");
        let target = Version::parse("1.0.0+two").expect("target");
        assert!(!should_install(&current, &target, false));
        assert!(should_install(&current, &target, true));
        assert_eq!(
            Channel::current(),
            Channel::for_version(&Version::parse(env!("CARGO_PKG_VERSION")).expect("build"))
        );
    }

    #[test]
    fn empty_channel_is_a_successful_noop_and_background_retries_are_throttled() {
        let root = tempfile::tempdir().expect("tempdir");
        let (base, join) = spawn_server(1, |_| json_reply("200 OK", serde_json::json!([])));
        let paths = paths(root.path(), &base);
        let mut options = options();
        options.auto = true;
        assert_eq!(
            run_upgrade(&options, &paths).expect("empty channel"),
            UpgradeOutcome::NoRelease {
                channel: Channel::Stable
            }
        );
        assert_eq!(
            run_upgrade(&options, &paths).expect("throttled"),
            UpgradeOutcome::Deferred
        );
        assert!(!paths.current_exe.exists());
        join.join().expect("server");
    }

    #[test]
    fn check_only_reports_latest_without_installing() {
        let root = tempfile::tempdir().expect("tempdir");
        let (base, join) = spawn_server(1, move |path| {
            assert!(path.starts_with("/repos/backrunner/hokan/releases?"));
            json_reply(
                "200 OK",
                serde_json::json!([release_json(
                    "v9.9.9",
                    &[archive_asset("9.9.9"), "SHA256SUMS".to_owned()]
                )]),
            )
        });
        let mut opts = options();
        opts.check_only = true;
        let outcome = run_upgrade(&opts, &paths(root.path(), &base)).expect("check run");
        assert_eq!(
            outcome,
            UpgradeOutcome::Checked {
                current: Version::parse(env!("CARGO_PKG_VERSION")).expect("current"),
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
                serde_json::json!([release_json(
                    "v0.0.1",
                    &[archive_asset("0.0.1"), "SHA256SUMS".to_owned()]
                )]),
            )
        });
        let outcome = run_upgrade(&options(), &paths(root.path(), &base)).expect("run");
        assert_eq!(
            outcome,
            UpgradeOutcome::AlreadyCurrent {
                version: Version::parse(env!("CARGO_PKG_VERSION")).expect("current"),
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
        assert!(
            !root.path().join("state/update-auto-attempt.json").exists(),
            "a cache hit must not postpone the next network attempt"
        );

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

    #[test]
    fn failed_background_attempts_are_throttled_but_manual_checks_can_retry() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = paths(root.path(), "http://127.0.0.1:1");
        let mut options = options();
        options.auto = true;
        assert!(run_upgrade(&options, &paths).is_err());
        assert_eq!(
            run_upgrade(&options, &paths).expect("defer retry"),
            UpgradeOutcome::Deferred
        );
        options.check_only = true;
        assert!(run_upgrade(&options, &paths).is_err());
        options.check_only = false;
        options.channel = Channel::Beta;
        assert!(
            run_upgrade(&options, &paths).is_err(),
            "a channel change can retry immediately"
        );
    }
}
