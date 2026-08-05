//! TTL cache for update checks at `{state_dir}/update-check.json`.
//!
//! Keeps background checks well under the unauthenticated GitHub API rate
//! limit. Writes are atomic (tempfile + persist + fsync, file 0600, parent
//! 0700) mirroring `config::credentials::io`; corrupt files and implausible
//! timestamps (clock rollback) read as stale, never as errors.

use std::{
    fs,
    io::Write,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use super::UpdateError;

/// Grace for small clock adjustments; beyond it a future-dated file means
/// the clock jumped backwards and the entry cannot be trusted.
const CLOCK_SKEW_TOLERANCE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CheckCache {
    pub last_check_epoch: u64,
    pub channel: String,
    pub latest_known: String,
}

pub(crate) fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

impl CheckCache {
    /// Loads the cache file; a missing, unreadable, or corrupt file is
    /// simply absent (treated as stale by callers).
    pub(crate) fn load(path: &Path) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Freshness against an explicit `now` so tests control the clock.
    /// Both the file mtime and the recorded epoch must be plausible: a file
    /// from the future means the clock rolled back and the entry is stale.
    pub(crate) fn is_fresh(&self, mtime: SystemTime, ttl: Duration, now: SystemTime) -> bool {
        if mtime > now + CLOCK_SKEW_TOLERANCE {
            return false;
        }
        let Some(now_epoch) = now.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()) else {
            return false;
        };
        let Some(elapsed) = now_epoch.checked_sub(self.last_check_epoch) else {
            return false;
        };
        elapsed < ttl.as_secs()
    }

    /// Loads the cache only when it is still within `ttl`.
    pub(crate) fn read_fresh(path: &Path, ttl: Duration) -> Option<Self> {
        let mtime = fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()?;
        let cache = Self::load(path)?;
        cache
            .is_fresh(mtime, ttl, SystemTime::now())
            .then_some(cache)
    }

    /// Atomic write: tempfile in the target directory, 0600 file under a
    /// 0700 parent, fsync of file and directory.
    pub(crate) fn write(&self, path: &Path) -> Result<(), UpdateError> {
        let parent = path.parent().ok_or_else(|| {
            UpdateError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cache path has no parent directory",
            ))
        })?;
        fs::create_dir_all(parent)?;
        set_private_directory_permissions(parent)?;
        let rendered = serde_json::to_string(self).map_err(|_| UpdateError::InvalidResponse)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".update-check.")
            .tempfile_in(parent)?;
        set_private_file_permissions(temporary.as_file())?;
        temporary.write_all(rendered.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        set_private_file_permissions(&fs::File::open(path)?)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_: &Path) -> Result<(), UpdateError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(file: &fs::File) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_: &fs::File) -> Result<(), UpdateError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(last_check_epoch: u64) -> CheckCache {
        CheckCache {
            last_check_epoch,
            channel: "stable".to_owned(),
            latest_known: "0.2.0".to_owned(),
        }
    }

    #[test]
    fn write_then_read_fresh_hits() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("state/update-check.json");
        entry(now_epoch_secs()).write(&path).expect("write cache");
        let fresh =
            CheckCache::read_fresh(&path, Duration::from_secs(1_800)).expect("cache must be fresh");
        assert_eq!(fresh.latest_known, "0.2.0");
    }

    #[cfg(unix)]
    #[test]
    fn written_cache_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("state/update-check.json");
        entry(now_epoch_secs()).write(&path).expect("write cache");
        let mode = fs::metadata(&path)
            .expect("cache metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let parent_mode = fs::metadata(path.parent().expect("parent"))
            .expect("parent metadata")
            .permissions()
            .mode();
        assert_eq!(parent_mode & 0o777, 0o700);
    }

    #[test]
    fn stale_after_ttl_expires() {
        let now = SystemTime::now();
        let cache = entry(now_epoch_secs().saturating_sub(1_801));
        assert!(!cache.is_fresh(now, Duration::from_secs(1_800), now));
        let cache = entry(now_epoch_secs());
        assert!(cache.is_fresh(now, Duration::from_secs(1_800), now));
    }

    #[test]
    fn corrupt_file_reads_as_stale() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("update-check.json");
        fs::write(&path, "{ not json").expect("corrupt cache");
        assert!(CheckCache::load(&path).is_none());
        assert!(CheckCache::read_fresh(&path, Duration::from_secs(1_800)).is_none());
    }

    #[test]
    fn future_mtime_reads_as_stale() {
        let now = SystemTime::now();
        let cache = entry(now_epoch_secs());
        let future = now + Duration::from_secs(3_600);
        assert!(!cache.is_fresh(future, Duration::from_secs(1_800), now));
        // A small adjustment within the skew tolerance stays valid.
        let slight = now + Duration::from_secs(30);
        assert!(cache.is_fresh(slight, Duration::from_secs(1_800), now));
    }

    #[test]
    fn future_check_epoch_reads_as_stale() {
        let now = SystemTime::now();
        let cache = entry(now_epoch_secs().saturating_add(3_600));
        assert!(!cache.is_fresh(now, Duration::from_secs(1_800), now));
    }
}
