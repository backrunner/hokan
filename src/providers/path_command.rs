use std::sync::Arc;

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, TextEdit,
    },
    platform::CommandPathCache,
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
        crate::providers::executable_position_open(context)
            && (crate::providers::command_position_open(context)
                || self.executable_slot_owner_available(context))
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        if !self.applies(context) {
            return ProviderOutput::default();
        }
        let mut names = self.commands.names();
        let command_slot = crate::providers::command_position_open(context);
        let symbol_query = crate::providers::shell_symbol_argument_position(context);
        let resolution = crate::providers::command_resolution_kind(context);
        let path_allowed =
            !command_slot || resolution != crate::parser::EffectiveCommandKind::Builtin;
        let shell_name_allowed = symbol_query
            || (command_slot && resolution != crate::parser::EffectiveCommandKind::External);
        if shell_name_allowed {
            names.extend(
                crate::providers::shell_builtins_and_keywords(context.shell)
                    .iter()
                    .copied()
                    .filter(|name| {
                        if symbol_query {
                            true
                        } else if resolution == crate::parser::EffectiveCommandKind::Shell {
                            crate::providers::is_shell_callable(context.shell, name)
                        } else {
                            crate::providers::is_shell_builtin(context.shell, name)
                        }
                    })
                    .map(str::to_owned),
            );
        }
        let corepack_dispatch = context.command() == Some("corepack")
            && self.commands.contains("corepack")
            && resolution != crate::parser::EffectiveCommandKind::Builtin;
        if corepack_dispatch {
            names.extend(
                crate::providers::MANAGERS
                    .iter()
                    .map(|manager| manager.name.to_owned()),
            );
        }
        names.sort_unstable();
        names.dedup();
        let query = context.parsed.current_prefix.as_str();
        let folded_query = query.to_lowercase();
        let allowed = |name: &str| {
            let on_path = path_allowed && self.commands.contains(name);
            let shell_symbol = shell_name_allowed
                && if symbol_query {
                    crate::providers::is_shell_builtin_or_keyword(context.shell, name)
                } else if resolution == crate::parser::EffectiveCommandKind::Shell {
                    crate::providers::is_shell_callable(context.shell, name)
                } else {
                    crate::providers::is_shell_builtin(context.shell, name)
                };
            let virtual_manager = corepack_dispatch
                && crate::providers::is_package_manager(name)
                && !query.is_empty();
            crate::providers::path_executable_name_allowed(context, name)
                && (on_path || shell_symbol || virtual_manager)
        };
        names.retain(|name| {
            allowed(name) && (query.is_empty() || name.to_lowercase().starts_with(&folded_query))
        });
        names.sort_by(|left, right| {
            crate::completion::match_quality(query, right)
                .cmp(&crate::completion::match_quality(query, left))
                .then_with(|| left.cmp(right))
        });
        let candidates = names
            .into_iter()
            .map(|name| {
                let on_path = path_allowed && self.commands.contains(&name);
                let builtin =
                    shell_name_allowed && crate::providers::is_shell_builtin(context.shell, &name);
                let shell_command = shell_name_allowed
                    && crate::providers::is_shell_callable(context.shell, &name)
                    && !builtin;
                let keyword = symbol_query
                    && crate::providers::is_shell_builtin_or_keyword(context.shell, &name)
                    && !crate::providers::is_shell_callable(context.shell, &name);
                let replacement = crate::parser::escape_for_shell(
                    &name,
                    crate::parser::QuoteContext::Unquoted,
                    context.shell,
                );
                let resulting = crate::parser::apply_edit(
                    &context.buffer.text,
                    context.parsed.replacement.clone(),
                    &replacement,
                )
                .unwrap_or_else(|_| replacement.clone());
                let display = if crate::providers::command_position_open(context) {
                    name.clone()
                } else {
                    resulting.clone()
                };
                Candidate::new(
                    context.query_id,
                    display,
                    if builtin {
                        "Shell 内建命令"
                    } else if shell_command {
                        "Shell 标准命令或函数"
                    } else if keyword {
                        "Shell 保留字"
                    } else if on_path {
                        "PATH 中的可执行命令"
                    } else {
                        "Hokan 支持的 Node 包管理器"
                    },
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement,
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::Insert,
                    CandidateSource::PathCommand,
                    CandidateKind::Command,
                    Completeness::Runnable,
                    crate::safety::classify_command(&resulting).level,
                    if builtin {
                        format!("builtin:{name}")
                    } else if shell_command {
                        format!("shell:{name}")
                    } else if keyword {
                        format!("keyword:{name}")
                    } else if on_path {
                        format!("path:{name}")
                    } else {
                        format!("manager:{name}")
                    },
                )
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

impl PathCommandProvider {
    fn executable_slot_owner_available(&self, context: &CompletionContext) -> bool {
        let Some(command) = context.command() else {
            return false;
        };
        crate::providers::is_shell_callable(context.shell, command)
            || crate::providers::resolved_executable_path(context, &self.commands).is_some()
    }
}

#[cfg(test)]
mod tests;
