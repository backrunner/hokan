use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Bound on how many parent levels the probe walks from the cwd.
const MAX_WALK_UP: usize = 8;

/// Project markers detected at or above a working directory. The walk stops
/// after the level containing `.git` (the workspace root boundary), so an
/// outer monorepo's markers do not leak into an inner repository.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceMarkers {
    pub git: bool,
    pub package_json: bool,
    pub cargo_toml: bool,
    pub makefile: bool,
    pub justfile: bool,
}

/// Cached per-cwd workspace probe. A cache entry is validated by comparing
/// the modification time of every walked directory: creating or removing a
/// marker file changes its parent's mtime, which invalidates the entry. Only
/// plain metadata calls are used — no file contents are read and no symlinks
/// are followed beyond the walk-up itself.
#[derive(Debug, Default)]
pub struct WorkspaceProbe {
    cache: HashMap<PathBuf, WorkspaceEntry>,
}

#[derive(Debug)]
struct WorkspaceEntry {
    markers: WorkspaceMarkers,
    identity: Vec<(PathBuf, Option<SystemTime>)>,
}

impl WorkspaceProbe {
    #[must_use]
    pub fn markers(&mut self, cwd: &Path) -> WorkspaceMarkers {
        if let Some(entry) = self.cache.get(cwd)
            && entry
                .identity
                .iter()
                .all(|(path, mtime)| *mtime == directory_mtime(path))
        {
            return entry.markers;
        }
        let (markers, identity) = probe(cwd);
        self.cache
            .insert(cwd.to_owned(), WorkspaceEntry { markers, identity });
        markers
    }
}

fn probe(cwd: &Path) -> (WorkspaceMarkers, Vec<(PathBuf, Option<SystemTime>)>) {
    let mut markers = WorkspaceMarkers::default();
    let mut identity = Vec::new();
    for directory in cwd.ancestors().take(MAX_WALK_UP) {
        identity.push((directory.to_owned(), directory_mtime(directory)));
        // `.git` may be a directory or a file (worktrees, submodules).
        let git = fs::symlink_metadata(directory.join(".git")).is_ok();
        markers.git |= git;
        markers.package_json |= directory.join("package.json").is_file();
        markers.cargo_toml |= directory.join("Cargo.toml").is_file();
        markers.makefile |= directory.join("Makefile").is_file();
        markers.justfile |=
            directory.join("justfile").is_file() || directory.join("Justfile").is_file();
        if git {
            break;
        }
    }
    (markers, identity)
}

fn directory_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_markers_up_to_the_git_boundary() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("Makefile"), "all:").expect("outer makefile");
        let repository = root.path().join("repo");
        let nested = repository.join("src/deep");
        fs::create_dir_all(repository.join(".git")).expect("git dir");
        fs::create_dir_all(&nested).expect("nested");
        fs::write(repository.join("Cargo.toml"), "[package]").expect("manifest");
        fs::write(repository.join("package.json"), "{}").expect("package");
        fs::write(repository.join("justfile"), "build:").expect("justfile");

        let markers = WorkspaceProbe::default().markers(&nested);
        assert!(markers.git);
        assert!(markers.cargo_toml);
        assert!(markers.package_json);
        assert!(markers.justfile);
        assert!(!markers.makefile, "outer marker beyond .git is not visible");
    }

    #[test]
    fn detects_git_files_and_capitalized_justfile() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join(".git"), "gitdir: elsewhere").expect("git file");
        fs::write(root.path().join("Justfile"), "build:").expect("Justfile");
        let markers = WorkspaceProbe::default().markers(root.path());
        assert!(markers.git);
        assert!(markers.justfile);
        assert!(!markers.cargo_toml);
    }

    #[test]
    fn plain_directory_has_no_markers() {
        let root = tempfile::tempdir().expect("root");
        let markers = WorkspaceProbe::default().markers(root.path());
        assert_eq!(markers, WorkspaceMarkers::default());
    }

    #[test]
    fn cache_invalidates_when_a_marker_appears() {
        let root = tempfile::tempdir().expect("root");
        let mut probe = WorkspaceProbe::default();
        assert!(!probe.markers(root.path()).cargo_toml);
        // Directory mtimes have coarse granularity on some filesystems; wait
        // out the resolution so the marker creation bumps the mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(root.path().join("Cargo.toml"), "[package]").expect("manifest");
        assert!(probe.markers(root.path()).cargo_toml);
    }
}
