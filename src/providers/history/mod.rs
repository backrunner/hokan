use std::sync::{Arc, RwLock};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CompletionMode, CursorPlacement, ProviderOutput, TextEdit,
    },
    history::HistoryIndex,
    platform::CommandPathCache,
    project::{NodeWorkspaceCache, ProjectCache},
    shell::AliasCache,
    specs::SpecRegistry,
};

use super::command_help::CommandHelpCache;

pub struct HistoryProvider {
    index: Arc<RwLock<HistoryIndex>>,
    commands: Arc<CommandPathCache>,
    aliases: Arc<AliasCache>,
    specs: Arc<SpecRegistry>,
    help: Arc<CommandHelpCache>,
    projects: Arc<ProjectCache>,
    workspaces: NodeWorkspaceCache,
    candidate_limit: usize,
    #[cfg(test)]
    allow_unknown_cwd: bool,
}

impl HistoryProvider {
    #[must_use]
    pub fn new(
        index: Arc<RwLock<HistoryIndex>>,
        commands: Arc<CommandPathCache>,
        aliases: Arc<AliasCache>,
        specs: Arc<SpecRegistry>,
        help: Arc<CommandHelpCache>,
    ) -> Self {
        Self {
            index,
            commands,
            aliases,
            specs,
            help,
            projects: Arc::new(ProjectCache::default()),
            workspaces: NodeWorkspaceCache::default(),
            candidate_limit: usize::MAX,
            #[cfg(test)]
            allow_unknown_cwd: false,
        }
    }

    #[must_use]
    pub fn with_project_cache(mut self, projects: Arc<ProjectCache>) -> Self {
        self.projects = projects;
        self
    }

    #[must_use]
    pub fn with_navigation_limit(mut self, limit: usize) -> Self {
        self.candidate_limit = if limit == 0 { usize::MAX } else { limit };
        self
    }

    #[cfg(test)]
    pub(crate) fn allow_unknown_cwd_for_tests(mut self) -> Self {
        self.allow_unknown_cwd = true;
        self
    }

    fn record_matches_cwd(
        &self,
        context: &CompletionContext,
        record: &crate::history::HistoryRecord,
    ) -> bool {
        record.last_cwd.as_deref() == Some(context.cwd.as_path()) || {
            #[cfg(test)]
            {
                self.allow_unknown_cwd && record.last_cwd.is_none()
            }
            #[cfg(not(test))]
            {
                false
            }
        }
    }
}

