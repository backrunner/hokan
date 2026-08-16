use std::{
    sync::{Arc, RwLock, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

use super::state::RuntimeState;
use crate::history::{HistoryEventV1, HistoryIndex, HistoryPolicy, HistoryStore};

const HISTORY_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const HISTORY_COMPACTION_THRESHOLD_BYTES: u64 = 4 * 1024 * 1024;

pub(super) fn record_history(
    command: String,
    exit_code: Option<i32>,
    state: &mut RuntimeState,
    store: &HistoryStore,
    history: &Arc<RwLock<HistoryIndex>>,
    policy: &HistoryPolicy,
) -> crate::Result<()> {
    let timestamp_ms = crate::history_now_ms();
    state.previous_command = Some(command.clone());
    if !policy.allows(&command) {
        return Ok(());
    }
    let event = HistoryEventV1 {
        event_id: Some(state.next_history_event_id()?),
        timestamp_ms,
        command: command.clone(),
        cwd: Some(state.cwd.clone()),
        shell: state.shell,
        exit_code,
        imported: false,
        occurrences: 1,
        cwd_occurrences: Some(1),
    };
    {
        let mut index = history
            .write()
            .map_err(|_| crate::Error::History("history index was poisoned".into()))?;
        index.ingest_weighted(
            &event.command,
            event.timestamp_ms,
            event.shell,
            event.cwd.as_deref(),
            event.occurrences,
            event.exit_code,
            policy,
        );
    }
    if let Some(event_id) = &event.event_id {
        state.local_history_ids.insert(event_id.clone());
    }
    state.pending_history.push_back(event);
    flush_pending_history(state, store)?;
    sync_history(state, store, history, policy)
}

pub(super) fn sync_history(
    state: &mut RuntimeState,
    store: &HistoryStore,
    history: &Arc<RwLock<HistoryIndex>>,
    policy: &HistoryPolicy,
) -> crate::Result<()> {
    flush_pending_history(state, store)?;
    let Some(mut delta) = store.try_read_since(state.history_cursor)? else {
        state.history_retry_at = Instant::now() + HISTORY_RETRY_INTERVAL;
        return Ok(());
    };
    if delta.snapshot_corrupt || delta.corrupt_offset.is_some() {
        store.quarantine_corrupt()?;
        let (report, cursor) = store.read_with_cursor()?;
        delta.events = report.events;
        delta.cursor = cursor;
        delta.reset = true;
        delta.torn_tail = report.torn_tail;
    }
    if delta.torn_tail {
        store.repair_torn_tail()?;
    }
    let mut index = history
        .write()
        .map_err(|_| crate::Error::History("history index was poisoned".into()))?;
    if delta.reset {
        index.merge_events_absolute(&delta.events, policy);
        if state.pending_history.is_empty() {
            state.local_history_ids.clear();
        }
    } else {
        for event in &delta.events {
            if event
                .event_id
                .as_ref()
                .is_some_and(|event_id| state.local_history_ids.remove(event_id))
            {
                continue;
            }
            index.ingest_event(event, policy);
        }
    }
    state.history_cursor = delta.cursor;
    Ok(())
}

pub(super) fn flush_pending_history(
    state: &mut RuntimeState,
    store: &HistoryStore,
) -> crate::Result<()> {
    if state.pending_history.is_empty() || Instant::now() < state.history_retry_at {
        return Ok(());
    }
    maybe_start_history_compaction(state, store)?;
    let batch: Vec<_> = state.pending_history.iter().cloned().collect();
    match store.try_append_many(&batch) {
        Ok(true) => {
            state.pending_history.clear();
            state.history_retry_at = Instant::now();
            maybe_start_history_compaction(state, store)?;
        }
        Ok(false) => {
            state.history_retry_at = Instant::now() + HISTORY_RETRY_INTERVAL;
        }
        Err(error) => {
            state.history_retry_at = Instant::now() + HISTORY_RETRY_INTERVAL;
            state.status = Some(format!("HK-HIS-WRITE queued for retry: {error}"));
        }
    }
    Ok(())
}

fn maybe_start_history_compaction(
    state: &mut RuntimeState,
    store: &HistoryStore,
) -> crate::Result<()> {
    if !store.needs_compaction(HISTORY_COMPACTION_THRESHOLD_BYTES)
        || state.history_compaction.swap(true, Ordering::AcqRel)
    {
        return Ok(());
    }
    let store = store.clone();
    let active = Arc::clone(&state.history_compaction);
    if let Err(error) = thread::Builder::new()
        .name("hokan-history-compact".into())
        .spawn(move || {
            let _ = store.compact();
            active.store(false, Ordering::Release);
        })
    {
        state.history_compaction.store(false, Ordering::Release);
        return Err(error.into());
    }
    Ok(())
}

pub(super) fn flush_history_before_exit(state: &mut RuntimeState, store: &HistoryStore) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while !state.pending_history.is_empty() && Instant::now() < deadline {
        state.history_retry_at = Instant::now();
        let _ = flush_pending_history(state, store);
        if !state.pending_history.is_empty() {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

pub(super) fn new_history_session_id() -> crate::Result<String> {
    let mut bytes = [0_u8; 12];
    getrandom::fill(&mut bytes)
        .map_err(|error| crate::Error::History(format!("random session id failed: {error}")))?;
    let mut id = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut id, "{byte:02x}").map_err(|error| crate::Error::History(error.to_string()))?;
    }
    Ok(id)
}

pub(super) fn history_control_ignores_space(value: &str) -> bool {
    value
        .split(':')
        .any(|setting| matches!(setting, "ignorespace" | "ignoreboth"))
}
