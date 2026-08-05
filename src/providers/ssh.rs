//! `ssh`/`scp`/`sftp`/`mosh` host completion from `~/.ssh/config` (and its
//! one-level `Include` files). The config is parsed once per mtime+length
//! fingerprint, so a burst of keystrokes re-reads nothing.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, TextEdit,
    },
    providers::argument_progress,
    terminal::RiskLevel,
};

/// Commands whose first positional argument is a host.
const SSH_COMMANDS: &[&str] = &["ssh", "sftp", "mosh"];
/// A `~/.ssh/config` larger than this is almost certainly not a hand-written
/// host list; skip it rather than stall completion.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

pub struct SshHostProvider {
    cache: SshConfigCache,
    config: Option<PathBuf>,
}

impl SshHostProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cache: SshConfigCache::default(),
            config: std::env::home_dir().map(|home| home.join(".ssh").join("config")),
        }
    }

    #[cfg(test)]
    fn for_config(config: PathBuf) -> Self {
        Self {
            cache: SshConfigCache::default(),
            config: Some(config),
        }
    }
}

impl Default for SshHostProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateProvider for SshHostProvider {
    fn id(&self) -> &'static str {
        "ssh-host"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        host_slot(context).is_some()
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        if host_slot(context).is_none() {
            return ProviderOutput::default();
        }
        let Some(config) = self.config.as_deref() else {
            return ProviderOutput::default();
        };
        let hosts = self.cache.hosts_from_config(config);
        let prefix = context.parsed.current_prefix.as_str();
        // A typed `user@` prefix is preserved so the row completes the host
        // part only (`scp user@ho` → `user@host`).
        let user = prefix
            .split_once('@')
            .map(|(user, _)| format!("{user}@"))
            .unwrap_or_default();
        ProviderOutput {
            candidates: hosts
                .iter()
                .map(|host| {
                    let completed = format!("{user}{host}");
                    Candidate::new(
                        context.query_id,
                        completed.clone(),
                        "SSH 主机（~/.ssh/config）",
                        Some(TextEdit {
                            range: context.parsed.replacement.clone(),
                            replacement: completed,
                            cursor_after: CursorPlacement::End,
                        }),
                        CandidateAction::Insert,
                        CandidateSource::Project,
                        CandidateKind::Command,
                        Completeness::Runnable,
                        RiskLevel::Low,
                        format!("ssh:host:{host}"),
                    )
                })
                .collect(),
            diagnostics: Vec::new(),
        }
    }
}

/// `Some(())` when the cursor sits at the host slot of an ssh-family
/// command — the unit keeps call sites terse without a bespoke type.
fn host_slot(context: &CompletionContext) -> Option<()> {
    let command = context.command()?;
    let (words, position) = argument_progress(context)?;
    let prefix = context.parsed.current_prefix.as_str();
    if SSH_COMMANDS.contains(&command) && at_host_slot(command, &words, position, prefix) {
        return Some(());
    }
    // scp keeps plain file completion at path slots; only an `user@…`
    // active word past the first argument flips to hosts.
    if command == "scp"
        && position >= 1
        && prefix.contains('@')
        && !prefix.contains('/')
        && !flag_takes_value(command, words.get(position).copied().unwrap_or_default())
    {
        return Some(());
    }
    None
}

/// Whether the cursor sits at the host slot of an ssh/sftp/mosh command:
/// the active word is not a flag, no positional argument precedes it, and
/// the word before the active slot is not a flag expecting a value
/// (`ssh -i <path>` wants a file, not a host). Shared with the filesystem
/// provider, which suppresses its rows at this slot.
pub(crate) fn at_host_slot(command: &str, words: &[&str], position: usize, prefix: &str) -> bool {
    if !SSH_COMMANDS.contains(&command) || prefix.starts_with('-') {
        return false;
    }
    if flag_takes_value(command, words.get(position).copied().unwrap_or_default()) {
        return false;
    }
    !has_positional_before(command, words, position)
}

/// True when a positional (non-flag, non-flag-value) argument already sits
/// before the active slot — the host is taken, later slots want paths.
fn has_positional_before(command: &str, words: &[&str], position: usize) -> bool {
    let mut index = 1; // skip the command word
    while index <= position {
        let word = words.get(index).copied().unwrap_or_default();
        if flag_takes_value(command, word) {
            index += 2; // the flag consumes the next word as its value
        } else if word.starts_with('-') {
            index += 1;
        } else {
            return true;
        }
    }
    false
}

