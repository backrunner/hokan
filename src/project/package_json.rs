use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};

use serde::Deserialize;

const MANIFEST_MAX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageManifest {
    pub path: PathBuf,
    pub scripts: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FileFingerprint {
    device: u64,
    inode: u64,
    length: u64,
    modified_ns: u128,
}

#[derive(Debug, Default)]
pub struct ProjectCache {
    manifests: Mutex<HashMap<PathBuf, (FileFingerprint, Arc<PackageManifest>)>>,
}

impl ProjectCache {
    pub fn load_nearest(&self, cwd: &Path) -> crate::Result<Option<Arc<PackageManifest>>> {
        let Some(path) = discover_package_json(cwd) else {
            return Ok(None);
        };
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() {
            return Err(crate::Error::Project(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        if metadata.len() > MANIFEST_MAX_BYTES {
            return Err(crate::Error::Project(format!(
                "{} exceeds the 2 MiB manifest limit",
                path.display()
            )));
        }
        let fingerprint = FileFingerprint {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_ns: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_nanos()),
        };
        if let Ok(cache) = self.manifests.lock()
            && let Some((cached_fingerprint, manifest)) = cache.get(&path)
            && *cached_fingerprint == fingerprint
        {
            return Ok(Some(Arc::clone(manifest)));
        }

        let mut file = fs::File::open(&path)?;
        let opened = file.metadata()?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(crate::Error::Project(format!(
                "{} changed while it was being opened",
                path.display()
            )));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(MANIFEST_MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MANIFEST_MAX_BYTES {
            return Err(crate::Error::Project(format!(
                "{} exceeds the 2 MiB manifest limit",
                path.display()
            )));
        }
        let final_metadata = file.metadata()?;
        let final_fingerprint = FileFingerprint {
            device: final_metadata.dev(),
            inode: final_metadata.ino(),
            length: final_metadata.len(),
            modified_ns: final_metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_nanos()),
        };
        if final_fingerprint != fingerprint {
            return Err(crate::Error::Project(format!(
                "{} changed while it was being read",
                path.display()
            )));
        }
        let parsed: PackageJson = serde_json::from_slice(&bytes).map_err(|error| {
            crate::Error::Project(format!("cannot parse {}: {error}", path.display()))
        })?;
        let manifest = Arc::new(PackageManifest {
            path: path.clone(),
            scripts: parsed.scripts,
        });
        let mut cache = self
            .manifests
            .lock()
            .map_err(|_| crate::Error::Project("project cache was poisoned".into()))?;
        cache.insert(path, (fingerprint, Arc::clone(&manifest)));
        Ok(Some(manifest))
    }
}

#[derive(Deserialize)]
struct PackageJson {
    #[serde(default)]
    scripts: BTreeMap<String, String>,
}

#[must_use]
pub fn discover_package_json(cwd: &Path) -> Option<PathBuf> {
    let mut directory = fs::canonicalize(cwd).ok()?;
    loop {
        let candidate = directory.join("package.json");
        if candidate.is_file() {
            return Some(candidate);
        }
        if directory.join(".git").exists() {
            return None;
        }
        if !directory.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::*;

    #[test]
    fn discovers_nearest_and_invalidates_by_metadata() {
        let root = tempfile::tempdir().expect("root");
        let nested = root.path().join("packages/app/src");
        fs::create_dir_all(&nested).expect("nested");
        fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"build":"vite build"}}"#,
        )
        .expect("manifest");
        let cache = ProjectCache::default();
        let first = cache
            .load_nearest(&nested)
            .expect("load")
            .expect("manifest");
        assert_eq!(first.scripts["build"], "vite build");
        thread::sleep(Duration::from_millis(2));
        fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"test":"vitest"}}"#,
        )
        .expect("update manifest");
        let second = cache
            .load_nearest(&nested)
            .expect("reload")
            .expect("manifest");
        assert!(second.scripts.contains_key("test"));
    }

    #[test]
    fn discovery_does_not_cross_the_nearest_git_boundary() {
        let outer = tempfile::tempdir().expect("outer");
        fs::write(
            outer.path().join("package.json"),
            r#"{"scripts":{"outer":"x"}}"#,
        )
        .expect("outer manifest");
        let repository = outer.path().join("repo");
        let nested = repository.join("src/deep");
        fs::create_dir_all(repository.join(".git")).expect("git marker");
        fs::create_dir_all(&nested).expect("nested");
        assert_eq!(discover_package_json(&nested), None);
    }

    #[test]
    fn oversized_manifest_is_rejected_before_parsing() {
        let root = tempfile::tempdir().expect("root");
        fs::write(
            root.path().join("package.json"),
            vec![b' '; MANIFEST_MAX_BYTES as usize + 1],
        )
        .expect("oversized manifest");
        let error = ProjectCache::default()
            .load_nearest(root.path())
            .expect_err("oversized manifest should fail");
        assert!(error.to_string().contains("2 MiB manifest limit"));
    }
}
