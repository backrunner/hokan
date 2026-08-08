//! Node workspace (monorepo) discovery: pnpm-workspace.yaml `packages:` or
//! the root package.json `workspaces` field, resolved to member packages
//! with their names and scripts. Used to complete `--filter` values and
//! member scripts. Result cached briefly — member manifests change rarely,
//! keystrokes are many.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use globset::{GlobBuilder, GlobMatcher};

const MEMBER_GLOBS_MAX: usize = 64;
const MEMBERS_MAX: usize = 256;
const DIRECTORIES_MAX: usize = 8192;
const RECURSIVE_DEPTH_MAX: usize = 32;
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

/// Globs from `pnpm-workspace.yaml`, else `workspaces` in package.json.
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

/// The `packages:` list of a pnpm-workspace.yaml. Both block and inline lists
/// are accepted; negations are retained so member resolution can apply them.
fn parse_pnpm_workspace_globs(text: &str) -> Vec<String> {
    let mut globs = Vec::new();
    let mut packages_indent = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if packages_indent.is_none() {
            let Some(rest) = trimmed.strip_prefix("packages:") else {
                continue;
            };
            packages_indent = Some(line.len() - line.trim_start().len());
            if !strip_yaml_comment(rest).trim().is_empty() {
                globs.extend(parse_yaml_inline_list(rest));
                break;
            }
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent <= packages_indent.unwrap_or_default() {
            break;
        }
        let Some(entry) = trimmed.strip_prefix('-') else {
            break;
        };
        if let Some(entry) = parse_yaml_scalar(entry) {
            globs.push(entry);
        }
    }
    globs_with_positive(globs)
}

fn parse_yaml_inline_list(value: &str) -> Vec<String> {
    let value = strip_yaml_comment(value).trim();
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    let mut start = 0;
    let mut quote = None;
    for (index, character) in inner.char_indices() {
        match (quote, character) {
            (None, '\'' | '"') => quote = Some(character),
            (Some(active), current) if active == current => quote = None,
            (None, ',') => {
                if let Some(entry) = parse_yaml_scalar(&inner[start..index]) {
                    entries.push(entry);
                }
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if let Some(entry) = parse_yaml_scalar(&inner[start..]) {
        entries.push(entry);
    }
    entries
}

fn parse_yaml_scalar(value: &str) -> Option<String> {
    let value = strip_yaml_comment(value).trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with('"') && value.ends_with('"') {
        return serde_json::from_str(value).ok();
    }
    if let Some(value) = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        return Some(value.replace("''", "'"));
    }
    Some(value.to_owned())
}

fn strip_yaml_comment(value: &str) -> &str {
    let mut quote = None;
    let mut previous_whitespace = true;
    for (index, character) in value.char_indices() {
        match (quote, character) {
            (None, '\'' | '"') => quote = Some(character),
            (Some(active), current) if active == current => quote = None,
            (None, '#') if previous_whitespace => return &value[..index],
            _ => {}
        }
        previous_whitespace = character.is_whitespace();
    }
    value
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
        .map(str::to_owned)
        .collect();
    let globs = globs_with_positive(globs);
    (!globs.is_empty()).then_some(globs)
}

fn resolve_members(root: &Path, globs: &[String]) -> Vec<WorkspaceMember> {
    let patterns = compile_patterns(globs);
    let mut scan_roots = BTreeMap::<PathBuf, usize>::new();
    for pattern in patterns.iter().filter(|pattern| !pattern.excluded) {
        scan_roots
            .entry(root.join(&pattern.base))
            .and_modify(|depth| *depth = (*depth).max(pattern.scan_depth))
            .or_insert(pattern.scan_depth);
    }

    let mut members = Vec::new();
    let mut visited = HashMap::<PathBuf, usize>::new();
    let mut directories_seen = 0;
    for (scan_root, scan_depth) in scan_roots {
        let mut pending = vec![(scan_root, scan_depth)];
        while let Some((directory, remaining_depth)) = pending.pop() {
            if directories_seen >= DIRECTORIES_MAX || members.len() >= MEMBERS_MAX {
                return finish_members(members);
            }
            if visited
                .get(&directory)
                .is_some_and(|visited_depth| *visited_depth >= remaining_depth)
            {
                continue;
            }
            visited.insert(directory.clone(), remaining_depth);
            let Ok(metadata) = fs::symlink_metadata(&directory) else {
                continue;
            };
            if !metadata.file_type().is_dir() {
                continue;
            }
            directories_seen += 1;

            if directory != root
                && relative_workspace_path(root, &directory)
                    .is_some_and(|relative| patterns_include(&patterns, &relative))
                && let Some(member) = load_member(&directory)
            {
                members.push(member);
            }
            if remaining_depth == 0 {
                continue;
            }
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            let mut children: Vec<PathBuf> = entries
                .flatten()
                .filter_map(|entry| {
                    let file_type = entry.file_type().ok()?;
                    let name = entry.file_name();
                    (file_type.is_dir() && !ignored_directory(&name)).then(|| entry.path())
                })
                .collect();
            children.sort();
            pending.extend(
                children
                    .into_iter()
                    .rev()
                    .map(|child| (child, remaining_depth - 1)),
            );
        }
    }
    finish_members(members)
}

struct WorkspacePattern {
    excluded: bool,
    matcher: GlobMatcher,
    base: PathBuf,
    scan_depth: usize,
}

fn compile_patterns(globs: &[String]) -> Vec<WorkspacePattern> {
    globs
        .iter()
        .take(MEMBER_GLOBS_MAX)
        .filter_map(|value| {
            let (excluded, value) = value
                .strip_prefix('!')
                .map_or((false, value.as_str()), |value| (true, value));
            let value = value.trim().trim_start_matches("./").trim_end_matches('/');
            if value.is_empty() || !safe_relative_pattern(value) {
                return None;
            }
            let glob = GlobBuilder::new(value)
                .literal_separator(true)
                .build()
                .ok()?;
            let (base, scan_depth) = pattern_scan_root(value);
            Some(WorkspacePattern {
                excluded,
                matcher: glob.compile_matcher(),
                base,
                scan_depth,
            })
        })
        .collect()
}

fn safe_relative_pattern(pattern: &str) -> bool {
    Path::new(pattern)
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn pattern_scan_root(pattern: &str) -> (PathBuf, usize) {
    let components: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    let literal_count = components
        .iter()
        .position(|component| component.contains(['*', '?', '[', '{']))
        .unwrap_or(components.len());
    let mut base = PathBuf::new();
    for component in &components[..literal_count] {
        base.push(component);
    }
    let remaining = &components[literal_count..];
    let scan_depth = if remaining.iter().any(|component| component.contains("**")) {
        RECURSIVE_DEPTH_MAX
    } else {
        remaining.len()
    };
    (base, scan_depth)
}

fn relative_workspace_path(root: &Path, directory: &Path) -> Option<String> {
    let relative = directory.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return None;
        };
        parts.push(part.to_str()?);
    }
    Some(parts.join("/"))
}

fn patterns_include(patterns: &[WorkspacePattern], relative: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| !pattern.excluded && pattern.matcher.is_match(relative))
        && !patterns
            .iter()
            .any(|pattern| pattern.excluded && pattern.matcher.is_match(relative))
}

