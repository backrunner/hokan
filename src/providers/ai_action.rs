use std::sync::Arc;

use crate::{
    ai::{AiCommand, detect_natural_language},
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, TextEdit,
    },
    config::AiConfig,
    platform::CommandPathCache,
    specs::SpecRegistry,
    terminal::RiskLevel,
};

pub struct AiActionProvider {
    config: Arc<AiConfig>,
    credential_available: bool,
    commands: Arc<CommandPathCache>,
    specs: Arc<SpecRegistry>,
}

impl AiActionProvider {
    #[must_use]
    pub fn new(
        config: Arc<AiConfig>,
        credential_available: bool,
        commands: Arc<CommandPathCache>,
        specs: Arc<SpecRegistry>,
    ) -> Self {
        Self {
            config,
            credential_available,
            commands,
            specs,
        }
    }
}

impl CandidateProvider for AiActionProvider {
    fn id(&self) -> &'static str {
        "ai_action"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        detect_natural_language(
            &context.buffer.text,
            &self.config.trigger_prefix,
            &self.commands,
            &self.specs,
        )
        .should_offer()
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let configured =
            self.config.enabled && !self.config.model.is_empty() && self.credential_available;
        let candidate = Candidate::new(
            context.query_id,
            if configured {
                "使用 AI 生成命令"
            } else {
                "配置 AI 命令生成"
            },
            if configured {
                "显式请求 OpenAI 兼容接口，结果仅回填"
            } else {
                "设置 endpoint、model 与凭据环境变量"
            },
            None,
            if configured {
                CandidateAction::RequestAi
            } else {
                CandidateAction::ConfigureAi
            },
            CandidateSource::Action,
            CandidateKind::AiAction,
            Completeness::ActionOnly,
            RiskLevel::Unknown,
            "ai:action",
        );
        ProviderOutput {
            candidates: vec![candidate],
            diagnostics: Vec::new(),
        }
    }
}

#[must_use]
pub fn ai_result_candidates(
    context: &CompletionContext,
    commands: Vec<AiCommand>,
) -> Vec<Candidate> {
    commands
        .into_iter()
        .enumerate()
        .map(|(index, command)| {
            Candidate::new(
                context.query_id,
                &command.command,
                command.explanation,
                Some(TextEdit {
                    range: 0..context.buffer.text.len(),
                    replacement: command.command.clone(),
                    cursor_after: CursorPlacement::End,
                }),
                CandidateAction::Insert,
                CandidateSource::Ai,
                CandidateKind::AiCommand,
                Completeness::Runnable,
                command.risk.unwrap_or(RiskLevel::Unknown),
                format!("ai:result:{index}"),
            )
        })
        .collect()
}

#[must_use]
pub fn ai_error_candidate(
    context: &CompletionContext,
    description: impl Into<String>,
    configure: bool,
) -> Candidate {
    Candidate::new(
        context.query_id,
        if configure {
            "配置 AI 后重试"
        } else {
            "重试 AI 请求"
        },
        description,
        None,
        if configure {
            CandidateAction::ConfigureAi
        } else {
            CandidateAction::Retry
        },
        CandidateSource::Action,
        CandidateKind::AiAction,
        Completeness::ActionOnly,
        RiskLevel::Unknown,
        if configure {
            "ai:configure"
        } else {
            "ai:retry"
        },
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        completion::{BufferSnapshot, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    #[test]
    fn typing_natural_language_does_not_call_a_client() {
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            BufferSnapshot::new(
                "查找最近修改的文件",
                "查找最近修改的文件".len(),
                BufferRevision::new(1),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context");
        let provider = AiActionProvider::new(
            Arc::new(AiConfig::default()),
            false,
            Arc::new(CommandPathCache::default()),
            Arc::new(SpecRegistry::load(None)),
        );
        assert!(provider.applies(&context));
        let output = provider.complete(&context);
        assert!(matches!(
            output.candidates[0].action,
            CandidateAction::ConfigureAi
        ));
    }
}
