use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use nix::fcntl::OFlag;
use serde::{Deserialize, Serialize};

use crate::shell::ShellKind;

mod lock;
mod read;
#[cfg(test)]
mod tests;
mod write;

pub(super) const EVENT_MAGIC: &[u8; 4] = b"HKE1";
pub(super) const EVENT_HEADER_BYTES: usize = 13;
pub(super) const SNAPSHOT_MAGIC: &[u8; 4] = b"HKS1";
pub(super) const SNAPSHOT_HEADER_BYTES: usize = 17;
pub(super) const MAX_EVENT_BYTES: usize = 128 * 1024;
pub(super) const MAX_EVENT_LOG_BYTES: usize = 256 * 1024 * 1024;
pub(super) const MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;
pub(super) const MAX_SNAPSHOT_FILE_BYTES: usize = SNAPSHOT_HEADER_BYTES + MAX_SNAPSHOT_BYTES;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HistoryEventV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    pub timestamp_ms: i64,
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub shell: ShellKind,
    pub exit_code: Option<i32>,
    pub imported: bool,
    #[serde(default = "one_occurrence")]
    pub occurrences: u64,
}

const fn one_occurrence() -> u64 {
    1
}

#[derive(Clone, Debug, Default)]
pub struct HistoryReadReport {
    pub events: Vec<HistoryEventV1>,
    pub torn_tail: bool,
    pub corrupt_offset: Option<u64>,
    pub snapshot_corrupt: bool,
    pub valid_event_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HistoryStats {
    pub events: usize,
    pub records: usize,
    pub bytes: u64,
    pub torn_tail: bool,
    pub snapshot_corrupt: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HistoryCompactionReport {
    pub records_before: usize,
    pub records_after: usize,
    pub logical_events: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HistoryCursor {
    snapshot_crc32: u32,
    event_offset: u64,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryDelta {
    pub events: Vec<HistoryEventV1>,
    pub cursor: HistoryCursor,
    pub reset: bool,
    pub torn_tail: bool,
    pub corrupt_offset: Option<u64>,
    pub snapshot_corrupt: bool,
}

#[derive(Clone, Debug)]
pub struct HistoryStore {
    state_directory: PathBuf,
    path: PathBuf,
    snapshot_path: PathBuf,
    lock_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct SnapshotV1 {
    pub(super) consumed_event_bytes: u64,
    pub(super) consumed_event_crc32: u32,
    pub(super) events: Vec<HistoryEventV1>,
}

#[derive(Debug, Default)]
pub(super) struct ParsedEvents {
    pub(super) events: Vec<HistoryEventV1>,
    pub(super) torn_tail: bool,
    pub(super) corrupt_offset: Option<u64>,
    pub(super) valid_bytes: u64,
}

impl HistoryStore {
    pub fn open(state_directory: &Path) -> crate::Result<Self> {
        fs::create_dir_all(state_directory)?;
        let metadata = fs::symlink_metadata(state_directory)?;
        if !metadata.file_type().is_dir() || metadata.uid() != nix::unistd::geteuid().as_raw() {
            return Err(crate::Error::History(format!(
                "{} must be a directory owned by the current user",
                state_directory.display()
            )));
        }
        fs::set_permissions(state_directory, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            state_directory: state_directory.to_owned(),
            path: state_directory.join("history.events"),
            snapshot_path: state_directory.join("history.snapshot"),
            lock_path: state_directory.join("history.lock"),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn snapshot_path(&self) -> &Path {
        &self.snapshot_path
    }

    #[must_use]
    pub fn needs_compaction(&self, event_bytes_threshold: u64) -> bool {
        file_len(&self.path) >= event_bytes_threshold
    }

    pub fn stats(&self) -> crate::Result<HistoryStats> {
        let report = self.read()?;
        let logical_events = report.events.iter().fold(0_u64, |total, event| {
            total.saturating_add(event.occurrences.max(1))
        });
        Ok(HistoryStats {
            events: usize::try_from(logical_events).unwrap_or(usize::MAX),
            records: report.events.len(),
            bytes: file_len(&self.path).saturating_add(file_len(&self.snapshot_path)),
            torn_tail: report.torn_tail,
            snapshot_corrupt: report.snapshot_corrupt,
        })
    }

    pub(super) fn open_lock(&self) -> crate::Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
            .open(&self.lock_path)?;
        ensure_private_file(&file, &self.lock_path)?;
        Ok(file)
    }

    pub(super) fn open_private_append(&self, path: &Path) -> crate::Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .mode(0o600)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
            .open(path)?;
        ensure_private_file(&file, path)?;
        Ok(file)
    }
}

pub(super) fn ensure_private_file(file: &File, path: &Path) -> crate::Result<()> {
    let mut metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != nix::unistd::geteuid().as_raw() {
        return Err(crate::Error::History(format!(
            "{} must be a private regular file owned by the current user",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        metadata = file.metadata()?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(crate::Error::History(format!(
                "{} permissions could not be restricted to the current user",
                path.display()
            )));
        }
    }
    Ok(())
}

pub(super) fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}
