use std::collections::HashMap;

use crate::{
    completion::{Candidate, CandidateAction, Completeness, CompletionContext, CursorPlacement},
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
        candidate.score.risk_penalty = risk_penalty(candidate.risk);
        candidate.score.incomplete_penalty = match candidate.completeness {
            Completeness::Runnable => 0,
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
            }
            None => {
                deduped.insert(key, candidate);
            }
        }
    }
    let mut candidates: Vec<_> = deduped.into_values().collect();
    candidates.sort_by(|left, right| {
        right
            .score
            .total()
            .cmp(&left.score.total())
            .then_with(|| left.source.order().cmp(&right.source.order()))
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates.truncate(limit);
    candidates
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

const fn risk_penalty(risk: RiskLevel) -> i16 {
    match risk {
        RiskLevel::ReadOnly => 0,
        RiskLevel::Low => 20,
        RiskLevel::Medium => 100,
        RiskLevel::High => 250,
        RiskLevel::Unknown => 300,
    }
}

const fn stricter_risk(left: RiskLevel, right: RiskLevel) -> RiskLevel {
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
        BufferSnapshot, CandidateKind, CandidateSource, SyncQuality, TextEdit,
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
}
