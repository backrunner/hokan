use std::{
    collections::{BTreeMap, HashSet},
    env,
    ffi::OsString,
    fs,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Deserialize;

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, SlotKind, TextEdit,
    },
    platform::CommandPathCache,
    terminal::RiskLevel,
};

use super::{
    CommandHelpCache, argument_progress,
    command_help::{
        CommandHelp, HelpEntry, history_arguments_are_plausible, one_edit_or_adjacent_transposition,
    },
};

const PYTHON_METADATA_TIMEOUT: Duration = Duration::from_millis(1200);
const PYTHON_METADATA_MAX_BYTES: usize = 64 * 1024;
const PYTHON_MODULE_HELP_TIMEOUT: Duration = Duration::from_millis(1200);
const PYTHON_MODULE_HELP_MAX_BYTES: usize = 1024 * 1024;
const ENVIRONMENT_SCAN_BUDGET: Duration = Duration::from_millis(180);
const PROJECT_SCAN_BUDGET: Duration = Duration::from_millis(40);
const ENVIRONMENT_ENTRY_LIMIT: usize = 12_000;
const PROJECT_ENTRY_LIMIT: usize = 3_000;
const MODULE_DEPTH_LIMIT: usize = 6;
const MODULE_CANDIDATE_LIMIT: usize = 500;
const PTH_FILE_LIMIT: usize = 128;
const PTH_MAX_BYTES: u64 = 256 * 1024;

const DESC_STDLIB_ENTRY: &str = "Python standard-library entry point";
const DESC_STDLIB_MODULE: &str = "Runnable Python standard-library module";
const DESC_SITE_ENTRY: &str = "Installed Python package entry point";
const DESC_SITE_MODULE: &str = "Runnable installed Python module";
const DESC_USER_ENTRY: &str = "User-site Python package entry point";
const DESC_USER_MODULE: &str = "Runnable user-site Python module";
const DESC_PATH_ENTRY: &str = "PYTHONPATH package entry point";
const DESC_PATH_MODULE: &str = "Runnable PYTHONPATH module";
const DESC_PROJECT_ENTRY: &str = "Project Python package entry point";
const DESC_PROJECT_MODULE: &str = "Runnable project Python module";

const STDLIB_CLI_MODULES: &[&str] = &[
    "cProfile",
    "calendar",
    "compileall",
    "dis",
    "doctest",
    "ensurepip",
    "gzip",
    "http.server",
    "idlelib",
    "json.tool",
    "pdb",
    "platform",
    "pydoc",
    "py_compile",
    "quopri",
    "site",
    "tarfile",
    "timeit",
    "tokenize",
    "trace",
    "tracemalloc",
    "venv",
    "webbrowser",
    "zipapp",
    "zipfile",
];

const PYTHON_METADATA_SCRIPT: &str = r#"import json, site, sysconfig
paths = sysconfig.get_paths()
user = site.getusersitepackages()
if isinstance(user, str):
    user = [user]
print(json.dumps({
    "stdlib": [paths.get("stdlib"), paths.get("platstdlib")],
    "site": [paths.get("purelib"), paths.get("platlib")],
    "user": user,
}))
"#;

pub struct PythonModuleProvider {
    commands: Arc<CommandPathCache>,
    cache: Arc<CommandHelpCache>,
}

impl PythonModuleProvider {
    #[must_use]
    pub fn new(commands: Arc<CommandPathCache>, cache: Arc<CommandHelpCache>) -> Self {
        Self { commands, cache }
    }
}

impl CandidateProvider for PythonModuleProvider {
    fn id(&self) -> &'static str {
        "python_module"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        let module_position = python_module_position(context);
        let help_invocation = completed_python_module_invocation(context);
        if module_position.is_none() && help_invocation.is_none() {
            return false;
        }
        if module_position
            .as_ref()
            .is_some_and(|position| !valid_module_prefix(position.prefix))
        {
            return false;
        }
        let Some(command) = context.command() else {
            return false;
        };
        let Some(executable) = super::resolved_executable_path(context, &self.commands) else {
            return false;
        };
        let key = python_module_cache_key(command);
        if self.cache.peek(&key).is_none() {
            let fetch_path = executable.clone();
            self.cache
                .request_with_path(&key, Some(executable.clone()), move |_| {
                    discover_environment_modules(&fetch_path)
                });
        }
        if module_position.is_some() {
            return true;
        }
        !matches!(
            lookup_python_module_help(
                context,
                &self.cache,
                help_invocation.expect("checked above"),
                Some(executable),
                true,
            ),
            PythonModuleHelpLookup::None
        )
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Some(position) = python_module_position(context) else {
            return complete_python_module_help(context, &self.cache);
        };
        if !valid_module_prefix(position.prefix) {
            return ProviderOutput::default();
        }
        let Some(command) = context.command() else {
            return ProviderOutput::default();
        };
        let mut modules = BTreeMap::new();
        if let Some(cached) = self.cache.peek(&python_module_cache_key(command)) {
            for entry in &cached.subcommands {
                if position.mode.allows_description(&entry.description) {
                    insert_module(
                        &mut modules,
                        ModuleRecord {
                            name: entry.name.clone(),
                            description: entry.description.clone(),
                            priority: description_priority(&entry.description),
                            high_confidence: description_is_entry(&entry.description)
                                || is_stdlib_cli_module(&entry.name),
                        },
                    );
                }
            }
        }

        let working_directory = super::invocation_working_directory(context);
        let mut scanner = ModuleScanner::new(PROJECT_SCAN_BUDGET, PROJECT_ENTRY_LIMIT);
        if !position.mode.safe_path {
            scanner.collect(&working_directory, ModuleOrigin::Project, &mut modules);
        }
        if let Some(paths) = position.python_path.as_ref() {
            for path in env::split_paths(paths).take(8) {
                let path = if path.as_os_str().is_empty() {
                    working_directory.clone()
                } else if path.is_absolute() {
                    path
                } else {
                    working_directory.join(path)
                };
                scanner.collect(&path, ModuleOrigin::PythonPath, &mut modules);
            }
        }