impl CandidateProvider for HistoryProvider {
    fn id(&self) -> &'static str {
        "history"
    }

    fn supports_mode(&self, _: CompletionMode) -> bool {
        true
    }

    fn applies(&self, _: &CompletionContext) -> bool {
        true
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        // ProjectProvider owns the package-manager command/script surface.
        // Whole-line history rows would otherwise outrank the current script
        // list and reintroduce scripts from other projects. Keep history after
        // the script token, and for explicit Ctrl-R search.
        if self.should_defer_normal_manager_history(context) {
            return ProviderOutput::default();
        }
        self.prefetch_context_help(context);
        let Ok(index) = self.index.read() else {
            return ProviderOutput::default();
        };
        let now_ms = crate::history_now_ms();
        let midline = context.buffer.cursor < context.buffer.text.len();
        let search_text = if midline {
            &context.buffer.text[..context.buffer.cursor]
        } else {
            &context.buffer.text
        };
        if context.mode == CompletionMode::Normal && search_text.trim().is_empty() {
            return ProviderOutput::default();
        }
        // Normal completion replaces the whole buffer with a history row, so
        // every non-empty input must be a literal line prefix. Broad fuzzy
        // recall belongs only to explicit history search (Ctrl-R).
        let later_segment = context.parsed.active_segment.start > 0;
        let anchor = (context.mode == CompletionMode::Normal).then(|| {
            if midline {
                context.buffer.text[..context.buffer.cursor]
                    .trim_start()
                    .to_lowercase()
            } else {
                continuation_prefix(&context.buffer.text)
            }
        });
        let suffix = (context.mode == CompletionMode::Normal && midline)
            .then(|| context.buffer.text[context.parsed.replacement.end..].to_lowercase());
        // Alias/function discovery fingerprints rc files. Load it once for
        // this query instead of repeating those filesystem checks for every
        // history record considered by the pre-top-k eligibility filter.
        let aliases = self.aliases.load(context.shell);
        // In explicit history search, a known command prefix still narrows the
        // result family; unknown fragments retain Ctrl-R's fuzzy recall.
        let command_prefix = (matches!(
            context.mode,
            CompletionMode::HistoryOnly | CompletionMode::HistoryNavigation
        ) && !later_segment)
            .then(|| self.known_command_prefix_with_aliases(context, &aliases))
            .flatten();
        // Apply every eligibility constraint before an explicit candidate
        // limit, so high-frecency typo or unrelated rows cannot hide valid
        // continuations.
        let navigation = context.mode == CompletionMode::HistoryNavigation;
        let matches = if navigation {
            index.search_recent_filtered(
                search_text,
                &context.cwd,
                now_ms,
                self.candidate_limit,
                |record| {
                    let command = record.command.trim().to_lowercase();
                    // Shell-style Up/Down recall must be scoped to commands that
                    // actually ran in the current directory. Imported shell
                    // history without a cwd is intentionally excluded here.
                    self.record_matches_cwd(context, record)
                        && command.starts_with(search_text.trim_start().to_lowercase().as_str())
                        && command_prefix.as_ref().is_none_or(|prefix| {
                            crate::safety::effective_command_word_for_shell(
                                &record.command,
                                context.shell,
                            )
                            .is_some_and(|command| command.to_lowercase().starts_with(prefix))
                        })
                        && self.plausible_record_with_aliases(context, record, &aliases)
                },
            )
        } else {
            index.search_filtered(
                search_text,
                &context.cwd,
                now_ms,
                self.candidate_limit,
                |record| {
                    // History recommendations must be grounded in executions
                    // recorded for the current directory; unknown/imported cwd
                    // records cannot be safely attributed here.
                    self.record_matches_cwd(context, record)
                        && anchor.as_ref().is_none_or(|anchor| {
                            let command = record.command.trim().to_lowercase();
                            command.starts_with(anchor.as_str())
                                && suffix.as_ref().is_none_or(|suffix| {
                                    command.len() >= anchor.len() + suffix.len()
                                        && command.ends_with(suffix.as_str())
                                })
                        })
                        && command_prefix.as_ref().is_none_or(|prefix| {
                            crate::safety::effective_command_word_for_shell(
                                &record.command,
                                context.shell,
                            )
                            .is_some_and(|command| command.to_lowercase().starts_with(prefix))
                        })
                        && self.plausible_record_with_aliases(context, record, &aliases)
                },
            )
        };
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
                if navigation {
                    candidate.score.history_timestamp = matched.record.last_used_ms;
                }
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

fn continuation_prefix(text: &str) -> String {
    let text = text.trim_start();
    let trimmed = text.trim_end();
    let mut prefix = trimmed.to_lowercase();
    if trimmed.len() < text.len() {
        prefix.push(' ');
    }
    prefix
}

impl HistoryProvider {
    fn should_defer_normal_manager_history(&self, context: &CompletionContext) -> bool {
        if context.mode != CompletionMode::Normal {
            return false;
        }
        if super::project::node_run_completion_position(context) {
            return true;
        }
        if super::filter_position(context).is_some()
            || super::manager_option_position(context).is_some()
            || super::manager_has_multiple_selectors_at_completion(context)
        {
            return true;
        }
        let Some(position) = super::manager_position(context) else {
            return false;
        };
        match position.position {
            super::Position::ScriptToken | super::Position::KeywordWord => true,
            // npm/deno expose only native commands at the first argument;
            // keep their standalone history rows for the generic provider.
            super::Position::ManagerWord | super::Position::CommandToken
                if position.spec.keyword.is_none() =>
            {
                match self.projects.load_nearest(&position.project_dir) {
                    Ok(Some(_)) | Err(_) => true,
                    Ok(None) => false,
                }
            }
            super::Position::ManagerWord | super::Position::CommandToken => false,
        }
    }

    fn prefetch_context_help(&self, context: &CompletionContext) {
        let Some(command) = context.command() else {
            return;
        };
        if self.specs.get(command).is_some() || !super::effective_command_accepts_external(context)
        {
            return;
        }
        let Some(executable) = super::resolved_executable_path(context, &self.commands) else {
            return;
        };
        self.help.request(command, Some(executable));
    }

    #[cfg(test)]
    fn known_command_prefix(&self, context: &CompletionContext) -> Option<String> {
        let aliases = self.aliases.load(context.shell);
        self.known_command_prefix_with_aliases(context, &aliases)
    }

    fn known_command_prefix_with_aliases(
        &self,
        context: &CompletionContext,
        aliases: &crate::shell::ShellAliases,
    ) -> Option<String> {
        if !crate::providers::command_position_open(context) {
            return None;
        }
        let prefix = context.parsed.current_prefix.as_str();
        if prefix.is_empty() {
            return None;
        }
        let folded_prefix = prefix.to_lowercase();
        let path = self
            .commands
            .names()
            .iter()
            .any(|command| command.to_lowercase().starts_with(&folded_prefix));
        let builtin = super::shell_builtin_has_prefix(context.shell, &folded_prefix);
        let symbol = super::shell_symbol_has_prefix(context.shell, &folded_prefix);
        let known = match crate::providers::command_resolution_kind(context) {
            crate::parser::EffectiveCommandKind::Shell => {
                path || symbol
                    || aliases
                        .names()
                        .any(|name| name.to_lowercase().starts_with(&folded_prefix))
            }
            crate::parser::EffectiveCommandKind::External => path,
            crate::parser::EffectiveCommandKind::ExternalOrBuiltin => path || builtin,
            crate::parser::EffectiveCommandKind::Builtin => builtin,
        };
        known.then_some(folded_prefix)
    }
}

mod flow;
mod scripts;
mod validation;

#[cfg(test)]
mod tests;