/// Flags after which a host row would be noise because the next word is the
/// flag's value. Mirrors the `flag_value_slot` table in the filesystem
/// provider for the ssh command family.
fn flag_takes_value(command: &str, flag: &str) -> bool {
    if !flag.starts_with('-') {
        return false;
    }
    match command {
        "ssh" | "sftp" | "mosh" => matches!(
            flag,
            "-i" | "-F"
                | "-o"
                | "-l"
                | "-p"
                | "-P"
                | "-L"
                | "-R"
                | "-D"
                | "-J"
                | "-W"
                | "-b"
                | "-c"
                | "-m"
                | "-S"
        ),
        "scp" => matches!(flag, "-i" | "-F" | "-o" | "-l" | "-P" | "-c" | "-S"),
        _ => false,
    }
}

/// (mtime+length fingerprint, parsed hosts) per config path.
type CacheEntry = ((u64, u128), Arc<Vec<String>>);

/// Parsed host lists keyed by config path, invalidated by an mtime+length
/// fingerprint like [`crate::project::ProjectCache`].
#[derive(Debug, Default)]
struct SshConfigCache {
    entries: Mutex<HashMap<PathBuf, CacheEntry>>,
}

impl SshConfigCache {
    /// Hosts of a config file, with its `Include` lines followed one level
    /// deep (includes inside includes are ignored).
    fn hosts_from_config(&self, path: &Path) -> Arc<Vec<String>> {
        let mut hosts = (*self.hosts_from(path)).clone();
        let Ok(text) = fs::read_to_string(path) else {
            return Arc::new(hosts);
        };
        for include in parse_config(&text).1 {
            let Some(resolved) = resolve_include(path, &include) else {
                continue;
            };
            for host in self.hosts_from(&resolved).iter() {
                if !hosts.contains(host) {
                    hosts.push(host.clone());
                }
            }
        }
        Arc::new(hosts)
    }

    /// Hosts of one file only, cached by fingerprint.
    fn hosts_from(&self, path: &Path) -> Arc<Vec<String>> {
        let Some(fingerprint) = fingerprint(path) else {
            return Arc::new(Vec::new());
        };
        if let Some(hosts) = self.entries.lock().ok().and_then(|entries| {
            entries
                .get(path)
                .filter(|(cached, _)| *cached == fingerprint)
                .map(|(_, hosts)| Arc::clone(hosts))
        }) {
            return hosts;
        }
        let hosts = Arc::new(
            fs::read_to_string(path)
                .map(|text| parse_config(&text).0)
                .unwrap_or_default(),
        );
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(path.to_owned(), (fingerprint, Arc::clone(&hosts)));
        }
        hosts
    }
}

/// (length, mtime in ns) — cheap to compute and changes on any edit that
/// matters here.
fn fingerprint(path: &Path) -> Option<(u64, u128)> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return None;
    }
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((metadata.len(), modified))
}

/// Extracts host names and `Include` paths. Wildcard/negated `Host`
/// patterns are skipped (unguessable as rows), everything else — `HostName`
/// lines included — is ignored.
fn parse_config(text: &str) -> (Vec<String>, Vec<String>) {
    let mut hosts = Vec::new();
    let mut includes = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        if keyword.eq_ignore_ascii_case("host") {
            for pattern in parts {
                if pattern.contains(['*', '?']) || pattern.starts_with('!') {
                    continue;
                }
                let host = pattern.to_owned();
                if !hosts.contains(&host) {
                    hosts.push(host);
                }
            }
        } else if keyword.eq_ignore_ascii_case("include") {
            includes.extend(parts.map(str::to_owned));
        }
    }
    (hosts, includes)
}

