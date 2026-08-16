use std::{
    fs::OpenOptions,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use fs2::FileExt;

use super::*;
use crate::completion::{
    Activation, BufferSnapshot, Candidate, CandidateAction, CandidateKind, CandidateSource,
    Completeness, CompletionContext, CursorPlacement, ProviderOutput, SyncQuality, TextEdit,
};
use crate::config::Config;
use crate::history::{HistoryCursor, HistoryIndex, HistoryPolicy, HistoryStore};
use crate::platform::CommandPathCache;
use crate::shell::{ControlMessage, ShellEvent, ShellKind};
use crate::terminal::{
    BufferRevision, QueryId, RiskLevel, TerminalQueryKind, TerminalReply, TerminalSize,
};

fn runtime_state(directory: &Path) -> RuntimeState {
    RuntimeState::new(
        ShellKind::Zsh,
        TerminalSize::new(24, 80).expect("terminal size"),
        directory.to_owned(),
        true,
        HistoryCursor::default(),
        12,
        directory.join("credentials.toml"),
        "0123456789abcdef01234567".into(),
        None,
        Arc::new(CommandPathCache::default()),
        Arc::new(crate::specs::SpecRegistry::default()),
        Arc::new(crate::providers::CommandHelpCache::default()),
    )
}

#[test]
fn private_cursor_timeout_fails_closed_without_standard_cpr() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    state.need_cpr = true;
    let (output, join) = test_output();

    handle_terminal_reply(
        TerminalReply::Timeout {
            query_id: QueryId::new(1),
            kind: TerminalQueryKind::CursorPositionPrivate,
        },
        &mut state,
        &output,
    )
    .expect("cursor timeout");

    assert_eq!(state.cursor_probe_backend, CursorProbeBackend::Unavailable);
    assert!(!state.need_cpr);
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn stale_tmux_cursor_result_is_retried_with_a_cooldown() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    state.editing = true;
    state.cursor_probe_backend = CursorProbeBackend::Tmux;
    let (output, join) = test_output();
    let output_state = output.state().expect("output state");
    state.pending_tmux_cursor = Some(PendingTmuxCursor {
        generation: 1,
        buffer_revision: BufferRevision::new(1),
        screen_revision: output_state.screen_revision,
        screen_epoch: output_state.screen_epoch,
        terminal_size: state.terminal_size,
    });

    let started = Instant::now();
    assert!(
        !handle_tmux_cursor_result(
            TmuxCursorResult {
                generation: 1,
                position: Some(crate::terminal::CellPos::new(0, 0)),
            },
            &mut state,
            &output,
        )
        .expect("stale result")
    );

    assert!(state.pending_tmux_cursor.is_none());
    assert!(state.need_cpr);
    assert!(
        state
            .tmux_cursor_retry_at
            .is_some_and(|retry_at| { retry_at >= started + TMUX_CURSOR_RETRY_DELAY })
    );
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn complete_executable_still_schedules_a_provider_query() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("directory");
    let bin = directory.path().join("bin");
    std::fs::create_dir(&bin).expect("bin directory");
    let executable = bin.join("ls");
    std::fs::write(&executable, b"#!/bin/sh\n").expect("write executable");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
        .expect("executable mode");

    let mut state = runtime_state(directory.path());
    state.commands.refresh_from_path(Some(bin.as_os_str()));
    state.buffer.set_exact("ls".into(), 2).expect("buffer");
    let mut engine = crate::completion::CompletionEngine::new(20, 12);
    engine.register(crate::providers::CommandSpecProvider::new(
        Arc::new(crate::specs::SpecRegistry::load(None)),
        Arc::clone(&state.commands),
    ));
    let worker = ProviderWorker::start(Arc::new(engine), None).expect("provider worker");

    state.schedule_query(&worker).expect("schedule query");
    assert!(
        state.context.is_some(),
        "a complete executable must not suppress the query"
    );
    assert!(state.provider_pending);

    // Results do not implicitly enter the list. The first navigation action
    // is what creates the selection (covered by move_selection tests below).
    let result = worker
        .results()
        .recv_timeout(Duration::from_secs(2))
        .expect("provider result");
    let (output, join) = test_output();
    handle_provider_result(result, &mut state, &output).expect("provider result handling");
    assert!(!state.candidates.is_empty());
    assert_eq!(state.selected, None);
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
    drop(worker);
}

