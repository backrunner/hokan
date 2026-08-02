#![cfg(unix)]

use std::{env, path::PathBuf, process::Command};

use hokann::{history::HistoryEventV1, shell::ShellKind};

const DIRECTORY_ENV: &str = "HOKANN_TEST_HISTORY_DIRECTORY";
const WORKER_ENV: &str = "HOKANN_TEST_HISTORY_WORKER";

#[test]
fn history_worker_process() {
    let Some(directory) = env::var_os(DIRECTORY_ENV).map(PathBuf::from) else {
        return;
    };
    let worker: u64 = env::var(WORKER_ENV)
        .expect("worker id")
        .parse()
        .expect("numeric worker id");
    let store = hokann::history::HistoryStore::open(&directory).expect("worker store");
    for batch in 0..50_u64 {
        let events: Vec<_> = (0..1_000_u64)
            .map(|offset| {
                let sequence = batch * 1_000 + offset;
                HistoryEventV1 {
                    event_id: Some(format!("{worker:x}:{sequence:x}")),
                    timestamp_ms: sequence as i64,
                    command: format!("worker-{worker} command-{sequence}"),
                    cwd: None,
                    shell: ShellKind::Bash,
                    exit_code: Some(0),
                    imported: false,
                    occurrences: 1,
                }
            })
            .collect();
        store.append_many(&events).expect("append worker batch");
    }
}

#[test]
fn two_processes_append_one_hundred_thousand_events_without_loss() {
    let directory = tempfile::tempdir().expect("history directory");
    let executable = env::current_exe().expect("test executable");
    let mut children = Vec::new();
    for worker in 0..2_u64 {
        children.push(
            Command::new(&executable)
                .args(["--exact", "history_worker_process", "--nocapture"])
                .env(DIRECTORY_ENV, directory.path())
                .env(WORKER_ENV, worker.to_string())
                .spawn()
                .expect("spawn history worker"),
        );
    }
    for mut child in children {
        let status = child.wait().expect("wait for history worker");
        assert!(status.success(), "history worker failed: {status}");
    }

    let store = hokann::history::HistoryStore::open(directory.path()).expect("parent store");
    let stats = store.stats().expect("history stats");
    assert_eq!(stats.events, 100_000);
    assert_eq!(stats.records, 100_000);
    assert!(!stats.torn_tail);
    assert!(!stats.snapshot_corrupt);
}
