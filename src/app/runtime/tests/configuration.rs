use super::*;

#[test]
fn startup_history_read_is_tail_bounded_and_line_aligned() {
    let directory = tempfile::tempdir().expect("history directory");
    let path = directory.path().join("history");
    std::fs::write(&path, b"discarded command\nrecent one\nrecent two\n").expect("history fixture");
    let bytes = read_history_tail(&path, 22).expect("history tail");
    assert!(bytes.len() <= 22);
    assert_eq!(bytes, b"recent one\nrecent two\n");
}

#[test]
fn startup_history_read_rejects_fifos_without_blocking() {
    use nix::{sys::stat::Mode, unistd::mkfifo};

    let directory = tempfile::tempdir().expect("history directory");
    let path = directory.path().join("history.fifo");
    mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).expect("history FIFO");
    let started = Instant::now();
    assert!(read_history_tail(&path, 1024).is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn history_lock_contention_queues_without_double_indexing() {
    let directory = tempfile::tempdir().expect("directory");
    let store = HistoryStore::open(directory.path()).expect("store");
    let lock_path = directory.path().join("history.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(lock_path)
        .expect("lock file");
    lock.lock_exclusive().expect("hold lock");

    let history = Arc::new(RwLock::new(HistoryIndex::default()));
    let policy = HistoryPolicy::new(16_384, &[]).expect("policy");
    let mut state = runtime_state(directory.path());
    record_history(
        "echo queued".into(),
        Some(0),
        &mut state,
        &store,
        &history,
        &policy,
    )
    .expect("queue history");
    assert_eq!(state.pending_history.len(), 1);
    assert_eq!(
        history
            .read()
            .expect("history index")
            .search("echo queued", Path::new("/"), 1, 10)[0]
            .record
            .count,
        1
    );

    FileExt::unlock(&lock).expect("unlock");
    state.history_retry_at = Instant::now();
    flush_pending_history(&mut state, &store).expect("flush queue");
    sync_history(&mut state, &store, &history, &policy).expect("sync queue");
    assert!(state.pending_history.is_empty());
    assert_eq!(
        history
            .read()
            .expect("history index")
            .search("echo queued", Path::new("/"), 1, 10)[0]
            .record
            .count,
        1
    );
}

#[test]
fn recognizes_bash_ignorespace_history_modes() {
    assert!(history_control_ignores_space("ignorespace"));
    assert!(history_control_ignores_space("erasedups:ignoreboth"));
    assert!(!history_control_ignores_space("ignoredups:erasedups"));
}

#[test]
fn live_config_keeps_structural_values_until_restart() {
    let current = Config::default();
    let mut loaded = current.clone();
    loaded.ui.max_rows = 7;
    loaded.completion.max_candidates = 77;
    loaded.core.login_shell = true;
    loaded.history.max_command_bytes = 999;
    loaded.logging.enabled = true;
    let (live, restart) = merge_live_config(&current, loaded);
    assert_eq!(live.ui.max_rows, 7);
    assert_eq!(live.completion.max_candidates, 77);
    assert_eq!(live.core, current.core);
    assert_eq!(live.history, current.history);
    assert_eq!(live.logging, current.logging);
    assert_eq!(restart, vec!["core", "history", "logging"]);
}

#[test]
fn config_reload_status_avoids_empty_success_but_preserves_restart_warnings() {
    assert_eq!(config_reload_status(&[], false), None);
    assert_eq!(
        config_reload_status(&[], true).as_deref(),
        Some("HK-CFG-RELOAD applied provider and UI configuration")
    );
    assert_eq!(
        config_reload_status(&["core", "history"], false).as_deref(),
        Some("HK-CFG-RESTART restart required for core, history changes")
    );
}