#[test]
fn exact_path_command_prefetches_help_but_project_paths_stay_cold() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("directory");
    let bin = directory.path().join("bin");
    std::fs::create_dir(&bin).expect("bin directory");
    let executable = bin.join("codex-fixture");
    std::fs::write(
        &executable,
        b"#!/bin/sh\nprintf '%s\\n' 'Commands:' '  resume    Resume a session'\n",
    )
    .expect("write executable");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("executable mode");
    let commands = CommandPathCache::from_path(Some(&std::ffi::OsString::from(&bin)));
    let specs = crate::specs::SpecRegistry::default();
    let help = Arc::new(crate::providers::CommandHelpCache::default());
    let context = CompletionContext::new(
        QueryId::new(1),
        ShellKind::Zsh,
        directory.path().to_owned(),
        BufferSnapshot::new(
            Arc::<str>::from("codex-fixture"),
            "codex-fixture".len(),
            BufferRevision::ZERO,
            SyncQuality::Exact,
        )
        .expect("snapshot"),
    )
    .expect("context");

    super::state::prefetch_command_help(&context, &commands, &specs, &help);
    assert_eq!(help.fetch_count(), 1);
    // A repeated buffer event must share the same pending/cache entry.
    super::state::prefetch_command_help(&context, &commands, &specs, &help);
    assert_eq!(help.fetch_count(), 1);

    let explicit_help = Arc::new(crate::providers::CommandHelpCache::default());
    let explicit = CompletionContext::new(
        QueryId::new(2),
        ShellKind::Zsh,
        directory.path().to_owned(),
        BufferSnapshot::new(
            Arc::<str>::from("./bin/codex-fixture"),
            "./bin/codex-fixture".len(),
            BufferRevision::ZERO,
            SyncQuality::Exact,
        )
        .expect("snapshot"),
    )
    .expect("context");
    super::state::prefetch_command_help(&explicit, &commands, &specs, &explicit_help);
    assert_eq!(
        explicit_help.fetch_count(),
        0,
        "project-local executables must not be launched just to discover help"
    );
}

