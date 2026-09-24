use super::*;

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

#[test]
fn unknown_risk_cannot_override_a_known_dangerous_execution() {
    for (text, risk) in [
        ("rm -rf ./build", RiskLevel::Unknown),
        ("echo $(date)", RiskLevel::High),
        ("rm -rf \"$(pwd)/build\"", RiskLevel::Low),
        ("eval payload; rm -rf ./build", RiskLevel::Low),
    ] {
        let candidate =
            enter_candidate(text, CandidateAction::Insert, Completeness::Runnable, risk);
        let activation = Activation::ReplaceBuffer {
            text: text.into(),
            cursor: text.len(),
        };
        assert!(
            matches!(
                resolve_enter(&candidate, &activation),
                EnterResolution::Confirm {
                    risk: RiskLevel::High,
                    ..
                }
            ),
            "confirmation missing for {text:?}"
        );
    }
}

#[test]
fn empty_partial_results_preserve_the_visible_list_and_its_activation_context() {
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
    let mut candidate = history_candidate(QueryId::new(1), "echo original");
    candidate.edit.as_mut().expect("edit").range = 0..2;
    handle_provider_result(
        provider_result(&state, vec![candidate]),
        &mut state,
        &output,
    )
    .expect("initial result");
    assert!(state.overlay_visible);
    let previous_frame = state.frame_revision;

    state.buffer.set_exact("ech".into(), 3).expect("new buffer");
    refresh_context(&mut state, QueryId::new(2));
    state.provider_pending = true;
    let mut empty = provider_result(&state, Vec::new());
    empty.final_batch = false;
    handle_provider_result(empty, &mut state, &output).expect("partial empty result");
    assert!(state.overlay_visible);
    assert!(state.provider_pending);
    assert_eq!(state.frame_revision, previous_frame);
    assert_eq!(state.candidates[0].display.primary, "echo original");
    assert_eq!(
        state
            .candidates_context
            .as_ref()
            .expect("list context")
            .query_id,
        QueryId::new(1),
    );
    state.selected = Some(state.candidates[0].id);
    assert!(matches!(
        resolve_selected_activation(&state).expect("activation"),
        SelectedActivation::Rejected,
    ));

    let mut replacement = history_candidate(QueryId::new(2), "echo refreshed");
    replacement.edit.as_mut().expect("edit").range = 0..3;
    handle_provider_result(
        provider_result(&state, vec![replacement]),
        &mut state,
        &output,
    )
    .expect("final result");
    assert_eq!(state.candidates[0].display.primary, "echo refreshed");
    assert!(!state.provider_pending);
    output.restore_and_exit().expect("shutdown");
    join.join().expect("actor joins").expect("actor exits");
}
