use std::{
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CompletionMode, CursorPlacement, ProviderOutput, TextEdit,
    },
    history::HistoryIndex,
    platform::CommandPathCache,
    project::{NodeWorkspaceCache, ProjectCache, WorkspaceMember},
    shell::AliasCache,
    specs::SpecRegistry,
};

use super::command_help::{
    CommandHelp, CommandHelpCache, HelpEntry, one_edit_or_adjacent_transposition,
    scoped_history_arguments_are_plausible,
};

pub struct HistoryProvider {
    index: Arc<RwLock<HistoryIndex>>,
    commands: Arc<CommandPathCache>,
    aliases: Arc<AliasCache>,
    specs: Arc<SpecRegistry>,
    help: Arc<CommandHelpCache>,
    projects: Arc<ProjectCache>,
    workspaces: NodeWorkspaceCache,
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
        }
    }

    #[must_use]
    pub fn with_project_cache(mut self, projects: Arc<ProjectCache>) -> Self {
        self.projects = projects;
        self
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
        let command_prefix = (context.mode == CompletionMode::HistoryOnly && !later_segment)
            .then(|| self.known_command_prefix_with_aliases(context, &aliases))
            .flatten();
        // Apply every eligibility constraint before HistoryIndex takes its
        // bounded top-k. Otherwise 50 high-frecency typo or unrelated rows can
        // hide the first valid continuation entirely.
        let matches = index.search_filtered(search_text, &context.cwd, now_ms, 50, |record| {
            anchor.as_ref().is_none_or(|anchor| {
                let command = record.command.trim().to_lowercase();
                command.starts_with(anchor.as_str())
                    && suffix.as_ref().is_none_or(|suffix| {
                        command.len() >= anchor.len() + suffix.len()
                            && command.ends_with(suffix.as_str())
                    })
            }) && command_prefix.as_ref().is_none_or(|prefix| {
                crate::safety::effective_command_word_for_shell(&record.command, context.shell)
                    .is_some_and(|command| command.to_lowercase().starts_with(prefix))
            }) && self.plausible_record_with_aliases(context, record, &aliases)
        });
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

    /// History rows whose command cannot ever have run — the word is not an
    /// executable on PATH, not a shell builtin, alias, or keyword, and not an
    /// explicit path — are typos and noise; drop
    /// them outright. Anything we cannot classify (unparseable line, opaque
    /// substitution) is kept: filtering must never hide a command we merely
    /// fail to understand.
    #[cfg(test)]
    fn plausible_command(&self, context: &CompletionContext, command: &str) -> bool {
        let aliases = self.aliases.load(context.shell);
        self.plausible_command_with_aliases(context, command, &aliases)
    }

    #[cfg(test)]
    fn plausible_command_with_aliases(
        &self,
        context: &CompletionContext,
        command: &str,
        aliases: &crate::shell::ShellAliases,
    ) -> bool {
        self.plausible_command_with_status(context, command, None, aliases)
    }

    fn plausible_record_with_aliases(
        &self,
        context: &CompletionContext,
        record: &crate::history::HistoryRecord,
        aliases: &crate::shell::ShellAliases,
    ) -> bool {
        self.plausible_command_with_status(context, &record.command, record.last_exit_code, aliases)
    }

    fn plausible_command_with_status(
        &self,
        context: &CompletionContext,
        command: &str,
        last_exit_code: Option<i32>,
        aliases: &crate::shell::ShellAliases,
    ) -> bool {
        let Some(segments) = command_segment_words(command) else {
            return true;
        };
        let mut states = vec![HistoryFlowState {
            cwd: context.cwd.as_ref().clone(),
            status: HistoryCommandStatus::Success,
            active: true,
            background_cwd: None,
        }];
        let mut previous_link = None;
        let mut previous_backgrounded = false;
        for segment in segments {
            if segment.backgrounded && !previous_backgrounded {
                for state in &mut states {
                    state.background_cwd = Some(state.cwd.clone());
                }
            }
            let mut active_seen = false;
            let mut active_plausible = false;
            let mut inactive_plausible = false;
            let plausibility = states
                .iter()
                .map(|state| {
                    let mut segment_context = context.clone();
                    segment_context.cwd = Arc::new(state.cwd.clone());
                    let plausible = self.plausible_segment(
                        &segment_context,
                        &segment.words,
                        last_exit_code,
                        aliases,
                    );
                    if state.active {
                        active_seen = true;
                        active_plausible |= plausible;
                    } else {
                        inactive_plausible |= plausible;
                    }
                    plausible
                })
                .collect::<Vec<_>>();
            if (active_seen && !active_plausible) || (!active_seen && !inactive_plausible) {
                return false;
            }
            if active_seen {
                states = states
                    .into_iter()
                    .zip(plausibility)
                    .filter_map(|(state, plausible)| (!state.active || plausible).then_some(state))
                    .collect();
            }

            let in_pipeline = previous_link == Some(HistorySegmentLink::Pipe)
                || segment.next == HistorySegmentLink::Pipe;
            let mut outcomes = Vec::new();
            for state in states {
                if !state.active {
                    push_history_flow_state(&mut outcomes, state);
                    continue;
                }
                if in_pipeline {
                    for status in [HistoryCommandStatus::Success, HistoryCommandStatus::Failure] {
                        push_history_flow_state(
                            &mut outcomes,
                            HistoryFlowState {
                                cwd: state.cwd.clone(),
                                status,
                                active: true,
                                background_cwd: state.background_cwd.clone(),
                            },
                        );
                    }
                    continue;
                }

                let mut segment_context = context.clone();
                segment_context.cwd = Arc::new(state.cwd.clone());
                match history_directory_change(&segment_context, &segment.words, aliases) {
                    HistoryDirectoryChange::NotApplicable => {
                        for status in [HistoryCommandStatus::Success, HistoryCommandStatus::Failure]
                        {
                            push_history_flow_state(
                                &mut outcomes,
                                HistoryFlowState {
                                    cwd: state.cwd.clone(),
                                    status,
                                    active: true,
                                    background_cwd: state.background_cwd.clone(),
                                },
                            );
                        }
                    }
                    HistoryDirectoryChange::Failed => {
                        push_history_flow_state(
                            &mut outcomes,
                            HistoryFlowState {
                                cwd: state.cwd,
                                status: HistoryCommandStatus::Failure,
                                active: true,
                                background_cwd: state.background_cwd,
                            },
                        );
                    }
                    HistoryDirectoryChange::Known(directory) => {
                        push_history_flow_state(
                            &mut outcomes,
                            HistoryFlowState {
                                cwd: directory,
                                status: HistoryCommandStatus::Success,
                                active: true,
                                background_cwd: state.background_cwd.clone(),
                            },
                        );
                        push_history_flow_state(
                            &mut outcomes,
                            HistoryFlowState {
                                cwd: state.cwd,
                                status: HistoryCommandStatus::Failure,
                                active: true,
                                background_cwd: state.background_cwd,
                            },
                        );
                    }
                    // A dynamic `cd` may have placed the remaining command in
                    // another project. Keep the history row instead of
                    // rejecting a script against a cwd we cannot know.
                    HistoryDirectoryChange::Unknown => return true,
                }
            }

            for state in &mut outcomes {
                if segment.next == HistorySegmentLink::Background {
                    if let Some(parent_cwd) = state.background_cwd.take() {
                        state.cwd = parent_cwd;
                    }
                    state.status = HistoryCommandStatus::Success;
                }
                state.active = match segment.next {
                    HistorySegmentLink::Always | HistorySegmentLink::Background => true,
                    HistorySegmentLink::OnSuccess => state.status == HistoryCommandStatus::Success,
                    HistorySegmentLink::OnFailure => state.status == HistoryCommandStatus::Failure,
                    HistorySegmentLink::Pipe => state.active,
                    HistorySegmentLink::End => false,
                };
            }
            if outcomes.len() > MAX_HISTORY_FLOW_STATES {
                return true;
            }
            previous_link = Some(segment.next);
            previous_backgrounded =
                segment.backgrounded && segment.next != HistorySegmentLink::Background;
            states = outcomes;
        }
        true
    }

    fn plausible_segment(
        &self,
        context: &CompletionContext,
        words: &[String],
        last_exit_code: Option<i32>,
        aliases: &crate::shell::ShellAliases,
    ) -> bool {
        if words.is_empty() {
            return true;
        }
        let cooked: Vec<&str> = words.iter().map(String::as_str).collect();
        let analysis =
            crate::parser::effective_command_analysis_for_shell(&cooked, false, context.shell);
        let command_index = match analysis.state {
            crate::parser::EffectiveCommandState::Found(index)
            | crate::parser::EffectiveCommandState::WrapperCommand(index) => index,
            crate::parser::EffectiveCommandState::IndeterminateWrapper(_)
            | crate::parser::EffectiveCommandState::AwaitingCommand
            | crate::parser::EffectiveCommandState::AwaitingWrapperValue => return true,
        };
        let Some(word) = cooked.get(command_index).copied() else {
            return true;
        };
        let corepack_dispatch = command_index
            .checked_sub(1)
            .and_then(|index| cooked.get(index))
            .is_some_and(|wrapper| *wrapper == "corepack");
        let path = if corepack_dispatch {
            self.commands.contains("corepack")
        } else if word.contains('/') {
            if word
                .chars()
                .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
            {
                true
            } else {
                let directory =
                    super::wrapper_working_directory_before(context, &cooked, command_index);
                let executable = super::resolve_directory(&directory, word);
                crate::platform::is_executable(&executable)
            }
        } else {
            self.commands.contains(word)
        };
        let builtin = super::is_shell_builtin(context.shell, word);
        let symbol = super::is_shell_builtin_or_keyword(context.shell, word);
        let plausible = match analysis.kind {
            crate::parser::EffectiveCommandKind::Shell => path || symbol || aliases.contains(word),
            crate::parser::EffectiveCommandKind::External => path,
            crate::parser::EffectiveCommandKind::ExternalOrBuiltin => path || builtin,
            crate::parser::EffectiveCommandKind::Builtin => builtin,
        };
        plausible
            && self.plausible_function_argument(context, words, command_index, aliases)
            && self.plausible_executable_arguments(context, words, command_index, last_exit_code)
    }

    fn plausible_executable_arguments(
        &self,
        context: &CompletionContext,
        words: &[String],
        command_index: usize,
        last_exit_code: Option<i32>,
    ) -> bool {
        let cooked: Vec<&str> = words.iter().map(String::as_str).collect();
        let Some(command_word) = cooked.get(command_index).copied() else {
            return true;
        };
        let arguments = cooked.get(command_index + 1..).unwrap_or_default();
        let known_non_failure =
            last_exit_code.is_some_and(|code| !crate::history::is_failed_exit(Some(code)));
        let executable = if context.command() == Some(command_word) {
            crate::providers::resolved_executable_path(context, &self.commands)
        } else if command_word.contains('/') {
            None
        } else {
            self.commands.path(command_word)
        };

        if let Some(plausible) = super::python_module::history_python_module_is_plausible(
            &self.help,
            command_word,
            executable.clone(),
            arguments,
            known_non_failure,
        ) {
            return plausible;
        }

        if let Some(plausible) =
            maven_history_arguments_are_plausible(command_word, arguments, known_non_failure)
        {
            return plausible;
        }

        if let Some(plausible) = self.node_run_script_is_plausible(context, &cooked, command_index)
        {
            return plausible;
        }

        if let Some(plausible) = self.manager_script_is_plausible(context, &cooked, command_index) {
            return plausible;
        }

        if let Some(manager) = super::MANAGERS
            .iter()
            .find(|manager| manager.name == super::executable_basename(command_word))
        {
            let Some(command_arguments) = manager_command_arguments(manager.name, arguments) else {
                return true;
            };
            let argument = command_arguments[0];
            if argument
                .chars()
                .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
                || known_non_failure
            {
                return true;
            }
            if manager
                .subcommands
                .iter()
                .any(|(subcommand, _)| *subcommand == argument)
            {
                if command_arguments.len() == 1
                    || !manager_subcommand_has_commands(manager.name, argument)
                {
                    return true;
                }
                let root = Arc::new(CommandHelp {
                    flags: Vec::new(),
                    subcommands: vec![HelpEntry {
                        name: argument.to_owned(),
                        description: String::new(),
                        takes_value: false,
                    }],
                    subcommand_aliases: Vec::new(),
                    accepts_positionals: false,
                    subcommands_exhaustive: false,
                });
                return scoped_history_arguments_are_plausible(
                    &self.help,
                    command_word,
                    executable,
                    root,
                    command_arguments,
                    false,
                    false,
                )
                .unwrap_or(false);
            }
            return !manager
                .subcommands
                .iter()
                .any(|(subcommand, _)| one_edit_or_adjacent_transposition(argument, subcommand));
        }
        if let Some(help) = self.help.peek(command_word) {
            return scoped_history_arguments_are_plausible(
                &self.help,
                command_word,
                executable,
                help,
                arguments,
                known_non_failure,
                allows_external_subcommands(command_word),
            )
            .unwrap_or(false);
        }
        // The runtime requests help as soon as an exact executable name is
        // typed. While that bounded background probe is pending, defer its
        // argument-bearing history rows instead of flashing unvalidated typo
        // commands for one frame and removing them on the help-cache refresh.
        if !arguments.is_empty() && self.help.is_pending(command_word) {
            return false;
        }
        true
    }

    fn node_run_script_is_plausible(
        &self,
        context: &CompletionContext,
        words: &[&str],
        command_index: usize,
    ) -> Option<bool> {
        let command = super::executable_basename(words.get(command_index).copied()?);
        if !matches!(command, "node" | "nodejs") {
            return None;
        }
        let arguments = words.get(command_index + 1..).unwrap_or_default();
        let mut index = 0;
        while let Some(argument) = arguments.get(index).copied() {
            let script = if let Some(script) = argument.strip_prefix("--run=") {
                Some(script)
            } else if argument == "--run" {
                Some(arguments.get(index + 1).copied()?)
            } else {
                None
            };
            if let Some(script) = script {
                if script.is_empty()
                    || script
                        .chars()
                        .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
                {
                    return None;
                }
                let project_dir =
                    super::wrapper_working_directory_before(context, words, command_index);
                return self.manifest_has_script(command, &project_dir, script);
            }
            if argument == "--" || argument == "-" || !argument.starts_with('-') {
                return None;
            }
            if super::project::node_option_takes_separate_value(argument) {
                if index + 1 >= arguments.len() {
                    return None;
                }
                index += 2;
            } else {
                index += 1;
            }
        }
        None
    }

    fn manager_script_is_plausible(
        &self,
        context: &CompletionContext,
        words: &[&str],
        command_index: usize,
    ) -> Option<bool> {
        let mut invocation = parse_manager_history_invocation(context, words, command_index)?;
        let script = if super::is_script_keyword(invocation.manager, &invocation.command) {
            invocation.operands.first()?.clone()
        } else if invocation.manager.name == "yarn" && invocation.command == "workspace" {
            let member = invocation.operands.first()?.clone();
            let mut operands = invocation.operands.iter().skip(1);
            let script = match operands.next() {
                Some(word) if word == "run" => operands.next()?.clone(),
                Some(word) => word.clone(),
                None => return None,
            };
            let selector = (super::WorkspaceStyle::YarnWorkspace, member);
            invocation.selector = Some(selector.clone());
            invocation.selectors.push(selector);
            invocation.selector_count = invocation.selectors.len();
            script
        } else if invocation.manager.keyword.is_none() && invocation.command == "run" {
            invocation.operands.first()?.clone()
        } else if invocation.manager.keyword.is_none()
            && !invocation
                .manager
                .subcommands
                .iter()
                .any(|(subcommand, _)| *subcommand == invocation.command)
        {
            invocation.command.clone()
        } else {
            return None;
        };
        if script.starts_with('-')
            || script
                .chars()
                .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
        {
            return None;
        }
        self.script_is_present(&invocation, &script)
    }

    fn script_is_present(
        &self,
        invocation: &ManagerHistoryInvocation,
        script: &str,
    ) -> Option<bool> {
        if invocation.selector_count > 1 {
            return self.multiple_selector_script_is_present(invocation, script);
        }
        let has_workspace_scope =
            invocation.selector.is_some() || invocation.recursive || invocation.workspace_root;
        if !has_workspace_scope {
            // `--if-present` only turns a missing script into a successful
            // no-op. A recommendation is still useful only when this project
            // actually defines the script.
            return self.manifest_has_script(
                invocation.manager.name,
                &invocation.project_dir,
                script,
            );
        }

        let Some(workspace) = self.workspaces.load(&invocation.project_dir) else {
            // Recursive completion falls back to the nearest manifest when
            // this is not actually a workspace, matching ProjectProvider.
            return if invocation.selector.is_none() {
                self.manifest_has_script(invocation.manager.name, &invocation.project_dir, script)
            } else {
                Some(false)
            };
        };

        if invocation.workspace_root {
            return self.workspace_root_has_script(
                invocation.manager.name,
                &workspace.root,
                script,
            );
        }

        if let Some((_, selector)) = invocation.selector.as_ref() {
            if !literal_workspace_selector(selector) {
                return None;
            }
            let selected: Vec<_> = workspace
                .members
                .iter()
                .filter(|member| workspace_member_matches(&workspace.root, member, selector))
                .collect();
            if selected.is_empty() {
                return Some(false);
            }
            return Some(
                selected
                    .iter()
                    .any(|member| member.scripts.contains_key(script)),
            );
        }

        if !invocation.recursive {
            return self.manifest_has_script(
                invocation.manager.name,
                &invocation.project_dir,
                script,
            );
        }

        let mut found = Vec::new();
        if invocation.include_workspace_root {
            found.push(self.workspace_root_has_script(
                invocation.manager.name,
                &workspace.root,
                script,
            )?);
        }
        found.extend(
            workspace
                .members
                .iter()
                .map(|member| member.scripts.contains_key(script)),
        );
        if found.is_empty() {
            return Some(false);
        }
        if invocation.manager.name == "npm" && !invocation.if_present {
            Some(found.into_iter().all(std::convert::identity))
        } else {
            Some(found.into_iter().any(std::convert::identity))
        }
    }

    fn multiple_selector_script_is_present(
        &self,
        invocation: &ManagerHistoryInvocation,
        script: &str,
    ) -> Option<bool> {
        if !invocation
            .selectors
            .iter()
            .all(|(_, selector)| literal_workspace_selector(selector))
        {
            return None;
        }
        let Some(workspace) = self.workspaces.load(&invocation.project_dir) else {
            return Some(false);
        };
        let selected: Vec<_> = workspace
            .members
            .iter()
            .filter(|member| {
                invocation.selectors.iter().any(|(_, selector)| {
                    workspace_member_matches(&workspace.root, member, selector)
                })
            })
            .collect();
        if selected.is_empty() {
            return Some(false);
        }
        if invocation.manager.name == "npm" && !invocation.if_present {
            Some(
                selected
                    .iter()
                    .all(|member| member.scripts.contains_key(script)),
            )
        } else {
            Some(
                selected
                    .iter()
                    .any(|member| member.scripts.contains_key(script)),
            )
        }
    }

    fn manifest_has_script(&self, manager: &str, directory: &Path, script: &str) -> Option<bool> {
        if manager == "deno" {
            return match self.projects.load_deno_nearest(directory) {
                Ok(Some(manifest)) => Some(manifest.tasks.contains_key(script)),
                Ok(None) => Some(false),
                Err(_) => Some(false),
            };
        }
        match self.projects.load_nearest(directory) {
            Ok(Some(manifest)) => Some(manifest.scripts.contains_key(script)),
            Ok(None) => Some(false),
            Err(_) => Some(false),
        }
    }

    fn workspace_root_has_script(&self, manager: &str, root: &Path, script: &str) -> Option<bool> {
        if manager == "deno" {
            return match self.projects.load_deno_nearest(root) {
                Ok(Some(manifest)) => Some(
                    manifest.path.parent() == Some(root) && manifest.tasks.contains_key(script),
                ),
                Ok(None) => Some(false),
                Err(_) => Some(false),
            };
        }
        match self.projects.load_nearest(root) {
            Ok(Some(manifest)) if manifest.path.parent() == Some(root) => {
                Some(manifest.scripts.contains_key(script))
            }
            Ok(Some(_)) | Ok(None) => Some(false),
            Err(_) => Some(false),
        }
    }

    fn plausible_function_argument(
        &self,
        context: &CompletionContext,
        words: &[String],
        command_index: usize,
        aliases: &crate::shell::ShellAliases,
    ) -> bool {
        let Some(entry) = words
            .get(command_index)
            .and_then(|name| aliases.get(name))
            .filter(|entry| entry.kind == crate::shell::AliasKind::Function)
        else {
            return true;
        };
        let Some(slot) = entry
            .body
            .as_deref()
            .and_then(|body| crate::shell::infer_function_slot(context.shell, body))
        else {
            return true;
        };
        let Some(argument) = words.get(command_index + 1) else {
            return true;
        };
        if argument
            .chars()
            .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
        {
            return true;
        }

        if slot.kind == crate::completion::SlotKind::Executable
            && slot.base.is_none()
            && !argument.contains('/')
        {
            return self.commands.contains(argument)
                || crate::providers::is_shell_callable(context.shell, argument)
                || aliases
                    .get(argument)
                    .is_some_and(|entry| entry.kind == crate::shell::AliasKind::Function);
        }

        let base = slot.base.as_ref().map_or_else(
            || context.cwd.as_ref().clone(),
            |base| {
                if base.is_absolute() {
                    base.clone()
                } else {
                    context.cwd.join(base)
                }
            },
        );
        let target = if slot.base.is_some() {
            base.join(argument.trim_start_matches('/'))
        } else {
            resolve_history_path(&base, argument)
        };
        match slot.kind {
            crate::completion::SlotKind::Directory => target.is_dir(),
            crate::completion::SlotKind::File => target.is_file(),
            crate::completion::SlotKind::Path => std::fs::symlink_metadata(target).is_ok(),
            crate::completion::SlotKind::Executable => {
                std::fs::metadata(target).is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            }
            crate::completion::SlotKind::NewFile
            | crate::completion::SlotKind::Process
            | crate::completion::SlotKind::Interface
            | crate::completion::SlotKind::Port
            | crate::completion::SlotKind::Value => true,
        }
    }
}

