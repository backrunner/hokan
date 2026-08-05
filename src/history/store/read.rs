use std::{fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::Path};

use fs2::FileExt;
use nix::fcntl::OFlag;

use super::{lock::try_lock_shared, *};

impl HistoryStore {
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

    pub(super) fn read_locked(&self) -> crate::Result<HistoryReadReport> {
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

    pub(super) fn read_since_locked(&self, cursor: HistoryCursor) -> crate::Result<HistoryDelta> {
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
}

pub(super) fn parse_event_bytes(bytes: &[u8], base_offset: u64) -> ParsedEvents {
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

pub(super) fn read_optional(path: &Path, max_bytes: usize) -> crate::Result<Vec<u8>> {
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
