use std::{collections::HashSet, io::Write, path::Path};

use super::HistoryCommand;
use crate::{
    config::{Config, ConfigPaths},
    history::{
        HistoryEventV1, HistoryPolicy, HistoryStore, ImportCheckpoints, ImportSourceState,
        default_history_path, parse_history,
    },
    shell::ShellKind,
};

const HISTORY_IMPORT_MAX_BYTES: u64 = 64 * 1024 * 1024;

pub fn run(
    output: &mut dyn Write,
    command: HistoryCommand,
    configured_shell: Option<ShellKind>,
) -> crate::Result<()> {
    let paths = ConfigPaths::discover()?;
    let store = HistoryStore::open(&paths.state_directory)?;
    match command {
        HistoryCommand::Import { shell, path } => {
            import(
                output,
                &store,
                shell.or(configured_shell),
                path.as_deref(),
                &paths,
            )?;
        }
        HistoryCommand::Stats { json } => write_stats(output, &store, json)?,
        HistoryCommand::Prune { keep } => prune(output, &store, keep)?,
        HistoryCommand::Repair => repair(output, &store)?,
        HistoryCommand::Compact => compact(output, &store)?,
        HistoryCommand::Clear { yes } => clear(output, &store, yes)?,
    }
    Ok(())
}

fn import(
    output: &mut dyn Write,
    store: &HistoryStore,
    shell: Option<ShellKind>,
    explicit_path: Option<&Path>,
    paths: &ConfigPaths,
) -> crate::Result<()> {
    let shell = shell.map_or_else(ShellKind::detect, Ok)?;
    let path = explicit_path
        .map(Path::to_owned)
        .or_else(|| default_history_path(shell));
    let path = path.ok_or_else(|| {
        crate::Error::History(format!("cannot determine the {shell} history path"))
    })?;
    let source = ImportSourceState::inspect(&path)?;
    let checkpoint_path = paths.state_directory.join("imports.toml");
    let mut checkpoints = ImportCheckpoints::load(&checkpoint_path)?;
    if checkpoints.is_unchanged(shell, &source) {
        writeln!(output, "source: {}", source.path.display())?;
        writeln!(output, "unchanged: yes")?;
        writeln!(output, "imported: 0")?;
        return Ok(());
    }
    let start_offset = checkpoints.start_offset(shell, &source);
    let bytes = source.read_from(start_offset, HISTORY_IMPORT_MAX_BYTES)?;
    let entries = parse_history(shell, &bytes);
    let config = Config::load(&paths.config_file)?;
    let policy = HistoryPolicy::new(config.history.max_command_bytes, &config.history.exclude)?;

    let mut existing = store.read()?;
    if existing.torn_tail && existing.corrupt_offset.is_none() {
        store.repair_torn_tail()?;
        existing = store.read()?;
    }
    if let Some(offset) = existing.corrupt_offset {
        return Err(crate::Error::History(format!(
            "history store is corrupt at byte {offset}; preserve it and run repair before importing"
        )));
    }
    let mut known: HashSet<_> = existing
        .events
        .iter()
        .filter(|event| event.imported && event.shell == shell)
        .map(|event| (event.timestamp_ms, event.command.clone()))
        .collect();

    let mut imported = 0_usize;
    let mut filtered = 0_usize;
    let mut duplicates = 0_usize;
    let mut pending = Vec::<HistoryEventV1>::new();
    let mut pending_keys = std::collections::HashMap::<(i64, String), usize>::new();
    for entry in entries {
        if !policy.allows(&entry.command) {
            filtered = filtered.saturating_add(1);
            continue;
        }
        let timestamp_ms = entry.timestamp_ms.unwrap_or(0);
        let key = (timestamp_ms, entry.command.clone());
        if known.contains(&key) {
            duplicates = duplicates.saturating_add(1);
            continue;
        }
        if let Some(index) = pending_keys.get(&key).copied() {
            pending[index].occurrences = pending[index].occurrences.saturating_add(1);
            imported = imported.saturating_add(1);
            continue;
        }
        known.insert(key.clone());
        pending_keys.insert(key, pending.len());
        pending.push(HistoryEventV1 {
            event_id: None,
            timestamp_ms,
            command: entry.command,
            cwd: None,
            shell,
            exit_code: None,
            imported: true,
            occurrences: 1,
        });
        imported = imported.saturating_add(1);
    }
    store.append_many(&pending)?;

    let source_after = ImportSourceState::inspect(&source.path)?;
    let checkpoint_saved = source.same_file_version(&source_after);
    if checkpoint_saved {
        checkpoints.update(shell, &source_after);
        checkpoints.write_atomic(&checkpoint_path)?;
    }
    writeln!(output, "source: {}", source.path.display())?;
    writeln!(output, "start offset: {start_offset}")?;
    writeln!(output, "imported: {imported}")?;
    writeln!(output, "duplicates skipped: {duplicates}")?;
    writeln!(output, "privacy filtered: {filtered}")?;
    writeln!(
        output,
        "checkpoint: {}",
        if checkpoint_saved {
            "saved"
        } else {
            "deferred"
        }
    )?;
    Ok(())
}