fn maven_history_arguments_are_plausible(
    command: &str,
    arguments: &[&str],
    known_non_failure: bool,
) -> Option<bool> {
    if !matches!(
        super::executable_basename(command),
        "mvn" | "mvnw" | "mvnDebug"
    ) {
        return None;
    }
    if known_non_failure {
        return Some(true);
    }
    let mut index = 0;
    while let Some(word) = arguments.get(index).copied() {
        if word == "--" {
            index += 1;
            continue;
        }
        if word.starts_with("-D")
            || word.starts_with("-P")
            || word.starts_with("-pl") && word.len() > 3
            || word.starts_with("-T") && word.len() > 2
        {
            index += 1;
            continue;
        }
        if matches!(
            word,
            "-f" | "--file"
                | "-s"
                | "--settings"
                | "-gs"
                | "--global-settings"
                | "-t"
                | "--toolchains"
                | "-pl"
                | "--projects"
                | "-rf"
                | "--resume-from"
                | "-T"
                | "--threads"
        ) {
            if index + 1 >= arguments.len() {
                return Some(false);
            }
            index += 2;
            continue;
        }
        if word.starts_with('-') {
            index += 1;
            continue;
        }
        if word.contains([':', '$', '`', '*', '?', '[', '{']) {
            index += 1;
            continue;
        }
        if super::toolchain::MAVEN_PHASES
            .iter()
            .any(|(phase, _)| *phase == word)
        {
            index += 1;
            continue;
        }
        if super::toolchain::MAVEN_PHASES
            .iter()
            .any(|(phase, _)| one_edit_or_adjacent_transposition(word, phase))
        {
            return Some(false);
        }
        index += 1;
    }
    Some(true)
}