        let exact_module = modules.contains_key(position.prefix);
        let mut modules: Vec<_> = modules
            .into_values()
            .filter(|module| {
                module.name.starts_with(position.prefix)
                    && (!exact_module || module.name == position.prefix)
                    && (!position.prefix.is_empty() || module.high_confidence)
            })
            .collect();
        modules.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.name.cmp(&right.name))
        });
        let candidates = modules
            .into_iter()
            .take(MODULE_CANDIDATE_LIMIT)
            .enumerate()
            .map(|(index, module)| {
                let replacement = format!("{}{}", position.replacement_prefix, module.name);
                let display = crate::parser::apply_edit(
                    &context.buffer.text,
                    position.replacement.clone(),
                    &replacement,
                )
                .unwrap_or_else(|_| module.name.clone());
                let mut candidate = Candidate::new(
                    context.query_id,
                    display,
                    module.description,
                    Some(TextEdit {
                        range: position.replacement.clone(),
                        replacement,
                        cursor_after: CursorPlacement::End,
                    }),
                    CandidateAction::InsertAndContinue {
                        next_slot: SlotKind::Value,
                    },
                    CandidateSource::CommandHelp,
                    CandidateKind::Command,
                    Completeness::NeedsInput {
                        slot: SlotKind::Value,
                    },
                    RiskLevel::Low,
                    format!("python-module:{}", module.name),
                );
                candidate.score.spec_priority = module
                    .priority
                    .saturating_sub(i16::try_from(index.min(50)).unwrap_or_default());
                candidate
            })
            .collect();
        ProviderOutput {
            candidates,
            diagnostics: Vec::new(),
        }
    }
}

struct CompletedPythonModule<'a> {
    command: &'a str,
    module: &'a str,
    tail: Vec<&'a str>,
    mode: PythonMode,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PythonModuleHelpPosition {
    Flags,
    Subcommands,
}

struct PythonModuleHelpTarget {
    help: Arc<CommandHelp>,
    position: PythonModuleHelpPosition,
    command: String,
    module: String,
    scope: Vec<String>,
}

enum PythonModuleHelpLookup {
    Ready(PythonModuleHelpTarget),
    Pending,
    None,
}

enum PythonModuleHelpStep<'a> {
    Scope { word: &'a str, consumed: usize },
    Ready,
    Blocked,
}

fn completed_python_module_invocation(
    context: &CompletionContext,
) -> Option<CompletedPythonModule<'_>> {
    let command = context.command()?;
    if !is_python_command(command) {
        return None;
    }
    let (words, position) = argument_progress(context)?;
    let before = words.get(1..=position).unwrap_or_default();
    let parsed = parse_python_module_arguments(before)?;
    let (mode, _) = parsed.mode.apply_environment(context);
    Some(CompletedPythonModule {
        command,
        module: parsed.module,
        tail: parsed.tail,
        mode,
    })
}

struct ParsedPythonModule<'a> {
    module: &'a str,
    tail: Vec<&'a str>,
    mode: PythonMode,
}

fn parse_python_module_arguments<'a>(arguments: &[&'a str]) -> Option<ParsedPythonModule<'a>> {
    let mut mode = PythonMode::default();
    let mut index = 0;
    while let Some(word) = arguments.get(index).copied() {
        match word {
            "-m" => {
                let module = arguments.get(index + 1).copied()?;
                if !valid_completed_module_name(module) {
                    return None;
                }
                return Some(ParsedPythonModule {
                    module,
                    tail: arguments.get(index + 2..).unwrap_or_default().to_vec(),
                    mode,
                });
            }
            "-c" | "--" => return None,
            _ if word
                .strip_prefix("-m")
                .is_some_and(|module| !module.is_empty()) =>
            {
                let module = word.strip_prefix("-m")?;
                if !valid_completed_module_name(module) {
                    return None;
                }
                return Some(ParsedPythonModule {
                    module,
                    tail: arguments.get(index + 1..).unwrap_or_default().to_vec(),
                    mode,
                });
            }
            _ if word
                .strip_prefix("-c")
                .is_some_and(|command| !command.is_empty()) =>
            {
                return None;
            }
            "-h" | "--help" | "-?" | "-V" | "--version" | "--help-env" | "--help-xoptions"
            | "--help-all" => return None,
            "-E" => mode.ignore_environment = true,
            "-I" => {
                mode.ignore_environment = true;
                mode.no_user_site = true;
                mode.safe_path = true;
            }
            "-P" => mode.safe_path = true,
            "-s" => mode.no_user_site = true,
            "-S" => mode.no_site = true,
            "-W" | "-X" | "--check-hash-based-pycs" => {
                index += 1;
                if index >= arguments.len() {
                    return None;
                }
            }
            _ if python_flag_without_value(word)
                || apply_python_short_flag_cluster(word, &mut mode)
                || word.starts_with("-W") && word.len() > 2
                || word.starts_with("-X") && word.len() > 2
                || word.starts_with("--check-hash-based-pycs=") => {}
            _ => return None,
        }
        index += 1;
    }
    None
}

fn valid_completed_module_name(module: &str) -> bool {
    !module.ends_with('.') && valid_module_prefix(module)
}

fn lookup_python_module_help(
    context: &CompletionContext,
    cache: &Arc<CommandHelpCache>,
    invocation: CompletedPythonModule<'_>,
    executable: Option<PathBuf>,
    request_missing: bool,
) -> PythonModuleHelpLookup {
    let metadata_key = python_module_cache_key(invocation.command);
    let Some(metadata) = cache.peek(&metadata_key) else {
        return if cache.is_pending(&metadata_key) {
            PythonModuleHelpLookup::Pending
        } else {
            PythonModuleHelpLookup::None
        };
    };
    if !metadata.subcommands.iter().any(|entry| {
        entry.name == invocation.module
            && invocation.mode.allows_description(&entry.description)
            && is_cached_environment_description(&entry.description)
    }) {
        return PythonModuleHelpLookup::None;
    }

    let mut scope = Vec::new();
    let mut remaining = invocation.tail.as_slice();
    loop {
        let key = python_module_help_cache_key(invocation.command, invocation.module, &scope);
        let Some(help) = cache.peek(&key) else {
            if request_missing {
                let Some(program) = executable.clone() else {
                    return PythonModuleHelpLookup::None;
                };
                let module = invocation.module.to_owned();
                let fetch_scope = scope.clone();
                let fetch_program = program.clone();
                cache.request_with_path(&key, Some(program), move |_| {
                    fetch_python_module_help(&fetch_program, &module, &fetch_scope)
                });
                return PythonModuleHelpLookup::Pending;
            }
            return if cache.is_pending(&key) {
                PythonModuleHelpLookup::Pending
            } else {
                PythonModuleHelpLookup::None
            };
        };
        match python_module_help_step(&help, remaining) {
            PythonModuleHelpStep::Scope { word, consumed } => {
                if scope.len() >= 3 {
                    return PythonModuleHelpLookup::None;
                }
                scope.push(word.to_owned());
                remaining = &remaining[consumed..];
            }
            PythonModuleHelpStep::Blocked => return PythonModuleHelpLookup::None,
            PythonModuleHelpStep::Ready => {
                let prefix = context.parsed.current_prefix.as_str();
                let position = if prefix.starts_with('-') {
                    if module_help_flag_usage(&help, prefix)
                        .is_some_and(|(entry, attached)| entry.takes_value && attached)
                    {
                        return PythonModuleHelpLookup::None;
                    }
                    PythonModuleHelpPosition::Flags
                } else if !help.subcommands.is_empty() {
                    PythonModuleHelpPosition::Subcommands
                } else {
                    return PythonModuleHelpLookup::None;
                };
                return PythonModuleHelpLookup::Ready(PythonModuleHelpTarget {
                    help,
                    position,
                    command: invocation.command.to_owned(),
                    module: invocation.module.to_owned(),
                    scope,
                });
            }
        }
    }
}

