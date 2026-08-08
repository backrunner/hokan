use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use nix::unistd::{AccessFlags, access};

#[derive(Debug, Default)]
pub struct CommandPathCache {
    commands: RwLock<HashMap<String, PathBuf>>,
}

impl Clone for CommandPathCache {
    fn clone(&self) -> Self {
        Self {
            commands: RwLock::new(read(&self.commands).clone()),
        }
    }
}

impl CommandPathCache {
    #[must_use]
    pub fn from_environment() -> Self {
        Self::from_path(env::var_os("PATH").as_deref())
    }

    #[must_use]
    pub fn from_path(path: Option<&std::ffi::OsStr>) -> Self {
        Self {
            commands: RwLock::new(scan_path(path)),
        }
    }

    /// Replace the cache with a snapshot of the child shell's current PATH.
    /// Returns true only when the executable set or resolution order changed.
    pub fn refresh_from_path(&self, path: Option<&std::ffi::OsStr>) -> bool {
        let scanned = scan_path(path);
        let mut commands = write(&self.commands);
        if *commands == scanned {
            return false;
        }
        *commands = scanned;
        true
    }

    #[must_use]
    pub fn contains(&self, command: &str) -> bool {
        read(&self.commands).contains_key(command)
    }

    #[must_use]
    pub fn has_prefix(&self, prefix: &str) -> bool {
        read(&self.commands)
            .keys()
            .any(|command| command.starts_with(prefix))
    }

    #[must_use]
    pub fn has_longer_prefix(&self, prefix: &str) -> bool {
        read(&self.commands)
            .keys()
            .any(|command| command.len() > prefix.len() && command.starts_with(prefix))
    }

    pub fn names(&self) -> Vec<String> {
        read(&self.commands).keys().cloned().collect()
    }

    #[must_use]
    pub fn path(&self, command: &str) -> Option<PathBuf> {
        read(&self.commands).get(command).cloned()
    }
}

fn scan_path(path: Option<&std::ffi::OsStr>) -> HashMap<String, PathBuf> {
    let mut commands = HashMap::new();
    let mut visited = HashSet::new();
    for directory in path.into_iter().flat_map(env::split_paths) {
        let canonical = fs::canonicalize(&directory).unwrap_or(directory);
        if !visited.insert(canonical.clone()) {
            continue;
        }
        let Ok(entries) = fs::read_dir(canonical) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if !commands.contains_key(name) && is_executable(&path) {
                commands.insert(name.to_owned(), path);
            }
        }
    }
    commands
}

fn read(
    commands: &RwLock<HashMap<String, PathBuf>>,
) -> RwLockReadGuard<'_, HashMap<String, PathBuf>> {
    commands.read().unwrap_or_else(PoisonError::into_inner)
}

fn write(
    commands: &RwLock<HashMap<String, PathBuf>>,
) -> RwLockWriteGuard<'_, HashMap<String, PathBuf>> {
    commands.write().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
        && access(path, AccessFlags::X_OK).is_ok()
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        io::Write,
        os::unix::fs::{PermissionsExt, symlink},
    };

    use super::*;

    #[test]
    fn scans_each_path_directory_once() {
        let directory = tempfile::tempdir().expect("temp directory");
        let executable = directory.path().join("demo-command");
        fs::File::create(&executable)
            .expect("create executable")
            .write_all(b"#!/bin/sh\n")
            .expect("write executable");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("set executable mode");
        let path = OsString::from(format!(
            "{}:{}",
            directory.path().display(),
            directory.path().display()
        ));
        let cache = CommandPathCache::from_path(Some(&path));
        assert!(cache.contains("demo-command"));
        assert_eq!(
            cache
                .names()
                .iter()
                .filter(|name| name.as_str() == "demo-command")
                .count(),
            1
        );
    }

    #[test]
    fn indexes_only_regular_files_executable_by_the_current_user() {
        let directory = tempfile::tempdir().expect("temp directory");
        let runnable = directory.path().join("runnable");
        let plain = directory.path().join("plain-file");
        let child_directory = directory.path().join("directory-entry");
        for (path, mode) in [(&runnable, 0o700), (&plain, 0o600)] {
            fs::write(path, b"#!/bin/sh\n").expect("write PATH entry");
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set mode");
        }
        fs::create_dir(&child_directory).expect("child directory");
        symlink(&runnable, directory.path().join("runnable-link")).expect("executable symlink");
        symlink(&plain, directory.path().join("plain-link")).expect("plain symlink");
        symlink("missing-target", directory.path().join("broken-link")).expect("broken symlink");

        let path = OsString::from(directory.path());
        let cache = CommandPathCache::from_path(Some(&path));
        assert!(cache.contains("runnable"));
        assert!(cache.contains("runnable-link"));
        for name in ["plain-file", "directory-entry", "plain-link", "broken-link"] {
            assert!(
                !cache.contains(name),
                "non-executable PATH entry leaked: {name}"
            );
        }

        if !nix::unistd::Uid::current().is_root() {
            let wrong_permission_class = directory.path().join("other-execute-only");
            fs::write(&wrong_permission_class, b"#!/bin/sh\n").expect("write mode fixture");
            fs::set_permissions(&wrong_permission_class, fs::Permissions::from_mode(0o001))
                .expect("set other-only execute mode");
            let cache = CommandPathCache::from_path(Some(&path));
            assert!(
                !cache.contains("other-execute-only"),
                "an execute bit for a different permission class is not runnable"
            );
        }
    }

    #[test]
    fn refreshes_from_the_child_shell_path() {
        let initial = tempfile::tempdir().expect("initial");
        let refreshed = tempfile::tempdir().expect("refreshed");
        let first = initial.path().join("first-command");
        let second = refreshed.path().join("second-command");
        for path in [&first, &second] {
            fs::File::create(path).expect("create executable");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("mode");
        }

        let initial_path = OsString::from(initial.path());
        let refreshed_path = OsString::from(refreshed.path());
        let cache = CommandPathCache::from_path(Some(&initial_path));
        assert!(cache.contains("first-command"));
        assert!(!cache.contains("second-command"));

        assert!(cache.refresh_from_path(Some(&refreshed_path)));
        assert!(!cache.contains("first-command"));
        assert!(cache.contains("second-command"));
        assert!(!cache.refresh_from_path(Some(&refreshed_path)));
    }

    #[test]
    fn distinguishes_an_exact_command_from_a_longer_completion() {
        let directory = tempfile::tempdir().expect("commands");
        for name in ["code", "codex"] {
            let path = directory.path().join(name);
            fs::File::create(&path).expect("create executable");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("mode");
        }
        let path = OsString::from(directory.path());
        let cache = CommandPathCache::from_path(Some(&path));
        assert!(cache.contains("code"));
        assert!(cache.has_longer_prefix("code"));
        assert!(!cache.has_longer_prefix("codex"));
    }
}
