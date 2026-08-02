use std::{
    collections::{HashMap, HashSet},
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default)]
pub struct CommandPathCache {
    commands: HashMap<String, PathBuf>,
}

impl CommandPathCache {
    #[must_use]
    pub fn from_environment() -> Self {
        Self::from_path(env::var_os("PATH").as_deref())
    }

    #[must_use]
    pub fn from_path(path: Option<&std::ffi::OsStr>) -> Self {
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
        Self { commands }
    }

    #[must_use]
    pub fn contains(&self, command: &str) -> bool {
        self.commands.contains_key(command)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.commands.keys().map(String::as_str)
    }

    #[must_use]
    pub fn path(&self, command: &str) -> Option<&Path> {
        self.commands.get(command).map(PathBuf::as_path)
    }
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, io::Write};

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
            cache.names().filter(|name| *name == "demo-command").count(),
            1
        );
    }
}