fn python_module_help_step<'a>(
    help: &CommandHelp,
    arguments: &'a [&'a str],
) -> PythonModuleHelpStep<'a> {
    let mut index = 0;
    while let Some(word) = arguments.get(index).copied() {
        if word == "--" || has_dynamic_shell_syntax(word) {
            return PythonModuleHelpStep::Blocked;
        }
        if word.starts_with('-') && word != "-" {
            let Some((entry, attached_value)) = module_help_flag_usage(help, word) else {
                return PythonModuleHelpStep::Blocked;
            };
            index += 1;
            if entry.takes_value && !attached_value {
                if index >= arguments.len() {
                    return PythonModuleHelpStep::Blocked;
                }
                index += 1;
            }
            continue;
        }
        if help.subcommands.iter().any(|entry| entry.name == word)
            || help.subcommand_aliases.iter().any(|alias| alias == word)
        {
            return PythonModuleHelpStep::Scope {
                word,
                consumed: index + 1,
            };
        }
        if help.accepts_positionals {
            index += 1;
            continue;
        }
        return PythonModuleHelpStep::Blocked;
    }
    PythonModuleHelpStep::Ready
}

fn module_help_flag_usage<'a>(help: &'a CommandHelp, word: &str) -> Option<(&'a HelpEntry, bool)> {
    if let Some(entry) = help.flags.iter().find(|entry| entry.name == word) {
        return Some((entry, false));
    }
    if let Some((name, _)) = word.split_once('=') {
        return help
            .flags
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| (entry, true));
    }
    help.flags
        .iter()
        .filter(|entry| entry.takes_value && entry.name.len() == 2)
        .find(|entry| word.len() > entry.name.len() && word.starts_with(&entry.name))
        .map(|entry| (entry, true))
}

fn complete_python_module_help(
    context: &CompletionContext,
    cache: &Arc<CommandHelpCache>,
) -> ProviderOutput {
    let Some(invocation) = completed_python_module_invocation(context) else {
        return ProviderOutput::default();
    };
    let PythonModuleHelpLookup::Ready(target) =
        lookup_python_module_help(context, cache, invocation, None, false)
    else {
        return ProviderOutput::default();
    };
    let flags = target.position == PythonModuleHelpPosition::Flags;
    let entries = if flags {
        &target.help.flags
    } else {
        &target.help.subcommands
    };
    let query = context.parsed.current_prefix.as_str();
    let folded_query = query.to_lowercase();
    let exact = entries.iter().any(|entry| entry.name == query)
        || (!flags
            && target
                .help
                .subcommand_aliases
                .iter()
                .any(|alias| alias == query));
    if exact {
        return ProviderOutput::default();
    }
    let candidates = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            query.is_empty() || entry.name.to_lowercase().starts_with(&folded_query)
        })
        .map(|(index, entry)| {
            let replacement = entry.name.clone();
            let display = crate::parser::apply_edit(
                &context.buffer.text,
                context.parsed.replacement.clone(),
                &replacement,
            )
            .map(|result| result.trim_end().to_owned())
            .unwrap_or_else(|_| replacement.clone());
            let mut candidate = Candidate::new(
                context.query_id,
                display,
                entry.description.as_str(),
                Some(TextEdit {
                    range: context.parsed.replacement.clone(),
                    replacement,
                    cursor_after: CursorPlacement::End,
                }),
                if flags {
                    CandidateAction::Insert
                } else {
                    CandidateAction::InsertAndContinue {
                        next_slot: SlotKind::Value,
                    }
                },
                CandidateSource::CommandHelp,
                CandidateKind::Command,
                Completeness::NeedsInput {
                    slot: SlotKind::Value,
                },
                RiskLevel::Low,
                format!(
                    "python-module-help:{}:{}:{}:{}",
                    target.command,
                    target.module,
                    target.scope.join(" "),
                    entry.name
                ),
            );
            candidate.score.spec_priority =
                i16::try_from(200_usize.saturating_sub(index)).unwrap_or_default();
            candidate
        })
        .collect();
    ProviderOutput {
        candidates,
        diagnostics: Vec::new(),
    }
}

fn python_module_help_cache_key(command: &str, module: &str, scope: &[String]) -> String {
    let mut key = format!("\0python-module-help\0{command}\0{module}");
    for word in scope {
        key.push('\0');
        key.push_str(word);
    }
    key
}

fn fetch_python_module_help(program: &Path, module: &str, scope: &[String]) -> CommandHelp {
    let mut arguments = Vec::with_capacity(scope.len() + 3);
    arguments.push("-m".to_owned());
    arguments.push(module.to_owned());
    arguments.extend(scope.iter().cloned());
    arguments.push("--help".to_owned());
    let Ok(output) = crate::platform::run_bounded(
        program.as_os_str(),
        &arguments,
        PYTHON_MODULE_HELP_TIMEOUT,
        PYTHON_MODULE_HELP_MAX_BYTES,
    ) else {
        return CommandHelp::default();
    };
    let mut text = String::new();
    for bytes in [&output.stdout, &output.stderr] {
        if !bytes.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&String::from_utf8_lossy(bytes));
        }
    }
    if !output.status.success() && !looks_like_module_help(&text) {
        return CommandHelp::default();
    }
    super::command_help::parse_help_output_for_scope(module, scope, &text)
}

fn looks_like_module_help(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        line.starts_with("usage:")
            || line == "usage"
            || line.starts_with("commands:")
            || line == "options:"
            || line == "flags:"
    })
}

fn has_dynamic_shell_syntax(word: &str) -> bool {
    word.chars()
        .any(|character| matches!(character, '$' | '`' | '*' | '?' | '[' | '{'))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PythonMode {
    ignore_environment: bool,
    ignore_cached_environment: bool,
    no_site: bool,
    no_user_site: bool,
    safe_path: bool,
}

impl PythonMode {
    fn apply_environment(mut self, context: &CompletionContext) -> (Self, Option<OsString>) {
        if self.ignore_environment {
            return (self, None);
        }
        self.safe_path |= invocation_env_flag_enabled(context, "PYTHONSAFEPATH");
        self.no_user_site |= invocation_env_flag_enabled(context, "PYTHONNOUSERSITE");
        self.ignore_cached_environment |= ["PYTHONHOME", "PYTHONPLATLIBDIR"]
            .iter()
            .any(|name| invocation_env_configured(context, name));
        self.no_user_site |= invocation_env_configured(context, "PYTHONUSERBASE");
        let python_path = match invocation_environment_value(context, "PYTHONPATH") {
            InvocationEnvironmentValue::Known(Some(value)) if !value.is_empty() => Some(value),
            InvocationEnvironmentValue::Known(None | Some(_))
            | InvocationEnvironmentValue::Unknown => None,
        };
        (self, python_path)
    }

    fn allows_description(self, description: &str) -> bool {
        if self.ignore_cached_environment && is_cached_environment_description(description) {
            return false;
        }
        if self.no_site && is_site_description(description) {
            return false;
        }
        if self.no_user_site && is_user_description(description) {
            return false;
        }
        true
    }
}

struct PythonModulePosition<'a> {
    prefix: &'a str,
    mode: PythonMode,
    replacement: Range<usize>,
    replacement_prefix: &'static str,
    python_path: Option<OsString>,
}

