use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use fs2::FileExt;
use nix::fcntl::OFlag;

use super::{
    lock::try_lock_exclusive,
    read::{parse_event_bytes, read_optional},
    *,
};

impl HistoryStore {
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
