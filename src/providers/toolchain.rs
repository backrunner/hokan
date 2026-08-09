use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderOutput, TextEdit,
    },
    parser::{QuoteContext, escape_for_shell},
    platform::CommandPathCache,
    terminal::RiskLevel,
};

const CACHE_TTL: Duration = Duration::from_secs(2);
const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PROJECT_DIRECTORIES: usize = 10_000;
const MAX_PROJECT_DEPTH: usize = 16;
const MAX_PROJECT_ITEMS: usize = 500;
const PROJECT_SCAN_BUDGET: Duration = Duration::from_millis(60);

pub(super) const MAVEN_PHASES: &[(&str, &str)] = &[
    ("pre-clean", "运行清理前阶段"),
    ("clean", "清理构建产物"),
    ("post-clean", "运行清理后阶段"),
    ("validate", "验证项目结构"),
    ("initialize", "初始化构建状态"),
    ("generate-sources", "生成主源码"),
    ("process-sources", "处理主源码"),
    ("generate-resources", "生成主资源"),
    ("process-resources", "处理主资源"),
    ("compile", "编译主源码"),
    ("process-classes", "处理主类文件"),
    ("generate-test-sources", "生成测试源码"),
    ("process-test-sources", "处理测试源码"),
    ("generate-test-resources", "生成测试资源"),
    ("process-test-resources", "处理测试资源"),
    ("test-compile", "编译测试源码"),
    ("process-test-classes", "处理测试类文件"),
    ("test", "运行测试"),
    ("prepare-package", "准备打包"),
    ("package", "生成可分发包"),
    ("pre-integration-test", "准备集成测试"),
    ("integration-test", "运行集成测试"),
    ("post-integration-test", "清理集成测试环境"),
    ("verify", "运行集成校验"),
    ("install", "安装到本地仓库"),
    ("deploy", "发布到远程仓库"),
    ("pre-site", "运行站点生成前阶段"),
    ("site", "生成项目站点"),
    ("post-site", "运行站点生成后阶段"),
    ("site-deploy", "发布项目站点"),
];

// Gradle build scripts are executable code. Only tasks supplied by Gradle's
// built-in help task surface are listed here; project-specific tasks remain a
// history concern instead of being discovered by running the build.
const GRADLE_HELP_TASKS: &[(&str, &str)] = &[
    ("help", "显示 Gradle 帮助"),
    ("tasks", "列出当前项目任务"),
    ("projects", "列出子项目"),
    ("properties", "显示项目属性"),
    ("dependencies", "显示依赖树"),
    ("dependencyInsight", "解释依赖选择"),
    ("buildEnvironment", "显示构建脚本依赖"),
    ("outgoingVariants", "显示可发布 variants"),
    ("resolvableConfigurations", "显示可解析 configurations"),
    ("javaToolchains", "显示可用 Java toolchains"),
];

const CMAKE_E_COMMANDS: &[(&str, &str)] = &[
    ("capabilities", "显示 CMake capabilities"),
    ("cat", "连接文件并写到标准输出"),
    ("chdir", "在指定目录运行命令"),
    ("compare_files", "比较两个文件"),
    ("copy", "复制文件"),
    ("copy_directory", "复制目录"),
    ("copy_if_different", "内容变化时复制"),
    ("echo", "输出文本"),
    ("echo_append", "输出文本且不换行"),
    ("env", "在修改后的环境中运行命令"),
    ("environment", "显示环境变量"),
    ("make_directory", "创建目录"),
    ("remove", "删除文件"),
    ("remove_directory", "删除目录"),
    ("rename", "重命名文件或目录"),
    ("sleep", "暂停指定时间"),
    ("tar", "创建或解压归档"),
    ("time", "统计命令运行时间"),
    ("touch", "创建或更新时间戳"),
    ("touch_nocreate", "仅更新已有文件时间戳"),
];

const NINJA_TOOLS: &[(&str, &str)] = &[
    ("clean", "清理构建产物"),
    ("commands", "显示目标对应命令"),
    ("compdb", "输出 compilation database"),
    ("deps", "显示依赖信息"),
    ("graph", "输出依赖图"),
    ("missingdeps", "检查缺失依赖"),
    ("query", "查询目标输入与输出"),
    ("recompact", "压缩 Ninja 日志"),
    ("restat", "重新统计输出"),
    ("rules", "列出规则"),
    ("targets", "列出构建目标"),
];

const XCODEBUILD_ACTIONS: &[(&str, &str)] = &[
    ("build", "构建选定 scheme 或 target"),
    ("build-for-testing", "构建测试产物但不运行"),
    ("analyze", "运行静态分析"),
    ("archive", "生成 Xcode archive"),
    ("test", "构建并运行测试"),
    ("test-without-building", "运行已构建的测试产物"),
    ("installsrc", "安装源码"),
    ("install", "构建并安装产物"),
    ("clean", "清理构建产物"),
];

pub struct ToolchainProvider {
    commands: Arc<CommandPathCache>,
    cargo: Mutex<HashMap<PathBuf, Timed<CargoProject>>>,
    go: Mutex<HashMap<PathBuf, Timed<GoProject>>>,
    cmake: Mutex<HashMap<PathBuf, Timed<Vec<NamedItem>>>>,
    ninja: Mutex<HashMap<PathBuf, Timed<Vec<NamedItem>>>>,
}

impl ToolchainProvider {
    #[must_use]
    pub fn new(commands: Arc<CommandPathCache>) -> Self {
        Self {
            commands,
            cargo: Mutex::new(HashMap::new()),
            go: Mutex::new(HashMap::new()),
            cmake: Mutex::new(HashMap::new()),
            ninja: Mutex::new(HashMap::new()),
        }
    }
}

impl CandidateProvider for ToolchainProvider {
    fn id(&self) -> &'static str {
        "toolchain"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        self.position(context).is_some()
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Some(position) = self.position(context) else {
            return ProviderOutput::default();
        };
        let (items, source) = match &position.kind {
            ToolchainKind::Static { entries, source } => (
                entries
                    .iter()
                    .map(|(name, description)| NamedItem::new(*name, *description))
                    .collect(),
                (*source).to_owned(),
            ),
            ToolchainKind::RustToolchain => (
                installed_rust_toolchains()
                    .into_iter()
                    .map(|name| NamedItem::new(name, "已安装 Rust toolchain"))
                    .collect(),
                "rustup".to_owned(),
            ),
            ToolchainKind::RustTarget { toolchain } => (
                installed_rust_targets(toolchain.as_deref())
                    .into_iter()
                    .map(|name| NamedItem::new(name, "已安装 Rust target"))
                    .collect(),
                "rustup-targets".to_owned(),
            ),
            ToolchainKind::Cargo {
                value,
                project_dir,
                manifest,
                selected_package,
            } => (
                self.cargo_items(
                    project_dir,
                    manifest.as_deref(),
                    selected_package.as_deref(),
                    *value,
                    &position.query,
                ),
                "cargo".to_owned(),
            ),
            ToolchainKind::GoPackage { project_dir } => {
                (self.go_items(project_dir, &position.query), "go".to_owned())
            }
            ToolchainKind::GoTool {
                executable,
                project_dir,
            } => (go_tool_items(executable, project_dir), "go-tool".to_owned()),
            ToolchainKind::CmakePreset { project_dir, kind } => (
                self.cmake_items(project_dir, *kind),
                "cmake-presets".to_owned(),
            ),
            ToolchainKind::NinjaTarget { manifest } => {
                (self.ninja_items(manifest), "ninja".to_owned())
            }
        };
        complete_items(context, &position, items, &source)
    }
}

#[derive(Clone)]
struct CompletionPosition {
    query: String,
    edit_prefix: String,
    excluded: BTreeSet<String>,
    next_slot: Option<crate::completion::SlotKind>,
    kind: ToolchainKind,
}

#[derive(Clone)]
enum ToolchainKind {
    Static {
        entries: &'static [(&'static str, &'static str)],
        source: &'static str,
    },
    RustToolchain,
    RustTarget {
        toolchain: Option<String>,
    },
    Cargo {
        value: CargoValue,
        project_dir: PathBuf,
        manifest: Option<PathBuf>,
        selected_package: Option<String>,
    },
    GoPackage {
        project_dir: PathBuf,
    },
    GoTool {
        executable: PathBuf,
        project_dir: PathBuf,
    },
    CmakePreset {
        project_dir: PathBuf,
        kind: CmakePresetKind,
    },
    NinjaTarget {
        manifest: PathBuf,
    },
}

#[derive(Clone, Debug)]
struct NamedItem {
    name: String,
    description: String,
    annotation: Option<String>,
}

impl NamedItem {
    fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            annotation: None,
        }
    }

    fn annotated(mut self, annotation: impl Into<String>) -> Self {
        self.annotation = Some(annotation.into());
        self
    }
}

struct Timed<T> {
    loaded: Instant,
    value: Option<Arc<T>>,
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

fn cached<T>(
    cache: &Mutex<HashMap<PathBuf, Timed<T>>>,
    key: PathBuf,
    load: impl FnOnce() -> Option<T>,
) -> Option<Arc<T>> {
    if let Some(entry) = lock(cache).get(&key)
        && entry.loaded.elapsed() < CACHE_TTL
    {
        return entry.value.clone();
    }
    let value = load().map(Arc::new);
    lock(cache).insert(
        key,
        Timed {
            loaded: Instant::now(),
            value: value.clone(),
        },
    );
    value
}

fn complete_items(
    context: &CompletionContext,
    position: &CompletionPosition,
    mut items: Vec<NamedItem>,
    source: &str,
) -> ProviderOutput {
    let mut seen = BTreeSet::new();
    items.retain(|item| seen.insert(item.name.clone()));
    items.retain(|item| !position.excluded.contains(&item.name));
    if items.iter().any(|item| item.name == position.query) {
        items.retain(|item| {
            item.name == position.query
                || item.name.starts_with(&position.query) && item.name.len() > position.query.len()
        });
    }
    let candidates = items
        .into_iter()
        .filter(|item| item.name.starts_with(&position.query))
        .take(MAX_PROJECT_ITEMS)
        .enumerate()
        .map(|(index, item)| {
            let escaped = escape_for_shell(&item.name, QuoteContext::Unquoted, context.shell);
            let replacement = format!("{}{}", position.edit_prefix, escaped);
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
                item.description,
                Some(TextEdit {
                    range: context.parsed.replacement.clone(),
                    replacement,
                    cursor_after: CursorPlacement::End,
                }),
                position
                    .next_slot
                    .map_or(CandidateAction::Insert, |next_slot| {
                        CandidateAction::InsertAndContinue { next_slot }
                    }),
                CandidateSource::Project,
                CandidateKind::Recipe,
                position.next_slot.map_or(Completeness::Runnable, |slot| {
                    Completeness::NeedsInput { slot }
                }),
                RiskLevel::Low,
                format!("toolchain:{source}:{}", item.name),
            );
            candidate.display.annotation = item.annotation;
            candidate.score.cwd_affinity = 100;
            candidate.score.spec_priority =
                i16::try_from(MAX_PROJECT_ITEMS.saturating_sub(index)).unwrap_or_default();
            candidate
        })
        .collect();
    ProviderOutput {
        candidates,
        diagnostics: Vec::new(),
    }
}

