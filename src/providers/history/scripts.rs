//! Package scripts, workspace selection, and static Maven argument validation.
use super::HistoryProvider;
use crate::providers::command_help::one_edit_or_adjacent_transposition;
use crate::{completion::CompletionContext, project::WorkspaceMember};
use std::path::Path;

impl HistoryProvider {
    pub(super) fn node_run_script_is_plausible(
        &self,
        context: &CompletionContext,
        words: &[&str],
        command_index: usize,
    ) -> Option<bool> {
        let command = crate::providers::executable_basename(words.get(command_index).copied()?);
        if !matches!(command, "node" | "nodejs") {
            return None;
        }
        let arguments = words.get(command_index + 1..).unwrap_or_default();
        let mut index = 0;
        while let Some(argument) = arguments.get(index).copied() {
            let script = if let Some(script) = argument.strip_prefix("--run=") {
                Some(script)
            } else if argument == "--run" {
                Some(arguments.get(index + 1).copied()?)
            } else {
                None
            };
            if let Some(script) = script {
                if script.is_empty()
                    || script
                        .chars()
                        .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
                {
                    return None;
                }
                let project_dir = crate::providers::wrapper_working_directory_before(
                    context,
                    words,
                    command_index,
                );
                return self.manifest_has_script(command, &project_dir, script);
            }
            if argument == "--" || argument == "-" || !argument.starts_with('-') {
                return None;
            }
            if crate::providers::project::node_option_takes_separate_value(argument) {
                if index + 1 >= arguments.len() {
                    return None;
                }
                index += 2;
            } else {
                index += 1;
            }
        }
        None
    }

    pub(super) fn manager_script_is_plausible(
        &self,
        context: &CompletionContext,
        words: &[&str],
        command_index: usize,
    ) -> Option<bool> {
        let mut invocation = parse_manager_history_invocation(context, words, command_index)?;
        let script = if crate::providers::is_script_keyword(invocation.manager, &invocation.command)
        {
            invocation.operands.first()?.clone()
        } else if invocation.manager.name == "yarn" && invocation.command == "workspace" {
            let member = invocation.operands.first()?.clone();
            let mut operands = invocation.operands.iter().skip(1);
            let script = match operands.next() {
                Some(word) if word == "run" => operands.next()?.clone(),
                Some(word) => word.clone(),
                None => return None,
            };
            let selector = (crate::providers::WorkspaceStyle::YarnWorkspace, member);
            invocation.selector = Some(selector.clone());
            invocation.selectors.push(selector);
            invocation.selector_count = invocation.selectors.len();
            script
        } else if invocation.manager.keyword.is_none() && invocation.command == "run" {
            invocation.operands.first()?.clone()
        } else if invocation.manager.keyword.is_none()
            && !invocation
                .manager
                .subcommands
                .iter()
                .any(|(subcommand, _)| *subcommand == invocation.command)
        {
            invocation.command.clone()
        } else {
            return None;
        };
        if script.starts_with('-')
            || script
                .chars()
                .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
        {
            return None;
        }
        self.script_is_present(&invocation, &script)
    }

    pub(super) fn script_is_present(
        &self,
        invocation: &ManagerHistoryInvocation,
        script: &str,
    ) -> Option<bool> {
        if invocation.selector_count > 1 {
            return self.multiple_selector_script_is_present(invocation, script);
        }
        let has_workspace_scope =
            invocation.selector.is_some() || invocation.recursive || invocation.workspace_root;
        if !has_workspace_scope {
            // `--if-present` only turns a missing script into a successful
            // no-op. A recommendation is still useful only when this project
            // actually defines the script.
            return self.manifest_has_script(
                invocation.manager.name,
                &invocation.project_dir,
                script,
            );
        }

        let Some(workspace) = self.workspaces.load(&invocation.project_dir) else {
            // Recursive completion falls back to the nearest manifest when
            // this is not actually a workspace, matching ProjectProvider.
            return if invocation.selector.is_none() {
                self.manifest_has_script(invocation.manager.name, &invocation.project_dir, script)
            } else {
                Some(false)
            };
        };

        if invocation.workspace_root {
            return self.workspace_root_has_script(
                invocation.manager.name,
                &workspace.root,
                script,
            );
        }

        if let Some((_, selector)) = invocation.selector.as_ref() {
            if !literal_workspace_selector(selector) {
                return None;
            }
            let selected: Vec<_> = workspace
                .members
                .iter()
                .filter(|member| workspace_member_matches(&workspace.root, member, selector))
                .collect();
            if selected.is_empty() {
                return Some(false);
            }
            return Some(
                selected
                    .iter()
                    .any(|member| member.scripts.contains_key(script)),
            );
        }

        if !invocation.recursive {
            return self.manifest_has_script(
                invocation.manager.name,
                &invocation.project_dir,
                script,
            );
        }

        let mut found = Vec::new();
        if invocation.include_workspace_root {
            found.push(self.workspace_root_has_script(
                invocation.manager.name,
                &workspace.root,
                script,
            )?);
        }
        found.extend(
            workspace
                .members
                .iter()
                .map(|member| member.scripts.contains_key(script)),
        );
        if found.is_empty() {
            return Some(false);
        }
        if invocation.manager.name == "npm" && !invocation.if_present {
            Some(found.into_iter().all(std::convert::identity))
        } else {
            Some(found.into_iter().any(std::convert::identity))
        }
    }

