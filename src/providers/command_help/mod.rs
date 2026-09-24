//! Documented command completion, shared with path and history providers.
mod cache;
mod parsing;
mod probe;
mod scope;
mod validation;

pub use cache::CommandHelpCache;
pub(crate) use parsing::parse_help_output_for_scope;
pub(crate) use scope::dynamic_help_owns_position;
pub(crate) use validation::{
    history_arguments_are_plausible, one_edit_or_adjacent_transposition,
    scoped_history_arguments_are_plausible,
};

use crate::providers::argument_progress;
use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, SlotKind, TextEdit,
    },
    platform::CommandPathCache,
    specs::SpecRegistry,
    terminal::RiskLevel,
};
use probe::help_probe_allowed;
use scope::{HelpLookup, HelpPosition, bare_command_position, lookup_help_scope};
use std::sync::Arc;

const ENTRY_PRIORITY: usize = 200;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandHelp {
    pub flags: Vec<HelpEntry>,
    pub subcommands: Vec<HelpEntry>,
    /// Accepted aliases parsed from command rows. They validate history and
    /// exact input but are not emitted as additional recommendation rows.
    pub subcommand_aliases: Vec<String>,
    /// The root invocation also accepts a free positional argument (for
    /// example Codex/Claude prompts), so an unknown first word is not by
    /// itself proof of an invalid subcommand.
    pub accepts_positionals: bool,
    /// True only when `<command> --help` exposed a parseable, untruncated
    /// Commands section. Man pages and partial parses remain non-exhaustive
    /// because commands such as Git can be extended by aliases or external
    /// helpers.
    pub subcommands_exhaustive: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpEntry {
    pub name: String,
    pub description: String,
    pub takes_value: bool,
}

impl CommandHelp {
    #[must_use]
    pub fn has_subcommands(&self) -> bool {
        !self.subcommands.is_empty()
    }
}

/// Builds editable candidates from the active command's cached documentation.
pub struct CommandHelpProvider {
    commands: Arc<CommandPathCache>,
    cache: Arc<CommandHelpCache>,
}

impl CommandHelpProvider {
    #[must_use]
    pub fn new(
        _specs: Arc<SpecRegistry>,
        commands: Arc<CommandPathCache>,
        cache: Arc<CommandHelpCache>,
    ) -> Self {
        Self { commands, cache }
    }
}

impl CandidateProvider for CommandHelpProvider {
    fn id(&self) -> &'static str {
        "command_help"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        let Some(command) = context.command() else {
            return false;
        };
        // Curated recipes and documented entries are complementary. The
        // ranking layer merges duplicate edits while preserving recipe scores.
        if !crate::providers::effective_command_accepts_external(context) {
            return false;
        }
        let executable = crate::providers::resolved_executable_path(context, &self.commands);
        if executable.is_none() {
            return false;
        }
        let request_missing = help_probe_allowed(command, executable.as_deref());
        if argument_progress(context).is_none() && !bare_command_position(context, command) {
            return false;
        }
        !matches!(
            lookup_help_scope(context, &self.cache, command, executable, request_missing,),
            HelpLookup::None
        )
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Some(command) = context.command() else {
            return ProviderOutput::default();
        };
        let executable = crate::providers::resolved_executable_path(context, &self.commands);
        let HelpLookup::Ready(target) =
            lookup_help_scope(context, &self.cache, command, executable, false)
        else {
            return ProviderOutput::default();
        };
        let help = target.help;
        let position = target.position;
        if let HelpPosition::Values(flag_index) = position {
            return complete_help_values(context, command, &target.scope, &help, flag_index);
        }
        let flags_position = position == HelpPosition::Flags;
        let bare_subcommands = position == HelpPosition::BareSubcommands;
        let entries = if flags_position {
            &help.flags
        } else {
            &help.subcommands
        };
        let query = if bare_subcommands {
            ""
        } else {
            context.parsed.current_prefix.as_str()
        };
        let folded_query = query.to_lowercase();
        let candidates = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.name != query
                    && (query.is_empty() || entry.name.to_lowercase().starts_with(&folded_query))
            })
            .map(|(index, entry)| {
                let replacement = if bare_subcommands {
                    format!("{command} {}", entry.name)
                } else {
                    entry.name.clone()
                };
                let display = crate::parser::apply_edit(
                    &context.buffer.text,
                    context.parsed.replacement.clone(),
                    &replacement,
                )
                .map(|result| result.trim_end().to_owned())
                .unwrap_or_else(|_| format!("{command} {}", entry.name));
                let mut candidate = Candidate::new(
                    context.query_id,
                    display,
                    entry.description.as_str(),
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement,
                        cursor_after: CursorPlacement::End,
                    }),
                    if flags_position {
                        CandidateAction::Insert
                    } else {
                        CandidateAction::InsertAndContinue {
                            next_slot: SlotKind::Path,
                        }
                    },
                    CandidateSource::CommandHelp,
                    CandidateKind::Command,
                    // Man-derived rows are never auto-executed: Enter degrades
                    // to a fill, exactly like an incomplete spec recipe.
                    Completeness::NeedsInput {
                        slot: if flags_position {
                            SlotKind::Value
                        } else {
                            SlotKind::Path
                        },
                    },
                    RiskLevel::Low,
                    if target.scope.is_empty() {
                        format!("help:{command}")
                    } else {
                        format!("help:{command}:{}", target.scope.join(" "))
                    },
                );
                // CLI authors generally put the most useful commands and
                // flags first. Preserve that signal so short, obscure man-page
                // entries cannot outrank the documented common workflow.
                candidate.score.spec_priority =
                    i16::try_from(ENTRY_PRIORITY.saturating_sub(index)).unwrap_or_default();
                candidate
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

