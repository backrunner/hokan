use super::*;

#[test]
fn quality_is_deterministic() {
    assert!(match_quality("git", "git status") > match_quality("git", "rg item"));
    assert!(match_quality("gco", "git checkout") > 0);
    assert_eq!(match_quality("xyz", "git status"), 0);
}

#[test]
fn candidate_id_is_orderable_for_stable_ties() {
    assert!(crate::completion::CandidateId(1) < crate::completion::CandidateId(2));
}

#[test]
fn filters_candidates_with_unsafe_or_invalid_edits() {
    let buffer =
        BufferSnapshot::new("x", 1, BufferRevision::new(1), SyncQuality::Exact).expect("buffer");
    let context = CompletionContext::new(
        QueryId::new(1),
        ShellKind::Zsh,
        PathBuf::from("/tmp"),
        buffer,
    )
    .expect("context");
    let candidate = |replacement: &str, range| {
        Candidate::new(
            context.query_id,
            replacement,
            "history",
            Some(TextEdit {
                range,
                replacement: replacement.into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::History,
            CandidateKind::History,
            Completeness::Runnable,
            RiskLevel::Low,
            replacement,
        )
    };

    let ranked = rank_and_dedupe(
        &context,
        vec![
            candidate("x-safe", 0..1),
            candidate("x\nunsafe", 0..1),
            candidate("x-invalid", 0..2),
        ],
        10,
    );
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].display.primary, "x-safe");
}

#[test]
fn escapes_control_characters_in_every_rendered_candidate_field() {
    let context = buffer_context("x");
    let mut candidate = history_candidate(&context, "x-safe", 1);
    candidate.display.primary = "x\nprimary".into();
    candidate.display.description = "line\tdescription".into();
    candidate.display.annotation = Some("origin\u{1b}".into());

    let ranked = rank_and_dedupe(&context, vec![candidate], 10);
    assert_eq!(ranked[0].display.primary, "x\\nprimary");
    assert_eq!(ranked[0].display.description, "line\\tdescription");
    assert_eq!(
        ranked[0].display.annotation.as_deref(),
        Some("origin\\u{1b}")
    );
}

#[test]
fn executable_name_beats_same_family_history_at_the_command_slot() {
    let context = buffer_context("cod");
    let mut history = history_candidate(&context, "codex --resume latest", 3);
    history.score.cwd_affinity = 100;
    history.score.frecency = 200;
    history.score.transition = 200;
    let executable = Candidate::new(
        context.query_id,
        "codex",
        "PATH command",
        Some(TextEdit {
            range: 0..3,
            replacement: "codex".into(),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::Insert,
        CandidateSource::PathCommand,
        CandidateKind::Command,
        Completeness::Runnable,
        RiskLevel::Unknown,
        "path:codex",
    );

    let ranked = rank_and_dedupe(&context, vec![history, executable], 10);
    assert_eq!(ranked[0].display.primary, "codex");
    assert_eq!(ranked[0].score.command_priority, 1);
    assert_eq!(ranked[1].score.command_priority, 0);
}

#[test]
fn workspace_bonus_is_applied_centrally_to_ranked_candidates() {
    let markers = WorkspaceMarkers {
        git: true,
        ..WorkspaceMarkers::default()
    };
    let context = buffer_context("git st").with_workspace(markers);
    let ranked = rank_and_dedupe(
        &context,
        vec![history_candidate(&context, "git status", 6)],
        10,
    );
    assert_eq!(ranked[0].score.context, 40);

    let plain = buffer_context("git st");
    let ranked = rank_and_dedupe(&plain, vec![history_candidate(&plain, "git status", 6)], 10);
    assert_eq!(ranked[0].score.context, 0);
}

#[test]
fn failed_penalty_pushes_recently_failed_commands_down() {
    let context = buffer_context("x");
    let mut failed = history_candidate(&context, "x-failed", 1);
    failed.score.failed_penalty = 150;
    failed.score.frecency = 200;
    let mut healthy = history_candidate(&context, "x-healthy", 1);
    healthy.score.frecency = 100;
    let ranked = rank_and_dedupe(&context, vec![failed, healthy], 10);
    assert_eq!(ranked[0].display.primary, "x-healthy");
}

#[test]
fn equal_score_ties_keep_a_stable_order_across_query_ids() {
    let first = recipe_order(1);
    let second = recipe_order(2);
    assert_eq!(
        first, second,
        "tie order must not depend on the query id hash"
    );
    assert_eq!(
        first,
        vec!["build", "dev", "test"],
        "ties resolve on display.primary"
    );
}

#[test]
fn directories_are_not_penalized_for_needing_input() {
    let context = buffer_context("src/");
    let path_candidate = |name: &str, kind, completeness| {
        Candidate::new(
            context.query_id,
            name,
            "filesystem",
            Some(TextEdit {
                range: 0..4,
                replacement: name.into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::InsertAndContinue {
                next_slot: SlotKind::Path,
            },
            CandidateSource::Filesystem,
            kind,
            completeness,
            RiskLevel::ReadOnly,
            "filesystem",
        )
    };
    let ranked = rank_and_dedupe(
        &context,
        vec![
            // Equal-length names keep match_quality identical so only the
            // completeness penalty can separate the two rows.
            path_candidate(
                "src/adir",
                CandidateKind::Directory,
                Completeness::NeedsInput {
                    slot: SlotKind::Path,
                },
            ),
            path_candidate("src/bfil", CandidateKind::File, Completeness::Runnable),
        ],
        10,
    );
    assert_eq!(ranked.len(), 2);
    let directory = &ranked[0];
    assert_eq!(directory.kind, CandidateKind::Directory);
    assert_eq!(directory.score.incomplete_penalty, 0);
    assert_eq!(
        ranked[0].score.total(),
        ranked[1].score.total(),
        "directory ranks level with the file at a path slot"
    );
}