    pub(super) fn multiple_selector_script_is_present(
        &self,
        invocation: &ManagerHistoryInvocation,
        script: &str,
    ) -> Option<bool> {
        if !invocation
            .selectors
            .iter()
            .all(|(_, selector)| literal_workspace_selector(selector))
        {
            return None;
        }
        let Some(workspace) = self.workspaces.load(&invocation.project_dir) else {
            return Some(false);
        };
        let selected: Vec<_> = workspace
            .members
            .iter()
            .filter(|member| {
                invocation.selectors.iter().any(|(_, selector)| {
                    workspace_member_matches(&workspace.root, member, selector)
                })
            })
            .collect();
        if selected.is_empty() {
            return Some(false);
        }
        if invocation.manager.name == "npm" && !invocation.if_present {
            Some(
                selected
                    .iter()
                    .all(|member| member.scripts.contains_key(script)),
            )
        } else {
            Some(
                selected
                    .iter()
                    .any(|member| member.scripts.contains_key(script)),
            )
        }
    }

    pub(super) fn manifest_has_script(
        &self,
        manager: &str,
        directory: &Path,
        script: &str,
    ) -> Option<bool> {
        if manager == "deno" {
            return match self.projects.load_deno_nearest(directory) {
                Ok(Some(manifest)) => Some(manifest.tasks.contains_key(script)),
                Ok(None) => Some(false),
                Err(_) => Some(false),
            };
        }
        match self.projects.load_nearest(directory) {
            Ok(Some(manifest)) => Some(manifest.scripts.contains_key(script)),
            Ok(None) => Some(false),
            Err(_) => Some(false),
        }
    }

    pub(super) fn workspace_root_has_script(
        &self,
        manager: &str,
        root: &Path,
        script: &str,
    ) -> Option<bool> {
        if manager == "deno" {
            return match self.projects.load_deno_nearest(root) {
                Ok(Some(manifest)) => Some(
                    manifest.path.parent() == Some(root) && manifest.tasks.contains_key(script),
                ),
                Ok(None) => Some(false),
                Err(_) => Some(false),
            };
        }
        match self.projects.load_nearest(root) {
            Ok(Some(manifest)) if manifest.path.parent() == Some(root) => {
                Some(manifest.scripts.contains_key(script))
            }
            Ok(Some(_)) | Ok(None) => Some(false),
            Err(_) => Some(false),
        }
    }
}

