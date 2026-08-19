use std::collections::{HashMap, HashSet};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, Completeness, CompletionContext, CursorPlacement,
    },
    parser::apply_edit,
    project::WorkspaceMarkers,
    terminal::RiskLevel,
};

const HISTORY_OVERLAP_BONUS: i16 = 100;

#[derive(Debug, Eq, Hash, PartialEq)]
enum DedupeKey {
    ResultingBuffer(String),
    Edit(usize, usize, String),
    Display(String),
}

pub fn rank_and_dedupe(
    context: &CompletionContext,
    candidates: Vec<Candidate>,
    limit: usize,
) -> Vec<Candidate> {
    let query = context.parsed.current_prefix.as_str();
    let exact_path_command = !query.is_empty()
        && candidates.iter().any(|candidate| {
            candidate.query_id == context.query_id
                && has_valid_edit(context, candidate)
                && candidate.source == crate::completion::CandidateSource::PathCommand
                && candidate.kind == CandidateKind::Command
                && produces_current_buffer(context, candidate)
        });
    let exact_explicit_executable = !query.is_empty()
        && (crate::providers::explicit_executable_path_position(context)
            || crate::providers::explicit_executable_argument_path_position(context))
        && candidates.iter().any(|candidate| {
            candidate.query_id == context.query_id
                && has_valid_edit(context, candidate)
                && candidate.source == crate::completion::CandidateSource::Filesystem
                && candidate.kind == CandidateKind::File
                && produces_current_buffer(context, candidate)
        });
    let direct_command_match = !query.is_empty()
        && candidates.iter().any(|candidate| {
            candidate.query_id == context.query_id
                && has_valid_edit(context, candidate)
                && candidate.source == crate::completion::CandidateSource::PathCommand
                && candidate.kind == CandidateKind::Command
                && candidate_match_signal(query, candidate).priority >= 3
        });
    // Token-oriented domains used to leave fuzzy siblings behind after their
    // exact row was removed as a no-op (`kill 123` -> unrelated processes
    // whose command happened to contain 123, `ssh dev` -> substring hosts,
    // or an exact executable path -> scattered files). Once an exact token is
    // already present, keep only genuinely longer prefixes from that domain.
    let exact_token_sources: HashSet<_> = if query.is_empty() {
        HashSet::new()
    } else {
        candidates
            .iter()
            .filter(|candidate| {
                candidate.query_id == context.query_id
                    && has_valid_edit(context, candidate)
                    && exact_token_domain(candidate)
                    && candidate
                        .edit
                        .as_ref()
                        .is_some_and(|edit| edit.range == context.parsed.replacement)
                    && produces_current_buffer(context, candidate)
            })
            .map(|candidate| candidate.source)
            .collect()
    };
    let mut deduped: HashMap<DedupeKey, Candidate> = HashMap::new();
    for mut candidate in candidates {
        if candidate.query_id != context.query_id || !has_valid_edit(context, &candidate) {
            continue;
        }
        if direct_command_match
            && candidate.source == crate::completion::CandidateSource::PathCommand
            && candidate.kind == CandidateKind::Command
            && candidate_match_signal(query, &candidate).priority < 3
        {
            continue;
        }
        if exact_path_command
            && candidate.source == crate::completion::CandidateSource::PathCommand
            && candidate.kind == CandidateKind::Command
            && !produces_current_buffer(context, &candidate)
        {
            continue;
        }
        if exact_explicit_executable
            && candidate.source == crate::completion::CandidateSource::Filesystem
            && !produces_current_buffer(context, &candidate)
        {
            continue;
        }
        if exact_token_sources.contains(&candidate.source)
            && exact_token_domain(&candidate)
            && !produces_current_buffer(context, &candidate)
            && candidate_match_signal(query, &candidate).priority < 3
        {
            continue;
        }
        sanitize_display(&mut candidate);
        if produces_current_buffer(context, &candidate) {
            continue;
        }
        let replacement_target = candidate
            .edit
            .as_ref()
            .map_or(candidate.display.primary.as_str(), |edit| {
                edit.replacement.as_str()
            });
        let replacement_match = match_signal(query, replacement_target);
        let display_match = match_signal(query, &candidate.display.primary);
        let continuation_match = history_continuation_match(context, &candidate);
        let best_match = continuation_match
            .unwrap_or(MatchSignal {
                priority: 0,
                quality: 0,
            })
            .max(replacement_match)
            .max(display_match);
        // Command-token providers are completion domains, not search results.
        // In normal mode, substring/subsequence matches are too weak to
        // justify changing what the user typed. Curated specs retain their
        // provider-level compatibility checks and the generic match gate
        // below.
        if context.mode == crate::completion::CompletionMode::Normal
            && !query.is_empty()
            && candidate.kind == CandidateKind::Command
            && matches!(
                candidate.source,
                crate::completion::CandidateSource::PathCommand
                    | crate::completion::CandidateSource::CommandHelp
                    | crate::completion::CandidateSource::Project
            )
            && best_match.priority < 3
        {
            continue;
        }
        candidate.score.match_priority = best_match.priority;
        candidate.score.continuation_priority = u8::from(continuation_match.is_some());
        candidate.score.command_priority = command_priority(context, &candidate);
        candidate.score.match_quality = best_match.quality;
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
        candidate.score.incomplete_penalty = incomplete_penalty(&candidate);
        // Providers edit at different scopes: history replaces the whole line,
        // while help, project, and filesystem providers usually replace only
        // the active token. Deduplicate the command the edit actually produces
        // so equivalent rows merge even when their TextEdits differ.
        let key = dedupe_key(context, &candidate);
        match deduped.get_mut(&key) {
            Some(existing) => {
                merge_duplicate(existing, candidate);
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
            .match_priority
            .cmp(&left.score.match_priority)
            .then_with(|| {
                right
                    .score
                    .continuation_priority
                    .cmp(&left.score.continuation_priority)
            })
            .then_with(|| {
                right
                    .score
                    .command_priority
                    .cmp(&left.score.command_priority)
            })
            .then_with(|| right.score.total().cmp(&left.score.total()))
            .then_with(|| left.source.order().cmp(&right.source.order()))
            .then_with(|| left.display.primary.cmp(&right.display.primary))
            .then_with(|| left.provenance.cmp(&right.provenance))
    });
    candidates.truncate(limit);
    candidates
}

fn dedupe_key(context: &CompletionContext, candidate: &Candidate) -> DedupeKey {
    candidate.edit.as_ref().map_or_else(
        || DedupeKey::Display(candidate.display.primary.clone()),
        |edit| match apply_edit(&context.buffer.text, edit.range.clone(), &edit.replacement) {
            Ok(resulting) => DedupeKey::ResultingBuffer(resulting),
            Err(_) => DedupeKey::Edit(edit.range.start, edit.range.end, edit.replacement.clone()),
        },
    )
}

fn merge_duplicate(existing: &mut Candidate, candidate: Candidate) {
    let distinct_history_source = (existing.source == crate::completion::CandidateSource::History
        && candidate.source != crate::completion::CandidateSource::History)
        || (candidate.source == crate::completion::CandidateSource::History
            && existing.source != crate::completion::CandidateSource::History);
    let stricter = stricter_risk(existing.risk, candidate.risk);
    let merged_score = merge_score_signals(existing.score, candidate.score);

    if ranking_key(&candidate) > ranking_key(existing) {
        *existing = candidate;
    }

    existing.risk = stricter;
    existing.score = merged_score;
    if distinct_history_source || existing.score.history_overlap > 0 {
        existing.score.history_overlap = HISTORY_OVERLAP_BONUS;
    }
    // The merge may have raised the risk level; keep the displayed score in
    // sync with the risk the merged row now carries.
    existing.score.risk_penalty = risk_penalty(existing.risk);
    // This penalty describes the retained candidate's interaction semantics,
    // unlike the other merged signals which describe shared ranking evidence.
    existing.score.incomplete_penalty = incomplete_penalty(existing);
}

fn merge_score_signals(
    left: crate::completion::ScoreSignals,
    right: crate::completion::ScoreSignals,
) -> crate::completion::ScoreSignals {
    crate::completion::ScoreSignals {
        match_priority: left.match_priority.max(right.match_priority),
        continuation_priority: left.continuation_priority.max(right.continuation_priority),
        command_priority: left.command_priority.max(right.command_priority),
        match_quality: left.match_quality.max(right.match_quality),
        source_trust: left.source_trust.max(right.source_trust),
        spec_priority: left.spec_priority.max(right.spec_priority),
        cwd_affinity: left.cwd_affinity.max(right.cwd_affinity),
        frecency: left.frecency.max(right.frecency),
        transition: left.transition.max(right.transition),
        history_overlap: left.history_overlap.max(right.history_overlap),
        context: left.context.max(right.context),
        risk_penalty: left.risk_penalty.max(right.risk_penalty),
        incomplete_penalty: left.incomplete_penalty.max(right.incomplete_penalty),
        failed_penalty: left.failed_penalty.max(right.failed_penalty),
    }
}

fn candidate_match_signal(query: &str, candidate: &Candidate) -> MatchSignal {
    let replacement = candidate
        .edit
        .as_ref()
        .map_or(candidate.display.primary.as_str(), |edit| {
            edit.replacement.as_str()
        });
    match_signal(query, replacement).max(match_signal(query, &candidate.display.primary))
}

fn exact_token_domain(candidate: &Candidate) -> bool {
    matches!(
        candidate.kind,
        CandidateKind::File
            | CandidateKind::Directory
            | CandidateKind::Process
            | CandidateKind::Interface
    ) || (candidate.source == crate::completion::CandidateSource::Project
        && candidate.kind == CandidateKind::Command)
}

fn sanitize_display(candidate: &mut Candidate) {
    candidate.display.primary = escape_control_characters(&candidate.display.primary);
    candidate.display.description = escape_control_characters(&candidate.display.description);
    if let Some(annotation) = candidate.display.annotation.as_mut() {
        *annotation = escape_control_characters(annotation);
    }
}

fn escape_control_characters(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            escaped.extend(character.escape_default());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn ranking_key(candidate: &Candidate) -> (u8, u8, u8, i32) {
    (
        candidate.score.match_priority,
        candidate.score.continuation_priority,
        candidate.score.command_priority,
        candidate.score.total(),
    )
}

/// A history row replaces the whole line, while ordinary argument candidates
/// replace only the active token. Match a validated history continuation
/// against the typed line prefix as well as the active token; otherwise
/// `proj s` sees `proj skillscat` as a weak substring and lets a directory
/// named `skillscat` incorrectly outrank it.
fn history_continuation_match(
    context: &CompletionContext,
    candidate: &Candidate,
) -> Option<MatchSignal> {
    if candidate.source != crate::completion::CandidateSource::History
        || crate::providers::executable_position_open(context)
    {
        return None;
    }
    let edit = candidate.edit.as_ref()?;
    if edit.range.start != 0 || edit.range.end != context.buffer.text.len() {
        return None;
    }

    let before_cursor = &context.buffer.text[..context.buffer.cursor];
    let mut prefix = before_cursor.to_lowercase();
    if context.buffer.cursor == context.buffer.text.len() {
        let trimmed = before_cursor.trim_end();
        prefix = trimmed.to_lowercase();
        if trimmed.len() < before_cursor.len() {
            prefix.push(' ');
        }
    }
    if prefix.is_empty() {
        return None;
    }

    let replacement = edit.replacement.trim().to_lowercase();
    if !replacement.starts_with(&prefix) {
        return None;
    }
    if context.buffer.cursor < context.buffer.text.len() {
        let suffix = context.buffer.text[context.parsed.replacement.end..].to_lowercase();
        if !replacement.ends_with(&suffix) {
            return None;
        }
    }
    Some(match_signal(&prefix, &replacement))
}

fn command_priority(context: &CompletionContext, candidate: &Candidate) -> u8 {
    u8::from(
        crate::providers::executable_position_open(context)
            && candidate.source == crate::completion::CandidateSource::PathCommand
            && candidate.kind == CandidateKind::Command,
    )
}

/// A candidate whose FULL resulting buffer equals what is already typed adds
/// nothing: accepting it would rewrite the edit line to itself. The comparison
/// is trim-normalized so trailing-whitespace near-misses count as identical
/// too. A completed line stays open only when a provider has a real next step.
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MatchSignal {
    priority: u8,
    quality: i16,
}

fn match_signal(query: &str, candidate: &str) -> MatchSignal {
    if query.is_empty() {
        return MatchSignal {
            priority: 0,
            quality: 500,
        };
    }
    let query = query.to_lowercase();
    let candidate = candidate.to_lowercase();
    if candidate == query {
        MatchSignal {
            priority: 4,
            quality: 1000,
        }
    } else if candidate.starts_with(&query) {
        MatchSignal {
            priority: 3,
            quality: 900_i16.saturating_sub((candidate.len() - query.len()).min(200) as i16),
        }
    } else if let Some(index) = candidate.find(&query) {
        MatchSignal {
            priority: 2,
            quality: 700_i16.saturating_sub(index.min(200) as i16),
        }
    } else if is_subsequence(&query, &candidate) {
        MatchSignal {
            priority: 1,
            quality: 450,
        }
    } else {
        MatchSignal {
            priority: 0,
            quality: 0,
        }
    }
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

fn incomplete_penalty(candidate: &Candidate) -> i16 {
    match candidate.completeness {
        Completeness::Runnable => 0,
        // Directories are always NeedsInput because descending into them IS
        // the interaction — the penalty would sink them below files.
        Completeness::NeedsInput { .. } if candidate.kind == CandidateKind::Directory => 0,
        Completeness::NeedsInput { .. } => 60,
        Completeness::ActionOnly => 80,
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
                history_candidate(&context, "git status", 10),
                history_candidate(&context, "git status ", 10),
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
    fn strong_prefix_beats_highly_personalized_fuzzy_history() {
        let context = buffer_context("cod");
        let mut fuzzy = history_candidate(&context, "cargo doc", 3);
        fuzzy.score.cwd_affinity = 100;
        fuzzy.score.frecency = 200;
        fuzzy.score.transition = 200;
        let direct = Candidate::new(
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

        let ranked = rank_and_dedupe(&context, vec![fuzzy, direct], 10);
        assert_eq!(ranked[0].display.primary, "codex");
        assert!(ranked[0].score.match_priority > ranked[1].score.match_priority);
    }

    #[test]
    fn direct_command_prefix_removes_fuzzy_rows_from_other_command_sources() {
        let context = buffer_context("cod");
        let command = |name: &str, provenance: &str| {
            Candidate::new(
                context.query_id,
                name,
                "command",
                Some(TextEdit {
                    range: 0..3,
                    replacement: name.into(),
                    cursor_after: CursorPlacement::End,
                }),
                CandidateAction::Insert,
                CandidateSource::PathCommand,
                CandidateKind::Command,
                Completeness::Runnable,
                RiskLevel::Unknown,
                provenance,
            )
        };

        let ranked = rank_and_dedupe(
            &context,
            vec![
                command("code", "path:code"),
                command("codex", "path:codex"),
                command("cargo-doc", "alias:cargo-doc"),
            ],
            10,
        );
        let names: Vec<_> = ranked
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert_eq!(names, ["code", "codex"]);
    }

    #[test]
    fn exact_path_command_closes_longer_executable_siblings() {
        let context = buffer_context("git");
        let command = |name: &str| {
            Candidate::new(
                context.query_id,
                name,
                "command",
                Some(TextEdit {
                    range: 0..3,
                    replacement: name.into(),
                    cursor_after: CursorPlacement::End,
                }),
                CandidateAction::Insert,
                CandidateSource::PathCommand,
                CandidateKind::Command,
                Completeness::Runnable,
                RiskLevel::Low,
                format!("path:{name}"),
            )
        };
        let help = Candidate::new(
            context.query_id,
            "git status",
            "subcommand",
            Some(TextEdit {
                range: 0..3,
                replacement: "git status".into(),
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

        let ranked = rank_and_dedupe(
            &context,
            vec![command("git"), command("git-helper"), help],
            10,
        );
        assert_eq!(
            ranked
                .iter()
                .map(|candidate| candidate.display.primary.as_str())
                .collect::<Vec<_>>(),
            ["git status"]
        );
    }

    #[test]
    fn command_rows_require_a_prefix_even_without_a_direct_sibling() {
        let context = buffer_context("cgd");
        let fuzzy = Candidate::new(
            context.query_id,
            "cargo-doc",
            "command",
            Some(TextEdit {
                range: 0..3,
                replacement: "cargo-doc".into(),
                cursor_after: CursorPlacement::End,
            }),
            CandidateAction::Insert,
            CandidateSource::PathCommand,
            CandidateKind::Command,
            Completeness::Runnable,
            RiskLevel::Unknown,
            "path:cargo-doc",
        );
        assert!(rank_and_dedupe(&context, vec![fuzzy], 10).is_empty());
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
    fn exact_explicit_executable_closes_longer_and_fuzzy_path_rows() {
        let context = buffer_context("./run");
        let path = |name: &str| {
            Candidate::new(
                context.query_id,
                name,
                "filesystem",
                Some(TextEdit {
                    range: context.parsed.replacement.clone(),
                    replacement: name.into(),
                    cursor_after: CursorPlacement::End,
                }),
                CandidateAction::Insert,
                CandidateSource::Filesystem,
                CandidateKind::File,
                Completeness::Runnable,
                RiskLevel::Low,
                format!("filesystem:{name}"),
            )
        };

        let ranked = rank_and_dedupe(
            &context,
            vec![path("./run"), path("./runner"), path("./around")],
            10,
        );
        let rows: Vec<_> = ranked
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert!(rows.is_empty());
    }

    #[test]
    fn exact_project_token_suppresses_substring_hosts() {
        let context = buffer_context("ssh dev");
        let host = |name: &str| {
            Candidate::new(
                context.query_id,
                name,
                "SSH host",
                Some(TextEdit {
                    range: context.parsed.replacement.clone(),
                    replacement: name.into(),
                    cursor_after: CursorPlacement::End,
                }),
                CandidateAction::Insert,
                CandidateSource::Project,
                CandidateKind::Command,
                Completeness::Runnable,
                RiskLevel::Low,
                format!("ssh:{name}"),
            )
        };

        let ranked = rank_and_dedupe(
            &context,
            vec![host("dev"), host("developer"), host("prod-dev")],
            10,
        );
        let rows: Vec<_> = ranked
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert_eq!(rows, ["developer"]);
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
