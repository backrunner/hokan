use std::sync::{Arc, RwLock};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, TextEdit,
    },
    history::HistoryIndex,
};

pub struct HistoryProvider {
    index: Arc<RwLock<HistoryIndex>>,
}

impl HistoryProvider {
    #[must_use]
    pub fn new(index: Arc<RwLock<HistoryIndex>>) -> Self {
        Self { index }
    }
}

impl CandidateProvider for HistoryProvider {
    fn id(&self) -> &'static str {
        "history"
    }

    fn applies(&self, _: &CompletionContext) -> bool {
        true
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Ok(index) = self.index.read() else {
            return ProviderOutput::default();
        };
        let now_ms = crate::history_now_ms();
        let matches = index.search(&context.buffer.text, &context.cwd, now_ms, 50);
        let candidates = matches
            .into_iter()
            .map(|matched| {
                let shell = matched.record.shell.to_string();
                let mut candidate = Candidate::new(
                    context.query_id,
                    &matched.record.command,
                    format!("{} · 使用 {} 次", shell, matched.record.count),
                    Some(TextEdit {
                        range: 0..context.buffer.text.len(),
                        replacement: matched.record.command.clone(),
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::Insert,
                    CandidateSource::History,
                    CandidateKind::History,
                    Completeness::Runnable,
                    crate::safety::classify_command(&matched.record.command).level,
                    format!(
                        "history:{}",
                        crc32fast::hash(matched.record.command.as_bytes())
                    ),
                );
                candidate.score.frecency = matched.frecency;
                candidate.score.cwd_affinity = matched.cwd_affinity;
                candidate
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}
