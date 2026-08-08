use std::{
    io::{Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{Arc, RwLock},
    time::Duration,
};

use crate::{
    completion::CompletionEngine,
    config::{Config, ConfigPaths},
    history::{
        HistoryCursor, HistoryIndex, HistoryPolicy, HistoryStore, default_history_path,
        parse_history,
    },
    platform::CommandPathCache,
    project::ProjectCache,
    providers::{
        AiActionProvider, CommandHelpCache, CommandHelpProvider, CommandSpecProvider,
        FilesystemProvider, HistoryProvider, NetworkInterfaceProvider, PathCommandProvider,
        ProcessProvider, ProjectProvider,
    },
    shell::{AliasCache, ShellKind},
    specs::SpecRegistry,
};
use nix::fcntl::OFlag;

const SHELL_HISTORY_STARTUP_MAX_BYTES: u64 = 8 * 1024 * 1024;

type EngineParts = (
    Arc<CompletionEngine>,
    Arc<SpecRegistry>,
    Arc<CommandPathCache>,
    Arc<CommandHelpCache>,
    Arc<AliasCache>,
);

pub(super) fn load_history(
    paths: &ConfigPaths,
    config: &Config,
    shell: ShellKind,
) -> crate::Result<(
    HistoryStore,
    Arc<RwLock<HistoryIndex>>,
    HistoryPolicy,
    HistoryCursor,
)> {
    let store = HistoryStore::open(&paths.state_directory)?;
    let policy = HistoryPolicy::new(config.history.max_command_bytes, &config.history.exclude)?;
    let mut index = HistoryIndex::default();
    let (mut report, mut cursor) = store.read_with_cursor()?;
    if report.snapshot_corrupt || report.corrupt_offset.is_some() {
        store.quarantine_corrupt()?;
        (report, cursor) = store.read_with_cursor()?;
    }
    if report.torn_tail {
        store.repair_torn_tail()?;
        (report, cursor) = store.read_with_cursor()?;
    }
    for event in report.events {
        index.ingest_weighted(
            &event.command,
            event.timestamp_ms,
            event.shell,
            event.cwd.as_deref(),
            event.occurrences,
            event.exit_code,
            &policy,
        );
    }
    if config.history.enabled
        && let Some(path) = default_history_path(shell)
        && let Ok(bytes) = read_history_tail(&path, SHELL_HISTORY_STARTUP_MAX_BYTES)
    {
        let now = crate::history_now_ms();
        // Ingest in chronological order so the transition bigram learns the
        // real command sequences; the timestamp assignment matches the old
        // newest-first enumeration exactly.
        let imported = parse_history(shell, &bytes);
        let total = imported.len();
        for (offset, imported) in imported.into_iter().enumerate() {
            let timestamp = imported
                .timestamp_ms
                .unwrap_or_else(|| now.saturating_sub((total - 1 - offset) as i64));
            index.ingest(&imported.command, timestamp, shell, None, None, &policy);
        }
    }
    Ok((store, Arc::new(RwLock::new(index)), policy, cursor))
}

pub(super) fn read_history_tail(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    if max_bytes == 0 {
        return Ok(Vec::new());
    }
    let path = std::fs::canonicalize(path)?;
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits());
    let mut file = options.open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a regular history file", path.display()),
        ));
    }
    let length = metadata.len();
    let logical_start = length.saturating_sub(max_bytes);
    let read_start = logical_start.saturating_sub(u64::from(logical_start > 0));
    file.seek(SeekFrom::Start(read_start))?;
    let read_limit = max_bytes.saturating_add(u64::from(logical_start > 0));
    let capacity = usize::try_from(read_limit.min(length)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(read_limit)
        .read_to_end(&mut bytes)?;

    if logical_start > 0 {
        if bytes.first() == Some(&b'\n') {
            bytes.remove(0);
        } else if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        } else {
            bytes.clear();
        }
    }
    Ok(bytes)
}

pub(super) fn build_engine(
    paths: &ConfigPaths,
    config: &Arc<Config>,
    history: Arc<RwLock<HistoryIndex>>,
    commands: Option<Arc<CommandPathCache>>,
) -> EngineParts {
    let specs = Arc::new(SpecRegistry::load(Some(&paths.specs_directory)));
    let commands = commands.unwrap_or_else(|| Arc::new(CommandPathCache::from_environment()));
    let projects = Arc::new(ProjectCache::default());
    let help = Arc::new(CommandHelpCache::default());
    let git_status = Arc::new(crate::project::GitStatusCache::default());
    let aliases = Arc::new(AliasCache::default());
    let mut engine = CompletionEngine::new(config.completion.max_candidates, config.ui.max_rows)
        .with_local_timeout(Duration::from_millis(config.completion.local_timeout_ms));
    engine.register(CommandSpecProvider::new(
        Arc::clone(&specs),
        Arc::clone(&commands),
    ));
    engine.register(ProjectProvider::new(
        projects,
        Arc::clone(&commands),
        Arc::clone(&history),
    ));
    engine.register(crate::providers::GitProvider::new(
        git_status,
        Arc::new(crate::project::GitRefsCache::default()),
        Arc::clone(&commands),
    ));
    engine.register(crate::providers::SshHostProvider::new());
    engine.register(PathCommandProvider::new(Arc::clone(&commands)));
    if config.history.enabled {
        engine.register(HistoryProvider::new(
            Arc::clone(&history),
            Arc::clone(&commands),
            Arc::clone(&aliases),
            Arc::clone(&specs),
            Arc::clone(&help),
        ));
    }
    // Full-line history continuation must run before providers that scan a
    // function directory, process table, or network interfaces. A slow local
    // source must never consume the query budget before `proj skillscat` can
    // continue `proj `.
    engine.register(crate::providers::AliasProvider::new(Arc::clone(&aliases)));
    engine.register(ProcessProvider);
    engine.register(NetworkInterfaceProvider::new(Arc::clone(&commands)));
    engine.register(CommandHelpProvider::new(
        Arc::clone(&specs),
        Arc::clone(&commands),
        Arc::clone(&help),
    ));
    // Directory scans have the largest local latency budget. Keep semantic,
    // PATH, and history providers ahead of them so a large cwd cannot starve
    // the rows that already know what the active slot means.
    engine.register(FilesystemProvider::new(
        config.ui.show_hidden,
        Arc::clone(&specs),
        Arc::clone(&help),
        Arc::clone(&aliases),
    ));
    engine.register(AiActionProvider::new(
        Arc::new(config.ai.clone()),
        crate::config::configured_credential_available(&config.ai, &paths.credentials_file),
        Arc::clone(&commands),
        Arc::clone(&specs),
        Arc::clone(&aliases),
    ));
    (Arc::new(engine), specs, commands, help, aliases)
}