pub(super) fn maven_history_arguments_are_plausible(
    command: &str,
    arguments: &[&str],
    known_non_failure: bool,
) -> Option<bool> {
    if !matches!(
        crate::providers::executable_basename(command),
        "mvn" | "mvnw" | "mvnDebug"
    ) {
        return None;
    }
    if known_non_failure {
        return Some(true);
    }
    let mut index = 0;
    while let Some(word) = arguments.get(index).copied() {
        if word == "--" {
            index += 1;
            continue;
        }
        if word.starts_with("-D")
            || word.starts_with("-P")
            || word.starts_with("-pl") && word.len() > 3
            || word.starts_with("-T") && word.len() > 2
        {
            index += 1;
            continue;
        }
        if matches!(
            word,
            "-f" | "--file"
                | "-s"
                | "--settings"
                | "-gs"
                | "--global-settings"
                | "-t"
                | "--toolchains"
                | "-pl"
                | "--projects"
                | "-rf"
                | "--resume-from"
                | "-T"
                | "--threads"
        ) {
            if index + 1 >= arguments.len() {
                return Some(false);
            }
            index += 2;
            continue;
        }
        if word.starts_with('-') {
            index += 1;
            continue;
        }
        if word.contains([':', '$', '`', '*', '?', '[', '{']) {
            index += 1;
            continue;
        }
        if crate::providers::toolchain::MAVEN_PHASES
            .iter()
            .any(|(phase, _)| *phase == word)
        {
            index += 1;
            continue;
        }
        if crate::providers::toolchain::MAVEN_PHASES
            .iter()
            .any(|(phase, _)| one_edit_or_adjacent_transposition(word, phase))
        {
            return Some(false);
        }
        index += 1;
    }
    Some(true)
}

pub(super) fn manager_command_arguments<'a>(
    manager: &str,
    arguments: &'a [&'a str],
) -> Option<&'a [&'a str]> {
    let mut index = 0;
    while let Some(argument) = arguments.get(index).copied() {
        if argument == "--" {
            return arguments
                .get(index + 1..)
                .filter(|arguments| !arguments.is_empty());
        }
        if crate::providers::attached_manager_value(manager, argument).is_some()
            || crate::providers::attached_manager_boolean(manager, argument).is_some()
            || crate::providers::manager_flag_without_value(manager, argument)
        {
            index += 1;
            continue;
        }
        if crate::providers::manager_value_option(manager, argument).is_some() {
            index += 2;
            continue;
        }
        if argument.starts_with('-') {
            return None;
        }
        return Some(&arguments[index..]);
    }
    None
}

#[derive(Clone, Debug)]
pub(super) struct ManagerHistoryOptions {
    pub(super) index: usize,
    pub(super) project_dir: std::path::PathBuf,
    pub(super) selector: Option<(crate::providers::WorkspaceStyle, String)>,
    pub(super) selectors: Vec<(crate::providers::WorkspaceStyle, String)>,
    pub(super) selector_count: usize,
    pub(super) workspace_root: bool,
    pub(super) recursive: bool,
    pub(super) include_workspace_root: bool,
    pub(super) if_present: bool,
}

impl ManagerHistoryOptions {
    pub(super) fn new(project_dir: &Path) -> Self {
        Self {
            index: 0,
            project_dir: project_dir.to_owned(),
            selector: None,
            selectors: Vec::new(),
            selector_count: 0,
            workspace_root: false,
            recursive: false,
            include_workspace_root: false,
            if_present: false,
        }
    }
}

#[derive(Clone)]
pub(super) struct ManagerHistoryInvocation {
    pub(super) manager: &'static crate::providers::ManagerSpec,
    pub(super) command: String,
    pub(super) operands: Vec<String>,
    pub(super) project_dir: std::path::PathBuf,
    pub(super) selector: Option<(crate::providers::WorkspaceStyle, String)>,
    pub(super) selectors: Vec<(crate::providers::WorkspaceStyle, String)>,
    pub(super) selector_count: usize,
    pub(super) workspace_root: bool,
    pub(super) recursive: bool,
    pub(super) include_workspace_root: bool,
    pub(super) if_present: bool,
}

