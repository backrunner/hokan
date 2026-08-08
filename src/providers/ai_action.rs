use std::sync::Arc;

use crate::{
    ai::{AiCommand, detect_natural_language},
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, TextEdit,
    },
    config::AiConfig,
    platform::CommandPathCache,
    shell::AliasCache,
    specs::SpecRegistry,
    terminal::RiskLevel,
};

pub struct AiActionProvider {
    config: Arc<AiConfig>,
    credential_available: bool,
    commands: Arc<CommandPathCache>,
    specs: Arc<SpecRegistry>,
    aliases: Arc<AliasCache>,
}

impl AiActionProvider {
    #[must_use]
    pub fn new(
        config: Arc<AiConfig>,
        credential_available: bool,
        commands: Arc<CommandPathCache>,
        specs: Arc<SpecRegistry>,
        aliases: Arc<AliasCache>,
    ) -> Self {
        Self {
            config,
            credential_available,
            commands,
            specs,
            aliases,
        }
    }
}

impl CandidateProvider for AiActionProvider {
    fn id(&self) -> &'static str {
        "ai_action"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        if self.is_available_shell_command(context) {
            return false;
        }
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

impl AiActionProvider {
    fn is_available_shell_command(&self, context: &CompletionContext) -> bool {
        let Some(info) =
            crate::safety::effective_command_info_for_shell(&context.buffer.text, context.shell)
        else {
            return false;
        };
        if info.indeterminate {
            return false;
        }
        let word = info.word.as_str();
        let path = word.contains('/') || self.commands.contains(word);
        let builtin = crate::providers::is_shell_builtin(context.shell, word);
        let symbol = crate::providers::is_shell_builtin_or_keyword(context.shell, word);
        match info.kind {
            crate::parser::EffectiveCommandKind::Shell => {
                path || symbol || self.aliases.load(context.shell).contains(word)
            }
            crate::parser::EffectiveCommandKind::External => path,
            crate::parser::EffectiveCommandKind::ExternalOrBuiltin => path || builtin,
            crate::parser::EffectiveCommandKind::Builtin => builtin,
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
            Arc::new(AliasCache::default()),
        );
        assert!(provider.applies(&context));
        let output = provider.complete(&context);
        assert!(matches!(
            output.candidates[0].action,
            CandidateAction::ConfigureAi
        ));
    }

    #[test]
    fn valid_builtins_and_aliases_are_not_offered_as_natural_language() {
        let mut aliases = crate::shell::ShellAliases::default();
        crate::shell::parse_rc_text(
            ShellKind::Zsh,
            "alias showall='printf done'\n",
            &mut aliases,
        );
        let provider = AiActionProvider::new(
            Arc::new(AiConfig::default()),
            false,
            Arc::new(CommandPathCache::default()),
            Arc::new(SpecRegistry::load(None)),
            Arc::new(AliasCache::new_fixed(aliases)),
        );
        let context = |text: &str| {
            CompletionContext::new(
                QueryId::new(1),
                ShellKind::Zsh,
                PathBuf::from("/tmp"),
                BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                    .expect("buffer"),
            )
            .expect("context")
        };

        // These lines deliberately contain enough English intent words to
        // cross the natural-language threshold without command awareness.
        for command in [
            "set show all files now",
            "time echo show all files now",
            "builtin echo show all files now",
            "showall show all files now",
        ] {
            assert!(
                !provider.applies(&context(command)),
                "valid shell command was treated as prose: {command:?}"
            );
        }
        assert!(provider.applies(&context("please show all files now")));
    }
}
