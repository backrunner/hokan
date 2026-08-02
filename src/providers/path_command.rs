use std::sync::Arc;

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, TextEdit,
    },
    platform::CommandPathCache,
    terminal::RiskLevel,
};

pub struct PathCommandProvider {
    commands: Arc<CommandPathCache>,
}

impl PathCommandProvider {
    #[must_use]
    pub fn new(commands: Arc<CommandPathCache>) -> Self {
        Self { commands }
    }
}

impl CandidateProvider for PathCommandProvider {
    fn id(&self) -> &'static str {
        "path_command"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        let word_count = context
            .parsed
            .tokens
            .iter()
            .filter(|token| {
                token.kind == crate::parser::TokenKind::Word
                    && token.range.start >= context.parsed.active_segment.start
                    && token.range.start <= context.buffer.cursor
            })
            .count();
        word_count <= 1 && !context.parsed.current_prefix.contains('/')
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let mut names: Vec<_> = self.commands.names().collect();
        names.sort_unstable();
        let candidates = names
            .into_iter()
            .filter(|name| {
                crate::completion::match_quality(&context.parsed.current_prefix, name) > 0
            })
            .take(1_000)
            .map(|name| {
                Candidate::new(
                    context.query_id,
                    name,
                    "PATH 中的可执行命令",
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement: name.to_owned(),
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::Insert,
                    CandidateSource::PathCommand,
                    CandidateKind::Command,
                    Completeness::Runnable,
                    RiskLevel::Unknown,
                    format!("path:{name}"),
                )
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}