impl ToolchainProvider {
    fn position(&self, context: &CompletionContext) -> Option<CompletionPosition> {
        if super::redirect_target(context) || !super::effective_command_accepts_external(context) {
            return None;
        }
        let executable = super::resolved_executable_path(context, &self.commands)?;
        let raw_command = context.command()?;
        let command = super::executable_basename(raw_command);
        let progress = super::argument_progress(context);

        if progress.is_none() {
            return (maven_command(command)
                && context.parsed.current_prefix == raw_command
                && (super::command_position_open(context)
                    || super::explicit_executable_path_position(context)))
            .then(|| CompletionPosition {
                query: String::new(),
                edit_prefix: format!("{raw_command} "),
                excluded: BTreeSet::new(),
                next_slot: None,
                kind: ToolchainKind::Static {
                    entries: MAVEN_PHASES,
                    source: "maven",
                },
            });
        }

        let (words, position) = progress?;
        let completed = words.get(1..=position).unwrap_or_default();
        let prefix = context.parsed.current_prefix.as_str();

        if matches!(command, "cargo" | "rustc" | "rustdoc" | "rustup")
            && prefix.starts_with('+')
            && rust_selector_slot(completed)
        {
            return Some(CompletionPosition {
                query: prefix.trim_start_matches('+').to_owned(),
                edit_prefix: "+".to_owned(),
                excluded: BTreeSet::new(),
                next_slot: Some(if matches!(command, "rustc" | "rustdoc") {
                    crate::completion::SlotKind::Path
                } else {
                    crate::completion::SlotKind::Value
                }),
                kind: ToolchainKind::RustToolchain,
            });
        }
        if command == "rustup"
            && let Some((query, edit_prefix, next_slot)) =
                rustup_toolchain_position(completed, prefix)
        {
            return Some(CompletionPosition {
                query,
                edit_prefix,
                excluded: BTreeSet::new(),
                next_slot,
                kind: ToolchainKind::RustToolchain,
            });
        }
        if matches!(command, "cargo" | "rustc" | "rustdoc")
            && let Some((query, edit_prefix)) =
                flag_value_position(completed, prefix, &[("--target", false)])
            && (matches!(command, "rustc" | "rustdoc")
                || cargo_subcommand(completed).is_some_and(cargo_target_supported))
        {
            return Some(CompletionPosition {
                query,
                edit_prefix,
                excluded: BTreeSet::new(),
                next_slot: matches!(command, "rustc" | "rustdoc")
                    .then_some(crate::completion::SlotKind::Path),
                kind: ToolchainKind::RustTarget {
                    toolchain: rust_toolchain_selection(command, completed),
                },
            });
        }
        if command == "rustup"
            && let Some(excluded) = rustup_target_remove_position(completed, prefix)
        {
            return Some(CompletionPosition {
                query: prefix.to_owned(),
                edit_prefix: String::new(),
                excluded,
                next_slot: None,
                kind: ToolchainKind::RustTarget {
                    toolchain: rust_toolchain_selection(command, completed),
                },
            });
        }

        if command == "cargo"
            && let Some((value, query, edit_prefix)) = cargo_value_position(completed, prefix)
            && cargo_subcommand(completed)
                .is_some_and(|subcommand| cargo_value_supported(subcommand, value))
        {
            let (project_dir, manifest) = cargo_project_location(context, completed);
            return Some(CompletionPosition {
                query,
                edit_prefix,
                excluded: cargo_excluded_values(value, completed, prefix),
                next_slot: None,
                kind: ToolchainKind::Cargo {
                    value,
                    project_dir,
                    manifest,
                    selected_package: cargo_selected_package(completed),
                },
            });
        }

        if command == "go" && go_tool_slot(completed, prefix) {
            return Some(CompletionPosition {
                query: prefix.to_owned(),
                edit_prefix: String::new(),
                excluded: BTreeSet::new(),
                next_slot: None,
                kind: ToolchainKind::GoTool {
                    executable,
                    project_dir: go_project_directory(context, completed),
                },
            });
        }
        if command == "go" && go_package_slot(completed, prefix) {
            return Some(CompletionPosition {
                query: prefix.to_owned(),
                edit_prefix: String::new(),
                excluded: BTreeSet::new(),
                next_slot: None,
                kind: ToolchainKind::GoPackage {
                    project_dir: go_project_directory(context, completed),
                },
            });
        }

        if maven_command(command) && task_slot(completed, prefix, ToolTask::Maven) {
            return Some(CompletionPosition {
                query: prefix.to_owned(),
                edit_prefix: String::new(),
                excluded: completed_static_items(completed, MAVEN_PHASES),
                next_slot: None,
                kind: ToolchainKind::Static {
                    entries: MAVEN_PHASES,
                    source: "maven",
                },
            });
        }
        if gradle_command(command) && task_slot(completed, prefix, ToolTask::Gradle) {
            return Some(CompletionPosition {
                query: prefix.to_owned(),
                edit_prefix: String::new(),
                excluded: completed_static_items(completed, GRADLE_HELP_TASKS),
                next_slot: None,
                kind: ToolchainKind::Static {
                    entries: GRADLE_HELP_TASKS,
                    source: "gradle-help",
                },
            });
        }
        if command == "xcodebuild" && xcodebuild_action_slot(completed, prefix) {
            return Some(CompletionPosition {
                query: prefix.to_owned(),
                edit_prefix: String::new(),
                excluded: completed_static_items(completed, XCODEBUILD_ACTIONS),
                next_slot: None,
                kind: ToolchainKind::Static {
                    entries: XCODEBUILD_ACTIONS,
                    source: "xcodebuild-actions",
                },
            });
        }

        if command == "cmake" && cmake_e_slot(completed, prefix) {
            return Some(CompletionPosition {
                query: prefix.to_owned(),
                edit_prefix: String::new(),
                excluded: BTreeSet::new(),
                next_slot: Some(crate::completion::SlotKind::Value),
                kind: ToolchainKind::Static {
                    entries: CMAKE_E_COMMANDS,
                    source: "cmake-e",
                },
            });
        }
        if let Some((query, edit_prefix)) =
            flag_value_position(completed, prefix, &[("--preset", false)])
            && let Some(kind) = cmake_preset_kind(command, completed)
        {
            return Some(CompletionPosition {
                query,
                edit_prefix,
                excluded: BTreeSet::new(),
                next_slot: None,
                kind: ToolchainKind::CmakePreset {
                    project_dir: super::invocation_working_directory(context),
                    kind,
                },
            });
        }

        if command == "ninja" {
            if ninja_tool_slot(completed, prefix) {
                return Some(CompletionPosition {
                    query: prefix.to_owned(),
                    edit_prefix: String::new(),
                    excluded: BTreeSet::new(),
                    next_slot: None,
                    kind: ToolchainKind::Static {
                        entries: NINJA_TOOLS,
                        source: "ninja-tool",
                    },
                });
            }
            if let Some((manifest, excluded)) = ninja_target_slot(context, completed, prefix) {
                return Some(CompletionPosition {
                    query: prefix.to_owned(),
                    edit_prefix: String::new(),
                    excluded,
                    next_slot: None,
                    kind: ToolchainKind::NinjaTarget { manifest },
                });
            }
        }
        None
    }
}

fn maven_command(command: &str) -> bool {
    matches!(command, "mvn" | "mvnw" | "mvnDebug")
}

fn gradle_command(command: &str) -> bool {
    matches!(command, "gradle" | "gradlew")
}

fn rust_selector_slot(completed: &[&str]) -> bool {
    completed.iter().all(|word| {
        word.starts_with('-')
            || word
                .strip_prefix('+')
                .is_some_and(|selector| !selector.is_empty())
    })
}

fn rust_toolchain_selection(command: &str, completed: &[&str]) -> Option<String> {
    if matches!(command, "cargo" | "rustc" | "rustdoc") {
        return completed.iter().find_map(|word| {
            word.strip_prefix('+')
                .filter(|selector| !selector.is_empty())
                .map(str::to_owned)
        });
    }
    if command != "rustup" {
        return None;
    }
    let mut index = 0;
    let mut selected = None;
    while let Some(word) = completed.get(index).copied() {
        if word == "--toolchain" {
            selected = completed.get(index + 1).map(|value| (*value).to_owned());
            index += 2;
            continue;
        }
        if let Some(value) = word.strip_prefix("--toolchain=") {
            selected = (!value.is_empty()).then(|| value.to_owned());
        }
        index += 1;
    }
    selected
}

fn rustup_toolchain_position(
    completed: &[&str],
    prefix: &str,
) -> Option<(String, String, Option<crate::completion::SlotKind>)> {
    if rustup_toolchain_flag_scope(completed)
        && let Some((query, edit_prefix)) =
            flag_value_position(completed, prefix, &[("--toolchain", false)])
    {
        let next_slot = match completed.first().copied() {
            Some("target" | "component") => Some(crate::completion::SlotKind::Value),
            Some("which") if !rustup_which_has_command(completed) => {
                Some(crate::completion::SlotKind::Executable)
            }
            _ => None,
        };
        return Some((query, edit_prefix, next_slot));
    }
    if prefix.starts_with('-') {
        return None;
    }
    let next_slot = match completed {
        ["run"] | ["run", "--install"] => Some(crate::completion::SlotKind::Executable),
        ["default"]
        | ["override", "set"]
        | ["toolchain", "install"]
        | ["toolchain", "uninstall"] => None,
        _ => return None,
    };
    Some((prefix.to_owned(), String::new(), next_slot))
}

fn rustup_toolchain_flag_scope(completed: &[&str]) -> bool {
    matches!(
        completed.first().copied(),
        Some("target" | "component" | "which" | "doc" | "man")
    )
}

fn rustup_which_has_command(completed: &[&str]) -> bool {
    let mut index = 1;
    while let Some(word) = completed.get(index).copied() {
        if word == "--toolchain" {
            if index + 1 >= completed.len() {
                return false;
            }
            index += 2;
            continue;
        }
        if word.starts_with("--toolchain=") || word.starts_with('-') {
            index += 1;
            continue;
        }
        return true;
    }
    false
}

fn rustup_target_remove_position(completed: &[&str], prefix: &str) -> Option<BTreeSet<String>> {
    if prefix.starts_with('-')
        || !matches!(
            completed,
            ["target", "remove", ..] | ["target", "uninstall", ..]
        )
    {
        return None;
    }
    let mut excluded = BTreeSet::new();
    let mut index = 2;
    while let Some(word) = completed.get(index).copied() {
        if word == "--toolchain" {
            if index + 1 >= completed.len() {
                return None;
            }
            index += 2;
            continue;
        }
        if word.starts_with("--toolchain=") {
            index += 1;
            continue;
        }
        if word.starts_with('-') {
            return None;
        }
        excluded.insert(word.to_owned());
        index += 1;
    }
    Some(excluded)
}

