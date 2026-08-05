use std::collections::HashMap;

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, Completeness, CompletionContext, CursorPlacement,
    },
    parser::apply_edit,
    project::WorkspaceMarkers,
    terminal::RiskLevel,
};

pub fn rank_and_dedupe(
    context: &CompletionContext,
    candidates: Vec<Candidate>,
    limit: usize,
) -> Vec<Candidate> {
    let query = context.parsed.current_prefix.as_str();
    let mut deduped: HashMap<(usize, usize, String), Candidate> = HashMap::new();
    for mut candidate in candidates {
        if candidate.query_id != context.query_id || !has_valid_edit(context, &candidate) {
            continue;
        }
        if produces_current_buffer(context, &candidate) {
            continue;
        }
        let replacement_target = candidate
            .edit
            .as_ref()
            .map_or(candidate.display.primary.as_str(), |edit| {
                edit.replacement.as_str()
            });
        candidate.score.match_quality = match_quality(query, replacement_target)
            .max(match_quality(query, &candidate.display.primary));
        if !query.is_empty() && candidate.score.match_quality == 0 {
            if matches!(
                candidate.source,
                crate::completion::CandidateSource::Action
                    | crate::completion::CandidateSource::Ai
                    | crate::completion::CandidateSource::Diagnostic
            ) {
                candidate.score.match_quality = 400;
            } else {
                continue;
            }
        }
        candidate.score.source_trust = candidate.source.trust();
        candidate.score.context = workspace_bonus(context.workspace, replacement_target).max(
            workspace_bonus(context.workspace, &candidate.display.primary),
        );
        candidate.score.risk_penalty = risk_penalty(candidate.risk);
        candidate.score.incomplete_penalty = match candidate.completeness {
            Completeness::Runnable => 0,
            // Directories are always NeedsInput because descending into them IS
            // the interaction — the penalty would sink them below files.
            Completeness::NeedsInput { .. } if candidate.kind == CandidateKind::Directory => 0,
            Completeness::NeedsInput { .. } => 60,
            Completeness::ActionOnly => 80,
        };
        let key = candidate.edit.as_ref().map_or_else(
            || (usize::MAX, usize::MAX, candidate.display.primary.clone()),
            |edit| (edit.range.start, edit.range.end, edit.replacement.clone()),
        );
        match deduped.get_mut(&key) {
            Some(existing) => {
                if candidate.score.total() > existing.score.total() {
                    let stricter = stricter_risk(existing.risk, candidate.risk);
                    *existing = candidate;
                    existing.risk = stricter;
                } else {
                    existing.risk = stricter_risk(existing.risk, candidate.risk);
                }
                // The merge may have raised the risk level; keep the displayed
                // score in sync with the risk the merged row now carries.
                existing.score.risk_penalty = risk_penalty(existing.risk);
            }
            None => {
                deduped.insert(key, candidate);
            }
        }
    }
    let mut candidates: Vec<_> = deduped.into_values().collect();
    // `deduped` is a HashMap, so only the sort keys stabilize the output order.
    // Ties fall back to deterministic content keys — never the candidate id,
    // which hashes the query id and would re-shuffle every keystroke.
    candidates.sort_by(|left, right| {
        right
            .score
            .total()
            .cmp(&left.score.total())
            .then_with(|| left.source.order().cmp(&right.source.order()))
            .then_with(|| left.display.primary.cmp(&right.display.primary))
            .then_with(|| left.provenance.cmp(&right.provenance))
    });
    candidates.truncate(limit);
    candidates
}

/// A candidate whose FULL resulting buffer equals what is already typed adds
/// nothing — accepting it would rewrite the edit line to itself (the classic
/// case is the spec "bare command" row duplicating the user's input). The
/// comparison is trim-normalized so trailing-whitespace near-misses count as
/// identical too.
fn produces_current_buffer(context: &CompletionContext, candidate: &Candidate) -> bool {
    let resulting = match candidate.edit.as_ref() {
        Some(edit) => match apply_edit(&context.buffer.text, edit.range.clone(), &edit.replacement)
        {
            Ok(text) => text,
            Err(_) => return false,
        },
        None => candidate.display.primary.clone(),
    };
    resulting.trim() == context.buffer.text.trim()
}

fn has_valid_edit(context: &CompletionContext, candidate: &Candidate) -> bool {
    let Some(edit) = candidate.edit.as_ref() else {
        return !matches!(
            candidate.action,
            CandidateAction::Insert | CandidateAction::InsertAndContinue { .. }
        );
    };
    if edit.range.start > edit.range.end
        || edit.range.end > context.buffer.text.len()
        || !context.buffer.text.is_char_boundary(edit.range.start)
        || !context.buffer.text.is_char_boundary(edit.range.end)
        || edit.replacement.chars().any(char::is_control)
    {
        return false;
    }
    match edit.cursor_after {
        CursorPlacement::End => true,
        CursorPlacement::Offset(offset) => {
            offset <= edit.replacement.len() && edit.replacement.is_char_boundary(offset)
        }
    }
}

