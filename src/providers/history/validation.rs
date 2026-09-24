//! Validate executable availability, shell flow, and recorded argument slots.
use super::{
    HistoryProvider,
    flow::{
        HistoryCommandStatus, HistoryDirectoryChange, HistoryFlowState, HistorySegmentLink,
        MAX_HISTORY_FLOW_STATES, command_segment_words, history_directory_change,
        push_history_flow_state, resolve_history_path,
    },
    scripts::{
        manager_command_arguments, manager_subcommand_has_commands,
        maven_history_arguments_are_plausible,
    },
};
use crate::{
    completion::CompletionContext,
    providers::command_help::{
        CommandHelp, HelpEntry, one_edit_or_adjacent_transposition,
        scoped_history_arguments_are_plausible,
    },
};
use std::{os::unix::fs::PermissionsExt as _, sync::Arc};

impl HistoryProvider {
    /// History rows whose command cannot ever have run — the word is not an
    /// executable on PATH, not a shell builtin, alias, or keyword, and not an
    /// explicit path — are typos and noise; drop
    /// them outright. Anything we cannot classify (unparseable line, opaque
    /// substitution) is kept: filtering must never hide a command we merely
    /// fail to understand.
    #[cfg(test)]
    pub(super) fn plausible_command(&self, context: &CompletionContext, command: &str) -> bool {
        let aliases = self.aliases.load(context.shell);
        self.plausible_command_with_aliases(context, command, &aliases)
    }

    #[cfg(test)]
    pub(super) fn plausible_command_with_aliases(
        &self,
        context: &CompletionContext,
        command: &str,
        aliases: &crate::shell::ShellAliases,
    ) -> bool {
        self.plausible_command_with_status(context, command, None, aliases)
    }

    pub(super) fn plausible_record_with_aliases(
        &self,
        context: &CompletionContext,
        record: &crate::history::HistoryRecord,
        aliases: &crate::shell::ShellAliases,
    ) -> bool {
        self.plausible_command_with_status(context, &record.command, record.last_exit_code, aliases)
    }

    pub(super) fn plausible_command_with_status(
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

    pub(super) fn plausible_segment(
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
                let directory = crate::providers::wrapper_working_directory_before(
                    context,
                    &cooked,
                    command_index,
                );
                let executable = crate::providers::resolve_directory(&directory, word);
                crate::platform::is_executable(&executable)
            }
        } else {
            self.commands.contains(word)
        };
        let builtin = crate::providers::is_shell_builtin(context.shell, word);
        let symbol = crate::providers::is_shell_builtin_or_keyword(context.shell, word);
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

    pub(super) fn plausible_executable_arguments(
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

        if let Some(plausible) = crate::providers::python_module::history_python_module_is_plausible(
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

        if let Some(manager) = crate::providers::MANAGERS
            .iter()
            .find(|manager| manager.name == crate::providers::executable_basename(command_word))
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

    pub(super) fn plausible_function_argument(
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

pub(super) fn allows_external_subcommands(command: &str) -> bool {
    matches!(
        command,
        "brew" | "cargo" | "docker" | "gh" | "git" | "kubectl" | "podman"
    )
}
