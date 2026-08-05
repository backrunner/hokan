//! Git repository context for intent-aware `git` completion: where the cwd
//! sits relative to a repository and what the repository's state is. The
//! status probe runs `git status --porcelain` through the bounded platform
//! runner and is cached briefly so a burst of keystrokes costs at most one
//! subprocess.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// How long a cached repository context is trusted. Short enough that a
/// commit made in another terminal shows up quickly, long enough that typing
/// a full command never spawns `git status` twice.
const STATUS_TTL: Duration = Duration::from_millis(2_000);
/// `git status` on a huge or NFS-backed repository must never stall the
/// completion worker; on timeout the context degrades to "unknown".
const STATUS_TIMEOUT: Duration = Duration::from_millis(800);
const STATUS_MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Matches the workspace probe's walk-up bound.
const MAX_WALK_UP: usize = 8;
/// Same TTL as the status cache: a burst of keystrokes costs at most one
/// `for-each-ref` run, while a branch created elsewhere shows up quickly.
const REFS_TTL: Duration = Duration::from_millis(2_000);
const REFS_TIMEOUT: Duration = Duration::from_millis(800);
const REFS_MAX_OUTPUT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitContext {
    /// No `.git` found at or above the cwd.
    NotARepository,
    /// Inside a repository but the status probe failed or timed out: the
    /// provider falls back to state-independent rows.
    RepositoryUnknown,
    /// Inside a repository with a fresh status.
    Repository(GitStatus),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitStatus {
    pub branch: Option<String>,
    /// Commits the branch is ahead of its upstream (0 also when there is no
    /// upstream — nothing to push *to*).
    pub ahead: u32,
    pub behind: u32,
    pub has_upstream: bool,
    /// Staged, unstaged, or untracked changes present.
    pub has_changes: bool,
}

#[derive(Debug, Default)]
pub struct GitStatusCache {
    entries: Mutex<HashMap<PathBuf, CacheEntry>>,
}

#[derive(Debug)]
struct CacheEntry {
    at: Instant,
    context: GitContext,
}

impl GitStatusCache {
    pub fn context_for(&self, cwd: &Path) -> GitContext {
        if let Some(entry) = self.entries.lock().ok().and_then(|entries| {
            entries
                .get(cwd)
                .filter(|entry| entry.at.elapsed() < STATUS_TTL)
                .map(|entry| entry.context.clone())
        }) {
            return entry;
        }
        let context = probe(cwd);
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(
                cwd.to_owned(),
                CacheEntry {
                    at: Instant::now(),
                    context: context.clone(),
                },
            );
        }
        context
    }
}

fn probe(cwd: &Path) -> GitContext {
    let Some(root) = find_repo_root(cwd) else {
        return GitContext::NotARepository;
    };
    let Ok(output) = crate::platform::run_bounded(
        "git",
        [
            "-C",
            root.to_str().unwrap_or("."),
            "status",
            "--porcelain=v1",
            "--branch",
            "--ahead-behind",
        ],
        STATUS_TIMEOUT,
        STATUS_MAX_OUTPUT_BYTES,
    ) else {
        return GitContext::RepositoryUnknown;
    };
    if !output.status.success() {
        return GitContext::RepositoryUnknown;
    }
    GitContext::Repository(parse_status(&String::from_utf8_lossy(&output.stdout)))
}

/// Branch/remote/tag listing of a repository, used by ref completion
/// (`git checkout <…>` and friends).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitRefs {
    /// The checked-out local branch, if any (detached HEAD → `None`).
    pub current: Option<String>,
    /// Local branch short names.
    pub locals: Vec<String>,
    /// Remote-tracking refs (`origin/main`), symbolic `*/HEAD` excluded.
    pub remotes: Vec<String>,
    /// Bare remote names derived from the remote-tracking refs (`origin`).
    pub remote_names: Vec<String>,
    pub tags: Vec<String>,
}

