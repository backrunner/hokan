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
    // Once the typed word is runnable, rank its semantic continuations ahead
    // of longer executable names without removing those names from the list.
    let exact_path_command = crate::providers::command_position_open(context)
        && !query.is_empty()
        && candidates.iter().any(|candidate| {
            candidate.query_id == context.query_id
                && has_valid_edit(context, candidate)
                && candidate.source == crate::completion::CandidateSource::PathCommand
                && candidate.kind == CandidateKind::Command
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
        candidate.score.command_priority = if exact_path_command {
            u8::from(candidate.source != crate::completion::CandidateSource::PathCommand)
        } else {
            command_priority(context, &candidate)
        };
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
        let navigation_order =
            (context.mode == crate::completion::CompletionMode::HistoryNavigation).then(|| {
                right
                    .score
                    .history_timestamp
                    .cmp(&left.score.history_timestamp)
            });
        navigation_order
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.score.match_priority.cmp(&left.score.match_priority))
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
        history_timestamp: left.history_timestamp.max(right.history_timestamp),
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
/// ignores ordinary edge spacing, but preserves whitespace that may belong
/// to a quoted/escaped argument or nested shell syntax.
fn produces_current_buffer(context: &CompletionContext, candidate: &Candidate) -> bool {
    let resulting = match candidate.edit.as_ref() {
        Some(edit) => match apply_edit(&context.buffer.text, edit.range.clone(), &edit.replacement)
        {
            Ok(text) => text,
            Err(_) => return false,
        },
        None => candidate.display.primary.clone(),
    };
    trim_insignificant_spacing(&resulting) == trim_insignificant_spacing(&context.buffer.text)
}

fn trim_insignificant_spacing(text: &str) -> &str {
    let text = text.trim_start_matches([' ', '\t']);
    if text.contains(['\'', '"', '\\', '\n', '\r', '`', '$', '#']) {
        text
    } else {
        text.trim_end_matches([' ', '\t'])
    }
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
    FoldedMatcher::new(query).quality(candidate)
}

/// Compile substring-search state once for a scan, instead of rebuilding
/// the same searcher for each of the potentially 100,000 history records.
pub(crate) struct FoldedMatcher<'a> {
    query: &'a str,
    finder: memchr::memmem::Finder<'a>,
}

impl<'a> FoldedMatcher<'a> {
    pub(crate) fn new(query: &'a str) -> Self {
        Self {
            query,
            finder: memchr::memmem::Finder::new(query),
        }
    }

    pub(crate) fn quality(&self, candidate: &str) -> i16 {
        let query = self.query;
        if query.is_empty() {
            500
        } else if candidate == query {
            1000
        } else if candidate.starts_with(query) {
            900_i16.saturating_sub((candidate.len() - query.len()).min(200) as i16)
        } else if let Some(index) = self.finder.find(candidate.as_bytes()) {
            700_i16.saturating_sub(index.min(200) as i16)
        } else if is_subsequence(query, candidate) {
            450
        } else {
            0
        }
    }
}

fn is_subsequence(query: &str, candidate: &str) -> bool {
    // ASCII bytes cannot match any UTF-8 continuation byte, so this also
    // preserves character-subsequence semantics for Unicode candidates.
    if query.is_ascii() {
        let mut query = query.bytes();
        let mut expected = query.next();
        for byte in candidate.bytes() {
            if Some(byte) == expected {
                expected = query.next();
                if expected.is_none() {
                    return true;
                }
            }
        }
        return false;
    }
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
        RiskLevel::Unknown => 3,
        RiskLevel::High => 4,
    }
}

#[cfg(test)]
mod tests;
