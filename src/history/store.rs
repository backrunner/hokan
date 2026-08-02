use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use fs2::FileExt;
use nix::fcntl::OFlag;
use serde::{Deserialize, Serialize};

use crate::shell::ShellKind;

const EVENT_MAGIC: &[u8; 4] = b"HKE1";
const EVENT_HEADER_BYTES: usize = 13;
const SNAPSHOT_MAGIC: &[u8; 4] = b"HKS1";
const SNAPSHOT_HEADER_BYTES: usize = 17;
const MAX_EVENT_BYTES: usize = 128 * 1024;
const MAX_EVENT_LOG_BYTES: usize = 256 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;
const MAX_SNAPSHOT_FILE_BYTES: usize = SNAPSHOT_HEADER_BYTES + MAX_SNAPSHOT_BYTES;

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
struct SnapshotV1 {
    consumed_event_bytes: u64,
    consumed_event_crc32: u32,
    events: Vec<HistoryEventV1>,
}

#[derive(Debug, Default)]
struct ParsedEvents {
    events: Vec<HistoryEventV1>,
    torn_tail: bool,
    corrupt_offset: Option<u64>,
    valid_bytes: u64,
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

    pub fn append(&self, event: &HistoryEventV1) -> crate::Result<()> {
        self.append_many(std::slice::from_ref(event))
    }

    pub fn append_many(&self, events: &[HistoryEventV1]) -> crate::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut encoded = Vec::new();
        for event in events {
            encode_event(event, &mut encoded)?;
        }

