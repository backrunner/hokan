use super::*;

#[test]
fn executable_argument_merges_matching_history_and_ranks_it_first() {
    let context = buffer_context("which co");
    let mut history = history_candidate(&context, "which codex", context.buffer.text.len());
    history.score.cwd_affinity = 100;
    history.score.frecency = 80;
    history.score.transition = 120;
    history.score.failed_penalty = 150;
    let executable = |name: &str| {
        Candidate::new(
            context.query_id,
            format!("which {name}"),
            "PATH command",
            Some(TextEdit {
                range: context.parsed.replacement.clone(),
                replacement: name.into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::PathCommand,
            CandidateKind::Command,
            Completeness::Runnable,
            RiskLevel::Unknown,
            format!("path:{name}"),
        )
    };

    let ranked = rank_and_dedupe(
        &context,
        vec![executable("codex"), executable("code"), history],
        10,
    );
    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0].display.primary, "which codex");
    assert_eq!(ranked[0].source, CandidateSource::PathCommand);
    assert_eq!(ranked[0].score.command_priority, 1);
    assert_eq!(ranked[0].score.history_overlap, HISTORY_OVERLAP_BONUS);
    assert_eq!(ranked[0].score.cwd_affinity, 100);
    assert_eq!(ranked[0].score.frecency, 80);
    assert_eq!(ranked[0].score.transition, 120);
    assert_eq!(ranked[0].score.failed_penalty, 150);
    assert_eq!(
        ranked[0].edit.as_ref().map(|edit| edit.range.clone()),
        Some(context.parsed.replacement.clone()),
        "the PATH candidate's token-level edit semantics must survive the merge"
    );
    assert_eq!(ranked[1].display.primary, "which code");
}

#[test]
fn stricter_risk_keeps_the_more_severe_level() {
    assert_eq!(
        stricter_risk(RiskLevel::Unknown, RiskLevel::High),
        RiskLevel::High
    );
    assert_eq!(
        stricter_risk(RiskLevel::High, RiskLevel::Unknown),
        RiskLevel::High
    );
    assert_eq!(
        stricter_risk(RiskLevel::Low, RiskLevel::High),
        RiskLevel::High
    );
    assert_eq!(
        stricter_risk(RiskLevel::Unknown, RiskLevel::Medium),
        RiskLevel::Unknown
    );
    assert_eq!(
        stricter_risk(RiskLevel::ReadOnly, RiskLevel::ReadOnly),
        RiskLevel::ReadOnly
    );
}

#[test]
fn merged_candidate_penalty_matches_the_merged_risk() {
    let risky_duplicate = |context: &CompletionContext, risk: RiskLevel| {
        let mut candidate = history_candidate(context, "x-duplicate", 1);
        candidate.risk = risk;
        candidate
    };

    // Lose branch: the kept row's risk is raised by the merged duplicate.
    let context = buffer_context("x");
    let ranked = rank_and_dedupe(
        &context,
        vec![
            risky_duplicate(&context, RiskLevel::Low),
            risky_duplicate(&context, RiskLevel::High),
        ],
        10,
    );
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].risk, RiskLevel::High);
    assert_eq!(
        ranked[0].score.risk_penalty,
        risk_penalty(RiskLevel::High),
        "kept row's penalty must match the merged risk"
    );

    // Win branch: the replacement row inherits the stricter merged risk.
    let ranked = rank_and_dedupe(
        &context,
        vec![
            risky_duplicate(&context, RiskLevel::High),
            risky_duplicate(&context, RiskLevel::Low),
        ],
        10,
    );
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].risk, RiskLevel::High);
    assert_eq!(
        ranked[0].score.risk_penalty,
        risk_penalty(RiskLevel::High),
        "replacement row's penalty must match the merged risk"
    );
}
