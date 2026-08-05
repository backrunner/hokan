use std::sync::Arc;

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, SlotKind, TextEdit,
    },
    parser::TokenKind,
    platform::CommandPathCache,
    project::{GitContext, GitRefsCache, GitStatus, GitStatusCache},
    providers::argument_progress,
    terminal::RiskLevel,
};

/// Subcommands whose argument is a ref (branch, remote, or tag) rather
/// than a path — `git add <path>` deliberately stays with file completion.
/// Shared with the filesystem provider, which suppresses its rows at these
/// slots.
pub(crate) const GIT_REF_SUBCOMMANDS: &[&str] = &[
    "checkout", "switch", "merge", "rebase", "log", "diff", "branch", "push", "pull",
];

/// Ranking for ref rows: local branches outrank remote refs and tags; the
/// current branch sinks below everything else — switching to it is a no-op.
const LOCAL_BOOST: i16 = 100;
const REMOTE_BOOST: i16 = 60;
const CURRENT_PENALTY: i16 = -60;
/// Repositories can carry thousands of refs; cap the rows handed to the
/// ranker (prefix filtering happens there anyway).
const MAX_REF_ROWS: usize = 500;

/// State-aware `git` completion: recommends from the repository's actual
/// state instead of a static subcommand list — `git init`/`git clone` outside
/// a repository, commit rows only when there is something to commit, `push`
/// only when the branch is ahead. Past the subcommand, ref-taking
/// subcommands (`git checkout <…>`) complete branch/remote/tag names.
pub struct GitProvider {
    cache: Arc<GitStatusCache>,
    refs: Arc<GitRefsCache>,
    commands: Arc<CommandPathCache>,
}

impl GitProvider {
    #[must_use]
    pub fn new(
        cache: Arc<GitStatusCache>,
        refs: Arc<GitRefsCache>,
        commands: Arc<CommandPathCache>,
    ) -> Self {
        Self {
            cache,
            refs,
            commands,
        }
    }
}

impl CandidateProvider for GitProvider {
    fn id(&self) -> &'static str {
        "git"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        context.command() == Some("git")
            && self.commands.contains("git")
            && (at_git_argument_position(context) || ref_subcommand(context).is_some())
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        if context.command() != Some("git") {
            return ProviderOutput::default();
        }
        if let Some(subcommand) = ref_subcommand(context) {
            return self.ref_candidates(context, subcommand);
        }
        if !at_git_argument_position(context) {
            return ProviderOutput::default();
        }
        let rows: Vec<(&str, &str, RowKind)> = match self.cache.context_for(&context.cwd) {
            GitContext::NotARepository => vec![
                (
                    "git init",
                    "在当前目录初始化新的 Git 仓库",
                    RowKind::Runnable,
                ),
                ("git clone ", "克隆一个远程仓库", RowKind::NeedsValue),
            ],
            GitContext::RepositoryUnknown => vec![
                ("git status", "查看工作区与暂存区状态", RowKind::Runnable),
                ("git add -A", "暂存全部改动", RowKind::Runnable),
                ("git commit -m ", "提交暂存的改动", RowKind::NeedsValue),
                ("git log --oneline -10", "最近十条提交", RowKind::Runnable),
            ],
            GitContext::Repository(status) => repository_rows(&status),
        };
        ProviderOutput {
            candidates: rows
                .into_iter()
                .map(|(line, description, kind)| row_candidate(context, line, description, kind))
                .collect(),
            diagnostics: Vec::new(),
        }
    }
}