fn flag_value_position(
    completed: &[&str],
    prefix: &str,
    flags: &[(&str, bool)],
) -> Option<(String, String)> {
    if completed.contains(&"--") {
        return None;
    }
    for (flag, short_attached) in flags {
        if let Some(value) = prefix.strip_prefix(&format!("{flag}=")) {
            return Some((value.to_owned(), format!("{flag}=")));
        }
        if *short_attached
            && flag.len() == 2
            && let Some(value) = prefix.strip_prefix(flag)
            && !value.is_empty()
        {
            return Some((value.to_owned(), (*flag).to_owned()));
        }
    }
    let previous = completed.last().copied()?;
    flags
        .iter()
        .any(|(flag, _)| *flag == previous)
        .then(|| (prefix.to_owned(), String::new()))
}

#[derive(Clone, Copy)]
enum ToolTask {
    Maven,
    Gradle,
}

fn task_slot(completed: &[&str], prefix: &str, tool: ToolTask) -> bool {
    if prefix.starts_with('-') {
        return false;
    }
    let mut index = 0;
    while let Some(word) = completed.get(index).copied() {
        if word == "--" {
            index += 1;
            continue;
        }
        if tool_option_attached(tool, word) || !word.starts_with('-') {
            index += 1;
            continue;
        }
        if tool_value_option(tool, word) {
            if index + 1 >= completed.len() {
                return false;
            }
            index += 2;
            continue;
        }
        if tool_boolean_option(tool, word) {
            index += 1;
            continue;
        }
        return false;
    }
    true
}

fn tool_option_attached(tool: ToolTask, word: &str) -> bool {
    match tool {
        ToolTask::Maven => {
            word.starts_with("-D")
                || word.starts_with("-P")
                || word.starts_with("-pl") && word.len() > 3
                || word.starts_with("-T") && word.len() > 2
        }
        ToolTask::Gradle => {
            word.starts_with("-D")
                || word.starts_with("-P")
                || word.starts_with("--project-prop=")
                || word.starts_with("--system-prop=")
        }
    }
}

fn tool_value_option(tool: ToolTask, word: &str) -> bool {
    match tool {
        ToolTask::Maven => matches!(
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
        ),
        ToolTask::Gradle => matches!(
            word,
            "-b" | "--build-file"
                | "-c"
                | "--settings-file"
                | "-g"
                | "--gradle-user-home"
                | "-I"
                | "--init-script"
                | "-p"
                | "--project-dir"
                | "--project-cache-dir"
                | "--max-workers"
                | "--priority"
                | "--console"
                | "--warning-mode"
        ),
    }
}

fn tool_boolean_option(tool: ToolTask, word: &str) -> bool {
    match tool {
        ToolTask::Maven => matches!(
            word,
            "-q" | "--quiet"
                | "-X"
                | "--debug"
                | "-e"
                | "--errors"
                | "-B"
                | "--batch-mode"
                | "-o"
                | "--offline"
                | "-U"
                | "--update-snapshots"
                | "-N"
                | "--non-recursive"
                | "-fae"
                | "--fail-at-end"
                | "-ff"
                | "--fail-fast"
                | "-fn"
                | "--fail-never"
                | "-am"
                | "--also-make"
                | "-amd"
                | "--also-make-dependents"
        ),
        ToolTask::Gradle => matches!(
            word,
            "-q" | "--quiet"
                | "-i"
                | "--info"
                | "-d"
                | "--debug"
                | "-s"
                | "--stacktrace"
                | "-S"
                | "--full-stacktrace"
                | "--scan"
                | "--no-scan"
                | "--offline"
                | "--refresh-dependencies"
                | "--rerun-tasks"
                | "--continue"
                | "--no-daemon"
                | "--daemon"
                | "--parallel"
                | "--no-parallel"
        ),
    }
}

fn completed_static_items(completed: &[&str], entries: &[(&str, &str)]) -> BTreeSet<String> {
    completed
        .iter()
        .filter(|word| entries.iter().any(|(name, _)| name == *word))
        .map(|word| (*word).to_owned())
        .collect()
}

fn xcodebuild_action_slot(completed: &[&str], prefix: &str) -> bool {
    if prefix.starts_with('-') {
        return false;
    }
    let mut index = 0;
    while let Some(word) = completed.get(index).copied() {
        if xcodebuild_value_option(word) {
            if index + 1 >= completed.len() {
                return false;
            }
            index += 2;
            continue;
        }
        if matches!(
            word,
            "-quiet"
                | "-dry-run"
                | "-parallelizeTargets"
                | "-hideShellScriptEnvironment"
                | "-allowProvisioningUpdates"
                | "-allowProvisioningDeviceRegistration"
                | "-skipUnavailableActions"
                | "-disableAutomaticPackageResolution"
                | "-onlyUsePackageVersionsFromResolvedFile"
                | "-skipPackageUpdates"
        ) {
            index += 1;
            continue;
        }
        if word.starts_with('-') {
            return false;
        }
        if word.contains('=') || XCODEBUILD_ACTIONS.iter().any(|(action, _)| *action == word) {
            index += 1;
            continue;
        }
        return false;
    }
    true
}

fn xcodebuild_value_option(word: &str) -> bool {
    matches!(
        word,
        "-project"
            | "-workspace"
            | "-target"
            | "-scheme"
            | "-configuration"
            | "-xcconfig"
            | "-arch"
            | "-sdk"
            | "-toolchain"
            | "-destination"
            | "-destination-timeout"
            | "-jobs"
            | "-resultBundlePath"
            | "-resultStreamPath"
            | "-derivedDataPath"
            | "-archivePath"
            | "-exportOptionsPlist"
            | "-exportPath"
            | "-clonedSourcePackagesDirPath"
            | "-packageCachePath"
            | "-parallel-testing-enabled"
            | "-parallel-testing-worker-count"
            | "-testPlan"
            | "-only-testing"
            | "-skip-testing"
            | "-testLanguage"
            | "-testRegion"
    )
}

fn cmake_e_slot(completed: &[&str], prefix: &str) -> bool {
    !prefix.starts_with('-') && completed.last().copied() == Some("-E")
}

#[derive(Clone, Copy)]
enum CmakePresetKind {
    Configure,
    Build,
    Test,
    Package,
    Workflow,
}

fn cmake_preset_kind(command: &str, completed: &[&str]) -> Option<CmakePresetKind> {
    match command {
        "ctest" => Some(CmakePresetKind::Test),
        "cpack" => Some(CmakePresetKind::Package),
        "cmake" if completed.contains(&"--workflow") => Some(CmakePresetKind::Workflow),
        "cmake" if completed.contains(&"--build") => {
            (!cmake_build_has_directory(completed)).then_some(CmakePresetKind::Build)
        }
        "cmake" => Some(CmakePresetKind::Configure),
        _ => None,
    }
}

fn cmake_build_has_directory(completed: &[&str]) -> bool {
    completed
        .iter()
        .position(|word| *word == "--build")
        .and_then(|index| completed.get(index + 1))
        .is_some_and(|value| !value.starts_with('-'))
}

fn ninja_tool_slot(completed: &[&str], prefix: &str) -> bool {
    !prefix.starts_with('-') && completed.last().copied() == Some("-t")
}

fn ninja_target_slot(
    context: &CompletionContext,
    completed: &[&str],
    prefix: &str,
) -> Option<(PathBuf, BTreeSet<String>)> {
    if prefix.starts_with('-') && !completed.contains(&"--") {
        return None;
    }
    let mut directory = super::invocation_working_directory(context);
    let mut manifest = None;
    let mut excluded = BTreeSet::new();
    let mut options = true;
    let mut index = 0;
    while let Some(word) = completed.get(index).copied() {
        if options && word == "--" {
            options = false;
            index += 1;
            continue;
        }
        if options && matches!(word, "-h" | "--help" | "--version") {
            return None;
        }
        if options && (word == "-t" || word.starts_with("-t") && word.len() > 2) {
            // Everything after a tool selector belongs to that tool's own
            // grammar. Some accept targets and others accept rule names or
            // no values, so ordinary build targets would be misleading.
            return None;
        }
        if options && word == "-C" {
            let value = completed.get(index + 1).copied()?;
            directory = super::resolve_directory(&directory, value);
            index += 2;
            continue;
        }
        if options && let Some(value) = word.strip_prefix("-C").filter(|value| !value.is_empty()) {
            directory = super::resolve_directory(&directory, value);
            index += 1;
            continue;
        }
        if options && word == "-f" {
            manifest = Some(PathBuf::from(completed.get(index + 1).copied()?));
            index += 2;
            continue;
        }
        if options && let Some(value) = word.strip_prefix("-f").filter(|value| !value.is_empty()) {
            manifest = Some(PathBuf::from(value));
            index += 1;
            continue;
        }
        if options && matches!(word, "-j" | "-k" | "-l" | "-d" | "-w") {
            if index + 1 >= completed.len() {
                return None;
            }
            index += 2;
            continue;
        }
        if options
            && ["-j", "-k", "-l", "-d", "-w"]
                .iter()
                .any(|flag| word.starts_with(flag) && word.len() > flag.len())
        {
            index += 1;
            continue;
        }
        if options && matches!(word, "-n" | "-v" | "--verbose" | "--quiet" | "--no-rebuild") {
            index += 1;
            continue;
        }
        if options && word.starts_with('-') {
            return None;
        }
        excluded.insert(word.to_owned());
        index += 1;
    }
    let manifest = manifest
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                directory.join(path)
            }
        })
        .unwrap_or_else(|| directory.join("build.ninja"));
    Some((manifest, excluded))
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CargoValue {
    Package,
    Feature,
    Bin,
    Example,
    Test,
    Bench,
    Profile,
}

fn cargo_subcommand<'a>(completed: &'a [&'a str]) -> Option<&'a str> {
    let mut index = 0;
    while let Some(word) = completed.get(index).copied() {
        if word == "--" {
            return None;
        }
        if word.starts_with('+') {
            index += 1;
            continue;
        }
        if matches!(word, "--color" | "--config" | "--target-dir" | "-Z") {
            if index + 1 >= completed.len() {
                return None;
            }
            index += 2;
            continue;
        }
        if word.starts_with("--color=")
            || word.starts_with("--config=")
            || word.starts_with("--target-dir=")
            || word.starts_with("-Z") && word.len() > 2
        {
            index += 1;
            continue;
        }
        if matches!(
            word,
            "-v" | "--verbose" | "-q" | "--quiet" | "--frozen" | "--locked" | "--offline"
        ) || word.len() > 2 && word.starts_with('-') && word[1..].chars().all(|c| c == 'v')
        {
            index += 1;
            continue;
        }
        if word.starts_with('-') {
            return None;
        }
        return Some(word);
    }
    None
}

fn cargo_target_supported(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "bench" | "build" | "check" | "clippy" | "doc" | "fix" | "run" | "rustc" | "test"
    )
}

