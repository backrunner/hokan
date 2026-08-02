use std::sync::Arc;

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, TextEdit,
    },
    platform::CommandPathCache,
    specs::{CompiledCommand, CompiledRecipe, SpecRegistry},
};

pub struct CommandSpecProvider {
    specs: Arc<SpecRegistry>,
    commands: Arc<CommandPathCache>,
}

impl CommandSpecProvider {
    #[must_use]
    pub fn new(specs: Arc<SpecRegistry>, commands: Arc<CommandPathCache>) -> Self {
        Self { specs, commands }
    }
}

impl CandidateProvider for CommandSpecProvider {
    fn id(&self) -> &'static str {
        "command_spec"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        context.command().is_some_and(|command| {
            self.specs.get(command).is_some()
                || (command == "ifconfig"
                    && !self.commands.contains("ifconfig")
                    && self.commands.contains("ip"))
        })
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Some(command_name) = context.command() else {
            return ProviderOutput::default();
        };
        if command_name == "ifconfig"
            && !self.commands.contains("ifconfig")
            && self.commands.contains("ip")
        {
            return ProviderOutput {
                candidates: vec![replacement_candidate(
                    context,
                    "ip addr",
                    "Linux 上使用 ip 查看接口与地址",
                    crate::terminal::RiskLevel::ReadOnly,
                    "linux.ip-alternative",
                    true,
                    None,
                )],
                diagnostics: Vec::new(),
            };
        }
        let Some(command) = self.specs.get(command_name) else {
            return ProviderOutput::default();
        };
        if !self.commands.contains(&command.name) && !is_shell_builtin(&command.name) {
            return ProviderOutput::default();
        }
        let exact_current = active_text(context).trim() == command_name;
        let mut candidates = Vec::new();
        if exact_current && command.default == "run_current" {
            let mut direct = Candidate::new(
                context.query_id,
                command_name,
                format!("直接运行：{}", command.description),
                None,
                CandidateAction::RunCurrent {
                    expected_revision: context.buffer.revision,
                    expected_hash: context.buffer.hash,
                },
                CandidateSource::CommandSpec,
                CandidateKind::Command,
                Completeness::Runnable,
                command.risk,
                format!("{}:default", command.id),
            );
            direct.score.spec_priority = 200;
            candidates.push(direct);
        }
        for recipe in ordered_recipes(command) {
            if normalize_command(active_text(context)) == normalize_command(&recipe.prefix) {
                continue;
            }
            let mut candidate = recipe_candidate(context, command, recipe);
            if command.default == format!("recipe:{}", recipe.id) {
                candidate.score.spec_priority = 150;
            }
            candidates.push(candidate);
        }
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn ordered_recipes(command: &CompiledCommand) -> Vec<&CompiledRecipe> {
    let default = command.default.strip_prefix("recipe:");
    let mut recipes: Vec<_> = command.recipes.iter().collect();
    recipes.sort_by_key(|recipe| {
        if Some(recipe.id.as_str()) == default {
            0
        } else {
            1
        }
    });
    recipes
}

fn recipe_candidate(
    context: &CompletionContext,
    command: &CompiledCommand,
    recipe: &CompiledRecipe,
) -> Candidate {
    replacement_candidate(
        context,
        &recipe.prefix,
        &recipe.description,
        recipe.risk,
        &format!("{}:{}", command.id, recipe.id),
        recipe.complete,
        recipe.next_slot,
    )
}

#[allow(clippy::too_many_arguments)]
fn replacement_candidate(
    context: &CompletionContext,
    replacement: &str,
    description: &str,
    risk: crate::terminal::RiskLevel,
    provenance: &str,
    complete: bool,
    next_slot: Option<crate::completion::SlotKind>,
) -> Candidate {
    let range = active_edit_range(context);
    let action = next_slot.map_or(CandidateAction::Insert, |next_slot| {
        CandidateAction::InsertAndContinue { next_slot }
    });
    Candidate::new(
        context.query_id,
        replacement,
        description,
        Some(TextEdit {
            range,
            replacement: replacement.to_owned(),
            cursor_after: CursorPlacement::End,
        }),
        action,
        CandidateSource::CommandSpec,
        CandidateKind::Recipe,
        if complete {
            Completeness::Runnable
        } else {
            Completeness::NeedsInput {
                slot: next_slot.unwrap_or(crate::completion::SlotKind::Value),
            }
        },
        risk,
        provenance,
    )
}

fn active_edit_range(context: &CompletionContext) -> std::ops::Range<usize> {
    let start = context
        .parsed
        .tokens
        .iter()
        .find(|token| {
            token.kind == crate::parser::TokenKind::Word
                && token.range.start >= context.parsed.active_segment.start
        })
        .map_or(context.buffer.cursor, |token| token.range.start);
    start..context.buffer.cursor
}

fn active_text(context: &CompletionContext) -> &str {
    &context.buffer.text[context.parsed.active_segment.start..context.buffer.cursor]
}

fn is_shell_builtin(command: &str) -> bool {
    matches!(command, "cd")
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs, os::unix::fs::PermissionsExt, path::PathBuf};

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    #[test]
    fn ls_direct_is_first_and_tar_requires_input() {
        let specs = Arc::new(SpecRegistry::load(None));
        let directory = tempfile::tempdir().expect("command directory");
        for command in ["ls", "df", "tar", "lsof", "ifconfig", "ps", "kill"] {
            let path = directory.path().join(command);
            fs::write(&path, b"#!/bin/sh\n").expect("fake command");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("command mode");
        }
        let path = OsString::from(directory.path());
        let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(CommandSpecProvider::new(specs, commands));

        for command in ["ls", "df", "lsof", "ifconfig", "ps"] {
            let output = engine.complete(&context(command));
            assert!(matches!(
                output.candidates[0].action,
                CandidateAction::RunCurrent { .. }
            ));
        }

        let tar = context("tar");
        let output = engine.complete(&tar);
        assert_eq!(output.candidates[0].display.primary, "tar -czf ");
        assert!(matches!(
            output.candidates[0].completeness,
            Completeness::NeedsInput { .. }
        ));

        let kill = engine.complete(&context("kill"));
        assert_eq!(kill.candidates[0].display.primary, "kill -TERM ");
        assert!(matches!(
            kill.candidates[0].completeness,
            Completeness::NeedsInput {
                slot: crate::completion::SlotKind::Process
            }
        ));

        let existing_recipe = engine.complete(&context("ls -la"));
        assert!(
            !existing_recipe
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "ls -la")
        );
    }

    fn context(text: &str) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context")
    }
}
