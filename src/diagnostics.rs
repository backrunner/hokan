use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use fs2::FileExt;
use nix::fcntl::OFlag;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::{config::LoggingConfig, shell::ShellKind, terminal::TerminalSize};

const LOG_NAME: &str = "debug.log";
const LOCK_NAME: &str = "debug.lock";
const MAX_DETAIL_CHARS: usize = 512;

#[derive(Clone, Debug)]
pub struct DebugLog {
    path: PathBuf,
    lock_path: PathBuf,
    max_bytes: u64,
    rotations: usize,
}

impl DebugLog {
    pub fn from_config(
        state_directory: &Path,
        config: &LoggingConfig,
    ) -> crate::Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        fs::create_dir_all(state_directory)?;
        fs::set_permissions(state_directory, fs::Permissions::from_mode(0o700))?;
        let log = Self {
            path: state_directory.join(LOG_NAME),
            lock_path: state_directory.join(LOCK_NAME),
            max_bytes: config.max_bytes,
            rotations: config.rotations,
        };
        open_private(&log.lock_path, false)?;
        open_private(&log.path, true)?;
        Ok(Some(log))
    }

    pub fn session_started(&self, shell: ShellKind, size: TerminalSize) {
        self.record(
            "session-started",
            [
                ("shell", Value::String(shell.name().into())),
                ("rows", json!(size.rows)),
                ("cols", json!(size.cols)),
            ],
        );
    }

    pub fn session_finished(&self, exit_code: u8) {
        self.record("session-finished", [("exit_code", json!(exit_code))]);
    }

    pub fn provider_finished(
        &self,
        provider: &'static str,
        duration: Duration,
        candidate_count: usize,
        cancelled: bool,
    ) {
        self.record(
            "provider-finished",
            [
                ("provider", Value::String(provider.into())),
                (
                    "duration_us",
                    json!(duration.as_micros().min(u64::MAX as u128) as u64),
                ),
                ("candidate_count", json!(candidate_count)),
                ("cancelled", json!(cancelled)),
            ],
        );
    }

    pub fn ai_event(&self, outcome: &'static str) {
        self.record("ai-request", [("outcome", Value::String(outcome.into()))]);
    }

    pub fn config_reload(&self, outcome: &'static str, detail: Option<&str>) {
        let mut fields = vec![("outcome", Value::String(outcome.into()))];
        if let Some(detail) = detail {
            fields.push(("detail", Value::String(redact_text(detail))));
        }
        self.record_values("config-reload", fields);
    }

    fn record<const N: usize>(&self, event: &'static str, fields: [(&'static str, Value); N]) {
        self.record_values(event, fields);
    }

    fn record_values(
        &self,
        event: &'static str,
        fields: impl IntoIterator<Item = (&'static str, Value)>,
    ) {
        let mut safe_fields = Map::new();
        for (name, value) in fields {
            safe_fields.insert(name.into(), value);
        }
        let record = json!({
            "timestamp_ms": crate::history_now_ms(),
            "event": event,
            "fields": safe_fields,
        });
        let Ok(mut line) = serde_json::to_vec(&record) else {
            return;
        };
        line.push(b'\n');
        let _ = self.write_line(&line);
    }

    fn write_line(&self, line: &[u8]) -> crate::Result<()> {
        let lock = open_private(&self.lock_path, false)?;
        lock.lock_exclusive()?;
        let result = (|| -> crate::Result<()> {
            let current_size = regular_file_size(&self.path)?;
            if current_size.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX))
                > self.max_bytes
            {
                self.rotate()?;
            }
            let mut file = open_private(&self.path, true)?;
            file.write_all(line)?;
            file.flush()?;
            Ok(())
        })();
        let unlock = FileExt::unlock(&lock);
        result?;
        unlock?;
        Ok(())
    }

    fn rotate(&self) -> crate::Result<()> {
        for index in (1..=self.rotations).rev() {
            let destination = rotated_path(&self.path, index);
            if index == self.rotations && destination.exists() {
                ensure_regular_path(&destination)?;
                fs::remove_file(&destination)?;
            }
            let source = if index == 1 {
                self.path.clone()
            } else {
                rotated_path(&self.path, index - 1)
            };
            if source.exists() {
                ensure_regular_path(&source)?;
                fs::rename(source, destination)?;
            }
        }
        Ok(())
    }
}