fn cargo_value_supported(subcommand: &str, value: CargoValue) -> bool {
    match value {
        CargoValue::Package => matches!(
            subcommand,
            "bench"
                | "build"
                | "check"
                | "clean"
                | "clippy"
                | "doc"
                | "fix"
                | "package"
                | "publish"
                | "run"
                | "rustc"
                | "test"
        ),
        CargoValue::Feature => matches!(
            subcommand,
            "bench"
                | "build"
                | "check"
                | "clippy"
                | "doc"
                | "fix"
                | "metadata"
                | "package"
                | "publish"
                | "run"
                | "rustc"
                | "test"
        ),
        CargoValue::Bin | CargoValue::Example => matches!(
            subcommand,
            "bench" | "build" | "check" | "clippy" | "doc" | "fix" | "run" | "rustc" | "test"
        ),
        CargoValue::Test | CargoValue::Bench => matches!(
            subcommand,
            "bench" | "build" | "check" | "clippy" | "fix" | "test"
        ),
        CargoValue::Profile => matches!(
            subcommand,
            "bench"
                | "build"
                | "check"
                | "clean"
                | "clippy"
                | "doc"
                | "fix"
                | "run"
                | "rustc"
                | "test"
        ),
    }
}

fn cargo_value_position(completed: &[&str], prefix: &str) -> Option<(CargoValue, String, String)> {
    if completed.contains(&"--") {
        return None;
    }
    let flags = [
        (CargoValue::Package, "-p", true),
        (CargoValue::Package, "--package", false),
        (CargoValue::Package, "--exclude", false),
        (CargoValue::Feature, "-F", true),
        (CargoValue::Feature, "--features", false),
        (CargoValue::Bin, "--bin", false),
        (CargoValue::Example, "--example", false),
        (CargoValue::Test, "--test", false),
        (CargoValue::Bench, "--bench", false),
        (CargoValue::Profile, "--profile", false),
    ];
    for (value, flag, short_attached) in flags {
        if let Some(query) = prefix.strip_prefix(&format!("{flag}=")) {
            let (list_prefix, query) = cargo_list_prefix(value, query);
            return Some((value, query.to_owned(), format!("{flag}={list_prefix}")));
        }
        if short_attached
            && let Some(query) = prefix.strip_prefix(flag)
            && !query.is_empty()
        {
            let (list_prefix, query) = cargo_list_prefix(value, query);
            return Some((value, query.to_owned(), format!("{flag}{list_prefix}")));
        }
    }
    let previous = completed.last().copied()?;
    let (value, _, _) = flags.iter().find(|(_, flag, _)| *flag == previous)?;
    let (list_prefix, query) = cargo_list_prefix(*value, prefix);
    Some((*value, query.to_owned(), list_prefix.to_owned()))
}

fn cargo_list_prefix(value: CargoValue, prefix: &str) -> (&str, &str) {
    if value != CargoValue::Feature {
        return ("", prefix);
    }
    prefix
        .rfind(',')
        .map_or(("", prefix), |index| prefix.split_at(index + 1))
}

fn cargo_excluded_values(value: CargoValue, completed: &[&str], prefix: &str) -> BTreeSet<String> {
    if value != CargoValue::Feature {
        return BTreeSet::new();
    }
    let mut excluded = BTreeSet::new();
    let mut index = 0;
    while let Some(word) = completed.get(index).copied() {
        let feature_list = if matches!(word, "-F" | "--features") {
            index += 1;
            completed.get(index).copied()
        } else {
            word.strip_prefix("--features=")
                .or_else(|| word.strip_prefix("-F").filter(|value| !value.is_empty()))
        };
        if let Some(feature_list) = feature_list {
            excluded.extend(
                feature_list
                    .split(',')
                    .map(str::trim)
                    .filter(|feature| !feature.is_empty())
                    .map(str::to_owned),
            );
        }
        index += 1;
    }
    let active = prefix
        .strip_prefix("--features=")
        .or_else(|| prefix.strip_prefix("-F"))
        .unwrap_or(prefix);
    if let Some(index) = active.rfind(',') {
        excluded.extend(
            active[..index]
                .split(',')
                .map(str::trim)
                .filter(|feature| !feature.is_empty())
                .map(str::to_owned),
        );
    }
    excluded
}

fn cargo_project_location(
    context: &CompletionContext,
    completed: &[&str],
) -> (PathBuf, Option<PathBuf>) {
    let mut directory = super::invocation_working_directory(context);
    let mut manifest = None;
    let mut index = 0;
    while let Some(word) = completed.get(index).copied() {
        if word == "--" {
            break;
        }
        if word == "-C" {
            if let Some(value) = completed.get(index + 1).copied() {
                directory = super::resolve_directory(&directory, value);
                index += 2;
                continue;
            }
            break;
        }
        if let Some(value) = word.strip_prefix("-C").filter(|value| !value.is_empty()) {
            directory = super::resolve_directory(&directory, value);
            index += 1;
            continue;
        }
        if word == "--manifest-path" {
            if let Some(value) = completed.get(index + 1).copied() {
                let path = PathBuf::from(value);
                manifest = Some(if path.is_absolute() {
                    path
                } else {
                    directory.join(path)
                });
                index += 2;
                continue;
            }
            break;
        }
        if let Some(value) = word.strip_prefix("--manifest-path=") {
            let path = PathBuf::from(value);
            manifest = Some(if path.is_absolute() {
                path
            } else {
                directory.join(path)
            });
        }
        index += 1;
    }
    (directory, manifest)
}

fn cargo_selected_package(completed: &[&str]) -> Option<String> {
    let mut selected = None;
    let mut index = 0;
    while let Some(word) = completed.get(index).copied() {
        if word == "--" {
            break;
        }
        if matches!(word, "-p" | "--package") {
            if let Some(value) = completed.get(index + 1).copied() {
                selected = Some(value.to_owned());
                index += 2;
                continue;
            }
            break;
        }
        if let Some(value) = word.strip_prefix("--package=") {
            selected = Some(value.to_owned());
        } else if let Some(value) = word.strip_prefix("-p").filter(|value| !value.is_empty()) {
            selected = Some(value.to_owned());
        }
        index += 1;
    }
    selected
}

#[derive(Debug)]
struct CargoProject {
    root_manifest: PathBuf,
    packages: Vec<CargoPackage>,
    profiles: Vec<String>,
}

#[derive(Debug)]
struct CargoPackage {
    name: String,
    root: PathBuf,
    manifest: PathBuf,
    values: BTreeMap<CargoValue, Vec<String>>,
}

impl ToolchainProvider {
    fn cargo_items(
        &self,
        project_dir: &Path,
        explicit_manifest: Option<&Path>,
        selected_package: Option<&str>,
        value: CargoValue,
        query: &str,
    ) -> Vec<NamedItem> {
        let Some(manifest) = explicit_manifest
            .map(Path::to_path_buf)
            .or_else(|| find_ancestor_file(project_dir, "Cargo.toml"))
        else {
            return Vec::new();
        };
        let key = canonical_or(manifest.clone());
        let Some(project) = cached(&self.cargo, key, || load_cargo_project(&manifest)) else {
            return Vec::new();
        };
        let annotation = display_relative(&project.root_manifest, project_dir);
        if value == CargoValue::Package {
            return project
                .packages
                .iter()
                .map(|package| {
                    NamedItem::new(&package.name, "Cargo workspace package")
                        .annotated(display_relative(&package.manifest, project_dir))
                })
                .collect();
        }
        if value == CargoValue::Profile {
            return project
                .profiles
                .iter()
                .map(|profile| {
                    NamedItem::new(profile, "Cargo build profile").annotated(&annotation)
                })
                .collect();
        }
        let package = selected_package
            .and_then(|name| project.packages.iter().find(|package| package.name == name))
            .or_else(|| nearest_cargo_package(&project.packages, project_dir))
            .or_else(|| (project.packages.len() == 1).then(|| &project.packages[0]));
        let mut items = package
            .into_iter()
            .flat_map(|package| {
                package
                    .values
                    .get(&value)
                    .into_iter()
                    .flatten()
                    .map(|name| {
                        NamedItem::new(name, cargo_value_description(value))
                            .annotated(display_relative(&package.manifest, project_dir))
                    })
            })
            .collect::<Vec<_>>();
        let workspace_root = project
            .root_manifest
            .parent()
            .map(canonical_or_path)
            .unwrap_or_default();
        if value == CargoValue::Feature
            && project.packages.len() > 1
            && (package.is_none()
                || query.contains('/')
                || canonical_or(project_dir.to_path_buf()) == workspace_root)
        {
            for package in &project.packages {
                if let Some(features) = package.values.get(&CargoValue::Feature) {
                    items.extend(features.iter().map(|feature| {
                        NamedItem::new(
                            format!("{}/{feature}", package.name),
                            "Cargo workspace feature",
                        )
                        .annotated(display_relative(&package.manifest, project_dir))
                    }));
                }
            }
        }
        items
    }
}

fn canonical_or_path(path: &Path) -> PathBuf {
    canonical_or(path.to_path_buf())
}

fn cargo_value_description(value: CargoValue) -> &'static str {
    match value {
        CargoValue::Package => "Cargo workspace package",
        CargoValue::Feature => "Cargo feature",
        CargoValue::Bin => "Cargo binary target",
        CargoValue::Example => "Cargo example target",
        CargoValue::Test => "Cargo test target",
        CargoValue::Bench => "Cargo benchmark target",
        CargoValue::Profile => "Cargo build profile",
    }
}

fn nearest_cargo_package<'a>(
    packages: &'a [CargoPackage],
    directory: &Path,
) -> Option<&'a CargoPackage> {
    let directory = canonical_or(directory.to_path_buf());
    packages
        .iter()
        .filter(|package| directory.starts_with(&package.root))
        .max_by_key(|package| package.root.components().count())
}

fn load_cargo_project(start_manifest: &Path) -> Option<CargoProject> {
    let start_manifest = canonical_or(start_manifest.to_path_buf());
    let start_value = read_toml(&start_manifest)?;
    let start_root = start_manifest.parent()?.to_path_buf();
    let workspace_manifest = cargo_declared_workspace(&start_value, &start_root)
        .or_else(|| find_cargo_workspace_manifest(&start_manifest))
        .unwrap_or_else(|| start_manifest.clone());
    let workspace_value = if workspace_manifest == start_manifest {
        start_value
    } else {
        read_toml(&workspace_manifest)?
    };
    let workspace_root = workspace_manifest.parent()?.to_path_buf();
    let workspace = workspace_value
        .get("workspace")
        .and_then(toml::Value::as_table);
    let members = workspace
        .and_then(|table| table.get("members"))
        .and_then(toml::Value::as_array)
        .map(|values| toml_string_array(values))
        .unwrap_or_default();
    let excludes = workspace
        .and_then(|table| table.get("exclude"))
        .and_then(toml::Value::as_array)
        .map(|values| toml_string_array(values))
        .unwrap_or_default();
    let member_set = build_globset(&members);
    let exclude_set = build_globset(&excludes);

    let mut manifests = Vec::new();
    if workspace_value.get("package").is_some() {
        manifests.push(workspace_manifest.clone());
    }
    if !members.is_empty() {
        for directory in walk_directories(&workspace_root) {
            let manifest = directory.join("Cargo.toml");
            if !manifest.is_file() {
                continue;
            }
            let relative = slash_path(directory.strip_prefix(&workspace_root).ok()?);
            if member_set
                .as_ref()
                .is_some_and(|set| set.is_match(&relative))
                && !exclude_set
                    .as_ref()
                    .is_some_and(|set| set.is_match(&relative))
            {
                manifests.push(manifest);
            }
        }
    } else if workspace_value.get("workspace").is_none() {
        manifests.push(start_manifest);
    }
    manifests.sort();
    manifests.dedup();

    let mut packages = manifests
        .iter()
        .filter_map(|manifest| load_cargo_package(manifest))
        .take(MAX_PROJECT_ITEMS)
        .collect::<Vec<_>>();
    packages.sort_by(|left, right| left.name.cmp(&right.name));
    packages.dedup_by(|left, right| left.name == right.name);

    let mut profiles = BTreeSet::from([
        "bench".to_owned(),
        "dev".to_owned(),
        "release".to_owned(),
        "test".to_owned(),
    ]);
    if let Some(table) = workspace_value
        .get("profile")
        .and_then(toml::Value::as_table)
    {
        profiles.extend(table.keys().cloned());
    }
    Some(CargoProject {
        root_manifest: workspace_manifest,
        packages,
        profiles: profiles.into_iter().collect(),
    })
}