fn write_stats(output: &mut dyn Write, store: &HistoryStore, json: bool) -> crate::Result<()> {
    let stats = store.stats()?;
    let report = store.read()?;
    if json {
        serde_json::to_writer_pretty(
            &mut *output,
            &serde_json::json!({
                "path": store.path(),
                "events": stats.events,
                "records": stats.records,
                "bytes": stats.bytes,
                "torn_tail": stats.torn_tail,
                "snapshot_corrupt": stats.snapshot_corrupt,
                "corrupt_offset": report.corrupt_offset,
            }),
        )?;
        writeln!(output)?;
    } else {
        writeln!(output, "path: {}", store.path().display())?;
        writeln!(output, "events: {}", stats.events)?;
        writeln!(output, "records: {}", stats.records)?;
        writeln!(output, "bytes: {}", stats.bytes)?;
        writeln!(output, "torn tail: {}", yes_no(stats.torn_tail))?;
        writeln!(
            output,
            "snapshot corrupt: {}",
            yes_no(stats.snapshot_corrupt)
        )?;
        match report.corrupt_offset {
            Some(offset) => writeln!(output, "corrupt offset: {offset}")?,
            None => writeln!(output, "corrupt offset: none")?,
        }
    }
    Ok(())
}

fn prune(output: &mut dyn Write, store: &HistoryStore, keep: usize) -> crate::Result<()> {
    let report = store.read()?;
    if let Some(offset) = report.corrupt_offset {
        return Err(crate::Error::History(format!(
            "refusing to prune a corrupt store at byte {offset}"
        )));
    }
    let before = report.events.len();
    let start = before.saturating_sub(keep);
    store.rewrite(&report.events[start..])?;
    writeln!(output, "kept: {}", before.saturating_sub(start))?;
    writeln!(output, "removed: {start}")?;
    Ok(())
}

fn repair(output: &mut dyn Write, store: &HistoryStore) -> crate::Result<()> {
    let report = store.read()?;
    if report.snapshot_corrupt || report.corrupt_offset.is_some() {
        let backups = store.quarantine_corrupt()?;
        for backup in backups {
            writeln!(output, "quarantined: {}", backup.display())?;
        }
    }
    let removed = store.repair_torn_tail()?;
    writeln!(output, "removed torn bytes: {removed}")?;
    Ok(())
}

fn compact(output: &mut dyn Write, store: &HistoryStore) -> crate::Result<()> {
    let report = store.compact()?;
    writeln!(output, "records before: {}", report.records_before)?;
    writeln!(output, "records after: {}", report.records_after)?;
    writeln!(output, "logical events: {}", report.logical_events)?;
    writeln!(output, "bytes before: {}", report.bytes_before)?;
    writeln!(output, "bytes after: {}", report.bytes_after)?;
    Ok(())
}

fn clear(output: &mut dyn Write, store: &HistoryStore, yes: bool) -> crate::Result<()> {
    if !yes {
        return Err(crate::Error::History(
            "refusing to clear without --yes; shell history files are never modified".into(),
        ));
    }
    let original = store.path().to_owned();
    let existed = original.exists();
    store.clear()?;
    if existed {
        writeln!(
            output,
            "moved Hokann history to {}",
            original.with_extension("events.cleared").display()
        )?;
    } else {
        writeln!(output, "Hokann history is already empty")?;
    }
    Ok(())
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn import_checkpoint_handles_append_unchanged_and_truncate() {
        let directory = tempfile::tempdir().expect("directory");
        let source = directory.path().join("bash_history");
        fs::write(&source, b"echo one\n").expect("initial history");
        let paths = ConfigPaths {
            config_file: directory.path().join("config/config.toml"),
            credentials_file: directory.path().join("config/credentials.toml"),
            specs_directory: directory.path().join("config/specs"),
            state_directory: directory.path().join("state"),
            cache_directory: directory.path().join("cache"),
        };
        let store = HistoryStore::open(&paths.state_directory).expect("store");

        let mut output = Vec::new();
        import(
            &mut output,
            &store,
            Some(ShellKind::Bash),
            Some(&source),
            &paths,
        )
        .expect("initial import");
        assert_eq!(store.stats().expect("stats").events, 1);

        fs::write(&source, b"echo one\necho two\n").expect("appended history");
        output.clear();
        import(
            &mut output,
            &store,
            Some(ShellKind::Bash),
            Some(&source),
            &paths,
        )
        .expect("tail import");
        assert_eq!(store.stats().expect("stats").events, 2);

        output.clear();
        import(
            &mut output,
            &store,
            Some(ShellKind::Bash),
            Some(&source),
            &paths,
        )
        .expect("unchanged import");
        assert!(
            String::from_utf8(output.clone())
                .expect("UTF-8")
                .contains("unchanged: yes")
        );
        assert_eq!(store.stats().expect("stats").events, 2);

        fs::write(&source, b"echo one\n").expect("truncated history");
        output.clear();
        import(
            &mut output,
            &store,
            Some(ShellKind::Bash),
            Some(&source),
            &paths,
        )
        .expect("truncated import");
        assert_eq!(store.stats().expect("stats").events, 2);
    }
}
