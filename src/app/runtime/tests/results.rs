use super::*;

#[test]
fn dismissed_overlay_drops_pending_frames_and_late_results_until_reopened_or_edited() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = test_output();
    state.editing = true;
    state.buffer.set_exact("echo ".into(), 5).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    let late = provider_result(
        &state,
        vec![history_candidate(QueryId::new(1), "echo later")],
    );
    state.candidates = vec![history_candidate(QueryId::new(1), "echo first")];
    output
        .allow_cursor_probe(state.buffer.revision)
        .expect("probe");
    output
        .confirm_cursor(crate::terminal::CellPos::new(0, 9))
        .expect("cursor");
    state.scheduler = crate::terminal::LatestFrameScheduler::new(1);
    render_current(&mut state, &output).expect("first frame");
    move_selection(&mut state, 1);
    render_current(&mut state, &output).expect("queued frame");
    assert!(!state.scheduler.is_idle());

    state.dismiss_overlay();
    output.hide_overlay().expect("hide");
    handle_provider_result(late, &mut state, &output).expect("late result");
    render_current(&mut state, &output).expect("redisplay");
    flush_scheduled_frame(&mut state, &output).expect("tick");
    assert!(state.scheduler.is_idle());
    assert!(!state.overlay_visible);
    assert!(!state.repaint_pending);
    assert!(state.candidates.is_empty());
    assert!(state.selected.is_none());
    assert_eq!(state.buffer.text, "echo ");

    let engine = Arc::new(crate::completion::CompletionEngine::new(20, 12));
    let worker = ProviderWorker::start(engine, None).expect("worker");
    state.schedule_query(&worker).expect("background refresh");
    assert!(!state.provider_pending);
    assert!(state.context.is_none());
    state.dismissed_revision = None;
    state.schedule_query(&worker).expect("explicit reopen");
    assert!(state.provider_pending);
    state.dismiss_overlay();
    state.buffer.set_exact("echo n".into(), 6).expect("edit");
    state.schedule_query(&worker).expect("query after edit");
    assert!(state.provider_pending);
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn a_final_frame_invalidated_by_child_output_is_retried_without_another_result() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = test_output();
    state.editing = true;
    state.buffer.set_exact("ec".into(), 2).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    output
        .allow_cursor_probe(state.buffer.revision)
        .expect("probe");
    output
        .confirm_cursor(crate::terminal::CellPos::new(0, 6))
        .expect("cursor");
    state.candidates = vec![history_candidate(QueryId::new(1), "echo final")];
    state.scheduler = crate::terminal::LatestFrameScheduler::new(1);
    render_current(&mut state, &output).expect("initial frame");
    output.barrier().expect("initial commit");
    flush_scheduled_frame(&mut state, &output).expect("acknowledge initial frame");
    assert!(state.in_flight_frame.is_none());

    move_selection(&mut state, 1);
    render_current(&mut state, &output).expect("queue final frame");
    let future = Instant::now() + Duration::from_secs(2);
    let (_, frame) = state.scheduler.take_ready(future).expect("queued frame");
    let stale_ticket = frame.ticket;
    // Advance the screen after layout but before the output actor admits the
    // frame. Its query is still current, but its screen ticket is unusable.
    output
        .child_output(crate::terminal::ChildOutputBatch {
            bytes: b"x".to_vec(),
            read_cycle: 1,
            drain: crate::terminal::DrainState::DrainedToEagain,
        })
        .expect("late shell output");
    output.commit_latest(frame).expect("submit stale frame");
    state.in_flight_frame = Some(stale_ticket);
    output.barrier().expect("actor rejected stale ticket");
    assert_ne!(
        output.state().expect("state").last_committed_frame,
        Some(stale_ticket),
    );

    flush_scheduled_frame(&mut state, &output).expect("retry without provider or PTY event");
    assert!(state.frame_revision > stale_ticket.frame_revision);
    let (_, replacement) = state
        .scheduler
        .take_ready(future + Duration::from_secs(2))
        .expect("fresh replacement frame");
    let fresh_ticket = replacement.ticket;
    output.commit_latest(replacement).expect("replacement");
    state.in_flight_frame = Some(fresh_ticket);
    output.barrier().expect("replacement committed");
    flush_scheduled_frame(&mut state, &output).expect("acknowledge replacement");
    assert_eq!(
        output.state().expect("state").last_committed_frame,
        Some(fresh_ticket),
    );
    assert!(state.in_flight_frame.is_none());
    assert!(state.scheduler.is_idle());
    assert_eq!(state.frame_revision, fresh_ticket.frame_revision);
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn final_empty_result_discards_queued_frames_so_the_list_cannot_reappear() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = test_output();
    state.editing = true;
    state.buffer.set_exact("ec".into(), 2).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    output
        .allow_cursor_probe(state.buffer.revision)
        .expect("probe");
    output
        .confirm_cursor(crate::terminal::CellPos::new(0, 6))
        .expect("cursor");
    state.scheduler = crate::terminal::LatestFrameScheduler::new(1);
    state.candidates = vec![history_candidate(QueryId::new(1), "echo old")];
    render_current(&mut state, &output).expect("first frame");
    move_selection(&mut state, 1);
    render_current(&mut state, &output).expect("queued frame");
    assert!(!state.scheduler.is_idle());

    handle_provider_result(provider_result(&state, Vec::new()), &mut state, &output)
        .expect("final empty result");
    assert!(!state.overlay_visible);
    assert!(!state.repaint_pending);
    assert!(!state.provider_pending);
    assert!(state.scheduler.is_idle());
    flush_scheduled_frame(&mut state, &output).expect("tick");
    assert!(!state.overlay_visible);
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}

#[test]
fn info_level_diagnostics_stay_out_of_the_status_line() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, _join) = test_output();

    state.buffer.set_exact("x".into(), 1).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));

    // A budget-cutoff note is informational: the row set is complete as far
    // as the budget allowed, so nothing reaches the status line.
    let mut info_only = provider_result(&state, Vec::new());
    info_only
        .output
        .diagnostics
        .push(crate::completion::ProviderDiagnostic {
            provider: "engine",
            code: "HK-CMP-001",
            level: crate::completion::DiagnosticLevel::Info,
            message: "local provider budget reached after 100 ms".into(),
        });
    handle_provider_result(info_only, &mut state, &output).expect("provider result");
    assert!(state.status.is_none());

    // A provider failure still surfaces: without it, an empty or partial
    // row set would look like a correct answer.
    let mut warning = provider_result(&state, Vec::new());
    warning
        .output
        .diagnostics
        .push(crate::completion::ProviderDiagnostic {
            provider: "engine",
            code: "HK-CMP-002",
            level: crate::completion::DiagnosticLevel::Warning,
            message: "provider x failed internally".into(),
        });
    handle_provider_result(warning, &mut state, &output).expect("provider result");
    assert_eq!(
        state.status.as_deref(),
        Some("provider x failed internally (HK-CMP-002)")
    );
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
