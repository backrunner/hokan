use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use super::scripts::maven_history_arguments_are_plausible;
use super::*;
use crate::providers::command_help::{CommandHelp, HelpEntry};
use crate::{
    completion::{BufferSnapshot, CandidateSource, SyncQuality, rank_and_dedupe},
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
    let buffer = BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
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
    .allow_unknown_cwd_for_tests()
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

mod flow;
mod navigation;
mod scripts;
mod validation;