        let lock = self.open_lock()?;
        lock.lock_exclusive()?;
        let result = (|| -> crate::Result<()> {
            let mut file = self.open_private_append(&self.path)?;
            ensure_event_log_capacity(&file, encoded.len())?;
            file.write_all(&encoded)?;
            file.flush()?;
            file.sync_data()?;
            Ok(())
        })();
        let unlock = FileExt::unlock(&lock);
        result?;
        unlock?;
        Ok(())
    }

    pub fn try_append_many(&self, events: &[HistoryEventV1]) -> crate::Result<bool> {
        if events.is_empty() {
            return Ok(true);
        }
        let mut encoded = Vec::new();
        for event in events {
            encode_event(event, &mut encoded)?;
        }

        let lock = self.open_lock()?;
        if !try_lock_exclusive(&lock)? {
            return Ok(false);
        }
        let result = (|| -> crate::Result<()> {
            let mut file = self.open_private_append(&self.path)?;
            ensure_event_log_capacity(&file, encoded.len())?;
            file.write_all(&encoded)?;
            file.flush()?;
            file.sync_data()?;
            Ok(())
        })();
        let unlock = FileExt::unlock(&lock);
        result?;
        unlock?;
        Ok(true)
    }

    pub fn read(&self) -> crate::Result<HistoryReadReport> {
        let lock = self.open_lock()?;
        lock.lock_shared()?;
        let result = self.read_locked();
        let unlock = FileExt::unlock(&lock);
        let report = result?;
        unlock?;
        Ok(report)
    }

    pub fn read_with_cursor(&self) -> crate::Result<(HistoryReadReport, HistoryCursor)> {
        let lock = self.open_lock()?;
        lock.lock_shared()?;
        let result = (|| -> crate::Result<(HistoryReadReport, HistoryCursor)> {
            let report = self.read_locked()?;
            let cursor = HistoryCursor {
                snapshot_crc32: crc32fast::hash(&read_optional(
                    &self.snapshot_path,
                    MAX_SNAPSHOT_FILE_BYTES,
                )?),
                event_offset: report.valid_event_bytes,
            };
            Ok((report, cursor))
        })();
        let unlock = FileExt::unlock(&lock);
        let value = result?;
        unlock?;
        Ok(value)
    }

    pub fn read_since(&self, cursor: HistoryCursor) -> crate::Result<HistoryDelta> {
        let lock = self.open_lock()?;
        lock.lock_shared()?;
        let result = self.read_since_locked(cursor);
        let unlock = FileExt::unlock(&lock);
        let delta = result?;
        unlock?;
        Ok(delta)
    }

    pub fn try_read_since(&self, cursor: HistoryCursor) -> crate::Result<Option<HistoryDelta>> {
        let lock = self.open_lock()?;
        if !try_lock_shared(&lock)? {
            return Ok(None);
        }
        let result = self.read_since_locked(cursor);
        let unlock = FileExt::unlock(&lock);
        let delta = result?;
        unlock?;
        Ok(Some(delta))
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

    pub fn repair_torn_tail(&self) -> crate::Result<u64> {
        let lock = self.open_lock()?;
        lock.lock_exclusive()?;
        let result = (|| -> crate::Result<u64> {
            let bytes = read_optional(&self.path, MAX_EVENT_LOG_BYTES)?;
            let parsed = parse_event_bytes(&bytes, 0);
            if let Some(offset) = parsed.corrupt_offset {
                return Err(crate::Error::History(format!(
                    "refusing to truncate corruption at byte {offset}"
                )));
            }
            if !parsed.torn_tail {
                return Ok(0);
            }
            let removed = bytes.len() as u64 - parsed.valid_bytes;
            let file = OpenOptions::new()
                .write(true)
                .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
                .open(&self.path)?;
            ensure_private_file(&file, &self.path)?;
            file.set_len(parsed.valid_bytes)?;
            file.sync_data()?;
            Ok(removed)
        })();
        let unlock = FileExt::unlock(&lock);
        let removed = result?;
        unlock?;
        Ok(removed)
    }

    pub fn compact(&self) -> crate::Result<HistoryCompactionReport> {
        let lock = self.open_lock()?;
        lock.lock_exclusive()?;
        let result = (|| {
            let report = self.read_locked()?;
            if let Some(offset) = report.corrupt_offset {
                return Err(crate::Error::History(format!(
                    "refusing to compact corruption at byte {offset}"
                )));
            }
            if report.snapshot_corrupt {
                return Err(crate::Error::History(
                    "refusing to compact a corrupt history snapshot".into(),
                ));
            }
            let bytes_before = file_len(&self.path).saturating_add(file_len(&self.snapshot_path));
            let records_before = report.events.len();
            let events = aggregate_events(report.events);
            let logical_events = events.iter().fold(0_u64, |total, event| {
                total.saturating_add(event.occurrences.max(1))
            });
            let event_bytes = read_optional(&self.path, MAX_EVENT_LOG_BYTES)?;
            let snapshot = SnapshotV1 {
                consumed_event_bytes: event_bytes.len() as u64,
                consumed_event_crc32: crc32fast::hash(&event_bytes),
                events,
            };
            self.write_snapshot_atomic(&snapshot)?;
            self.truncate_events()?;
            sync_directory(&self.state_directory)?;
            Ok(HistoryCompactionReport {
                records_before,
                records_after: snapshot.events.len(),
                logical_events: usize::try_from(logical_events).unwrap_or(usize::MAX),
                bytes_before,
                bytes_after: file_len(&self.snapshot_path),
            })
        })();
        let unlock = FileExt::unlock(&lock);
        let report = result?;
        unlock?;
        Ok(report)
    }

    pub fn quarantine_corrupt(&self) -> crate::Result<Vec<PathBuf>> {
        let lock = self.open_lock()?;
        lock.lock_exclusive()?;
        let result = (|| -> crate::Result<Vec<PathBuf>> {
            let report = self.read_locked()?;
            let mut backups = Vec::new();
            if report.snapshot_corrupt && self.snapshot_path.exists() {
                backups.push(move_to_unique_backup(&self.snapshot_path)?);
            }
            if report.corrupt_offset.is_some() && self.path.exists() {
                backups.push(move_to_unique_backup(&self.path)?);
            }
            if backups.is_empty() {
                return Ok(backups);
            }
            let snapshot = SnapshotV1 {
                consumed_event_bytes: 0,
                consumed_event_crc32: crc32fast::hash(&[]),
                events: report.events,
            };
            self.write_snapshot_atomic(&snapshot)?;
            self.truncate_events()?;
            sync_directory(&self.state_directory)?;
            Ok(backups)
        })();
        let unlock = FileExt::unlock(&lock);
        let backups = result?;
        unlock?;
        Ok(backups)
    }

    pub fn rewrite(&self, events: &[HistoryEventV1]) -> crate::Result<()> {
        let lock = self.open_lock()?;
        lock.lock_exclusive()?;
        let result = (|| {
            let snapshot = SnapshotV1 {
                consumed_event_bytes: 0,
                consumed_event_crc32: crc32fast::hash(&[]),
                events: events.to_vec(),
            };
            self.write_snapshot_atomic(&snapshot)?;
            self.truncate_events()?;
            sync_directory(&self.state_directory)
        })();
        let unlock = FileExt::unlock(&lock);
        result?;
        unlock?;
        Ok(())
    }

    pub fn clear(&self) -> crate::Result<()> {
        let lock = self.open_lock()?;
        lock.lock_exclusive()?;
        let result = (|| {
            move_to_backup(&self.path, &self.path.with_extension("events.cleared"))?;
            move_to_backup(
                &self.snapshot_path,
                &self.snapshot_path.with_extension("snapshot.cleared"),
            )?;
            sync_directory(&self.state_directory)
        })();
        let unlock = FileExt::unlock(&lock);
        result?;
        unlock?;
        Ok(())
    }

    fn read_locked(&self) -> crate::Result<HistoryReadReport> {
        let snapshot_bytes = read_optional(&self.snapshot_path, MAX_SNAPSHOT_FILE_BYTES)?;
        let (snapshot, snapshot_corrupt) = if snapshot_bytes.is_empty() {
            (None, false)
        } else {
            match decode_snapshot(&snapshot_bytes) {
                Ok(snapshot) => (Some(snapshot), false),
                Err(()) => (None, true),
            }
        };
        let event_bytes = read_optional(&self.path, MAX_EVENT_LOG_BYTES)?;
        let tail_start = snapshot
            .as_ref()
            .filter(|snapshot| {
                let consumed = usize::try_from(snapshot.consumed_event_bytes).ok();
                consumed.is_some_and(|consumed| {
                    consumed <= event_bytes.len()
                        && crc32fast::hash(&event_bytes[..consumed])
                            == snapshot.consumed_event_crc32
                })
            })
            .and_then(|snapshot| usize::try_from(snapshot.consumed_event_bytes).ok())
            .unwrap_or(0);
        let parsed = parse_event_bytes(&event_bytes[tail_start..], tail_start as u64);
        let mut events = snapshot.map_or_else(Vec::new, |snapshot| snapshot.events);
        events.extend(parsed.events);
        Ok(HistoryReadReport {
            events,
            torn_tail: parsed.torn_tail,
            corrupt_offset: parsed.corrupt_offset,
            snapshot_corrupt,
            valid_event_bytes: tail_start as u64 + parsed.valid_bytes,
        })
    }

    fn read_since_locked(&self, cursor: HistoryCursor) -> crate::Result<HistoryDelta> {
        let snapshot_bytes = read_optional(&self.snapshot_path, MAX_SNAPSHOT_FILE_BYTES)?;
        let event_bytes = read_optional(&self.path, MAX_EVENT_LOG_BYTES)?;
        let snapshot_crc32 = crc32fast::hash(&snapshot_bytes);
        let start = usize::try_from(cursor.event_offset).ok();
        if snapshot_crc32 != cursor.snapshot_crc32
            || start.is_none_or(|start| start > event_bytes.len())
        {
            let report = self.read_locked()?;
            return Ok(HistoryDelta {
                cursor: HistoryCursor {
                    snapshot_crc32,
                    event_offset: report.valid_event_bytes,
                },
                events: report.events,
                reset: true,
                torn_tail: report.torn_tail,
                corrupt_offset: report.corrupt_offset,
                snapshot_corrupt: report.snapshot_corrupt,
            });
        }
        let start = start.expect("cursor offset was checked above");
        let parsed = parse_event_bytes(&event_bytes[start..], start as u64);
        Ok(HistoryDelta {
            cursor: HistoryCursor {
                snapshot_crc32,
                event_offset: start as u64 + parsed.valid_bytes,
            },
            events: parsed.events,
            reset: false,
            torn_tail: parsed.torn_tail,
            corrupt_offset: parsed.corrupt_offset,
            snapshot_corrupt: false,
        })
    }

    fn open_lock(&self) -> crate::Result<File> {
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

    fn open_private_append(&self, path: &Path) -> crate::Result<File> {
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

    fn write_snapshot_atomic(&self, snapshot: &SnapshotV1) -> crate::Result<()> {
        let payload = serde_json::to_vec(snapshot)?;
        if payload.len() > MAX_SNAPSHOT_BYTES {
            return Err(crate::Error::History(
                "history snapshot is too large".into(),
            ));
        }
        let temporary = self.snapshot_path.with_extension("snapshot.pending");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
            .open(&temporary)?;
        ensure_private_file(&file, &temporary)?;
        file.write_all(SNAPSHOT_MAGIC)?;
        file.write_all(&[1])?;
        file.write_all(&(payload.len() as u64).to_be_bytes())?;
        file.write_all(&crc32fast::hash(&payload).to_be_bytes())?;
        file.write_all(&payload)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temporary, &self.snapshot_path)?;
        Ok(())
    }

    fn truncate_events(&self) -> crate::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
            .open(&self.path)?;
        ensure_private_file(&file, &self.path)?;
        file.sync_all()?;
        Ok(())
    }
}

