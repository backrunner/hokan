use std::{path::PathBuf, sync::Arc};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, SlotKind, TextEdit,
    },
    platform::CommandPathCache,
    project::{GitContext, GitRefsCache, GitStatus, GitStatusCache},
    providers::argument_progress,
    terminal::RiskLevel,
};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefSlotKind {
    RemoteNames,
    Locals,
    LocalsAndRemotes,
    LocalsAndTags,
    AllRefs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RefSlot {
    kind: RefSlotKind,
    edit_prefix: String,
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
        if context.command() != Some("git")
            || !crate::providers::effective_command_accepts_external(context)
            || !self.commands.contains("git")
        {
            return false;
        }
        let words = crate::providers::segment_words(context);
        git_context_supported(&words)
            && (at_git_argument_position(context) || ref_slot(context).is_some())
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        if context.command() != Some("git")
            || !crate::providers::effective_command_accepts_external(context)
        {
            return ProviderOutput::default();
        }
        let words = crate::providers::segment_words(context);
        let directory = git_working_directory(context, &words);
        if let Some(slot) = ref_slot(context) {
            return self.ref_candidates(context, slot, &directory);
        }
        if !at_git_argument_position(context) {
            return ProviderOutput::default();
        }
        let rows: Vec<(&str, &str, RowKind)> = match self.cache.context_for(&directory) {
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

fn git_context_supported(words: &[&str]) -> bool {
    let mut index = 1;
    while let Some(word) = words.get(index).copied() {
        let flag = word.split_once('=').map_or(word, |(flag, _)| flag);
        if matches!(flag, "--git-dir" | "--work-tree" | "--namespace") || word == "--bare" {
            return false;
        }
        if word == "--" {
            return true;
        }
        if git_global_option_has_attached_value(word) || git_global_flag(word) {
            index += 1;
        } else if git_global_value_flag(word) {
            index += 2;
        } else {
            return !word.starts_with('-');
        }
    }
    true
}

impl GitProvider {
    fn ref_candidates(
        &self,
        context: &CompletionContext,
        slot: RefSlot,
        directory: &std::path::Path,
    ) -> ProviderOutput {
        let Some(refs) = self.refs.refs_for(directory) else {
            return ProviderOutput::default();
        };
        let mut candidates: Vec<Candidate> = Vec::new();
        match slot.kind {
            RefSlotKind::RemoteNames => {
                for name in &refs.remote_names {
                    candidates.push(ref_candidate(
                        context,
                        name,
                        "远程仓库",
                        REMOTE_BOOST,
                        &slot.edit_prefix,
                    ));
                }
            }
            RefSlotKind::Locals => {
                for local in &refs.locals {
                    let current = refs.current.as_deref() == Some(local);
                    if current {
                        continue;
                    }
                    candidates.push(ref_candidate(
                        context,
                        local,
                        "本地分支",
                        LOCAL_BOOST,
                        &slot.edit_prefix,
                    ));
                }
            }
            RefSlotKind::LocalsAndRemotes => {
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
                        &slot.edit_prefix,
                    ));
                }
                for remote in &refs.remotes {
                    candidates.push(ref_candidate(
                        context,
                        remote,
                        "远程分支",
                        REMOTE_BOOST,
                        &slot.edit_prefix,
                    ));
                }
            }
            RefSlotKind::LocalsAndTags => {
                for local in &refs.locals {
                    candidates.push(ref_candidate(
                        context,
                        local,
                        "本地分支",
                        LOCAL_BOOST,
                        &slot.edit_prefix,
                    ));
                }
                for tag in &refs.tags {
                    candidates.push(ref_candidate(
                        context,
                        tag,
                        "标签",
                        REMOTE_BOOST,
                        &slot.edit_prefix,
                    ));
                }
            }
            RefSlotKind::AllRefs => {
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
                        &slot.edit_prefix,
                    ));
                }
                for remote in &refs.remotes {
                    candidates.push(ref_candidate(
                        context,
                        remote,
                        "远程分支",
                        REMOTE_BOOST,
                        &slot.edit_prefix,
                    ));
                }
                for tag in &refs.tags {
                    candidates.push(ref_candidate(
                        context,
                        tag,
                        "标签",
                        REMOTE_BOOST,
                        &slot.edit_prefix,
                    ));
                }
            }
        }
        preselect_ref_candidates(context, &mut candidates);
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