fn complete_help_values(
    context: &CompletionContext,
    command: &str,
    scope: &[String],
    help: &CommandHelp,
    flag_index: usize,
) -> ProviderOutput {
    let Some(entry) = help.flags.get(flag_index) else {
        return ProviderOutput::default();
    };
    let choices = documented_value_choices(&entry.description);
    if choices.is_empty() {
        return ProviderOutput::default();
    }

    let prefix = context.parsed.current_prefix.as_str();
    let mut edit_prefix = String::new();
    let mut query = prefix;
    if let Some(rest) = prefix.strip_prefix(&entry.name) {
        if let Some(value) = rest.strip_prefix('=') {
            edit_prefix = format!("{}=", entry.name);
            query = value;
        } else if entry.name.len() == 2 && !rest.is_empty() {
            edit_prefix = entry.name.clone();
            query = rest;
        }
    }
    let folded_query = query.to_ascii_lowercase();
    let candidates = choices
        .into_iter()
        .enumerate()
        .filter(|(_, choice)| {
            !choice.eq_ignore_ascii_case(query)
                && (folded_query.is_empty()
                    || choice.to_ascii_lowercase().starts_with(&folded_query))
        })
        .map(|(index, choice)| {
            let replacement = format!("{edit_prefix}{choice}");
            let display = crate::parser::apply_edit(
                &context.buffer.text,
                context.parsed.replacement.clone(),
                &replacement,
            )
            .map(|result| result.trim_end().to_owned())
            .unwrap_or_else(|_| format!("{command} {replacement}"));
            let scope = if scope.is_empty() {
                String::new()
            } else {
                format!(":{}", scope.join(" "))
            };
            let mut candidate = Candidate::new(
                context.query_id,
                display,
                format!("{} 的文档可选值", entry.name),
                Some(TextEdit {
                    range: context.parsed.replacement.clone(),
                    replacement,
                    cursor_after: CursorPlacement::End,
                }),
                CandidateAction::Insert,
                CandidateSource::CommandHelp,
                CandidateKind::Recipe,
                Completeness::NeedsInput {
                    slot: SlotKind::Value,
                },
                RiskLevel::Low,
                format!("help:{command}{scope}:{}:{choice}", entry.name),
            );
            candidate.score.spec_priority =
                i16::try_from(ENTRY_PRIORITY.saturating_sub(index)).unwrap_or_default();
            candidate
        })
        .collect();
    ProviderOutput {
        candidates,
        diagnostics: Vec::new(),
    }
}

fn documented_value_choices(description: &str) -> Vec<String> {
    let folded = description.to_ascii_lowercase();
    let Some((start, marker_len)) = [
        "valid values are:",
        "possible values:",
        "valid values:",
        "values are:",
        "values:",
    ]
    .iter()
    .filter_map(|marker| folded.find(marker).map(|start| (start, marker.len())))
    .min_by_key(|(start, _)| *start) else {
        return Vec::new();
    };
    let tail = description[start + marker_len..].trim_start();
    let end = tail.find([']', ')', ';']).unwrap_or(tail.len());
    let list = tail[..end].trim();
    if !list.contains([',', '|']) {
        return Vec::new();
    }

    let mut choices = Vec::new();
    for raw in list.split([',', '|']) {
        let choice = raw
            .trim()
            .trim_matches(['`', '\'', '"', '<', '>', '[', ']', '(', ')']);
        if choice.is_empty()
            || choice.len() > 64
            || choice.contains(char::is_whitespace)
            || !choice.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || matches!(character, '-' | '_' | '+' | '.' | '/' | ':')
            })
        {
            continue;
        }
        if !choices.iter().any(|existing: &String| existing == choice) {
            choices.push(choice.to_owned());
        }
    }
    if choices.len() >= 2 {
        choices
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests;
