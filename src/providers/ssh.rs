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

use globset::GlobBuilder;

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, SlotKind, TextEdit,
    },
    platform::CommandPathCache,
    providers::argument_progress,
    terminal::RiskLevel,
};

/// Commands whose first positional argument is a host.
const SSH_COMMANDS: &[&str] = &["ssh", "sftp", "mosh"];
/// A `~/.ssh/config` larger than this is almost certainly not a hand-written
/// host list; skip it rather than stall completion.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_INCLUDE_FILES: usize = 64;

pub struct SshHostProvider {
    commands: Option<Arc<CommandPathCache>>,
    cache: SshConfigCache,
    config: Option<PathBuf>,
}

impl SshHostProvider {
    #[must_use]
    pub fn new(commands: Arc<CommandPathCache>) -> Self {
        Self {
            commands: Some(commands),
            cache: SshConfigCache::default(),
            config: std::env::home_dir().map(|home| home.join(".ssh").join("config")),
        }
    }

    #[cfg(test)]
    fn for_config(config: PathBuf) -> Self {
        Self {
            // Unit tests exercise parsing and slot behavior independently of
            // the process PATH. Production construction always supplies the
            // shared executable cache.
            commands: None,
            cache: SshConfigCache::default(),
            config: Some(config),
        }
    }
}