#[test]
fn child_shell_path_event_refreshes_the_shared_provider_cache() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("directory");
    let bin = directory.path().join("child-bin");
    std::fs::create_dir(&bin).expect("bin directory");
    let executable = bin.join("second-command");
    std::fs::write(&executable, b"#!/bin/sh\n").expect("write executable");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("executable mode");

    let mut state = runtime_state(directory.path());
    state.editing = true;
    state.buffer.set_exact("second".into(), 6).expect("buffer");
    let mut engine = crate::completion::CompletionEngine::new(20, 12);
    engine.register(crate::providers::PathCommandProvider::new(Arc::clone(
        &state.commands,
    )));
    let worker = ProviderWorker::start(Arc::new(engine), None).expect("provider worker");
    let (output, join) = test_output();
    let store = HistoryStore::open(&directory.path().join("state")).expect("history store");
    let history = Arc::new(RwLock::new(HistoryIndex::default()));
    let policy = HistoryPolicy::new(1024, &[]).expect("history policy");

    handle_control_message(
        ControlMessage::Event(ShellEvent::PathChanged {
            path: bin.as_os_str().to_owned(),
        }),
        &mut state,
        &output,
        &worker,
        &store,
        &history,
        &policy,
    )
    .expect("PATH event");

    assert!(state.commands.contains("second-command"));
    let result = worker
        .results()
        .recv_timeout(Duration::from_secs(2))
        .expect("refreshed completion result");
    assert!(result.output.candidates.iter().any(|candidate| {
        candidate.display.primary == "second-command"
            && candidate.source == CandidateSource::PathCommand
    }));

    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn background_help_refresh_does_not_steal_owned_overlay_states() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    state.editing = true;
    state
        .buffer
        .set_exact("natural language request".into(), 24)
        .expect("buffer");
    state.ai_owns_candidates = true;
    state.help.bump_revision();

    let worker = ProviderWorker::start(
        Arc::new(crate::completion::CompletionEngine::new(20, 12)),
        None,
    )
    .expect("provider worker");
    let before = state.query_id;
    state
        .refresh_help_results(&worker)
        .expect("refresh help revision");

    assert_eq!(state.query_id, before, "AI-owned rows must not be replaced");
    assert_eq!(state.help_revision, state.help.revision());
}

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
fn stale_candidate_activation_is_nonfatal() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    state
        .buffer
        .set_exact("ec".into(), 2)
        .expect("initial buffer");
    let context = Arc::new(
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            directory.path().to_owned(),
            state.snapshot().expect("snapshot"),
        )
        .expect("context"),
    );
    let candidate = Candidate::new(
        context.query_id,
        "echo",
        "candidate",
        Some(TextEdit {
            range: 0..2,
            replacement: "echo".into(),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::Insert,
        CandidateSource::History,
        CandidateKind::History,
        Completeness::Runnable,
        RiskLevel::Low,
        "stale-test",
    );
    state.selected = Some(candidate.id);
    state.candidates = vec![candidate];
    state.context = Some(Arc::clone(&context));
    state.candidates_context = Some(context);
    state
        .buffer
        .set_exact("echo".into(), 4)
        .expect("newer buffer");

    assert!(matches!(
        resolve_selected_activation(&state).expect("stale activation is handled"),
        SelectedActivation::Rejected
    ));
}

#[test]
fn same_buffer_candidates_survive_a_superseding_refresh() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    state
        .buffer
        .set_exact("ec".into(), 2)
        .expect("initial buffer");
    let candidates_context = Arc::new(
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            directory.path().to_owned(),
            state.snapshot().expect("snapshot"),
        )
        .expect("candidate context"),
    );
    let candidate = Candidate::new(
        candidates_context.query_id,
        "echo",
        "candidate",
        Some(TextEdit {
            range: 0..2,
            replacement: "echo".into(),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::Insert,
        CandidateSource::History,
        CandidateKind::History,
        Completeness::Runnable,
        RiskLevel::Low,
        "same-buffer-refresh",
    );
    state.selected = Some(candidate.id);
    state.candidates = vec![candidate];
    state.candidates_context = Some(candidates_context);

    // A background help refresh advances the active query without changing
    // the shell buffer. The visible row must still activate on the first key.
    refresh_context(&mut state, QueryId::new(2));

    assert_eq!(
        match resolve_selected_activation(&state).expect("activation") {
            SelectedActivation::Ready { activation, .. } => activation,
            SelectedActivation::None | SelectedActivation::Rejected => {
                panic!("same-buffer candidate was rejected")
            }
        },
        Activation::ReplaceBuffer {
            text: "echo".into(),
            cursor: 4,
        }
    );
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
fn pagination_capacity_tracks_terminal_height() {
    assert_eq!(
        visible_page_size(12, TerminalSize::new(24, 80).expect("size")),
        10
    );
    assert_eq!(
        visible_page_size(12, TerminalSize::new(5, 80).expect("size")),
        2
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

fn selection_candidate(state: &RuntimeState, primary: &str) -> Candidate {
    Candidate::new(
        QueryId::new(1),
        primary,
        "test",
        Some(TextEdit {
            range: 0..state.buffer.text.len(),
            replacement: primary.into(),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::Insert,
        CandidateSource::History,
        CandidateKind::History,
        Completeness::Runnable,
        RiskLevel::Low,
        primary,
    )
}

#[test]
fn move_selection_from_none_lands_on_the_edges() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    state
        .buffer
        .set_exact("ec".into(), 2)
        .expect("initial buffer");
    state.candidates = (0..25)
        .map(|index| selection_candidate(&state, &format!("echo {index:02}")))
        .collect();
    state.selected = None;

    move_selection(&mut state, 1);
    assert_eq!(state.selected, Some(state.candidates[0].id));

    state.selected = None;
    move_selection(&mut state, -1);
    assert_eq!(state.selected, Some(state.candidates[24].id));

    // Page jumps land on the first row / the start of the last page.
    let page_size = state.page_size as isize;
    state.selected = None;
    move_selection(&mut state, page_size);
    assert_eq!(state.selected, Some(state.candidates[20].id));

    state.selected = None;
    move_selection(&mut state, -page_size);
    assert_eq!(state.selected, Some(state.candidates[0].id));

    // Selection movement still wraps once a selection exists.
    move_selection(&mut state, -1);
    assert_eq!(state.selected, Some(state.candidates[24].id));
}

fn enter_candidate(
    primary: &str,
    action: CandidateAction,
    completeness: Completeness,
    risk: RiskLevel,
) -> Candidate {
    Candidate::new(
        QueryId::new(1),
        primary,
        "test",
        (matches!(
            action,
            CandidateAction::Insert | CandidateAction::InsertAndContinue { .. }
        ))
        .then(|| TextEdit {
            range: 0..2,
            replacement: primary.into(),
            cursor_after: CursorPlacement::End,
        }),
        action,
        CandidateSource::History,
        CandidateKind::History,
        completeness,
        risk,
        primary,
    )
}

#[test]
fn enter_executes_runnable_safe_candidates() {
    let candidate = enter_candidate(
        "echo ok",
        CandidateAction::Insert,
        Completeness::Runnable,
        RiskLevel::Low,
    );
    let activation = Activation::ReplaceBuffer {
        text: "echo ok".into(),
        cursor: 7,
    };
    assert!(matches!(
        resolve_enter(&candidate, &activation),
        EnterResolution::Execute(text) if text == "echo ok"
    ));
}

#[test]
fn enter_degrades_non_runnable_candidates_to_fill() {
    let needs_input = enter_candidate(
        "tar -czf",
        CandidateAction::InsertAndContinue {
            next_slot: crate::completion::SlotKind::File,
        },
        Completeness::NeedsInput {
            slot: crate::completion::SlotKind::File,
        },
        RiskLevel::Low,
    );
    let activation = Activation::ReplaceBuffer {
        text: "tar -czf".into(),
        cursor: 8,
    };
    assert!(matches!(
        resolve_enter(&needs_input, &activation),
        EnterResolution::Fill
    ));

    let ai = enter_candidate(
        "ask ai",
        CandidateAction::RequestAi,
        Completeness::ActionOnly,
        RiskLevel::Low,
    );
    assert!(matches!(
        resolve_enter(&ai, &Activation::RequestAi),
        EnterResolution::Fill
    ));
}

#[test]
fn enter_confirms_when_the_effective_risk_is_dangerous() {
    // Classified risk is stricter than the provider-assigned Low.
    let dangerous = enter_candidate(
        "rm -rf /tmp/x",
        CandidateAction::Insert,
        Completeness::Runnable,
        RiskLevel::Low,
    );
    let activation = Activation::ReplaceBuffer {
        text: "rm -rf /tmp/x".into(),
        cursor: 13,
    };
    match resolve_enter(&dangerous, &activation) {
        EnterResolution::Confirm {
            text,
            risk,
            reasons,
        } => {
            assert_eq!(text, "rm -rf /tmp/x");
            assert_eq!(risk, RiskLevel::High);
            assert!(reasons.contains(&"recursive operation".to_owned()));
            assert!(reasons.contains(&"force flag".to_owned()));
        }
        other => panic!("expected confirmation, got {}", enter_label(&other)),
    }

    // A provider-flagged executable still executes directly: Unknown risk
    // (opaque syntax, unclassified provenance) no longer gates confirmation —
    // only High does.
    let flagged = enter_candidate(
        "ls",
        CandidateAction::Insert,
        Completeness::Runnable,
        RiskLevel::Unknown,
    );
    let activation = Activation::ReplaceBuffer {
        text: "ls".into(),
        cursor: 2,
    };
    assert!(matches!(
        resolve_enter(&flagged, &activation),
        EnterResolution::Execute(_)
    ));

    // Medium risk still executes without confirmation.
    let medium = enter_candidate(
        "rm file",
        CandidateAction::Insert,
        Completeness::Runnable,
        RiskLevel::Low,
    );
    let activation = Activation::ReplaceBuffer {
        text: "rm file".into(),
        cursor: 7,
    };
    assert!(matches!(
        resolve_enter(&medium, &activation),
        EnterResolution::Execute(_)
    ));
}

fn enter_label(resolution: &EnterResolution) -> &'static str {
    match resolution {
        EnterResolution::Fill => "fill",
        EnterResolution::Execute(_) => "execute",
        EnterResolution::Confirm { .. } => "confirm",
    }
}

fn session_token() -> crate::terminal::SessionToken {
    crate::terminal::SessionToken::parse("0123456789abcdef0123456789abcdef")
        .expect("fixture token is valid")
}

fn history_candidate(query_id: QueryId, text: &str) -> Candidate {
    Candidate::new(
        query_id,
        text,
        "from history",
        Some(TextEdit {
            range: 0..0,
            replacement: text.into(),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::Insert,
        CandidateSource::History,
        CandidateKind::History,
        Completeness::Runnable,
        RiskLevel::Low,
        "reselection-test",
    )
}

fn refresh_context(state: &mut RuntimeState, query_id: QueryId) {
    let context = Arc::new(
        CompletionContext::new(
            query_id,
            ShellKind::Zsh,
            state.cwd.clone(),
            state.snapshot().expect("snapshot"),
        )
        .expect("context"),
    );
    state.context = Some(context);
}

fn provider_result(state: &RuntimeState, candidates: Vec<Candidate>) -> ProviderResult {
    ProviderResult {
        context: Arc::clone(state.context.as_ref().expect("context")),
        output: ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        },
        final_batch: true,
    }
}

#[test]
fn navigation_intent_is_reapplied_when_queued_buffer_events_move_the_query() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = crate::terminal::spawn_with_writer(
        Vec::new(),
        session_token(),
        TerminalSize::new(24, 80).expect("terminal size"),
        3,
    )
    .expect("output actor");

    // The user navigates against the visible list…
    state.buffer.set_exact("ec".into(), 2).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    state.candidates = vec![history_candidate(QueryId::new(1), "echo HKSEL_HIDDEN")];
    move_selection(&mut state, 1);
    let first = state.selected.expect("navigation selects a row");

    // …but queued buffer events move the query on before the selection
    // is rendered; fresh candidates carry unrelated per-query ids.
    state
        .buffer
        .set_exact("echo HKSEL_H".into(), 12)
        .expect("buffer");
    refresh_context(&mut state, QueryId::new(2));
    state.selected = None;
    let result = provider_result(
        &state,
        vec![history_candidate(QueryId::new(2), "echo HKSEL_HIDDEN")],
    );
    handle_provider_result(result, &mut state, &output).expect("provider result");

    let reselected = state.selected.expect("intent re-applies the selection");
    assert_ne!(reselected, first, "the fresh candidate has a fresh id");
    assert_eq!(
        state
            .candidates
            .iter()
            .find(|candidate| candidate.id == reselected)
            .map(|candidate| candidate.display.primary.as_str()),
        Some("echo HKSEL_HIDDEN")
    );
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn tab_intent_waits_for_candidates_after_a_queued_buffer_event() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = crate::terminal::spawn_with_writer(
        Vec::new(),
        session_token(),
        TerminalSize::new(24, 80).expect("terminal size"),
        3,
    )
    .expect("output actor");

    // The visible list belongs to `tar`, and Tab selects its first row.
    state.buffer.set_exact("tar".into(), 3).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    state.candidates_context = state.context.as_ref().cloned();
    state.candidates = vec![Candidate::new(
        QueryId::new(1),
        "tar -czf",
        "create archive",
        Some(TextEdit {
            range: 0..3,
            replacement: "tar -czf".into(),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::Insert,
        CandidateSource::CommandSpec,
        CandidateKind::Recipe,
        Completeness::NeedsInput {
            slot: crate::completion::SlotKind::NewFile,
        },
        RiskLevel::Low,
        "queued-tab-old",
    )];
    move_selection(&mut state, 1);

    // The shell's exact buffer event for the already-typed trailing space
    // arrives before the Tab can activate the old row. The one-shot Tab
    // intent must survive until this query's replacement row arrives.
    state
        .buffer
        .set_exact("tar ".into(), 4)
        .expect("newer buffer");
    refresh_context(&mut state, QueryId::new(2));
    state.selected = None;
    state.pending_accept = true;
    let result = provider_result(
        &state,
        vec![Candidate::new(
            QueryId::new(2),
            "tar -czf",
            "create archive",
            Some(TextEdit {
                range: 0..4,
                replacement: "tar -czf".into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::CommandSpec,
            CandidateKind::Recipe,
            Completeness::NeedsInput {
                slot: crate::completion::SlotKind::NewFile,
            },
            RiskLevel::Low,
            "queued-tab-fresh",
        )],
    );
    handle_provider_result(result, &mut state, &output).expect("provider result");

    assert!(state.pending_accept, "fresh results must not drop the Tab");
    assert!(
        state.selected.is_some(),
        "fresh top row should be reselected"
    );
    assert!(matches!(
        resolve_selected_activation(&state).expect("fresh activation"),
        SelectedActivation::Ready {
            activation: Activation::ReplaceBuffer { ref text, cursor: 8 },
            ..
        } if text == "tar -czf"
    ));
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn navigation_intent_falls_back_to_the_delta_when_the_row_vanishes() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = crate::terminal::spawn_with_writer(
        Vec::new(),
        session_token(),
        TerminalSize::new(24, 80).expect("terminal size"),
        3,
    )
    .expect("output actor");

    state.buffer.set_exact("ec".into(), 2).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    state.candidates = vec![history_candidate(QueryId::new(1), "echo gone")];
    move_selection(&mut state, 1);
    state.buffer.set_exact("echo HK".into(), 6).expect("buffer");
    refresh_context(&mut state, QueryId::new(2));
    state.selected = None;
    let result = provider_result(
        &state,
        vec![
            history_candidate(QueryId::new(2), "echo HKSEL_HIDDEN"),
            history_candidate(QueryId::new(2), "echo HKOTHER"),
        ],
    );
    handle_provider_result(result, &mut state, &output).expect("provider result");

    assert_eq!(
        state.selected,
        Some(state.candidates[0].id),
        "Down from nothing lands on the first row of the fresh list"
    );
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn navigation_intent_does_not_create_a_selection_by_itself() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = crate::terminal::spawn_with_writer(
        Vec::new(),
        session_token(),
        TerminalSize::new(24, 80).expect("terminal size"),
        3,
    )
    .expect("output actor");

    // No navigation happened: results must never pre-select a row.
    state.buffer.set_exact("ec".into(), 2).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    let result = provider_result(
        &state,
        vec![history_candidate(QueryId::new(1), "echo HKSEL_HIDDEN")],
    );
    handle_provider_result(result, &mut state, &output).expect("provider result");
    assert_eq!(state.selected, None);
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn deferred_navigation_lands_on_the_first_history_batch() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = crate::terminal::spawn_with_writer(
        Vec::new(),
        session_token(),
        TerminalSize::new(24, 80).expect("terminal size"),
        3,
    )
    .expect("output actor");

    state.buffer.set_exact(String::new(), 0).expect("buffer");
    state.history_only = true;
    refresh_context(&mut state, QueryId::new(1));
    defer_selection(&mut state, 1);
    let result = provider_result(
        &state,
        vec![
            history_candidate(QueryId::new(1), "echo newest"),
            history_candidate(QueryId::new(1), "echo older"),
        ],
    );
    handle_provider_result(result, &mut state, &output).expect("provider result");

    assert_eq!(
        state.selected,
        Some(state.candidates[0].id),
        "the Down key that opened history must select the first row"
    );
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

fn test_output() -> crate::terminal::SpawnedOutput<Vec<u8>> {
    crate::terminal::spawn_with_writer(
        Vec::new(),
        session_token(),
        TerminalSize::new(24, 80).expect("terminal size"),
        3,
    )
    .expect("output actor")
}

fn ai_command(text: &str) -> crate::ai::AiCommand {
    crate::ai::AiCommand {
        command: text.to_owned(),
        explanation: "ai suggestion".to_owned(),
        risk: None,
    }
}

#[test]
fn ai_request_owns_candidates_until_the_next_query() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = test_output();

    state.buffer.set_exact("git ".into(), 4).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    state.candidates = vec![history_candidate(QueryId::new(1), "git status")];

    // The user activates RequestAi: the wait screen takes over the overlay.
    let config = Arc::new(Config::default());
    let (ai_sender, _ai_receiver) = crossbeam_channel::unbounded();
    let context = Arc::clone(state.context.as_ref().expect("context"));
    start_ai_request(&mut state, &context, &config, &ai_sender, &output).expect("ai request");
    assert!(state.ai_query.is_some());
    assert!(state.ai_owns_candidates);
    assert!(state.candidates.is_empty());

    // A provider batch for the same query id still passes the staleness
    // check (the buffer never moved) but must not wipe the AI wait screen.
    let result = provider_result(&state, vec![history_candidate(QueryId::new(1), "git push")]);
    handle_provider_result(result, &mut state, &output).expect("provider result");
    assert!(
        state.candidates.is_empty(),
        "provider batch must not replace the AI wait screen"
    );
    assert_eq!(
        state.status.as_deref(),
        Some("HK-AI-WAIT requesting commands; Esc cancels")
    );

    // The AI result lands and owns the candidate list…
    let generation = state.ai_query.as_ref().expect("active request").generation;
    handle_ai_result(
        AiResult {
            query_id: QueryId::new(1),
            generation,
            result: Ok(vec![ai_command("git commit -m wip")]),
        },
        &mut state,
        &output,
    )
    .expect("ai result");
    assert!(state.ai_query.is_none());
    assert!(
        state.ai_owns_candidates,
        "AI results keep owning the overlay until the next query"
    );
    assert_eq!(
        state
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect::<Vec<_>>(),
        vec!["git commit -m wip"]
    );

    // …and a provider batch arriving after the AI result still must not
    // replace it.
    let result = provider_result(&state, vec![history_candidate(QueryId::new(1), "git push")]);
    handle_provider_result(result, &mut state, &output).expect("provider result");
    assert_eq!(
        state
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect::<Vec<_>>(),
        vec!["git commit -m wip"],
        "late provider batch must not overwrite AI results"
    );

    // The next scheduled query hands the overlay back to providers.
    let worker = ProviderWorker::start(
        Arc::new(crate::completion::CompletionEngine::new(8, 12)),
        None,
    )
    .expect("provider worker");
    state.schedule_query(&worker).expect("schedule query");
    assert!(!state.ai_owns_candidates);
    assert_eq!(state.query_id, QueryId::new(1), "bumped from ZERO");
    let fresh_query = state.context.as_ref().expect("context").query_id;
    let result = provider_result(&state, vec![history_candidate(fresh_query, "git push")]);
    handle_provider_result(result, &mut state, &output).expect("provider result");
    assert_eq!(
        state
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect::<Vec<_>>(),
        vec!["git push"],
        "provider batches are accepted again once the AI window closes"
    );
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn provider_result_is_dropped_when_buffer_sync_is_uncertain() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = test_output();

    state.buffer.set_exact("ec".into(), 2).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    state.candidates = vec![history_candidate(QueryId::new(1), "echo old")];

    // `mark_uncertain` changes neither the revision nor the text, so the
    // late batch below still matches on query id, revision, and hash.
    let result = provider_result(&state, vec![history_candidate(QueryId::new(1), "echo new")]);
    state.buffer.mark_uncertain();
    handle_provider_result(result, &mut state, &output).expect("provider result");
    assert_eq!(
        state
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect::<Vec<_>>(),
        vec!["echo old"],
        "uncertain sync must reject late provider batches"
    );
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn stale_ai_result_never_takes_the_active_request() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = test_output();

    // Two consecutive AI requests share the query id because the buffer
    // never moved between them; request B (generation 2) is active while a
    // late result from the cancelled request A (generation 1) arrives.
    state.buffer.set_exact("git ".into(), 4).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    state.ai_generation = 2;
    state.ai_owns_candidates = true;
    state.ai_query = Some(ActiveAiRequest {
        query_id: QueryId::new(1),
        generation: 2,
        cancel: tokio_util::sync::CancellationToken::new(),
    });
    let active_cancel = state
        .ai_query
        .as_ref()
        .expect("active request")
        .cancel
        .clone();

    handle_ai_result(
        AiResult {
            query_id: QueryId::new(1),
            generation: 1,
            result: Ok(vec![ai_command("git push")]),
        },
        &mut state,
        &output,
    )
    .expect("stale ai result");
    assert!(
        state.ai_query.is_some(),
        "the stale result must not take the active request's slot"
    );
    assert!(
        state.candidates.is_empty(),
        "the stale result must not paint its candidates"
    );

    // The active request is still cancellable…
    state.cancel_ai();
    assert!(active_cancel.is_cancelled());

    // …and its own result is the one that gets accepted.
    state.ai_query = Some(ActiveAiRequest {
        query_id: QueryId::new(1),
        generation: 2,
        cancel: tokio_util::sync::CancellationToken::new(),
    });
    handle_ai_result(
        AiResult {
            query_id: QueryId::new(1),
            generation: 2,
            result: Ok(vec![ai_command("git commit -m wip")]),
        },
        &mut state,
        &output,
    )
    .expect("active ai result");
    assert!(state.ai_query.is_none());
    assert_eq!(
        state
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect::<Vec<_>>(),
        vec!["git commit -m wip"]
    );
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn unready_terminal_arms_repaint_retry_and_probe_recovery() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = test_output();

    state.buffer.set_exact("git ".into(), 4).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    state.candidates = vec![history_candidate(QueryId::new(1), "git status")];

    // A fresh output actor starts with `Unknown` readiness (a lost render
    // gate lands here too): the frame cannot commit, so the owed repaint is
    // armed for the main-loop retry and the cursor-probe re-anchor kicks in.
    render_current(&mut state, &output).expect("render");
    assert!(state.repaint_pending, "retry must be armed");
    assert!(state.need_cpr, "cursor-probe recovery must be armed");

    // Nothing to show at all: neither retry nor probe is armed.
    state.candidates.clear();
    state.repaint_pending = false;
    state.need_cpr = false;
    render_current(&mut state, &output).expect("render");
    assert!(!state.repaint_pending);
    assert!(!state.need_cpr);

    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn auto_update_spawn_requires_enabled_config_and_no_env_opt_out() {
    let mut config = Config::default();
    assert!(should_spawn_auto_update(&config, false));
    assert!(!should_spawn_auto_update(&config, true));
    config.update.enabled = false;
    assert!(!should_spawn_auto_update(&config, false));
    assert!(!should_spawn_auto_update(&config, true));
}
