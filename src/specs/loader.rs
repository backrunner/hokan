use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    completion::SlotKind,
    specs::{CommandSpec, RecipeSpec, SpecDocument},
    terminal::RiskLevel,
};

const COMMON_SPECS: &str = include_str!("../../assets/specs/common/core.toml");
const LINUX_SPECS: &str = include_str!("../../assets/specs/linux/alternatives.toml");
const USER_SPEC_MAX_FILES: usize = 128;
const USER_SPEC_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpecDiagnostic {
    pub path: PathBuf,
    pub code: &'static str,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct CompiledRecipe {
    pub id: String,
    pub template: String,
    pub prefix: String,
    pub description: String,
    pub risk: RiskLevel,
    pub next_slot: Option<SlotKind>,
    pub complete: bool,
}

#[derive(Clone, Debug)]
pub struct CompiledCommand {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub description: String,
    pub requires_arguments: bool,
    pub risk: RiskLevel,
    pub default: String,
    pub recipes: Vec<CompiledRecipe>,
    pub provenance: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct SpecRegistry {
    by_name: BTreeMap<String, Arc<CompiledCommand>>,
    by_id: BTreeMap<String, Arc<CompiledCommand>>,
    diagnostics: Vec<SpecDiagnostic>,
}

impl SpecRegistry {
    #[must_use]
    pub fn load(user_directory: Option<&Path>) -> Self {
        let mut registry = Self::default();
        registry.load_text(Path::new("<builtin:common>"), COMMON_SPECS, false);
        if std::env::consts::OS == "linux" {
            registry.load_text(Path::new("<builtin:linux>"), LINUX_SPECS, false);
        }
        if let Some(directory) = user_directory {
            registry.load_user_directory(directory);
        }
        registry
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<CompiledCommand>> {
        self.by_name.get(name)
    }

    pub fn commands(&self) -> impl Iterator<Item = &Arc<CompiledCommand>> {
        self.by_id.values()
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[SpecDiagnostic] {
        &self.diagnostics
    }

    fn load_user_directory(&mut self, directory: &Path) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        let mut paths: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "toml")
            })
            .collect();
        paths.sort();
        if paths.len() > USER_SPEC_MAX_FILES {
            self.diagnostics.push(SpecDiagnostic {
                path: directory.to_owned(),
                code: "HK-SPC-004",
                message: format!("only the first {USER_SPEC_MAX_FILES} user spec files are loaded"),
            });
            paths.truncate(USER_SPEC_MAX_FILES);
        }
        for path in paths {
            match read_user_spec(&path) {
                Ok(text) => self.load_text(&path, &text, true),
                Err(message) => self.diagnostics.push(SpecDiagnostic {
                    path,
                    code: "HK-SPC-001",
                    message,
                }),
            }
        }
    }

    fn load_text(&mut self, path: &Path, text: &str, user: bool) {
        let document: SpecDocument = match toml::from_str(text) {
            Ok(document) => document,
            Err(error) => {
                self.diagnostics.push(SpecDiagnostic {
                    path: path.to_owned(),
                    code: "HK-SPC-002",
                    message: format!("invalid TOML: {}", error.message()),
                });
                return;
            }
        };
        if document.schema != 1 {
            self.diagnostics.push(SpecDiagnostic {
                path: path.to_owned(),
                code: "HK-SPC-003",
                message: format!("unsupported schema {}", document.schema),
            });
            return;
        }
        for spec in document.commands {
            match compile_command(&spec, path) {
                Ok(command) => self.insert(command, spec.replaces.as_deref(), user),
                Err(message) => self.diagnostics.push(SpecDiagnostic {
                    path: path.to_owned(),
                    code: "HK-SPC-010",
                    message,
                }),
            }
        }
    }

    fn insert(&mut self, command: CompiledCommand, replaces: Option<&str>, user: bool) {
        let replacement_id = replaces.filter(|_| user);
        let collision = std::iter::once(command.name.as_str())
            .chain(command.aliases.iter().map(String::as_str))
            .find(|name| {
                self.by_name
                    .get(*name)
                    .is_some_and(|existing| Some(existing.id.as_str()) != replacement_id)
            });
        if let Some(name) = collision {
            self.diagnostics.push(SpecDiagnostic {
                path: command.provenance.clone(),
                code: "HK-SPC-012",
                message: format!("duplicate command name or alias {name:?}"),
            });
            return;
        }
        if let Some(replaced) = replaces {
            if !user || !self.by_id.contains_key(replaced) {
                self.diagnostics.push(SpecDiagnostic {
                    path: command.provenance.clone(),
                    code: "HK-SPC-011",
                    message: format!("replacement target {replaced:?} does not exist"),
                });
                return;
            }
            if let Some(previous) = self.by_id.remove(replaced) {
                self.by_name.retain(|_, value| value.id != previous.id);
            }
        } else if self.by_id.contains_key(&command.id) || self.by_name.contains_key(&command.name) {
            self.diagnostics.push(SpecDiagnostic {
                path: command.provenance.clone(),
                code: "HK-SPC-012",
                message: format!("duplicate command id or name for {:?}", command.name),
            });
            return;
        }
        let command = Arc::new(command);
        self.by_name
            .insert(command.name.clone(), Arc::clone(&command));
        for alias in &command.aliases {
            self.by_name.insert(alias.clone(), Arc::clone(&command));
        }
        self.by_id.insert(command.id.clone(), command);
    }
}

