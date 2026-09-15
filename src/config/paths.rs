use std::{
    env,
    ffi::OsStr,
    fs,
    io::ErrorKind,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigPaths {
    pub config_file: PathBuf,
    pub credentials_file: PathBuf,
    pub specs_directory: PathBuf,
    pub state_directory: PathBuf,
    pub cache_directory: PathBuf,
}

impl ConfigPaths {
    pub fn discover() -> crate::Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| crate::Error::Config("$HOME is not set".into()))?;
        let config_root = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("hokan");
        let state_directory = state_directory(
            &home,
            env::var_os("HOKAN_STATE_DIR").as_deref(),
            env::var_os("XDG_STATE_HOME").as_deref(),
        )?;
        let cache_directory = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
            .join("hokan");
        Ok(Self {
            config_file: config_root.join("config.toml"),
            credentials_file: config_root.join("credentials.toml"),
            specs_directory: config_root.join("specs"),
            state_directory,
            cache_directory,
        })
    }
}

/// State resolution order: `HOKAN_STATE_DIR`, then `XDG_STATE_HOME`, then the
/// private `~/.hokan` default. The default avoids the shared `~/.local/state`
/// tree: a hokan-managed directory only needs a writable `$HOME` — the same
/// precondition as the rc-file integration — while a foreign-owned directory
/// under `~/.local` used to abort every login session with EACCES.
fn state_directory(
    home: &Path,
    hokan_state_dir: Option<&OsStr>,
    xdg_state_home: Option<&OsStr>,
) -> crate::Result<PathBuf> {
    if let Some(dir) = hokan_state_dir.filter(|dir| !dir.is_empty()) {
        return absolute(dir, "HOKAN_STATE_DIR");
    }
    if let Some(dir) = xdg_state_home.filter(|dir| !dir.is_empty()) {
        return absolute(dir, "XDG_STATE_HOME").map(|dir| dir.join("hokan"));
    }
    let target = home.join(".hokan");
    migrate_legacy_state(home, &target);
    Ok(target)
}

fn absolute(dir: &OsStr, variable: &str) -> crate::Result<PathBuf> {
    let path = PathBuf::from(dir);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(crate::Error::Config(format!(
            "${variable} must be an absolute path"
        )))
    }
}

/// Moves the pre-`~/.hokan` defaults (`~/.local/state/hokan`, and the
/// pre-rename beta's `~/.local/state/hokann`) into the private directory.
/// Runs only on the default path: explicit `HOKAN_STATE_DIR` or
/// `XDG_STATE_HOME` choices are used as given. A rename is atomic and may
/// replace an empty `~/.hokan`, so an installer-pre-created directory never
/// strands an existing history store. No compatibility link is left behind:
/// a symlinked state dir is rejected by `HistoryStore::open`, so an old
/// binary on a downgrade path must find the legacy path simply absent.
fn migrate_legacy_state(home: &Path, target: &Path) {
    let migrate = |legacy: &Path| -> bool {
        let Ok(metadata) = fs::symlink_metadata(legacy) else {
            return false;
        };
        if !metadata.is_dir() || metadata.uid() != nix::unistd::geteuid().as_raw() {
            return false;
        }
        match fs::symlink_metadata(target) {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Ok(metadata) if metadata.is_dir() && directory_is_empty(target) => {}
            _ => return false,
        }
        fs::rename(legacy, target).is_ok()
    };
    let xdg_state = home.join(".local/state");
    if migrate(&xdg_state.join("hokan")) {
        return;
    }
    migrate(&xdg_state.join("hokann"));
}