/// Short-TTL cache of ref listings keyed by repository root, mirroring
/// [`GitStatusCache`]: one bounded `for-each-ref` run per burst of
/// keystrokes.
#[derive(Debug, Default)]
pub struct GitRefsCache {
    entries: Mutex<HashMap<PathBuf, RefsEntry>>,
}

#[derive(Debug)]
struct RefsEntry {
    at: Instant,
    refs: Arc<GitRefs>,
}

impl GitRefsCache {
    /// `None` outside a repository or when the probe fails — ref completion
    /// stays silent rather than guessing.
    pub fn refs_for(&self, cwd: &Path) -> Option<Arc<GitRefs>> {
        let root = find_repo_root(cwd)?;
        if let Some(refs) = self.entries.lock().ok().and_then(|entries| {
            entries
                .get(&root)
                .filter(|entry| entry.at.elapsed() < REFS_TTL)
                .map(|entry| Arc::clone(&entry.refs))
        }) {
            return Some(refs);
        }
        let refs = probe_refs(&root)?;
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(
                root,
                RefsEntry {
                    at: Instant::now(),
                    refs: Arc::clone(&refs),
                },
            );
        }
        Some(refs)
    }
}

fn probe_refs(root: &Path) -> Option<Arc<GitRefs>> {
    let output = crate::platform::run_bounded(
        "git",
        [
            "-C",
            root.to_str().unwrap_or("."),
            "for-each-ref",
            // %(HEAD) marks the checked-out branch, so a single run carries
            // everything ref completion needs.
            "--format=%(HEAD)%09%(refname)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
        REFS_TIMEOUT,
        REFS_MAX_OUTPUT_BYTES,
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(Arc::new(parse_refs(&String::from_utf8_lossy(
        &output.stdout,
    ))))
}

/// Parses `%(HEAD)%09%(refname)` lines into short names grouped by kind.
fn parse_refs(text: &str) -> GitRefs {
    let mut refs = GitRefs::default();
    for line in text.lines() {
        let (head, refname) = line.split_once('\t').unwrap_or((" ", line));
        if let Some(local) = refname.strip_prefix("refs/heads/") {
            if head.trim() == "*" {
                refs.current = Some(local.to_owned());
            }
            refs.locals.push(local.to_owned());
        } else if let Some(remote) = refname.strip_prefix("refs/remotes/") {
            // Symbolic refs such as `origin/HEAD` duplicate the default
            // branch row.
            if remote.ends_with("/HEAD") {
                continue;
            }
            if let Some((name, _)) = remote.split_once('/')
                && !refs.remote_names.iter().any(|known| known == name)
            {
                refs.remote_names.push(name.to_owned());
            }
            refs.remotes.push(remote.to_owned());
        } else if let Some(tag) = refname.strip_prefix("refs/tags/") {
            refs.tags.push(tag.to_owned());
        }
    }
    refs
}

/// Nearest ancestor (or the directory itself) containing `.git` — a
/// directory, or a file for worktrees and submodules.
fn find_repo_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .take(MAX_WALK_UP)
        .find(|directory| fs::symlink_metadata(directory.join(".git")).is_ok())
        .map(Path::to_owned)
}

/// Parses `git status --porcelain=v1 --branch --ahead-behind`. The first
/// `##` line carries the branch and tracking counters; every other line is
/// a changed or untracked path.
fn parse_status(text: &str) -> GitStatus {
    let mut status = GitStatus::default();
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("## ") {
            parse_branch_header(header, &mut status);
        } else if !line.trim().is_empty() {
            status.has_changes = true;
        }
    }
    status
}