#[must_use]
pub fn match_quality(query: &str, candidate: &str) -> i16 {
    if query.is_empty() {
        return 500;
    }
    let query = query.to_lowercase();
    let candidate = candidate.to_lowercase();
    match_quality_folded(&query, &candidate)
}

#[must_use]
pub(crate) fn match_quality_folded(query: &str, candidate: &str) -> i16 {
    if query.is_empty() {
        return 500;
    }
    if candidate == query {
        1000
    } else if candidate.starts_with(query) {
        900_i16.saturating_sub((candidate.len() - query.len()).min(200) as i16)
    } else if let Some(index) = candidate.find(query) {
        700_i16.saturating_sub(index.min(200) as i16)
    } else if is_subsequence(query, candidate) {
        450
    } else {
        0
    }
}

fn is_subsequence(query: &str, candidate: &str) -> bool {
    let mut query = query.chars();
    let mut expected = query.next();
    for character in candidate.chars() {
        if Some(character) == expected {
            expected = query.next();
            if expected.is_none() {
                return true;
            }
        }
    }
    false
}

/// Project-context bonus: commands that match the detected workspace markers
/// score higher so e.g. `git` wins inside a git repository and `npm run`
/// inside a Node package. Applied centrally so every source benefits. Each
/// matched rule adds 40; the sum is clamped to [0, 100] (the rules key on
/// the first token and are therefore mutually exclusive in practice — the
/// clamp is defensive).
fn workspace_bonus(markers: WorkspaceMarkers, command: &str) -> i16 {
    let command = command.trim_start();
    let mut bonus = 0_i16;
    if markers.git && command.starts_with("git ") {
        bonus += 40;
    }
    if markers.package_json {
        let mut tokens = command.split_whitespace();
        if matches!(tokens.next(), Some("npm" | "pnpm" | "yarn" | "bun"))
            && matches!(
                tokens.next(),
                Some("run" | "test" | "start" | "build" | "dev")
            )
        {
            bonus += 40;
        }
    }
    if markers.cargo_toml && command.starts_with("cargo ") {
        bonus += 40;
    }
    if markers.makefile && command.starts_with("make ") {
        bonus += 40;
    }
    if markers.justfile && command.starts_with("just ") {
        bonus += 40;
    }
    bonus.clamp(0, 100)
}

const fn risk_penalty(risk: RiskLevel) -> i16 {
    match risk {
        RiskLevel::ReadOnly => 0,
        RiskLevel::Low => 20,
        RiskLevel::Medium => 100,
        RiskLevel::High => 250,
        RiskLevel::Unknown => 300,
    }
}

#[must_use]
pub const fn stricter_risk(left: RiskLevel, right: RiskLevel) -> RiskLevel {
    if risk_severity(left) >= risk_severity(right) {
        left
    } else {
        right
    }
}