impl CandidateProvider for SshHostProvider {
    fn id(&self) -> &'static str {
        "ssh-host"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        host_slot(context, self.commands.as_deref()).is_some()
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Some(slot) = host_slot(context, self.commands.as_deref()) else {
            return ProviderOutput::default();
        };
        let Some(config) = self.config.as_deref() else {
            return ProviderOutput::default();
        };
        let hosts = self.cache.hosts_from_config(config);
        let prefix = match slot {
            HostSlot::BareCommand => "",
            HostSlot::Argument => context.parsed.current_prefix.as_str(),
        };
        // A typed `user@` prefix is preserved so the row completes the host
        // part only (`scp user@ho` → `user@host`).
        let (user, host_prefix) = prefix
            .split_once('@')
            .map_or((String::new(), prefix), |(user, host)| {
                (format!("{user}@"), host)
            });
        let command = context.command().unwrap_or_default();
        let command_name = super::executable_basename(command);
        let scp_remote = command_name == "scp";
        let folded_prefix = host_prefix.to_lowercase();
        ProviderOutput {
            candidates: hosts
                .iter()
                .filter(|host| {
                    folded_prefix.is_empty() || host.to_lowercase().starts_with(&folded_prefix)
                })
                .map(|host| {
                    let completed = format!("{user}{host}");
                    let host_replacement = if scp_remote {
                        format!("{completed}:")
                    } else {
                        completed.clone()
                    };
                    let replacement = if slot == HostSlot::BareCommand {
                        format!("{command} {host_replacement}")
                    } else {
                        host_replacement
                    };
                    let resulting = crate::parser::apply_edit(
                        &context.buffer.text,
                        context.parsed.replacement.clone(),
                        &replacement,
                    )
                    .unwrap_or_else(|_| replacement.clone());
                    Candidate::new(
                        context.query_id,
                        if slot == HostSlot::BareCommand {
                            resulting
                        } else {
                            completed.clone()
                        },
                        "SSH 主机（~/.ssh/config）",
                        Some(TextEdit {
                            range: context.parsed.replacement.clone(),
                            replacement,
                            cursor_after: CursorPlacement::End,
                        }),
                        if scp_remote {
                            CandidateAction::InsertAndContinue {
                                next_slot: crate::completion::SlotKind::Path,
                            }
                        } else {
                            CandidateAction::Insert
                        },
                        CandidateSource::Project,
                        CandidateKind::Command,
                        if scp_remote {
                            Completeness::NeedsInput {
                                slot: crate::completion::SlotKind::Path,
                            }
                        } else {
                            Completeness::Runnable
                        },
                        RiskLevel::Low,
                        format!("ssh:host:{host}"),
                    )
                })
                .collect(),
            diagnostics: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostSlot {
    BareCommand,
    Argument,
}

/// Identify a configured-host slot only for an executable that the current
/// user can actually run. Required-host commands can offer a complete
/// invocation at their exact bare name; ordinary argument slots keep replacing
/// only the active word.
fn host_slot(context: &CompletionContext, commands: Option<&CommandPathCache>) -> Option<HostSlot> {
    if !crate::providers::effective_command_accepts_external(context) {
        return None;
    }
    if commands.is_some_and(|commands| {
        crate::providers::resolved_executable_path(context, commands).is_none()
    }) {
        return None;
    }
    let raw_command = context.command()?;
    let command = super::executable_basename(raw_command);
    if SSH_COMMANDS.contains(&command)
        && (crate::providers::command_position_open(context)
            || crate::providers::explicit_executable_path_position(context))
        && context.parsed.current_prefix == raw_command
    {
        return Some(HostSlot::BareCommand);
    }
    let (words, position) = argument_progress(context)?;
    let prefix = context.parsed.current_prefix.as_str();
    if SSH_COMMANDS.contains(&command) && at_host_slot(command, &words, position, prefix) {
        return Some(HostSlot::Argument);
    }
    // Every scp operand may be local or remote. Offer configured aliases next
    // to local paths until a slash or colon makes the user's intent explicit.
    if at_scp_candidate_slot(command, &words, position, prefix) {
        return Some(HostSlot::Argument);
    }
    None
}

fn at_scp_candidate_slot(command: &str, words: &[&str], position: usize, prefix: &str) -> bool {
    command == "scp"
        && !prefix.starts_with('-')
        && !prefix.contains(['/', ':'])
        && !flag_takes_value(command, words.get(position).copied().unwrap_or_default())
}

/// An explicit `user@host` prefix belongs exclusively to remote-host
/// completion. Plain aliases remain ambiguous and therefore keep local file
/// rows visible beside SSH host rows.
pub(crate) fn at_scp_host_slot(
    command: &str,
    words: &[&str],
    position: usize,
    prefix: &str,
) -> bool {
    command == "scp"
        && prefix.contains('@')
        && !prefix.contains(['/', ':'])
        && !flag_takes_value(command, words.get(position).copied().unwrap_or_default())
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
        if flag_has_attached_value(command, word) {
            index += 1;
        } else if flag_takes_value(command, word) {
            index += 2; // the flag consumes the next word as its value
        } else if word.starts_with('-') {
            index += 1;
        } else {
            return true;
        }
    }
    false
}

fn flag_has_attached_value(command: &str, word: &str) -> bool {
    if let Some((flag, _)) = word.split_once('=') {
        return flag_value_slot(command, flag).is_some();
    }
    attached_short_value_flags(command)
        .iter()
        .any(|flag| word.len() > flag.len() && word.starts_with(flag))
}

/// Flags after which a host row would be noise because the next word is the
/// flag's value. Mirrors the `flag_value_slot` table in the filesystem
/// provider for the ssh command family.
fn flag_takes_value(command: &str, flag: &str) -> bool {
    flag_value_slot(command, flag).is_some()
}

pub(crate) fn flag_value_slot(command: &str, flag: &str) -> Option<SlotKind> {
    if !flag.starts_with('-') {
        return None;
    }
    match (command, flag) {
        ("ssh", "-E" | "-F" | "-I" | "-i" | "-S")
        | ("sftp", "-b" | "-D" | "-F" | "-i" | "-S")
        | ("scp", "-D" | "-F" | "-i" | "-S")
        | ("mosh", "--client" | "--server") => Some(SlotKind::Path),
        (
            "ssh",
            "-B" | "-b" | "-c" | "-D" | "-e" | "-J" | "-L" | "-l" | "-m" | "-O" | "-o" | "-P"
            | "-p" | "-Q" | "-R" | "-W" | "-w",
        )
        | ("sftp", "-B" | "-c" | "-J" | "-l" | "-o" | "-P" | "-R" | "-s" | "-X")
        | ("scp", "-c" | "-J" | "-l" | "-o" | "-P" | "-X")
        | ("mosh", "-p" | "--port" | "--ssh" | "--predict" | "--bind-server") => {
            Some(SlotKind::Value)
        }
        _ => None,
    }
}

pub(crate) fn attached_short_value_flags(command: &str) -> &'static [&'static str] {
    match command {
        "ssh" => &[
            "-B", "-b", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J", "-L", "-l", "-m", "-O",
            "-o", "-P", "-p", "-Q", "-R", "-S", "-W", "-w",
        ],
        "sftp" => &[
            "-B", "-b", "-c", "-D", "-F", "-i", "-J", "-l", "-o", "-P", "-R", "-s", "-S", "-X",
        ],
        "scp" => &["-c", "-D", "-F", "-i", "-J", "-l", "-o", "-P", "-S", "-X"],
        "mosh" => &["-p"],
        _ => &[],
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
        let mut included_files = 0;
        'includes: for include in parse_config(&text).1 {
            for resolved in resolve_includes(path, &include) {
                if included_files >= MAX_INCLUDE_FILES {
                    break 'includes;
                }
                included_files += 1;
                for host in self.hosts_from(&resolved).iter() {
                    if !hosts.contains(host) {
                        hosts.push(host.clone());
                    }
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
        let line = strip_config_comment(line).trim_start();
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

fn strip_config_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        match (quote, character) {
            (None, '\'' | '"') => quote = Some(character),
            (Some(active), current) if active == current => quote = None,
            (None, '#') => return &line[..index],
            _ => {}
        }
    }
    line
}

/// Resolve a plain Include or a filename glob under a fixed directory.
/// Relative paths use the including file's directory. Wildcards in parent
/// directories stay unsupported so one config line cannot trigger a broad
/// recursive scan.
fn resolve_includes(config_path: &Path, include: &str) -> Vec<PathBuf> {
    let path = if let Some(rest) = include.strip_prefix("~/") {
        let Some(home) = std::env::home_dir() else {
            return Vec::new();
        };
        home.join(rest)
    } else {
        let path = PathBuf::from(include);
        if path.is_absolute() {
            path
        } else {
            let Some(directory) = config_path.parent() else {
                return Vec::new();
            };
            directory.join(path)
        }
    };
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    if !file_name.contains(['*', '?', '[', '{']) {
        return vec![path];
    }
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    if parent.to_string_lossy().contains(['*', '?', '[', '{']) {
        return Vec::new();
    }
    let Ok(glob) = GlobBuilder::new(file_name).literal_separator(true).build() else {
        return Vec::new();
    };
    let matcher = glob.compile_matcher();
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| matcher.is_match(entry.file_name()))
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    paths.truncate(MAX_INCLUDE_FILES);
    paths
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
             Host dev-box staging !secret qa-? # inline comment\n\
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
    fn include_filename_globs_are_expanded_in_stable_order() {
        let directory = tempfile::tempdir().expect("directory");
        let includes = directory.path().join("config.d");
        fs::create_dir(&includes).expect("include directory");
        let nested = directory.path().join("nested");
        fs::write(&nested, "Host from-nested\n").expect("nested config");
        fs::write(includes.join("20-staging.conf"), "Host staging\n").expect("staging config");
        fs::write(
            includes.join("10-dev.conf"),
            format!("Host dev\nInclude {}\n", nested.display()),
        )
        .expect("dev config");
        fs::write(includes.join("ignored.txt"), "Host ignored\n").expect("ignored config");
        let config = directory.path().join("config");
        fs::write(&config, "Include config.d/*.conf\n").expect("config");

        let hosts = SshConfigCache::default().hosts_from_config(&config);
        assert_eq!(hosts.as_slice(), ["dev", "staging"]);
        assert!(!hosts.iter().any(|host| host == "from-nested"));
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
        assert!(at("ssh", "ssh -p22 de"));
        assert!(at("ssh", "ssh -i~/.ssh/id_ed25519 de"));
        assert!(!at("ssh", "ssh -i "));
        assert!(!at("ssh", "ssh -p "));
        assert!(!at("ssh", "ssh -Q "));
        assert!(!at("ssh", "ssh -O check"));
        assert!(!at("sftp", "sftp -D "));
        assert!(!at("sftp", "sftp -c "));
        assert!(!at("sftp", "sftp -s "));
        assert!(!at("ssh", "ssh -"));
        assert!(!at("ssh", "ssh host1 de"));
        assert!(at("sftp", "sftp "));
        assert!(at("mosh", "mosh "));
        assert!(at("mosh", "mosh --ssh 'ssh -p 2222' de"));
        assert!(at("mosh", "mosh --port=60000 de"));
        assert!(!at("mosh", "mosh --ssh "));
        assert!(!at("scp", "scp "));
    }

    #[test]
    fn scp_offers_plain_and_user_qualified_hosts_at_every_operand() {
        let directory = tempfile::tempdir().expect("directory");
        let fires = |text: &str, query: u64| {
            host_slot(&context(directory.path(), text, query), None).is_some()
        };
        assert!(fires("scp ", 1));
        assert!(fires("scp de", 2));
        assert!(fires("scp file ", 3));
        assert!(fires("scp file de", 4));
        assert!(fires("scp file user@", 1));
        assert!(fires("scp file user@ho", 2));
        assert!(fires("scp user@ho", 3), "explicit remote first argument");
        assert!(!fires("scp user@host:/pa", 5), "a slash ends the host part");
        assert!(!fires("scp user@host:pa", 6), "a colon ends the host part");
        assert!(!fires("scp ./de", 7), "an explicit local path stays local");
        assert!(!fires("scp -P ", 8), "option value slots are not hosts");
        assert!(!fires("scp -J ", 9), "jump-host value is not an operand");
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
            "admin@dev-box:"
        );
        assert_eq!(
            dev.completeness,
            Completeness::NeedsInput {
                slot: crate::completion::SlotKind::Path,
            }
        );

        // Plain aliases are also offered; choosing one adds the remote-path
        // separator while local file completion remains available in parallel.
        let scp = context(directory.path(), "scp de", 4);
        let output = provider.complete(&scp);
        let dev = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "dev-box")
            .expect("plain scp host row");
        assert_eq!(dev.edit.as_ref().expect("edit").replacement, "dev-box:");
    }

    #[test]
    fn exact_required_host_command_completes_to_a_runnable_invocation() {
        let directory = tempfile::tempdir().expect("directory");
        let config = directory.path().join("config");
        fs::write(&config, "Host dev-box staging\n").expect("config");
        let provider = SshHostProvider::for_config(config);

        for text in ["ssh", "sudo ssh", "./bin/ssh"] {
            let context = context(directory.path(), text, 1);
            assert!(provider.applies(&context), "bare slot for {text:?}");
            let output = provider.complete(&context);
            let dev = output
                .candidates
                .iter()
                .find(|candidate| candidate.display.primary.ends_with("ssh dev-box"))
                .expect("complete invocation");
            let edit = dev.edit.as_ref().expect("edit");
            assert_eq!(
                crate::parser::apply_edit(
                    &context.buffer.text,
                    edit.range.clone(),
                    &edit.replacement
                )
                .expect("apply edit"),
                format!("{} dev-box", text)
            );
        }
    }

    #[test]
    fn production_provider_requires_a_real_executable() {
        use std::{ffi::OsString, os::unix::fs::PermissionsExt};

        let directory = tempfile::tempdir().expect("directory");
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        let config = directory.path().join("config");
        fs::write(&config, "Host dev-box\n").expect("config");
        let plain = bin.join("ssh");
        fs::write(&plain, b"#!/bin/sh\n").expect("plain file");
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o600)).expect("plain mode");

        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(&bin))));
        let provider = SshHostProvider {
            commands: Some(Arc::clone(&commands)),
            cache: SshConfigCache::default(),
            config: Some(config),
        };
        assert!(!provider.applies(&context(directory.path(), "ssh ", 1)));

        fs::set_permissions(&plain, fs::Permissions::from_mode(0o700)).expect("executable mode");
        commands.refresh_from_path(Some(&OsString::from(&bin)));
        assert!(provider.applies(&context(directory.path(), "ssh ", 2)));
    }
}