fn encode_event(event: &HistoryEventV1, output: &mut Vec<u8>) -> crate::Result<()> {
    if event.occurrences == 0 {
        return Err(crate::Error::History(
            "history event occurrences must be positive".into(),
        ));
    }
    if event.event_id.as_ref().is_some_and(|event_id| {
        event_id.is_empty()
            || event_id.len() > 128
            || !event_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b':')
    }) {
        return Err(crate::Error::History("history event id is invalid".into()));
    }
    let payload = serde_json::to_vec(event)?;
    if payload.len() > MAX_EVENT_BYTES {
        return Err(crate::Error::History("history event is too large".into()));
    }
    output.extend_from_slice(EVENT_MAGIC);
    output.push(1);
    output.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    output.extend_from_slice(&crc32fast::hash(&payload).to_be_bytes());
    output.extend_from_slice(&payload);
    Ok(())
}

fn ensure_event_log_capacity(file: &File, append_bytes: usize) -> crate::Result<()> {
    let append_bytes = u64::try_from(append_bytes)
        .map_err(|_| crate::Error::History("history append is too large".into()))?;
    let current_bytes = file.metadata()?.len();
    if current_bytes
        .checked_add(append_bytes)
        .is_none_or(|total| total > MAX_EVENT_LOG_BYTES as u64)
    {
        return Err(crate::Error::History(
            "history event log would exceed the 256 MiB limit".into(),
        ));
    }
    Ok(())
}