const fn risk_severity(risk: RiskLevel) -> u8 {
    match risk {
        RiskLevel::ReadOnly => 0,
        RiskLevel::Low => 1,
        RiskLevel::Medium => 2,
        RiskLevel::High => 3,
        RiskLevel::Unknown => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{
        BufferSnapshot, CandidateKind, CandidateSource, SlotKind, SyncQuality, TextEdit,
    };
    use crate::shell::ShellKind;
    use crate::terminal::{BufferRevision, QueryId};
    use std::path::PathBuf;

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
        let buffer = BufferSnapshot::new("x", 1, BufferRevision::new(1), SyncQuality::Exact)
            .expect("buffer");
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

    fn buffer_context(text: &str) -> CompletionContext {
        let buffer =
            BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer");
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            buffer,
        )
        .expect("context")
    }

    fn history_candidate(
        context: &CompletionContext,
        replacement: &str,
        range_end: usize,
    ) -> Candidate {
        Candidate::new(
            context.query_id,
            replacement,
            "history",
            Some(TextEdit {
                range: 0..range_end,
                replacement: replacement.into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::History,
            CandidateKind::History,
            Completeness::Runnable,
            RiskLevel::Low,
            "history",
        )
    }

    #[test]
    fn drops_candidates_identical_to_the_current_buffer() {
        let context = buffer_context("git status");
        let ranked = rank_and_dedupe(
            &context,
            vec![
                // Spec-style bare command duplicating the typed buffer.
                history_candidate(&context, "git status", 10),
                // Trim-normalized near-miss: trailing space still counts as identical.
                history_candidate(&context, "git status ", 10),
                // Genuinely different completion stays.
                history_candidate(&context, "git status --short", 10),
            ],
            10,
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].display.primary, "git status --short");
    }

    #[test]
    fn drops_editless_candidates_matching_the_current_buffer() {
        let context = buffer_context("make build");
        let editless = Candidate::new(
            context.query_id,
            "make build",
            "project",
            None,
            CandidateAction::None,
            CandidateSource::Project,
            CandidateKind::Recipe,
            Completeness::Runnable,
            RiskLevel::Low,
            "project",
        );
        let different = Candidate::new(
            context.query_id,
            "make build release",
            "project",
            None,
            CandidateAction::None,
            CandidateSource::Project,
            CandidateKind::Recipe,
            Completeness::Runnable,
            RiskLevel::Low,
            "project-x",
        );
        let ranked = rank_and_dedupe(&context, vec![editless, different], 10);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].display.primary, "make build release");
    }

    #[test]
    fn stricter_risk_keeps_the_more_severe_level() {
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
    fn workspace_bonus_rewards_commands_matching_the_markers() {
        let git = WorkspaceMarkers {
            git: true,
            ..WorkspaceMarkers::default()
        };
        let node = WorkspaceMarkers {
            package_json: true,
            ..WorkspaceMarkers::default()
        };
        let rust = WorkspaceMarkers {
            cargo_toml: true,
            ..WorkspaceMarkers::default()
        };
        let make = WorkspaceMarkers {
            makefile: true,
            ..WorkspaceMarkers::default()
        };
        let just = WorkspaceMarkers {
            justfile: true,
            ..WorkspaceMarkers::default()
        };

        assert_eq!(workspace_bonus(git, "git status"), 40);
        assert_eq!(workspace_bonus(git, "cargo build"), 0);
        assert_eq!(workspace_bonus(git, "git"), 0, "bare command gets nothing");
        assert_eq!(workspace_bonus(node, "npm run build"), 40);
        assert_eq!(workspace_bonus(node, "pnpm test"), 40);
        assert_eq!(workspace_bonus(node, "yarn dev"), 40);
        assert_eq!(workspace_bonus(node, "bun start"), 40);
        assert_eq!(workspace_bonus(node, "npm install"), 0);
        assert_eq!(workspace_bonus(rust, "cargo build"), 40);
        assert_eq!(workspace_bonus(make, "make install"), 40);
        assert_eq!(workspace_bonus(just, "just build"), 40);
        assert_eq!(
            workspace_bonus(WorkspaceMarkers::default(), "git status"),
            0
        );
        assert!(
            workspace_bonus(
                WorkspaceMarkers {
                    git: true,
                    package_json: true,
                    cargo_toml: true,
                    makefile: true,
                    justfile: true,
                },
                "git status"
            ) <= 100,
            "bonus stays clamped"
        );
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

    fn recipe(context: &CompletionContext, name: &str) -> Candidate {
        Candidate::new(
            context.query_id,
            name,
            "project",
            None,
            CandidateAction::None,
            CandidateSource::Project,
            CandidateKind::Recipe,
            Completeness::Runnable,
            RiskLevel::Low,
            "project",
        )
    }

    fn recipe_order(query_id: u64) -> Vec<String> {
        let buffer =
            BufferSnapshot::new("", 0, BufferRevision::new(1), SyncQuality::Exact).expect("buffer");
        let context = CompletionContext::new(
            QueryId::new(query_id),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            buffer,
        )
        .expect("context");
        // Curated spec order — deliberately not alphabetical.
        let curated = ["test", "dev", "build"];
        let candidates = curated.iter().map(|name| recipe(&context, name)).collect();
        rank_and_dedupe(&context, candidates, 10)
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect()
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

    #[test]
    fn workspace_bonus_matches_display_primary_for_short_replacements() {
        let markers = WorkspaceMarkers {
            git: true,
            ..WorkspaceMarkers::default()
        };
        let context = buffer_context("st").with_workspace(markers);
        // Man-page style row: the edit inserts only the subcommand, but the
        // display shows the full command line.
        let row = Candidate::new(
            context.query_id,
            "git status",
            "man",
            Some(TextEdit {
                range: 0..2,
                replacement: "status".into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::CommandHelp,
            CandidateKind::Command,
            Completeness::Runnable,
            RiskLevel::ReadOnly,
            "man",
        );
        let ranked = rank_and_dedupe(&context, vec![row], 10);
        assert_eq!(
            ranked[0].score.context, 40,
            "git bonus must apply via display.primary"
        );
    }
}