fn python_module_position(context: &CompletionContext) -> Option<PythonModulePosition<'_>> {
    let command = context.command()?;
    if !is_python_command(command) {
        return None;
    }
    let (words, position) = argument_progress(context)?;
    let before = words.get(1..=position).unwrap_or_default();
    let mut mode = PythonMode::default();
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        match word {
            "-m" if index + 1 == before.len() => {
                let (mode, python_path) = mode.apply_environment(context);
                return Some(PythonModulePosition {
                    prefix: context.parsed.current_prefix.as_str(),
                    mode,
                    replacement: context.parsed.replacement.clone(),
                    replacement_prefix: "",
                    python_path,
                });
            }
            "-m" | "-c" | "--" => return None,
            _ if word
                .strip_prefix("-m")
                .is_some_and(|module| !module.is_empty()) =>
            {
                return None;
            }
            _ if word
                .strip_prefix("-c")
                .is_some_and(|command| !command.is_empty()) =>
            {
                return None;
            }
            "-h" | "--help" | "-?" | "-V" | "--version" | "--help-env" | "--help-xoptions"
            | "--help-all" => return None,
            "-E" => mode.ignore_environment = true,
            "-I" => {
                mode.ignore_environment = true;
                mode.no_user_site = true;
                mode.safe_path = true;
            }
            "-P" => mode.safe_path = true,
            "-s" => mode.no_user_site = true,
            "-S" => mode.no_site = true,
            "-W" | "-X" | "--check-hash-based-pycs" => {
                index += 1;
                if index >= before.len() {
                    return None;
                }
            }
            _ if python_flag_without_value(word)
                || apply_python_short_flag_cluster(word, &mut mode)
                || word.starts_with("-W") && word.len() > 2
                || word.starts_with("-X") && word.len() > 2
                || word.starts_with("--check-hash-based-pycs=") => {}
            _ => return None,
        }
        index += 1;
    }
    let prefix = context.parsed.current_prefix.strip_prefix("-m")?;
    valid_module_prefix(prefix).then(|| {
        let (mode, python_path) = mode.apply_environment(context);
        PythonModulePosition {
            prefix,
            mode,
            replacement: context.parsed.replacement.clone(),
            replacement_prefix: if prefix.is_empty() { "-m " } else { "-m" },
            python_path,
        }
    })
}

fn python_flag_without_value(word: &str) -> bool {
    matches!(
        word,
        "-b" | "-B" | "-d" | "-i" | "-O" | "-OO" | "-q" | "-u" | "-v" | "-x"
    )
}

fn apply_python_short_flag_cluster(word: &str, mode: &mut PythonMode) -> bool {
    let Some(flags) = word.strip_prefix('-') else {
        return false;
    };
    if flags.len() <= 1 || flags.starts_with('-') {
        return false;
    }
    let mut next = *mode;
    let mut optimize = 0;
    for flag in flags.chars() {
        match flag {
            'b' | 'B' | 'd' | 'i' | 'q' | 'u' | 'v' | 'x' => {}
            'E' => next.ignore_environment = true,
            'I' => {
                next.ignore_environment = true;
                next.no_user_site = true;
                next.safe_path = true;
            }
            'O' => {
                optimize += 1;
                if optimize > 2 {
                    return false;
                }
            }
            'P' => next.safe_path = true,
            's' => next.no_user_site = true,
            'S' => next.no_site = true,
            _ => return false,
        }
    }
    *mode = next;
    true
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum InvocationEnvironmentValue {
    Known(Option<OsString>),
    Unknown,
}

fn invocation_environment_value(
    context: &CompletionContext,
    name: &str,
) -> InvocationEnvironmentValue {
    let tokens =
        crate::parser::semantic_word_tokens(&context.parsed.tokens, &context.parsed.active_segment);
    let words: Vec<&str> = tokens
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let Some(command_index) =
        crate::parser::effective_command_index_for_shell(&words, context.shell)
    else {
        return InvocationEnvironmentValue::Known(env::var_os(name));
    };
    let mut value = InvocationEnvironmentValue::Known(env::var_os(name));
    for change in
        crate::parser::wrapper_environment_changes_for_shell(&words, command_index, context.shell)
    {
        match change {
            crate::parser::EnvironmentChange::Clear => {
                value = InvocationEnvironmentValue::Known(None);
            }
            crate::parser::EnvironmentChange::Set {
                name: changed,
                value: changed_value,
            } if changed == name => {
                value = if changed_value.contains(['$', '`']) {
                    InvocationEnvironmentValue::Unknown
                } else {
                    InvocationEnvironmentValue::Known(Some(OsString::from(changed_value)))
                };
            }
            crate::parser::EnvironmentChange::Unset(changed) if changed == name => {
                value = InvocationEnvironmentValue::Known(None);
            }
            crate::parser::EnvironmentChange::Set { .. }
            | crate::parser::EnvironmentChange::Unset(_) => {}
        }
    }
    value
}

fn invocation_env_flag_enabled(context: &CompletionContext, name: &str) -> bool {
    match invocation_environment_value(context, name) {
        InvocationEnvironmentValue::Known(Some(value)) => !value.is_empty() && value != "0",
        InvocationEnvironmentValue::Known(None) => false,
        InvocationEnvironmentValue::Unknown => true,
    }
}

fn invocation_env_configured(context: &CompletionContext, name: &str) -> bool {
    match invocation_environment_value(context, name) {
        InvocationEnvironmentValue::Known(Some(value)) => !value.is_empty(),
        InvocationEnvironmentValue::Known(None) => false,
        InvocationEnvironmentValue::Unknown => true,
    }
}

pub(crate) fn is_python_command(command: &str) -> bool {
    let name = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command);
    ["python", "pypy"].iter().any(|prefix| {
        name.strip_prefix(prefix).is_some_and(|suffix| {
            if suffix.is_empty() {
                return true;
            }
            let version = suffix.strip_suffix('t').unwrap_or(suffix);
            !version.is_empty()
                && version.split('.').all(|component| {
                    !component.is_empty()
                        && component
                            .chars()
                            .all(|character| character.is_ascii_digit())
                })
        })
    })
}

