use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, TextEdit,
    },
    parser::{QuoteContext, escape_for_shell},
    terminal::RiskLevel,
};

const LEAVE_COMMAND: &str = "hokan-leave";

/// Commands supplied by the active Hokan session rather than by the user's
/// shell or PATH. Keeping these rows separate prevents them from changing
/// shell parsing and builtin classification outside the overlay.
pub struct SessionCommandProvider;

impl CandidateProvider for SessionCommandProvider {
    fn id(&self) -> &'static str {
        "session_command"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        crate::providers::shell_command_position_open(context)
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        if !self.applies(context) {
            return ProviderOutput::default();
        }
        let query = context.parsed.current_prefix.as_str();
        let folded_query = query.to_lowercase();
        if !LEAVE_COMMAND.to_lowercase().starts_with(&folded_query) {
            return ProviderOutput::default();
        }
        let replacement = escape_for_shell(LEAVE_COMMAND, QuoteContext::Unquoted, context.shell);
        ProviderOutput {
            candidates: vec![Candidate::new(
                context.query_id,
                LEAVE_COMMAND,
                "退出 Hokan，返回未包装的 shell",
                Some(TextEdit {
                    range: context.parsed.replacement.clone(),
                    replacement,
                    cursor_after: CursorPlacement::End,
                }),
                CandidateAction::Insert,
                CandidateSource::Action,
                CandidateKind::Command,
                Completeness::Runnable,
                RiskLevel::Low,
                "session:hokan-leave",
            )],
            diagnostics: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    fn context(shell: ShellKind, text: &str) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(1),
            shell,
            PathBuf::from("/tmp"),
            BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context")
    }

    #[test]
    fn offers_leave_only_at_the_shell_command_position() {
        let provider = SessionCommandProvider;
        for shell in [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish] {
            assert!(provider.applies(&context(shell, "hokan-l")));
            assert_eq!(
                provider
                    .complete(&context(shell, "hokan-l"))
                    .candidates
                    .len(),
                1
            );
            assert!(!provider.applies(&context(shell, "printf hokan-l")));
            assert!(
                provider
                    .complete(&context(shell, "other"))
                    .candidates
                    .is_empty()
            );
        }
    }

    #[test]
    fn leave_candidate_is_low_risk_and_replaces_only_the_command_prefix() {
        let provider = SessionCommandProvider;
        let candidate = provider
            .complete(&context(ShellKind::Zsh, "hokan-l"))
            .candidates
            .pop()
            .expect("leave candidate");
        assert_eq!(candidate.display.primary, LEAVE_COMMAND);
        assert_eq!(candidate.risk, RiskLevel::Low);
        assert_eq!(candidate.edit.expect("edit").replacement, LEAVE_COMMAND);
        assert_eq!(candidate.provenance, "session:hokan-leave");
    }

    #[test]
    fn leave_candidate_survives_engine_ranking() {
        let mut engine = CompletionEngine::new(10, 10);
        engine.register(SessionCommandProvider);
        let candidates = engine
            .complete(&context(ShellKind::Zsh, "hokan-l"))
            .candidates;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].display.primary, LEAVE_COMMAND);
    }
}