fn open_private(path: &Path, append: bool) -> crate::Result<File> {
    let mut options = OpenOptions::new();
    options
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits());
    if append {
        options.append(true);
    }
    let file = options.open(path)?;
    ensure_private_file(&file, path)?;
    Ok(file)
}

fn ensure_private_file(file: &File, path: &Path) -> crate::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(crate::Error::Config(format!(
            "debug log path must be a private regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn regular_file_size(path: &Path) -> crate::Result<u64> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(metadata.len()),
        Ok(_) => Err(crate::Error::Config(format!(
            "debug log path is not a regular file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn ensure_regular_path(path: &Path) -> crate::Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_file() {
        Ok(())
    } else {
        Err(crate::Error::Config(format!(
            "debug log rotation path is not a regular file: {}",
            path.display()
        )))
    }
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    path.with_file_name(format!("{LOG_NAME}.{index}"))
}

fn redact_text(value: &str) -> String {
    static PATTERNS: OnceLock<Option<(Regex, Regex, Regex)>> = OnceLock::new();
    let Some((authorization, named_secret, key_like)) = PATTERNS.get_or_init(|| {
        Some((
            Regex::new(r"(?i)(authorization\s*[:=]\s*)[^\r\n]+").ok()?,
            Regex::new(
                r#"(?i)\b(api[_-]?key|token|password|secret)\s*[:=]\s*(?:"[^"\r\n]*"|'[^'\r\n]*'|[^\s,;]+)"#,
            )
            .ok()?,
            Regex::new(r"(?i)\bsk-[A-Za-z0-9._-]+\b").ok()?,
        ))
    }) else {
        return value
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .take(MAX_DETAIL_CHARS)
            .collect();
    };
    let value = authorization.replace_all(value, "$1<redacted>");
    let value = named_secret.replace_all(&value, "$1=<redacted>");
    let value = key_like.replace_all(&value, "<redacted>");
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAX_DETAIL_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_logging_creates_nothing() {
        let directory = tempfile::tempdir().expect("directory");
        let state = directory.path().join("state");
        assert!(
            DebugLog::from_config(&state, &LoggingConfig::default())
                .expect("disabled logger")
                .is_none()
        );
        assert!(!state.exists());
    }

    #[test]
    fn writes_private_bounded_redacted_json_lines() {
        let directory = tempfile::tempdir().expect("directory");
        let config = LoggingConfig {
            enabled: true,
            max_bytes: 1_024,
            rotations: 2,
        };
        let log = DebugLog::from_config(directory.path(), &config)
            .expect("logger")
            .expect("enabled logger");
        for _ in 0..20 {
            log.config_reload(
                "invalid",
                Some(
                    "Authorization: Basic basic-secret\nAuthorization = \"Digest digest-secret\"\npassword=\"two words\" token='three words' api_key=other sk-third\nnext",
                ),
            );
        }
        let current = fs::read_to_string(directory.path().join(LOG_NAME)).expect("current log");
        let rotated =
            fs::read_to_string(directory.path().join("debug.log.1")).expect("rotated log");
        for text in [&current, &rotated] {
            assert!(!text.contains("basic-secret"));
            assert!(!text.contains("digest-secret"));
            assert!(!text.contains("two words"));
            assert!(!text.contains("three words"));
            assert!(!text.contains("other"));
            assert!(!text.contains("sk-third"));
            for line in text.lines() {
                let value: serde_json::Value = serde_json::from_str(line).expect("JSON line");
                assert_eq!(value["event"], "config-reload");
            }
        }
        assert!(directory.path().join("debug.log.2").exists());
        assert_eq!(
            fs::metadata(directory.path().join(LOG_NAME))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
