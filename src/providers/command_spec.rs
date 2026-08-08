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
            (self.specs.get(command).is_some()
                && command_available(context, command, &self.commands))
                || (command == "ifconfig"
                    && !self.commands.contains("ifconfig")
                    && self.commands.contains("ip")
                    && crate::providers::command_resolution_kind(context)
                        != crate::parser::EffectiveCommandKind::Builtin)
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
            if crate::providers::segment_words(context).len() != 1
                || !context.buffer.text[context.parsed.replacement.end..]
                    .trim()
                    .is_empty()
                || context.parsed.tokens.iter().any(|token| {
                    token.kind == crate::parser::TokenKind::Redirect
                        && token.range.start < context.buffer.cursor
                })
            {
                return ProviderOutput::default();
            }
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
        if !command_available(context, &command.name, &self.commands) {
            return ProviderOutput::default();
        }
        let exact_current = effective_text(context).trim() == command_name;
        let mut candidates = Vec::new();
        if exact_current && command.default == "run_current" {
            // The spec default "run_current" no longer executes anything: it is
            // surfaced as an ordinary fill-back candidate for the bare command.
            // Enter always submits the buffer exactly as typed; only Tab
            // performs this edit-back.
            let mut direct = Candidate::new(
                context.query_id,
                command_name,
                command.description.clone(),
                Some(TextEdit {
                    range: active_edit_range(context),
                    replacement: command_name.to_owned(),
                    cursor_after: CursorPlacement::End,
                }),
                CandidateAction::Insert,
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
            if !recipe_is_compatible(context, recipe) {
                continue;
            }
            let effective = effective_text(context);
            let recipe_is_already_present = normalize_command(effective)
                == normalize_command(&recipe.prefix)
                && (recipe.complete
                    || effective
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace));
            if recipe_is_already_present {
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
    let display = crate::parser::apply_edit(&context.buffer.text, range.clone(), replacement)
        .unwrap_or_else(|_| replacement.to_owned());
    let action = next_slot.map_or(CandidateAction::Insert, |next_slot| {
        CandidateAction::InsertAndContinue { next_slot }
    });
    Candidate::new(
        context.query_id,
        display,
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

/// The edit starts at the effective command word, preserving any
/// wrapper/assignment prefix (`FOO=bar ls ` keeps `FOO=bar `).
fn active_edit_range(context: &CompletionContext) -> std::ops::Range<usize> {
    let start = context
        .parsed
        .command_range
        .as_ref()
        .map_or(context.buffer.cursor, |range| range.start);
    let replacement_contains_cursor = context.parsed.replacement.start >= start
        && context.parsed.replacement.start <= context.buffer.cursor
        && context.buffer.cursor <= context.parsed.replacement.end;
    let end = if replacement_contains_cursor {
        context.parsed.replacement.end
    } else {
        context.buffer.cursor
    };
    start..end
}

fn effective_text(context: &CompletionContext) -> &str {
    let start = context
        .parsed
        .command_range
        .as_ref()
        .map_or(context.buffer.cursor, |range| range.start);
    &context.buffer.text[start..context.buffer.cursor]
}

/// Full-line recipes are useful while choosing a recipe, but become harmful
/// once the user has committed an unrelated argument: `ls foo ` must never
/// offer a row that replaces it with `ls -la`. A redirect also makes a
/// whole-command rewrite unsafe because it would silently discard the target.
fn recipe_is_compatible(context: &CompletionContext, recipe: &CompiledRecipe) -> bool {
    let Some(command_range) = context.parsed.command_range.as_ref() else {
        return false;
    };
    if context.parsed.tokens.iter().any(|token| {
        token.kind == crate::parser::TokenKind::Redirect
            && token.range.start >= command_range.start
            && token.range.start < context.buffer.cursor
    }) {
        return false;
    }
    let active_word = context.parsed.replacement.start < context.parsed.replacement.end
        && context.parsed.replacement.start >= command_range.start
        && context.parsed.replacement.start <= context.buffer.cursor
        && context.buffer.cursor <= context.parsed.replacement.end;
    if !active_word
        && context.buffer.text[..context.buffer.cursor]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        return crate::providers::segment_words(context).len() == 1;
    }
    normalize_command(&recipe.prefix).starts_with(&normalize_command(effective_text(context)))
}

fn is_shell_builtin(command: &str) -> bool {
    matches!(command, "cd" | "kill")
}

fn command_available(
    context: &CompletionContext,
    command: &str,
    commands: &CommandPathCache,
) -> bool {
    let path = commands.contains(command);
    let builtin = is_shell_builtin(command);
    match crate::providers::command_resolution_kind(context) {
        crate::parser::EffectiveCommandKind::Shell => path || builtin,
        crate::parser::EffectiveCommandKind::External => path,
        crate::parser::EffectiveCommandKind::ExternalOrBuiltin => path || builtin,
        crate::parser::EffectiveCommandKind::Builtin => builtin,
    }
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

        // The `run_current` spec default duplicates the typed buffer, so it is
        // filtered out: no candidate may rewrite the buffer to itself. The
        // remaining first candidate is an ordinary fill-back (Insert) row.
        for command in ["ls", "df", "lsof", "ifconfig", "ps"] {
            let output = engine.complete(&context(command));
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == command),
                "buffer-identical bare command was listed for {command}"
            );
            let first = &output.candidates[0];
            assert!(matches!(first.action, CandidateAction::Insert));
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

    #[test]
    fn ifconfig_platform_replacement_only_applies_to_the_bare_command() {
        let directory = tempfile::tempdir().expect("command directory");
        let ip = directory.path().join("ip");
        fs::write(&ip, b"#!/bin/sh\n").expect("fake ip");
        fs::set_permissions(&ip, fs::Permissions::from_mode(0o700)).expect("ip mode");
        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
            directory.path(),
        ))));
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(CommandSpecProvider::new(
            Arc::new(SpecRegistry::load(None)),
            commands,
        ));

        let bare = engine.complete(&context("ifconfig "));
        assert!(
            bare.candidates
                .iter()
                .any(|candidate| candidate.display.primary == "ip addr")
        );
        for text in ["ifconfig en0", "ifconfig -a", "ifconfig > out"] {
            assert!(
                engine.complete(&context(text)).candidates.is_empty(),
                "platform replacement leaked for {text:?}"
            );
        }
    }

    #[test]
    fn recipe_fill_preserves_assignment_and_wrapper_prefixes() {
        let specs = Arc::new(SpecRegistry::load(None));
        let directory = tempfile::tempdir().expect("command directory");
        let ls = directory.path().join("ls");
        fs::write(&ls, b"#!/bin/sh\n").expect("fake ls");
        fs::set_permissions(&ls, fs::Permissions::from_mode(0o700)).expect("ls mode");
        let path = OsString::from(directory.path());
        let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(CommandSpecProvider::new(specs, commands));

        // `FOO=bar ls ` hits the ls spec through the assignment prefix, and
        // the fill starts at the effective command word: the assignment
        // survives.
        let output = engine.complete(&context("FOO=bar ls "));
        let recipe = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "FOO=bar ls -la")
            .expect("ls -la recipe");
        let edit = recipe.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 8..11);
        let buffer = "FOO=bar ls ";
        let filled = format!("{}{}", &buffer[..edit.range.start], edit.replacement);
        assert_eq!(filled, "FOO=bar ls -la");

        // Same through a wrapper: `sudo ls ` fills `sudo ls -la `.
        let output = engine.complete(&context("sudo ls "));
        let recipe = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "sudo ls -la")
            .expect("ls -la recipe behind sudo");
        assert_eq!(recipe.edit.as_ref().expect("edit").range, 5..8);
    }

    #[test]
    fn shell_script_recipe_uses_a_file_slot_not_an_executable_slot() {
        let specs = Arc::new(SpecRegistry::load(None));
        let directory = tempfile::tempdir().expect("command directory");
        let bash = directory.path().join("bash");
        fs::write(&bash, b"#!/bin/sh\n").expect("fake bash");
        fs::set_permissions(&bash, fs::Permissions::from_mode(0o700)).expect("bash mode");
        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
            directory.path(),
        ))));
        let provider = CommandSpecProvider::new(specs, commands);

        let script = provider
            .complete(&context("bash"))
            .candidates
            .into_iter()
            .find(|candidate| candidate.display.primary == "bash ")
            .expect("bash script recipe");
        assert!(matches!(
            script.action,
            CandidateAction::InsertAndContinue {
                next_slot: crate::completion::SlotKind::File
            }
        ));
    }

    #[test]
    fn specs_respect_the_wrapper_command_resolution_domain() {
        let specs = Arc::new(SpecRegistry::load(None));
        let directory = tempfile::tempdir().expect("command directory");
        let ls = directory.path().join("ls");
        fs::write(&ls, b"#!/bin/sh\n").expect("fake ls");
        fs::set_permissions(&ls, fs::Permissions::from_mode(0o700)).expect("ls mode");
        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(
            directory.path(),
        ))));
        let provider = CommandSpecProvider::new(specs, commands);

        for text in ["builtin ls", "env cd", "sudo cd"] {
            assert!(
                !provider.applies(&context(text)),
                "invalid spec for {text:?}"
            );
            assert!(provider.complete(&context(text)).candidates.is_empty());
        }
        for text in ["sudo ls", "builtin cd", "command cd"] {
            assert!(
                provider.applies(&context(text)),
                "valid spec missing for {text:?}"
            );
            let candidates = provider.complete(&context(text)).candidates;
            assert!(!candidates.is_empty(), "valid recipes missing for {text:?}");
            if text.ends_with("cd") {
                assert!(matches!(
                    candidates[0].action,
                    CandidateAction::InsertAndContinue {
                        next_slot: crate::completion::SlotKind::Directory
                    }
                ));
            }
        }

        assert!(
            provider
                .complete(&context("builtin cd "))
                .candidates
                .is_empty(),
            "the directory recipe must not repeat after entering its slot"
        );
    }

    #[test]
    fn recipes_stop_after_committed_arguments_and_preserve_midline_suffixes() {
        let specs = Arc::new(SpecRegistry::load(None));
        let directory = tempfile::tempdir().expect("command directory");
        let ls = directory.path().join("ls");
        fs::write(&ls, b"#!/bin/sh\n").expect("fake ls");
        fs::set_permissions(&ls, fs::Permissions::from_mode(0o700)).expect("ls mode");
        let path = OsString::from(directory.path());
        let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(CommandSpecProvider::new(specs, commands));

        for text in ["ls target ", "ls -la ", "ls > output "] {
            let output = engine.complete(&context(text));
            assert!(
                output.candidates.is_empty(),
                "committed input must not be overwritten for {text:?}: {:?}",
                output
                    .candidates
                    .iter()
                    .map(|candidate| candidate.display.primary.as_str())
                    .collect::<Vec<_>>()
            );
        }

        let text = "ls -l target";
        let midline = CompletionContext::new(
            QueryId::new(2),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            BufferSnapshot::new(text, 5, BufferRevision::new(2), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context");
        let output = engine.complete(&midline);
        let candidate = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "ls -la target")
            .expect("compatible recipe");
        let edit = candidate.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 0..5);
        assert_eq!(
            crate::parser::apply_edit(text, edit.range.clone(), &edit.replacement)
                .expect("apply edit"),
            "ls -la target"
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
