use super::*;

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
fn repeated_history_arrows_are_retained_before_the_first_result() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    state.history_navigation = true;
    defer_selection(&mut state, -1);
    defer_selection(&mut state, -1);
    defer_selection(&mut state, -1);
    let delta = state.selection_intent.as_ref().expect("intent").delta;
    assert_eq!(state::history_landing_row(80, delta), 2);
    defer_selection(&mut state, 1);
    let delta = state.selection_intent.as_ref().expect("intent").delta;
    assert_eq!(state::history_landing_row(80, delta), 1);
    for _ in 0..4 {
        defer_selection(&mut state, 1);
    }
    defer_selection(&mut state, -1);
    let delta = state.selection_intent.as_ref().expect("intent").delta;
    assert_eq!(state::history_landing_row(80, delta), 78);

    // A full cycle also works before any candidates have arrived.
    for _ in 0..80 {
        defer_selection(&mut state, -1);
    }
    let delta = state.selection_intent.as_ref().expect("intent").delta;
    assert_eq!(state::history_landing_row(80, delta), 78);
    assert_eq!(state::history_landing_row(1, delta), 0);
    assert_eq!(state::history_landing_row(0, delta), 0);
}

#[test]
fn history_arrows_and_pages_wrap_at_both_ends() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    state.history_navigation = true;
    state.candidates = (0..25)
        .map(|index| selection_candidate(&state, &format!("echo {index:02}")))
        .collect();

    for index in 0..25 {
        move_selection(&mut state, -1);
        assert_eq!(state.selected, Some(state.candidates[index].id));
    }
    move_selection(&mut state, -1);
    assert_eq!(state.selected, Some(state.candidates[0].id));
    move_selection(&mut state, 1);
    assert_eq!(state.selected, Some(state.candidates[24].id));
    let page_size = state.page_size as isize;
    move_selection(&mut state, page_size);
    assert_eq!(state.selected, Some(state.candidates[14].id));
    for index in (0..14).rev() {
        move_selection(&mut state, 1);
        assert_eq!(state.selected, Some(state.candidates[index].id));
    }
    move_selection(&mut state, 1);
    assert_eq!(state.selected, Some(state.candidates[24].id));
    move_selection(&mut state, -page_size);
    assert_eq!(state.selected, Some(state.candidates[9].id));
    move_selection(&mut state, page_size);
    assert_eq!(state.selected, Some(state.candidates[24].id));

    // Either opening arrow still starts at the newest command.
    state.selected = None;
    move_selection(&mut state, 1);
    assert_eq!(state.selected, Some(state.candidates[0].id));
}

#[test]
fn ordinary_completion_visits_every_row_and_wraps_in_both_directions() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    for count in [0, 1, 25, 1_205] {
        state.selected = None;
        state.candidates = (0..count)
            .map(|index| selection_candidate(&state, &format!("echo {index:04}")))
            .collect();
        if count == 0 {
            move_selection(&mut state, 1);
            move_selection(&mut state, -1);
            assert_eq!(state.selected, None);
            continue;
        }
        for index in 0..count {
            move_selection(&mut state, 1);
            assert_eq!(state.selected, Some(state.candidates[index].id));
        }
        move_selection(&mut state, 1);
        assert_eq!(state.selected, Some(state.candidates[0].id));
        for index in (0..count).rev() {
            move_selection(&mut state, -1);
            assert_eq!(state.selected, Some(state.candidates[index].id));
        }
        let page_size = state.page_size as isize;
        move_selection(&mut state, -page_size);
        move_selection(&mut state, page_size);
        assert_eq!(state.selected, Some(state.candidates[0].id));
    }
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
    assert_eq!(state.selected, Some(state.candidates[0].id));

    state.selected = None;
    move_selection(&mut state, -page_size);
    assert_eq!(state.selected, Some(state.candidates[20].id));

    // Selection movement still wraps once a selection exists.
    state.selected = Some(state.candidates[0].id);
    move_selection(&mut state, -1);
    assert_eq!(state.selected, Some(state.candidates[24].id));
}

#[test]
fn partial_refresh_waits_one_frame_but_final_results_and_navigation_are_immediate() {
    for trigger in ["final", "navigation", "deadline"] {
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
        state.candidates = vec![history_candidate(QueryId::new(1), "echo original")];
        render_current(&mut state, &output).expect("initial list");
        let previous_frame = state.frame_revision;
        refresh_context(&mut state, QueryId::new(2));
        // Use a controlled deadline so host scheduling cannot turn the test
        // into a timing race. Production arms this for only one frame.
        state.provider_batch_deadline = Some(Instant::now() + Duration::from_secs(60));
        let mut partial = provider_result(
            &state,
            vec![history_candidate(QueryId::new(2), "echo partial")],
        );
        partial.final_batch = false;
        handle_provider_result(partial, &mut state, &output).expect("partial result");
        flush_scheduled_frame(&mut state, &output).expect("early tick");
        assert!(state.overlay_visible);
        assert!(state.repaint_pending);
        assert_eq!(state.frame_revision, previous_frame);

        match trigger {
            "final" => {
                let final_result = provider_result(
                    &state,
                    vec![history_candidate(QueryId::new(2), "echo final")],
                );
                handle_provider_result(final_result, &mut state, &output).expect("final result");
                assert!(state.provider_batch_deadline.is_none());
                assert_eq!(state.candidates[0].display.primary, "echo final");
            }
            "navigation" => {
                move_selection(&mut state, 1);
                render_current(&mut state, &output).expect("navigation");
            }
            _ => {
                state.provider_batch_deadline = Some(Instant::now());
                flush_scheduled_frame(&mut state, &output).expect("deadline tick");
            }
        }
        assert!(state.overlay_visible, "failed to paint after {trigger}");
        assert!(!state.repaint_pending);
        assert!(state.frame_revision > previous_frame);
        output.restore_and_exit().expect("shutdown");
        join.join().expect("actor joins").expect("actor exits");
    }
}

#[test]
fn selection_survives_a_later_source_replacing_the_same_command() {
    let directory = tempfile::tempdir().expect("directory");
    let mut state = runtime_state(directory.path());
    let (output, join) = test_output();
    state.buffer.set_exact("git ch".into(), 6).expect("buffer");
    refresh_context(&mut state, QueryId::new(1));
    let first = selection_candidate(&state, "git cherry");
    let selected = selection_candidate(&state, "git checkout");
    handle_provider_result(
        provider_result(&state, vec![first.clone(), selected]),
        &mut state,
        &output,
    )
    .expect("first batch");
    move_selection(&mut state, 1);
    move_selection(&mut state, 1);
    let replacement = Candidate::new(
        QueryId::new(1),
        "git checkout",
        "from help",
        Some(TextEdit {
            range: 4..6,
            replacement: "checkout".into(),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::InsertAndContinue {
            next_slot: crate::completion::SlotKind::Path,
        },
        CandidateSource::CommandHelp,
        CandidateKind::Command,
        Completeness::NeedsInput {
            slot: crate::completion::SlotKind::Path,
        },
        RiskLevel::Low,
        "help:git",
    );
    let expected = replacement.id;
    handle_provider_result(
        provider_result(&state, vec![first, replacement]),
        &mut state,
        &output,
    )
    .expect("merged batch");
    assert_eq!(state.selected, Some(expected));
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
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
