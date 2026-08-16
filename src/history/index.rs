use std::{
    cmp::Ordering,
    collections::HashMap,
    path::{Path, PathBuf},
};

use regex::Regex;

use crate::{completion::match_quality_folded, history::HistoryEventV1, shell::ShellKind};

#[derive(Clone, Debug)]
pub struct HistoryPolicy {
    max_command_bytes: usize,
    excluded: Vec<Regex>,
}

impl HistoryPolicy {
    pub fn new(max_command_bytes: usize, excluded: &[String]) -> crate::Result<Self> {
        let excluded = excluded
            .iter()
            .map(|pattern| {
                Regex::new(pattern).map_err(|error| {
                    crate::Error::Config(format!("invalid history exclusion {pattern:?}: {error}"))
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            max_command_bytes,
            excluded,
        })
    }

    #[must_use]
    pub fn allows(&self, command: &str) -> bool {
        !command.is_empty()
            && command.len() <= self.max_command_bytes
            && !command.contains('\0')
            && !self
                .excluded
                .iter()
                .any(|pattern| pattern.is_match(command))
            && !looks_sensitive(command)
    }
}

#[derive(Clone, Debug)]
pub struct HistoryRecord {
    pub command: String,
    pub count: u64,
    pub last_used_ms: i64,
    pub shell: ShellKind,
    pub last_cwd: Option<PathBuf>,
    pub multiline: bool,
    /// Exit code of the most recent run with a known exit code; `None` when
    /// the record came from an import or a shell that reported none.
    pub last_exit_code: Option<i32>,
    search_key: String,
}

/// A run counts as failed when its exit code is known, non-zero, and not 130
/// (SIGINT — the user aborted, which says nothing about the command).
#[must_use]
pub fn is_failed_exit(exit_code: Option<i32>) -> bool {
    exit_code.is_some_and(|code| code != 0 && code != 130)
}

/// Reduce a command line to its transition skeleton: the first token, plus
/// the second token when it is a plain lowercase word (a subcommand, not a
/// flag or path). `git commit -m x` -> `git commit`, `ls -la` -> `ls`.
#[must_use]
pub fn command_skeleton(command: &str) -> String {
    let mut tokens = command.split_whitespace();
    let Some(first) = tokens.next() else {
        return String::new();
    };
    let first = first.to_lowercase();
    let Some(second) = tokens.next() else {
        return first;
    };
    let second = second.to_lowercase();
    if second.len() >= 2 && second.bytes().all(|byte| byte.is_ascii_lowercase()) {
        format!("{first} {second}")
    } else {
        first
    }
}

#[derive(Clone, Debug)]
pub struct HistoryMatch {
    pub record: HistoryRecord,
    pub quality: i16,
    pub frecency: i16,
    pub cwd_affinity: i16,
    pub failed_penalty: i16,
}

#[derive(Clone, Debug, Default)]
struct DirectoryUsage {
    count: u64,
    last_used_ms: i64,
}

struct RankedRecord<'a> {
    record: &'a HistoryRecord,
    quality: i16,
    frecency: i16,
    cwd_affinity: i16,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryIndex {
    records: HashMap<String, HistoryRecord>,
    /// Successful usage counts for each normalized command, partitioned by
    /// the directory in which it was executed. Imported history normally has
    /// no cwd and therefore does not contribute to project-local ranking.
    cwd_usage: HashMap<String, HashMap<PathBuf, DirectoryUsage>>,
    /// Canonicalizing every event in a large history log is expensive. Cache
    /// the result per distinct cwd while retaining nonexistent old paths as
    /// they were recorded.
    cwd_normalizations: HashMap<PathBuf, PathBuf>,
    /// Bigram of consecutive executed commands per shell stream:
    /// previous skeleton -> next skeleton -> observed count.
    transitions: HashMap<String, HashMap<String, u64>>,
    last_skeleton: HashMap<ShellKind, String>,
}

impl HistoryIndex {
    pub fn ingest(
        &mut self,
        command: &str,
        timestamp_ms: i64,
        shell: ShellKind,
        cwd: Option<&Path>,
        exit_code: Option<i32>,
        policy: &HistoryPolicy,
    ) -> bool {
        self.ingest_weighted(command, timestamp_ms, shell, cwd, 1, exit_code, policy)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ingest_weighted(
        &mut self,
        command: &str,
        timestamp_ms: i64,
        shell: ShellKind,
        cwd: Option<&Path>,
        occurrences: u64,
        exit_code: Option<i32>,
        policy: &HistoryPolicy,
    ) -> bool {
        self.ingest_weighted_with_cwd_scope(
            command,
            timestamp_ms,
            shell,
            cwd,
            occurrences,
            occurrences,
            exit_code,
            policy,
        )
    }

    pub fn ingest_event(&mut self, event: &HistoryEventV1, policy: &HistoryPolicy) -> bool {
        self.ingest_weighted_with_cwd_scope(
            &event.command,
            event.timestamp_ms,
            event.shell,
            event.cwd.as_deref(),
            event.occurrences,
            event
                .cwd_occurrences
                .unwrap_or(1)
                .clamp(1, event.occurrences.max(1)),
            event.exit_code,
            policy,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn ingest_weighted_with_cwd_scope(
        &mut self,
        command: &str,
        timestamp_ms: i64,
        shell: ShellKind,
        cwd: Option<&Path>,
        occurrences: u64,
        cwd_occurrences: u64,
        exit_code: Option<i32>,
        policy: &HistoryPolicy,
    ) -> bool {
        if occurrences == 0 {
            return false;
        }
        if !policy.allows(command) {
            return false;
        }
        let normalized = normalize(command);
        let normalized_cwd = cwd.map(|cwd| self.normalized_cwd(cwd));
        self.record_transition(shell, command);
        let record = self
            .records
            .entry(normalized.clone())
            .or_insert_with(|| HistoryRecord {
                command: command.trim().to_owned(),
                count: 0,
                last_used_ms: timestamp_ms,
                shell,
                last_cwd: cwd.map(Path::to_owned),
                multiline: command.contains('\n'),
                last_exit_code: exit_code,
                search_key: command.trim().to_lowercase(),
            });
        // Failed runs refresh recency but do not build frequency: a command
        // that keeps failing is not a command worth recommending more.
        if !is_failed_exit(exit_code) {
            record.count = record.count.saturating_add(occurrences);
        }
        if let Some(cwd) = normalized_cwd.as_deref() {
            let usage = self
                .cwd_usage
                .entry(normalized)
                .or_default()
                .entry(cwd.to_owned())
                .or_default();
            if !is_failed_exit(exit_code) {
                usage.count = usage.count.saturating_add(cwd_occurrences);
            }
            usage.last_used_ms = usage.last_used_ms.max(timestamp_ms);
        }
        if timestamp_ms >= record.last_used_ms {
            record.command = command.trim().to_owned();
            record.last_used_ms = timestamp_ms;
            record.shell = shell;
            record.last_cwd = cwd.map(Path::to_owned);
            record.multiline = command.contains('\n');
            record.last_exit_code = exit_code;
            record.search_key = command.trim().to_lowercase();
        }
        true
    }

    fn record_transition(&mut self, shell: ShellKind, command: &str) {
        let skeleton = command_skeleton(command);
        if skeleton.is_empty() {
            return;
        }
        if let Some(previous) = self.last_skeleton.insert(shell, skeleton.clone()) {
            *self
                .transitions
                .entry(previous)
                .or_default()
                .entry(skeleton)
                .or_insert(0) += 1;
        }
    }

    fn normalized_cwd(&mut self, cwd: &Path) -> PathBuf {
        if let Some(normalized) = self.cwd_normalizations.get(cwd) {
            return normalized.clone();
        }
        let normalized = normalize_history_cwd(cwd);
        self.cwd_normalizations
            .insert(cwd.to_owned(), normalized.clone());
        normalized
    }

    /// Score how strongly `candidate` follows `previous` in the recorded
    /// command streams: 0 when no bigram is known, otherwise
    /// count/max_count_for_previous * 200 (capped at 200).
    #[must_use]
    pub fn transition_score(&self, previous: &str, candidate: &str) -> i16 {
        let previous = command_skeleton(previous);
        let candidate = command_skeleton(candidate);
        if previous.is_empty() || candidate.is_empty() {
            return 0;
        }
        let Some(successors) = self.transitions.get(&previous) else {
            return 0;
        };
        let Some(&count) = successors.get(&candidate) else {
            return 0;
        };
        let max = successors.values().copied().max().unwrap_or(1).max(1);
        (count.saturating_mul(200) / max).min(200) as i16
    }

    /// Usage-based frecency of an exact command line, matching `search`'s
    /// scoring. 0 when the command was never recorded.
    #[must_use]
    pub fn usage_frecency(&self, command: &str, now_ms: i64) -> i16 {
        let Some(record) = self.records.get(&normalize(command)) else {
            return 0;
        };
        frecency_score(record.last_used_ms, record.count, now_ms)
    }

    /// Usage-based frecency of an exact command line, limited to executions
    /// whose cwd is the project root or one of its descendants. History rows
    /// without a cwd are deliberately excluded because their project cannot
    /// be established reliably. Project discovery supplies a canonical root,
    /// matching the canonical cwd keys retained by this index.
    #[must_use]
    pub fn usage_frecency_in_project(
        &self,
        command: &str,
        project_root: &Path,
        now_ms: i64,
    ) -> i16 {
        let normalized = normalize(command);
        let Some(usages) = self.cwd_usage.get(&normalized) else {
            return 0;
        };
        let mut count = 0_u64;
        let mut last_used_ms = i64::MIN;
        for (cwd, usage) in usages {
            if cwd == project_root || cwd.starts_with(project_root) {
                count = count.saturating_add(usage.count);
                last_used_ms = last_used_ms.max(usage.last_used_ms);
            }
        }
        if last_used_ms == i64::MIN || count == 0 {
            return 0;
        }
        frecency_score(last_used_ms, count, now_ms)
    }

    pub fn merge_events_absolute(&mut self, events: &[HistoryEventV1], policy: &HistoryPolicy) {
        let mut incoming = Self::default();
        for event in events {
            incoming.ingest_event(event, policy);
        }
        for (normalized, incoming) in incoming.records {
            match self.records.get_mut(&normalized) {
                Some(existing) => {
                    existing.count = existing.count.max(incoming.count);
                    if incoming.last_used_ms >= existing.last_used_ms {
                        let count = existing.count;
                        *existing = incoming;
                        existing.count = count;
                    }
                }
                None => {
                    self.records.insert(normalized, incoming);
                }
            }
        }
        for (normalized, incoming_usages) in incoming.cwd_usage {
            let usages = self.cwd_usage.entry(normalized).or_default();
            for (cwd, incoming_usage) in incoming_usages {
                let usage = usages.entry(cwd).or_default();
                usage.count = usage.count.max(incoming_usage.count);
                usage.last_used_ms = usage.last_used_ms.max(incoming_usage.last_used_ms);
            }
        }
        // Transitions are additive, so an absolute merge must rebuild them
        // from the event stream (chronological per shell) rather than merge,
        // or replaying the same log would double-count every bigram.
        let mut ordered: Vec<&HistoryEventV1> = events
            .iter()
            .filter(|event| policy.allows(&event.command))
            .collect();
        ordered.sort_by_key(|event| event.timestamp_ms);
        self.transitions.clear();
        self.last_skeleton.clear();
        for event in ordered {
            self.record_transition(event.shell, &event.command);
        }
    }

    #[must_use]
    pub fn search(&self, query: &str, cwd: &Path, now_ms: i64, limit: usize) -> Vec<HistoryMatch> {
        self.search_filtered(query, cwd, now_ms, limit, |_| true)
    }

    /// Search history after removing records that cannot participate in the
    /// current completion context. Filtering before the bounded top-k keeps
    /// noisy or stale rows from crowding relevant candidates out.
    pub(crate) fn search_filtered(
        &self,
        query: &str,
        cwd: &Path,
        now_ms: i64,
        limit: usize,
        mut predicate: impl FnMut(&HistoryRecord) -> bool,
    ) -> Vec<HistoryMatch> {
        if limit == 0 {
            return Vec::new();
        }
        let query = query.to_lowercase();
        let mut ranked: Vec<_> = self
            .records
            .values()
            .filter(|record| !record.multiline)
            .filter_map(|record| {
                let quality = match_quality_folded(&query, &record.search_key);
                if !query.is_empty() && quality == 0 {
                    return None;
                }
                if !predicate(record) {
                    return None;
                }
                let age_hours = now_ms.saturating_sub(record.last_used_ms).max(0) / 3_600_000;
                let recency = 150_i64.saturating_sub(age_hours.min(150));
                let frequency = (record.count.min(50) * 2) as i64;
                Some(RankedRecord {
                    record,
                    quality,
                    frecency: (recency + frequency).min(200) as i16,
                    cwd_affinity: if record.last_cwd.as_deref() == Some(cwd) {
                        100
                    } else {
                        0
                    },
                })
            })
            .collect();
        if ranked.len() > limit {
            ranked.select_nth_unstable_by(limit, compare_ranked);
            ranked.truncate(limit);
        }
        ranked.sort_by(compare_ranked);
        ranked
            .into_iter()
            .map(|ranked| HistoryMatch {
                record: ranked.record.clone(),
                quality: ranked.quality,
                frecency: ranked.frecency,
                cwd_affinity: ranked.cwd_affinity,
                failed_penalty: if is_failed_exit(ranked.record.last_exit_code) {
                    150
                } else {
                    0
                },
            })
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

fn compare_ranked(left: &RankedRecord<'_>, right: &RankedRecord<'_>) -> Ordering {
    let left_score = left.quality as i32 + left.frecency as i32 + left.cwd_affinity as i32;
    let right_score = right.quality as i32 + right.frecency as i32 + right.cwd_affinity as i32;
    right_score
        .cmp(&left_score)
        .then_with(|| right.record.last_used_ms.cmp(&left.record.last_used_ms))
        .then_with(|| left.record.command.cmp(&right.record.command))
}

fn normalize(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn frecency_score(last_used_ms: i64, count: u64, now_ms: i64) -> i16 {
    let age_hours = now_ms.saturating_sub(last_used_ms).max(0) / 3_600_000;
    let recency = 150_i64.saturating_sub(age_hours.min(150));
    let frequency = (count.min(50) * 2) as i64;
    (recency + frequency).min(200) as i16
}

fn normalize_history_cwd(cwd: &Path) -> PathBuf {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_owned())
}

fn looks_sensitive(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    // Flags appear in both `--api-key=` and `--api_key=` spellings; normalize
    // dashes (in the command and the markers) so one list covers both.
    let normalized = lower.replace('-', "_");
    let marker = [
        "--password",
        "--passwd",
        "--token",
        "password=",
        "passwd=",
        "token=",
        "access_token=",
        "refresh_token=",
        "client_secret=",
        "api_key=",
        "apikey=",
        "authorization:",
        "cookie:",
        "private_key=",
        "secret_access_key",
    ]
    .iter()
    .any(|marker| normalized.contains(&marker.replace('-', "_")));
    marker || contains_url_credentials(&lower)
}

fn contains_url_credentials(command: &str) -> bool {
    command.split_whitespace().any(|word| {
        let Some((_, rest)) = word.split_once("://") else {
            return false;
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        authority
            .split_once('@')
            .is_some_and(|(userinfo, _)| userinfo.contains(':'))
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn dedupes_ranks_and_filters_secrets() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        assert!(index.ingest(
            "git  status",
            100,
            ShellKind::Zsh,
            Some(Path::new("/a")),
            Some(0),
            &policy
        ));
        assert!(index.ingest(
            "git status",
            200,
            ShellKind::Bash,
            Some(Path::new("/a")),
            Some(0),
            &policy
        ));
        assert!(!index.ingest(
            "curl --token secret",
            300,
            ShellKind::Zsh,
            None,
            None,
            &policy
        ));
        for sensitive in [
            "PASSWORD=secret command",
            "export ACCESS_TOKEN=secret",
            "tool --header 'Cookie: session=secret'",
            "curl https://user:secret@example.test/path",
            "tool --api-key=secret",
            "tool --client-secret=secret",
            "tool --access-token=secret",
            "tool --refresh-token=secret",
        ] {
            assert!(!index.ingest(sensitive, 300, ShellKind::Zsh, None, None, &policy));
        }
        assert_eq!(index.len(), 1);
        let matches = index.search("git", Path::new("/a"), 300, 10);
        assert_eq!(matches[0].record.count, 2);
        assert_eq!(matches[0].cwd_affinity, 100);
    }

    #[test]
    fn failed_runs_refresh_recency_without_building_frequency() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        let cwd = Path::new("/a");
        index.ingest(
            "make deploy",
            100,
            ShellKind::Zsh,
            Some(cwd),
            Some(0),
            &policy,
        );
        index.ingest(
            "make deploy",
            200,
            ShellKind::Zsh,
            Some(cwd),
            Some(0),
            &policy,
        );
        index.ingest(
            "make deploy",
            300,
            ShellKind::Zsh,
            Some(cwd),
            Some(1),
            &policy,
        );
        let matched = index.search("make", cwd, 300, 1).pop().expect("match");
        assert_eq!(matched.record.count, 2, "failure must not bump count");
        assert_eq!(
            matched.record.last_used_ms, 300,
            "failure refreshes recency"
        );
        assert_eq!(matched.record.last_exit_code, Some(1));
        assert_eq!(matched.failed_penalty, 150);

        // A later success clears the penalty and resumes counting.
        index.ingest(
            "make deploy",
            400,
            ShellKind::Zsh,
            Some(cwd),
            Some(0),
            &policy,
        );
        let matched = index.search("make", cwd, 400, 1).pop().expect("match");
        assert_eq!(matched.record.count, 3);
        assert_eq!(matched.record.last_exit_code, Some(0));
        assert_eq!(matched.failed_penalty, 0);
    }

    #[test]
    fn project_frecency_uses_only_cwds_inside_the_project() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let project = tempfile::tempdir().expect("project");
        let nested = project.path().join("packages/app");
        let other = tempfile::tempdir().expect("other");
        std::fs::create_dir_all(&nested).expect("nested");
        let project_root = project.path().canonicalize().expect("canonical project");
        let nested = nested.canonicalize().expect("canonical nested");
        let other = other.path().canonicalize().expect("canonical other");
        let mut index = HistoryIndex::default();

        for round in 0..10 {
            index.ingest(
                "pnpm dev",
                1_000 + round,
                ShellKind::Zsh,
                Some(&nested),
                Some(0),
                &policy,
            );
        }
        for round in 0..100 {
            index.ingest(
                "pnpm build",
                2_000 + round,
                ShellKind::Zsh,
                Some(&other),
                Some(0),
                &policy,
            );
        }
        // Imported rows have no cwd and must not acquire an invented project.
        index.ingest("pnpm deploy", 3_000, ShellKind::Zsh, None, Some(0), &policy);
        index.ingest(
            "pnpm broken",
            3_000,
            ShellKind::Zsh,
            Some(&project_root),
            Some(1),
            &policy,
        );

        let dev = index.usage_frecency_in_project("pnpm dev", &project_root, 3_000);
        let build = index.usage_frecency_in_project("pnpm build", &project_root, 3_000);
        assert!(dev > build, "local usage must beat another project's usage");
        assert_eq!(
            index.usage_frecency_in_project("pnpm deploy", &project_root, 3_000),
            0,
            "cwd-less imports cannot affect project ranking"
        );
        assert_eq!(
            index.usage_frecency_in_project("pnpm broken", &project_root, 3_000),
            0,
            "a command that only failed cannot gain project frequency"
        );
    }

    #[test]
    fn sigint_and_unknown_exit_codes_are_not_failures() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        index.ingest("make build", 100, ShellKind::Zsh, None, Some(130), &policy);
        index.ingest("make clean", 100, ShellKind::Zsh, None, None, &policy);
        let build = index
            .search("make build", Path::new("/"), 100, 1)
            .pop()
            .expect("build");
        assert_eq!(build.record.count, 1, "SIGINT still counts as a run");
        assert_eq!(build.record.last_exit_code, Some(130));
        assert_eq!(build.failed_penalty, 0);
        let clean = index
            .search("make clean", Path::new("/"), 100, 1)
            .pop()
            .expect("clean");
        assert_eq!(clean.record.count, 1, "imports count normally");
        assert_eq!(clean.record.last_exit_code, None);
        assert_eq!(clean.failed_penalty, 0);
    }

    #[test]
    fn skeletons_keep_plain_word_subcommands_only() {
        assert_eq!(command_skeleton("git commit -m x"), "git commit");
        assert_eq!(command_skeleton("git commit"), "git commit");
        assert_eq!(command_skeleton("Git COMMIT --amend"), "git commit");
        assert_eq!(command_skeleton("ls -la"), "ls");
        assert_eq!(command_skeleton("ls /tmp"), "ls");
        assert_eq!(command_skeleton("npm run dev"), "npm run");
        assert_eq!(command_skeleton("cargo build"), "cargo build");
        assert_eq!(
            command_skeleton("git x"),
            "git",
            "one-letter words are not subcommands"
        );
        assert_eq!(command_skeleton("gst"), "gst");
        assert_eq!(command_skeleton("./script.sh run"), "./script.sh run");
        assert_eq!(command_skeleton("   "), "");
    }

    #[test]
    fn transitions_score_known_successors_in_proportion() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for round in 0..2 {
            let base = 100 + round * 10;
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
        index.ingest("git add x", 200, ShellKind::Zsh, None, Some(0), &policy);
        index.ingest(
            "git checkout z",
            201,
            ShellKind::Zsh,
            None,
            Some(0),
            &policy,
        );

        assert_eq!(index.transition_score("git add x", "git commit -m a"), 200);
        assert_eq!(index.transition_score("git add .", "git checkout z"), 100);
        assert_eq!(
            index.transition_score("ls", "git commit"),
            0,
            "unknown previous"
        );
        assert_eq!(
            index.transition_score("git add x", "git push"),
            0,
            "not a successor"
        );
        assert!(
            index.transition_score("git add x", "git commit") <= 200,
            "cap respected"
        );
    }

    #[test]
    fn transitions_are_tracked_per_shell_and_rebuilt_on_absolute_merge() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let event = |timestamp_ms, command: &str, shell| HistoryEventV1 {
            event_id: None,
            timestamp_ms,
            command: command.into(),
            cwd: None,
            shell,
            exit_code: Some(0),
            imported: false,
            occurrences: 1,
            cwd_occurrences: None,
        };
        let mut index = HistoryIndex::default();
        index.merge_events_absolute(
            &[
                // Deliberately unordered input: ordering comes from timestamps.
                event(30, "git push", ShellKind::Zsh),
                event(10, "git add x", ShellKind::Zsh),
                event(20, "git commit -m y", ShellKind::Zsh),
                // Interleaved bash stream must not pollute the zsh bigram.
                event(15, "make build", ShellKind::Bash),
            ],
            &policy,
        );
        assert_eq!(index.transition_score("git add x", "git commit"), 200);
        assert_eq!(index.transition_score("git commit", "git push"), 200);
        assert_eq!(index.transition_score("git add x", "make build"), 0);

        // A second absolute merge rebuilds rather than double-counting.
        index.merge_events_absolute(
            &[
                event(10, "git add x", ShellKind::Zsh),
                event(20, "git commit -m y", ShellKind::Zsh),
            ],
            &policy,
        );
        assert_eq!(index.transition_score("git add x", "git commit"), 200);
        assert_eq!(index.transition_score("git commit", "git push"), 0);
    }

    #[test]
    fn absolute_merge_does_not_double_compacted_counts() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        index.ingest_weighted("git status", 100, ShellKind::Zsh, None, 3, Some(0), &policy);
        let event = HistoryEventV1 {
            event_id: None,
            timestamp_ms: 100,
            command: "git status".into(),
            cwd: None,
            shell: ShellKind::Zsh,
            exit_code: Some(0),
            imported: false,
            occurrences: 3,
            cwd_occurrences: None,
        };
        index.merge_events_absolute(&[event], &policy);
        assert_eq!(
            index.search("git", Path::new("/"), 100, 1)[0].record.count,
            3
        );
    }

    #[test]
    fn absolute_merge_does_not_double_project_counts() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let project = tempfile::tempdir().expect("project");
        let project_root = project.path().canonicalize().expect("canonical project");
        let event = HistoryEventV1 {
            event_id: None,
            timestamp_ms: 100,
            command: "pnpm dev".into(),
            cwd: Some(project_root.clone()),
            shell: ShellKind::Zsh,
            exit_code: Some(0),
            imported: false,
            occurrences: 3,
            cwd_occurrences: Some(3),
        };
        let mut index = HistoryIndex::default();
        index.merge_events_absolute(std::slice::from_ref(&event), &policy);
        index.merge_events_absolute(&[event], &policy);
        assert_eq!(
            index.usage_frecency_in_project("pnpm dev", &project_root, 100),
            156,
            "three occurrences score as 150 recency + 6 frequency"
        );
    }

    #[test]
    fn legacy_cross_directory_compaction_uses_a_conservative_project_count() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let project = tempfile::tempdir().expect("project");
        let project_root = project.path().canonicalize().expect("canonical project");
        let legacy = HistoryEventV1 {
            event_id: None,
            timestamp_ms: 100,
            command: "pnpm dev".into(),
            cwd: Some(project_root.clone()),
            shell: ShellKind::Zsh,
            exit_code: Some(0),
            imported: false,
            occurrences: 100,
            cwd_occurrences: None,
        };
        let mut index = HistoryIndex::default();
        index.ingest_event(&legacy, &policy);
        assert_eq!(
            index.usage_frecency_in_project("pnpm dev", &project_root, 100),
            152,
            "legacy global occurrences only prove one run in the recorded cwd"
        );
        assert_eq!(
            index.search("pnpm dev", &project_root, 100, 1)[0]
                .record
                .count,
            100,
            "global history frequency remains backward compatible"
        );
    }

    #[test]
    fn filtered_search_checks_only_textually_matching_records() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for number in 0..100 {
            index.ingest(
                &format!("unrelated-command-{number}"),
                number,
                ShellKind::Zsh,
                None,
                Some(0),
                &policy,
            );
        }
        index.ingest(
            "needle-command",
            1_000,
            ShellKind::Zsh,
            None,
            Some(0),
            &policy,
        );

        let mut checks = 0;
        let rows = index.search_filtered("needle", Path::new("/"), 2_000, 10, |_| {
            checks += 1;
            true
        });
        assert_eq!(checks, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record.command, "needle-command");
    }

    #[test]
    fn searches_one_hundred_thousand_records_with_bounded_work() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        for number in 0..100_000 {
            index.ingest(
                &format!("git project-{number} status"),
                number,
                ShellKind::Zsh,
                None,
                None,
                &policy,
            );
        }
        let started = Instant::now();
        let matches = index.search("git project-999", Path::new("/"), 100_000, 50);
        let elapsed = started.elapsed();
        assert!(!matches.is_empty());
        let budget = if cfg!(debug_assertions) {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(30)
        };
        assert!(elapsed <= budget, "100k history query took {elapsed:?}");
    }
}