fn valid_module_prefix(prefix: &str) -> bool {
    if prefix.starts_with('.') || prefix.contains("..") {
        return false;
    }
    prefix.split('.').enumerate().all(|(index, component)| {
        if component.is_empty() {
            return index + 1 == prefix.split('.').count();
        }
        valid_module_component(component)
    })
}

fn valid_module_component(component: &str) -> bool {
    component
        .chars()
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && component
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric())
}

pub(crate) fn python_module_cache_key(command: &str) -> String {
    format!("\0python-modules\0{command}")
}

pub(crate) fn history_python_module_is_plausible(
    cache: &Arc<CommandHelpCache>,
    command: &str,
    executable: Option<PathBuf>,
    arguments: &[&str],
    known_non_failure: bool,
) -> Option<bool> {
    if !is_python_command(command) {
        return None;
    }
    let invocation = parse_python_module_arguments(arguments)?;
    if known_non_failure
        || std::iter::once(invocation.module)
            .chain(invocation.tail.iter().copied())
            .any(has_dynamic_shell_syntax)
    {
        return Some(true);
    }
    let key = python_module_cache_key(command);
    let Some(modules) = cache.peek(&key) else {
        if cache.is_pending(&key) {
            return Some(false);
        }
        let Some(program) = executable.clone() else {
            return Some(true);
        };
        let fetch_program = program.clone();
        cache.request_with_path(&key, Some(program), move |_| {
            discover_environment_modules(&fetch_program)
        });
        return Some(false);
    };
    let matching: Vec<_> = modules
        .subcommands
        .iter()
        .filter(|entry| entry.name == invocation.module)
        .collect();
    if matching.is_empty() {
        return Some(!modules.subcommands.iter().any(|entry| {
            invocation.mode.allows_description(&entry.description)
                && one_edit_or_adjacent_transposition(invocation.module, &entry.name)
        }));
    }
    if !matching.iter().any(|entry| {
        invocation.mode.allows_description(&entry.description)
            && is_cached_environment_description(&entry.description)
    }) {
        return Some(false);
    }
    if invocation.tail.is_empty() {
        return Some(true);
    }

    let mut scope = Vec::new();
    let mut remaining = invocation.tail.as_slice();
    loop {
        let help_key = python_module_help_cache_key(command, invocation.module, &scope);
        let Some(help) = cache.peek(&help_key) else {
            if cache.is_pending(&help_key) {
                return Some(false);
            }
            let Some(program) = executable.clone() else {
                return Some(true);
            };
            let module = invocation.module.to_owned();
            let fetch_scope = scope.clone();
            let fetch_program = program.clone();
            cache.request_with_path(&help_key, Some(program), move |_| {
                fetch_python_module_help(&fetch_program, &module, &fetch_scope)
            });
            return Some(false);
        };
        match python_module_help_step(&help, remaining) {
            PythonModuleHelpStep::Scope { word, consumed } => {
                remaining = &remaining[consumed..];
                if remaining.is_empty() {
                    return Some(true);
                }
                if scope.len() >= 3 {
                    return Some(true);
                }
                scope.push(word.to_owned());
            }
            PythonModuleHelpStep::Ready | PythonModuleHelpStep::Blocked => {
                return Some(history_arguments_are_plausible(
                    &help, remaining, false, false,
                ));
            }
        }
    }
}

#[derive(Deserialize)]
struct PythonMetadata {
    #[serde(default)]
    stdlib: Vec<Option<String>>,
    #[serde(default)]
    site: Vec<Option<String>>,
    #[serde(default)]
    user: Vec<String>,
}

fn discover_environment_modules(program: &Path) -> CommandHelp {
    let Ok(output) = crate::platform::run_bounded(
        program.as_os_str(),
        ["-E", "-S", "-c", PYTHON_METADATA_SCRIPT],
        PYTHON_METADATA_TIMEOUT,
        PYTHON_METADATA_MAX_BYTES,
    ) else {
        return CommandHelp::default();
    };
    if !output.status.success() || output.stdout.is_empty() {
        return CommandHelp::default();
    }
    let Ok(metadata) = serde_json::from_slice::<PythonMetadata>(&output.stdout) else {
        return CommandHelp::default();
    };
    let mut modules = BTreeMap::new();
    let mut roots = HashSet::new();
    let mut scanner = ModuleScanner::new(ENVIRONMENT_SCAN_BUDGET, ENVIRONMENT_ENTRY_LIMIT);
    for path in metadata.stdlib.into_iter().flatten() {
        let path = PathBuf::from(path);
        if roots.insert(path.clone()) {
            scanner.collect(&path, ModuleOrigin::Stdlib, &mut modules);
        }
    }
    for path in metadata.site.into_iter().flatten() {
        let path = PathBuf::from(path);
        if roots.insert(path.clone()) {
            scanner.collect(&path, ModuleOrigin::Site, &mut modules);
            for linked in literal_pth_roots(&path) {
                if roots.insert(linked.clone()) {
                    scanner.collect(&linked, ModuleOrigin::Site, &mut modules);
                }
            }
        }
    }
    for path in metadata.user {
        let path = PathBuf::from(path);
        if roots.insert(path.clone()) {
            scanner.collect(&path, ModuleOrigin::UserSite, &mut modules);
            for linked in literal_pth_roots(&path) {
                if roots.insert(linked.clone()) {
                    scanner.collect(&linked, ModuleOrigin::UserSite, &mut modules);
                }
            }
        }
    }
    CommandHelp {
        subcommands: modules
            .into_values()
            .map(|module| HelpEntry {
                name: module.name,
                description: module.description.to_owned(),
                takes_value: false,
            })
            .collect(),
        ..CommandHelp::default()
    }
}

#[derive(Clone, Copy)]
enum ModuleOrigin {
    Stdlib,
    Site,
    UserSite,
    PythonPath,
    Project,
}

impl ModuleOrigin {
    const fn descriptions(self) -> (&'static str, &'static str) {
        match self {
            Self::Stdlib => (DESC_STDLIB_ENTRY, DESC_STDLIB_MODULE),
            Self::Site => (DESC_SITE_ENTRY, DESC_SITE_MODULE),
            Self::UserSite => (DESC_USER_ENTRY, DESC_USER_MODULE),
            Self::PythonPath => (DESC_PATH_ENTRY, DESC_PATH_MODULE),
            Self::Project => (DESC_PROJECT_ENTRY, DESC_PROJECT_MODULE),
        }
    }

    const fn priority(self, entry_point: bool) -> i16 {
        match (self, entry_point) {
            (Self::Project, true) => 200,
            (Self::Project, false) => 190,
            (Self::PythonPath, true) => 185,
            (Self::Site | Self::UserSite, true) => 180,
            (Self::Stdlib, true) => 175,
            (Self::PythonPath, false) => 155,
            (Self::Site | Self::UserSite, false) => 145,
            (Self::Stdlib, false) => 135,
        }
    }
}

