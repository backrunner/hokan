use super::*;

#[test]
fn shutdown_settles_an_inflight_cursor_query_without_reprobing() {
    let mut router = crate::terminal::TerminalReplyRouter::default();
    router
        .register(
            TerminalQueryKind::CursorPositionPrivate,
            Instant::now(),
            TERMINAL_QUERY_TIMEOUT,
        )
        .expect("cursor query");
    let (sender, input) = crossbeam_channel::unbounded();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            sender.send(b"\x1b[?1;1R".to_vec()).expect("reply");
        });
        drain_terminal_queries_before_exit(&mut router, &input);
        assert!(!router.has_outstanding());
        assert!(
            input.try_recv().is_err(),
            "shutdown left the cursor response unread"
        );
    });
    assert!(
        input.try_recv().is_err(),
        "shutdown returned before the reply arrived"
    );
}

#[test]
fn shutdown_does_not_wait_forever_for_an_unresponsive_terminal() {
    let mut router = crate::terminal::TerminalReplyRouter::default();
    router
        .register(
            TerminalQueryKind::CursorPositionPrivate,
            Instant::now(),
            Duration::ZERO,
        )
        .expect("expired query");
    let (_sender, input) = crossbeam_channel::unbounded();
    drain_terminal_queries_before_exit(&mut router, &input);
    assert!(!router.has_outstanding());
}

#[test]
fn terminal_cursor_replies_cannot_rewind_a_newer_prompt_or_buffer() {
    for change in ["screen", "buffer", "epoch"] {
        let directory = tempfile::tempdir().expect("directory");
        let mut state = runtime_state(directory.path());
        let (output, join) = test_output();
        state.editing = true;
        state.need_cpr = true;
        let mut router = crate::terminal::TerminalReplyRouter::default();
        output
            .allow_cursor_probe(state.buffer.revision)
            .expect("probe enabled");
        maybe_probe_cursor(&mut state, &mut router, None, &output).expect("first query");
        assert!(state.pending_terminal_cursor.is_some());
        match change {
            "screen" => output
                .child_output(crate::terminal::ChildOutputBatch {
                    bytes: b"\r\nnew prompt> ".to_vec(),
                    read_cycle: 1,
                    drain: crate::terminal::DrainState::DrainedToEagain,
                })
                .expect("new output"),
            "buffer" => {
                state.buffer.set_exact("x".into(), 1).expect("new buffer");
            }
            _ => output.invalidate_anchor().expect("new epoch"),
        }
        let before = output.state().expect("before reply");
        let reply = router
            .route(b"\x1b[?1;1R", Instant::now())
            .replies
            .pop()
            .expect("old cursor reply");
        assert!(
            !handle_terminal_reply(reply, &mut state, &output).expect("stale reply"),
            "accepted after {change}"
        );
        assert_eq!(output.state().expect("after reply").cursor, before.cursor);
        assert!(state.need_cpr);

        maybe_probe_cursor(&mut state, &mut router, None, &output).expect("replacement query");
        let position = output.state().expect("current position").cursor;
        let response = format!("\x1b[?{};{}R", position.row + 1, position.col + 1);
        let reply = router
            .route(response.as_bytes(), Instant::now())
            .replies
            .pop()
            .expect("fresh reply");
        assert!(handle_terminal_reply(reply, &mut state, &output).expect("current reply"));
        assert_eq!(
            output.state().expect("anchor").confidence,
            crate::terminal::AnchorConfidence::Exact
        );
        output.restore_and_exit().expect("shutdown");
        join.join().expect("actor joins").expect("actor exits");
    }
}

#[test]
fn initial_prompt_does_not_reclaim_a_command_already_queued_by_enter() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    state.queued_startup_enter = true;
    state.foreground_process = true;
    let (output, join) = test_output();
    output.set_foreground(true).expect("queued Enter");
    let worker = ProviderWorker::start(
        Arc::new(crate::completion::CompletionEngine::new(20, 12)),
        None,
    )
    .expect("worker");
    let store = HistoryStore::open(&directory.path().join("state")).expect("store");
    let history = Arc::new(RwLock::new(HistoryIndex::default()));
    let policy = HistoryPolicy::new(1024, &[]).expect("policy");
    for boundary in 1..=2 {
        handle_control_message(
            ControlMessage::Event(ShellEvent::Prompt {
                boundary_id: BoundaryId::new(boundary),
                cwd: directory.path().to_owned(),
                history_control: None,
            }),
            &mut state,
            &output,
            &worker,
            &store,
            &history,
            &policy,
        )
        .expect("prompt");
        let queued = boundary == 1;
        assert_eq!(state.foreground_process, queued);
        assert_eq!(output.state().expect("output state").foreground, queued);
        assert_eq!(state.editing, !queued);
        assert_eq!(state.need_cpr, !queued);
        assert!(!state.queued_startup_enter);
    }
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn private_cursor_timeout_uses_guarded_standard_fallback() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    state.editing = true;
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

    assert_eq!(
        state.cursor_probe_backend,
        CursorProbeBackend::TerminalStandardGuarded
    );
    assert!(state.need_cpr);

    handle_terminal_reply(
        TerminalReply::Timeout {
            query_id: QueryId::new(2),
            kind: TerminalQueryKind::CursorPositionStandardGuarded,
        },
        &mut state,
        &output,
    )
    .expect("guarded cursor timeout");

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
