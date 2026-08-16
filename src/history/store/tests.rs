use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::Arc,
};

use fs2::FileExt;

use super::*;
use crate::{
    history::{HistoryIndex, HistoryPolicy, is_failed_exit},
    shell::ShellKind,
};

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
        cwd_occurrences: Some(1),
    }
}

#[test]
fn legacy_events_default_to_unknown_cwd_occurrences() {
    let mut value = serde_json::to_value(event("pnpm dev")).expect("serialize event");
    value
        .as_object_mut()
        .expect("event object")
        .remove("cwd_occurrences");
    let decoded: HistoryEventV1 = serde_json::from_value(value).expect("decode legacy event");
    assert_eq!(decoded.cwd_occurrences, None);
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
fn compaction_preserves_per_directory_command_counts() {
    let directory = tempfile::tempdir().expect("directory");
    let store = HistoryStore::open(directory.path()).expect("store");
    let mut first = event("pnpm dev");
    first.cwd = Some(PathBuf::from("/project-a"));
    first.timestamp_ms = 1;
    let mut second = first.clone();
    second.timestamp_ms = 2;
    let mut other = event("pnpm dev");
    other.cwd = Some(PathBuf::from("/project-b"));
    other.timestamp_ms = 3;
    store
        .append_many(&[first, second, other])
        .expect("append events");
    store.compact().expect("compact");

    let mut events = store.read().expect("read").events;
    events.sort_by(|left, right| left.cwd.cmp(&right.cwd));
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].cwd, Some(PathBuf::from("/project-a")));
    assert_eq!(events[0].occurrences, 2);
    assert_eq!(events[0].cwd_occurrences, Some(2));
    assert_eq!(events[1].cwd, Some(PathBuf::from("/project-b")));
    assert_eq!(events[1].occurrences, 1);
    assert_eq!(events[1].cwd_occurrences, Some(1));
}

#[test]
fn compaction_extends_the_conservative_count_from_legacy_snapshots() {
    let directory = tempfile::tempdir().expect("directory");
    let store = HistoryStore::open(directory.path()).expect("store");
    let mut legacy = event("pnpm dev");
    legacy.occurrences = 100;
    legacy.cwd_occurrences = None;
    let current = event("pnpm dev");
    store
        .append_many(&[legacy, current])
        .expect("append events");
    store.compact().expect("compact");

    let events = store.read().expect("read").events;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].occurrences, 101);
    assert_eq!(
        events[0].cwd_occurrences,
        Some(2),
        "the legacy row proves one local run and the new row adds another"
    );
}

#[test]
fn compaction_keeps_successful_and_failed_counts_separate() {
    let directory = tempfile::tempdir().expect("directory");
    let project = tempfile::tempdir().expect("project");
    let project_root = project.path().canonicalize().expect("canonical project");
    let store = HistoryStore::open(directory.path()).expect("store");

    let mut successful = event("pnpm dev");
    successful.cwd = Some(project_root.clone());
    successful.occurrences = 3;
    successful.cwd_occurrences = Some(3);
    let mut failed = successful.clone();
    failed.timestamp_ms = 2;
    failed.exit_code = Some(1);
    failed.occurrences = 5;
    failed.cwd_occurrences = Some(5);
    store
        .append_many(&[successful, failed])
        .expect("append events");

    let report = store.compact().expect("compact");
    assert_eq!(report.records_after, 2);
    assert_eq!(report.logical_events, 8);
    let events = store.read().expect("read").events;
    let successful = events
        .iter()
        .find(|event| !is_failed_exit(event.exit_code))
        .expect("successful aggregate");
    let failed = events
        .iter()
        .find(|event| is_failed_exit(event.exit_code))
        .expect("failed aggregate");
    assert_eq!(successful.occurrences, 3);
    assert_eq!(successful.cwd_occurrences, Some(3));
    assert_eq!(failed.occurrences, 5);
    assert_eq!(failed.cwd_occurrences, Some(5));

    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    let mut index = HistoryIndex::default();
    for event in &events {
        index.ingest_event(event, &policy);
    }
    assert_eq!(
        index.search("pnpm dev", &project_root, 2, 1)[0]
            .record
            .count,
        3,
        "failed occurrences must not inflate global frequency"
    );
    assert_eq!(
        index.usage_frecency_in_project("pnpm dev", &project_root, 2),
        156,
        "project frequency must count only the three successful runs"
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