fn parse_event_bytes(bytes: &[u8], base_offset: u64) -> ParsedEvents {
    let mut parsed = ParsedEvents::default();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes.len() - offset < EVENT_HEADER_BYTES {
            parsed.torn_tail = true;
            break;
        }
        if &bytes[offset..offset + 4] != EVENT_MAGIC || bytes[offset + 4] != 1 {
            parsed.corrupt_offset = Some(base_offset + offset as u64);
            break;
        }
        let length = u32::from_be_bytes(
            bytes[offset + 5..offset + 9]
                .try_into()
                .expect("four-byte length slice"),
        ) as usize;
        let checksum = u32::from_be_bytes(
            bytes[offset + 9..offset + 13]
                .try_into()
                .expect("four-byte checksum slice"),
        );
        if length > MAX_EVENT_BYTES {
            parsed.corrupt_offset = Some(base_offset + offset as u64);
            break;
        }
        let payload_start = offset + EVENT_HEADER_BYTES;
        let payload_end = payload_start.saturating_add(length);
        if payload_end > bytes.len() {
            parsed.torn_tail = true;
            break;
        }
        let payload = &bytes[payload_start..payload_end];
        if crc32fast::hash(payload) != checksum {
            parsed.corrupt_offset = Some(base_offset + offset as u64);
            break;
        }
        match serde_json::from_slice::<HistoryEventV1>(payload) {
            Ok(event) if event.occurrences > 0 => parsed.events.push(event),
            _ => {
                parsed.corrupt_offset = Some(base_offset + offset as u64);
                break;
            }
        }
        offset = payload_end;
        parsed.valid_bytes = offset as u64;
    }
    parsed
}