struct ModuleRecord {
    name: String,
    description: String,
    priority: i16,
    high_confidence: bool,
}

#[cfg(test)]
fn collect_modules(
    root: &Path,
    origin: ModuleOrigin,
    budget: Duration,
    entry_limit: usize,
    modules: &mut BTreeMap<String, ModuleRecord>,
) {
    ModuleScanner::new(budget, entry_limit).collect(root, origin, modules);
}

struct ModuleScanner {
    started: Instant,
    budget: Duration,
    entry_limit: usize,
    visited_entries: usize,
}

fn literal_pth_roots(site_directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(site_directory) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .flatten()
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("pth"))
        .collect();
    files.sort_by_key(fs::DirEntry::file_name);
    let mut roots = Vec::new();
    for entry in files.into_iter().take(PTH_FILE_LIMIT) {
        let path = entry.path();
        if !fs::metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= PTH_MAX_BYTES)
        {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty()
                || line.starts_with('#')
                || line == "import"
                || line.starts_with("import ")
                || line.starts_with("import\t")
            {
                continue;
            }
            let linked = PathBuf::from(line);
            let linked = if linked.is_absolute() {
                linked
            } else {
                site_directory.join(linked)
            };
            if linked.is_dir() {
                roots.push(linked);
            }
        }
    }
    roots
}

impl ModuleScanner {
    fn new(budget: Duration, entry_limit: usize) -> Self {
        Self {
            started: Instant::now(),
            budget,
            entry_limit,
            visited_entries: 0,
        }
    }

    fn exhausted(&self) -> bool {
        self.started.elapsed() >= self.budget || self.visited_entries >= self.entry_limit
    }

    fn collect(
        &mut self,
        root: &Path,
        origin: ModuleOrigin,
        modules: &mut BTreeMap<String, ModuleRecord>,
    ) {
        if !root.is_dir() {
            return;
        }
        let mut stack = vec![(root.to_owned(), String::new(), 0_usize)];
        while let Some((directory, prefix, depth)) = stack.pop() {
            if self.exhausted() {
                break;
            }
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(fs::DirEntry::file_name);
            for entry in entries {
                if self.started.elapsed() >= self.budget {
                    break;
                }
                self.visited_entries += 1;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                let followed = file_type
                    .is_symlink()
                    .then(|| fs::metadata(&path).ok())
                    .flatten();
                if file_type.is_dir() || followed.as_ref().is_some_and(fs::Metadata::is_dir) {
                    if depth >= MODULE_DEPTH_LIMIT
                        || name == "__pycache__"
                        || !valid_module_component(&name)
                    {
                        continue;
                    }
                    let module = dotted_name(&prefix, &name);
                    if path.join("__main__.py").is_file() {
                        add_discovered_module(modules, module.clone(), origin, true);
                    }
                    stack.push((path, module, depth + 1));
                    continue;
                }
                if !file_type.is_file() && !followed.as_ref().is_some_and(fs::Metadata::is_file) {
                    continue;
                }
                if path.extension().and_then(|extension| extension.to_str()) != Some("py") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                if matches!(stem, "__init__" | "__main__") || !valid_module_component(stem) {
                    continue;
                }
                add_discovered_module(modules, dotted_name(&prefix, stem), origin, false);
            }
        }
    }
}