fn cargo_declared_workspace(value: &toml::Value, package_root: &Path) -> Option<PathBuf> {
    let relative = value.get("package")?.get("workspace")?.as_str()?;
    let root = canonical_or(package_root.join(relative));
    let manifest = root.join("Cargo.toml");
    manifest.is_file().then_some(manifest)
}

fn find_cargo_workspace_manifest(start_manifest: &Path) -> Option<PathBuf> {
    let mut directory = start_manifest.parent()?.parent();
    while let Some(current) = directory {
        let manifest = current.join("Cargo.toml");
        if manifest.is_file()
            && read_toml(&manifest).is_some_and(|value| value.get("workspace").is_some())
        {
            return Some(manifest);
        }
        directory = current.parent();
    }
    None
}

fn load_cargo_package(manifest: &Path) -> Option<CargoPackage> {
    let value = read_toml(manifest)?;
    let package = value.get("package")?.as_table()?;
    let name = package.get("name")?.as_str()?.to_owned();
    let root = manifest.parent()?.to_path_buf();
    let mut values = BTreeMap::new();
    let feature_table = value.get("features").and_then(toml::Value::as_table);
    let mut features = feature_table
        .map(|table| table.keys().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    let explicitly_namespaced = feature_table
        .into_iter()
        .flat_map(|table| table.values())
        .filter_map(toml::Value::as_array)
        .flatten()
        .filter_map(toml::Value::as_str)
        .filter_map(|feature| feature.strip_prefix("dep:"))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    for (dependency, specification) in cargo_dependency_entries(&value) {
        if specification
            .as_table()
            .and_then(|table| table.get("optional"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
            && !explicitly_namespaced.contains(dependency)
        {
            features.insert(dependency.to_owned());
        }
    }
    values.insert(CargoValue::Feature, features.into_iter().collect());

    let target_specs = [
        (CargoValue::Bin, "bin", "src/bin", "src/main.rs", "autobins"),
        (
            CargoValue::Example,
            "example",
            "examples",
            "",
            "autoexamples",
        ),
        (CargoValue::Test, "test", "tests", "", "autotests"),
        (CargoValue::Bench, "bench", "benches", "", "autobenches"),
    ];
    for (kind, table_name, automatic_directory, default_file, auto_key) in target_specs {
        let mut names = BTreeSet::new();
        if let Some(targets) = value.get(table_name).and_then(toml::Value::as_array) {
            for target in targets {
                if let Some(name) = target.get("name").and_then(toml::Value::as_str) {
                    names.insert(name.to_owned());
                }
            }
        }
        let automatic = package
            .get(auto_key)
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        if automatic {
            if !default_file.is_empty() && root.join(default_file).is_file() {
                names.insert(name.clone());
            }
            names.extend(discover_rust_targets(&root.join(automatic_directory)));
        }
        values.insert(kind, names.into_iter().collect());
    }
    Some(CargoPackage {
        name,
        root: canonical_or(root),
        manifest: manifest.to_path_buf(),
        values,
    })
}

fn cargo_dependency_entries(value: &toml::Value) -> Vec<(&str, &toml::Value)> {
    const TABLES: &[&str] = &["dependencies", "build-dependencies", "dev-dependencies"];
    let mut entries = Vec::new();
    for table_name in TABLES {
        if let Some(table) = value.get(*table_name).and_then(toml::Value::as_table) {
            entries.extend(table.iter().map(|(name, value)| (name.as_str(), value)));
        }
    }
    if let Some(targets) = value.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            for table_name in TABLES {
                if let Some(table) = target.get(*table_name).and_then(toml::Value::as_table) {
                    entries.extend(table.iter().map(|(name, value)| (name.as_str(), value)));
                }
            }
        }
    }
    entries
}

fn discover_rust_targets(directory: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut names = BTreeSet::new();
    for entry in entries.flatten().take(MAX_PROJECT_ITEMS) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() && path.extension().and_then(|value| value.to_str()) == Some("rs") {
            if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
                names.insert(stem.to_owned());
            }
        } else if file_type.is_dir()
            && path.join("main.rs").is_file()
            && let Some(name) = path.file_name().and_then(|value| value.to_str())
        {
            names.insert(name.to_owned());
        }
    }
    names.into_iter().collect()
}

fn read_toml(path: &Path) -> Option<toml::Value> {
    let text = read_bounded_text(path)?;
    toml::from_str(&text).ok()
}

