use std::sync::{Arc, RwLock};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, TextEdit,
    },
    history::HistoryIndex,
    platform::CommandPathCache,
    specs::SpecRegistry,
};

pub struct HistoryProvider {
    index: Arc<RwLock<HistoryIndex>>,
    commands: Arc<CommandPathCache>,
    specs: Arc<SpecRegistry>,
}

impl HistoryProvider {
    #[must_use]
    pub fn new(
        index: Arc<RwLock<HistoryIndex>>,
        commands: Arc<CommandPathCache>,
        specs: Arc<SpecRegistry>,
    ) -> Self {
        Self {
            index,
            commands,
            specs,
        }
    }
}

impl CandidateProvider for HistoryProvider {
    fn id(&self) -> &'static str {
        "history"
    }

    fn applies(&self, _: &CompletionContext) -> bool {
        true
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Ok(index) = self.index.read() else {
            return ProviderOutput::default();
        };
        let now_ms = crate::history_now_ms();
        let matches = index.search(&context.buffer.text, &context.cwd, now_ms, 50);
        let candidates = matches
            .into_iter()
            .filter(|matched| self.plausible_command(&matched.record.command))
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

impl HistoryProvider {
    /// History rows whose command cannot ever have run — the word is not an
    /// executable on PATH, not a shell builtin or keyword, not spec-covered,
    /// and not an explicit path — are typos and noise; drop them outright.
    /// Anything we cannot classify (unparseable line, opaque substitution)
    /// is kept: filtering must never hide a command we merely fail to
    /// understand.
    fn plausible_command(&self, command: &str) -> bool {
        let Some(word) = crate::safety::effective_command_word(command) else {
            return true;
        };
        word.contains('/')
            || self.commands.contains(&word)
            || self.specs.get(&word).is_some()
            || is_shell_builtin_or_keyword(&word)
    }
}

/// Union of common zsh/bash/fish builtins and reserved words, so history
/// entries like `cd /tmp` or `for f in *; do …` are not mistaken for typos.
fn is_shell_builtin_or_keyword(word: &str) -> bool {
    matches!(
        word,
        "." | ":"
            | "["
            | "[["
            | "alias"
            | "autoload"
            | "bg"
            | "bind"
            | "bindkey"
            | "break"
            | "builtin"
            | "case"
            | "cd"
            | "command"
            | "compdef"
            | "continue"
            | "coproc"
            | "declare"
            | "dirs"
            | "disown"
            | "do"
            | "done"
            | "echo"
            | "elif"
            | "else"
            | "end"
            | "esac"
            | "eval"
            | "exec"
            | "exit"
            | "export"
            | "false"
            | "fc"
            | "fg"
            | "fi"
            | "for"
            | "foreach"
            | "function"
            | "functions"
            | "getopts"
            | "hash"
            | "history"
            | "if"
            | "jobs"
            | "let"
            | "local"
            | "logout"
            | "noglob"
            | "popd"
            | "print"
            | "printf"
            | "pushd"
            | "pwd"
            | "read"
            | "readonly"
            | "rehash"
            | "repeat"
            | "return"
            | "select"
            | "set"
            | "setopt"
            | "shift"
            | "source"
            | "suspend"
            | "test"
            | "then"
            | "time"
            | "times"
            | "trap"
            | "true"
            | "type"
            | "typeset"
            | "ulimit"
            | "umask"
            | "unalias"
            | "unfunction"
            | "unhash"
            | "unset"
            | "unsetopt"
            | "until"
            | "vared"
            | "wait"
            | "whence"
            | "where"
            | "which"
            | "while"
            | "zmodload"
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};

    use super::*;
    use crate::{
        completion::{BufferSnapshot, SyncQuality, rank_and_dedupe},
        history::HistoryPolicy,
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    fn context(text: &str, previous_command: Option<&str>) -> CompletionContext {
        let buffer =
            BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer");
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from("/tmp"),
            buffer,
        )
        .expect("context")
        .with_previous_command(previous_command.map(str::to_owned))
    }

    /// A PATH cache with the executables the fixtures rely on, plus an empty
    /// spec registry (spec coverage is exercised in the filter test below).
    fn provider_with_executables(index: HistoryIndex, names: &[&str]) -> HistoryProvider {
        let directory = tempfile::tempdir().expect("command directory");
        for name in names {
            let path = directory.path().join(name);
            fs::write(&path, b"#!/bin/sh\n").expect("fake command");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("mode");
        }
        let path = std::ffi::OsString::from(directory.path());
        let commands = Arc::new(CommandPathCache::from_path(Some(&path)));
        HistoryProvider::new(
            Arc::new(RwLock::new(index)),
            commands,
            Arc::new(SpecRegistry::default()),
        )
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
            ("for f in *; do git add $f; done", true), // shell keyword
            ("./run.sh --fast", true),                 // explicit path
            ("echo done | gti log", true),             // typo in a later segment: first word rules
        ] {
            index.ingest(command, 1_000, ShellKind::Zsh, None, Some(0), &policy);
            let provider = provider_with_executables(HistoryIndex::default(), &["git"]);
            assert_eq!(
                provider.plausible_command(command),
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
    fn unparseable_or_opaque_history_rows_are_kept() {
        let provider = provider_with_executables(HistoryIndex::default(), &["git"]);
        assert!(provider.plausible_command("echo $(gti status)"));
        assert!(provider.plausible_command("echo 'unterminated"));
        assert!(provider.plausible_command(""));
    }
}