fn manager_command_arguments<'a>(manager: &str, arguments: &'a [&'a str]) -> Option<&'a [&'a str]> {
    let mut index = 0;
    while let Some(argument) = arguments.get(index).copied() {
        if argument == "--" {
            return arguments
                .get(index + 1..)
                .filter(|arguments| !arguments.is_empty());
        }
        if super::attached_manager_value(manager, argument).is_some()
            || super::attached_manager_boolean(manager, argument).is_some()
            || super::manager_flag_without_value(manager, argument)
        {
            index += 1;
            continue;
        }
        if super::manager_value_option(manager, argument).is_some() {
            index += 2;
            continue;
        }
        if argument.starts_with('-') {
            return None;
        }
        return Some(&arguments[index..]);
    }
    None
}

#[derive(Clone, Debug)]
struct ManagerHistoryOptions {
    index: usize,
    project_dir: std::path::PathBuf,
    selector: Option<(super::WorkspaceStyle, String)>,
    selectors: Vec<(super::WorkspaceStyle, String)>,
    selector_count: usize,
    workspace_root: bool,
    recursive: bool,
    include_workspace_root: bool,
    if_present: bool,
}

impl ManagerHistoryOptions {
    fn new(project_dir: &Path) -> Self {
        Self {
            index: 0,
            project_dir: project_dir.to_owned(),
            selector: None,
            selectors: Vec::new(),
            selector_count: 0,
            workspace_root: false,
            recursive: false,
            include_workspace_root: false,
            if_present: false,
        }
    }
}

#[derive(Clone)]
struct ManagerHistoryInvocation {
    manager: &'static super::ManagerSpec,
    command: String,
    operands: Vec<String>,
    project_dir: std::path::PathBuf,
    selector: Option<(super::WorkspaceStyle, String)>,
    selectors: Vec<(super::WorkspaceStyle, String)>,
    selector_count: usize,
    workspace_root: bool,
    recursive: bool,
    include_workspace_root: bool,
    if_present: bool,
}

fn parse_manager_history_options(
    manager: &str,
    arguments: &[&str],
    directory_base: &Path,
    mut state: ManagerHistoryOptions,
    allow_double_dash: bool,
) -> Option<ManagerHistoryOptions> {
    state.index = 0;
    while let Some(argument) = arguments.get(state.index).copied() {
        if argument == "--" {
            if !allow_double_dash {
                return None;
            }
            state.index += 1;
            break;
        }
        if let Some((kind, value)) = super::attached_manager_value(manager, argument) {
            match kind {
                super::ManagerValue::Directory => {
                    state.project_dir = super::resolve_directory(directory_base, value);
                }
                super::ManagerValue::Workspace(style) => {
                    state.selector_count = state.selector_count.saturating_add(1);
                    let selector = (style, value.to_owned());
                    state.selector = Some(selector.clone());
                    state.selectors.push(selector);
                }
                super::ManagerValue::Other => {}
            }
            state.index += 1;
            continue;
        }
        if let Some((flag, enabled)) = super::attached_manager_boolean(manager, argument) {
            super::apply_manager_boolean(
                manager,
                flag,
                enabled,
                &mut state.workspace_root,
                &mut state.recursive,
                &mut state.include_workspace_root,
                &mut state.if_present,
            );
            state.index += 1;
            continue;
        }
        if let Some(kind) = super::manager_value_option(manager, argument) {
            let value = arguments.get(state.index + 1).copied()?;
            match kind {
                super::ManagerValue::Directory => {
                    state.project_dir = super::resolve_directory(directory_base, value);
                }
                super::ManagerValue::Workspace(style) => {
                    state.selector_count = state.selector_count.saturating_add(1);
                    let selector = (style, value.to_owned());
                    state.selector = Some(selector.clone());
                    state.selectors.push(selector);
                }
                super::ManagerValue::Other => {}
            }
            state.index += 2;
            continue;
        }
        if argument.starts_with('-') {
            if !super::manager_flag_without_value(manager, argument) {
                return None;
            }
            super::apply_manager_flag(
                manager,
                argument,
                &mut state.workspace_root,
                &mut state.recursive,
                &mut state.include_workspace_root,
                &mut state.if_present,
            );
            state.index += 1;
            continue;
        }
        break;
    }
    Some(state)
}

fn parse_manager_history_invocation(
    context: &CompletionContext,
    words: &[&str],
    command_index: usize,
) -> Option<ManagerHistoryInvocation> {
    let manager_name = super::executable_basename(words.get(command_index).copied()?);
    let manager = super::MANAGERS
        .iter()
        .find(|manager| manager.name == manager_name)?;
    let arguments = words.get(command_index + 1..).unwrap_or_default();
    let invocation_dir = super::wrapper_working_directory_before(context, words, command_index);
    let mut options = parse_manager_history_options(
        manager.name,
        arguments,
        &invocation_dir,
        ManagerHistoryOptions::new(&invocation_dir),
        true,
    )?;
    let command_offset = options.index;
    let command = arguments.get(command_offset)?.to_string();
    let mut operand_offset = command_offset + 1;
    if matches!(manager.name, "npm" | "pnpm") && super::is_script_keyword(manager, &command) {
        options = parse_manager_history_options(
            manager.name,
            arguments.get(operand_offset..).unwrap_or_default(),
            &invocation_dir,
            options,
            false,
        )?;
        operand_offset += options.index;
    }
    Some(ManagerHistoryInvocation {
        manager,
        command,
        operands: arguments
            .get(operand_offset..)
            .unwrap_or_default()
            .iter()
            .map(|word| (*word).to_owned())
            .collect(),
        project_dir: options.project_dir,
        selector: options.selector,
        selectors: options.selectors,
        selector_count: options.selector_count,
        workspace_root: options.workspace_root,
        recursive: options.recursive,
        include_workspace_root: options.include_workspace_root,
        if_present: options.if_present,
    })
}

fn literal_workspace_selector(selector: &str) -> bool {
    !selector.is_empty()
        && !selector.starts_with('!')
        && !selector.starts_with("...")
        && !selector
            .chars()
            .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
}

fn workspace_member_matches(root: &Path, member: &WorkspaceMember, selector: &str) -> bool {
    let selector_path = Path::new(selector.strip_prefix("./").unwrap_or(selector));
    member.name == selector
        || member.directory == selector_path
        || member
            .directory
            .strip_prefix(root)
            .ok()
            .is_some_and(|path| path == selector_path)
        || member
            .directory
            .file_name()
            .is_some_and(|name| name == std::ffi::OsStr::new(selector))
}

fn manager_subcommand_has_commands(manager: &str, command: &str) -> bool {
    matches!(
        (manager, command),
        ("pnpm", "cache" | "config" | "env" | "runtime" | "store")
            | ("npm", "cache" | "config" | "pkg" | "token")
            | ("yarn", "config" | "set" | "workspaces")
            | ("bun", "pm")
    )
}

fn allows_external_subcommands(command: &str) -> bool {
    matches!(
        command,
        "brew" | "cargo" | "docker" | "gh" | "git" | "kubectl" | "podman"
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistorySegmentLink {
    Always,
    OnSuccess,
    OnFailure,
    Pipe,
    Background,
    End,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HistorySegment {
    words: Vec<String>,
    next: HistorySegmentLink,
    backgrounded: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryCommandStatus {
    Success,
    Failure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HistoryFlowState {
    cwd: PathBuf,
    status: HistoryCommandStatus,
    active: bool,
    background_cwd: Option<PathBuf>,
}

const MAX_HISTORY_FLOW_STATES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
enum HistoryDirectoryChange {
    NotApplicable,
    Failed,
    Known(PathBuf),
    Unknown,
}

fn push_history_flow_state(states: &mut Vec<HistoryFlowState>, state: HistoryFlowState) {
    if !states.contains(&state) {
        states.push(state);
    }
}

fn command_segment_words(command: &str) -> Option<Vec<HistorySegment>> {
    let parsed = crate::parser::parse_line(command, command.len()).ok()?;
    if parsed
        .tokens
        .iter()
        .any(|token| token.quote == crate::parser::QuoteContext::Opaque)
    {
        return None;
    }
    let mut segments = Vec::new();
    let mut start = 0;
    for token in &parsed.tokens {
        if matches!(token.kind, crate::parser::TokenKind::Comment) {
            push_history_segment(
                &mut segments,
                &parsed.tokens,
                start..token.range.start,
                HistorySegmentLink::End,
            );
            start = command.len();
            break;
        }
        let next = match token.kind {
            crate::parser::TokenKind::Pipe => Some(HistorySegmentLink::Pipe),
            crate::parser::TokenKind::AndIf => Some(HistorySegmentLink::OnSuccess),
            crate::parser::TokenKind::OrIf => Some(HistorySegmentLink::OnFailure),
            crate::parser::TokenKind::Separator
                if command
                    .get(token.range.clone())
                    .is_some_and(|operator| operator == "&") =>
            {
                Some(HistorySegmentLink::Background)
            }
            crate::parser::TokenKind::Separator => Some(HistorySegmentLink::Always),
            _ => None,
        };
        if let Some(next) = next {
            push_history_segment(
                &mut segments,
                &parsed.tokens,
                start..token.range.start,
                next,
            );
            start = token.range.end;
        }
    }
    if start < command.len() {
        push_history_segment(
            &mut segments,
            &parsed.tokens,
            start..command.len(),
            HistorySegmentLink::End,
        );
    }
    mark_history_background_groups(&mut segments);
    Some(segments)
}

fn push_history_segment(
    segments: &mut Vec<HistorySegment>,
    tokens: &[crate::parser::Token],
    range: std::ops::Range<usize>,
    next: HistorySegmentLink,
) {
    let words = crate::parser::semantic_word_tokens(tokens, &range)
        .into_iter()
        .map(|token| token.cooked_prefix.clone())
        .collect::<Vec<_>>();
    if !words.is_empty() {
        segments.push(HistorySegment {
            words,
            next,
            backgrounded: false,
        });
    }
}

fn mark_history_background_groups(segments: &mut [HistorySegment]) {
    let mut group_start = 0;
    for index in 0..segments.len() {
        match segments[index].next {
            HistorySegmentLink::Background => {
                for segment in &mut segments[group_start..=index] {
                    segment.backgrounded = true;
                }
                group_start = index + 1;
            }
            HistorySegmentLink::Always | HistorySegmentLink::End => {
                group_start = index + 1;
            }
            HistorySegmentLink::OnSuccess
            | HistorySegmentLink::OnFailure
            | HistorySegmentLink::Pipe => {}
        }
    }
}

fn history_directory_change(
    context: &CompletionContext,
    words: &[String],
    aliases: &crate::shell::ShellAliases,
) -> HistoryDirectoryChange {
    let cooked = words.iter().map(String::as_str).collect::<Vec<_>>();
    let analysis =
        crate::parser::effective_command_analysis_for_shell(&cooked, false, context.shell);
    let command_index = match analysis.state {
        crate::parser::EffectiveCommandState::Found(index) => index,
        crate::parser::EffectiveCommandState::WrapperCommand(_)
        | crate::parser::EffectiveCommandState::IndeterminateWrapper(_)
        | crate::parser::EffectiveCommandState::AwaitingCommand
        | crate::parser::EffectiveCommandState::AwaitingWrapperValue => {
            return HistoryDirectoryChange::NotApplicable;
        }
    };
    let Some(command) = cooked.get(command_index).copied() else {
        return HistoryDirectoryChange::NotApplicable;
    };
    if super::executable_basename(command) != "cd"
        || command.contains('/')
        || analysis.privileged
        || analysis.opaque
        || analysis.kind == crate::parser::EffectiveCommandKind::External
    {
        return HistoryDirectoryChange::NotApplicable;
    }
    if analysis.kind == crate::parser::EffectiveCommandKind::Shell && aliases.contains(command) {
        return HistoryDirectoryChange::Unknown;
    }
    if cooked[..command_index]
        .iter()
        .any(|word| matches!(*word, "!" | "not" | "and" | "or"))
    {
        return HistoryDirectoryChange::Unknown;
    }

    let mut path = None;
    let mut options = true;
    for argument in cooked.get(command_index + 1..).unwrap_or_default() {
        if options && *argument == "--" {
            options = false;
            continue;
        }
        if options && argument.starts_with('-') {
            if *argument == "-"
                || argument.len() == 1
                || !argument[1..].chars().all(|flag| matches!(flag, 'L' | 'P'))
            {
                return HistoryDirectoryChange::Unknown;
            }
            continue;
        }
        options = false;
        if path.replace(*argument).is_some() {
            return HistoryDirectoryChange::Unknown;
        }
    }

    let target = match path {
        Some("") => return HistoryDirectoryChange::Failed,
        Some(value) => {
            if value
                .chars()
                .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
                || (value.starts_with('~') && value != "~" && !value.starts_with("~/"))
            {
                return HistoryDirectoryChange::Unknown;
            }
            resolve_history_path(&context.cwd, value)
        }
        None => {
            let Some(home) = std::env::home_dir() else {
                return HistoryDirectoryChange::Failed;
            };
            home
        }
    };
    match std::fs::canonicalize(target) {
        Ok(directory) if directory.is_dir() => HistoryDirectoryChange::Known(directory),
        Ok(_) | Err(_) => HistoryDirectoryChange::Failed,
    }
}

fn resolve_history_path(base: &Path, value: &str) -> std::path::PathBuf {
    if value == "~"
        && let Some(home) = std::env::home_dir()
    {
        return home;
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    let value = std::path::PathBuf::from(value);
    if value.is_absolute() {
        value
    } else {
        base.join(value)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration};

    use super::*;
    use crate::{
        completion::{BufferSnapshot, SyncQuality, rank_and_dedupe},
        history::HistoryPolicy,
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    fn context(text: &str, previous_command: Option<&str>) -> CompletionContext {
        context_at(text, text.len(), previous_command)
    }

    fn context_for_shell(text: &str, shell: ShellKind) -> CompletionContext {
        context_at_for_shell(text, text.len(), None, shell)
    }

    fn context_at(text: &str, cursor: usize, previous_command: Option<&str>) -> CompletionContext {
        context_at_for_shell(text, cursor, previous_command, ShellKind::Zsh)
    }

    fn context_at_for_shell(
        text: &str,
        cursor: usize,
        previous_command: Option<&str>,
        shell: ShellKind,
    ) -> CompletionContext {
        let buffer = BufferSnapshot::new(text, cursor, BufferRevision::new(1), SyncQuality::Exact)
            .expect("buffer");
        CompletionContext::new(QueryId::new(1), shell, PathBuf::from("/tmp"), buffer)
            .expect("context")
            .with_previous_command(previous_command.map(str::to_owned))
    }

    fn context_in(directory: &Path, text: &str, mode: CompletionMode) -> CompletionContext {
        let buffer =
            BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer");
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            directory.canonicalize().expect("canonical directory"),
            buffer,
        )
        .expect("context")
        .with_mode(mode)
    }

    /// A PATH cache with the executables the fixtures rely on.
    fn provider_with_executables(index: HistoryIndex, names: &[&str]) -> HistoryProvider {
        provider_with_executables_and_help(index, names, Arc::new(CommandHelpCache::default()))
    }

    fn provider_with_executables_and_help(
        index: HistoryIndex,
        names: &[&str],
        help: Arc<CommandHelpCache>,
    ) -> HistoryProvider {
        let directory = tempfile::tempdir().expect("command directory");
        for name in names {
            let path = directory.path().join(name);
            fs::write(&path, b"#!/bin/sh\n").expect("fake command");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("mode");
        }
        let path = std::ffi::OsString::from(directory.path());
        let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
        for name in names {
            if help.peek(name).is_none() {
                help.seed(name, CommandHelp::default());
            }
        }
        HistoryProvider::new(
            Arc::new(RwLock::new(index)),
            commands,
            Arc::new(AliasCache::default()),
            Arc::new(SpecRegistry::default()),
            help,
        )
    }

    fn provider_with_project(
        index: HistoryIndex,
        names: &[&str],
        project_cache: Arc<ProjectCache>,
    ) -> HistoryProvider {
        provider_with_executables(index, names).with_project_cache(project_cache)
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
        let provider = provider_with_executables(history_index(), &["git"]);

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
        let provider = provider_with_executables(index, &["make"]);
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

    #[test]
    fn argument_position_history_must_continue_the_typed_words() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        // The only row that genuinely continues `kimi `.
        index.ingest("kimi web", 1_000, ShellKind::Zsh, None, Some(0), &policy);
        // Contains `kimi ` as a substring, but is an unrelated command.
        index.ingest(
            "echo kimi rocks",
            2_000,
            ShellKind::Zsh,
            None,
            Some(0),
            &policy,
        );
        // Matches `kimi ` only as a subsequence (k…i…m…i…space).
        index.ingest(
            "docker build -t myimage .",
            3_000,
            ShellKind::Zsh,
            None,
            Some(0),
            &policy,
        );
        index.ingest(
            "kimi > logs/output.log",
            4_000,
            ShellKind::Zsh,
            None,
            Some(0),
            &policy,
        );
        let provider = provider_with_executables(index, &["kimi", "docker"]);

        let primaries = |text: &str| {
            let context = context(text, None);
            let mut primaries: Vec<_> = provider
                .complete(&context)
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect();
            primaries.sort();
            primaries
        };

        // Past the command token only rows that genuinely continue the full
        // typed prefix may surface.
        assert_eq!(
            primaries("kimi "),
            vec!["kimi > logs/output.log", "kimi web"]
        );
        assert_eq!(primaries("kimi w"), vec!["kimi web"]);
        assert_eq!(primaries("kimi > lo"), vec!["kimi > logs/output.log"]);
        // Normal completion accepts literal prefixes only.
        assert_eq!(primaries("kim"), vec!["kimi > logs/output.log", "kimi web"]);
        assert!(primaries("dob").is_empty());

        // Explicit history search keeps broad fuzzy recall for remembered
        // fragments without leaking that low-confidence behavior into the
        // normal recommendation list.
        let history_only = context("dob", None).with_mode(CompletionMode::HistoryOnly);
        let rows: Vec<_> = provider
            .complete(&history_only)
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["docker build -t myimage ."]);
    }

    #[test]
    fn history_continuations_respect_token_boundaries_and_midline_suffixes() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in ["kimi web", "kimiko deploy", "git status --short"] {
            index.ingest(command, 1_000, ShellKind::Zsh, None, Some(0), &policy);
        }
        let provider = provider_with_executables(index, &["kimi", "kimiko", "git"]);

        let output = provider.complete(&context("kimi ", None));
        let rows: Vec<_> = output
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert_eq!(rows, ["kimi web"]);

        let text = "git sta --short";
        let output = provider.complete(&context_at(text, "git sta".len(), None));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "git status --short")
        );

        let text = "git sta --long";
        let output = provider.complete(&context_at(text, "git sta".len(), None));
        assert!(output.candidates.is_empty());
    }

    #[test]
    fn later_command_segments_use_the_full_line_as_the_history_anchor() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in [
            "echo ok && codex review",
            "codex unrelated",
            "echo ok && cargo doc",
        ] {
            index.ingest(command, 1_000, ShellKind::Zsh, None, Some(0), &policy);
        }
        let provider = provider_with_executables(index, &["echo", "codex", "cargo"]);
        let output = provider.complete(&context("echo ok && cod", None));
        let rows: Vec<_> = output
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert_eq!(rows, ["echo ok && codex review"]);
    }

    #[test]
    fn history_rows_with_unknown_commands_are_filtered() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for (command, kept) in [
            ("git status", true),                      // executable on PATH
            ("gti status", false),                     // typo: not executable
            ("sl -la", false),                         // typo
            ("sudo gti status", false),                // wrapper peeled, still a typo
            ("FOO=bar git diff", true),                // assignment peeled
            ("cd /tmp", true),                         // builtin
            ("if true; then echo ok; fi", true),       // reserved word in shell syntax
            ("builtin if", false),                     // keyword is not a callable builtin
            ("command if", false),                     // `command` cannot execute keywords
            ("for f in *; do git add $f; done", true), // shell keyword
            ("./run.sh --fast", false),                // missing explicit path
            ("echo done | gti log", false),            // later segments are validated too
        ] {
            index.ingest(command, 1_000, ShellKind::Zsh, None, Some(0), &policy);
            let provider = provider_with_executables(HistoryIndex::default(), &["git"]);
            assert_eq!(
                provider.plausible_command(&context(command, None), command),
                kept,
                "plausibility of {command:?}"
            );
        }
        // End to end: the typo row never leaves the provider.
        let provider = provider_with_executables(index, &["git"]);
        let output = provider.complete(&context("g", None));
        let primaries: Vec<_> = output
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.as_str())
            .collect();
        assert!(primaries.contains(&"git status"), "rows: {primaries:?}");
        assert!(!primaries.contains(&"gti status"), "rows: {primaries:?}");
    }

    #[test]
    fn explicit_history_commands_must_still_be_executable() {
        let directory = tempfile::tempdir().expect("directory");
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        let script = bin.join("run.sh");
        fs::write(&script, b"#!/bin/sh\n").expect("script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("executable mode");
        let provider = provider_with_executables(HistoryIndex::default(), &[]);
        let mut current = context("./bin/run.sh --fast", None);
        current.cwd = Arc::new(directory.path().to_owned());

        assert!(provider.plausible_command(&current, "./bin/run.sh --fast"));
        assert!(provider.plausible_command(&current, "env -C bin ./run.sh --fast"));

        fs::set_permissions(&script, fs::Permissions::from_mode(0o600)).expect("plain mode");
        assert!(!provider.plausible_command(&current, "./bin/run.sh --fast"));
        assert!(!provider.plausible_command(&current, "env -C bin ./run.sh --fast"));
    }

    #[test]
    fn history_subcommand_typos_are_filtered_against_cached_help() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for (offset, (command, exit_code)) in [
            ("git pull", Some(1)),
            ("git pul", None),
            ("git push origin main", Some(1)),
            ("git pushh", Some(1)),
            ("git psuh", None),
            ("git -C repo pull", Some(1)),
            ("git -C repo pul", None),
            ("git custom-tool", None),
            ("echo ok && git pull", Some(1)),
            ("echo ok && git pul", None),
            ("codex resume", Some(1)),
            ("codex e", None),
            ("codex fix this bug", None),
            ("codex --config value resume", None),
            ("codex --confg value resume", None),
            // Hybrid prompt CLIs still reject a command-like near miss even
            // if the root process happened to exit successfully.
            ("codex upgrad", Some(0)),
        ]
        .into_iter()
        .enumerate()
        {
            index.ingest(
                command,
                1_000 + offset as i64,
                ShellKind::Zsh,
                None,
                exit_code,
                &policy,
            );
        }

        let entry = |name: &str| HelpEntry {
            name: name.to_owned(),
            description: String::new(),
            takes_value: false,
        };
        let help = Arc::new(CommandHelpCache::default());
        help.seed(
            "git",
            CommandHelp {
                flags: vec![HelpEntry {
                    name: "-C".into(),
                    description: String::new(),
                    takes_value: true,
                }],
                subcommands: vec![entry("pull"), entry("push")],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                // Man-derived Git lists are intentionally extensible.
                subcommands_exhaustive: false,
            },
        );
        help.seed(
            "codex",
            CommandHelp {
                flags: vec![HelpEntry {
                    name: "--config".into(),
                    description: String::new(),
                    takes_value: true,
                }],
                subcommands: vec![entry("resume"), entry("review"), entry("update")],
                subcommand_aliases: vec!["e".into()],
                accepts_positionals: true,
                subcommands_exhaustive: true,
            },
        );
        help.seed_scope("git", &["push"], CommandHelp::default());
        let provider = provider_with_executables_and_help(index, &["git", "codex"], help);

        let rows = |text: &str| {
            provider
                .complete(&context(text, None))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };
        let git = rows("git p");
        assert!(git.contains(&"git pull".to_owned()), "rows: {git:?}");
        assert!(
            git.contains(&"git push origin main".to_owned()),
            "rows: {git:?}"
        );
        for typo in ["git pul", "git pushh", "git psuh"] {
            assert!(!git.contains(&typo.to_owned()), "rows: {git:?}");
        }
        assert_eq!(rows("git -C repo p"), ["git -C repo pull"]);
        assert_eq!(rows("git c"), ["git custom-tool"]);
        assert_eq!(rows("echo ok && git p"), ["echo ok && git pull"]);
        let codex = rows("codex ");
        assert!(
            codex.contains(&"codex resume".to_owned()),
            "rows: {codex:?}"
        );
        assert!(codex.contains(&"codex e".to_owned()), "rows: {codex:?}");
        assert!(
            codex.contains(&"codex fix this bug".to_owned()),
            "free prompt history must survive: {codex:?}"
        );
        assert!(
            !codex.contains(&"codex upgrad".to_owned()),
            "rows: {codex:?}"
        );
        assert_eq!(
            rows("codex --c"),
            ["codex --config value resume"],
            "a one-edit top-level flag typo must not survive history"
        );
    }

    #[test]
    fn maven_lifecycle_typos_are_filtered_without_running_the_build() {
        assert_eq!(
            maven_history_arguments_are_plausible("mvn", &["install"], false),
            Some(true)
        );
        assert_eq!(
            maven_history_arguments_are_plausible("./mvnw", &["instal"], false),
            Some(false)
        );
        assert_eq!(
            maven_history_arguments_are_plausible("mvn", &["-q", "pakage"], false),
            Some(false)
        );
        assert_eq!(
            maven_history_arguments_are_plausible("mvn", &["dependency:tree"], false),
            Some(true)
        );
        assert_eq!(
            maven_history_arguments_are_plausible("mvn", &["clean", "pakage"], false),
            Some(false),
            "every lifecycle phase must be checked, not only the first"
        );
        assert_eq!(
            maven_history_arguments_are_plausible("mvn", &["dependency:tree", "pakage"], false,),
            Some(false),
            "an extension goal must not hide a later lifecycle typo"
        );
        assert_eq!(
            maven_history_arguments_are_plausible("mvn", &["process-test-resources"], false),
            Some(true)
        );
        assert_eq!(
            maven_history_arguments_are_plausible("mvn", &["instal"], true),
            Some(true),
            "a recorded successful extension command must be preserved"
        );
    }

    #[test]
    fn nested_history_typos_are_filtered_against_scoped_help() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for (offset, command) in [
            "gh pr create",
            "gh pr creat",
            "gh pr list",
            "gh pr lits",
            "gh pr create --fill",
            "gh pr create --fil",
        ]
        .into_iter()
        .enumerate()
        {
            index.ingest(
                command,
                1_000 + offset as i64,
                ShellKind::Zsh,
                None,
                None,
                &policy,
            );
        }

        let entry = |name: &str| HelpEntry {
            name: name.to_owned(),
            description: String::new(),
            takes_value: false,
        };
        let help = Arc::new(CommandHelpCache::default());
        help.seed(
            "gh",
            CommandHelp {
                flags: Vec::new(),
                subcommands: vec![entry("pr")],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: true,
            },
        );
        help.seed_scope(
            "gh",
            &["pr"],
            CommandHelp {
                flags: Vec::new(),
                subcommands: vec![entry("create"), entry("list")],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: true,
            },
        );
        help.seed_scope(
            "gh",
            &["pr", "create"],
            CommandHelp {
                flags: vec![HelpEntry {
                    name: "--fill".into(),
                    description: String::new(),
                    takes_value: false,
                }],
                subcommands: Vec::new(),
                subcommand_aliases: Vec::new(),
                accepts_positionals: true,
                subcommands_exhaustive: false,
            },
        );
        let provider = provider_with_executables_and_help(index, &["gh"], help);
        let rows = |text: &str| {
            provider
                .complete(&context(text, None))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };

        let create = rows("gh pr c");
        assert!(
            create.contains(&"gh pr create".to_owned()),
            "rows: {create:?}"
        );
        assert!(
            create.contains(&"gh pr create --fill".to_owned()),
            "rows: {create:?}"
        );
        assert!(
            !create.contains(&"gh pr creat".to_owned()),
            "rows: {create:?}"
        );

        assert_eq!(rows("gh pr l"), ["gh pr list"]);
        assert_eq!(rows("gh pr create --f"), ["gh pr create --fill"]);
    }

    #[test]
    fn successful_extensible_subcommands_override_the_typo_heuristic() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        index.ingest("git pul", 1_000, ShellKind::Zsh, None, Some(0), &policy);
        let help = Arc::new(CommandHelpCache::default());
        help.seed(
            "git",
            CommandHelp {
                flags: Vec::new(),
                subcommands: vec![HelpEntry {
                    name: "pull".into(),
                    description: String::new(),
                    takes_value: false,
                }],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: false,
            },
        );
        let provider = provider_with_executables_and_help(index, &["git"], help);
        let rows: Vec<_> = provider
            .complete(&context("git p", None))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["git pul"]);
    }

    #[test]
    fn pending_help_defers_argument_history_until_it_can_be_validated() {
        let directory = tempfile::tempdir().expect("directory");
        let executable = directory.path().join("demo-tool");
        fs::write(&executable, b"#!/bin/sh\n").expect("fake command");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("mode");
        let commands = Arc::new(CommandPathCache::from_path(Some(
            &std::ffi::OsString::from(directory.path()),
        )));
        let help = Arc::new(CommandHelpCache::default());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        help.request_with_path("demo-tool", commands.path("demo-tool"), move |_| {
            started_tx.send(()).expect("started");
            release_rx.recv().expect("released");
            CommandHelp {
                flags: Vec::new(),
                subcommands: vec![HelpEntry {
                    name: "good".into(),
                    description: String::new(),
                    takes_value: false,
                }],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: true,
            }
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("help request started");

        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in ["demo-tool good", "demo-tool godd"] {
            index.ingest(command, 1_000, ShellKind::Zsh, None, None, &policy);
        }
        let provider = HistoryProvider::new(
            Arc::new(RwLock::new(index)),
            commands,
            Arc::new(AliasCache::default()),
            Arc::new(SpecRegistry::default()),
            Arc::clone(&help),
        );
        assert!(
            provider
                .complete(&context("demo-tool ", None))
                .candidates
                .is_empty(),
            "pending help must not allow unvalidated history to flash"
        );

        release_tx.send(()).expect("release help");
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while help.peek("demo-tool").is_none() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(
            help.peek("demo-tool").expect("cached help").subcommands[0].name,
            "good"
        );
        let rows: Vec<_> = provider
            .complete(&context("demo-tool ", None))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["demo-tool good"]);
    }

    #[test]
    fn package_manager_native_subcommand_typos_are_filtered() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in [
            "npm install",
            "npm instal",
            "npm --prefix repo install",
            "npm --prefix repo instal",
            "pnpm -C repo install",
            "pnpm -C repo instal",
            "bun upgrade",
            "bun upgrad",
        ] {
            index.ingest(command, 1_000, ShellKind::Zsh, None, None, &policy);
        }
        let provider = provider_with_executables(index, &["npm", "pnpm", "bun"]);
        let rows = |text: &str| {
            provider
                .complete(&context(text, None))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };
        assert_eq!(rows("npm i"), ["npm install"]);
        assert_eq!(rows("npm --prefix repo i"), ["npm --prefix repo install"]);
        assert_eq!(rows("pnpm -C repo i"), ["pnpm -C repo install"]);
        assert_eq!(rows("bun up"), ["bun upgrade"]);
    }

    #[test]
    fn package_manager_script_history_is_filtered_by_the_current_manifest() {
        let project = tempfile::tempdir().expect("project");
        let other = tempfile::tempdir().expect("other project");
        fs::write(
            project.path().join("package.json"),
            r#"{"scripts":{"dev":"vite","build":"vite build"}}"#,
        )
        .expect("package manifest");
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in [
            "pnpm run dev",
            "pnpm dev",
            "npm run dev",
            "npm run --if-present dev",
            "yarn dev",
            "bun dev",
            "pnpm run dev -- --watch",
        ] {
            index.ingest(
                command,
                1_000,
                ShellKind::Zsh,
                Some(project.path()),
                Some(0),
                &policy,
            );
        }
        for _ in 0..20 {
            index.ingest(
                "pnpm run deploy",
                1_001,
                ShellKind::Zsh,
                Some(other.path()),
                Some(0),
                &policy,
            );
        }
        for command in [
            "pnpm run missing",
            "pnpm missing",
            "pnpm run --if-present missing",
            "npm run --if-present missing",
            "npm run missing --if-present",
            "yarn missing",
            "bun missing",
        ] {
            index.ingest(
                command,
                1_002,
                ShellKind::Zsh,
                Some(other.path()),
                Some(0),
                &policy,
            );
        }
        let provider = provider_with_project(
            index,
            &["pnpm", "npm", "yarn", "bun"],
            Arc::new(ProjectCache::default()),
        );
        let rows = |text: &str| {
            provider
                .complete(&context_in(
                    project.path(),
                    text,
                    CompletionMode::HistoryOnly,
                ))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };
        assert_eq!(rows("pnpm run missing"), Vec::<String>::new());
        assert_eq!(rows("pnpm missing"), Vec::<String>::new());
        assert!(rows("pnpm run --if-present missing").is_empty());
        assert!(rows("npm run --if-present missing").is_empty());
        assert!(rows("npm run missing --if-present").is_empty());
        assert_eq!(rows("pnpm run deploy"), Vec::<String>::new());
        assert_eq!(rows("pnpm run dev -- --watch"), ["pnpm run dev -- --watch"]);
        assert!(rows("npm run dev").contains(&"npm run dev".to_owned()));
        assert!(rows("npm run --if-present dev").contains(&"npm run --if-present dev".to_owned()));
        assert!(rows("yarn dev").contains(&"yarn dev".to_owned()));
        assert!(rows("bun dev").contains(&"bun dev".to_owned()));
        assert!(!rows("yarn missing").contains(&"yarn missing".to_owned()));
        assert!(!rows("bun missing").contains(&"bun missing".to_owned()));
    }

    #[test]
    fn renamed_package_script_invalidates_cached_history_validation() {
        let project = tempfile::tempdir().expect("project");
        let manifest = project.path().join("package.json");
        fs::write(&manifest, r#"{"scripts":{"old":"echo old"}}"#).expect("old manifest");
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in ["pnpm run old", "pnpm run replacement"] {
            index.ingest(
                command,
                1_000,
                ShellKind::Zsh,
                Some(project.path()),
                Some(0),
                &policy,
            );
        }
        let provider = provider_with_project(index, &["pnpm"], Arc::new(ProjectCache::default()));
        let rows = |text: &str| {
            provider
                .complete(&context_in(
                    project.path(),
                    text,
                    CompletionMode::HistoryOnly,
                ))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };
        assert_eq!(rows("pnpm run old"), ["pnpm run old"]);
        fs::write(
            &manifest,
            r#"{"scripts":{"replacement":"echo replacement"}}"#,
        )
        .expect("replacement manifest");
        assert!(rows("pnpm run old").is_empty());
        assert_eq!(rows("pnpm run replacement"), ["pnpm run replacement"]);
    }

    #[test]
    fn invalid_manifest_fails_closed_for_literal_script_history() {
        let project = tempfile::tempdir().expect("project");
        fs::write(
            project.path().join("package.json"),
            br#"{"scripts":{"dev":"#,
        )
        .expect("invalid manifest");
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in [
            "pnpm run stale",
            "pnpm stale",
            "npm run stale",
            "node --run=stale",
            "pnpm install",
        ] {
            index.ingest(
                command,
                1_000,
                ShellKind::Zsh,
                Some(project.path()),
                Some(0),
                &policy,
            );
        }
        let provider = provider_with_project(
            index,
            &["pnpm", "npm", "node"],
            Arc::new(ProjectCache::default()),
        );
        for command in [
            "pnpm run stale",
            "pnpm stale",
            "npm run stale",
            "node --run=stale",
        ] {
            let rows: Vec<_> = provider
                .complete(&context_in(
                    project.path(),
                    command,
                    CompletionMode::HistoryOnly,
                ))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect();
            assert!(rows.is_empty(), "invalid manifest leaked {command:?}");
        }

        assert!(
            provider
                .complete(&context_in(project.path(), "pnpm ", CompletionMode::Normal))
                .candidates
                .is_empty(),
            "normal manager history must stay deferred while the manifest is invalid"
        );
    }

    #[test]
    fn node_run_history_uses_the_current_package_scripts() {
        let project = tempfile::tempdir().expect("project");
        fs::write(
            project.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .expect("manifest");
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in [
            "node --run=dev",
            "node --run dev",
            "node --run=missing",
            "nodejs --run missing",
        ] {
            index.ingest(
                command,
                1_000,
                ShellKind::Zsh,
                Some(project.path()),
                Some(0),
                &policy,
            );
        }
        let provider = provider_with_project(
            index,
            &["node", "nodejs"],
            Arc::new(ProjectCache::default()),
        );
        let rows = |text: &str| {
            provider
                .complete(&context_in(
                    project.path(),
                    text,
                    CompletionMode::HistoryOnly,
                ))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };
        assert!(rows("node --run=dev").contains(&"node --run=dev".to_owned()));
        assert!(rows("node --run dev").contains(&"node --run dev".to_owned()));
        assert!(rows("node --run=missing").is_empty());
        assert!(rows("nodejs --run missing").is_empty());
    }

    #[test]
    fn deno_task_history_uses_the_current_deno_manifest() {
        let project = tempfile::tempdir().expect("project");
        fs::write(
            project.path().join("deno.json"),
            r#"{"tasks":{"dev":"deno run main.ts"}}"#,
        )
        .expect("deno manifest");
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in ["deno task dev", "deno task missing"] {
            index.ingest(
                command,
                1_000,
                ShellKind::Zsh,
                Some(project.path()),
                Some(0),
                &policy,
            );
        }
        let provider = provider_with_project(index, &["deno"], Arc::new(ProjectCache::default()));
        let rows = |text: &str| {
            provider
                .complete(&context_in(
                    project.path(),
                    text,
                    CompletionMode::HistoryOnly,
                ))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };
        assert_eq!(rows("deno task dev"), ["deno task dev"]);
        assert!(rows("deno task missing").is_empty());
    }

    #[test]
    fn package_manager_directory_options_validate_the_selected_manifest() {
        let root = tempfile::tempdir().expect("root");
        let app = root.path().join("app");
        fs::create_dir(&app).expect("app directory");
        fs::write(app.join("package.json"), r#"{"scripts":{"dev":"vite"}}"#).expect("app manifest");
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in [
            "pnpm -C app run dev",
            "pnpm -Capp run dev",
            "pnpm run -C app dev",
            "npm --prefix app run dev",
            "npm --prefix=app run dev",
            "npm run --prefix app dev",
            "yarn --cwd app run dev",
            "bun --cwd app run dev",
            "pnpm -C app run missing",
            "npm --prefix app run missing",
        ] {
            index.ingest(
                command,
                1_000,
                ShellKind::Zsh,
                Some(root.path()),
                Some(0),
                &policy,
            );
        }
        let provider = provider_with_project(
            index,
            &["pnpm", "npm", "yarn", "bun"],
            Arc::new(ProjectCache::default()),
        );
        let rows = |text: &str| {
            provider
                .complete(&context_in(root.path(), text, CompletionMode::HistoryOnly))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };
        for command in [
            "pnpm -C app run dev",
            "pnpm -Capp run dev",
            "pnpm run -C app dev",
            "npm --prefix app run dev",
            "npm --prefix=app run dev",
            "npm run --prefix app dev",
            "yarn --cwd app run dev",
            "bun --cwd app run dev",
        ] {
            assert!(
                rows(command).contains(&command.to_owned()),
                "valid selected-project script was filtered for {command:?}"
            );
        }
        assert!(rows("pnpm -C app run missing").is_empty());
        assert!(rows("npm --prefix app run missing").is_empty());
    }

    #[test]
    fn compound_history_uses_the_directory_selected_by_cd() {
        let root = tempfile::tempdir().expect("root");
        let app = root.path().join("app");
        let dashed = root.path().join("-app");
        fs::create_dir(&app).expect("app directory");
        fs::create_dir(&dashed).expect("dashed directory");
        fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"root-script":"echo root"}}"#,
        )
        .expect("root manifest");
        for directory in [&app, &dashed] {
            fs::write(
                directory.join("package.json"),
                r#"{"scripts":{"dev":"vite"}}"#,
            )
            .expect("app manifest");
        }
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in [
            "cd app && pnpm dev",
            "cd -P app; pnpm dev",
            "cd -- -app && pnpm dev",
            "cd app && pnpm missing",
            "cd missing || pnpm root-script",
            "cd missing || pnpm dev",
            "cd app || pnpm root-script; pnpm dev",
            "cd app || pnpm root-script; pnpm missing",
            "cd missing && pnpm root-script",
            "cd missing && pnpm dev",
        ] {
            index.ingest(
                command,
                1_000,
                ShellKind::Zsh,
                Some(root.path()),
                Some(0),
                &policy,
            );
        }
        let provider = provider_with_project(index, &["pnpm"], Arc::new(ProjectCache::default()));
        let rows = |text: &str| {
            provider
                .complete(&context_in(root.path(), text, CompletionMode::HistoryOnly))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };
        for command in [
            "cd app && pnpm dev",
            "cd -P app; pnpm dev",
            "cd -- -app && pnpm dev",
            "cd missing || pnpm root-script",
            "cd app || pnpm root-script; pnpm dev",
            "cd missing && pnpm root-script",
        ] {
            assert!(
                rows(command).contains(&command.to_owned()),
                "valid compound history was filtered for {command:?}"
            );
        }
        assert!(!rows("cd app && pnpm missing").contains(&"cd app && pnpm missing".to_owned()));
        assert!(!rows("cd missing || pnpm dev").contains(&"cd missing || pnpm dev".to_owned()));
        assert!(
            !rows("cd app || pnpm root-script; pnpm missing")
                .contains(&"cd app || pnpm root-script; pnpm missing".to_owned()),
            "cwd from a successful cd must survive a skipped || branch"
        );
        assert!(!rows("cd missing && pnpm dev").contains(&"cd missing && pnpm dev".to_owned()));
    }

    #[test]
    fn pipeline_and_background_cd_do_not_change_history_validation_cwd() {
        let root = tempfile::tempdir().expect("root");
        let app = root.path().join("app");
        fs::create_dir(&app).expect("app directory");
        fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"root-script":"echo root"}}"#,
        )
        .expect("root manifest");
        fs::write(app.join("package.json"), r#"{"scripts":{"dev":"vite"}}"#).expect("app manifest");
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in [
            "cd app | pnpm root-script",
            "cd app | pnpm dev",
            "cd app & pnpm root-script",
            "cd app & pnpm dev",
            "cd app && pnpm dev & pnpm root-script",
            "cd app && pnpm dev & pnpm dev",
            "cd app & cd app & pnpm root-script",
            "cd app & cd app & pnpm dev",
        ] {
            index.ingest(
                command,
                1_000,
                ShellKind::Zsh,
                Some(root.path()),
                Some(0),
                &policy,
            );
        }
        let provider = provider_with_project(index, &["pnpm"], Arc::new(ProjectCache::default()));
        let rows = |text: &str| {
            provider
                .complete(&context_in(root.path(), text, CompletionMode::HistoryOnly))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };
        assert!(
            rows("cd app | pnpm root-script").contains(&"cd app | pnpm root-script".to_owned())
        );
        assert!(
            rows("cd app & pnpm root-script").contains(&"cd app & pnpm root-script".to_owned())
        );
        assert!(
            rows("cd app && pnpm dev & pnpm root-script")
                .contains(&"cd app && pnpm dev & pnpm root-script".to_owned()),
            "the background group must use app internally and restore root afterward"
        );
        assert!(
            rows("cd app & cd app & pnpm root-script")
                .contains(&"cd app & cd app & pnpm root-script".to_owned()),
            "each consecutive background group must restore the parent cwd"
        );
        for command in [
            "cd app | pnpm dev",
            "cd app & pnpm dev",
            "cd app && pnpm dev & pnpm dev",
            "cd app & cd app & pnpm dev",
        ] {
            assert!(
                !rows(command).contains(&command.to_owned()),
                "invalid background or pipeline script leaked for {command:?}"
            );
        }
    }

    #[test]
    fn dynamic_cd_history_fails_open_instead_of_using_the_wrong_manifest() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("package.json"), r#"{"scripts":{}}"#).expect("root manifest");
        let provider = provider_with_project(
            HistoryIndex::default(),
            &["pnpm"],
            Arc::new(ProjectCache::default()),
        );
        for command in [
            "cd - && pnpm dev",
            "cd $PROJECT && pnpm dev",
            "cd old new; pnpm dev",
        ] {
            assert!(
                provider.plausible_command(
                    &context_in(root.path(), command, CompletionMode::HistoryOnly),
                    command,
                ),
                "dynamic cwd was incorrectly validated for {command:?}"
            );
        }
    }

    #[test]
    fn wrapped_manager_history_still_uses_the_current_manifest() {
        let project = tempfile::tempdir().expect("project");
        fs::write(
            project.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"}}"#,
        )
        .expect("manifest");
        let provider = provider_with_project(
            HistoryIndex::default(),
            &["corepack", "pnpm", "npm", "sudo", "env"],
            Arc::new(ProjectCache::default()),
        );
        for command in [
            "corepack pnpm run dev",
            "sudo pnpm dev",
            "env -C . npm run dev",
        ] {
            assert!(
                provider.plausible_command(
                    &context_in(project.path(), command, CompletionMode::HistoryOnly),
                    command,
                ),
                "valid wrapped script was filtered for {command:?}"
            );
        }
        for command in [
            "corepack pnpm run missing",
            "sudo pnpm missing",
            "env -C . npm run missing",
        ] {
            assert!(
                !provider.plausible_command(
                    &context_in(project.path(), command, CompletionMode::HistoryOnly),
                    command,
                ),
                "invalid wrapped script leaked for {command:?}"
            );
        }
    }

    #[test]
    fn workspace_script_history_respects_member_and_recursive_semantics() {
        let root = tempfile::tempdir().expect("workspace");
        let app = root.path().join("packages/app");
        let api = root.path().join("packages/api");
        fs::create_dir_all(&app).expect("app");
        fs::create_dir_all(&api).expect("api");
        fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"root-only":"echo root"},"workspaces":["packages/*"]}"#,
        )
        .expect("root manifest");
        fs::write(
            app.join("package.json"),
            r#"{"name":"app","scripts":{"dev":"vite"}}"#,
        )
        .expect("app manifest");
        fs::write(
            api.join("package.json"),
            r#"{"name":"api","scripts":{"build":"tsc"}}"#,
        )
        .expect("api manifest");
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in [
            "pnpm --filter app run dev",
            "pnpm --filter app --filter api run dev",
            "npm --workspace app run dev",
            "npm --workspace app --workspace api run dev",
            "npm --workspace app --workspace api --if-present run dev",
            "yarn workspace app run dev",
            "pnpm -r run dev",
            "npm --workspaces run dev",
            "npm --workspaces --if-present run dev",
            "pnpm -w run root-only",
            "pnpm --filter app run missing",
            "pnpm --filter app --filter api run missing",
            "pnpm --filter app run --if-present missing",
            "npm --workspace app run --if-present missing",
            "npm --workspace app --workspace api run missing",
            "npm --workspace app --workspace api --if-present run missing",
        ] {
            index.ingest(
                command,
                1_000,
                ShellKind::Zsh,
                Some(root.path()),
                Some(0),
                &policy,
            );
        }
        let provider = provider_with_project(
            index,
            &["pnpm", "npm", "yarn"],
            Arc::new(ProjectCache::default()),
        );
        let rows = |text: &str| {
            provider
                .complete(&context_in(root.path(), text, CompletionMode::HistoryOnly))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };
        for command in [
            "pnpm --filter app run dev",
            "pnpm --filter app --filter api run dev",
            "npm --workspace app run dev",
            "npm --workspace app --workspace api --if-present run dev",
            "yarn workspace app run dev",
            "pnpm -r run dev",
            "npm --workspaces --if-present run dev",
            "pnpm -w run root-only",
        ] {
            assert!(
                rows(command).contains(&command.to_owned()),
                "valid workspace script was filtered for {command:?}"
            );
        }
        assert!(rows("pnpm --filter app run missing").is_empty());
        assert!(rows("pnpm --filter app --filter api run missing").is_empty());
        assert!(rows("pnpm --filter app run --if-present missing").is_empty());
        assert!(rows("npm --workspace app run --if-present missing").is_empty());
        assert!(rows("npm --workspace app --workspace api run missing").is_empty());
        assert!(rows("npm --workspace app --workspace api --if-present run missing").is_empty());
        assert!(
            !rows("npm --workspaces run dev").contains(&"npm --workspaces run dev".to_owned()),
            "npm without --if-present must require every selected workspace"
        );
        assert!(
            !rows("npm --workspace app --workspace api run dev")
                .contains(&"npm --workspace app --workspace api run dev".to_owned()),
            "npm multi-workspace scripts must require every selected workspace"
        );
    }

    #[test]
    fn package_manager_nested_subcommand_typos_are_filtered() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in [
            "pnpm store prune",
            "pnpm store prun",
            "npm cache clean",
            "npm cache cler",
        ] {
            index.ingest(command, 1_000, ShellKind::Zsh, None, None, &policy);
        }
        let help = Arc::new(CommandHelpCache::default());
        let scoped_help = |names: &[&str]| CommandHelp {
            flags: Vec::new(),
            subcommands: names
                .iter()
                .map(|name| HelpEntry {
                    name: (*name).to_owned(),
                    description: String::new(),
                    takes_value: false,
                })
                .collect(),
            subcommand_aliases: Vec::new(),
            accepts_positionals: false,
            subcommands_exhaustive: true,
        };
        help.seed_scope(
            "pnpm",
            &["store"],
            scoped_help(&["add", "path", "prune", "status"]),
        );
        help.seed_scope(
            "npm",
            &["cache"],
            scoped_help(&["add", "clean", "ls", "verify"]),
        );
        let provider =
            provider_with_executables_and_help(index, &["npm", "pnpm"], Arc::clone(&help));
        let rows = |text: &str| {
            provider
                .complete(&context(text, None))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect::<Vec<_>>()
        };
        assert_eq!(rows("pnpm store pr"), ["pnpm store prune"]);
        assert_eq!(rows("npm cache cl"), ["npm cache clean"]);
    }

    #[test]
    fn static_specs_do_not_make_missing_commands_plausible() {
        assert!(crate::specs::SpecRegistry::load(None).get("ls").is_some());
        let provider = provider_with_executables(HistoryIndex::default(), &[]);
        assert!(!provider.plausible_command(&context("ls -la", None), "ls -la"));
    }

    #[test]
    fn manager_history_requires_a_currently_runnable_manager() {
        let unavailable = provider_with_executables(HistoryIndex::default(), &[]);
        for command in [
            "pnpm dev",
            "npm run build",
            "yarn test",
            "bun run app",
            "deno task lint",
            "sudo pnpm dev",
        ] {
            assert!(
                !unavailable.plausible_command(&context(command, None), command),
                "unavailable manager history leaked for {command:?}"
            );
        }
        assert_eq!(
            unavailable.known_command_prefix(&context("pnp", None)),
            None
        );

        let available = provider_with_executables(
            HistoryIndex::default(),
            &["pnpm", "npm", "yarn", "bun", "deno"],
        );
        for command in [
            "pnpm install",
            "npm install",
            "yarn install",
            "bun install",
            "deno test",
        ] {
            assert!(
                available.plausible_command(&context(command, None), command),
                "installed manager history was filtered for {command:?}"
            );
        }
        for command in [
            "pnpm dev",
            "npm run build",
            "yarn test",
            "bun run app",
            "deno task lint",
            "sudo pnpm dev",
        ] {
            assert!(
                !available.plausible_command(&context(command, None), command),
                "script history without a current manifest leaked for {command:?}"
            );
        }
        assert!(!available.plausible_command(&context("pnpn dev", None), "pnpn dev"));
        assert_eq!(
            available.known_command_prefix(&context("pnp", None)),
            Some("pnp".into())
        );
    }

    #[test]
    fn invalid_history_rows_cannot_crowd_a_valid_row_out_of_the_top_k() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        index.ingest("git status", 1, ShellKind::Zsh, None, Some(0), &policy);
        for number in 0..75 {
            index.ingest_weighted(
                &format!("gti-{number:02} status"),
                10_000 + number,
                ShellKind::Zsh,
                None,
                50,
                Some(0),
                &policy,
            );
        }
        let provider = provider_with_executables(index, &["git"]);
        let rows: Vec<_> = provider
            .complete(&context("g", None))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["git status"]);
    }

    #[test]
    fn every_builtin_can_establish_its_command_prefix() {
        let provider = provider_with_executables(HistoryIndex::default(), &[]);
        for prefix in ["aut", "ret", "seto", "unf", "zmo"] {
            assert_eq!(
                provider.known_command_prefix(&context(prefix, None)),
                Some(prefix.to_owned()),
                "builtin prefix {prefix:?}"
            );
        }
        for shell in [ShellKind::Bash, ShellKind::Zsh, ShellKind::Fish] {
            for builtin in crate::providers::shell_builtins_and_keywords(shell) {
                assert!(crate::providers::is_shell_builtin_or_keyword(
                    shell, builtin
                ));
                assert!(crate::providers::shell_symbol_has_prefix(shell, builtin));
                if crate::providers::is_shell_builtin(shell, builtin) {
                    assert!(crate::providers::shell_builtin_has_prefix(shell, builtin));
                }
            }
        }
        assert!(!crate::providers::is_shell_builtin(ShellKind::Bash, "if"));
        assert!(!crate::providers::is_shell_builtin(
            ShellKind::Zsh,
            "nocorrect"
        ));
        assert!(!crate::providers::is_shell_builtin(ShellKind::Fish, "not"));
        assert!(crate::providers::is_shell_builtin_or_keyword(
            ShellKind::Bash,
            "shopt"
        ));
        assert!(crate::providers::is_shell_builtin_or_keyword(
            ShellKind::Zsh,
            "autoload"
        ));
        assert!(crate::providers::is_shell_builtin_or_keyword(
            ShellKind::Fish,
            "string"
        ));
        assert!(!crate::providers::is_shell_builtin_or_keyword(
            ShellKind::Bash,
            "autoload"
        ));
    }

    #[test]
    fn unparseable_or_opaque_history_rows_are_kept() {
        let provider = provider_with_executables(HistoryIndex::default(), &["git"]);
        let ctx = |text: &str| context(text, None);
        assert!(provider.plausible_command(&ctx("echo $(gti status)"), "echo $(gti status)"));
        assert!(provider.plausible_command(&ctx("echo 'unterminated"), "echo 'unterminated"));
        assert!(provider.plausible_command(&ctx(""), ""));
    }

    #[test]
    fn aliases_from_rc_files_are_not_mistaken_for_typos() {
        // `gc` is not on PATH, not a builtin, not spec-covered — but it is
        // defined in the user's rc files, so the row must survive.
        let mut aliases = crate::shell::ShellAliases::default();
        crate::shell::parse_rc_text(ShellKind::Zsh, "alias gc='git commit'\n", &mut aliases);
        let provider = HistoryProvider::new(
            Arc::new(RwLock::new(HistoryIndex::default())),
            Arc::new(CommandPathCache::default()),
            Arc::new(AliasCache::new_fixed(aliases)),
            Arc::new(SpecRegistry::default()),
            Arc::new(CommandHelpCache::default()),
        );
        assert!(provider.plausible_command(&context("gc", None), "gc"));
        assert!(provider.plausible_command(&context("time gc", None), "time gc"));
        assert!(!provider.plausible_command(&context("sudo gc", None), "sudo gc"));
        assert!(!provider.plausible_command(&context("command gc", None), "command gc"));

        // Without the alias definition the same row is filtered as a typo.
        let provider = provider_with_executables(HistoryIndex::default(), &["git"]);
        assert!(!provider.plausible_command(&context("gc", None), "gc"));
    }

    #[test]
    fn inferred_function_slots_remove_stale_history_arguments() {
        let projects = tempfile::tempdir().expect("projects");
        fs::create_dir(projects.path().join("skillscat")).expect("valid project");
        fs::create_dir(projects.path().join("aipass")).expect("valid project");

        let mut definitions = crate::shell::ShellAliases::default();
        crate::shell::parse_rc_text(
            ShellKind::Zsh,
            &format!(
                "proj() {{\n  if [ -n \"$1\" ]; then\n    cd \"{}/$1\"\n  else\n    cd \"{}\"\n  fi\n}}\n",
                projects.path().display(),
                projects.path().display()
            ),
            &mut definitions,
        );
        let aliases = Arc::new(AliasCache::new_fixed(definitions));

        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        index.ingest(
            "proj skillscat",
            1_000,
            ShellKind::Zsh,
            None,
            Some(0),
            &policy,
        );
        index.ingest("proj aipass", 1_001, ShellKind::Zsh, None, Some(0), &policy);
        index.ingest_weighted(
            "proj start-claaude",
            2_000,
            ShellKind::Zsh,
            None,
            50,
            Some(0),
            &policy,
        );

        let provider = HistoryProvider::new(
            Arc::new(RwLock::new(index)),
            Arc::new(CommandPathCache::default()),
            aliases,
            Arc::new(SpecRegistry::default()),
            Arc::new(CommandHelpCache::default()),
        );
        let rows: Vec<_> = provider
            .complete(&context("proj ", None))
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert!(
            rows.contains(&"proj skillscat".to_owned()),
            "rows: {rows:?}"
        );
        assert!(rows.contains(&"proj aipass".to_owned()), "rows: {rows:?}");
        assert!(
            !rows.contains(&"proj start-claaude".to_owned()),
            "stale function target leaked despite its high frecency: {rows:?}"
        );
    }

    #[test]
    fn fish_modifiers_keep_history_in_the_nested_command_family() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for command in ["not codex review", "not cargo doc"] {
            index.ingest(command, 1_000, ShellKind::Fish, None, Some(0), &policy);
        }
        let provider = provider_with_executables(index, &["codex", "cargo"]);
        let fish = context_for_shell("not cod", ShellKind::Fish);
        let rows: Vec<_> = provider
            .complete(&fish)
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect();
        assert_eq!(rows, ["not codex review"]);

        assert!(!provider.plausible_command(
            &context_for_shell("not codex review", ShellKind::Zsh),
            "not codex review"
        ));
    }
}