/// Apply the provider cap after query filtering. Large repositories can have
/// thousands of refs; truncating the unfiltered local-branch list would hide
/// a matching remote or tag merely because it was enumerated later.
fn preselect_ref_candidates(context: &CompletionContext, candidates: &mut Vec<Candidate>) {
    let query = context.parsed.current_prefix.as_str();
    if !query.is_empty() {
        candidates.sort_by(|left, right| {
            ref_candidate_match(query, right).cmp(&ref_candidate_match(query, left))
        });
    }
    candidates.truncate(MAX_REF_ROWS);
}

fn ref_candidate_match(query: &str, candidate: &Candidate) -> i16 {
    let replacement = candidate
        .edit
        .as_ref()
        .map_or(candidate.display.primary.as_str(), |edit| {
            edit.replacement.as_str()
        });
    crate::completion::match_quality(query, replacement).max(crate::completion::match_quality(
        query,
        &candidate.display.primary,
    ))
}

pub(crate) fn git_working_directory(context: &CompletionContext, words: &[&str]) -> PathBuf {
    let mut directory = crate::providers::invocation_working_directory(context);
    let mut index = 1;
    while let Some(word) = words.get(index).copied() {
        let value = if word == "-C" {
            let Some(value) = words.get(index + 1).copied() else {
                break;
            };
            index += 2;
            Some(value)
        } else if word.len() > 2 && word.starts_with("-C") {
            index += 1;
            Some(&word[2..])
        } else if git_global_option_has_attached_value(word) || git_global_flag(word) {
            index += 1;
            None
        } else if git_global_value_flag(word) {
            index += 2;
            None
        } else {
            break;
        };
        if let Some(value) = value {
            directory = crate::providers::resolve_directory(&directory, value);
        }
    }
    directory
}

