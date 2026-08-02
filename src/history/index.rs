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
    search_key: String,
}

#[derive(Clone, Debug)]
pub struct HistoryMatch {
    pub record: HistoryRecord,
    pub quality: i16,
    pub frecency: i16,
    pub cwd_affinity: i16,
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
}

impl HistoryIndex {
    pub fn ingest(
        &mut self,
        command: &str,
        timestamp_ms: i64,
        shell: ShellKind,
        cwd: Option<&Path>,
        policy: &HistoryPolicy,
    ) -> bool {
        self.ingest_weighted(command, timestamp_ms, shell, cwd, 1, policy)
    }

    pub fn ingest_weighted(
        &mut self,
        command: &str,
        timestamp_ms: i64,
        shell: ShellKind,
        cwd: Option<&Path>,
        occurrences: u64,
        policy: &HistoryPolicy,
    ) -> bool {
        if occurrences == 0 {
            return false;
        }
        if !policy.allows(command) {
            return false;
        }
        let normalized = normalize(command);
        let record = self
            .records
            .entry(normalized)
            .or_insert_with(|| HistoryRecord {
                command: command.trim().to_owned(),
                count: 0,
                last_used_ms: timestamp_ms,
                shell,
                last_cwd: cwd.map(Path::to_owned),
                multiline: command.contains('\n'),
                search_key: command.trim().to_lowercase(),
            });
        record.count = record.count.saturating_add(occurrences);
        if timestamp_ms >= record.last_used_ms {
            record.command = command.trim().to_owned();
            record.last_used_ms = timestamp_ms;
            record.shell = shell;
            record.last_cwd = cwd.map(Path::to_owned);
            record.multiline = command.contains('\n');
            record.search_key = command.trim().to_lowercase();
        }
        true
    }

    pub fn merge_events_absolute(&mut self, events: &[HistoryEventV1], policy: &HistoryPolicy) {
        let mut incoming = Self::default();
        for event in events {
            incoming.ingest_weighted(
                &event.command,
                event.timestamp_ms,
                event.shell,
                event.cwd.as_deref(),
                event.occurrences,
                policy,
            );
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
    }

    #[must_use]
    pub fn search(&self, query: &str, cwd: &Path, now_ms: i64, limit: usize) -> Vec<HistoryMatch> {
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

fn looks_sensitive(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
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
    .any(|marker| lower.contains(marker));
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
            &policy
        ));
        assert!(index.ingest(
            "git status",
            200,
            ShellKind::Bash,
            Some(Path::new("/a")),
            &policy
        ));
        assert!(!index.ingest("curl --token secret", 300, ShellKind::Zsh, None, &policy));
        for sensitive in [
            "PASSWORD=secret command",
            "export ACCESS_TOKEN=secret",
            "tool --header 'Cookie: session=secret'",
            "curl https://user:secret@example.test/path",
        ] {
            assert!(!index.ingest(sensitive, 300, ShellKind::Zsh, None, &policy));
        }
        assert_eq!(index.len(), 1);
        let matches = index.search("git", Path::new("/a"), 300, 10);
        assert_eq!(matches[0].record.count, 2);
        assert_eq!(matches[0].cwd_affinity, 100);
    }

    #[test]
    fn absolute_merge_does_not_double_compacted_counts() {
        let policy = HistoryPolicy::new(1024, &[]).expect("policy");
        let mut index = HistoryIndex::default();
        index.ingest_weighted("git status", 100, ShellKind::Zsh, None, 3, &policy);
        let event = HistoryEventV1 {
            event_id: None,
            timestamp_ms: 100,
            command: "git status".into(),
            cwd: None,
            shell: ShellKind::Zsh,
            exit_code: Some(0),
            imported: false,
            occurrences: 3,
        };
        index.merge_events_absolute(&[event], &policy);
        assert_eq!(
            index.search("git", Path::new("/"), 100, 1)[0].record.count,
            3
        );
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