fn decode_snapshot(bytes: &[u8]) -> Result<SnapshotV1, ()> {
    if bytes.len() < SNAPSHOT_HEADER_BYTES || &bytes[..4] != SNAPSHOT_MAGIC || bytes[4] != 1 {
        return Err(());
    }
    let length = u64::from_be_bytes(bytes[5..13].try_into().map_err(|_| ())?);
    let length = usize::try_from(length).map_err(|_| ())?;
    let checksum = u32::from_be_bytes(bytes[13..17].try_into().map_err(|_| ())?);
    if length > MAX_SNAPSHOT_BYTES || SNAPSHOT_HEADER_BYTES + length != bytes.len() {
        return Err(());
    }
    let payload = &bytes[SNAPSHOT_HEADER_BYTES..];
    if crc32fast::hash(payload) != checksum {
        return Err(());
    }
    let snapshot: SnapshotV1 = serde_json::from_slice(payload).map_err(|_| ())?;
    if snapshot.events.iter().any(|event| event.occurrences == 0) {
        return Err(());
    }
    Ok(snapshot)
}

fn aggregate_events(events: Vec<HistoryEventV1>) -> Vec<HistoryEventV1> {
    let mut aggregated: HashMap<String, HistoryEventV1> = HashMap::new();
    for event in events {
        let key = normalize_command(&event.command);
        match aggregated.get_mut(&key) {
            Some(existing) => {
                existing.occurrences = existing
                    .occurrences
                    .saturating_add(event.occurrences.max(1));
                if event.timestamp_ms >= existing.timestamp_ms {
                    let occurrences = existing.occurrences;
                    *existing = event;
                    existing.occurrences = occurrences;
                }
            }
            None => {
                aggregated.insert(key, event);
            }
        }
    }
    let mut events: Vec<_> = aggregated.into_values().collect();
    events.sort_by(|left, right| {
        left.command
            .cmp(&right.command)
            .then_with(|| left.timestamp_ms.cmp(&right.timestamp_ms))
    });
    events
}

fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn read_optional(path: &Path, max_bytes: usize) -> crate::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits());
    match options.open(path) {
        Ok(mut file) => {
            ensure_private_file(&file, path)?;
            if file.metadata()?.len() > max_bytes as u64 {
                return Err(crate::Error::History(format!(
                    "{} exceeds the history file limit",
                    path.display()
                )));
            }
            Read::by_ref(&mut file)
                .take(max_bytes as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > max_bytes {
                return Err(crate::Error::History(format!(
                    "{} exceeds the history file limit",
                    path.display()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(bytes)
}

fn ensure_private_file(file: &File, path: &Path) -> crate::Result<()> {
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

fn try_lock_exclusive(file: &File) -> crate::Result<bool> {
    match FileExt::try_lock_exclusive(file) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn try_lock_shared(file: &File) -> crate::Result<bool> {
    match FileExt::try_lock_shared(file) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn move_to_backup(path: &Path, backup: &Path) -> crate::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if backup.exists() {
        fs::remove_file(backup)?;
    }
    fs::rename(path, backup)?;
    Ok(())
}

fn move_to_unique_backup(path: &Path) -> crate::Result<PathBuf> {
    for sequence in 0_u32..=u32::MAX {
        let suffix = if sequence == 0 {
            "corrupt".to_owned()
        } else {
            format!("corrupt.{sequence}")
        };
        let mut backup = path.as_os_str().to_os_string();
        backup.push(format!(".{suffix}"));
        let backup = PathBuf::from(backup);
        if !backup.exists() {
            fs::rename(path, &backup)?;
            return Ok(backup);
        }
    }
    Err(crate::Error::History(
        "could not allocate a corruption backup path".into(),
    ))
}

fn sync_directory(path: &Path) -> crate::Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn event(command: &str) -> HistoryEventV1 {
        HistoryEventV1 {
            event_id: None,
            timestamp_ms: 1,
            command: command.into(),
            cwd: Some(PathBuf::from("/tmp")),
            shell: ShellKind::Zsh,
            exit_code: Some(0),
            imported: false,
            occurrences: 1,
        }
    }

    #[test]
    fn rejects_symlinked_state_and_history_files() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("directory");
        let actual_state = directory.path().join("actual-state");
        fs::create_dir(&actual_state).expect("actual state");
        let linked_state = directory.path().join("linked-state");
        symlink(&actual_state, &linked_state).expect("state symlink");
        assert!(HistoryStore::open(&linked_state).is_err());

        let store = HistoryStore::open(&actual_state).expect("store");
        let target = directory.path().join("target");
        fs::write(&target, b"unchanged").expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("target mode");
        symlink(&target, store.path()).expect("history symlink");
        assert!(store.append(&event("must not be written")).is_err());
        assert_eq!(fs::read(&target).expect("target contents"), b"unchanged");
    }

    #[test]
    fn tightens_existing_owned_history_file_permissions() {
        let directory = tempfile::tempdir().expect("directory");
        let store = HistoryStore::open(directory.path()).expect("store");
        fs::write(store.path(), []).expect("history file");
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644))
            .expect("broad fixture mode");
        store.read().expect("owned history should be migrated");
        assert_eq!(
            fs::metadata(store.path())
                .expect("history metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn round_trips_and_ignores_every_torn_tail() {
        let directory = tempfile::tempdir().expect("directory");
        let store = HistoryStore::open(directory.path()).expect("store");
        store.append(&event("one")).expect("append one");
        store.append(&event("two")).expect("append two");
        let bytes = fs::read(store.path()).expect("event bytes");
        assert_eq!(store.read().expect("read").events.len(), 2);

        for length in 1..bytes.len() {
            fs::write(store.path(), &bytes[..length]).expect("truncate fixture");
            let report = store.read().expect("torn read should not fail");
            assert!(report.events.len() <= 2);
            if !report.torn_tail && report.corrupt_offset.is_none() {
                assert!(!report.events.is_empty());
            }
        }
    }

    #[test]
    fn repairs_only_a_torn_tail() {
        let directory = tempfile::tempdir().expect("directory");
        let store = HistoryStore::open(directory.path()).expect("store");
        store.append(&event("one")).expect("append one");
        let valid = fs::metadata(store.path()).expect("metadata").len();
        let mut file = OpenOptions::new()
            .append(true)
            .open(store.path())
            .expect("open tail");
        file.write_all(b"HKE1\x01").expect("write torn bytes");
        assert_eq!(store.repair_torn_tail().expect("repair"), 5);
        assert_eq!(fs::metadata(store.path()).expect("metadata").len(), valid);
        assert_eq!(store.read().expect("read").events.len(), 1);
    }

    #[test]
    fn rejects_appends_before_the_event_log_exceeds_its_read_limit() {
        let directory = tempfile::tempdir().expect("directory");
        let store = HistoryStore::open(directory.path()).expect("store");
        store.append(&event("seed")).expect("create event log");
        let file = OpenOptions::new()
            .write(true)
            .open(store.path())
            .expect("open event log");
        file.set_len(MAX_EVENT_LOG_BYTES as u64)
            .expect("create sparse full event log");
        drop(file);

        let before = fs::metadata(store.path()).expect("before metadata").len();
        let error = store
            .append(&event("must not be appended"))
            .expect_err("full event log should reject append");
        assert!(error.to_string().contains("256 MiB limit"));
        assert!(store.try_append_many(&[event("also rejected")]).is_err());
        assert_eq!(
            fs::metadata(store.path()).expect("after metadata").len(),
            before
        );
    }

    #[test]
    fn compaction_aggregates_and_survives_the_rename_truncate_crash_window() {
        let directory = tempfile::tempdir().expect("directory");
        let store = HistoryStore::open(directory.path()).expect("store");
        store
            .append_many(&[event("git  status"), event("git status"), event("ls")])
            .expect("append events");
        let old_tail = fs::read(store.path()).expect("old event tail");
        let report = store.compact().expect("compact");
        assert_eq!(report.records_before, 3);
        assert_eq!(report.records_after, 2);
        assert_eq!(report.logical_events, 3);
        assert_eq!(store.stats().expect("stats").events, 3);

        fs::write(store.path(), old_tail).expect("restore pre-truncate crash state");
        let read = store.read().expect("read crash state");
        assert_eq!(read.events.len(), 2);
        assert_eq!(
            read.events
                .iter()
                .map(|event| event.occurrences)
                .sum::<u64>(),
            3
        );
    }

    #[test]
    fn concurrent_batches_are_not_lost_or_interleaved() {
        let directory = tempfile::tempdir().expect("directory");
        let store = Arc::new(HistoryStore::open(directory.path()).expect("store"));
        let mut joins = Vec::new();
        for worker in 0..4 {
            let store = Arc::clone(&store);
            joins.push(std::thread::spawn(move || {
                let events: Vec<_> = (0..250)
                    .map(|index| event(&format!("worker-{worker}-{index}")))
                    .collect();
                store.append_many(&events).expect("append batch");
            }));
        }
        for join in joins {
            join.join().expect("worker should not panic");
        }
        let report = store.read().expect("read");
        assert_eq!(report.events.len(), 1_000);
        assert!(!report.torn_tail);
        assert_eq!(report.corrupt_offset, None);
    }

    #[test]
    fn corruption_is_quarantined_and_valid_prefix_is_rebuilt() {
        let directory = tempfile::tempdir().expect("directory");
        let store = HistoryStore::open(directory.path()).expect("store");
        store.append(&event("one")).expect("append one");
        let mut file = OpenOptions::new()
            .append(true)
            .open(store.path())
            .expect("open events");
        file.write_all(b"BAD!not-a-record")
            .expect("write corruption");
        assert!(store.read().expect("read").corrupt_offset.is_some());
        let backups = store.quarantine_corrupt().expect("quarantine");
        assert_eq!(backups.len(), 1);
        assert!(backups[0].exists());
        let report = store.read().expect("rebuilt read");
        assert_eq!(report.events.len(), 1);
        assert_eq!(report.events[0].command, "one");
        assert_eq!(report.corrupt_offset, None);
    }

    #[test]
    fn cursor_reads_only_new_tail_and_resets_after_compaction() {
        let directory = tempfile::tempdir().expect("directory");
        let store = HistoryStore::open(directory.path()).expect("store");
        store.append(&event("one")).expect("append one");
        let (_, cursor) = store.read_with_cursor().expect("initial cursor");
        store.append(&event("two")).expect("append two");
        let delta = store.read_since(cursor).expect("tail delta");
        assert!(!delta.reset);
        assert_eq!(delta.events.len(), 1);
        assert_eq!(delta.events[0].command, "two");

        store.compact().expect("compact");
        let reset = store.read_since(delta.cursor).expect("reset delta");
        assert!(reset.reset);
        assert_eq!(reset.events.len(), 2);
    }

    #[test]
    fn nonblocking_operations_report_lock_contention() {
        let directory = tempfile::tempdir().expect("directory");
        let store = HistoryStore::open(directory.path()).expect("store");
        let lock = store.open_lock().expect("lock file");
        lock.lock_exclusive().expect("hold lock");
        assert!(
            !store
                .try_append_many(&[event("queued")])
                .expect("try append")
        );
        assert!(
            store
                .try_read_since(HistoryCursor::default())
                .expect("try read")
                .is_none()
        );
        FileExt::unlock(&lock).expect("unlock");
        assert!(store.try_append_many(&[event("queued")]).expect("append"));
    }

    #[test]
    fn compaction_threshold_uses_event_tail_bytes() {
        let directory = tempfile::tempdir().expect("directory");
        let store = HistoryStore::open(directory.path()).expect("store");
        assert!(!store.needs_compaction(1));
        store.append(&event("one")).expect("append");
        assert!(store.needs_compaction(1));
        store.compact().expect("compact");
        assert!(!store.needs_compaction(1));
    }
}
