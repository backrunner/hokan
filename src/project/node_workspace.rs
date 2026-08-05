//! Node workspace (monorepo) discovery: pnpm-workspace.yaml `packages:` or
//! the root package.json `workspaces` field, resolved to member packages
//! with their names and scripts. Used to complete `--filter` values and
//! member scripts. Result cached briefly — member manifests change rarely,
//! keystrokes are many.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MEMBER_GLOBS_MAX: usize = 64;
const MEMBERS_MAX: usize = 256;
const CACHE_TTL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMember {
    pub name: String,
    pub directory: PathBuf,
    pub scripts: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeWorkspace {
    pub root: PathBuf,
    pub members: Vec<WorkspaceMember>,
}

type WorkspaceSlot = Option<Arc<NodeWorkspace>>;

#[derive(Debug, Default)]
pub struct NodeWorkspaceCache {
    cached: Mutex<HashMap<PathBuf, (Instant, WorkspaceSlot)>>,
}

impl NodeWorkspaceCache {
    pub fn load(&self, cwd: &Path) -> Option<Arc<NodeWorkspace>> {
        if let Ok(cache) = self.cached.lock()
            && let Some((at, workspace)) = cache.get(cwd)
            && at.elapsed() < CACHE_TTL
        {
            return workspace.clone();
        }
        let workspace = discover_node_workspace(cwd).map(Arc::new);
        if let Ok(mut cache) = self.cached.lock() {
            cache.insert(cwd.to_owned(), (Instant::now(), workspace.clone()));
        }
        workspace
    }
}

/// Find the nearest workspace root (pnpm-workspace.yaml or a package.json
/// with `workspaces`) walking up, stopping after the `.git` level.
pub fn discover_node_workspace(cwd: &Path) -> Option<NodeWorkspace> {
    let mut directory = fs::canonicalize(cwd).ok()?;
    loop {
        if let Some(globs) = workspace_globs(&directory) {
            let members = resolve_members(&directory, &globs);
            if !members.is_empty() {
                return Some(NodeWorkspace {
                    root: directory,
                    members,
                });
            }
        }
        if directory.join(".git").exists() || !directory.pop() {
            return None;
        }
    }
}

/// Globs from `pnpm-workspace.yaml` (conservative line parsing — the file is
/// small and machine-written in practice), else `workspaces` in package.json.
fn workspace_globs(root: &Path) -> Option<Vec<String>> {
    let pnpm_yaml = root.join("pnpm-workspace.yaml");
    if pnpm_yaml.is_file()
        && let Ok(text) = fs::read_to_string(&pnpm_yaml)
    {
        let globs = parse_pnpm_workspace_globs(&text);
        if !globs.is_empty() {
            return Some(globs);
        }
    }
    let manifest = root.join("package.json");
    let text = fs::read_to_string(manifest).ok()?;
    parse_json_workspaces(&text)
}

/// The `packages:` list of a pnpm-workspace.yaml: lines after the
/// `packages:` header that start with `- `, quotes stripped, negations kept
/// out.
fn parse_pnpm_workspace_globs(text: &str) -> Vec<String> {
    let mut globs = Vec::new();
    let mut in_packages = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("packages:") {
            in_packages = true;
            continue;
        }
        if in_packages {
            if let Some(entry) = trimmed.strip_prefix("- ") {
                let entry = entry.trim().trim_matches(['\'', '"']);
                if !entry.is_empty() && !entry.starts_with('!') {
                    globs.push(entry.to_owned());
                }
            } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                break;
            }
        }
    }
    globs
}

/// `workspaces` in package.json: either an array or `{ "packages": [...] }`.
fn parse_json_workspaces(text: &str) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let workspaces = value.get("workspaces")?;
    let entries = workspaces
        .as_array()
        .or_else(|| workspaces.get("packages")?.as_array())?;
    let globs: Vec<String> = entries
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|entry| !entry.starts_with('!'))
        .map(str::to_owned)
        .collect();
    (!globs.is_empty()).then_some(globs)
}

fn resolve_members(root: &Path, globs: &[String]) -> Vec<WorkspaceMember> {
    let mut members = Vec::new();
    for glob in globs.iter().take(MEMBER_GLOBS_MAX) {
        let directories: Vec<PathBuf> = if let Some(parent) = glob.strip_suffix("/*") {
            let Ok(entries) = fs::read_dir(root.join(parent)) else {
                continue;
            };
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        } else {
            let directory = root.join(glob);
            directory
                .is_dir()
                .then_some(directory)
                .into_iter()
                .collect()
        };
        for directory in directories {
            if members.len() >= MEMBERS_MAX {
                return members;
            }
            if let Some(member) = load_member(&directory) {
                members.push(member);
            }
        }
    }
    members.sort_by(|left, right| left.name.cmp(&right.name));
    members.dedup_by(|left, right| left.name == right.name);
    members
}

fn load_member(directory: &Path) -> Option<WorkspaceMember> {
    let manifest = directory.join("package.json");
    let text = fs::read_to_string(manifest).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            directory
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })?;
    let scripts = value
        .get("scripts")
        .and_then(serde_json::Value::as_object)
        .map(|scripts| {
            scripts
                .iter()
                .filter_map(|(name, script)| {
                    script
                        .as_str()
                        .map(|script| (name.clone(), script.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(WorkspaceMember {
        name,
        directory: directory.to_owned(),
        scripts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_members_from_pnpm_workspace_yaml() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir_all(root.path().join("packages/api")).expect("api");
        fs::create_dir_all(root.path().join("packages/web")).expect("web");
        fs::create_dir_all(root.path().join("packages/.hidden")).expect("hidden");
        fs::write(
            root.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '!packages/.hidden'\n",
        )
        .expect("yaml");
        for (dir, name, script) in [
            ("packages/api", "@acme/api", "start"),
            ("packages/web", "@acme/web", "dev"),
        ] {
            fs::write(
                root.path().join(dir).join("package.json"),
                format!(r#"{{"name":"{name}","scripts":{{"{script}":"node index.js"}}}}"#),
            )
            .expect("member manifest");
        }
        let nested = root.path().join("packages/api/src");
        fs::create_dir_all(&nested).expect("nested");
        let workspace = discover_node_workspace(&nested).expect("workspace");
        let names: Vec<_> = workspace
            .members
            .iter()
            .map(|member| member.name.as_str())
            .collect();
        assert_eq!(names, ["@acme/api", "@acme/web"]);
        assert_eq!(workspace.members[0].scripts["start"], "node index.js");
    }

    #[test]
    fn discovers_members_from_package_json_workspaces() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir_all(root.path().join("apps/cli")).expect("cli");
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"root","workspaces":["apps/*"]}"#,
        )
        .expect("root manifest");
        fs::write(
            root.path().join("apps/cli/package.json"),
            r#"{"scripts":{"build":"esbuild"}}"#,
        )
        .expect("member manifest");
        let workspace = discover_node_workspace(root.path()).expect("workspace");
        // No `name` in the member manifest: the directory name is used.
        assert_eq!(workspace.members[0].name, "cli");
        assert_eq!(workspace.members[0].scripts["build"], "esbuild");
    }

    #[test]
    fn non_workspace_directories_yield_none() {
        let root = tempfile::tempdir().expect("root");
        assert!(discover_node_workspace(root.path()).is_none());
    }
}
