use std::{
    collections::HashMap,
    fs,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};

const MAKEFILE_MAX_BYTES: u64 = 1024 * 1024;
/// Bound on how many parent levels the discovery walk takes from the cwd,
/// matching the workspace probe.
const MAX_WALK_UP: usize = 8;

/// Which rule-file syntax to look for and parse. Both share the
/// `target: deps` line shape; justfiles additionally allow `@target:` quiet
/// recipes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ManifestKind {
    Makefile,
    Justfile,
}

impl ManifestKind {
    /// Candidate file names in lookup order (GNU make's own precedence:
    /// GNUmakefile, Makefile, makefile).
    fn file_names(self) -> &'static [&'static str] {
        match self {
            Self::Makefile => &["GNUmakefile", "Makefile", "makefile"],
            Self::Justfile => &["justfile", "Justfile"],
        }
    }

    /// The rule-file kind for a supported tool command.
    #[must_use]
    pub fn for_tool(tool: &str) -> Self {
        match tool {
            "just" => Self::Justfile,
            _ => Self::Makefile,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MakeTarget {
    pub name: String,
    /// First line of the `# comment` block immediately above the rule, when
    /// present.
    pub doc: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MakefileManifest {
    pub path: PathBuf,
    /// Targets in file order, duplicates removed (the first rule wins, like
    /// make itself).
    pub targets: Vec<MakeTarget>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FileFingerprint {
    device: u64,
    inode: u64,
    length: u64,
    modified_ns: u128,
}

/// Fingerprint-cached nearest-rule-file loader, mirroring `ProjectCache`:
/// a cached parse is reused while the file's device/inode/length/mtime
/// fingerprint is unchanged, so repeated `make <tab>` queries cost one
/// metadata call per query and at most one read per edit.
#[derive(Debug, Default)]
pub struct MakefileCache {
    manifests: Mutex<HashMap<PathBuf, (FileFingerprint, Arc<MakefileManifest>)>>,
}

impl MakefileCache {
    pub fn load_nearest(
        &self,
        cwd: &Path,
        kind: ManifestKind,
    ) -> crate::Result<Option<Arc<MakefileManifest>>> {
        let Some(path) = discover_makefile(cwd, kind) else {
            return Ok(None);
        };
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() {
            return Err(crate::Error::Project(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        if metadata.len() > MAKEFILE_MAX_BYTES {
            return Err(crate::Error::Project(format!(
                "{} exceeds the 1 MiB rule-file limit",
                path.display()
            )));
        }
        let fingerprint = fingerprint_of(&metadata);
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
            .take(MAKEFILE_MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAKEFILE_MAX_BYTES {
            return Err(crate::Error::Project(format!(
                "{} exceeds the 1 MiB rule-file limit",
                path.display()
            )));
        }
        let final_fingerprint = fingerprint_of(&file.metadata()?);
        if final_fingerprint != fingerprint {
            return Err(crate::Error::Project(format!(
                "{} changed while it was being read",
                path.display()
            )));
        }
        let text = String::from_utf8_lossy(&bytes);
        let manifest = Arc::new(MakefileManifest {
            path: path.clone(),
            targets: parse_targets(&text, kind),
        });
        let mut cache = self
            .manifests
            .lock()
            .map_err(|_| crate::Error::Project("makefile cache was poisoned".into()))?;
        cache.insert(path, (fingerprint, Arc::clone(&manifest)));
        Ok(Some(manifest))
    }
}

fn fingerprint_of(metadata: &fs::Metadata) -> FileFingerprint {
    FileFingerprint {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        modified_ns: metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos()),
    }
}

/// Nearest rule file of the given kind at or above `cwd`. The walk is bounded
/// like the workspace probe and stops after the level containing `.git`, so
/// an outer repository's Makefile does not leak into an inner one.
#[must_use]
pub fn discover_makefile(cwd: &Path, kind: ManifestKind) -> Option<PathBuf> {
    let mut directory = fs::canonicalize(cwd).ok()?;
    for _ in 0..MAX_WALK_UP {
        for name in kind.file_names() {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if directory.join(".git").exists() {
            return None;
        }
        if !directory.pop() {
            return None;
        }
    }
    None
}

/// Conservative target extraction: only flush-left `name:` / `name: deps`
/// lines (plus `@name:` quiet recipes in justfiles) count. Recipe lines are
/// indented and never match; `%`/`$`/`.`-prefixed names (`.PHONY`, pattern
/// rules, variable references), `include` directives, and assignments
/// (`VAR := value`) are skipped. A partial target list is useful, a wrong one
/// is not.
fn parse_targets(text: &str, kind: ManifestKind) -> Vec<MakeTarget> {
    let mut targets = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut pending_doc: Option<String> = None;
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            // Doc comment: the first line of the contiguous `#` block directly
            // above the rule.
            if pending_doc.is_none() {
                let doc = line.trim_start().trim_start_matches('#').trim();
                pending_doc = (!doc.is_empty()).then(|| doc.to_owned());
            }
            continue;
        }
        let doc = pending_doc.take();
        if line.trim().is_empty() {
            continue;
        }
        if let Some(name) = rule_name(line, kind)
            && seen.insert(name.to_owned())
        {
            targets.push(MakeTarget {
                name: name.to_owned(),
                doc,
            });
        }
    }
    targets
}

/// Rule target name of a flush-left line, or `None` for recipes, comments,
/// assignments, directives, and special/pattern targets.
fn rule_name(line: &str, kind: ManifestKind) -> Option<&str> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let mut rest = line;
    if kind == ManifestKind::Justfile {
        rest = rest.strip_prefix('@').unwrap_or(rest);
    }
    let end = rest.find(|character: char| character == ':' || character.is_whitespace())?;
    let name = &rest[..end];
    if !is_target_name(name) {
        return None;
    }
    let after = rest[end..].trim_start();
    let after = after.strip_prefix(':')?;
    // `VAR := value` is an assignment, not a rule.
    if after.starts_with('=') {
        return None;
    }
    Some(name)
}

fn is_target_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    // `.PHONY` and friends, pattern rules (`%.o`), variable references
    // (`$(BIN)`), and `include`/`-include` directives are never completions.
    if name.starts_with(['%', '$', '.', '-']) || name.contains(['%', '$']) {
        return false;
    }
    if matches!(name, "include" | "sinclude" | "export" | "set") {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'))
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::*;

    #[test]
    fn parses_makefile_targets_and_skips_non_rules() {
        let makefile = "\
# Build the release binary.
build: deps
	cargo build --release

# Run all tests.
# Second comment line is ignored.
test: build
	cargo test

.PHONY: build test clean
%.o: %.c
	$(CC) -c $<

CC := gcc
CFLAGS = -Wall
include extra.mk
-include optional.mk
$(BIN): src/main.rs
clean :
	rm -rf target
build: duplicate
";
        let targets = parse_targets(makefile, ManifestKind::Makefile);
        let names: Vec<&str> = targets.iter().map(|target| target.name.as_str()).collect();
        assert_eq!(names, ["build", "test", "clean"]);
        assert_eq!(targets[0].doc.as_deref(), Some("Build the release binary."));
        assert_eq!(targets[1].doc.as_deref(), Some("Run all tests."));
        assert_eq!(targets[2].doc, None);
    }

    #[test]
    fn doc_comment_must_immediately_precede_the_rule() {
        let makefile = "# Detached comment.\n\nbuild:\n\ttrue\n";
        let targets = parse_targets(makefile, ManifestKind::Makefile);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].doc, None);
    }

    #[test]
    fn parses_justfile_quiet_recipes() {
        let justfile = "\
set positional-arguments

# Build the project.
@build:
    cargo build

test filter='':
    cargo test {{filter}}

@lint: build
    cargo clippy
";
        let targets = parse_targets(justfile, ManifestKind::Justfile);
        let names: Vec<&str> = targets.iter().map(|target| target.name.as_str()).collect();
        // `test filter='':` is a parametrized recipe; the conservative parser
        // skips it rather than guessing the name.
        assert_eq!(names, ["build", "lint"]);
        assert_eq!(targets[0].doc.as_deref(), Some("Build the project."));
    }

    #[test]
    fn at_prefixed_rules_are_makefile_silent_commands_not_targets() {
        // In a Makefile a flush-left `@x:` is not valid rule syntax; it stays
        // unparseable (conservative), while justfiles accept it.
        let text = "@build:\n\ttrue\n";
        assert!(parse_targets(text, ManifestKind::Makefile).is_empty());
        assert_eq!(parse_targets(text, ManifestKind::Justfile).len(), 1);
    }

    #[test]
    fn discovery_prefers_gnu_makefile_and_stops_at_git_boundary() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("Makefile"), "all:").expect("Makefile");
        fs::write(root.path().join("GNUmakefile"), "all:").expect("GNUmakefile");
        let found = discover_makefile(root.path(), ManifestKind::Makefile).expect("makefile");
        assert_eq!(found.file_name().expect("name"), "GNUmakefile");

        let repository = root.path().join("repo");
        let nested = repository.join("src/deep");
        fs::create_dir_all(repository.join(".git")).expect("git marker");
        fs::create_dir_all(&nested).expect("nested");
        assert_eq!(discover_makefile(&nested, ManifestKind::Makefile), None);
        fs::write(repository.join("justfile"), "build:").expect("justfile");
        assert!(
            discover_makefile(&nested, ManifestKind::Justfile)
                .expect("justfile")
                .ends_with("justfile")
        );
    }

    #[test]
    fn cache_invalidates_when_the_rule_file_changes() {
        let root = tempfile::tempdir().expect("root");
        fs::write(root.path().join("Makefile"), "build:\n").expect("makefile");
        let cache = MakefileCache::default();
        let first = cache
            .load_nearest(root.path(), ManifestKind::Makefile)
            .expect("load")
            .expect("manifest");
        assert_eq!(first.targets.len(), 1);
        assert_eq!(first.targets[0].name, "build");
        thread::sleep(Duration::from_millis(2));
        fs::write(root.path().join("Makefile"), "build:\ntest:\n").expect("update makefile");
        let second = cache
            .load_nearest(root.path(), ManifestKind::Makefile)
            .expect("reload")
            .expect("manifest");
        let names: Vec<&str> = second
            .targets
            .iter()
            .map(|target| target.name.as_str())
            .collect();
        assert_eq!(names, ["build", "test"]);
    }
}