fn read_user_spec(path: &Path) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("user spec is not a regular file".into());
    }
    if metadata.len() > USER_SPEC_MAX_BYTES {
        return Err("user spec exceeds the 1 MiB file limit".into());
    }
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let opened = file.metadata().map_err(|error| error.to_string())?;
    if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
        return Err("user spec changed while it was being opened".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(USER_SPEC_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > USER_SPEC_MAX_BYTES {
        return Err("user spec exceeds the 1 MiB file limit".into());
    }
    let final_metadata = file.metadata().map_err(|error| error.to_string())?;
    if final_metadata.dev() != metadata.dev()
        || final_metadata.ino() != metadata.ino()
        || final_metadata.len() != metadata.len()
        || final_metadata.mtime() != metadata.mtime()
        || final_metadata.mtime_nsec() != metadata.mtime_nsec()
    {
        return Err("user spec changed while it was being read".into());
    }
    String::from_utf8(bytes).map_err(|_| "user spec is not valid UTF-8".into())
}

fn compile_command(spec: &CommandSpec, path: &Path) -> Result<CompiledCommand, String> {
    validate_identifier(&spec.id, "command id")?;
    validate_command_name(&spec.name)?;
    let mut names = HashSet::from([spec.name.as_str()]);
    for alias in &spec.aliases {
        validate_command_name(alias)?;
        if !names.insert(alias) {
            return Err(format!("duplicate command name or alias {alias:?}"));
        }
    }
    validate_description(&spec.description)?;
    if !spec
        .platforms
        .iter()
        .any(|platform| platform == std::env::consts::OS)
    {
        return Err(format!(
            "command {:?} is not available on this platform",
            spec.name
        ));
    }
    let mut recipe_ids = HashSet::new();
    let recipes: Vec<_> = spec
        .recipes
        .iter()
        .filter(|recipe| {
            recipe.platforms.is_empty()
                || recipe
                    .platforms
                    .iter()
                    .any(|platform| platform == std::env::consts::OS)
        })
        .map(|recipe| {
            if !recipe_ids.insert(recipe.id.as_str()) {
                return Err(format!("duplicate recipe id {:?}", recipe.id));
            }
            compile_recipe(recipe)
        })
        .collect::<Result<_, _>>()?;
    if spec.default == "run_current" {
        let risk: RiskLevel = spec.risk.into();
        if spec.requires_arguments || !matches!(risk, RiskLevel::ReadOnly | RiskLevel::Low) {
            return Err("run_current requires a complete low-risk command".into());
        }
    } else if let Some(recipe_id) = spec.default.strip_prefix("recipe:") {
        if !recipes.iter().any(|recipe| recipe.id == recipe_id) {
            return Err(format!("default recipe {recipe_id:?} does not exist"));
        }
    } else {
        return Err(format!("unknown default action {:?}", spec.default));
    }
    Ok(CompiledCommand {
        id: spec.id.clone(),
        name: spec.name.clone(),
        aliases: spec.aliases.clone(),
        description: spec.description.clone(),
        requires_arguments: spec.requires_arguments,
        risk: spec.risk.into(),
        default: spec.default.clone(),
        recipes,
        provenance: path.to_owned(),
    })
}

fn compile_recipe(spec: &RecipeSpec) -> Result<CompiledRecipe, String> {
    validate_identifier(&spec.id, "recipe id")?;
    validate_description(&spec.description)?;
    if !matches!(spec.activation.as_str(), "insert" | "continue") {
        return Err(format!("invalid activation {:?}", spec.activation));
    }
    let placeholders = placeholders(&spec.template)?;
    let unique_placeholders: HashSet<_> = placeholders.iter().map(String::as_str).collect();
    if unique_placeholders.len() != placeholders.len() {
        return Err(format!("recipe {:?} uses a slot more than once", spec.id));
    }
    let defined: HashSet<_> = spec.slots.iter().map(|slot| slot.name.as_str()).collect();
    if defined.len() != spec.slots.len() {
        return Err(format!("recipe {:?} has duplicate slot names", spec.id));
    }
    if placeholders
        .iter()
        .any(|name| !defined.contains(name.as_str()))
        || defined
            .iter()
            .any(|name| !placeholders.iter().any(|value| value == name))
    {
        return Err(format!(
            "recipe {:?} template and slots do not match",
            spec.id
        ));
    }
    for slot in &spec.slots {
        let expected_provider = match slot.kind {
            crate::specs::SpecSlotKind::Process => "process",
            crate::specs::SpecSlotKind::Interface => "network_interface",
            crate::specs::SpecSlotKind::Port | crate::specs::SpecSlotKind::Value => "value",
            _ => "filesystem",
        };
        if slot.provider != expected_provider {
            return Err(format!(
                "slot {:?} must use provider {expected_provider:?}",
                slot.name
            ));
        }
    }
    let first_placeholder = spec.template.find("${");
    let prefix = first_placeholder.map_or_else(
        || spec.template.clone(),
        |index| spec.template[..index].to_owned(),
    );
    let next_slot = placeholders.first().and_then(|name| {
        spec.slots
            .iter()
            .find(|slot| &slot.name == name)
            .map(|slot| slot.kind.into())
    });
    Ok(CompiledRecipe {
        id: spec.id.clone(),
        template: spec.template.clone(),
        prefix,
        description: spec.description.clone(),
        risk: spec.risk.into(),
        next_slot,
        complete: placeholders.is_empty(),
    })
}

fn placeholders(template: &str) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err("template contains an unterminated slot".into());
        };
        let name = &after[..end];
        validate_identifier(name, "slot name")?;
        names.push(name.to_owned());
        rest = &after[end + 1..];
    }
    Ok(names)
}