fn directory_is_empty(path: &Path) -> bool {
    fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_directory_prefers_hokan_state_dir() {
        let home = tempfile::tempdir().expect("temporary HOME");
        let custom = PathBuf::from("/custom/state");
        let resolved = state_directory(
            home.path(),
            Some(OsStr::new("/custom/state")),
            Some(OsStr::new("/xdg/state")),
        )
        .expect("state directory");
        assert_eq!(resolved, custom);
    }

    #[test]
    fn state_directory_appends_hokan_to_xdg_state_home() {
        let home = tempfile::tempdir().expect("temporary HOME");
        let resolved = state_directory(home.path(), None, Some(OsStr::new("/xdg/state")))
            .expect("state directory");
        assert_eq!(resolved, PathBuf::from("/xdg/state/hokan"));
    }

    #[test]
    fn state_directory_defaults_to_private_home_directory() {
        let home = tempfile::tempdir().expect("temporary HOME");
        let resolved = state_directory(home.path(), None, None).expect("state directory");
        assert_eq!(resolved, home.path().join(".hokan"));
    }

    #[test]
    fn state_directory_rejects_relative_overrides() {
        let home = tempfile::tempdir().expect("temporary HOME");
        let error = state_directory(home.path(), Some(OsStr::new("relative/dir")), None)
            .expect_err("relative HOKAN_STATE_DIR must be rejected");
        assert!(error.to_string().contains("HOKAN_STATE_DIR"));
        let error = state_directory(home.path(), None, Some(OsStr::new("relative/dir")))
            .expect_err("relative XDG_STATE_HOME must be rejected");
        assert!(error.to_string().contains("XDG_STATE_HOME"));
    }

    #[test]
    fn state_directory_ignores_empty_overrides() {
        let home = tempfile::tempdir().expect("temporary HOME");
        let resolved = state_directory(home.path(), Some(OsStr::new("")), Some(OsStr::new("")))
            .expect("state directory");
        assert_eq!(resolved, home.path().join(".hokan"));
    }

    #[test]
    fn default_state_directory_migrates_legacy_xdg_content() {
        let home = tempfile::tempdir().expect("temporary HOME");
        let legacy = home.path().join(".local/state/hokan");
        fs::create_dir_all(&legacy).expect("legacy state directory");
        fs::write(legacy.join("history.events"), "events").expect("legacy history");

        let resolved = state_directory(home.path(), None, None).expect("state directory");

        assert_eq!(resolved, home.path().join(".hokan"));
        assert!(!legacy.exists());
        assert_eq!(
            fs::read(resolved.join("history.events")).expect("migrated history"),
            b"events"
        );
    }

    #[test]
    fn pre_rename_hokann_state_migrates_when_hokan_is_absent() {
        let home = tempfile::tempdir().expect("temporary HOME");
        let legacy = home.path().join(".local/state/hokann");
        fs::create_dir_all(&legacy).expect("legacy hokann state");
        fs::write(legacy.join("history.events"), "beta").expect("legacy history");

        let resolved = state_directory(home.path(), None, None).expect("state directory");

        assert_eq!(resolved, home.path().join(".hokan"));
        assert!(!legacy.exists());
        assert_eq!(
            fs::read(resolved.join("history.events")).expect("migrated history"),
            b"beta"
        );
    }

    #[test]
    fn hokan_legacy_wins_over_hokann_when_both_exist() {
        let home = tempfile::tempdir().expect("temporary HOME");
        let hokan_legacy = home.path().join(".local/state/hokan");
        let hokann_legacy = home.path().join(".local/state/hokann");
        fs::create_dir_all(&hokan_legacy).expect("hokan legacy");
        fs::create_dir_all(&hokann_legacy).expect("hokann legacy");
        fs::write(hokan_legacy.join("history.events"), "new").expect("hokan history");
        fs::write(hokann_legacy.join("history.events"), "old").expect("hokann history");

        let resolved = state_directory(home.path(), None, None).expect("state directory");

        assert_eq!(
            fs::read(resolved.join("history.events")).expect("migrated history"),
            b"new"
        );
        assert!(hokann_legacy.exists());
    }

    #[test]
    fn migration_replaces_an_empty_target_created_by_the_installer() {
        let home = tempfile::tempdir().expect("temporary HOME");
        let legacy = home.path().join(".local/state/hokan");
        fs::create_dir_all(&legacy).expect("legacy state directory");
        fs::write(legacy.join("history.events"), "events").expect("legacy history");
        let target = home.path().join(".hokan");
        fs::create_dir(&target).expect("empty target directory");

        state_directory(home.path(), None, None).expect("state directory");

        assert!(!legacy.exists());
        assert_eq!(
            fs::read(target.join("history.events")).expect("migrated history"),
            b"events"
        );
    }

    #[test]
    fn migration_keeps_a_non_empty_target_untouched() {
        let home = tempfile::tempdir().expect("temporary HOME");
        let legacy = home.path().join(".local/state/hokan");
        fs::create_dir_all(&legacy).expect("legacy state directory");
        fs::write(legacy.join("history.events"), "old").expect("legacy history");
        let target = home.path().join(".hokan");
        fs::create_dir(&target).expect("target directory");
        fs::write(target.join("history.events"), "new").expect("current history");

        state_directory(home.path(), None, None).expect("state directory");

        assert!(legacy.exists());
        assert_eq!(
            fs::read(target.join("history.events")).expect("current history"),
            b"new"
        );
    }
}