/// Plain paths and `~/` expansion only — glob includes are skipped.
/// Relative paths resolve against the including file's directory.
fn resolve_include(config_path: &Path, include: &str) -> Option<PathBuf> {
    if include.contains(['*', '?', '[']) {
        return None;
    }
    if let Some(rest) = include.strip_prefix("~/") {
        return std::env::home_dir().map(|home| home.join(rest));
    }
    let path = PathBuf::from(include);
    if path.is_absolute() {
        Some(path)
    } else {
        config_path.parent().map(|directory| directory.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion::{BufferSnapshot, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    fn context(directory: &Path, text: &str, query: u64) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(query),
            ShellKind::Zsh,
            directory.to_owned(),
            BufferSnapshot::new(
                text,
                text.len(),
                BufferRevision::new(query),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context")
    }

    #[test]
    fn parses_hosts_and_skips_wildcards_comments_and_hostnames() {
        let (hosts, includes) = parse_config(
            "# a comment\n\
             Host *\n\
             \x20   ServerAliveInterval 30\n\
             Host dev-box staging !secret qa-?\n\
             \x20   HostName 192.168.1.10\n\
             Include ~/.ssh/hosts.local\n",
        );
        assert_eq!(hosts, ["dev-box", "staging"]);
        assert_eq!(includes, ["~/.ssh/hosts.local"]);
    }

    #[test]
    fn include_is_followed_one_level_deep() {
        let directory = tempfile::tempdir().expect("directory");
        let nested = directory.path().join("nested");
        fs::write(&nested, "Host from-nested\n").expect("nested config");
        let included = directory.path().join("included");
        fs::write(
            &included,
            format!("Host from-include\nInclude {}\n", nested.display()),
        )
        .expect("included config");
        let config = directory.path().join("config");
        fs::write(
            &config,
            format!("Host top\nInclude {}\n", included.display()),
        )
        .expect("config");

        let cache = SshConfigCache::default();
        let hosts = cache.hosts_from_config(&config);
        assert!(hosts.iter().any(|host| host == "top"));
        assert!(hosts.iter().any(|host| host == "from-include"));
        assert!(
            !hosts.iter().any(|host| host == "from-nested"),
            "includes inside includes are not followed: {hosts:?}"
        );
    }

    #[test]
    fn cache_reloads_when_the_config_changes() {
        let directory = tempfile::tempdir().expect("directory");
        let config = directory.path().join("config");
        fs::write(&config, "Host alpha\n").expect("config");
        let cache = SshConfigCache::default();
        let first = cache.hosts_from(&config);
        assert_eq!(first.as_slice(), ["alpha"]);
        let cached = cache.hosts_from(&config);
        assert!(Arc::ptr_eq(&first, &cached), "unchanged file stays cached");

        fs::write(&config, "Host alpha beta-longer\n").expect("rewrite");
        let reloaded = cache.hosts_from(&config);
        assert_eq!(reloaded.as_slice(), ["alpha", "beta-longer"]);
    }

    #[test]
    fn host_slot_rules() {
        let at = |command: &str, text: &str| {
            let directory = tempfile::tempdir().expect("directory");
            let context = context(directory.path(), text, 1);
            let (words, position) = argument_progress(&context).expect("progress");
            at_host_slot(
                command,
                &words,
                position,
                context.parsed.current_prefix.as_str(),
            )
        };
        assert!(at("ssh", "ssh "));
        assert!(at("ssh", "ssh de"));
        assert!(at("ssh", "ssh -v de"));
        assert!(at("ssh", "ssh -p 22 de"));
        assert!(!at("ssh", "ssh -i "));
        assert!(!at("ssh", "ssh -p "));
        assert!(!at("ssh", "ssh -"));
        assert!(!at("ssh", "ssh host1 de"));
        assert!(at("sftp", "sftp "));
        assert!(at("mosh", "mosh "));
        assert!(!at("scp", "scp "));
    }

    #[test]
    fn scp_flips_to_hosts_only_on_an_at_word_past_the_first_argument() {
        let directory = tempfile::tempdir().expect("directory");
        let fires =
            |text: &str, query: u64| host_slot(&context(directory.path(), text, query)).is_some();
        assert!(fires("scp file user@", 1));
        assert!(fires("scp file user@ho", 2));
        assert!(!fires("scp user@ho", 3), "first argument stays a path slot");
        assert!(!fires("scp file ", 4));
        assert!(!fires("scp user@host:/pa", 5), "a slash ends the host part");
    }

    #[test]
    fn completes_hosts_replacing_only_the_typed_word() {
        let directory = tempfile::tempdir().expect("directory");
        let config = directory.path().join("config");
        fs::write(&config, "Host dev-box staging\n").expect("config");
        let provider = SshHostProvider::for_config(config);

        let ssh = context(directory.path(), "ssh de", 1);
        assert!(provider.applies(&ssh));
        let output = provider.complete(&ssh);
        let dev = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "dev-box")
            .expect("dev-box row");
        assert_eq!(dev.display.description, "SSH 主机（~/.ssh/config）");
        let edit = dev.edit.as_ref().expect("edit");
        assert_eq!(edit.range, 4..6);
        assert_eq!(edit.replacement, "dev-box");
        assert_eq!(dev.completeness, Completeness::Runnable);
        assert_eq!(dev.source, CandidateSource::Project);
        assert_eq!(dev.kind, CandidateKind::Command);

        // After `-i` the slot wants an identity file, not a host.
        assert!(!provider.applies(&context(directory.path(), "ssh -i ", 2)));

        // scp preserves a typed `user@` prefix.
        let scp = context(directory.path(), "scp file admin@de", 3);
        let output = provider.complete(&scp);
        let dev = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "admin@dev-box")
            .expect("scp row");
        assert_eq!(
            dev.edit.as_ref().expect("edit").replacement,
            "admin@dev-box"
        );
    }
}