fn dotted_name(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

fn add_discovered_module(
    modules: &mut BTreeMap<String, ModuleRecord>,
    name: String,
    origin: ModuleOrigin,
    entry_point: bool,
) {
    let (entry_description, module_description) = origin.descriptions();
    let stdlib_cli = matches!(origin, ModuleOrigin::Stdlib) && is_stdlib_cli_module(&name);
    let high_confidence =
        entry_point || stdlib_cli || matches!(origin, ModuleOrigin::Project) && !name.contains('.');
    insert_module(
        modules,
        ModuleRecord {
            name,
            description: if entry_point || stdlib_cli {
                entry_description
            } else {
                module_description
            }
            .to_owned(),
            priority: origin.priority(entry_point || stdlib_cli),
            high_confidence,
        },
    );
}

fn insert_module(modules: &mut BTreeMap<String, ModuleRecord>, module: ModuleRecord) {
    match modules.get_mut(&module.name) {
        Some(existing) if existing.priority < module.priority => *existing = module,
        Some(_) => {}
        None => {
            modules.insert(module.name.clone(), module);
        }
    }
}

fn is_stdlib_cli_module(name: &str) -> bool {
    STDLIB_CLI_MODULES.contains(&name)
}

fn description_is_entry(description: &str) -> bool {
    matches!(
        description,
        DESC_STDLIB_ENTRY
            | DESC_SITE_ENTRY
            | DESC_USER_ENTRY
            | DESC_PATH_ENTRY
            | DESC_PROJECT_ENTRY
    )
}

fn is_site_description(description: &str) -> bool {
    matches!(
        description,
        DESC_SITE_ENTRY | DESC_SITE_MODULE | DESC_USER_ENTRY | DESC_USER_MODULE
    )
}

fn is_user_description(description: &str) -> bool {
    matches!(description, DESC_USER_ENTRY | DESC_USER_MODULE)
}

fn is_cached_environment_description(description: &str) -> bool {
    matches!(
        description,
        DESC_STDLIB_ENTRY
            | DESC_STDLIB_MODULE
            | DESC_SITE_ENTRY
            | DESC_SITE_MODULE
            | DESC_USER_ENTRY
            | DESC_USER_MODULE
    )
}

fn description_priority(description: &str) -> i16 {
    match description {
        DESC_PROJECT_ENTRY => 200,
        DESC_PROJECT_MODULE => 190,
        DESC_PATH_ENTRY => 185,
        DESC_SITE_ENTRY | DESC_USER_ENTRY => 180,
        DESC_STDLIB_ENTRY => 175,
        DESC_PATH_MODULE => 155,
        DESC_SITE_MODULE | DESC_USER_MODULE => 145,
        DESC_STDLIB_MODULE => 135,
        _ => 100,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        os::unix::fs::{PermissionsExt, symlink},
    };

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    #[test]
    fn scanner_finds_packages_and_runnable_modules_without_data_directories() {
        let root = tempfile::tempdir().expect("module root");
        fs::write(root.path().join("tool.py"), "").expect("tool module");
        fs::create_dir(root.path().join("pkg")).expect("package");
        fs::write(root.path().join("pkg/__main__.py"), "").expect("package main");
        fs::write(root.path().join("pkg/worker.py"), "").expect("worker module");
        fs::create_dir(root.path().join("bad-name")).expect("data directory");
        fs::write(root.path().join("bad-name/hidden.py"), "").expect("hidden module");
        let mut modules = BTreeMap::new();
        collect_modules(
            root.path(),
            ModuleOrigin::Project,
            Duration::from_secs(1),
            100,
            &mut modules,
        );
        assert!(modules.contains_key("tool"));
        assert!(modules.contains_key("pkg"));
        assert!(modules.contains_key("pkg.worker"));
        assert!(!modules.contains_key("bad-name.hidden"));

        let linked = tempfile::tempdir().expect("linked module root");
        fs::write(linked.path().join("__main__.py"), "").expect("linked main");
        fs::write(linked.path().join("worker.py"), "").expect("linked worker");
        symlink(linked.path(), root.path().join("linked")).expect("module symlink");
        collect_modules(
            root.path(),
            ModuleOrigin::Project,
            Duration::from_secs(1),
            100,
            &mut modules,
        );
        assert!(modules.contains_key("linked"));
        assert!(modules.contains_key("linked.worker"));
    }

    #[test]
    fn pth_scanning_accepts_literal_paths_without_executing_import_lines() {
        let site = tempfile::tempdir().expect("site packages");
        let linked = tempfile::tempdir().expect("linked packages");
        fs::write(linked.path().join("entry.py"), "").expect("linked module");
        fs::write(
            site.path().join("editable.pth"),
            format!(
                "# comment\n{}\nimport arbitrary_side_effect\nmissing\n",
                linked.path().display()
            ),
        )
        .expect("pth file");

        assert_eq!(literal_pth_roots(site.path()), [linked.path().to_owned()]);
        let mut modules = BTreeMap::new();
        let mut scanner = ModuleScanner::new(Duration::from_secs(1), 100);
        for root in literal_pth_roots(site.path()) {
            scanner.collect(&root, ModuleOrigin::Site, &mut modules);
        }
        assert!(modules.contains_key("entry"));
    }

    #[test]
    fn provider_completes_only_the_python_m_module_slot() {
        let directory = tempfile::tempdir().expect("workspace");
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).expect("bin");
        let python = bin.join("python3");
        fs::write(&python, b"#!/bin/sh\n").expect("python fixture");
        fs::set_permissions(&python, fs::Permissions::from_mode(0o700)).expect("python mode");
        fs::write(directory.path().join("project_tool.py"), "").expect("project module");
        fs::create_dir(directory.path().join("extra")).expect("python path directory");
        fs::write(directory.path().join("extra/path_tool.py"), "").expect("python path module");

        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(&bin))));
        let cache = Arc::new(CommandHelpCache::default());
        cache.seed(
            &python_module_cache_key("python3"),
            CommandHelp {
                subcommands: vec![
                    HelpEntry {
                        name: "pytest".into(),
                        description: DESC_SITE_ENTRY.into(),
                        takes_value: false,
                    },
                    HelpEntry {
                        name: "plain_helper".into(),
                        description: DESC_SITE_MODULE.into(),
                        takes_value: false,
                    },
                    HelpEntry {
                        name: "pytest_extra".into(),
                        description: DESC_SITE_ENTRY.into(),
                        takes_value: false,
                    },
                    HelpEntry {
                        name: "pip".into(),
                        description: DESC_SITE_ENTRY.into(),
                        takes_value: false,
                    },
                    HelpEntry {
                        name: "http.server".into(),
                        description: DESC_STDLIB_ENTRY.into(),
                        takes_value: false,
                    },
                ],
                ..CommandHelp::default()
            },
        );
        cache.seed(
            &python_module_help_cache_key("python3", "pip", &[]),
            CommandHelp {
                subcommands: vec![
                    HelpEntry {
                        name: "install".into(),
                        description: "Install packages".into(),
                        takes_value: false,
                    },
                    HelpEntry {
                        name: "list".into(),
                        description: "List installed packages".into(),
                        takes_value: false,
                    },
                ],
                subcommands_exhaustive: true,
                ..CommandHelp::default()
            },
        );
        cache.seed(
            &python_module_help_cache_key("python3", "pip", &["install".into()]),
            CommandHelp {
                flags: vec![HelpEntry {
                    name: "--requirement".into(),
                    description: "Install from a requirements file".into(),
                    takes_value: true,
                }],
                accepts_positionals: true,
                ..CommandHelp::default()
            },
        );
        cache.seed(
            &python_module_help_cache_key("python3", "http.server", &[]),
            CommandHelp {
                flags: vec![
                    HelpEntry {
                        name: "--bind".into(),
                        description: "Bind to this address".into(),
                        takes_value: true,
                    },
                    HelpEntry {
                        name: "--directory".into(),
                        description: "Serve this directory".into(),
                        takes_value: true,
                    },
                ],
                accepts_positionals: true,
                ..CommandHelp::default()
            },
        );
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(PythonModuleProvider::new(commands, cache));

        let initial_rows = rows(&engine, context(directory.path(), "python3 -m ", 1));
        assert!(initial_rows.contains(&"python3 -m project_tool".to_owned()));
        assert!(initial_rows.contains(&"python3 -m pytest".to_owned()));
        assert!(!initial_rows.contains(&"python3 -m plain_helper".to_owned()));

        let active_m_rows = rows(&engine, context(directory.path(), "python3 -m", 2));
        assert!(active_m_rows.contains(&"python3 -m project_tool".to_owned()));
        assert!(active_m_rows.contains(&"python3 -m pytest".to_owned()));

        assert_eq!(
            rows(&engine, context(directory.path(), "python3 -m py", 3)),
            ["python3 -m pytest", "python3 -m pytest_extra"]
        );
        assert_eq!(
            rows(&engine, context(directory.path(), "python3 -mpy", 4)),
            ["python3 -mpytest", "python3 -mpytest_extra"]
        );
        assert_eq!(
            rows(&engine, context(directory.path(), "python3 -Bq -m pro", 5)),
            ["python3 -Bq -m project_tool"]
        );
        assert!(rows(&engine, context(directory.path(), "python3 -m zzz", 6)).is_empty());
        assert!(rows(&engine, context(directory.path(), "python3 -m pytest", 7)).is_empty());
        assert!(rows(&engine, context(directory.path(), "python3 -m pytest ", 8)).is_empty());
        assert_eq!(
            rows(&engine, context(directory.path(), "python3 -m pip ins", 22)),
            ["python3 -m pip install"]
        );
        assert_eq!(
            rows(
                &engine,
                context(directory.path(), "python3 -m pip install --requ", 23),
            ),
            ["python3 -m pip install --requirement"]
        );
        assert_eq!(
            rows(
                &engine,
                context(directory.path(), "python3 -m http.server --d", 24),
            ),
            ["python3 -m http.server --directory"]
        );
        assert!(
            rows(
                &engine,
                context(directory.path(), "python3 -m pip unknown", 25),
            )
            .is_empty()
        );
        assert!(
            rows(&engine, context(directory.path(), "python3 -IBq -m pro", 9)).is_empty(),
            "isolated mode must not expose cwd modules"
        );

        assert_eq!(
            rows(
                &engine,
                context(directory.path(), "PYTHONPATH=extra python3 -P -m path", 10,),
            ),
            ["PYTHONPATH=extra python3 -P -m path_tool"]
        );
        assert_eq!(
            rows(
                &engine,
                context(
                    directory.path(),
                    "env -i PYTHONPATH=extra python3 -P -m path",
                    11,
                ),
            ),
            ["env -i PYTHONPATH=extra python3 -P -m path_tool"]
        );
        assert!(
            rows(
                &engine,
                context(directory.path(), "PYTHONPATH= python3 -P -m pro", 12),
            )
            .is_empty(),
            "an empty PYTHONPATH must not re-add cwd"
        );
        assert_eq!(
            rows(
                &engine,
                context(directory.path(), "PYTHONPATH=: python3 -P -m pro", 13),
            ),
            ["PYTHONPATH=: python3 -P -m project_tool"]
        );
        assert!(
            rows(
                &engine,
                context(directory.path(), "PYTHONSAFEPATH=1 python3 -m pro", 14,),
            )
            .is_empty()
        );
        assert_eq!(
            rows(
                &engine,
                context(directory.path(), "PYTHONSAFEPATH=0 python3 -m pro", 15,),
            ),
            ["PYTHONSAFEPATH=0 python3 -m project_tool"]
        );
        assert!(
            rows(
                &engine,
                context(directory.path(), "PYTHONHOME=/custom python3 -m py", 16),
            )
            .is_empty(),
            "cached interpreter roots must not leak across PYTHONHOME"
        );
        assert_eq!(
            rows(
                &engine,
                context(directory.path(), "PYTHONHOME=/custom python3 -E -m py", 17,),
            ),
            [
                "PYTHONHOME=/custom python3 -E -m pytest",
                "PYTHONHOME=/custom python3 -E -m pytest_extra",
            ]
        );
        assert!(
            rows(
                &engine,
                context(
                    directory.path(),
                    "PYTHONPATH=$UNKNOWN python3 -P -m path",
                    18,
                ),
            )
            .is_empty(),
            "dynamic environment values must not borrow the parent PYTHONPATH"
        );
        assert_eq!(
            rows(
                &engine,
                context(directory.path(), "sudo -n python3 -m py", 19),
            ),
            [
                "sudo -n python3 -m pytest",
                "sudo -n python3 -m pytest_extra",
            ]
        );
        assert_eq!(
            rows(
                &engine,
                context(directory.path(), "command python3 -m pro", 20),
            ),
            ["command python3 -m project_tool"]
        );
        assert_eq!(
            rows(
                &engine,
                context(directory.path(), "python3 -W ignore -m py", 21),
            ),
            [
                "python3 -W ignore -m pytest",
                "python3 -W ignore -m pytest_extra",
            ]
        );
        assert_eq!(
            rows(
                &engine,
                context(directory.path(), "python3 -Xdev -m pro", 22),
            ),
            ["python3 -Xdev -m project_tool"]
        );
    }

    #[test]
    fn recognizes_versioned_and_free_threaded_python_executables() {
        for command in [
            "python",
            "python3",
            "python3.14",
            "python3.14t",
            "/opt/bin/python3.14t",
            "pypy",
            "pypy3.11",
        ] {
            assert!(
                is_python_command(command),
                "expected Python command: {command}"
            );
        }
        for command in [
            "pythonw",
            "python3.",
            "python3..14",
            "python3.14tt",
            "pypy3x",
        ] {
            assert!(
                !is_python_command(command),
                "invalid Python command: {command}"
            );
        }
    }

    #[test]
    fn combined_short_flags_preserve_python_mode_semantics() {
        let mut mode = PythonMode::default();
        assert!(apply_python_short_flag_cluster("-Bq", &mut mode));
        assert_eq!(mode, PythonMode::default());

        assert!(apply_python_short_flag_cluster("-EIPsS", &mut mode));
        assert!(mode.ignore_environment);
        assert!(mode.no_site);
        assert!(mode.no_user_site);
        assert!(mode.safe_path);
        assert!(!apply_python_short_flag_cluster("-OOO", &mut mode));
        assert!(!apply_python_short_flag_cluster("--quiet", &mut mode));
    }

    #[test]
    fn history_rejects_near_miss_module_names_only_when_metadata_is_cached() {
        let cache = Arc::new(CommandHelpCache::default());
        cache.seed(
            &python_module_cache_key("python3"),
            CommandHelp {
                subcommands: vec![HelpEntry {
                    name: "pytest".into(),
                    description: DESC_SITE_ENTRY.into(),
                    takes_value: false,
                }],
                ..CommandHelp::default()
            },
        );
        cache.seed(
            &python_module_help_cache_key("python3", "pytest", &[]),
            CommandHelp {
                subcommands: vec![HelpEntry {
                    name: "collect".into(),
                    description: "Collect tests".into(),
                    takes_value: false,
                }],
                subcommands_exhaustive: true,
                ..CommandHelp::default()
            },
        );
        cache.seed(
            &python_module_help_cache_key("python3", "pytest", &["collect".into()]),
            CommandHelp {
                flags: vec![HelpEntry {
                    name: "--quiet".into(),
                    description: "Reduce output".into(),
                    takes_value: false,
                }],
                ..CommandHelp::default()
            },
        );
        assert_eq!(
            history_python_module_is_plausible(&cache, "python3", None, &["-m", "pytest"], false,),
            Some(true)
        );
        assert_eq!(
            history_python_module_is_plausible(&cache, "python3", None, &["-m", "pytes"], false,),
            Some(false)
        );
        assert_eq!(
            history_python_module_is_plausible(&cache, "python3", None, &["-mpytes"], false,),
            Some(false)
        );
        assert_eq!(
            history_python_module_is_plausible(
                &cache,
                "python3",
                None,
                &["-m", "private_app"],
                false,
            ),
            Some(true)
        );
        assert_eq!(
            history_python_module_is_plausible(
                &cache,
                "python3",
                None,
                &["-m", "pytest", "collet"],
                false,
            ),
            Some(false)
        );
        assert_eq!(
            history_python_module_is_plausible(
                &cache,
                "python3",
                None,
                &["-m", "pytest", "collect", "--quiet"],
                false,
            ),
            Some(true)
        );
        assert_eq!(
            history_python_module_is_plausible(
                &cache,
                "python3",
                None,
                &["-m", "pytest", "collect", "--quite"],
                false,
            ),
            Some(false)
        );
    }

    fn rows(engine: &CompletionEngine, context: CompletionContext) -> Vec<String> {
        engine
            .complete(&context)
            .candidates
            .into_iter()
            .map(|candidate| candidate.display.primary)
            .collect()
    }

    fn context(directory: &Path, text: &str, query: u64) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(query),
            ShellKind::Zsh,
            directory.to_owned(),
            BufferSnapshot::new(
                text,
                text.len(),
                BufferRevision::new(query),
                SyncQuality::Exact,
            )
            .expect("buffer"),
        )
        .expect("context")
    }
}