pub(super) fn parse_manager_history_options(
    manager: &str,
    arguments: &[&str],
    directory_base: &Path,
    mut state: ManagerHistoryOptions,
    allow_double_dash: bool,
) -> Option<ManagerHistoryOptions> {
    state.index = 0;
    while let Some(argument) = arguments.get(state.index).copied() {
        if argument == "--" {
            if !allow_double_dash {
                return None;
            }
            state.index += 1;
            break;
        }
        if let Some((kind, value)) = crate::providers::attached_manager_value(manager, argument) {
            match kind {
                crate::providers::ManagerValue::Directory => {
                    state.project_dir = crate::providers::resolve_directory(directory_base, value);
                }
                crate::providers::ManagerValue::Workspace(style) => {
                    state.selector_count = state.selector_count.saturating_add(1);
                    let selector = (style, value.to_owned());
                    state.selector = Some(selector.clone());
                    state.selectors.push(selector);
                }
                crate::providers::ManagerValue::Other => {}
            }
            state.index += 1;
            continue;
        }
        if let Some((flag, enabled)) = crate::providers::attached_manager_boolean(manager, argument)
        {
            crate::providers::apply_manager_boolean(
                manager,
                flag,
                enabled,
                &mut state.workspace_root,
                &mut state.recursive,
                &mut state.include_workspace_root,
                &mut state.if_present,
            );
            state.index += 1;
            continue;
        }
        if let Some(kind) = crate::providers::manager_value_option(manager, argument) {
            let value = arguments.get(state.index + 1).copied()?;
            match kind {
                crate::providers::ManagerValue::Directory => {
                    state.project_dir = crate::providers::resolve_directory(directory_base, value);
                }
                crate::providers::ManagerValue::Workspace(style) => {
                    state.selector_count = state.selector_count.saturating_add(1);
                    let selector = (style, value.to_owned());
                    state.selector = Some(selector.clone());
                    state.selectors.push(selector);
                }
                crate::providers::ManagerValue::Other => {}
            }
            state.index += 2;
            continue;
        }
        if argument.starts_with('-') {
            if !crate::providers::manager_flag_without_value(manager, argument) {
                return None;
            }
            crate::providers::apply_manager_flag(
                manager,
                argument,
                &mut state.workspace_root,
                &mut state.recursive,
                &mut state.include_workspace_root,
                &mut state.if_present,
            );
            state.index += 1;
            continue;
        }
        break;
    }
    Some(state)
}

pub(super) fn parse_manager_history_invocation(
    context: &CompletionContext,
    words: &[&str],
    command_index: usize,
) -> Option<ManagerHistoryInvocation> {
    let manager_name = crate::providers::executable_basename(words.get(command_index).copied()?);
    let manager = crate::providers::MANAGERS
        .iter()
        .find(|manager| manager.name == manager_name)?;
    let arguments = words.get(command_index + 1..).unwrap_or_default();
    let invocation_dir =
        crate::providers::wrapper_working_directory_before(context, words, command_index);
    let mut options = parse_manager_history_options(
        manager.name,
        arguments,
        &invocation_dir,
        ManagerHistoryOptions::new(&invocation_dir),
        true,
    )?;
    let command_offset = options.index;
    let command = arguments.get(command_offset)?.to_string();
    let mut operand_offset = command_offset + 1;
    if matches!(manager.name, "npm" | "pnpm")
        && crate::providers::is_script_keyword(manager, &command)
    {
        options = parse_manager_history_options(
            manager.name,
            arguments.get(operand_offset..).unwrap_or_default(),
            &invocation_dir,
            options,
            false,
        )?;
        operand_offset += options.index;
    }
    Some(ManagerHistoryInvocation {
        manager,
        command,
        operands: arguments
            .get(operand_offset..)
            .unwrap_or_default()
            .iter()
            .map(|word| (*word).to_owned())
            .collect(),
        project_dir: options.project_dir,
        selector: options.selector,
        selectors: options.selectors,
        selector_count: options.selector_count,
        workspace_root: options.workspace_root,
        recursive: options.recursive,
        include_workspace_root: options.include_workspace_root,
        if_present: options.if_present,
    })
}

pub(super) fn literal_workspace_selector(selector: &str) -> bool {
    !selector.is_empty()
        && !selector.starts_with('!')
        && !selector.starts_with("...")
        && !selector
            .chars()
            .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
}

pub(super) fn workspace_member_matches(
    root: &Path,
    member: &WorkspaceMember,
    selector: &str,
) -> bool {
    let selector_path = Path::new(selector.strip_prefix("./").unwrap_or(selector));
    member.name == selector
        || member.directory == selector_path
        || member
            .directory
            .strip_prefix(root)
            .ok()
            .is_some_and(|path| path == selector_path)
        || member
            .directory
            .file_name()
            .is_some_and(|name| name == std::ffi::OsStr::new(selector))
}

pub(super) fn manager_subcommand_has_commands(manager: &str, command: &str) -> bool {
    matches!(
        (manager, command),
        ("pnpm", "cache" | "config" | "env" | "runtime" | "store")
            | ("npm", "cache" | "config" | "pkg" | "token")
            | ("yarn", "config" | "set" | "workspaces")
            | ("bun", "pm")
    )
}