/// A ref row replaces only the typed word, never the whole line.
fn ref_candidate(
    context: &CompletionContext,
    name: &str,
    description: &str,
    boost: i16,
    edit_prefix: &str,
) -> Candidate {
    let mut candidate = Candidate::new(
        context.query_id,
        name,
        description,
        Some(TextEdit {
            range: context.parsed.replacement.clone(),
            replacement: format!("{edit_prefix}{name}"),
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

fn ref_slot(context: &CompletionContext) -> Option<RefSlot> {
    let (words, position) = argument_progress(context)?;
    if context.parsed.current_prefix.starts_with('-') {
        let (_, subcommand) = git_subcommand(&words)?;
        let (flag, _) = context.parsed.current_prefix.split_once('=')?;
        let GitValueKind::Ref(kind) = git_value_flag_kind(subcommand, flag)? else {
            return None;
        };
        return Some(RefSlot {
            kind,
            edit_prefix: format!("{flag}="),
        });
    }
    if new_branch_slot(&words, position) {
        return None;
    }
    ref_slot_kind(&words, position).map(|kind| RefSlot {
        kind,
        edit_prefix: String::new(),
    })
}

/// The ref-taking subcommand at the active slot, if any. A `--` in the words
/// before the active slot ends ref territory: what follows is a pathspec.
/// Shared with the filesystem provider, which suppresses its rows at ref
/// slots and resumes them after `--`.
pub(crate) fn ref_slot_subcommand<'a>(words: &'a [&'a str], position: usize) -> Option<&'a str> {
    ref_slot_kind(words, position)?;
    git_subcommand(words).map(|(_, subcommand)| subcommand)
}

fn ref_slot_kind(words: &[&str], position: usize) -> Option<RefSlotKind> {
    let (subcommand_index, subcommand) = git_subcommand(words)?;
    if subcommand_index > position {
        return None;
    }
    let before = words
        .get(subcommand_index + 1..=position)
        .unwrap_or_default();
    if before.contains(&"--") {
        return None;
    }
    let arguments = scan_ref_arguments(subcommand, before)?;
    if let Some(kind) = arguments.pending_ref {
        return Some(kind);
    }
    let positional = arguments.positionals.len();
    match subcommand {
        "checkout" | "switch" if positional == 0 => Some(RefSlotKind::LocalsAndRemotes),
        "merge" | "log" | "diff" | "show" | "cherry-pick" | "revert" => Some(RefSlotKind::AllRefs),
        "rebase" if positional < 2 => Some(RefSlotKind::AllRefs),
        "push" if positional == 0 => Some(RefSlotKind::RemoteNames),
        "push" => Some(RefSlotKind::LocalsAndTags),
        "pull" if positional == 0 => Some(RefSlotKind::RemoteNames),
        "pull" => Some(RefSlotKind::AllRefs),
        "branch" if before.iter().any(|word| matches!(*word, "-d" | "-D")) => {
            Some(RefSlotKind::Locals)
        }
        "branch" if branch_rename_or_copy(before) => None,
        "branch" if positional == 1 => Some(RefSlotKind::AllRefs),
        "reset" if positional == 0 => Some(RefSlotKind::AllRefs),
        _ => None,
    }
}

struct RefArguments<'a> {
    positionals: Vec<&'a str>,
    pending_ref: Option<RefSlotKind>,
}

fn scan_ref_arguments<'a>(subcommand: &str, before: &'a [&'a str]) -> Option<RefArguments<'a>> {
    let mut positional = Vec::new();
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if word == "--" {
            return None;
        }
        if let Some((flag, _)) = word.split_once('=')
            && git_value_flag_kind(subcommand, flag).is_some()
        {
            index += 1;
            continue;
        }
        if let Some(kind) = git_value_flag_kind(subcommand, word) {
            if index + 1 >= before.len() {
                return match kind {
                    GitValueKind::Ref(kind) => Some(RefArguments {
                        positionals: positional,
                        pending_ref: Some(kind),
                    }),
                    GitValueKind::Other => None,
                };
            }
            index += 2;
        } else if word.starts_with('-') {
            index += 1;
        } else {
            positional.push(word);
            index += 1;
        }
    }
    Some(RefArguments {
        positionals: positional,
        pending_ref: None,
    })
}

#[derive(Clone, Copy)]
enum GitValueKind {
    Ref(RefSlotKind),
    Other,
}

fn git_value_flag_kind(subcommand: &str, word: &str) -> Option<GitValueKind> {
    match subcommand {
        "checkout" | "switch" => matches!(
            word,
            "--conflict" | "--pathspec-from-file" | "-b" | "-B" | "-c" | "-C" | "--orphan"
        )
        .then_some(GitValueKind::Other),
        "merge" => matches!(
            word,
            "-m" | "--message" | "-s" | "--strategy" | "-X" | "--strategy-option"
        )
        .then_some(GitValueKind::Other),
        "rebase" if word == "--onto" => Some(GitValueKind::Ref(RefSlotKind::AllRefs)),
        "rebase" => matches!(
            word,
            "--exec" | "-s" | "--strategy" | "-X" | "--strategy-option"
        )
        .then_some(GitValueKind::Other),
        "log" | "diff" => matches!(
            word,
            "-n" | "--max-count"
                | "--skip"
                | "--since"
                | "--until"
                | "--author"
                | "--grep"
                | "--format"
                | "--pretty"
                | "--output"
        )
        .then_some(GitValueKind::Other),
        "push" => matches!(
            word,
            "--repo" | "--receive-pack" | "--exec" | "-o" | "--push-option"
        )
        .then_some(GitValueKind::Other),
        "pull" => matches!(
            word,
            "-s" | "--strategy"
                | "-X"
                | "--strategy-option"
                | "--depth"
                | "--shallow-since"
                | "--upload-pack"
                | "--server-option"
        )
        .then_some(GitValueKind::Other),
        "branch"
            if matches!(
                word,
                "--contains"
                    | "--no-contains"
                    | "--merged"
                    | "--no-merged"
                    | "--points-at"
                    | "-u"
                    | "--set-upstream-to"
            ) =>
        {
            Some(GitValueKind::Ref(RefSlotKind::AllRefs))
        }
        "branch" => matches!(word, "--sort" | "--format").then_some(GitValueKind::Other),
        "restore" if matches!(word, "-s" | "--source") => {
            Some(GitValueKind::Ref(RefSlotKind::AllRefs))
        }
        "restore" => {
            matches!(word, "--conflict" | "--pathspec-from-file").then_some(GitValueKind::Other)
        }
        "reset" => (word == "--pathspec-from-file").then_some(GitValueKind::Other),
        _ => None,
    }
}

fn branch_rename_or_copy(words: &[&str]) -> bool {
    words
        .iter()
        .any(|word| matches!(*word, "-m" | "-M" | "--move" | "-c" | "-C" | "--copy"))
}

/// The word before the active slot is `checkout -b` / `switch -c`: the slot
/// takes a NEW branch name (a value), so neither ref rows nor file rows
/// belong there.
pub(crate) fn new_branch_slot(words: &[&str], position: usize) -> bool {
    let flag = words.get(position).copied().unwrap_or_default();
    let Some((subcommand_index, subcommand)) = git_subcommand(words) else {
        return false;
    };
    if matches!(
        (subcommand, flag),
        ("checkout", "-b" | "-B" | "--orphan") | ("switch", "-c" | "-C" | "--orphan")
    ) {
        return true;
    }
    if subcommand != "branch" {
        return false;
    }
    let before = words
        .get(subcommand_index + 1..=position)
        .unwrap_or_default();
    let Some(arguments) = scan_ref_arguments(subcommand, before) else {
        return true;
    };
    if arguments.pending_ref.is_some() {
        return false;
    }
    branch_rename_or_copy(before)
        || (arguments.positionals.is_empty()
            && !before.iter().any(|word| matches!(*word, "-d" | "-D")))
}

pub(crate) fn ref_subcommand_accepts_paths(words: &[&str]) -> bool {
    git_subcommand(words).is_some_and(|(_, subcommand)| {
        matches!(
            subcommand,
            "checkout" | "restore" | "reset" | "log" | "diff"
        )
    })
}

pub(crate) fn path_slot_after_ref(words: &[&str], position: usize) -> bool {
    let Some((subcommand_index, subcommand)) = git_subcommand(words) else {
        return false;
    };
    let before = words
        .get(subcommand_index + 1..=position)
        .unwrap_or_default();
    let Some(arguments) = scan_ref_arguments(subcommand, before) else {
        return false;
    };
    if arguments.pending_ref.is_some() {
        return false;
    }
    match subcommand {
        "checkout" | "reset" => !arguments.positionals.is_empty(),
        "restore" => true,
        _ => false,
    }
}

/// First non-global-option word of a git invocation. Global options may sit
/// before the subcommand (`git -C repo --no-pager status`). Incomplete value
/// options return `None`, keeping completion on their value slot.
pub(crate) fn git_subcommand<'a>(words: &'a [&'a str]) -> Option<(usize, &'a str)> {
    let mut index = 1;
    while let Some(word) = words.get(index).copied() {
        if word == "--" {
            index += 1;
            break;
        }
        if git_global_option_has_attached_value(word) || git_global_flag(word) {
            index += 1;
            continue;
        }
        if git_global_value_flag(word) {
            if index + 1 >= words.len() {
                return None;
            }
            index += 2;
            continue;
        }
        if word.starts_with('-') {
            return None;
        }
        break;
    }
    words.get(index).copied().map(|command| (index, command))
}

fn git_global_value_flag(word: &str) -> bool {
    matches!(
        word,
        "-C" | "-c"
            | "--git-dir"
            | "--work-tree"
            | "--namespace"
            | "--super-prefix"
            | "--exec-path"
            | "--config-env"
    )
}

fn git_global_option_has_attached_value(word: &str) -> bool {
    word.split_once('=')
        .is_some_and(|(flag, _)| git_global_value_flag(flag))
        || (word.len() > 2 && (word.starts_with("-C") || word.starts_with("-c")))
}

fn git_global_flag(word: &str) -> bool {
    matches!(
        word,
        "--paginate"
            | "-P"
            | "--no-pager"
            | "--bare"
            | "--no-replace-objects"
            | "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "--no-optional-locks"
    )
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
    let (range, replacement) = if crate::providers::argument_progress(context).is_none() {
        (context.parsed.replacement.clone(), line.to_owned())
    } else {
        (
            context.parsed.replacement.clone(),
            line.strip_prefix("git ").unwrap_or(line).to_owned(),
        )
    };
    Candidate::new(
        context.query_id,
        line.trim_end(),
        description,
        Some(TextEdit {
            range,
            replacement,
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

/// Only the `git` word itself or its first argument: ref-taking deeper
/// slots (`git checkout <…>`) are handled by `ref_subcommand` instead.
/// Measured from the effective command, so `sudo git st` still fires.
fn at_git_argument_position(context: &CompletionContext) -> bool {
    match argument_progress(context) {
        // Still on the effective command word itself.
        None => crate::providers::command_position_open(context),
        Some((words, position)) => git_subcommand_slot(&words, position),
    }
}

fn git_subcommand_slot(words: &[&str], position: usize) -> bool {
    let before = words.get(1..=position).unwrap_or_default();
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if word == "--" {
            index += 1;
            break;
        }
        if git_global_option_has_attached_value(word) || git_global_flag(word) {
            index += 1;
            continue;
        }
        if git_global_value_flag(word) {
            if index + 1 >= before.len() {
                return false;
            }
            index += 2;
            continue;
        }
        return false;
    }
    index == before.len()
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
        assert_eq!(edit.range, 4..4, "bare `git ` fills the subcommand slot");
        assert_eq!(edit.replacement, "init");
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
    fn global_c_option_uses_the_selected_repository() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().expect("root");
        let repository = root.path().join("repo");
        fs::create_dir(&repository).expect("repository");
        git(&repository, &["init", "-q", "-b", "main"]);
        fs::write(repository.join("untracked.txt"), b"x").expect("file");
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );

        let rows = primaries(&context(root.path(), "git -C repo "), &provider);
        assert!(rows.contains(&"git status".to_owned()), "rows: {rows:?}");
        assert!(!rows.contains(&"git init".to_owned()), "rows: {rows:?}");
    }

    #[test]
    fn unmodeled_repository_selectors_suppress_state_rows() {
        assert!(git_context_supported(&["git", "-C", "repo"]));
        assert!(git_context_supported(&["git", "-c", "color.ui=false"]));
        for words in [
            &["git", "--bare"][..],
            &["git", "--git-dir", "repo/.git"][..],
            &["git", "--git-dir=repo/.git"][..],
            &["git", "--work-tree", "repo"][..],
            &["git", "--namespace=ci"][..],
        ] {
            assert!(!git_context_supported(words), "unsupported: {words:?}");
        }
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
        let add_context = context(directory.path(), "git add ma");
        assert!(provider.complete(&add_context).candidates.is_empty());
        assert!(!provider.applies(&add_context));
        let builtin_context = context(directory.path(), "builtin git ");
        assert!(provider.complete(&builtin_context).candidates.is_empty());
        assert!(!provider.applies(&builtin_context));
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
    fn push_and_pull_distinguish_remote_and_refspec_slots() {
        let Some(root) = ref_repository() else { return };
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );
        let rows = primaries(&context(root.path(), "git push "), &provider);
        assert_eq!(rows, vec!["origin".to_owned()]);

        let rows = primaries(&context(root.path(), "git push origin "), &provider);
        assert!(rows.contains(&"main".to_owned()), "rows: {rows:?}");
        assert!(rows.contains(&"v1".to_owned()), "rows: {rows:?}");
        assert!(!rows.contains(&"origin".to_owned()), "rows: {rows:?}");
        assert!(!rows.contains(&"origin/main".to_owned()), "rows: {rows:?}");
    }

    #[test]
    fn revision_and_branch_slots_offer_only_valid_ref_families() {
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

        let branch = context(root.path(), "git branch ");
        assert!(!provider.applies(&branch));
        assert!(provider.complete(&branch).candidates.is_empty());

        let rows = primaries(&context(root.path(), "git branch new "), &provider);
        assert!(rows.contains(&"main".to_owned()), "rows: {rows:?}");
        assert!(rows.contains(&"v1".to_owned()), "rows: {rows:?}");

        for text in [
            "git branch new main ",
            "git branch -m old ",
            "git branch -m old new ",
            "git rebase main topic ",
        ] {
            let context = context(root.path(), text);
            assert!(!provider.applies(&context), "{text:?} must be complete");
            assert!(
                provider.complete(&context).candidates.is_empty(),
                "{text:?} must not offer more refs"
            );
        }

        let rows = primaries(&context(root.path(), "git branch -d "), &provider);
        assert!(rows.contains(&"feature/mars".to_owned()), "rows: {rows:?}");
        assert!(!rows.contains(&"main".to_owned()), "rows: {rows:?}");
    }

    #[test]
    fn ref_valued_flags_complete_refs_in_separate_and_attached_forms() {
        let Some(root) = ref_repository() else { return };
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );
        for text in [
            "git branch --contains ",
            "git branch --merged ma",
            "git branch --points-at ",
            "git rebase --onto ",
            "git restore --source ",
        ] {
            let rows = primaries(&context(root.path(), text), &provider);
            assert!(
                rows.contains(&"main".to_owned()),
                "rows for {text:?}: {rows:?}"
            );
        }

        let context = context(root.path(), "git branch --contains=fea");
        let feature = provider
            .complete(&context)
            .candidates
            .into_iter()
            .find(|candidate| candidate.display.primary == "feature/mars")
            .expect("attached ref flag candidate");
        assert_eq!(
            feature.edit.as_ref().expect("edit").replacement,
            "--contains=feature/mars"
        );
    }

    #[test]
    fn wrapper_prefix_is_preserved_in_row_edits() {
        let directory = tempfile::tempdir().expect("directory");
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(directory.path()),
        );
        // `sudo git ` still recommends from state, and the row fills only
        // the empty subcommand slot so every wrapper/global prefix survives.
        let sudo_git = context(directory.path(), "sudo git ");
        assert!(provider.applies(&sudo_git));
        let output = provider.complete(&sudo_git);
        assert_eq!(
            output
                .candidates
                .first()
                .map(|candidate| candidate.display.primary.as_str()),
            Some("git init")
        );
        let edit = output.candidates[0].edit.as_ref().expect("edit");
        assert_eq!(edit.range, 9..9);
        assert_eq!(edit.replacement, "init");

        // `sudo git st` still fires at the subcommand slot.
        assert!(provider.applies(&context(directory.path(), "sudo git st")));
    }

    #[test]
    fn wrapper_prefixed_checkout_completes_refs() {
        let Some(root) = ref_repository() else { return };
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );
        let context = context(root.path(), "sudo git checkout fea");
        assert!(provider.applies(&context));
        let rows = primaries(&context, &provider);
        assert!(rows.contains(&"feature/mars".to_owned()), "rows: {rows:?}");
    }

    #[test]
    fn wrapper_chdir_selects_the_nested_git_repository() {
        if !git_available() {
            return;
        }
        let root = tempfile::tempdir().expect("root");
        let app = root.path().join("app");
        fs::create_dir(&app).expect("app");
        git(&app, &["init", "-q", "-b", "main"]);
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );
        let context = context(root.path(), "sudo -D app git ");
        let rows = primaries(&context, &provider);
        assert!(
            rows.contains(&"git branch".to_owned()) || rows.contains(&"git status".to_owned()),
            "rows: {rows:?}"
        );
        assert!(!rows.contains(&"git init".to_owned()), "rows: {rows:?}");
    }

    #[test]
    fn double_dash_ends_the_ref_slot() {
        let Some(root) = ref_repository() else { return };
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );
        // After `--` the slot is a pathspec: no ref rows.
        for text in ["git checkout -- ", "git checkout -- ma"] {
            let context = context(root.path(), text);
            assert!(
                provider.complete(&context).candidates.is_empty(),
                "{text:?} must not offer refs"
            );
            assert!(!provider.applies(&context), "{text:?} must not apply");
        }
        // Without `--` the same subcommand still offers refs.
        assert!(provider.applies(&context(root.path(), "git checkout ")));
        assert!(!provider.applies(&context(root.path(), "git checkout main ")));
    }

    #[test]
    fn checkout_dash_b_and_switch_dash_c_take_a_new_branch_name() {
        let Some(root) = ref_repository() else { return };
        let provider = GitProvider::new(
            Arc::new(GitStatusCache::default()),
            Arc::new(GitRefsCache::default()),
            git_on_path(root.path()),
        );
        // The word after `-b`/`-c` is a NEW branch name, not an existing ref.
        for text in ["git checkout -b ", "git checkout -b ne", "git switch -c "] {
            let context = context(root.path(), text);
            assert!(
                provider.complete(&context).candidates.is_empty(),
                "{text:?} must not offer refs"
            );
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

    #[test]
    fn ref_cap_is_applied_after_matching_the_typed_prefix() {
        let directory = tempfile::tempdir().expect("directory");
        let context = context(directory.path(), "git checkout release-special");
        let mut candidates: Vec<_> = (0..MAX_REF_ROWS + 50)
            .map(|index| {
                ref_candidate(
                    &context,
                    &format!("branch-{index:04}"),
                    "本地分支",
                    LOCAL_BOOST,
                    "",
                )
            })
            .collect();
        candidates.push(ref_candidate(
            &context,
            "release-special",
            "标签",
            REMOTE_BOOST,
            "",
        ));

        preselect_ref_candidates(&context, &mut candidates);
        assert_eq!(candidates.len(), MAX_REF_ROWS);
        assert_eq!(candidates[0].display.primary, "release-special");
    }
}