fn ignored_directory(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git" | ".hg" | ".svn" | "node_modules")
    )
}

fn globs_with_positive(globs: Vec<String>) -> Vec<String> {
    let globs: Vec<String> = globs
        .into_iter()
        .map(|glob| glob.trim().to_owned())
        .filter(|glob| !glob.is_empty())
        .collect();
    if globs.iter().any(|glob| !glob.trim_start().starts_with('!')) {
        globs
    } else {
        Vec::new()
    }
}

fn finish_members(mut members: Vec<WorkspaceMember>) -> Vec<WorkspaceMember> {
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
            "packages: # workspace members\n  - 'packages/*'\n  - '!packages/.hidden'\n",
        )
        .expect("yaml");
        for (dir, name, script) in [
            ("packages/api", "@acme/api", "start"),
            ("packages/web", "@acme/web", "dev"),
            ("packages/.hidden", "@acme/hidden", "hidden"),
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
    fn recursive_globs_negations_and_ignored_dependency_trees_are_respected() {
        let root = tempfile::tempdir().expect("root");
        for (directory, name) in [
            ("packages/platform/api", "@acme/api"),
            ("packages/legacy/old", "@acme/old"),
            ("node_modules/vendor", "vendor"),
        ] {
            fs::create_dir_all(root.path().join(directory)).expect("member directory");
            fs::write(
                root.path().join(directory).join("package.json"),
                format!(r#"{{"name":"{name}"}}"#),
            )
            .expect("member manifest");
        }
        fs::write(
            root.path().join("pnpm-workspace.yaml"),
            "packages: ['**', '!packages/legacy/**'] # inline form\n",
        )
        .expect("workspace yaml");

        let workspace = discover_node_workspace(root.path()).expect("workspace");
        let names: Vec<_> = workspace
            .members
            .iter()
            .map(|member| member.name.as_str())
            .collect();
        assert_eq!(names, ["@acme/api"]);
    }

    #[test]
    fn package_json_workspace_braces_and_negations_are_respected() {
        let root = tempfile::tempdir().expect("root");
        for (directory, name) in [
            ("apps/web", "@acme/web"),
            ("packages/api", "@acme/api"),
            ("packages/private", "@acme/private"),
        ] {
            fs::create_dir_all(root.path().join(directory)).expect("member directory");
            fs::write(
                root.path().join(directory).join("package.json"),
                format!(r#"{{"name":"{name}"}}"#),
            )
            .expect("member manifest");
        }
        fs::write(
            root.path().join("package.json"),
            r#"{"workspaces":["{apps,packages}/*","!packages/private"]}"#,
        )
        .expect("root manifest");

        let workspace = discover_node_workspace(root.path()).expect("workspace");
        let names: Vec<_> = workspace
            .members
            .iter()
            .map(|member| member.name.as_str())
            .collect();
        assert_eq!(names, ["@acme/api", "@acme/web"]);
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