fn toml_string_array(values: &[toml::Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(toml::Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn build_globset(patterns: &[String]) -> Option<GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    let mut added = false;
    for pattern in patterns {
        if let Ok(glob) = Glob::new(pattern.trim_end_matches('/')) {
            builder.add(glob);
            added = true;
        }
    }
    added.then(|| builder.build().ok()).flatten()
}

fn go_project_directory(context: &CompletionContext, completed: &[&str]) -> PathBuf {
    let mut directory = super::invocation_working_directory(context);
    let mut index = 0;
    while let Some(word) = completed.get(index).copied() {
        if word == "-C" {
            if let Some(value) = completed.get(index + 1).copied() {
                directory = super::resolve_directory(&directory, value);
                index += 2;
                continue;
            }
            return directory;
        }
        if let Some(value) = word.strip_prefix("-C=") {
            directory = super::resolve_directory(&directory, value);
        }
        index += 1;
    }
    directory
}

fn go_tool_slot(completed: &[&str], prefix: &str) -> bool {
    if prefix.starts_with('-') {
        return false;
    }
    let mut index = 0;
    while let Some(word) = completed.get(index).copied() {
        if word == "-C" {
            if index + 1 >= completed.len() {
                return false;
            }
            index += 2;
            continue;
        }
        if word.starts_with("-C=") {
            index += 1;
            continue;
        }
        return word == "tool" && index + 1 == completed.len();
    }
    false
}

fn go_package_slot(completed: &[&str], prefix: &str) -> bool {
    if prefix.starts_with('-') || completed.contains(&"--") {
        return false;
    }
    let mut index = 0;
    let mut command = None;
    let mut positional_count = 0;
    while let Some(word) = completed.get(index).copied() {
        if command.is_none() {
            if word == "-C" {
                if index + 1 >= completed.len() {
                    return false;
                }
                index += 2;
                continue;
            }
            if word.starts_with("-C=") {
                index += 1;
                continue;
            }
            if word.starts_with('-') {
                return false;
            }
            command = Some(word);
            index += 1;
            continue;
        }
        if word == "-args" {
            return false;
        }
        if go_value_flag(word) {
            if index + 1 >= completed.len() {
                return false;
            }
            index += 2;
            continue;
        }
        if !word.starts_with('-') {
            positional_count += 1;
        }
        index += 1;
    }
    command.is_some_and(|command| {
        match command {
            // Once the program/package operand is complete these slots are
            // program arguments (`run`) or symbol text (`doc`), not another
            // package. Staying quiet is more accurate than guessing whether
            // a second `.go` word was intended for `run`.
            "run" | "doc" => positional_count == 0,
            "build" | "clean" | "fix" | "fmt" | "generate" | "get" | "install" | "list"
            | "test" | "vet" => true,
            _ => false,
        }
    })
}

fn go_value_flag(flag: &str) -> bool {
    matches!(
        flag,
        "-C" | "-o"
            | "-p"
            | "-asmflags"
            | "-buildmode"
            | "-compiler"
            | "-covermode"
            | "-coverpkg"
            | "-exec"
            | "-gccgoflags"
            | "-gcflags"
            | "-installsuffix"
            | "-ldflags"
            | "-mod"
            | "-modfile"
            | "-overlay"
            | "-pgo"
            | "-pkgdir"
            | "-tags"
            | "-toolexec"
            | "-vettool"
            | "-coverprofile"
            | "-cpu"
            | "-list"
            | "-memprofile"
            | "-outputdir"
            | "-run"
            | "-shuffle"
            | "-timeout"
    )
}

#[derive(Debug)]
struct GoProject {
    key: PathBuf,
    packages: Vec<GoPackage>,
}

#[derive(Debug)]
struct GoPackage {
    directory: PathBuf,
    import_path: Option<String>,
}

impl ToolchainProvider {
    fn go_items(&self, directory: &Path, query: &str) -> Vec<NamedItem> {
        let Some(key) = find_ancestor_file(directory, "go.work")
            .or_else(|| find_ancestor_file(directory, "go.mod"))
        else {
            return Vec::new();
        };
        let key = canonical_or(key);
        let Some(project) = cached(&self.go, key.clone(), || load_go_project(&key)) else {
            return Vec::new();
        };
        let cwd = canonical_or(directory.to_path_buf());
        let annotation = display_relative(&project.key, &cwd);
        let mut items = Vec::new();
        if (query.is_empty() || "./...".starts_with(query))
            && project
                .packages
                .iter()
                .any(|package| package.directory.starts_with(&cwd))
        {
            items
                .push(NamedItem::new("./...", "当前目录下全部 Go packages").annotated(&annotation));
        }
        for package in &project.packages {
            let relative = relative_path(&cwd, &package.directory);
            let local = if relative.as_os_str().is_empty() {
                ".".to_owned()
            } else {
                let value = slash_path(&relative);
                if value.starts_with('.') {
                    value
                } else {
                    format!("./{value}")
                }
            };
            if local.starts_with(query) {
                items.push(
                    NamedItem::new(local, "Go package")
                        .annotated(display_relative(&package.directory, &cwd)),
                );
            }
            if !query.is_empty()
                && let Some(import) = &package.import_path
                && import.starts_with(query)
            {
                items.push(NamedItem::new(import, "Go module import path").annotated(&annotation));
            }
        }
        items
    }
}

fn go_tool_items(executable: &Path, project_dir: &Path) -> Vec<NamedItem> {
    let executable = canonical_or(executable.to_path_buf());
    let mut names = BTreeSet::new();
    if let Some(root) = executable.parent().and_then(Path::parent) {
        let tools = root.join("pkg/tool");
        if let Ok(platforms) = fs::read_dir(tools) {
            for platform in platforms.flatten().take(32) {
                if !platform.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                if let Ok(entries) = fs::read_dir(platform.path()) {
                    for entry in entries.flatten().take(MAX_PROJECT_ITEMS) {
                        if entry.file_type().is_ok_and(|kind| kind.is_file())
                            && crate::platform::is_executable(&entry.path())
                            && let Some(name) = entry.file_name().to_str()
                        {
                            names.insert(name.to_owned());
                        }
                    }
                }
            }
        }
    }
    if let Some(go_mod) = find_ancestor_file(project_dir, "go.mod")
        && let Some(text) = read_bounded_text(&go_mod)
    {
        names.extend(go_tool_directives(&text));
    }
    names
        .into_iter()
        .map(|name| NamedItem::new(name, "Go tool command"))
        .collect()
}

fn go_tool_directives(text: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    let mut in_block = false;
    for line in text.lines() {
        let line = line.split_once("//").map_or(line, |(line, _)| line).trim();
        if line == "tool (" {
            in_block = true;
            continue;
        }
        if in_block && line == ")" {
            in_block = false;
            continue;
        }
        let tool = if in_block {
            line
        } else if let Some(tool) = line.strip_prefix("tool ") {
            tool.trim()
        } else {
            continue;
        };
        let tool = tool.split_whitespace().next().unwrap_or_default();
        if let Some(name) = tool.rsplit('/').next()
            && !name.is_empty()
            && !name.contains(['$', '*', '?', '['])
        {
            names.insert(name.to_owned());
        }
    }
    names.into_iter().collect()
}

fn load_go_project(key: &Path) -> Option<GoProject> {
    let file_name = key.file_name()?.to_str()?;
    let modules = if file_name == "go.work" {
        parse_go_work(key)
    } else {
        vec![key.parent()?.to_path_buf()]
    };
    let mut packages = Vec::new();
    for module_root in modules.into_iter().take(MAX_PROJECT_ITEMS) {
        let module_file = module_root.join("go.mod");
        let module_path = parse_go_module_path(&module_file);
        for directory in walk_go_package_directories(&module_root) {
            let relative = directory.strip_prefix(&module_root).ok()?;
            let import_path = module_path.as_ref().map(|module| {
                if relative.as_os_str().is_empty() {
                    module.clone()
                } else {
                    format!("{module}/{}", slash_path(relative))
                }
            });
            packages.push(GoPackage {
                directory,
                import_path,
            });
            if packages.len() >= MAX_PROJECT_ITEMS {
                break;
            }
        }
        if packages.len() >= MAX_PROJECT_ITEMS {
            break;
        }
    }
    packages.sort_by(|left, right| left.directory.cmp(&right.directory));
    packages.dedup_by(|left, right| left.directory == right.directory);
    Some(GoProject {
        key: key.to_path_buf(),
        packages,
    })
}

fn parse_go_module_path(path: &Path) -> Option<String> {
    read_bounded_text(path)?.lines().find_map(|line| {
        let line = line.trim();
        let value = line.strip_prefix("module")?.trim();
        (!value.is_empty()).then(|| trim_go_string(value).to_owned())
    })
}

fn parse_go_work(path: &Path) -> Vec<PathBuf> {
    let Some(text) = read_bounded_text(path) else {
        return Vec::new();
    };
    let Some(root) = path.parent() else {
        return Vec::new();
    };
    let mut modules = Vec::new();
    let mut in_use = false;
    for line in text.lines() {
        let line = line.split_once("//").map_or(line, |(line, _)| line).trim();
        if line == "use (" {
            in_use = true;
            continue;
        }
        if in_use && line == ")" {
            in_use = false;
            continue;
        }
        let value = if in_use {
            line
        } else if let Some(value) = line.strip_prefix("use ") {
            value.trim()
        } else {
            continue;
        };
        if value.is_empty() || value.contains(['$', '*', '?', '[']) {
            continue;
        }
        let path = PathBuf::from(trim_go_string(value));
        let path = canonical_or(if path.is_absolute() {
            path
        } else {
            root.join(path)
        });
        if path.join("go.mod").is_file() {
            modules.push(path);
        }
    }
    modules.sort();
    modules.dedup();
    modules
}

fn trim_go_string(value: &str) -> &str {
    value.trim().trim_matches('"').trim_matches('`')
}

fn walk_go_package_directories(root: &Path) -> Vec<PathBuf> {
    let root = canonical_or(root.to_path_buf());
    let mut packages = Vec::new();
    let mut stack = vec![(root.clone(), 0usize)];
    let mut visited = 0usize;
    let started = Instant::now();
    while let Some((directory, depth)) = stack.pop() {
        if visited >= MAX_PROJECT_DIRECTORIES || started.elapsed() >= PROJECT_SCAN_BUDGET {
            break;
        }
        visited += 1;
        // A nested module is a separate package graph. It is scanned only
        // when go.work explicitly lists it, never as part of its parent.
        if directory != root && directory.join("go.mod").is_file() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let mut has_source = false;
        let mut children = Vec::new();
        for entry in entries.flatten() {
            if visited + stack.len() + children.len() >= MAX_PROJECT_DIRECTORIES
                || started.elapsed() >= PROJECT_SCAN_BUDGET
            {
                break;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_file() {
                has_source |= entry.path().extension().and_then(|value| value.to_str())
                    == Some("go")
                    && entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| !name.starts_with(['.', '_']));
                continue;
            }
            if depth >= MAX_PROJECT_DEPTH || !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.starts_with('.')
                || name.starts_with('_')
                || matches!(name.as_str(), "vendor" | "testdata")
            {
                continue;
            }
            children.push((entry.path(), depth + 1));
        }
        if has_source {
            packages.push(directory);
            if packages.len() >= MAX_PROJECT_ITEMS {
                break;
            }
        }
        stack.extend(children);
    }
    packages
}

impl ToolchainProvider {
    fn cmake_items(&self, directory: &Path, kind: CmakePresetKind) -> Vec<NamedItem> {
        let Some(root_file) =
            find_ancestor_any(directory, &["CMakeUserPresets.json", "CMakePresets.json"])
        else {
            return Vec::new();
        };
        let root = root_file.parent().unwrap_or(directory).to_path_buf();
        let key = canonical_or(root.clone()).join(format!(
            ".hokan-cmake-preset-cache-{}",
            cmake_preset_kind_name(kind)
        ));
        let Some(items) = cached(&self.cmake, key, || load_cmake_presets(&root, kind)) else {
            return Vec::new();
        };
        items.as_ref().clone()
    }

    fn ninja_items(&self, manifest: &Path) -> Vec<NamedItem> {
        let key = canonical_or(manifest.to_path_buf());
        let Some(items) = cached(&self.ninja, key, || load_ninja_targets(manifest)) else {
            return Vec::new();
        };
        items.as_ref().clone()
    }
}

fn cmake_preset_kind_name(kind: CmakePresetKind) -> &'static str {
    match kind {
        CmakePresetKind::Configure => "configure",
        CmakePresetKind::Build => "build",
        CmakePresetKind::Test => "test",
        CmakePresetKind::Package => "package",
        CmakePresetKind::Workflow => "workflow",
    }
}

fn load_cmake_presets(root: &Path, kind: CmakePresetKind) -> Option<Vec<NamedItem>> {
    let mut files = Vec::new();
    let user = root.join("CMakeUserPresets.json");
    let project = root.join("CMakePresets.json");
    if project.is_file() {
        files.push(project);
    }
    if user.is_file() {
        files.push(user);
    }
    if files.is_empty() {
        return None;
    }
    let mut visited = BTreeSet::new();
    let mut values = Vec::new();
    for file in files {
        load_cmake_preset_file(&file, &mut visited, &mut values, 0);
    }
    let field = match kind {
        CmakePresetKind::Configure => "configurePresets",
        CmakePresetKind::Build => "buildPresets",
        CmakePresetKind::Test => "testPresets",
        CmakePresetKind::Package => "packagePresets",
        CmakePresetKind::Workflow => "workflowPresets",
    };
    let annotation = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("CMake presets")
        .to_owned();
    let mut presets_by_name = BTreeMap::new();
    for value in &values {
        let Some(presets) = value.get(field).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for preset in presets {
            if let Some(name) = preset.get("name").and_then(serde_json::Value::as_str) {
                presets_by_name.entry(name.to_owned()).or_insert(preset);
            }
        }
    }
    let mut items = Vec::new();
    for value in &values {
        let Some(presets) = value.get(field).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for preset in presets {
            if preset
                .get("hidden")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                || !cmake_preset_is_enabled(preset, &presets_by_name)
            {
                continue;
            }
            let Some(name) = preset.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if name.is_empty() || name.contains(['\0', '\n', '\r']) {
                continue;
            }
            let description = preset
                .get("description")
                .or_else(|| preset.get("displayName"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("CMake preset");
            items.push(NamedItem::new(name, description).annotated(&annotation));
        }
    }
    Some(items)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CmakeInheritedCondition {
    Absent,
    Known(bool),
    Unknown,
}

fn cmake_preset_is_enabled(
    preset: &serde_json::Value,
    presets: &BTreeMap<String, &serde_json::Value>,
) -> bool {
    if let Some(condition) = preset.get("condition") {
        return condition.is_null() || cmake_preset_condition_allows(Some(condition));
    }
    let mut visiting = BTreeSet::new();
    match cmake_inherited_condition(preset, presets, &mut visiting) {
        CmakeInheritedCondition::Absent | CmakeInheritedCondition::Known(true) => true,
        CmakeInheritedCondition::Known(false) | CmakeInheritedCondition::Unknown => false,
    }
}

fn cmake_inherited_condition(
    preset: &serde_json::Value,
    presets: &BTreeMap<String, &serde_json::Value>,
    visiting: &mut BTreeSet<String>,
) -> CmakeInheritedCondition {
    if let Some(condition) = preset.get("condition") {
        return if condition.is_null() {
            CmakeInheritedCondition::Absent
        } else {
            cmake_preset_condition_value(Some(condition)).map_or(
                CmakeInheritedCondition::Unknown,
                CmakeInheritedCondition::Known,
            )
        };
    }
    let Some(parents) = cmake_preset_parents(preset) else {
        return CmakeInheritedCondition::Unknown;
    };
    for parent in parents {
        if !visiting.insert(parent.to_owned()) {
            return CmakeInheritedCondition::Unknown;
        }
        let state = presets
            .get(parent)
            .map_or(CmakeInheritedCondition::Unknown, |preset| {
                cmake_inherited_condition(preset, presets, visiting)
            });
        visiting.remove(parent);
        if state != CmakeInheritedCondition::Absent {
            return state;
        }
    }
    CmakeInheritedCondition::Absent
}

fn cmake_preset_parents(preset: &serde_json::Value) -> Option<Vec<&str>> {
    match preset.get("inherits") {
        None => Some(Vec::new()),
        Some(serde_json::Value::String(parent)) => Some(vec![parent]),
        Some(serde_json::Value::Array(parents)) => parents
            .iter()
            .map(serde_json::Value::as_str)
            .collect::<Option<Vec<_>>>(),
        Some(_) => None,
    }
}

fn cmake_preset_condition_allows(condition: Option<&serde_json::Value>) -> bool {
    cmake_preset_condition_value(condition) == Some(true)
}

fn cmake_preset_condition_value(condition: Option<&serde_json::Value>) -> Option<bool> {
    match condition {
        None | Some(serde_json::Value::Null) => Some(true),
        Some(serde_json::Value::Bool(value)) => Some(*value),
        Some(serde_json::Value::Object(value)) => match value.get("type")?.as_str()? {
            "const" => value.get("value")?.as_bool(),
            "not" => cmake_preset_condition_value(value.get("condition")).map(|result| !result),
            "anyOf" => cmake_condition_group(value.get("conditions")?, false),
            "allOf" => cmake_condition_group(value.get("conditions")?, true),
            "equals" | "notEquals" => {
                let left = cmake_condition_literal(value.get("lhs")?)?;
                let right = cmake_condition_literal(value.get("rhs")?)?;
                let equal = left == right;
                Some(if value.get("type")?.as_str()? == "equals" {
                    equal
                } else {
                    !equal
                })
            }
            "inList" | "notInList" => {
                let needle = cmake_condition_literal(value.get("string")?)?;
                let haystack = value.get("list")?.as_array()?;
                let mut found = false;
                for item in haystack {
                    found |= cmake_condition_literal(item)? == needle;
                }
                Some(if value.get("type")?.as_str()? == "inList" {
                    found
                } else {
                    !found
                })
            }
            "matches" | "notMatches" => {
                let subject = cmake_condition_literal(value.get("string")?)?;
                let pattern = cmake_condition_literal(value.get("regex")?)?;
                let matched = regex::Regex::new(pattern).ok()?.is_match(subject);
                Some(if value.get("type")?.as_str()? == "matches" {
                    matched
                } else {
                    !matched
                })
            }
            _ => None,
        },
        Some(_) => None,
    }
}

fn cmake_condition_group(value: &serde_json::Value, all: bool) -> Option<bool> {
    let conditions = value.as_array()?;
    let mut unknown = false;
    for condition in conditions {
        match cmake_preset_condition_value(Some(condition)) {
            Some(result) if result != all => return Some(!all),
            Some(_) => {}
            None => unknown = true,
        }
    }
    (!unknown).then_some(all)
}

fn cmake_condition_literal(value: &serde_json::Value) -> Option<&str> {
    value
        .as_str()
        .filter(|literal| !literal.contains('$') && !literal.contains(['\0', '\n', '\r']))
}

fn load_cmake_preset_file(
    path: &Path,
    visited: &mut BTreeSet<PathBuf>,
    values: &mut Vec<serde_json::Value>,
    depth: usize,
) {
    if depth >= 8 || values.len() >= 32 {
        return;
    }
    let path = canonical_or(path.to_path_buf());
    if !visited.insert(path.clone()) {
        return;
    }
    let Some(text) = read_bounded_text(&path) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    if let Some(includes) = value.get("include") {
        let include_values: Vec<&str> = match includes {
            serde_json::Value::String(value) => vec![value.as_str()],
            serde_json::Value::Array(values) => values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect(),
            _ => Vec::new(),
        };
        if let Some(parent) = path.parent() {
            for include in include_values {
                if include.contains('$') {
                    continue;
                }
                let include = PathBuf::from(include);
                let include = if include.is_absolute() {
                    include
                } else {
                    parent.join(include)
                };
                load_cmake_preset_file(&include, visited, values, depth + 1);
            }
        }
    }
    values.push(value);
}

fn load_ninja_targets(path: &Path) -> Option<Vec<NamedItem>> {
    let text = read_bounded_text(path)?;
    let annotation = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("build.ninja")
        .to_owned();
    let logical = ninja_logical_lines(&text);
    let mut phony = BTreeSet::new();
    let mut defaults = BTreeSet::new();
    for line in logical.lines() {
        let trimmed = ninja_strip_comment(line).trim_start();
        if let Some(rest) = trimmed.strip_prefix("build ") {
            let Some(colon) = ninja_unescaped_colon(rest) else {
                continue;
            };
            let outputs = &rest[..colon];
            let rule = rest[colon + 1..]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            if rule == "phony" {
                phony.extend(ninja_words(outputs));
            }
        } else if let Some(rest) = trimmed.strip_prefix("default ") {
            defaults.extend(ninja_words(rest));
        }
    }
    phony.extend(defaults);
    Some(
        phony
            .into_iter()
            .filter(|target| !target.contains('$') && !target.is_empty())
            .map(|target| NamedItem::new(target, "Ninja build target").annotated(&annotation))
            .collect(),
    )
}

fn ninja_strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'$' {
            index = index.saturating_add(2);
            continue;
        }
        if bytes[index] == b'#' {
            return &line[..index];
        }
        index += 1;
    }
    line
}

fn ninja_logical_lines(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut continued = false;
    for line in text.lines() {
        let mut line = line.trim_end_matches('\r');
        if continued {
            line = line.trim_start();
        }
        if let Some(prefix) = line.strip_suffix('$') {
            output.push_str(prefix);
            continued = true;
        } else {
            output.push_str(line);
            output.push('\n');
            continued = false;
        }
    }
    output
}

fn ninja_unescaped_colon(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'$' {
            index = index.saturating_add(2);
            continue;
        }
        if bytes[index] == b':' {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn ninja_words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character == '$' {
            if let Some(escaped) = characters.next() {
                if matches!(escaped, ' ' | ':' | '#') {
                    current.push(escaped);
                } else {
                    // Keep variable references visible so the caller can
                    // omit unresolved targets instead of turning `$out`
                    // into the bogus literal target `out`.
                    current.push('$');
                    current.push(escaped);
                }
            } else {
                current.push('$');
            }
        } else if character.is_whitespace() {
            if !current.is_empty() {
                if current != "|" && current != "||" {
                    words.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
        } else {
            current.push(character);
        }
    }
    if !current.is_empty() && current != "|" && current != "||" {
        words.push(current);
    }
    words
}

fn installed_rust_toolchains() -> Vec<String> {
    let Some(home) = rustup_home() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(home.join("toolchains")) else {
        return Vec::new();
    };
    let mut names = BTreeSet::new();
    for entry in entries.flatten().take(MAX_PROJECT_ITEMS) {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        for channel in ["stable", "beta", "nightly"] {
            let host_qualified = name
                .strip_prefix(&format!("{channel}-"))
                .is_some_and(|suffix| {
                    suffix
                        .chars()
                        .next()
                        .is_some_and(|character| !character.is_ascii_digit())
                });
            if name == channel || host_qualified {
                names.insert(channel.to_owned());
            }
        }
        names.insert(name);
    }
    names.into_iter().collect()
}

fn installed_rust_targets(selected_toolchain: Option<&str>) -> Vec<String> {
    let Some(home) = rustup_home() else {
        return Vec::new();
    };
    let Ok(toolchains) = fs::read_dir(home.join("toolchains")) else {
        return Vec::new();
    };
    let mut targets = BTreeSet::new();
    for toolchain in toolchains.flatten().take(64) {
        let Some(toolchain_name) = toolchain.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if selected_toolchain
            .is_some_and(|selected| !rust_toolchain_name_matches(&toolchain_name, selected))
        {
            continue;
        }
        let root = toolchain.path().join("lib/rustlib");
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten().take(MAX_PROJECT_ITEMS) {
            if entry.path().is_dir()
                && entry.path().join("lib").is_dir()
                && let Some(name) = entry.file_name().to_str()
                && name.contains('-')
            {
                targets.insert(name.to_owned());
            }
        }
    }
    targets.into_iter().collect()
}

fn rust_toolchain_name_matches(installed: &str, selected: &str) -> bool {
    if installed == selected {
        return true;
    }
    let official_shorthand = matches!(selected, "stable" | "beta" | "nightly")
        || selected
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
        || ["stable-", "beta-", "nightly-"].iter().any(|prefix| {
            selected
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.chars().next().is_some_and(|c| c.is_ascii_digit()))
        });
    official_shorthand
        && installed
            .strip_prefix(selected)
            .is_some_and(|suffix| suffix.starts_with('-') && suffix.len() > 1)
}

fn rustup_home() -> Option<PathBuf> {
    std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".rustup")))
}

