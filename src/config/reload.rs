use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use super::Config;

const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub enum ConfigReload {
    Unchanged,
    Loaded(Box<Config>),
    Invalid(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    length: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug)]
pub struct ConfigWatcher {
    config_path: PathBuf,
    credential_path: PathBuf,
    observed_config: Option<FileStamp>,
    observed_credential: Option<FileStamp>,
    next_poll: Instant,
}

impl ConfigWatcher {
    #[must_use]
    pub fn new(config_path: PathBuf, credential_path: PathBuf, now: Instant) -> Self {
        Self {
            observed_config: file_stamp(&config_path),
            observed_credential: file_stamp(&credential_path),
            config_path,
            credential_path,
            next_poll: now + POLL_INTERVAL,
        }
    }

    pub fn poll(&mut self, now: Instant) -> ConfigReload {
        if now < self.next_poll {
            return ConfigReload::Unchanged;
        }
        self.next_poll = now + POLL_INTERVAL;
        let config_stamp = file_stamp(&self.config_path);
        let credential_stamp = file_stamp(&self.credential_path);
        if config_stamp == self.observed_config && credential_stamp == self.observed_credential {
            return ConfigReload::Unchanged;
        }
        self.observed_config = config_stamp;
        self.observed_credential = credential_stamp;
        match Config::load(&self.config_path) {
            Ok(config) => ConfigReload::Loaded(Box::new(config)),
            Err(error) => ConfigReload::Invalid(error.to_string()),
        }
    }

    pub fn watch_credential_path(&mut self, path: PathBuf) {
        if self.credential_path != path {
            self.observed_credential = file_stamp(&path);
            self.credential_path = path;
        }
    }
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(FileStamp {
            length: metadata.len(),
            modified: metadata.modified().ok(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Some(FileStamp {
            length: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reloads_valid_changes_and_reports_each_invalid_version_once() {
        let directory = tempfile::tempdir().expect("directory");
        let config_path = directory.path().join("config.toml");
        let credential_path = directory.path().join("credentials.toml");
        let started = Instant::now();
        let mut watcher = ConfigWatcher::new(config_path.clone(), credential_path.clone(), started);
        assert!(matches!(
            watcher.poll(started + POLL_INTERVAL),
            ConfigReload::Unchanged
        ));

        fs::write(&config_path, "not = [valid").expect("invalid config");
        assert!(matches!(
            watcher.poll(started + POLL_INTERVAL * 2),
            ConfigReload::Invalid(_)
        ));
        assert!(matches!(
            watcher.poll(started + POLL_INTERVAL * 3),
            ConfigReload::Unchanged
        ));

        let mut config = Config::default();
        config.ui.max_rows = 7;
        config.write_atomic(&config_path).expect("valid config");
        assert!(matches!(
            watcher.poll(started + POLL_INTERVAL * 4),
            ConfigReload::Loaded(loaded) if loaded.ui.max_rows == 7
        ));

        fs::write(&credential_path, "version = 1\napi_key = 'x'\n").expect("credential change");
        assert!(matches!(
            watcher.poll(started + POLL_INTERVAL * 5),
            ConfigReload::Loaded(_)
        ));
    }
}
