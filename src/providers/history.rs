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
                candidate.score.failed_penalty = matched.failed_penalty;
                if let Some(previous) = context.previous_command.as_deref() {
                    candidate.score.transition =
                        index.transition_score(previous, &matched.record.command);
                }
                candidate
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        completion::{BufferSnapshot, SyncQuality, rank_and_dedupe},
        history::HistoryPolicy,
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    fn context(text: &str, previous_command: Option<&str>) -> CompletionContext {
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
        .with_previous_command(previous_command.map(str::to_owned))
    }

    fn history_index() -> HistoryIndex {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        // `git add` -> `git commit` is a well-worn path.
        for round in 0..3 {
            let base = 1_000 + round * 10;
            index.ingest("git add x", base, ShellKind::Zsh, None, Some(0), &policy);
            index.ingest(
                "git commit -m y",
                base + 1,
                ShellKind::Zsh,
                None,
                Some(0),
                &policy,
            );
        }
        // `git config` is far more frequent, so it wins plain frecency
        // ordering whenever the transition boost does not apply.
        index.ingest_weighted(
            "git config user.name x",
            2_000,
            ShellKind::Zsh,
            None,
            30,
            Some(0),
            &policy,
        );
        index
    }

    #[test]
    fn transition_bigram_boosts_the_known_successor_end_to_end() {
        let provider = HistoryProvider::new(std::sync::Arc::new(RwLock::new(history_index())));

        let boosted = context("git c", Some("git add x"));
        let ranked = rank_and_dedupe(&boosted, provider.complete(&boosted).candidates, 10);
        assert_eq!(ranked[0].display.primary, "git commit -m y");
        assert_eq!(ranked[0].score.transition, 200);
        assert_eq!(ranked[1].display.primary, "git config user.name x");
        assert_eq!(ranked[1].score.transition, 0);

        // Without a matching previous command there is no boost and plain
        // match/frecency ordering decides.
        let plain = context("git c", Some("ls -la"));
        let ranked = rank_and_dedupe(&plain, provider.complete(&plain).candidates, 10);
        assert_eq!(ranked[0].display.primary, "git config user.name x");
        assert_eq!(ranked[0].score.transition, 0);
    }

    #[test]
    fn recently_failed_commands_carry_the_failure_penalty() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        index.ingest("make deploy", 1_000, ShellKind::Zsh, None, Some(0), &policy);
        index.ingest("make deploy", 2_000, ShellKind::Zsh, None, Some(2), &policy);
        index.ingest("make build", 3_000, ShellKind::Zsh, None, Some(0), &policy);
        let provider = HistoryProvider::new(std::sync::Arc::new(RwLock::new(index)));
        let output = provider.complete(&context("make ", None));
        let deploy = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "make deploy")
            .expect("deploy candidate");
        assert_eq!(deploy.score.failed_penalty, 150);
        let build = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "make build")
            .expect("build candidate");
        assert_eq!(build.score.failed_penalty, 0);
    }
}