fn read_bounded_text(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_METADATA_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn find_ancestor_file(start: &Path, name: &str) -> Option<PathBuf> {
    let start = if start.is_dir() {
        start
    } else {
        start.parent()?
    };
    start
        .ancestors()
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

fn find_ancestor_any(start: &Path, names: &[&str]) -> Option<PathBuf> {
    let start = if start.is_dir() {
        start
    } else {
        start.parent()?
    };
    for directory in start.ancestors() {
        for name in names {
            let path = directory.join(name);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn walk_directories(root: &Path) -> Vec<PathBuf> {
    let root = canonical_or(root.to_path_buf());
    let mut output = Vec::new();
    let mut stack = vec![(root, 0usize)];
    let started = Instant::now();
    while let Some((directory, depth)) = stack.pop() {
        if output.len() >= MAX_PROJECT_DIRECTORIES || started.elapsed() >= PROJECT_SCAN_BUDGET {
            break;
        }
        output.push(directory.clone());
        if depth >= MAX_PROJECT_DEPTH {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if output.len() + stack.len() >= MAX_PROJECT_DIRECTORIES
                || started.elapsed() >= PROJECT_SCAN_BUDGET
            {
                break;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.starts_with('.')
                || matches!(
                    name.as_str(),
                    "target" | "vendor" | "node_modules" | "testdata"
                )
                || name.starts_with('_')
            {
                continue;
            }
            stack.push((entry.path(), depth + 1));
        }
    }
    output
}

fn canonical_or(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            std::path::Component::ParentDir => Some(".."),
            std::path::Component::CurDir => Some("."),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from = canonical_or(from.to_path_buf());
    let to = canonical_or(to.to_path_buf());
    let from_components = from.components().collect::<Vec<_>>();
    let to_components = to.components().collect::<Vec<_>>();
    let common = from_components
        .iter()
        .zip(&to_components)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in common..from_components.len() {
        relative.push("..");
    }
    for component in &to_components[common..] {
        relative.push(component.as_os_str());
    }
    relative
}

fn display_relative(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, os::unix::fs::PermissionsExt};

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    fn context(directory: &Path, text: &str) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            directory.to_path_buf(),
            BufferSnapshot::new(text, text.len(), BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context")
    }

    fn engine(directory: &Path, commands: &[&str]) -> CompletionEngine {
        let bin = directory.join("bin");
        fs::create_dir_all(&bin).expect("bin");
        for command in commands {
            let path = bin.join(command);
            fs::write(&path, b"#!/bin/sh\n").expect("executable");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("mode");
        }
        let commands = Arc::new(CommandPathCache::from_path(Some(&OsString::from(bin))));
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(ToolchainProvider::new(commands));
        engine
    }

    fn replacements(output: ProviderOutput) -> Vec<String> {
        output
            .candidates
            .into_iter()
            .filter_map(|candidate| candidate.edit.map(|edit| edit.replacement))
            .collect()
    }

    #[test]
    fn cargo_workspace_values_come_only_from_manifests_and_targets() {
        let root = tempfile::tempdir().expect("workspace");
        fs::create_dir_all(root.path().join("crates/app/src/bin")).expect("app dirs");
        fs::create_dir_all(root.path().join("crates/lib/src")).expect("lib dirs");
        fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n[profile.fast]\ninherits = \"release\"\n",
        )
        .expect("workspace manifest");
        fs::write(
            root.path().join("crates/app/Cargo.toml"),
            "[package]\nname = \"app-cli\"\nversion = \"0.1.0\"\n[dependencies]\nserde = { version = \"1\", optional = true }\ntracing = { version = \"1\", optional = true }\n[features]\ndefault = []\nlogging = [\"dep:tracing\"]\n[[bin]]\nname = \"admin\"\npath = \"src/bin/admin.rs\"\n",
        )
        .expect("app manifest");
        fs::write(root.path().join("crates/app/src/main.rs"), "fn main() {}").expect("main");
        fs::write(
            root.path().join("crates/app/src/bin/admin.rs"),
            "fn main() {}",
        )
        .expect("admin");
        fs::write(
            root.path().join("crates/lib/Cargo.toml"),
            "[package]\nname = \"shared-lib\"\nversion = \"0.1.0\"\n",
        )
        .expect("lib manifest");
        let engine = engine(root.path(), &["cargo"]);

        let packages = replacements(engine.complete(&context(root.path(), "cargo build -p ap")));
        assert_eq!(packages, ["app-cli"]);
        let features = replacements(engine.complete(&context(
            &root.path().join("crates/app"),
            "cargo build --features lo",
        )));
        assert_eq!(features, ["logging"]);
        assert_eq!(
            replacements(engine.complete(&context(
                &root.path().join("crates/app"),
                "cargo build --features se",
            ))),
            ["serde"],
            "optional dependencies expose their implicit Cargo feature"
        );
        assert!(
            engine
                .complete(&context(
                    &root.path().join("crates/app"),
                    "cargo build --features tr",
                ))
                .candidates
                .is_empty(),
            "dep: syntax suppresses the optional dependency's implicit feature"
        );
        assert_eq!(
            replacements(engine.complete(&context(
                &root.path().join("crates/app"),
                "cargo build --features logging,",
            ))),
            ["logging,serde", "logging,default"]
        );
        assert_eq!(
            replacements(
                engine.complete(&context(root.path(), "cargo build --features app-cli/lo",))
            ),
            ["app-cli/logging"]
        );
        let bins = replacements(engine.complete(&context(
            &root.path().join("crates/app"),
            "cargo run --bin ad",
        )));
        assert_eq!(bins, ["admin"]);
        let profiles =
            replacements(engine.complete(&context(root.path(), "cargo build --profile f")));
        assert_eq!(profiles, ["fast"]);
        assert!(
            engine
                .complete(&context(root.path(), "cargo update -p ap"))
                .candidates
                .is_empty(),
            "cargo update package specs are not workspace-package slots"
        );
        assert_eq!(
            cargo_subcommand(&["+nightly", "--locked", "build"]),
            Some("build")
        );
        assert_eq!(cargo_subcommand(&["--target", "wasm32-wasip1"]), None);
    }

    #[test]
    fn go_packages_are_derived_from_local_module_directories() {
        let root = tempfile::tempdir().expect("module");
        fs::create_dir_all(root.path().join("cmd/server")).expect("server");
        fs::create_dir_all(root.path().join("internal/store")).expect("store");
        fs::write(
            root.path().join("go.mod"),
            "module example.com/acme\n\ngo 1.25\n\ntool (\nexample.com/tools/cmd/mockgen\nexample.com/tools/cmd/stringer\n)\n",
        )
        .expect("go.mod");
        fs::write(root.path().join("cmd/server/main.go"), "package main\n").expect("main.go");
        fs::write(root.path().join("main.go"), "package acme\n").expect("root go file");
        fs::write(
            root.path().join("internal/store/store.go"),
            "package store\n",
        )
        .expect("store.go");
        fs::create_dir_all(root.path().join("nested/module")).expect("nested module");
        fs::write(
            root.path().join("nested/go.mod"),
            "module example.com/nested\n\ngo 1.25\n",
        )
        .expect("nested go.mod");
        fs::write(
            root.path().join("nested/module/nested.go"),
            "package module\n",
        )
        .expect("nested package");
        let engine = engine(root.path(), &["go"]);

        let local = replacements(engine.complete(&context(root.path(), "go test ./cmd")));
        assert_eq!(local, ["./cmd/server"]);
        let import =
            replacements(engine.complete(&context(root.path(), "go test example.com/acme/int")));
        assert_eq!(import, ["example.com/acme/internal/store"]);
        assert_eq!(
            replacements(engine.complete(&context(root.path(), "go tool mo"))),
            ["mockgen"]
        );
        assert_eq!(
            replacements(engine.complete(&context(root.path(), "go tool str"))),
            ["stringer"]
        );
        let dot = replacements(engine.complete(&context(root.path(), "go test .")));
        assert!(dot.contains(&"./...".to_owned()), "rows: {dot:?}");
        assert!(
            engine
                .complete(&context(root.path(), "go test ./nested"))
                .candidates
                .is_empty(),
            "a parent module must not leak packages from a nested go.mod"
        );
        assert!(
            engine
                .complete(&context(root.path(), "go run . argument"))
                .candidates
                .is_empty(),
            "program arguments must not be treated as package slots"
        );
        assert!(
            engine
                .complete(&context(root.path(), "go doc . Symbol"))
                .candidates
                .is_empty(),
            "the symbol after a go doc package is literal text"
        );
    }

    #[test]
    fn cmake_presets_ninja_targets_and_jvm_tasks_are_static() {
        let root = tempfile::tempdir().expect("project");
        fs::write(
            root.path().join("CMakePresets.json"),
            r#"{"version": 6, "configurePresets": [{"name":"debug", "displayName":"Debug"}, {"name":"debug build"}, {"name":"disabled", "condition": false}, {"name":"disabled-base", "hidden":true, "condition":false}, {"name":"inherited-disabled", "inherits":"disabled-base"}, {"name":"null-base", "hidden":true, "condition":null}, {"name":"inherits-null", "inherits":"null-base"}, {"name":"literal-enabled", "condition":{"type":"equals","lhs":"Darwin","rhs":"Darwin"}}, {"name":"unknown-condition", "condition":{"type":"equals","lhs":"${hostSystemName}","rhs":"Darwin"}}], "buildPresets": [{"name":"debug-build"}]}"#,
        )
        .expect("presets");
        fs::write(
            root.path().join("build.ninja"),
            "build all: phony app # the default target\nbuild real$ target: phony\nbuild $generated: phony\nbuild app: CXX_EXECUTABLE_LINKER app.o\ndefault all # comments are not targets\n",
        )
        .expect("ninja");
        let engine = engine(
            root.path(),
            &["cmake", "ninja", "mvn", "gradle", "xcodebuild"],
        );

        assert_eq!(
            replacements(engine.complete(&context(root.path(), "cmake --preset de"))),
            ["debug", "'debug build'"]
        );
        assert_eq!(
            replacements(engine.complete(&context(root.path(), "cmake --build --preset de"))),
            ["debug-build"]
        );
        assert!(
            engine
                .complete(&context(
                    root.path(),
                    "cmake --build explicit-dir --preset de",
                ))
                .candidates
                .is_empty(),
            "the directory and preset forms of cmake --build are mutually exclusive"
        );
        assert_eq!(
            replacements(engine.complete(&context(root.path(), "cmake --preset debug"))),
            ["'debug build'"]
        );
        assert!(
            engine
                .complete(&context(root.path(), "cmake --preset dis"))
                .candidates
                .is_empty(),
            "a statically disabled preset must not be offered"
        );
        assert_eq!(
            replacements(engine.complete(&context(root.path(), "cmake --preset literal"))),
            ["literal-enabled"]
        );
        assert!(
            engine
                .complete(&context(root.path(), "cmake --preset unknown"))
                .candidates
                .is_empty(),
            "a condition that cannot be evaluated statically must stay quiet"
        );
        assert!(
            engine
                .complete(&context(root.path(), "cmake --preset inherited-d"))
                .candidates
                .is_empty(),
            "a disabled condition inherited from a base preset must be respected"
        );
        assert_eq!(
            replacements(engine.complete(&context(root.path(), "cmake --preset inherits-n"))),
            ["inherits-null"],
            "an explicit null condition is enabled and is not inherited"
        );
        assert_eq!(
            replacements(engine.complete(&context(root.path(), "ninja a"))),
            ["all"]
        );
        assert_eq!(
            replacements(engine.complete(&context(root.path(), "ninja real"))),
            ["'real target'"]
        );
        assert!(
            engine
                .complete(&context(root.path(), "ninja -t targets "))
                .candidates
                .is_empty(),
            "tool arguments must not fall back to ordinary build targets"
        );
        assert_eq!(
            replacements(engine.complete(&context(root.path(), "mvn cl"))),
            ["clean"]
        );
        let after_clean = replacements(engine.complete(&context(root.path(), "mvn clean ")));
        assert!(!after_clean.contains(&"clean".to_owned()));
        assert!(after_clean.contains(&"install".to_owned()));
        assert_eq!(
            replacements(engine.complete(&context(root.path(), "gradle ta"))),
            ["tasks"]
        );
        assert_eq!(
            replacements(engine.complete(&context(root.path(), "xcodebuild -scheme App bu",))),
            ["build", "build-for-testing"]
        );
        assert_eq!(
            replacements(engine.complete(&context(root.path(), "xcodebuild clean bu"))),
            ["build", "build-for-testing"]
        );
        assert!(
            engine
                .complete(&context(root.path(), "gradle"))
                .candidates
                .is_empty()
        );
        assert!(
            engine
                .complete(&context(root.path(), "xcodebuild"))
                .candidates
                .is_empty(),
            "xcodebuild without an action is itself runnable"
        );
    }

    #[test]
    fn rustup_positions_continue_into_the_required_next_slot() {
        let (query, edit_prefix, next_slot) =
            rustup_toolchain_position(&["run", "--install"], "sta").expect("run toolchain");
        assert_eq!(query, "sta");
        assert!(edit_prefix.is_empty());
        assert_eq!(next_slot, Some(crate::completion::SlotKind::Executable));

        let (query, edit_prefix, next_slot) =
            rustup_toolchain_position(&["target", "remove"], "--toolchain=sta")
                .expect("target toolchain flag");
        assert_eq!(query, "sta");
        assert_eq!(edit_prefix, "--toolchain=");
        assert_eq!(next_slot, Some(crate::completion::SlotKind::Value));

        assert_eq!(
            rustup_target_remove_position(
                &[
                    "target",
                    "remove",
                    "--toolchain",
                    "nightly",
                    "wasm32-wasip1"
                ],
                "aarch64",
            ),
            Some(BTreeSet::from(["wasm32-wasip1".to_owned()]))
        );
        assert_eq!(
            rust_toolchain_selection("rustup", &["target", "remove", "--toolchain", "nightly"]),
            Some("nightly".to_owned())
        );
        assert!(rust_toolchain_name_matches(
            "nightly-aarch64-apple-darwin",
            "nightly"
        ));
        assert!(rust_toolchain_name_matches(
            "nightly-2026-08-10-aarch64-apple-darwin",
            "nightly-2026-08-10"
        ));
        assert!(!rust_toolchain_name_matches("custom-nightly", "custom"));
    }
}