impl GitProvider {
    fn ref_candidates(&self, context: &CompletionContext, subcommand: &str) -> ProviderOutput {
        let Some(refs) = self.refs.refs_for(&context.cwd) else {
            return ProviderOutput::default();
        };
        let mut candidates: Vec<Candidate> = Vec::new();
        match subcommand {
            "push" | "pull" => {
                for branch in &refs.remotes {
                    candidates.push(ref_candidate(context, branch, "远程分支", REMOTE_BOOST));
                }
                for name in &refs.remote_names {
                    candidates.push(ref_candidate(context, name, "远程仓库", REMOTE_BOOST));
                }
            }
            "log" | "diff" | "branch" => {
                for local in &refs.locals {
                    let current = refs.current.as_deref() == Some(local);
                    candidates.push(ref_candidate(
                        context,
                        local,
                        if current {
                            "当前分支"
                        } else {
                            "本地分支"
                        },
                        LOCAL_BOOST,
                    ));
                }
                for remote in &refs.remotes {
                    candidates.push(ref_candidate(context, remote, "远程分支", REMOTE_BOOST));
                }
                for tag in &refs.tags {
                    candidates.push(ref_candidate(context, tag, "标签", REMOTE_BOOST));
                }
            }
            // checkout / switch / merge / rebase: locals first, then remotes.
            _ => {
                for local in &refs.locals {
                    let current = refs.current.as_deref() == Some(local);
                    candidates.push(ref_candidate(
                        context,
                        local,
                        if current {
                            "当前分支"
                        } else {
                            "本地分支"
                        },
                        if current {
                            CURRENT_PENALTY
                        } else {
                            LOCAL_BOOST
                        },
                    ));
                }
                for remote in &refs.remotes {
                    candidates.push(ref_candidate(context, remote, "远程分支", REMOTE_BOOST));
                }
            }
        }
        candidates.truncate(MAX_REF_ROWS);
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

/// A ref row replaces only the typed word, never the whole line.
fn ref_candidate(
    context: &CompletionContext,
    name: &str,
    description: &str,
    boost: i16,
) -> Candidate {
    let mut candidate = Candidate::new(
        context.query_id,
        name,
        description,
        Some(TextEdit {
            range: context.parsed.replacement.clone(),
            replacement: name.to_owned(),
            cursor_after: CursorPlacement::End,
        }),
        CandidateAction::Insert,
        CandidateSource::Project,
        CandidateKind::Command,
        Completeness::Runnable,
        RiskLevel::Low,
        format!("git:ref:{name}"),
    );
    candidate.score.spec_priority = boost;
    candidate
}

/// The ref-taking subcommand when the cursor sits at-or-past its first
/// argument (`git checkout <…>`), `None` everywhere else — `git add <path>`
/// keeps file completion, and a dashed active word belongs to flag
/// completion.
fn ref_subcommand(context: &CompletionContext) -> Option<&'static str> {
    if context.parsed.current_prefix.starts_with('-') {
        return None;
    }
    let (words, position) = argument_progress(context)?;
    if position == 0 {
        return None;
    }
    let subcommand = words.get(1).copied()?;
    GIT_REF_SUBCOMMANDS
        .iter()
        .copied()
        .find(|name| *name == subcommand)
}

#[derive(Clone, Copy)]
enum RowKind {
    Runnable,
    NeedsValue,
}

fn repository_rows(status: &GitStatus) -> Vec<(&'static str, &'static str, RowKind)> {
    let mut rows = Vec::new();
    if status.has_changes {
        rows.push(("git status", "查看工作区与暂存区状态", RowKind::Runnable));
        rows.push(("git add -A", "暂存全部改动", RowKind::Runnable));
        rows.push(("git commit -m ", "提交暂存的改动", RowKind::NeedsValue));
        rows.push(("git diff", "查看未暂存的改动", RowKind::Runnable));
        rows.push(("git diff --staged", "查看已暂存的改动", RowKind::Runnable));
        rows.push(("git stash", "临时收起当前改动", RowKind::Runnable));
    }
    if status.ahead > 0 && status.has_upstream {
        rows.push(("git push", "推送本地领先的提交", RowKind::Runnable));
    }
    if status.behind > 0 {
        rows.push(("git pull", "拉取远端新增的提交", RowKind::Runnable));
    }
    if !status.has_changes {
        rows.push(("git log --oneline -10", "最近十条提交", RowKind::Runnable));
        if status.has_upstream && status.behind == 0 {
            rows.push(("git fetch", "同步远端引用", RowKind::Runnable));
        }
        rows.push(("git branch", "查看本地分支", RowKind::Runnable));
    }
    rows
}

fn row_candidate(
    context: &CompletionContext,
    line: &str,
    description: &str,
    kind: RowKind,
) -> Candidate {
    let incomplete = matches!(kind, RowKind::NeedsValue);
    Candidate::new(
        context.query_id,
        line.trim_end(),
        description,
        Some(TextEdit {
            range: command_edit_range(context),
            replacement: line.to_owned(),
            cursor_after: CursorPlacement::End,
        }),
        if incomplete {
            CandidateAction::InsertAndContinue {
                next_slot: SlotKind::Value,
            }
        } else {
            CandidateAction::Insert
        },
        CandidateSource::Project,
        CandidateKind::Command,
        if incomplete {
            Completeness::NeedsInput {
                slot: SlotKind::Value,
            }
        } else {
            Completeness::Runnable
        },
        crate::safety::classify_command(line).level,
        format!("git:{line}"),
    )
}

/// The whole typed line is replaced, like spec recipes: from the command
/// word's start to the cursor.
fn command_edit_range(context: &CompletionContext) -> std::ops::Range<usize> {
    let start = context
        .parsed
        .tokens
        .iter()
        .find(|token| {
            token.kind == TokenKind::Word
                && token.range.start >= context.parsed.active_segment.start
        })
        .map_or(context.buffer.cursor, |token| token.range.start);
    start..context.buffer.cursor
}

/// Only the `git` word itself or its first argument: ref-taking deeper
/// slots (`git checkout <…>`) are handled by `ref_subcommand` instead.
fn at_git_argument_position(context: &CompletionContext) -> bool {
    let words = context
        .parsed
        .tokens
        .iter()
        .filter(|token| {
            token.kind == TokenKind::Word
                && token.range.start >= context.parsed.active_segment.start
                && token.range.start <= context.buffer.cursor
        })
        .count();
    words <= 2
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs, os::unix::fs::PermissionsExt};

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    fn context(directory: &std::path::Path, text: &str) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            directory.to_owned(),
            BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context")
    }

    fn git_on_path(directory: &std::path::Path) -> Arc<CommandPathCache> {
        let bin = directory.join("bin");
        fs::create_dir_all(&bin).expect("bin");
        let git = bin.join("git");
        fs::write(&git, b"#!/bin/sh\n").expect("fake git");
        fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).expect("mode");
        let path = OsString::from(bin);
        Arc::new(CommandPathCache::from_path(Some(&path)))
    }

    fn git_available() -> bool {
        crate::platform::run_bounded(
            "git",
            ["--version"],
            std::time::Duration::from_secs(2),
            1024,
        )
        .is_ok_and(|output| output.status.success())
    }

    fn git(directory: &std::path::Path, args: &[&str]) {
        // `commit.gpgsign=false`: a developer machine with global signing on
        // must not hang the fixture commit on a pinentry prompt.
        let mut command = vec![
            "-C",
            directory.to_str().expect("utf-8 path"),
            "-c",
            "commit.gpgsign=false",
        ];
        command.extend_from_slice(args);
        let output =
            crate::platform::run_bounded("git", command, std::time::Duration::from_secs(10), 1024)
                .expect("git run");
        assert!(output.status.success(), "git {args:?} failed");
    }

    fn primaries(context: &CompletionContext, provider: &GitProvider) -> Vec<String> {
        provider
            .complete(context)
            .candidates
            .iter()
            .map(|candidate| candidate.display.primary.clone())
            .collect()
    }

    #[test]
    fn outside_a_repository_recommends_init_and_clone() {
        let directory = tempfile::tempdir().expect("directory");
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(directory.path()),
        );
        let context = context(directory.path(), "git ");
        let rows = primaries(&context, &provider);
        assert_eq!(rows, ["git init", "git clone"]);
        let init = &provider.complete(&context).candidates[0];
        let edit = init.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 0..4, "bare `git ` replaces the whole line");
        assert_eq!(edit.replacement, "git init");
    }

    #[test]
    fn inside_a_repository_recommends_from_state() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().expect("repo");
        git(root.path(), &["init", "-q", "-b", "main"]);
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );

        // Fresh empty repository, untracked file → commit-oriented rows.
        fs::write(root.path().join("a.txt"), b"a").expect("file");
        let rows = primaries(&context(root.path(), "git "), &provider);
        assert!(rows.contains(&"git status".to_owned()), "rows: {rows:?}");
        assert!(rows.contains(&"git add -A".to_owned()), "rows: {rows:?}");
        assert!(rows.contains(&"git commit -m".to_owned()), "rows: {rows:?}");
        assert!(!rows.contains(&"git push".to_owned()), "rows: {rows:?}");

        // Commit everything: clean, no upstream → no push/pull, log instead.
        git(root.path(), &["add", "-A"]);
        git(
            root.path(),
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ],
        );
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );
        let rows = primaries(&context(root.path(), "git "), &provider);
        assert!(
            rows.contains(&"git log --oneline -10".to_owned()),
            "rows: {rows:?}"
        );
        assert!(rows.contains(&"git branch".to_owned()), "rows: {rows:?}");
        assert!(!rows.contains(&"git push".to_owned()), "rows: {rows:?}");
        assert!(
            !rows.contains(&"git commit -m".to_owned()),
            "rows: {rows:?}"
        );
    }

    #[test]
    fn non_ref_deeper_positions_do_not_fire() {
        let directory = tempfile::tempdir().expect("directory");
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(directory.path()),
        );
        // `git add <path>` keeps file completion: no git rows at all here.
        let context = context(directory.path(), "git add ma");
        assert!(provider.complete(&context).candidates.is_empty());
        assert!(!provider.applies(&context));
    }

    /// A repository on `main` with a `feature/mars` branch, an `origin`
    /// remote-tracking ref, and a `v1` tag.
    fn ref_repository() -> Option<tempfile::TempDir> {
        if !git_available() {
            return None;
        }
        let root = tempfile::tempdir().expect("repo");
        git(root.path(), &["init", "-q", "-b", "main"]);
        fs::write(root.path().join("a.txt"), b"a").expect("file");
        git(root.path(), &["add", "-A"]);
        git(
            root.path(),
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ],
        );
        git(root.path(), &["branch", "feature/mars"]);
        git(root.path(), &["tag", "v1"]);
        git(
            root.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        Some(root)
    }

    #[test]
    fn checkout_completes_refs_and_marks_the_current_branch() {
        let Some(root) = ref_repository() else { return };
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );
        let context = context(root.path(), "git checkout fea");
        assert!(provider.applies(&context));
        let output = provider.complete(&context);
        let rows = primaries(&context, &provider);
        assert!(rows.contains(&"main".to_owned()), "rows: {rows:?}");
        assert!(rows.contains(&"feature/mars".to_owned()), "rows: {rows:?}");
        assert!(rows.contains(&"origin/main".to_owned()), "rows: {rows:?}");
        assert!(!rows.contains(&"v1".to_owned()), "tags stay out: {rows:?}");

        // Locals are listed before remotes.
        let local = rows.iter().position(|row| row == "main").expect("main");
        let remote = rows
            .iter()
            .position(|row| row == "origin/main")
            .expect("origin/main");
        assert!(local < remote, "rows: {rows:?}");

        // The current branch is annotated and demoted below other locals.
        let current = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "main")
            .expect("current branch row");
        assert_eq!(current.display.description, "当前分支");
        let other = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "feature/mars")
            .expect("other branch row");
        assert!(current.score.spec_priority < 0);
        assert!(current.score.spec_priority < other.score.spec_priority);

        // The edit covers only the typed word, not the whole line.
        let edit = other.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 13..16);
        assert_eq!(edit.replacement, "feature/mars");
        assert_eq!(current.completeness, Completeness::Runnable);
    }

    #[test]
    fn checkout_outside_a_repository_stays_silent() {
        let directory = tempfile::tempdir().expect("directory");
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(directory.path()),
        );
        let context = context(directory.path(), "git checkout ma");
        assert!(provider.complete(&context).candidates.is_empty());
    }

    #[test]
    fn push_and_pull_offer_remote_branches_and_remote_names() {
        let Some(root) = ref_repository() else { return };
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );
        let rows = primaries(&context(root.path(), "git push "), &provider);
        assert!(rows.contains(&"origin/main".to_owned()), "rows: {rows:?}");
        assert!(rows.contains(&"origin".to_owned()), "rows: {rows:?}");
        assert!(!rows.contains(&"main".to_owned()), "rows: {rows:?}");
    }

    #[test]
    fn log_diff_and_branch_offer_all_refs() {
        let Some(root) = ref_repository() else { return };
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );
        let rows = primaries(&context(root.path(), "git log "), &provider);
        for expected in ["main", "feature/mars", "origin/main", "v1"] {
            assert!(rows.contains(&expected.to_owned()), "rows: {rows:?}");
        }
    }

    #[test]
    fn ref_rows_mix_with_history_through_the_engine() {
        let Some(root) = ref_repository() else { return };
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(provider);
        let output = engine.complete(&context(root.path(), "git checkout fea"));
        assert_eq!(
            output
                .candidates
                .first()
                .map(|candidate| candidate.display.primary.as_str()),
            Some("feature/mars")
        );
    }

    #[test]
    fn engine_mixes_git_rows_with_history() {
        let directory = tempfile::tempdir().expect("directory");
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(directory.path()),
        );
        let mut engine = CompletionEngine::new(100, 12);
        engine.register(provider);
        let output = engine.complete(&context(directory.path(), "git in"));
        assert_eq!(
            output
                .candidates
                .first()
                .map(|c| c.display.primary.as_str()),
            Some("git init")
        );
    }
}
