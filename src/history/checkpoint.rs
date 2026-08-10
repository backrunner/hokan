use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use nix::fcntl::OFlag;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::shell::ShellKind;

const CHECKPOINT_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImportCheckpoints {
    version: u32,
    sources: Vec<ImportCheckpoint>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImportCheckpoint {
    shell: ShellKind,
    path: PathBuf,
    device: u64,
    inode: u64,
    size: u64,
    modified_ns: i128,
}

#[derive(Clone, Debug)]
pub struct ImportSourceState {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub modified_ns: i128,
}

impl Default for ImportCheckpoint {
    fn default() -> Self {
        Self {
            shell: ShellKind::Zsh,
            path: PathBuf::new(),
            device: 0,
            inode: 0,
            size: 0,
            modified_ns: 0,
        }
    }
}

impl ImportCheckpoints {
    pub fn load(path: &Path) -> crate::Result<Self> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits());
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    version: 1,
                    sources: Vec::new(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != nix::unistd::geteuid().as_raw() {
            return Err(crate::Error::History(format!(
                "{} must be a regular file owned by the current user",
                path.display()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(crate::Error::History(format!(
                "{} must have private permissions",
                path.display()
            )));
        }
        if metadata.len() > CHECKPOINT_MAX_BYTES {
            return Err(checkpoint_too_large(path));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(CHECKPOINT_MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > CHECKPOINT_MAX_BYTES {
            return Err(checkpoint_too_large(path));
        }
        let final_metadata = file.metadata()?;
        if !same_metadata(&metadata, &final_metadata) {
            return Err(crate::Error::History(format!(
                "{} changed while it was being read",
                path.display()
            )));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| crate::Error::History(format!("{} is not valid UTF-8", path.display())))?;
        let checkpoints: Self = toml::from_str(&text).map_err(|error| {
            let location = toml_error_location(&text, &error);
            crate::Error::History(format!(
                "invalid import checkpoint {}{location}",
                path.display()
            ))
        })?;
        if checkpoints.version != 1 {
            return Err(crate::Error::History(format!(
                "unsupported import checkpoint version {}",
                checkpoints.version
            )));
        }
        Ok(checkpoints)
    }

    #[must_use]
    pub fn start_offset(&self, shell: ShellKind, source: &ImportSourceState) -> u64 {
        self.sources
            .iter()
            .find(|checkpoint| checkpoint.shell == shell && checkpoint.path == source.path)
            .filter(|checkpoint| {
                checkpoint.device == source.device
                    && checkpoint.inode == source.inode
                    && source.size >= checkpoint.size
            })
            .map_or(0, |checkpoint| checkpoint.size)
    }

    #[must_use]
    pub fn is_unchanged(&self, shell: ShellKind, source: &ImportSourceState) -> bool {
        self.sources.iter().any(|checkpoint| {
            checkpoint.shell == shell
                && checkpoint.path == source.path
                && checkpoint.device == source.device
                && checkpoint.inode == source.inode
                && checkpoint.size == source.size
                && checkpoint.modified_ns == source.modified_ns
        })
    }

    pub fn update(&mut self, shell: ShellKind, source: &ImportSourceState) {
        let checkpoint = ImportCheckpoint {
            shell,
            path: source.path.clone(),
            device: source.device,
            inode: source.inode,
            size: source.size,
            modified_ns: source.modified_ns,
        };
        if let Some(existing) = self
            .sources
            .iter_mut()
            .find(|existing| existing.shell == shell && existing.path == source.path)
        {
            *existing = checkpoint;
        } else {
            self.sources.push(checkpoint);
            self.sources.sort_by(|left, right| {
                left.path
                    .cmp(&right.path)
                    .then_with(|| left.shell.name().cmp(right.shell.name()))
            });
        }
    }

    pub fn write_atomic(&self, path: &Path) -> crate::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| crate::Error::History("import checkpoint path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let text = toml::to_string_pretty(self)
            .map_err(|error| crate::Error::History(error.to_string()))?;
        if text.len() as u64 > CHECKPOINT_MAX_BYTES {
            return Err(checkpoint_too_large(path));
        }
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary
            .as_file_mut()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        temporary.write_all(text.as_bytes())?;
        temporary.flush()?;
        temporary.as_file_mut().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

impl ImportSourceState {
    pub fn inspect(path: &Path) -> crate::Result<Self> {
        let path = fs::canonicalize(path)?;
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() {
            return Err(crate::Error::History(format!(
                "{} is not a regular history file",
                path.display()
            )));
        }
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified_ns: i128::from(metadata.mtime()) * 1_000_000_000
                + i128::from(metadata.mtime_nsec()),
        })
    }

    #[must_use]
    pub fn same_file_version(&self, other: &Self) -> bool {
        self.path == other.path
            && self.device == other.device
            && self.inode == other.inode
            && self.size == other.size
            && self.modified_ns == other.modified_ns
    }

    pub fn read_from(&self, offset: u64, max_bytes: u64) -> crate::Result<Vec<u8>> {
        let remaining = self.size.checked_sub(offset).ok_or_else(|| {
            crate::Error::History("history import offset exceeds the source size".into())
        })?;
        if remaining > max_bytes {
            return Err(crate::Error::History(format!(
                "history import would read more than {} MiB from {}; compact or split the source",
                max_bytes / (1024 * 1024),
                self.path.display()
            )));
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits());
        let mut file = options.open(&self.path)?;
        let opened = file.metadata()?;
        if !self.matches_metadata(&opened) {
            return Err(crate::Error::History(format!(
                "{} changed before it could be read",
                self.path.display()
            )));
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::with_capacity(usize::try_from(remaining).unwrap_or(0));
        Read::by_ref(&mut file)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            return Err(crate::Error::History(format!(
                "history import exceeded the {} MiB read limit",
                max_bytes / (1024 * 1024)
            )));
        }
        let final_metadata = file.metadata()?;
        if !self.matches_metadata(&final_metadata) || bytes.len() as u64 != remaining {
            return Err(crate::Error::History(format!(
                "{} changed while it was being read",
                self.path.display()
            )));
        }
        Ok(bytes)
    }

    fn matches_metadata(&self, metadata: &fs::Metadata) -> bool {
        metadata.is_file()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
            && metadata.len() == self.size
            && i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec())
                == self.modified_ns
    }
}

fn checkpoint_too_large(path: &Path) -> crate::Error {
    crate::Error::History(format!(
        "{} exceeds the 1 MiB import checkpoint limit",
        path.display()
    ))
}

fn same_metadata(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
}

fn toml_error_location(text: &str, error: &toml::de::Error) -> String {
    error
        .span()
        .and_then(|span| text.get(..span.start))
        .map(|prefix| {
            let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
            let column = prefix
                .rsplit('\n')
                .next()
                .map_or(1, |tail| tail.chars().count() + 1);
            format!(" at line {line}, column {column}")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_append_truncate_and_rotation() {
        let directory = tempfile::tempdir().expect("directory");
        let source = directory.path().join("history");
        fs::write(&source, b"one\n").expect("source");
        let first = ImportSourceState::inspect(&source).expect("inspect first");
        let mut checkpoints = ImportCheckpoints {
            version: 1,
            ..ImportCheckpoints::default()
        };
        checkpoints.update(ShellKind::Bash, &first);
        assert!(checkpoints.is_unchanged(ShellKind::Bash, &first));

        fs::write(&source, b"one\ntwo\n").expect("append fixture");
        let appended = ImportSourceState::inspect(&source).expect("inspect append");
        assert_eq!(
            checkpoints.start_offset(ShellKind::Bash, &appended),
            first.size
        );

        fs::write(&source, b"x\n").expect("truncate fixture");
        let truncated = ImportSourceState::inspect(&source).expect("inspect truncate");
        assert_eq!(checkpoints.start_offset(ShellKind::Bash, &truncated), 0);

        let rotated_source = directory.path().join("history.old");
        fs::rename(&source, rotated_source).expect("rotate old source");
        fs::write(&source, b"rotated\n").expect("rotated source");
        let rotated = ImportSourceState::inspect(&source).expect("inspect rotation");
        assert_eq!(checkpoints.start_offset(ShellKind::Bash, &rotated), 0);
    }

    #[test]
    fn checkpoint_load_is_bounded_and_does_not_follow_symlinks() {
        let directory = tempfile::tempdir().expect("directory");
        let oversized = directory.path().join("oversized.toml");
        File::create(&oversized)
            .expect("oversized fixture")
            .set_len(CHECKPOINT_MAX_BYTES + 1)
            .expect("extend fixture");
        assert!(ImportCheckpoints::load(&oversized).is_err());

        let target = directory.path().join("target.toml");
        fs::write(&target, "version = 1\n").expect("target fixture");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("private target");
        let link = directory.path().join("imports.toml");
        std::os::unix::fs::symlink(&target, &link).expect("checkpoint symlink");
        assert!(ImportCheckpoints::load(&link).is_err());

        let fifo = directory.path().join("imports.fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("checkpoint FIFO");
        assert!(ImportCheckpoints::load(&fifo).is_err());
    }

    #[test]
    fn checkpoint_parse_errors_do_not_echo_source_values() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("imports.toml");
        let secret = "checkpoint-secret-value";
        fs::write(&path, format!("version = 1\nsecret = \"{secret}\"\n"))
            .expect("invalid checkpoint");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private checkpoint");
        let detail = ImportCheckpoints::load(&path)
            .expect_err("unknown field must fail")
            .to_string();
        assert!(detail.contains("invalid import checkpoint"));
        assert!(!detail.contains(secret));
    }
}