fn validate_identifier(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(format!("invalid {label} {value:?}"));
    }
    Ok(())
}

fn validate_command_name(value: &str) -> Result<(), String> {
    validate_identifier(value, "command name")
}

fn validate_description(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 240 || value.chars().any(char::is_control) {
        return Err("description is empty, too long, or contains control characters".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_compile_and_cover_required_commands() {
        let registry = SpecRegistry::load(None);
        let unexpected: Vec<_> = registry
            .diagnostics()
            .iter()
            .filter(|diagnostic| {
                !diagnostic
                    .message
                    .contains("not available on this platform")
            })
            .collect();
        assert!(unexpected.is_empty(), "{unexpected:#?}");
        for name in ["ls", "df", "tar", "lsof", "ifconfig", "ps", "kill"] {
            assert!(registry.get(name).is_some(), "missing {name}");
        }
        let tar = registry.get("tar").expect("tar spec");
        assert_eq!(tar.recipes[0].prefix, "tar -czf ");
        assert_eq!(tar.recipes[0].next_slot, Some(SlotKind::NewFile));
    }

    #[test]
    fn unsafe_run_current_and_mismatched_slots_are_rejected() {
        let document = r#"
schema = 1
[[commands]]
id = "bad.kill"
name = "badkill"
description = "bad"
platforms = ["macos", "linux"]
requires_arguments = true
risk = "high"
default = "run_current"
"#;
        let mut registry = SpecRegistry::default();
        registry.load_text(Path::new("bad.toml"), document, true);
        assert!(registry.get("badkill").is_none());
        assert_eq!(registry.diagnostics()[0].code, "HK-SPC-010");
    }

    #[test]
    fn user_override_is_atomic_and_records_provenance() {
        let directory = tempfile::tempdir().expect("spec directory");
        let override_path = directory.path().join("ls.toml");
        fs::write(
            &override_path,
            r#"
schema = 1
[[commands]]
id = "user.ls"
name = "ls"
description = "用户定义的目录列表"
platforms = ["macos", "linux"]
requires_arguments = false
risk = "read_only"
default = "run_current"
replaces = "core.ls"
"#,
        )
        .expect("override spec");
        let registry = SpecRegistry::load(Some(directory.path()));
        let command = registry.get("ls").expect("overridden ls");
        assert_eq!(command.id, "user.ls");
        assert_eq!(command.provenance, override_path);

        fs::write(
            directory.path().join("collision.toml"),
            r#"
schema = 1
[[commands]]
id = "user.collision"
name = "other"
aliases = ["df"]
description = "冲突别名"
platforms = ["macos", "linux"]
requires_arguments = false
risk = "read_only"
default = "run_current"
"#,
        )
        .expect("collision spec");
        let registry = SpecRegistry::load(Some(directory.path()));
        assert!(registry.get("other").is_none());
        assert_eq!(registry.get("df").expect("builtin df").id, "core.df");
        assert!(
            registry
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.code == "HK-SPC-012")
        );
    }

    #[test]
    fn only_current_platform_recipes_are_compiled() {
        let registry = SpecRegistry::load(None);
        let ls = registry.get("ls").expect("ls");
        let ids: HashSet<_> = ls.recipes.iter().map(|recipe| recipe.id.as_str()).collect();
        if cfg!(target_os = "linux") {
            assert!(ids.contains("directories_first"));
            assert!(!ids.contains("extended_attributes"));
        } else if cfg!(target_os = "macos") {
            assert!(ids.contains("extended_attributes"));
            assert!(!ids.contains("directories_first"));
        }
    }

    #[test]
    fn user_specs_are_size_bounded_and_parse_errors_do_not_echo_source() {
        let directory = tempfile::tempdir().expect("spec directory");
        fs::write(
            directory.path().join("large.toml"),
            vec![b'x'; USER_SPEC_MAX_BYTES as usize + 1],
        )
        .expect("large spec");
        fs::write(
            directory.path().join("malformed.toml"),
            "schema = 1\nprivate_value = 'do-not-echo'\n[[commands]\n",
        )
        .expect("malformed spec");
        let registry = SpecRegistry::load(Some(directory.path()));
        assert!(registry.diagnostics().iter().any(|diagnostic| {
            diagnostic.code == "HK-SPC-001" && diagnostic.message.contains("1 MiB")
        }));
        let parse = registry
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.code == "HK-SPC-002")
            .expect("parse diagnostic");
        assert!(!parse.message.contains("do-not-echo"));
    }
}
