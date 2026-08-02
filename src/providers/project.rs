use std::sync::Arc;

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderDiagnostic, ProviderOutput,
        TextEdit,
    },
    parser::{QuoteContext, escape_for_shell},
    platform::CommandPathCache,
    project::ProjectCache,
};

pub struct ProjectProvider {
    cache: Arc<ProjectCache>,
    commands: Arc<CommandPathCache>,
}

impl ProjectProvider {
    #[must_use]
    pub fn new(cache: Arc<ProjectCache>, commands: Arc<CommandPathCache>) -> Self {
        Self { cache, commands }
    }
}

impl CandidateProvider for ProjectProvider {
    fn id(&self) -> &'static str {
        "project"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        package_manager(context).is_some_and(|manager| self.commands.contains(manager))
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Some(manager) = package_manager(context) else {
            return ProviderOutput::default();
        };
        let manifest = match self.cache.load_nearest(&context.cwd) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => return ProviderOutput::default(),
            Err(error) => {
                return ProviderOutput {
                    candidates: Vec::new(),
                    diagnostics: vec![ProviderDiagnostic {
                        provider: self.id(),
                        code: "HK-PROJ-001",
                        message: error.to_string(),
                    }],
                };
            }
        };
        let relative = manifest
            .path
            .strip_prefix(context.cwd.as_ref())
            .unwrap_or(&manifest.path)
            .display()
            .to_string();
        let candidates = manifest
            .scripts
            .iter()
            .map(|(name, script)| {
                let replacement = escape_for_shell(name, QuoteContext::Unquoted, context.shell);
                let mut candidate = Candidate::new(
                    context.query_id,
                    format!("{manager} run {name}"),
                    truncate(script, 100),
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement,
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::Insert,
                    CandidateSource::Project,
                    CandidateKind::ProjectScript,
                    Completeness::Runnable,
                    crate::safety::classify_command(script).level,
                    format!("project:{relative}:{name}"),
                );
                candidate.display.annotation = Some(relative.clone());
                candidate.score.cwd_affinity = 100;
                candidate
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

fn package_manager(context: &CompletionContext) -> Option<&str> {
    let words: Vec<_> = context
        .parsed
        .tokens
        .iter()
        .filter(|token| {
            token.kind == crate::parser::TokenKind::Word
                && token.range.start >= context.parsed.active_segment.start
                && token.range.start <= context.buffer.cursor
        })
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    match words.as_slice() {
        [manager, run, ..]
            if matches!(*manager, "pnpm" | "npm" | "yarn" | "bun") && *run == "run" =>
        {
            Some(manager)
        }
        _ => None,
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let sanitized: String = value
        .chars()
        .filter(|character| !character.is_control())
        .take(max_chars)
        .collect();
    if value.chars().count() > max_chars {
        format!("{sanitized}...")
    } else {
        sanitized
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
    fn replaces_only_the_script_token() {
        let directory = tempfile::tempdir().expect("project");
        fs::write(
            directory.path().join("package.json"),
            r#"{"scripts":{"build docs":"vite build"}}"#,
        )
        .expect("manifest");
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        let pnpm = bin.join("pnpm");
        fs::write(&pnpm, b"#!/bin/sh\n").expect("fake pnpm");
        fs::set_permissions(&pnpm, fs::Permissions::from_mode(0o700)).expect("pnpm mode");
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new(
                "pnpm run bu",
                11,
                BufferRevision::new(1),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context");
        let path = OsString::from(bin);
        let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(ProjectProvider::new(
            Arc::new(ProjectCache::default()),
            commands,
        ));
        let output = engine.complete(&context);
        assert_eq!(
            output.candidates[0].edit.as_ref().expect("edit").range,
            9..11
        );
        assert_eq!(
            output.candidates[0]
                .edit
                .as_ref()
                .expect("edit")
                .replacement,
            "'build docs'"
        );
    }
}
