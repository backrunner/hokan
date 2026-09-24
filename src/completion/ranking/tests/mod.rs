use super::*;

proptest::proptest! {
    #[test]
    fn prepared_matcher_preserves_unicode_and_ascii_scores(
        query in "[a-zé中🙂0-9 ]{0,16}",
        text in "[a-zé中🙂0-9 ]{0,60}",
    ) {
        let matcher = FoldedMatcher::new(&query);
        let scattered = query.chars().map(|character| format!("{character}🙂 ")).collect::<String>();
        for candidate in [text.clone(), format!("{query}{text}"), format!("{text}{query}"), scattered] {
            let mut expected = query.chars().peekable();
            for character in candidate.chars() {
                if expected.peek() == Some(&character) {
                    expected.next();
                }
            }
            let reference = if query.is_empty() { 500 }
            else if candidate == query { 1000 }
            else if candidate.starts_with(&query) { 900 - (candidate.len() - query.len()).min(200) as i16 }
            else if let Some(offset) = candidate.find(&query) { 700 - offset.min(200) as i16 }
            else if expected.peek().is_none() { 450 }
            else { 0 };
            proptest::prop_assert_eq!(matcher.quality(&candidate), reference, "query={:?}, candidate={:?}", &query, &candidate);
        }
    }
}
use crate::completion::{
    BufferSnapshot, CandidateKind, CandidateSource, SlotKind, SyncQuality, TextEdit,
};
use crate::shell::ShellKind;
use crate::terminal::{BufferRevision, QueryId};
use std::path::PathBuf;

fn buffer_context(text: &str) -> CompletionContext {
    let buffer = BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
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

mod dedupe;
mod matching;
mod scoring;