fn parse_branch_header(header: &str, status: &mut GitStatus) {
    // "## main...origin/main [ahead 2, behind 1]" — but also
    // "## No commits yet on main" and "## HEAD (no branch)".
    let (branch_part, counters) = match header.split_once(" [") {
        Some((branch, counters)) => (branch, counters.trim_end_matches(']')),
        None => (header, ""),
    };
    let branch = branch_part
        .strip_prefix("No commits yet on ")
        .unwrap_or(branch_part);
    status.branch = match branch.split_once("...") {
        Some((local, upstream)) => {
            status.has_upstream = !upstream.is_empty();
            Some(local.to_owned())
        }
        None => (branch != "HEAD (no branch)").then(|| branch.to_owned()),
    };
    for counter in counters.split(", ") {
        if let Some(ahead) = counter.strip_prefix("ahead ") {
            status.ahead = ahead.parse().unwrap_or(0);
        } else if let Some(behind) = counter.strip_prefix("behind ") {
            status.behind = behind.parse().unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ahead_behind_and_changes() {
        let status = parse_status(
            "## main...origin/main [ahead 2, behind 1]\n M src/lib.rs\n?? notes.txt\n",
        );
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert!(status.has_upstream);
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 1);
        assert!(status.has_changes);
    }

    #[test]
    fn parses_clean_branch_without_upstream() {
        let status = parse_status("## work\n");
        assert_eq!(status.branch.as_deref(), Some("work"));
        assert!(!status.has_upstream);
        assert_eq!(status.ahead, 0);
        assert!(!status.has_changes);
    }

    #[test]
    fn parses_no_commits_yet_and_detached_head() {
        let empty = parse_status("## No commits yet on main\n?? a\n");
        assert_eq!(empty.branch.as_deref(), Some("main"));
        assert!(empty.has_changes);

        let detached = parse_status("## HEAD (no branch)\n");
        assert_eq!(detached.branch, None);
    }

    #[test]
    fn finds_repo_root_upwards_and_reports_plain_directories() {
        let root = tempfile::tempdir().expect("root");
        let repository = root.path().join("repo/src/deep");
        fs::create_dir_all(&repository).expect("nested");
        assert_eq!(find_repo_root(&repository), None);
        fs::create_dir_all(root.path().join("repo/.git")).expect("git dir");
        assert_eq!(
            find_repo_root(&repository).as_deref(),
            Some(root.path().join("repo").as_path())
        );
    }

    #[test]
    fn parses_ref_lines_into_kinds_and_marks_current() {
        let refs = parse_refs(
            "*\trefs/heads/main\n \trefs/heads/feature/mars\n \trefs/remotes/origin/main\n \trefs/remotes/origin/HEAD\n \trefs/tags/v1\n",
        );
        assert_eq!(refs.current.as_deref(), Some("main"));
        assert_eq!(refs.locals, ["main", "feature/mars"]);
        assert_eq!(refs.remotes, ["origin/main"]);
        assert_eq!(refs.remote_names, ["origin"]);
        assert_eq!(refs.tags, ["v1"]);
    }

    fn git_available() -> bool {
        crate::platform::run_bounded("git", ["--version"], Duration::from_secs(2), 1024)
            .is_ok_and(|output| output.status.success())
    }

    fn git(directory: &Path, args: &[&str]) {
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
            crate::platform::run_bounded("git", command, Duration::from_secs(10), 1024 * 1024)
                .expect("git run");
        assert!(output.status.success(), "git {args:?} failed");
    }

    #[test]
    fn refs_cache_lists_and_caches_a_real_repository() {
        if !git_available() {
            return;
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
        git(root.path(), &["branch", "tmp"]);

        let cache = GitRefsCache::default();
        let first = cache.refs_for(root.path()).expect("refs");
        assert_eq!(first.current.as_deref(), Some("main"));
        assert!(first.locals.iter().any(|name| name == "tmp"));

        // Within the TTL a deleted branch is still served from the cache —
        // no second `for-each-ref` run per keystroke burst.
        git(root.path(), &["branch", "-qD", "tmp"]);
        let second = cache.refs_for(root.path()).expect("cached refs");
        assert!(second.locals.iter().any(|name| name == "tmp"));
        assert!(Arc::ptr_eq(&first, &second));

        // Outside a repository there is nothing to offer.
        let plain = tempfile::tempdir().expect("plain");
        assert!(cache.refs_for(plain.path()).is_none());
    }
}
