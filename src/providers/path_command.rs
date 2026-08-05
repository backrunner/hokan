use std::sync::Arc;

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, TextEdit,
    },
    platform::CommandPathCache,
    terminal::RiskLevel,
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
        crate::providers::command_position_open(context)
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let mut names: Vec<_> = self.commands.names().collect();
        names.sort_unstable();
        let candidates = names
            .into_iter()
            .filter(|name| {
                crate::completion::match_quality(&context.parsed.current_prefix, name) > 0
            })
            .take(1_000)
            .map(|name| {
                Candidate::new(
                    context.query_id,
                    name,
                    "PATH 中的可执行命令",
                    Some(TextEdit {
                        range: context.parsed.replacement.clone(),
                        replacement: name.to_owned(),
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::Insert,
                    CandidateSource::PathCommand,
                    CandidateKind::Command,
                    Completeness::Runnable,
                    RiskLevel::Unknown,
                    format!("path:{name}"),
                )
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs, os::unix::fs::PermissionsExt, path::PathBuf};

    use super::*;
    use crate::{
        completion::{BufferSnapshot, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

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

    #[test]
    fn fires_only_at_the_open_command_position() {
        let directory = tempfile::tempdir().expect("bin");
        let ls = directory.path().join("ls");
        fs::write(&ls, b"#!/bin/sh\n").expect("fake ls");
        fs::set_permissions(&ls, fs::Permissions::from_mode(0o700)).expect("ls mode");
        let path = OsString::from(directory.path());
        let provider = PathCommandProvider::new(Arc::new(CommandPathCache::from_path(Some(&path))));

        // On the (effective) command word — including right after wrappers
        // and assignments, where the command name is still being chosen.
        for buffer in ["", "l", "ls", "sudo ", "sudo l", "FOO=bar "] {
            assert!(provider.applies(&context(buffer)), "{buffer:?} must fire");
        }
        // At an argument position PATH rows are a flood, not a completion.
        for buffer in ["ls ", "sudo ls ", "FOO=bar ls ", "git checkout "] {
            assert!(
                !provider.applies(&context(buffer)),
                "{buffer:?} must not fire"
            );
        }

        // Command rows still complete at the wrapper slot.
        let output = provider.complete(&context("sudo l"));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "ls")
        );
    }
}
