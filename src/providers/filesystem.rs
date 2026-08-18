use std::{collections::VecDeque, fs, sync::Arc, time::Instant};

use crate::{
    completion::{
        Candidate, CandidateAction, CandidateKind, CandidateProvider, CandidateSource,
        Completeness, CompletionContext, CursorPlacement, ProviderDiagnostic, ProviderOutput,
        SlotKind, TextEdit,
    },
    parser::escape_for_shell,
    providers::{CommandHelpCache, argument_progress},
    shell::{AliasCache, AliasKind},
    specs::SpecRegistry,
    terminal::RiskLevel,
};

const MAX_DIRECTORY_ENTRIES: usize = 5_000;
const DIRECTORY_BUDGET_MS: u128 = 80;

pub struct FilesystemProvider {
    show_hidden: bool,
    specs: Arc<SpecRegistry>,
    help: Arc<CommandHelpCache>,
    aliases: Arc<AliasCache>,
}

impl FilesystemProvider {
    #[must_use]
    pub fn new(
        show_hidden: bool,
        specs: Arc<SpecRegistry>,
        help: Arc<CommandHelpCache>,
        aliases: Arc<AliasCache>,
    ) -> Self {
        Self {
            show_hidden,
            specs,
            help,
            aliases,
        }
    }
}

impl CandidateProvider for FilesystemProvider {
    fn id(&self) -> &'static str {
        "filesystem"
    }

    fn applies(&self, context: &CompletionContext) -> bool {
        self.infer_slot(context).is_some()
    }

    fn complete(&self, context: &CompletionContext) -> ProviderOutput {
        let Some(slot) = self.infer_slot(context) else {
            return ProviderOutput::default();
        };
        let prefix = slot.prefix;
        let (directory_prefix, basename) = split_prefix(prefix);
        let scan_directory = scan_directory_for(
            slot.base_dir.as_deref().unwrap_or(context.cwd.as_ref()),
            directory_prefix,
        );
        let started = Instant::now();
        let entries = match fs::read_dir(&scan_directory) {
            Ok(entries) => entries,
            Err(error) => {
                return ProviderOutput {
                    candidates: Vec::new(),
                    diagnostics: vec![ProviderDiagnostic {
                        provider: self.id(),
                        code: "HK-FS-001",
                        message: format!("cannot read {}: {error}", scan_directory.display()),
                    }],
                };
            }
        };
        let file_next_slot = required_file_followup_slot(context, slot.kind);
        let show_hidden = self.show_hidden || basename.starts_with('.');
        let mut candidates = Vec::new();
        let mut partial = false;
        for (position, entry) in entries.enumerate() {
            if position >= MAX_DIRECTORY_ENTRIES {
                partial = true;
                break;
            }
            if started.elapsed().as_millis() > DIRECTORY_BUDGET_MS {
                partial = true;
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if (!show_hidden && name.starts_with('.')) || name.contains(['\0', '\n', '\r']) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let metadata = entry.metadata().ok();
            if metadata.is_none() && !file_type.is_symlink() {
                continue;
            }
            let directory =
                metadata.as_ref().is_some_and(fs::Metadata::is_dir) || file_type.is_dir();
            if !slot_accepts(slot.kind, &path, directory) {
                continue;
            }
            let mut logical = format!("{directory_prefix}{name}");
            // A leading dash makes the path look like a flag (`cd -foo/`);
            // prefix `./` for directories and files alike.
            if directory_prefix.is_empty() && logical.starts_with('-') {
                logical.insert_str(0, "./");
            }
            if directory {
                logical.push('/');
            }
            // The edit replaces the complete raw token, including any open quote.
            let escaped = escape_path_for_shell(&logical, context.shell);
            let replacement = format!("{}{}", slot.edit_prefix, escaped);
            if replacement.is_empty() {
                continue;
            }
            let executable = crate::platform::is_executable(&path);
            let next_slot = if directory {
                Some(SlotKind::Path)
            } else {
                file_next_slot
            };
            let mut candidate = Candidate::new(
                context.query_id,
                &logical,
                if directory {
                    "进入目录继续补全"
                } else if matches!(slot.kind, SlotKind::Executable) {
                    "当前目录中的可执行文件"
                } else {
                    "当前目录中的文件"
                },
                Some(TextEdit {
                    range: context.parsed.replacement.clone(),
                    replacement,
                    cursor_after: CursorPlacement::End,
                }),
                next_slot.map_or(CandidateAction::Insert, |next_slot| {
                    CandidateAction::InsertAndContinue { next_slot }
                }),
                CandidateSource::Filesystem,
                if directory {
                    CandidateKind::Directory
                } else {
                    CandidateKind::File
                },
                next_slot.map_or(Completeness::Runnable, |slot| Completeness::NeedsInput {
                    slot,
                }),
                RiskLevel::Low,
                format!("fs:{}", path.display()),
            );
            candidate.score.cwd_affinity = 100;
            candidate.score.spec_priority = match (slot.kind, directory, executable) {
                (SlotKind::Executable, false, true) => 120,
                (SlotKind::Executable, true, _) => 60,
                // Descending is the norm at path-like slots: keep
                // directories level with files instead of sinking them
                // below the siblings the user drills through.
                (SlotKind::Path | SlotKind::Directory | SlotKind::NewFile, true, _) => 80,
                (_, true, _) => 50,
                _ => 80,
            };
            candidates.push(candidate);
        }
        ProviderOutput {
            candidates,
            diagnostics: partial
                .then_some(ProviderDiagnostic {
                    provider: self.id(),
                    code: "HK-FS-002",
                    message: format!(
                        "partial directory results for {} (80 ms / 5000 entry budget)",
                        scan_directory.display()
                    ),
                })
                .into_iter()
                .collect(),
        }
    }
}

impl FilesystemProvider {
    fn infer_slot<'a>(&self, context: &'a CompletionContext) -> Option<FilesystemSlot<'a>> {
        if super::redirect_target(context) {
            return super::redirect_path_target(context).then(|| {
                FilesystemSlot::plain(SlotKind::Path, context.parsed.current_prefix.as_str())
            });
        }
        if let Some(slot) = wrapper_filesystem_slot(context) {
            return Some(slot);
        }
        if super::explicit_executable_path_position(context) {
            let mut slot =
                FilesystemSlot::plain(SlotKind::Executable, context.parsed.current_prefix.as_str());
            slot.base_dir = Some(super::invocation_working_directory(context));
            return Some(slot);
        }
        let raw_command = context.command()?;
        let command = if super::python_module::is_python_command(raw_command) {
            "python"
        } else {
            super::executable_basename(raw_command)
        };
        let resolution = crate::providers::command_resolution_kind(context);
        if self.inferred_function_argument(context) {
            return None;
        }
        if resolution == crate::parser::EffectiveCommandKind::Builtin
            && !matches!(command, "cd" | "source" | ".")
        {
            return None;
        }
        let (mut words, argument_position) = argument_progress(context)?;
        if let Some(effective) = words.first_mut() {
            *effective = command;
        }
        let prefix = context.parsed.current_prefix.as_str();
        let command_base =
            command_directory_before_active(context, command, &words, argument_position);
        if command == "find" && super::find_exec_command_position(context) {
            if !prefix.contains('/') {
                return None;
            }
            let mut slot = FilesystemSlot::plain(SlotKind::Executable, prefix);
            slot.base_dir = Some(if prefix.starts_with('/') {
                super::invocation_working_directory(context)
            } else {
                super::find_exec_working_directory(context)?
            });
            return Some(slot);
        }
        if super::explicit_executable_argument_path_position(context) {
            let mut slot = FilesystemSlot::plain(SlotKind::Executable, prefix);
            slot.base_dir = Some(command_base);
            return Some(slot);
        }
        if let Some(value) = response_file_prefix(command, prefix) {
            return Some(FilesystemSlot {
                kind: SlotKind::Path,
                prefix: value,
                edit_prefix: "@",
                base_dir: Some(command_base.clone()),
            });
        }
        if command == "tar" {
            return tar_slot(context, &words, argument_position);
        }
        let flags_ended = flags_ended_before(&words, argument_position);
        if !flags_ended
            && let Some(mut slot) = structured_path_value_slot(
                command,
                words.get(argument_position).copied().unwrap_or_default(),
                prefix,
            )
        {
            slot.base_dir = Some(command_base.clone());
            return Some(slot);
        }
        if !flags_ended
            && let Some(mut slot) = hybrid_explicit_path_slot(
                command,
                words.get(argument_position).copied().unwrap_or_default(),
                prefix,
            )
        {
            slot.base_dir = Some(command_base.clone());
            return Some(slot);
        }
        // An attached flag value (`--output=fi`, `-Cdir`) owns only the value
        // suffix. Literal-value flags suppress filesystem rows; path flags
        // preserve the flag spelling in the edit.
        if !flags_ended && prefix.starts_with('-') {
            return attached_flag_value(command, prefix).and_then(|attached| {
                if command == "curl"
                    && curl_data_file_flag(attached.flag.trim_end_matches('='))
                    && let Some(value) = attached.value.strip_prefix('@')
                {
                    let edit_prefix = &prefix[..attached.flag.len() + 1];
                    return Some(FilesystemSlot {
                        kind: SlotKind::Path,
                        prefix: value,
                        edit_prefix,
                        base_dir: Some(command_base.clone()),
                    });
                }
                (attached.kind != SlotKind::Value).then_some(FilesystemSlot {
                    kind: attached.kind,
                    prefix: attached.value,
                    edit_prefix: attached.flag,
                    base_dir: Some(command_base.clone()),
                })
            });
        }
        if !flags_ended
            && let Some(slot) = flag_value_slot(
                command,
                words.get(argument_position).copied().unwrap_or_default(),
            )
        {
            if command == "curl"
                && curl_data_file_flag(words.get(argument_position).copied().unwrap_or_default())
            {
                return prefix.strip_prefix('@').map(|prefix| FilesystemSlot {
                    kind: SlotKind::Path,
                    prefix,
                    edit_prefix: "@",
                    base_dir: Some(command_base.clone()),
                });
            }
            return (slot != SlotKind::Value).then_some(FilesystemSlot {
                kind: slot,
                prefix,
                edit_prefix: "",
                base_dir: Some(command_base),
            });
        }
        let kind = match command {
            "cd" if resolution != crate::parser::EffectiveCommandKind::External
                && positional_arguments_before(command, &words, argument_position, &[])
                    .is_some_and(|count| count == 0) =>
            {
                Some(SlotKind::Directory)
            }
            "cd" => None,
            "pushd"
                if resolution != crate::parser::EffectiveCommandKind::External
                    && !prefix.starts_with('+')
                    && positional_arguments_before(command, &words, argument_position, &[])
                        .is_some_and(|count| count == 0) =>
            {
                Some(SlotKind::Directory)
            }
            "pushd" => None,
            "source" | "." if resolution == crate::parser::EffectiveCommandKind::External => None,
            "bash" | "zsh" | "sh"
                if !flag_before_active(command, &words, argument_position, &["-c"])
                    && positional_arguments_before(command, &words, argument_position, &[])
                        .is_some_and(|count| count == 0) =>
            {
                Some(SlotKind::File)
            }
            "bash" | "zsh" | "sh" => explicit_path(prefix).then_some(SlotKind::Path),
            "df" => Some(SlotKind::Path),
            "lsof" if words.get(argument_position).copied() == Some("+D") => {
                Some(SlotKind::Directory)
            }
            "lsof" => None,
            "sudo" if sudo_edit_mode(&words, argument_position) => Some(SlotKind::Path),
            "kill" | "ifconfig" | "ip" | "ps" => None,
            // Ref-taking git slots (`git checkout <…>`) belong to the git
            // provider's branch/remote/tag rows; `git add <path>` keeps
            // file completion.
            "git" if super::git::new_branch_slot(&words, argument_position) => None,
            "git"
                if at_git_ref_slot(&words, argument_position)
                    && !(explicit_git_path(prefix)
                        && super::git::ref_subcommand_accepts_paths(&words)) =>
            {
                None
            }
            // The ssh host slot belongs to the ssh-host provider.
            "ssh" | "sftp" | "mosh"
                if super::ssh::at_host_slot(
                    command,
                    &words,
                    argument_position,
                    &context.parsed.current_prefix,
                ) =>
            {
                None
            }
            // After the destination, these commands consume a remote command
            // or remote path. Local cwd entries would be valid-looking but
            // execute on the wrong machine.
            "ssh" | "sftp" | "mosh" => None,
            "scp"
                if super::ssh::at_scp_host_slot(
                    command,
                    &words,
                    argument_position,
                    &context.parsed.current_prefix,
                ) =>
            {
                None
            }
            "scp" if remote_path_prefix(prefix) => None,
            "rsync" if non_local_path_prefix(prefix) => None,
            // Build-tool first arguments are target names, not paths.
            "make" | "just" => None,
            // Package-manager command/script selection belongs to the project
            // provider. Re-enable paths only for known file-taking commands
            // or once the user has made path intent explicit (`./`, `../`,
            // `~/`, or another slash-containing prefix).
            _ if super::filter_position(context).is_some() => None,
            _ if super::package_manager(context).is_some() => {
                package_manager_path_slot(context, command, &words, argument_position, prefix)
            }
            _ => {
                // Spec-covered commands own the empty slot; with a typed
                // prefix the spec rows die on match anyway and file rows
                // must appear (`ls Do` completes `Documents/`).
                if self.specs.get(command).is_some() && context.parsed.current_prefix.is_empty() {
                    return None;
                }
                // A flag immediately before the active slot decides what the
                // slot wants: value flags (`git commit -m`, `ssh -p`) ask for
                // literal text, so raw file rows would be noise; file flags
                // (`curl -o`, `make -C`) want paths. `words[position]` is the
                // word before the active slot in both the trailing-space and
                // mid-typing cases (see `argument_progress`).
                // At the first-argument position a command whose man page
                // documents subcommands (git-style) takes subcommand rows
                // instead of a raw directory scan. Past the first argument —
                // and whenever help has no subcommands (`cp`, `vim`) — file
                // completion still applies. Peek only: the help provider is
                // registered first and warms the shared cache from its
                // applies pass; never spawn `man` from here.
                if super::command_help::dynamic_help_owns_position(context, &self.help) {
                    return None;
                }
                explicit_path(prefix)
                    .then_some(SlotKind::Path)
                    .or_else(|| default_path_slot(command, &words, argument_position))
            }
        }?;
        let mut slot = FilesystemSlot::plain(kind, prefix);
        if command == "git" {
            slot.base_dir = Some(super::git::git_working_directory(context, &words));
        } else if super::is_package_manager(command) {
            slot.base_dir = super::manager_project_dir(context);
        } else if let Some(directory) = super::delegated_command_working_directory(context) {
            slot.base_dir = Some(directory);
        } else {
            slot.base_dir = Some(super::invocation_working_directory(context));
        }
        Some(slot)
    }
}

impl FilesystemProvider {
    fn inferred_function_argument(&self, context: &CompletionContext) -> bool {
        if !super::effective_command_is_shell_command(context)
            || argument_progress(context).is_none_or(|(_, position)| position != 0)
        {
            return false;
        }
        let Some(command) = context.command() else {
            return false;
        };
        let aliases = self.aliases.load(context.shell);
        let Some(entry) = aliases.get(command) else {
            return false;
        };
        entry.kind == AliasKind::Function
            && entry.body.as_deref().is_some_and(|body| {
                crate::shell::infer_function_slot(context.shell, body).is_some()
            })
    }
}

#[derive(Clone)]
struct FilesystemSlot<'a> {
    kind: SlotKind,
    prefix: &'a str,
    edit_prefix: &'a str,
    base_dir: Option<std::path::PathBuf>,
}

impl<'a> FilesystemSlot<'a> {
    const fn plain(kind: SlotKind, prefix: &'a str) -> Self {
        Self {
            kind,
            prefix,
            edit_prefix: "",
            base_dir: None,
        }
    }
}

fn wrapper_filesystem_slot(context: &CompletionContext) -> Option<FilesystemSlot<'_>> {
    let tokens = super::segment_word_tokens(context);
    let words: Vec<&str> = tokens
        .iter()
        .map(|token| token.cooked_prefix.as_str())
        .collect();
    let active_word = tokens.last().is_some_and(|token| {
        context.buffer.cursor >= token.range.start && context.buffer.cursor <= token.range.end
    });
    let prefix = context.parsed.current_prefix.as_str();
    let analysis =
        crate::parser::effective_command_analysis_for_shell(&words, active_word, context.shell);

    if prefix.starts_with('-')
        && analysis.state == crate::parser::EffectiveCommandState::AwaitingCommand
    {
        let flag_index = words.len().checked_sub(1)?;
        let wrapper = nearest_wrapper(&words[..flag_index])?;
        if let Some((flag, value)) = prefix.split_once('=')
            && let Some(kind) = wrapper_flag_value_slot(wrapper, flag)
        {
            return Some(FilesystemSlot {
                kind,
                prefix: value,
                edit_prefix: &prefix[..flag.len() + 1],
                base_dir: Some(super::wrapper_working_directory_before(
                    context, &words, flag_index,
                )),
            });
        }
        for flag in wrapper_attached_short_flags(wrapper) {
            if prefix.len() > flag.len() && prefix.starts_with(flag) {
                return Some(FilesystemSlot {
                    kind: wrapper_flag_value_slot(wrapper, flag)?,
                    prefix: &prefix[flag.len()..],
                    edit_prefix: &prefix[..flag.len()],
                    base_dir: Some(super::wrapper_working_directory_before(
                        context, &words, flag_index,
                    )),
                });
            }
        }
    }

    if analysis.state != crate::parser::EffectiveCommandState::AwaitingWrapperValue {
        return None;
    }
    let flag_index = if active_word {
        words.len().checked_sub(2)?
    } else {
        words.len().checked_sub(1)?
    };
    let flag = words.get(flag_index).copied()?;
    let wrapper = nearest_wrapper(&words[..flag_index])?;
    let kind = wrapper_flag_value_slot(wrapper, flag)?;
    Some(FilesystemSlot {
        kind,
        prefix,
        edit_prefix: "",
        base_dir: Some(super::wrapper_working_directory_before(
            context, &words, flag_index,
        )),
    })
}

fn nearest_wrapper(words: &[&str]) -> Option<&'static str> {
    words.iter().rev().find_map(|word| {
        let command = word.rsplit('/').next().unwrap_or(word);
        match command {
            "sudo" => Some("sudo"),
            "doas" => Some("doas"),
            "env" => Some("env"),
            "time" => Some("time"),
            "xargs" => Some("xargs"),
            _ => None,
        }
    })
}

fn wrapper_flag_value_slot(wrapper: &str, flag: &str) -> Option<SlotKind> {
    match (wrapper, flag) {
        ("sudo", "-D" | "--chdir" | "-R" | "--chroot") | ("env", "-C" | "--chdir") => {
            Some(SlotKind::Directory)
        }
        ("doas", "-C") | ("time", "-o" | "--output") | ("xargs", "-a" | "--arg-file") => {
            Some(SlotKind::Path)
        }
        _ => None,
    }
}

fn wrapper_attached_short_flags(wrapper: &str) -> &'static [&'static str] {
    match wrapper {
        "sudo" => &["-D", "-R"],
        "doas" | "env" => &["-C"],
        "time" => &["-o"],
        "xargs" => &["-a"],
        _ => &[],
    }
}

#[derive(Clone, Copy)]
struct AttachedFlagValue<'a> {
    kind: SlotKind,
    flag: &'a str,
    value: &'a str,
}

fn flags_ended_before(words: &[&str], argument_position: usize) -> bool {
    words
        .get(1..=argument_position)
        .is_some_and(|words| words.contains(&"--"))
}

fn explicit_path(prefix: &str) -> bool {
    prefix.starts_with(['.', '~', '/']) || (prefix.contains('/') && !non_local_path_prefix(prefix))
}

fn response_file_prefix<'a>(command: &str, prefix: &'a str) -> Option<&'a str> {
    let supported = super::is_c_family_compiler(command)
        || matches!(
            command,
            "javac"
                | "rustc"
                | "rustdoc"
                | "swiftc"
                | "kotlinc"
                | "kotlinc-jvm"
                | "kotlinc-js"
                | "kotlinc-native"
                | "konanc"
        );
    supported.then(|| prefix.strip_prefix('@')).flatten()
}

fn hybrid_explicit_path_slot<'a>(
    command: &str,
    previous: &str,
    prefix: &'a str,
) -> Option<FilesystemSlot<'a>> {
    let flags: &[&str] = match command {
        "cargo" => &["--target", "--config"],
        "rustc" | "rustdoc" => &["--target"],
        "conan" => &[
            "-pr",
            "--profile",
            "-pr:h",
            "--profile:host",
            "-pr:b",
            "--profile:build",
        ],
        _ => return None,
    };
    if flags.contains(&previous) && explicit_path(prefix) {
        return Some(FilesystemSlot::plain(SlotKind::Path, prefix));
    }
    for flag in flags {
        let attached = format!("{flag}=");
        if let Some(value) = prefix.strip_prefix(&attached)
            && explicit_path(value)
        {
            return Some(FilesystemSlot {
                kind: SlotKind::Path,
                prefix: value,
                edit_prefix: &prefix[..attached.len()],
                base_dir: None,
            });
        }
    }
    None
}

fn structured_path_value_slot<'a>(
    command: &str,
    previous: &str,
    prefix: &'a str,
) -> Option<FilesystemSlot<'a>> {
    let classpath_flags: &[&str] = match command {
        "java" | "javac" => &[
            "-cp",
            "-classpath",
            "--class-path",
            "-p",
            "--module-path",
            "-sourcepath",
            "--source-path",
            "-processorpath",
            "--processor-path",
            "--processor-module-path",
            "--module-source-path",
            "--upgrade-module-path",
        ],
        "kotlin" | "kotlinc" | "kotlinc-jvm" | "kotlinc-js" => &["-cp", "-classpath"],
        _ => &[],
    };
    if classpath_flags.contains(&previous)
        && let Some(index) = prefix.rfind(':')
    {
        return Some(FilesystemSlot {
            kind: SlotKind::Path,
            prefix: &prefix[index + 1..],
            edit_prefix: &prefix[..=index],
            base_dir: None,
        });
    }
    for flag in classpath_flags {
        let attached = format!("{flag}=");
        if let Some(value) = prefix.strip_prefix(&attached)
            && let Some(index) = value.rfind(':')
        {
            let path_start = attached.len() + index + 1;
            return Some(FilesystemSlot {
                kind: SlotKind::Path,
                prefix: &prefix[path_start..],
                edit_prefix: &prefix[..path_start],
                base_dir: None,
            });
        }
    }

    if matches!(command, "rustc" | "rustdoc") {
        let value_start = if previous == "-L" {
            0
        } else if prefix.starts_with("-L") {
            2
        } else {
            return None;
        };
        let value = &prefix[value_start..];
        if let Some(index) = value.find('=')
            && matches!(
                &value[..index],
                "all" | "dependency" | "crate" | "native" | "framework"
            )
        {
            let path_start = value_start + index + 1;
            return Some(FilesystemSlot {
                kind: SlotKind::Directory,
                prefix: &prefix[path_start..],
                edit_prefix: &prefix[..path_start],
                base_dir: None,
            });
        }
    }
    None
}

fn explicit_git_path(prefix: &str) -> bool {
    prefix.starts_with("./")
        || prefix.starts_with("../")
        || prefix.starts_with("~/")
        || prefix.starts_with('/')
}

fn remote_path_prefix(prefix: &str) -> bool {
    prefix
        .split_once(':')
        .is_some_and(|(host, _)| !host.is_empty() && !host.contains('/'))
}

fn non_local_path_prefix(prefix: &str) -> bool {
    remote_path_prefix(prefix)
        || prefix.split_once(':').is_some_and(|(scheme, _)| {
            !scheme.is_empty()
                && scheme.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
                })
        })
}

fn sudo_edit_mode(words: &[&str], argument_position: usize) -> bool {
    words.get(1..=argument_position).is_some_and(|arguments| {
        arguments
            .iter()
            .any(|word| matches!(*word, "-e" | "--edit"))
    })
}

fn curl_data_file_flag(flag: &str) -> bool {
    matches!(flag, "-d" | "--data" | "--data-binary")
}

fn attached_flag_value<'a>(command: &str, word: &'a str) -> Option<AttachedFlagValue<'a>> {
    if let Some((flag, value)) = word.split_once('=') {
        let kind = flag_value_slot(command, flag)?;
        let split = flag.len() + 1;
        return Some(AttachedFlagValue {
            kind,
            flag: &word[..split],
            value,
        });
    }
    for flag in attached_short_flags(command) {
        if word.len() > flag.len() && word.starts_with(flag) {
            return Some(AttachedFlagValue {
                kind: flag_value_slot(command, flag)?,
                flag: &word[..flag.len()],
                value: &word[flag.len()..],
            });
        }
    }
    None
}

fn attached_short_flags(command: &str) -> &'static [&'static str] {
    if super::is_c_family_compiler(command) {
        return &[
            "-isystem",
            "-iquote",
            "-idirafter",
            "-include-pch",
            "-include",
            "-imacros",
            "-I",
            "-L",
            "-F",
            "-B",
            "-D",
            "-U",
            "-o",
            "-x",
        ];
    }
    match command {
        "cp" | "mv" => &["-t", "-S"],
        "git" | "make" => &["-C"],
        "pnpm" => &["-C", "-F"],
        "npm" => &["-w"],
        "just" => &["-d", "-f"],
        "curl" => &["-o", "-K", "-T", "-d"],
        "cargo" => &["-C", "-F", "-p", "-j"],
        "rustc" => &["-L", "-l", "-o", "-A", "-W", "-D", "-F", "-C"],
        "rustdoc" => &["-L", "-o", "-A", "-W", "-D", "-F"],
        "cmake" => &["-S", "-B", "-C", "-P", "-G", "-D", "-U"],
        "ninja" => &["-C", "-f", "-j", "-k", "-l"],
        "meson" => &["-C"],
        "gradle" | "gradlew" => &["-b", "-c", "-g", "-I", "-p"],
        "mvn" | "mvnw" | "mvnDebug" => &["-f", "-s", "-t"],
        "swift" | "swiftc" => &["-I", "-L", "-F", "-D", "-o", "-j"],
        "python" => &["-c", "-m", "-W", "-X"],
        "ruby" => &["-e", "-r", "-E", "-C", "-I"],
        "perl" => &["-e", "-E", "-M", "-m", "-F", "-I"],
        "node" => &["-e", "-p", "-r"],
        "uv" => &["-m", "-s", "-p", "-w", "-i", "-f", "-P", "-C"],
        "poetry" => &["-C", "-P"],
        "bundle" | "bundler" => &["-j", "-r"],
        "grep" | "egrep" | "fgrep" => &["-e", "-f", "-A", "-B", "-C", "-m"],
        "rg" => &["-e", "-f", "-A", "-B", "-C", "-m", "-g", "-t", "-T", "-j"],
        "ag" => &["-A", "-B", "-C", "-G", "-g", "-m", "-W"],
        "sed" => &["-e", "-f"],
        "awk" => &["-v", "-F", "-f"],
        "jq" => &["-f"],
        "ssh" | "sftp" | "mosh" | "scp" => super::ssh::attached_short_value_flags(command),
        _ => &[],
    }
}

fn command_directory_before_active(
    context: &CompletionContext,
    command: &str,
    words: &[&str],
    argument_position: usize,
) -> std::path::PathBuf {
    if command == "git" {
        let before = words
            .get(..=argument_position)
            .unwrap_or_else(|| words.get(..1).unwrap_or_default());
        return super::git::git_working_directory(context, before);
    }
    if super::is_package_manager(command) {
        return super::manager_project_dir(context)
            .unwrap_or_else(|| super::invocation_working_directory(context));
    }
    if let Some(directory) = super::delegated_command_working_directory(context) {
        return directory;
    }

    let mut directory = super::invocation_working_directory(context);
    if !matches!(command, "make" | "just") {
        return directory;
    }
    let before = words.get(1..=argument_position).unwrap_or_default();
    let mut options = true;
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if options && word == "--" {
            options = false;
            index += 1;
            continue;
        }
        if !options {
            index += 1;
            continue;
        }
        if let Some(attached) = attached_flag_value(command, word) {
            if attached.kind == SlotKind::Directory {
                directory = super::resolve_directory(&directory, attached.value);
            }
            index += 1;
            continue;
        }
        if let Some(kind) = flag_value_slot(command, word) {
            let Some(value) = before.get(index + 1).copied() else {
                break;
            };
            if kind == SlotKind::Directory {
                directory = super::resolve_directory(&directory, value);
            }
            index += 2;
            continue;
        }
        index += 1;
    }
    directory
}

/// Conservative default path surface. Unknown commands and literal-oriented
/// CLIs stay quiet unless the user types explicit path syntax; common file
/// tools still provide normal basename completion.
fn default_path_slot(command: &str, words: &[&str], argument_position: usize) -> Option<SlotKind> {
    match command {
        "mkdir" => Some(SlotKind::NewFile),
        "rmdir" => Some(SlotKind::Directory),
        "ls" | "cat" | "bat" | "less" | "more" | "head" | "tail" | "wc" | "sort" | "uniq"
        | "file" | "stat" | "du" | "cp" | "mv" | "rm" | "touch" | "ln" | "readlink"
        | "realpath" | "dirname" | "basename" | "open" | "xdg-open" | "vi" | "vim" | "nvim"
        | "nano" | "emacs" | "code" | "diff" | "cmp" | "tee" | "zip" | "rsync" | "scp" | "gzip"
        | "gunzip" | "bzip2" | "bunzip2" | "xz" | "unxz" | "sha1sum" | "sha256sum" | "md5"
        | "md5sum" | "javac" | "swiftc" | "kotlinc" | "kotlinc-jvm" | "kotlinc-js"
        | "kotlinc-native" | "konanc" | "rustfmt" | "gofmt" | "goimports" | "clang-format"
        | "clang-tidy" | "swift-format" | "swiftformat" | "ktlint" => Some(SlotKind::Path),
        command if super::is_c_family_compiler(command) => Some(SlotKind::Path),
        "rustc" | "rustdoc" if rust_input_path_slot(command, words, argument_position) => {
            Some(SlotKind::Path)
        }
        "kotlin"
            if !flag_before_active(command, words, argument_position, &["-e", "-expression"])
                && positional_arguments_before(command, words, argument_position, &[])
                    .is_some_and(|count| count == 0) =>
        {
            Some(SlotKind::Path)
        }
        "swift"
            if positional_arguments_before(command, words, argument_position, &[])
                .is_some_and(|count| count == 0) =>
        {
            Some(SlotKind::Path)
        }
        "cargo" if cargo_positional_path_slot(words, argument_position) => Some(SlotKind::NewFile),
        "go" if go_source_path_slot(words, argument_position) => Some(SlotKind::Path),
        "go" if go_work_directory_slot(words, argument_position) => Some(SlotKind::Directory),
        "go" if go_version_path_slot(words, argument_position) => Some(SlotKind::Path),
        "cmake" if cmake_source_directory_slot(words, argument_position) => {
            Some(SlotKind::Directory)
        }
        "rustup" if rustup_toolchain_link_path_slot(words, argument_position) => {
            Some(SlotKind::Directory)
        }
        "meson" if meson_directory_slot(words, argument_position) => Some(SlotKind::Directory),
        "source" | "." | "unzip"
            if positional_arguments_before(command, words, argument_position, &[])
                .is_some_and(|count| count == 0) =>
        {
            Some(SlotKind::Path)
        }
        "python"
            if !flag_before_active(command, words, argument_position, &["-c", "-m"])
                && positional_arguments_before(command, words, argument_position, &[])
                    .is_some_and(|count| count == 0) =>
        {
            Some(SlotKind::Path)
        }
        "ruby" | "perl" | "node"
            if !flag_before_active(
                command,
                words,
                argument_position,
                &["-e", "--eval", "-p", "--print"],
            ) && positional_arguments_before(command, words, argument_position, &[])
                .is_some_and(|count| count == 0) =>
        {
            Some(SlotKind::Path)
        }
        "find" if find_start_path_slot(words, argument_position) => Some(SlotKind::Path),
        "grep" | "egrep" | "fgrep" | "rg" | "ag" | "sed" | "awk" | "jq"
            if search_input_path_slot(command, words, argument_position) =>
        {
            Some(SlotKind::Path)
        }
        "chmod" | "chown" | "chgrp"
            if positional_arguments_before(command, words, argument_position, &[])
                .is_some_and(|count| count >= 1) =>
        {
            Some(SlotKind::Path)
        }
        "git" if git_path_slot(words, argument_position) => Some(SlotKind::Path),
        _ => None,
    }
}

/// A selected file is not always a complete command. `cp` and `mv` require a
/// source plus a destination (or one source with `--target-directory`). Mark
/// the first source as a continuation so Enter fills it back into the shell
/// instead of submitting a command that is known to be missing an operand.
fn required_file_followup_slot(
    context: &CompletionContext,
    slot_kind: SlotKind,
) -> Option<SlotKind> {
    if !matches!(slot_kind, SlotKind::File | SlotKind::Path) {
        return None;
    }
    let command = super::executable_basename(context.command()?);
    if !matches!(command, "cp" | "mv") {
        return None;
    }
    let (words, argument_position) = argument_progress(context)?;
    let completed = positional_arguments_before(command, &words, argument_position, &[])?;
    let required = if target_directory_before_active(command, &words, argument_position) {
        1
    } else {
        2
    };
    (completed.saturating_add(1) < required).then_some(SlotKind::Path)
}

fn target_directory_before_active(command: &str, words: &[&str], argument_position: usize) -> bool {
    let before = words.get(1..=argument_position).unwrap_or_default();
    let mut options = true;
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if options && word == "--" {
            options = false;
            index += 1;
            continue;
        }
        if !options {
            index += 1;
            continue;
        }
        if let Some(attached) = attached_flag_value(command, word) {
            if matches!(
                attached.flag.trim_end_matches('='),
                "-t" | "--target-directory"
            ) && !attached.value.is_empty()
            {
                return true;
            }
            index += 1;
            continue;
        }
        if flag_value_slot(command, word).is_some() {
            let has_value = index + 1 < before.len();
            if matches!(word, "-t" | "--target-directory") && has_value {
                return true;
            }
            index += if has_value { 2 } else { 1 };
            continue;
        }
        index += 1;
    }
    false
}

fn cargo_positional_path_slot(words: &[&str], argument_position: usize) -> bool {
    positional_words_before("cargo", words, argument_position).is_some_and(|positionals| {
        matches!(positionals.as_slice(), ["new"] | ["init"] | ["vendor"])
    })
}

fn go_work_directory_slot(words: &[&str], argument_position: usize) -> bool {
    positional_words_before("go", words, argument_position).is_some_and(|positionals| {
        matches!(
            positionals.as_slice(),
            ["work", "use", ..] | ["work", "init", ..]
        )
    })
}

fn go_version_path_slot(words: &[&str], argument_position: usize) -> bool {
    positional_words_before("go", words, argument_position)
        .is_some_and(|positionals| matches!(positionals.as_slice(), ["version", ..]))
}

fn cmake_source_directory_slot(words: &[&str], argument_position: usize) -> bool {
    !flag_before_active(
        "cmake",
        words,
        argument_position,
        &[
            "--preset",
            "--build",
            "--install",
            "--open",
            "-P",
            "-E",
            "--workflow",
        ],
    ) && positional_arguments_before("cmake", words, argument_position, &[])
        .is_some_and(|count| count == 0)
}

fn rustup_toolchain_link_path_slot(words: &[&str], argument_position: usize) -> bool {
    positional_words_before("rustup", words, argument_position)
        .is_some_and(|positionals| matches!(positionals.as_slice(), ["toolchain", "link", _]))
}

fn meson_directory_slot(words: &[&str], argument_position: usize) -> bool {
    positional_words_before("meson", words, argument_position).is_some_and(|positionals| {
        matches!(
            positionals.as_slice(),
            ["setup"] | ["setup", _] | ["configure"]
        )
    })
}

fn go_source_path_slot(words: &[&str], argument_position: usize) -> bool {
    positional_words_before("go", words, argument_position).is_some_and(|positionals| {
        match positionals.as_slice() {
            ["run"] | ["build"] => true,
            ["build", inputs @ ..] => inputs.iter().all(|input| input.ends_with(".go")),
            _ => false,
        }
    })
}

fn rust_input_path_slot(command: &str, words: &[&str], argument_position: usize) -> bool {
    positional_words_before(command, words, argument_position).is_some_and(|positionals| {
        positionals
            .iter()
            .filter(|word| !word.starts_with('+'))
            .count()
            == 0
    })
}

fn find_start_path_slot(words: &[&str], argument_position: usize) -> bool {
    let before = words.get(1..=argument_position).unwrap_or_default();
    let mut index = 0;
    let mut options = true;
    while let Some(word) = before.get(index).copied() {
        if options && word == "--" {
            options = false;
            index += 1;
            continue;
        }
        if options && matches!(word, "-H" | "-L" | "-P") {
            index += 1;
            continue;
        }
        if options && word == "-D" {
            if index + 1 >= before.len() {
                return false;
            }
            index += 2;
            continue;
        }
        if options && word.starts_with("-O") && word.len() > 2 {
            index += 1;
            continue;
        }
        if word.starts_with('-') || matches!(word, "!" | "(" | ")" | ",") {
            return false;
        }
        index += 1;
    }
    true
}

fn positional_arguments_before(
    command: &str,
    words: &[&str],
    argument_position: usize,
    value_flags: &[&str],
) -> Option<usize> {
    let before = words.get(1..=argument_position).unwrap_or_default();
    let mut positional = 0;
    let mut options = true;
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if options && word == "--" {
            options = false;
            index += 1;
        } else if options && attached_flag_value(command, word).is_some() {
            index += 1;
        } else if options
            && (value_flags.contains(&word) || flag_value_slot(command, word).is_some())
        {
            if index + 1 >= before.len() {
                return None;
            }
            index += 2;
        } else if options && word.starts_with('-') && word != "-" {
            index += 1;
        } else {
            positional += 1;
            index += 1;
        }
    }
    Some(positional)
}

fn positional_words_before<'a>(
    command: &str,
    words: &'a [&'a str],
    argument_position: usize,
) -> Option<Vec<&'a str>> {
    let before = words.get(1..=argument_position).unwrap_or_default();
    let mut positionals = Vec::new();
    let mut options = true;
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if options && word == "--" {
            options = false;
            index += 1;
        } else if options && attached_flag_value(command, word).is_some() {
            index += 1;
        } else if options && flag_value_slot(command, word).is_some() {
            if index + 1 >= before.len() {
                return None;
            }
            index += 2;
        } else if options && word.starts_with('-') && word != "-" {
            index += 1;
        } else {
            positionals.push(word);
            index += 1;
        }
    }
    Some(positionals)
}

fn flag_before_active(
    command: &str,
    words: &[&str],
    argument_position: usize,
    flags: &[&str],
) -> bool {
    let before = words.get(1..=argument_position).unwrap_or_default();
    for word in before {
        if *word == "--" {
            return false;
        }
        if flags.contains(word) {
            return true;
        }
        if let Some(attached) = attached_flag_value(command, word)
            && flags.contains(&attached.flag.trim_end_matches('='))
        {
            return true;
        }
    }
    false
}

fn search_input_path_slot(command: &str, words: &[&str], argument_position: usize) -> bool {
    let before = words.get(1..=argument_position).unwrap_or_default();
    let mut expression_supplied = false;
    let mut positionals = 0;
    let mut options = true;
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if options && word == "--" {
            options = false;
            index += 1;
            continue;
        }
        if options {
            if let Some(attached) = attached_flag_value(command, word) {
                expression_supplied |=
                    search_expression_flag(command, attached.flag.trim_end_matches('='));
                index += 1;
                continue;
            }
            if flag_value_slot(command, word).is_some() {
                if index + 1 >= before.len() {
                    return false;
                }
                expression_supplied |= search_expression_flag(command, word);
                index += 2;
                continue;
            }
            if word.starts_with('-') {
                index += 1;
                continue;
            }
        }
        positionals += 1;
        index += 1;
    }
    expression_supplied || positionals >= 1
}

fn search_expression_flag(command: &str, flag: &str) -> bool {
    match command {
        "grep" | "egrep" | "fgrep" | "rg" => {
            matches!(flag, "-e" | "--regexp" | "-f" | "--file")
        }
        "sed" => matches!(flag, "-e" | "--expression" | "-f" | "--file"),
        "awk" => flag == "-f",
        "jq" => matches!(flag, "-f" | "--from-file"),
        _ => false,
    }
}

fn git_path_slot(words: &[&str], argument_position: usize) -> bool {
    let Some((subcommand_index, subcommand)) = super::git::git_subcommand(words) else {
        return false;
    };
    matches!(
        subcommand,
        "add" | "rm" | "mv" | "restore" | "clean" | "status"
    ) || super::git::path_slot_after_ref(words, argument_position)
        || words
            .get(subcommand_index + 1..=argument_position)
            .is_some_and(|words| words.contains(&"--"))
}

/// `git checkout <…>`-style slots take refs, not files — the git provider
/// owns them (`git add <path>` keeps file completion). After `--` the slot is
/// a pathspec and file rows resume; after `checkout -b` / `switch -c` the
/// slot is a new branch name and stays suppressed.
fn at_git_ref_slot(words: &[&str], argument_position: usize) -> bool {
    super::git::ref_slot_subcommand(words, argument_position).is_some()
        || super::git::new_branch_slot(words, argument_position)
}

fn package_manager_path_slot(
    context: &CompletionContext,
    command: &str,
    words: &[&str],
    argument_position: usize,
    prefix: &str,
) -> Option<SlotKind> {
    let command = super::executable_basename(command);
    if non_local_path_prefix(prefix) {
        return None;
    }
    let explicit_path = explicit_path(prefix);
    if argument_position == 0 {
        return explicit_path.then_some(SlotKind::Path);
    }
    let subcommand = super::manager_command(context)
        .or_else(|| words.get(1).copied())
        .unwrap_or_default();
    let file_command = match command {
        "deno" => matches!(subcommand, "run" | "test" | "fmt" | "lint" | "compile"),
        "bun" => {
            matches!(subcommand, "build" | "test") || (subcommand == "run" && !prefix.is_empty())
        }
        _ => false,
    };
    (file_command || explicit_path).then_some(SlotKind::Path)
}

#[derive(Clone, Copy)]
enum TarValue {
    Archive,
    Directory,
    File,
    Path,
    NewFile,
    Value,
}

fn tar_slot<'a>(
    context: &'a CompletionContext,
    words: &[&str],
    argument_position: usize,
) -> Option<FilesystemSlot<'a>> {
    let before = words.get(1..=argument_position).unwrap_or_default();
    let mut mode = None;
    let mut expected = VecDeque::new();
    let mut options = true;
    let mut base_dir = super::invocation_working_directory(context);
    for (index, word) in before.iter().copied().enumerate() {
        if let Some(value) = expected.pop_front() {
            if matches!(value, TarValue::Directory) {
                base_dir = super::resolve_directory(&base_dir, word);
            }
            continue;
        }
        if options && word == "--" {
            options = false;
            continue;
        }
        if !options {
            continue;
        }
        if let Some((flag, value)) = word.split_once('=') {
            match flag {
                "--create" => mode = Some('c'),
                "--extract" | "--get" => mode = Some('x'),
                "--list" => mode = Some('t'),
                _ if matches!(tar_long_value(flag), Some(TarValue::Directory)) => {
                    base_dir = super::resolve_directory(&base_dir, value);
                }
                _ => {}
            }
            continue;
        }
        match word {
            "--create" => mode = Some('c'),
            "--extract" | "--get" => mode = Some('x'),
            "--list" => mode = Some('t'),
            _ if word.starts_with("--") => {
                if let Some(value) = tar_long_value(word) {
                    expected.push_back(value);
                }
            }
            _ if index == 0 && tar_old_style_options(word) => {
                for option in word.chars() {
                    match option {
                        'c' | 'x' | 't' => mode = Some(option),
                        _ => {
                            if let Some(value) = tar_short_value(option) {
                                expected.push_back(value);
                            }
                        }
                    }
                }
            }
            _ if word.starts_with('-') && word != "-" => {
                let options = word.strip_prefix('-').unwrap_or_default();
                for (offset, option) in options.char_indices() {
                    match option {
                        'c' | 'x' | 't' => mode = Some(option),
                        _ => {
                            let Some(value) = tar_short_value(option) else {
                                continue;
                            };
                            let value_start = offset + option.len_utf8();
                            let attached = &options[value_start..];
                            if attached.is_empty() {
                                expected.push_back(value);
                            } else if matches!(value, TarValue::Directory) {
                                base_dir = super::resolve_directory(&base_dir, attached);
                            }
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let prefix = context.parsed.current_prefix.as_str();
    if let Some(value) = expected.front().copied() {
        return tar_value_slot(value, mode, prefix, "", base_dir);
    }

    if let Some((flag, value_prefix)) = prefix.split_once('=')
        && let Some(value) = tar_long_value(flag)
    {
        return tar_value_slot(
            value,
            mode,
            value_prefix,
            &prefix[..flag.len() + 1],
            base_dir,
        );
    }

    if let Some(short_options) = prefix.strip_prefix('-')
        && !short_options.starts_with('-')
    {
        let mut active_mode = mode;
        for (offset, option) in short_options.char_indices() {
            match option {
                'c' | 'x' | 't' => active_mode = Some(option),
                _ => {
                    let Some(value) = tar_short_value(option) else {
                        continue;
                    };
                    let value_start = offset + option.len_utf8();
                    let value_prefix = &short_options[value_start..];
                    if value_prefix.is_empty() {
                        return None;
                    }
                    return tar_value_slot(
                        value,
                        active_mode,
                        value_prefix,
                        &prefix[..value_start + 1],
                        base_dir,
                    );
                }
            }
        }
        return None;
    }

    if argument_position == 0 && tar_old_style_options(prefix) {
        return None;
    }

    (mode == Some('c')).then_some(FilesystemSlot {
        kind: SlotKind::Path,
        prefix,
        edit_prefix: "",
        base_dir: Some(base_dir),
    })
}

fn tar_value_slot<'a>(
    value: TarValue,
    mode: Option<char>,
    prefix: &'a str,
    edit_prefix: &'a str,
    base_dir: std::path::PathBuf,
) -> Option<FilesystemSlot<'a>> {
    if matches!(value, TarValue::Archive | TarValue::File)
        && (prefix == "-" || non_local_path_prefix(prefix))
    {
        return None;
    }
    let kind = match value {
        TarValue::Archive => match mode {
            Some('c') => SlotKind::NewFile,
            Some('x' | 't') => SlotKind::File,
            _ => SlotKind::Path,
        },
        TarValue::Directory => SlotKind::Directory,
        TarValue::File => SlotKind::File,
        TarValue::Path => SlotKind::Path,
        TarValue::NewFile => SlotKind::NewFile,
        TarValue::Value => return None,
    };
    Some(FilesystemSlot {
        kind,
        prefix,
        edit_prefix,
        base_dir: Some(base_dir),
    })
}

fn tar_long_value(flag: &str) -> Option<TarValue> {
    match flag {
        "--file" => Some(TarValue::Archive),
        "--directory" => Some(TarValue::Directory),
        "--files-from" | "--exclude-from" => Some(TarValue::File),
        "--add-file" | "--listed-incremental" | "--info-script" | "--new-volume-script" => {
            Some(TarValue::Path)
        }
        "--index-file" => Some(TarValue::NewFile),
        "--format"
        | "--newer"
        | "--after-date"
        | "--newer-mtime"
        | "--exclude"
        | "--exclude-tag"
        | "--exclude-tag-all"
        | "--exclude-tag-under"
        | "--strip-components"
        | "--transform"
        | "--xform"
        | "--checkpoint-action"
        | "--mtime"
        | "--owner"
        | "--group"
        | "--mode"
        | "--sort"
        | "--warning"
        | "--quoting-style"
        | "--suffix"
        | "--blocking-factor"
        | "--record-size"
        | "--tape-length"
        | "--starting-file"
        | "--rmt-command"
        | "--rsh-command"
        | "--use-compress-program" => Some(TarValue::Value),
        _ => None,
    }
}

fn tar_short_value(option: char) -> Option<TarValue> {
    match option {
        'f' => Some(TarValue::Archive),
        'C' => Some(TarValue::Directory),
        'T' | 'X' => Some(TarValue::File),
        'F' | 'g' => Some(TarValue::Path),
        'b' | 'H' | 'I' | 'K' | 'L' | 'N' | 's' => Some(TarValue::Value),
        _ => None,
    }
}

fn tar_old_style_options(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|character| character.is_ascii_alphabetic())
        && word
            .chars()
            .any(|character| matches!(character, 'c' | 'x' | 't'))
}

fn language_tool_flag_value_slot(command: &str, flag: &str) -> Option<SlotKind> {
    if super::is_c_family_compiler(command) {
        return c_compiler_flag_value_slot(flag);
    }
    match command {
        "cargo" => cargo_flag_value_slot(flag),
        "rustc" => rustc_flag_value_slot(flag),
        "rustdoc" => rustdoc_flag_value_slot(flag),
        "go" => go_flag_value_slot(flag),
        "cmake" => cmake_flag_value_slot(flag),
        "ctest" => ctest_flag_value_slot(flag),
        "cpack" => cpack_flag_value_slot(flag),
        "ninja" => ninja_flag_value_slot(flag),
        "meson" => meson_flag_value_slot(flag),
        "conan" => conan_flag_value_slot(flag),
        "vcpkg" => vcpkg_flag_value_slot(flag),
        "gradle" | "gradlew" => gradle_flag_value_slot(flag),
        "mvn" | "mvnw" | "mvnDebug" => maven_flag_value_slot(flag),
        "java" | "javac" => java_flag_value_slot(command, flag),
        "kotlinc" | "kotlinc-jvm" | "kotlinc-js" | "kotlinc-native" | "konanc" | "kotlin" => {
            kotlin_flag_value_slot(flag)
        }
        "swift" | "swiftc" => swift_flag_value_slot(flag),
        "xcodebuild" => xcodebuild_flag_value_slot(flag),
        "rustfmt" => rustfmt_flag_value_slot(flag),
        "gofmt" | "goimports" => go_formatter_flag_value_slot(command, flag),
        "clang-format" | "clang-tidy" => clang_tool_flag_value_slot(command, flag),
        "swift-format" | "swiftformat" => swift_formatter_flag_value_slot(command, flag),
        "ktlint" => ktlint_flag_value_slot(flag),
        _ => None,
    }
}

fn c_compiler_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-I" | "-L" | "-F" | "-Fsystem" | "-B" | "-isystem" | "-iquote" | "-idirafter"
        | "-iframework" | "-cxx-isystem" | "--sysroot" | "-isysroot" | "--gcc-toolchain"
        | "--cuda-path" | "--rocm-path" | "-resource-dir" => Some(SlotKind::Directory),
        "-include" | "-imacros" | "-include-pch" | "--config" | "-fmodule-map-file"
        | "-ivfsoverlay" => Some(SlotKind::Path),
        "-o" | "-MF" | "-MJ" | "-dependency-file" | "-serialize-diagnostics" => {
            Some(SlotKind::NewFile)
        }
        "-D" | "-U" | "-x" | "-std" | "--std" | "-stdlib" | "-target" | "--target" | "-arch"
        | "-Xclang" | "-Xlinker" | "-mllvm" | "-fuse-ld" | "-rtlib" | "-unwindlib" | "-march"
        | "-mcpu" | "-mtune" | "--analyzer-output" => Some(SlotKind::Value),
        _ => None,
    }
}

fn cargo_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-C" | "--target-dir" | "--artifact-dir" | "--path" | "--root" => Some(SlotKind::Directory),
        "--manifest-path" | "--lockfile-path" => Some(SlotKind::Path),
        "-p" | "--package" | "--exclude" | "-F" | "--features" | "--bin" | "--example"
        | "--test" | "--bench" | "-j" | "--jobs" | "--target" | "--profile" | "--color"
        | "--message-format" | "--config" | "-Z" => Some(SlotKind::Value),
        _ => None,
    }
}

fn rustc_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-L" | "--out-dir" | "--sysroot" => Some(SlotKind::Directory),
        "-o" => Some(SlotKind::NewFile),
        "--cfg" | "--check-cfg" | "-l" | "--crate-type" | "--crate-name" | "--edition"
        | "--emit" | "--print" | "--explain" | "--target" | "-A" | "--allow" | "-W" | "--warn"
        | "-D" | "--deny" | "-F" | "--forbid" | "--force-warn" | "--cap-lints" | "-C"
        | "--codegen" | "--error-format" | "--json" => Some(SlotKind::Value),
        _ => None,
    }
}

fn rustdoc_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-L" | "-o" | "--output" | "--test-run-directory" => Some(SlotKind::Directory),
        "--extend-css"
        | "--markdown-css"
        | "--html-in-header"
        | "--html-before-content"
        | "--html-after-content" => Some(SlotKind::Path),
        "--cfg"
        | "--check-cfg"
        | "--crate-type"
        | "--crate-name"
        | "--edition"
        | "--emit"
        | "--target"
        | "--extern"
        | "-A"
        | "--allow"
        | "-W"
        | "--warn"
        | "-D"
        | "--deny"
        | "-F"
        | "--forbid"
        | "--cap-lints"
        | "--error-format"
        | "--json"
        | "--extern-html-root-url" => Some(SlotKind::Value),
        _ => None,
    }
}

fn go_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-C" | "-pkgdir" | "-outputdir" => Some(SlotKind::Directory),
        "-modfile" | "-overlay" | "-pgo" | "-vettool" => Some(SlotKind::Path),
        "-o" | "-coverprofile" | "-cpuprofile" | "-memprofile" | "-mutexprofile" | "-trace" => {
            Some(SlotKind::NewFile)
        }
        "-p"
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
        | "-tags"
        | "-toolexec"
        | "-cpu"
        | "-list"
        | "-run"
        | "-shuffle"
        | "-timeout"
        | "-count"
        | "-parallel"
        | "-benchtime"
        | "-blockprofilerate"
        | "-memprofilerate"
        | "-mutexprofilefraction" => Some(SlotKind::Value),
        _ => None,
    }
}

fn cmake_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-S" | "--source" | "-B" | "--build" | "--install" | "--open" | "--install-prefix" => {
            Some(SlotKind::Directory)
        }
        "-C" | "-P" | "--toolchain" | "--project-file" | "--trace-source" => Some(SlotKind::Path),
        "--graphviz" | "--trace-redirect" => Some(SlotKind::NewFile),
        "-D"
        | "-U"
        | "-G"
        | "-A"
        | "-T"
        | "--preset"
        | "--target"
        | "--config"
        | "--parallel"
        | "-j"
        | "--resolve-package-references" => Some(SlotKind::Value),
        _ => None,
    }
}

fn ctest_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "--test-dir" => Some(SlotKind::Directory),
        "--resource-spec-file" => Some(SlotKind::Path),
        "--output-log" | "--output-junit" => Some(SlotKind::NewFile),
        "--preset" | "-C" | "--build-config" | "-R" | "-E" | "-L" | "-LE" | "-I" | "-j"
        | "--parallel" | "--repeat" | "--timeout" => Some(SlotKind::Value),
        _ => None,
    }
}

fn cpack_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "--config" => Some(SlotKind::Path),
        "-B" | "--package-directory" => Some(SlotKind::Directory),
        "--preset" | "-G" | "-C" | "-D" | "-P" | "-R" | "-V" => Some(SlotKind::Value),
        _ => None,
    }
}

fn ninja_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-C" => Some(SlotKind::Directory),
        "-f" => Some(SlotKind::Path),
        "-j" | "-k" | "-l" | "-d" | "-w" | "-t" => Some(SlotKind::Value),
        _ => None,
    }
}

fn meson_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-C" | "--wd" | "--prefix" | "--bindir" | "--datadir" | "--includedir" | "--infodir"
        | "--libdir" | "--libexecdir" | "--localedir" | "--localstatedir" | "--mandir"
        | "--sbindir" | "--sharedstatedir" | "--sysconfdir" => Some(SlotKind::Directory),
        "--cross-file" | "--native-file" => Some(SlotKind::Path),
        "-D"
        | "--backend"
        | "--buildtype"
        | "--default-library"
        | "--layout"
        | "--optimization"
        | "--unity"
        | "--warnlevel"
        | "--wrap-mode"
        | "--jobs"
        | "--num-processes"
        | "--maxfail"
        | "--suite"
        | "--no-suite"
        | "--setup"
        | "--wrapper"
        | "--logbase"
        | "--test-args"
        | "--ninja-args"
        | "--vs-args"
        | "--xcode-args"
        | "--force-fallback-for"
        | "--pkg-config-path"
        | "--cmake-prefix-path" => Some(SlotKind::Value),
        _ => None,
    }
}

fn conan_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-of" | "--output-folder" | "--deployer-folder" => Some(SlotKind::Directory),
        "--lockfile" => Some(SlotKind::Path),
        "--lockfile-out" => Some(SlotKind::NewFile),
        "-pr" | "--profile" | "-pr:h" | "--profile:host" | "-pr:b" | "--profile:build" | "-s"
        | "--settings" | "-o" | "--options" | "-c" | "--conf" | "-b" | "--build" | "-r"
        | "--remote" | "--deployer" | "--format" | "--core-conf" => Some(SlotKind::Value),
        _ => None,
    }
}

fn vcpkg_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "--vcpkg-root"
        | "--x-install-root"
        | "--downloads-root"
        | "--x-buildtrees-root"
        | "--x-packages-root"
        | "--overlay-ports"
        | "--overlay-triplets"
        | "--x-manifest-root" => Some(SlotKind::Directory),
        "--triplet" | "--host-triplet" | "--x-feature" => Some(SlotKind::Value),
        _ => None,
    }
}

fn gradle_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-g" | "--gradle-user-home" | "-p" | "--project-dir" | "--project-cache-dir" => {
            Some(SlotKind::Directory)
        }
        "-b" | "--build-file" | "-c" | "--settings-file" | "-I" | "--init-script" => {
            Some(SlotKind::Path)
        }
        "-D"
        | "--system-prop"
        | "-P"
        | "--project-prop"
        | "--max-workers"
        | "--priority"
        | "--console"
        | "--warning-mode"
        | "--dependency-verification" => Some(SlotKind::Value),
        _ => None,
    }
}

fn maven_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-f" | "--file" | "-s" | "--settings" | "-gs" | "--global-settings" | "-t"
        | "--toolchains" => Some(SlotKind::Path),
        "-D"
        | "-P"
        | "--activate-profiles"
        | "-pl"
        | "--projects"
        | "-rf"
        | "--resume-from"
        | "-T"
        | "--threads"
        | "--builder" => Some(SlotKind::Value),
        _ => None,
    }
}

fn java_flag_value_slot(command: &str, flag: &str) -> Option<SlotKind> {
    match (command, flag) {
        ("java", "-jar") => Some(SlotKind::Path),
        ("java" | "javac", "-cp" | "-classpath" | "--class-path" | "-p" | "--module-path") => {
            Some(SlotKind::Path)
        }
        (
            "javac",
            "-sourcepath"
            | "--source-path"
            | "-processorpath"
            | "--processor-path"
            | "--processor-module-path"
            | "--module-source-path"
            | "--upgrade-module-path",
        ) => Some(SlotKind::Path),
        ("javac", "-d" | "-s" | "-h") => Some(SlotKind::Directory),
        (
            "java" | "javac",
            "--add-modules" | "--limit-modules" | "--add-exports" | "--add-opens" | "--add-reads"
            | "--patch-module",
        ) => Some(SlotKind::Value),
        ("javac", "-encoding" | "-source" | "--source" | "-target" | "--target" | "--release") => {
            Some(SlotKind::Value)
        }
        _ => None,
    }
}

fn kotlin_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-jdk-home" | "-kotlin-home" | "-repo" => Some(SlotKind::Directory),
        "-classpath" | "-cp" | "-d" | "-Xplugin" | "-Xfriend-paths" | "-library" => {
            Some(SlotKind::Path)
        }
        "-module-name" | "-language-version" | "-api-version" | "-jvm-target" | "-P"
        | "-script-templates" | "-Xjdk-release" | "-howtorun" | "-e" | "-expression"
        | "-target" | "-produce" | "-entry" => Some(SlotKind::Value),
        _ => None,
    }
}

fn swift_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-I" | "-L" | "-F" | "-Fsystem" | "-Isystem" | "-module-cache-path" | "-sdk"
        | "-sysroot" | "-working-directory" | "-plugin-path" | "--package-path"
        | "--cache-path" | "--config-path" | "--security-path" | "--scratch-path"
        | "--swift-sdks-path" | "--pkg-config-path" => Some(SlotKind::Directory),
        "-import-bridging-header"
        | "-vfsoverlay"
        | "-load-plugin-library"
        | "--toolset"
        | "--netrc-file" => Some(SlotKind::Path),
        "-o"
        | "-emit-module-path"
        | "-serialize-diagnostics-path"
        | "-index-store-path"
        | "-emit-dependencies-path"
        | "-emit-reference-dependencies-path" => Some(SlotKind::NewFile),
        "-D" | "-module-name" | "-package-name" | "-target" | "-swift-version" | "-sanitize"
        | "-j" | "--jobs" | "--configuration" | "--target" | "--product" | "--traits"
        | "--triple" | "--swift-sdk" | "--toolchain" | "--build-system" | "-Xcc" | "-Xswiftc"
        | "-Xlinker" | "-Xcxx" => Some(SlotKind::Value),
        _ => None,
    }
}

fn xcodebuild_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "-project"
        | "-workspace"
        | "-xcconfig"
        | "-resultStreamPath"
        | "-exportOptionsPlist"
        | "-importPath"
        | "-localizationPath"
        | "-xctestrun"
        | "-testProductsPath"
        | "-authenticationKeyPath" => Some(SlotKind::Path),
        "-resultBundlePath"
        | "-clonedSourcePackagesDirPath"
        | "-derivedDataPath"
        | "-archivePath"
        | "-exportPath"
        | "-packageCachePath" => Some(SlotKind::Directory),
        "-projectName"
        | "-target"
        | "-scheme"
        | "-configuration"
        | "-arch"
        | "-sdk"
        | "-toolchain"
        | "-destination"
        | "-destination-timeout"
        | "-jobs"
        | "-parallel-testing-enabled"
        | "-parallel-testing-worker-count"
        | "-testPlan"
        | "-only-testing"
        | "-skip-testing"
        | "-testLanguage"
        | "-testRegion" => Some(SlotKind::Value),
        _ => None,
    }
}

fn rustfmt_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "--config-path" | "--out-dir" => Some(SlotKind::Directory),
        "--edition" | "--style-edition" | "--emit" | "--file-lines" | "--config" => {
            Some(SlotKind::Value)
        }
        _ => None,
    }
}

fn go_formatter_flag_value_slot(command: &str, flag: &str) -> Option<SlotKind> {
    match (command, flag) {
        ("gofmt" | "goimports", "-cpuprofile") => Some(SlotKind::NewFile),
        ("goimports", "-srcdir") => Some(SlotKind::Directory),
        ("gofmt", "-r") | ("goimports", "-local") => Some(SlotKind::Value),
        _ => None,
    }
}

fn clang_tool_flag_value_slot(command: &str, flag: &str) -> Option<SlotKind> {
    match (command, flag) {
        ("clang-format", "-assume-filename" | "--assume-filename") => Some(SlotKind::Path),
        (
            "clang-format",
            "-style"
            | "--style"
            | "-fallback-style"
            | "--fallback-style"
            | "-cursor"
            | "--cursor"
            | "-offset"
            | "--offset"
            | "-length"
            | "--length"
            | "-lines"
            | "--lines"
            | "-qualifier-alignment"
            | "--qualifier-alignment",
        ) => Some(SlotKind::Value),
        ("clang-tidy", "-p" | "--p") => Some(SlotKind::Directory),
        ("clang-tidy", "-config-file" | "--config-file") => Some(SlotKind::Path),
        ("clang-tidy", "-export-fixes" | "--export-fixes") => Some(SlotKind::NewFile),
        (
            "clang-tidy",
            "-checks"
            | "--checks"
            | "-config"
            | "--config"
            | "-header-filter"
            | "--header-filter"
            | "-exclude-header-filter"
            | "--exclude-header-filter"
            | "-line-filter"
            | "--line-filter"
            | "-extra-arg"
            | "--extra-arg"
            | "-extra-arg-before"
            | "--extra-arg-before"
            | "-warnings-as-errors"
            | "--warnings-as-errors"
            | "-format-style",
        ) => Some(SlotKind::Value),
        _ => None,
    }
}

fn swift_formatter_flag_value_slot(command: &str, flag: &str) -> Option<SlotKind> {
    match (command, flag) {
        ("swift-format", "--configuration" | "--assume-filename")
        | ("swiftformat", "--config" | "--baseconfig") => Some(SlotKind::Path),
        ("swiftformat", "--output") => Some(SlotKind::NewFile),
        ("swift-format", "--selection")
        | ("swiftformat", "--cache" | "--exclude" | "--rules" | "--disable" | "--enable") => {
            Some(SlotKind::Value)
        }
        _ => None,
    }
}

fn ktlint_flag_value_slot(flag: &str) -> Option<SlotKind> {
    match flag {
        "--editorconfig" | "--baseline" | "--ruleset" => Some(SlotKind::Path),
        "--reporter" | "--disabled_rules" => Some(SlotKind::Value),
        _ => None,
    }
}

/// Well-known flags whose value is literal text (`Value` — no filesystem
/// rows) versus a path (`Path`/`Directory`). Best-effort heuristics for the
/// common commands; unknown flags fall through to the caller's default.
fn flag_value_slot(command: &str, flag: &str) -> Option<SlotKind> {
    if command == "clang-cl" && flag.starts_with('/') {
        return match flag {
            "/I" => Some(SlotKind::Directory),
            "/FI" => Some(SlotKind::Path),
            "/Fo" | "/Fe" | "/Fd" | "/Fa" | "/Fp" => Some(SlotKind::NewFile),
            "/D" | "/U" | "/std" | "/arch" => Some(SlotKind::Value),
            _ => None,
        };
    }
    if !flag.starts_with('-') {
        return None;
    }
    if let Some(slot) = super::ssh::flag_value_slot(command, flag) {
        return Some(slot);
    }
    if let Some(slot) = language_tool_flag_value_slot(command, flag) {
        return Some(slot);
    }
    match (command, flag) {
        ("cp" | "mv", "-t" | "--target-directory") => Some(SlotKind::Directory),
        ("cp" | "mv", "-S" | "--suffix") => Some(SlotKind::Value),
        ("cp", "--no-preserve" | "--sparse") => Some(SlotKind::Value),
        ("git", "-C" | "--git-dir" | "--work-tree") => Some(SlotKind::Directory),
        ("git", "-F" | "--file") => Some(SlotKind::Path),
        (
            "git",
            "-m" | "--message" | "-c" | "--author" | "--date" | "--format" | "--pretty" | "--grep",
        ) => Some(SlotKind::Value),
        (
            "curl",
            "-o" | "--output" | "-K" | "--config" | "--cacert" | "--cert" | "--key" | "-T"
            | "--upload-file",
        ) => Some(SlotKind::Path),
        (
            "curl",
            "-d" | "--data" | "--data-binary" | "--data-raw" | "-H" | "--header" | "-X"
            | "--request" | "-u" | "--user" | "-A" | "--user-agent" | "-e" | "--referer" | "-x"
            | "--proxy" | "--connect-timeout" | "--max-time",
        ) => Some(SlotKind::Value),
        ("cargo", "--manifest-path") => Some(SlotKind::Path),
        (
            "cargo",
            "-p" | "--package" | "--features" | "-j" | "--jobs" | "--target" | "--profile",
        ) => Some(SlotKind::Value),
        ("make", "-f" | "--file") => Some(SlotKind::Path),
        ("make", "-C" | "--directory") => Some(SlotKind::Directory),
        ("make", "-j" | "--jobs") => Some(SlotKind::Value),
        ("just", "-f" | "--justfile" | "--dotenv-path") => Some(SlotKind::Path),
        ("just", "-d" | "--working-directory") => Some(SlotKind::Directory),
        ("uv", "-m" | "--module" | "--python" | "-p") => Some(SlotKind::Value),
        (
            "uv",
            "-s"
            | "--script"
            | "--gui-script"
            | "--env-file"
            | "--with-requirements"
            | "-f"
            | "--find-links"
            | "--config-file",
        ) => Some(SlotKind::Path),
        ("uv", "--directory" | "--project" | "--cache-dir")
        | ("poetry", "-C" | "--directory" | "-P" | "--project") => Some(SlotKind::Directory),
        ("pipenv", "--python" | "--categories" | "--extra-pip-args") => Some(SlotKind::Value),
        ("bundle" | "bundler", "--gemfile") => Some(SlotKind::Path),
        ("bundle" | "bundler", "-j" | "--jobs" | "-r" | "--retry") => Some(SlotKind::Value),
        ("bash" | "zsh" | "sh", "-c" | "-O" | "-o")
        | ("python", "-c" | "-m" | "-W" | "-X" | "--check-hash-based-pycs")
        | ("ruby", "-e" | "-r" | "-E")
        | ("perl", "-e" | "-E" | "-M" | "-m" | "-F")
        | ("node", "-e" | "--eval" | "-p" | "--print" | "--import" | "--loader" | "--conditions")
        | (
            "grep" | "egrep" | "fgrep" | "rg" | "ag",
            "-e" | "--regexp" | "-A" | "--after-context" | "-B" | "--before-context" | "-C"
            | "--context" | "-m" | "--max-count",
        )
        | (
            "rg",
            "-g" | "--glob" | "-t" | "--type" | "-T" | "--type-not" | "-j" | "--threads"
            | "--max-depth" | "--encoding" | "--engine" | "--sort" | "--sortr",
        )
        | ("ag", "-G" | "--file-search-regex" | "-g" | "--filename-pattern" | "-W" | "--width")
        | ("find", "-name" | "-iname" | "-path" | "-type" | "-user" | "-group")
        | ("sed", "-e" | "--expression")
        | ("awk", "-v" | "-F")
        | ("chown", "--from") => Some(SlotKind::Value),
        ("python", "--pycache-prefix") | ("ruby", "-C" | "-I") | ("perl", "-I") => {
            Some(SlotKind::Directory)
        }
        ("java", "-jar" | "-cp" | "-classpath" | "--class-path" | "--module-path")
        | ("javac", "-cp" | "-classpath" | "--class-path" | "--module-path") => {
            Some(SlotKind::Path)
        }
        ("javac", "-d") | ("rustc", "--out-dir") => Some(SlotKind::Directory),
        ("rustc", "-o") => Some(SlotKind::NewFile),
        ("bash", "--rcfile" | "--init-file") => Some(SlotKind::Path),
        ("grep" | "egrep" | "fgrep" | "rg", "-f" | "--file")
        | ("grep" | "egrep" | "fgrep", "--exclude-from")
        | ("sed", "-f" | "--file")
        | ("awk", "-f")
        | ("jq", "-f" | "--from-file")
        | ("node", "-r" | "--require")
        | ("chmod" | "chown" | "chgrp", "--reference")
        | ("find", "-newer" | "-anewer" | "-cnewer") => Some(SlotKind::Path),
        (
            "pnpm",
            "-C"
            | "--dir"
            | "--store-dir"
            | "--global-dir"
            | "--global-bin-dir"
            | "--state-dir"
            | "--cache-dir"
            | "--virtual-store-dir"
            | "--lockfile-dir"
            | "--config-dir",
        )
        | ("npm", "--prefix" | "--cache")
        | ("yarn", "--cwd" | "--cache-folder" | "--modules-folder")
        | ("bun", "--cwd" | "--outdir") => Some(SlotKind::Directory),
        ("npm", "--userconfig")
        | ("yarn", "--use-yarnrc")
        | ("bun", "--config" | "--preload")
        | ("deno", "--config" | "--cert" | "--import-map" | "--lock" | "--env-file") => {
            Some(SlotKind::Path)
        }
        ("pnpm", "-F" | "--filter" | "--reporter" | "--workspace-concurrency")
        | ("npm", "-w" | "--workspace" | "--location" | "--registry" | "--otp")
        | ("yarn", "--mutex")
        | ("bun", "--backend" | "--target" | "--format" | "--timeout" | "--rerun-each")
        | (
            "deno",
            "--location" | "--v8-flags" | "--seed" | "--inspect" | "--inspect-brk"
            | "--inspect-wait" | "--node-modules-dir",
        ) => Some(SlotKind::Value),
        // The attached form `make -j4` carries its own value: the slot after
        // it is a target, not a path.
        ("make", flag) if is_attached_jobs_flag(flag) => Some(SlotKind::Value),
        _ => None,
    }
}

/// `make -j<digits>`: the jobs flag with its value attached.
fn is_attached_jobs_flag(flag: &str) -> bool {
    flag.len() > 2
        && flag.starts_with("-j")
        && flag[2..]
            .chars()
            .all(|character| character.is_ascii_digit())
}

pub(super) fn split_prefix(prefix: &str) -> (&str, &str) {
    prefix
        .rfind('/')
        .map_or(("", prefix), |index| prefix.split_at(index + 1))
}

/// Keep a known shell expansion marker outside quotes. Quoting the entire
/// value (`'~/Projects'`, `'$HOME/Projects'`) turns the marker into a literal;
/// quoting only the candidate-controlled remainder is safe and expandable.
pub(super) fn escape_path_for_shell(value: &str, shell: crate::shell::ShellKind) -> String {
    for marker in ["~/", "$HOME/", "${HOME}/", "$PWD/", "${PWD}/"] {
        if let Some(rest) = value.strip_prefix(marker) {
            return format!(
                "{marker}{}",
                escape_for_shell(rest, crate::parser::QuoteContext::Unquoted, shell)
            );
        }
    }
    escape_for_shell(value, crate::parser::QuoteContext::Unquoted, shell)
}

/// The directory to scan for a typed path prefix. Known home/PWD expansions
/// are resolved for scanning while their literal spelling is kept in edits.
pub(super) fn scan_directory_for(
    cwd: &std::path::Path,
    directory_prefix: &str,
) -> std::path::PathBuf {
    if directory_prefix.is_empty() {
        return cwd.to_owned();
    }
    if let Some(rest) = directory_prefix.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    if let Some(rest) = directory_prefix
        .strip_prefix("$HOME/")
        .or_else(|| directory_prefix.strip_prefix("${HOME}/"))
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    if let Some(rest) = directory_prefix
        .strip_prefix("$PWD/")
        .or_else(|| directory_prefix.strip_prefix("${PWD}/"))
    {
        return cwd.join(rest);
    }
    cwd.join(directory_prefix)
}

fn slot_accepts(slot: SlotKind, path: &std::path::Path, directory: bool) -> bool {
    match slot {
        SlotKind::Directory => directory,
        SlotKind::Executable => directory || crate::platform::is_executable(path),
        SlotKind::File => true,
        SlotKind::Path => true,
        SlotKind::NewFile => directory,
        SlotKind::Process | SlotKind::Interface | SlotKind::Port | SlotKind::Value => false,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        os::unix::fs::{PermissionsExt, symlink},
        path::PathBuf,
        time::Duration,
    };

    use super::*;
    use crate::{
        completion::{BufferSnapshot, CompletionEngine, SyncQuality},
        providers::command_help::{CommandHelp, HelpEntry},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };

    #[test]
    fn bash_prefers_scripts_and_escapes_spaces() {
        let directory = tempfile::tempdir().expect("directory");
        fs::File::create(directory.path().join("hello world.sh"))
            .expect("script")
            .write_all(b"echo ok\n")
            .expect("write script");
        fs::write(directory.path().join("script"), b"echo plain\n").expect("non-executable script");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        let backslash_directory = directory.path().join(r"dir\name");
        fs::create_dir(&backslash_directory).expect("backslash directory");
        fs::write(backslash_directory.join("run.sh"), b"echo quoted\n").expect("nested script");
        let context = CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new("bash ", 5, BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context");
        let mut engine = CompletionEngine::new(100, 20);
        engine.register(provider(Arc::new(SpecRegistry::default())));
        let output = engine.complete(&context);
        assert!(output.candidates.iter().any(|candidate| {
            candidate.display.primary == "hello world.sh"
                && candidate
                    .edit
                    .as_ref()
                    .is_some_and(|edit| edit.replacement == "'hello world.sh'")
        }));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "nested/")
        );
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "script"),
            "shell script files do not need an executable bit or extension"
        );

        let quoted = CompletionContext::new(
            QueryId::new(2),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new(
                "bash 'hello",
                11,
                BufferRevision::new(2),
                SyncQuality::Exact,
            )
            .expect("quoted buffer"),
        )
        .expect("quoted context");
        let output = engine.complete(&quoted);
        let candidate = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "hello world.sh")
            .expect("quoted script candidate");
        assert_eq!(candidate.edit.as_ref().expect("edit").range, 5..11);
        assert_eq!(
            candidate.edit.as_ref().expect("edit").replacement,
            "'hello world.sh'"
        );

        let quoted_backslash = CompletionContext::new(
            QueryId::new(3),
            ShellKind::Zsh,
            PathBuf::from(directory.path()),
            BufferSnapshot::new(
                r"bash 'dir\name/ru",
                r"bash 'dir\name/ru".len(),
                BufferRevision::new(3),
                SyncQuality::Exact,
            )
            .expect("quoted backslash buffer"),
        )
        .expect("quoted backslash context");
        let output = engine.complete(&quoted_backslash);
        let candidate = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == r"dir\name/run.sh")
            .expect("backslash path candidate");
        assert_eq!(
            candidate.edit.as_ref().expect("edit").replacement,
            r"'dir\name/run.sh'"
        );
    }

    #[test]
    fn explicit_command_paths_complete_executables_and_preserve_wrappers() {
        let directory = tempfile::tempdir().expect("directory");
        let executable = directory.path().join("runner");
        fs::write(&executable, b"#!/bin/sh\n").expect("executable");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("executable mode");
        fs::write(directory.path().join("regular.txt"), b"plain").expect("regular file");
        fs::write(directory.path().join("not-executable.sh"), b"#!/bin/sh\n")
            .expect("non-executable script");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "./ru",
            "sudo ./ru",
            "which ./ru",
            "command -v ./ru",
            "npx ./ru",
            "npx -w app ./ru",
            "npm exec -- ./ru",
            "npm exec --workspace app -- ./ru",
            "pnpm exec ./ru",
            "yarn exec ./ru",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            let runner = output
                .candidates
                .iter()
                .find(|candidate| candidate.display.primary == "./runner")
                .unwrap_or_else(|| panic!("explicit executable missing for {buffer:?}"));
            assert_eq!(runner.edit.as_ref().expect("edit").replacement, "./runner");
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "./regular.txt")
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "./not-executable.sh")
            );
        }
    }

    #[test]
    fn tar_slots_distinguish_new_and_existing_archives() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("archive.tgz"), b"archive").expect("archive");
        fs::create_dir(directory.path().join("src")).expect("source directory");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["tar -czf ", "tar -c -f ", "tar cf "] {
            let create_output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                create_output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "src/"),
                "new archive directory missing for {buffer:?}"
            );
            assert!(
                !create_output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "archive.tgz"),
                "existing archive leaked into new-file slot for {buffer:?}"
            );
        }

        for buffer in ["tar -xzf ", "tar --extract --file "] {
            let extract_output = provider.complete(&context(directory.path(), buffer, 2));
            assert!(
                extract_output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "archive.tgz"),
                "input archive missing for {buffer:?}"
            );
        }

        let sources = provider.complete(&context(directory.path(), "tar -czf out.tgz sr", 3));
        assert!(
            sources
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "src/")
        );
        assert!(
            provider
                .complete(&context(directory.path(), "tar -xzf archive.tgz member", 4))
                .candidates
                .is_empty()
        );
    }

    #[test]
    fn tar_slots_cover_attached_values_chdir_chains_and_literal_values() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("archive.tgz"), b"archive").expect("archive");
        fs::write(directory.path().join("list.txt"), b"list").expect("list");
        fs::write(directory.path().join("exclude.txt"), b"exclude").expect("exclude");
        fs::create_dir(directory.path().join("archives")).expect("archives");
        fs::create_dir_all(directory.path().join("base/nested/source")).expect("nested source");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["tar -C ba", "tar -Cba", "tar --directory=ba"] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "base/"),
                "directory value missing for {buffer:?}"
            );
        }

        let chained = provider.complete(&context(
            directory.path(),
            "tar -czf out.tgz -C base -C nested so",
            2,
        ));
        assert!(
            chained
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "source/"),
            "consecutive -C values must resolve successively"
        );

        for buffer in ["tar cvzf ", "tar -c --file=ar"] {
            let output = provider.complete(&context(directory.path(), buffer, 3));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "archives/"),
                "new archive slot missing for {buffer:?}"
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "archive.tgz"),
                "existing archive leaked for {buffer:?}"
            );
        }

        let attached = provider.complete(&context(directory.path(), "tar -x -farc", 4));
        let archive = attached
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "archive.tgz")
            .expect("attached archive input");
        assert_eq!(
            archive.edit.as_ref().expect("edit").replacement,
            "-farchive.tgz"
        );

        for (buffer, expected) in [("tar -T li", "list.txt"), ("tar -X ex", "exclude.txt")] {
            let output = provider.complete(&context(directory.path(), buffer, 5));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == expected),
                "file-list value missing for {buffer:?}"
            );
        }

        for buffer in ["tar -c --owner ", "tar -c --format ", "tar -c -H "] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 6))
                    .candidates
                    .is_empty(),
                "literal tar value leaked filesystem rows for {buffer:?}"
            );
        }
        for buffer in ["tar -xf -", "tar -T -", "tar --file=host:archive"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 7))
                    .candidates
                    .is_empty(),
                "stdio or remote tar values must stay out of local filesystem rows: {buffer:?}"
            );
        }
    }

    #[test]
    fn five_thousand_entry_directory_returns_within_the_first_batch_budget() {
        let directory = tempfile::tempdir().expect("directory");
        for index in 0..5_000 {
            fs::write(directory.path().join(format!("entry-{index:04}")), b"")
                .expect("directory fixture");
        }
        let context = context(directory.path(), "cat ", 3);
        let provider = provider(Arc::new(SpecRegistry::default()));
        let mut samples = Vec::new();
        for _ in 0..10 {
            let started = Instant::now();
            let output = provider.complete(&context);
            samples.push(started.elapsed());
            assert!(!output.candidates.is_empty());
        }
        samples.sort_unstable();
        let p95 = samples[9];
        let budget = if cfg!(debug_assertions) {
            Duration::from_millis(120)
        } else {
            Duration::from_millis(85)
        };
        assert!(p95 <= budget, "5000-entry directory p95 was {p95:?}");
    }

    #[test]
    fn dash_prefixed_directories_get_a_dot_slash_prefix_like_files() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir(directory.path().join("-dashdir")).expect("dash directory");
        fs::write(directory.path().join("-dashfile"), b"").expect("dash file");
        let provider = provider(Arc::new(SpecRegistry::default()));
        let output = provider.complete(&context(directory.path(), "cd ", 1));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "./-dashdir/")
        );
        let output = provider.complete(&context(directory.path(), "cat ", 2));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "./-dashfile")
        );
    }

    #[test]
    fn dashed_active_word_produces_no_filesystem_candidates() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("-dash"), b"").expect("dash file");
        let provider = provider(Arc::new(SpecRegistry::default()));
        let output = provider.complete(&context(directory.path(), "cat -", 1));
        assert!(output.candidates.is_empty());
        assert!(!provider.applies(&context(directory.path(), "cat -d", 2)));
    }

    #[test]
    fn spec_covered_commands_own_the_fallback_path_slot() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"").expect("file");
        let provider = provider(Arc::new(SpecRegistry::load(None)));
        // `ls` is spec-covered: the fallback path slot is suppressed.
        assert!(
            provider
                .complete(&context(directory.path(), "ls ", 1))
                .candidates
                .is_empty()
        );
        // `cat` is not: file completion still works.
        assert!(
            provider
                .complete(&context(directory.path(), "cat ", 2))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );
    }

    #[test]
    fn help_subcommands_suppress_files_at_the_real_top_level_command_slot() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"").expect("file");
        fs::write(directory.path().join("plain.swift"), b"").expect("swift file");
        let help = Arc::new(CommandHelpCache::default());
        help.seed(
            "git",
            CommandHelp {
                flags: vec![HelpEntry {
                    name: "-C".into(),
                    description: String::new(),
                    takes_value: true,
                }],
                subcommands: vec![HelpEntry {
                    name: "add".into(),
                    description: String::new(),
                    takes_value: false,
                }],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: false,
            },
        );
        let provider = FilesystemProvider::new(
            false,
            Arc::new(SpecRegistry::load(None)),
            Arc::clone(&help),
            Arc::new(AliasCache::default()),
        );
        // Subcommand position with a positive help result: no file rows.
        assert!(
            provider
                .complete(&context(directory.path(), "git ", 1))
                .candidates
                .is_empty()
        );
        assert!(
            provider
                .complete(&context(directory.path(), "git -C . ", 2))
                .candidates
                .is_empty(),
            "global flag values must not shift the top-level subcommand slot"
        );
        // Past the first argument file completion still works.
        assert!(
            provider
                .complete(&context(directory.path(), "git -C . add ", 3))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );
        help.seed(
            "swift",
            CommandHelp {
                flags: Vec::new(),
                subcommands: vec![HelpEntry {
                    name: "build".into(),
                    description: String::new(),
                    takes_value: false,
                }],
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: true,
            },
        );
        assert!(
            provider
                .complete(&context(directory.path(), "swift ", 30))
                .candidates
                .is_empty(),
            "SwiftPM subcommands own the empty slot"
        );
        assert!(
            provider
                .complete(&context(directory.path(), "swift b", 31))
                .candidates
                .is_empty(),
            "a matching SwiftPM subcommand prefix must suppress files"
        );
        assert!(
            provider
                .complete(&context(directory.path(), "swift pl", 32))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.swift"),
            "a non-subcommand Swift prefix must fall back to source files"
        );
        // Help without subcommands (e.g. `cp`) never suppresses files.
        help.seed(
            "cp",
            CommandHelp {
                flags: vec![HelpEntry {
                    name: "-R".into(),
                    description: String::new(),
                    takes_value: false,
                }],
                subcommands: Vec::new(),
                subcommand_aliases: Vec::new(),
                accepts_positionals: false,
                subcommands_exhaustive: false,
            },
        );
        assert!(
            provider
                .complete(&context(directory.path(), "cp ", 4))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );
    }

    #[test]
    fn cp_and_mv_first_file_candidates_require_a_destination() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("source.txt"), b"source").expect("source file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "cp ",
            "cp -R ",
            "cp -S .backup ",
            "cp -S -tdir ",
            "cp --no-preserve -tdir ",
            "cp --sparse -tdir ",
            "mv ",
            "mv --suffix=-tdir ",
            "sudo mv ",
        ] {
            let candidate = provider
                .complete(&context(directory.path(), buffer, 1))
                .candidates
                .into_iter()
                .find(|candidate| candidate.display.primary == "source.txt")
                .unwrap_or_else(|| panic!("source candidate missing for {buffer:?}"));
            assert_eq!(
                candidate.action,
                CandidateAction::InsertAndContinue {
                    next_slot: SlotKind::Path
                },
                "first operand must fill without executing for {buffer:?}"
            );
            assert_eq!(
                candidate.completeness,
                Completeness::NeedsInput {
                    slot: SlotKind::Path
                },
                "first operand must still require a destination for {buffer:?}"
            );
        }
    }

    #[test]
    fn cp_and_mv_destination_file_candidates_are_runnable() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("source.txt"), b"source").expect("source file");
        fs::write(directory.path().join("target.txt"), b"target").expect("target file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "cp source.txt ",
            "cp source.txt -R ",
            "cp -- -tdir ",
            "mv source.txt ",
            "sudo mv source.txt ",
        ] {
            let candidate = provider
                .complete(&context(directory.path(), buffer, 2))
                .candidates
                .into_iter()
                .find(|candidate| candidate.display.primary == "target.txt")
                .unwrap_or_else(|| panic!("destination candidate missing for {buffer:?}"));
            assert_eq!(
                candidate.action,
                CandidateAction::Insert,
                "destination file should complete the invocation for {buffer:?}"
            );
            assert_eq!(
                candidate.completeness,
                Completeness::Runnable,
                "destination file should be runnable for {buffer:?}"
            );
        }
    }

    #[test]
    fn cp_and_mv_target_directory_forms_need_only_a_source() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("source.txt"), b"source").expect("source file");
        fs::create_dir(directory.path().join("dest")).expect("destination directory");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "cp -t dest ",
            "cp --target-directory=dest -S .backup ",
            "mv -tdest ",
            "mv --target-directory dest ",
        ] {
            let candidate = provider
                .complete(&context(directory.path(), buffer, 3))
                .candidates
                .into_iter()
                .find(|candidate| candidate.display.primary == "source.txt")
                .unwrap_or_else(|| panic!("source candidate missing for {buffer:?}"));
            assert_eq!(
                candidate.action,
                CandidateAction::Insert,
                "target-directory form is complete after its first source for {buffer:?}"
            );
            assert_eq!(
                candidate.completeness,
                Completeness::Runnable,
                "target-directory form should be runnable for {buffer:?}"
            );
        }
    }

    #[test]
    fn cp_and_mv_non_path_option_values_do_not_offer_filesystem_rows() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("source.txt"), b"source").expect("source file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "cp -S ",
            "cp -S.backup",
            "cp --suffix ",
            "cp --no-preserve ",
            "cp --sparse=",
            "mv -S ",
            "mv --suffix=-backup",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 4))
                    .candidates
                    .is_empty(),
                "non-path option value leaked filesystem rows for {buffer:?}"
            );
        }
    }

    #[test]
    fn shell_expansion_prefixes_scan_the_resolved_directory() {
        let cwd = tempfile::tempdir().expect("cwd");
        let home = std::env::home_dir().expect("home directory");
        assert_eq!(scan_directory_for(cwd.path(), "~/"), home.clone());
        assert_eq!(
            scan_directory_for(cwd.path(), "~/Documents/"),
            home.join("Documents")
        );
        assert_eq!(scan_directory_for(cwd.path(), "$HOME/"), home.clone());
        assert_eq!(
            scan_directory_for(cwd.path(), "${HOME}/Documents/"),
            home.join("Documents")
        );
        assert_eq!(scan_directory_for(cwd.path(), "$PWD/"), cwd.path());
        assert_eq!(
            scan_directory_for(cwd.path(), "${PWD}/src/"),
            cwd.path().join("src")
        );
        assert_eq!(
            scan_directory_for(cwd.path(), "src/"),
            cwd.path().join("src")
        );
        assert_eq!(scan_directory_for(cwd.path(), ""), cwd.path().to_owned());
    }

    #[test]
    fn path_edits_preserve_known_shell_expansions() {
        for shell in [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish] {
            assert_eq!(
                escape_path_for_shell("~/Projects/app/", shell),
                "~/Projects/app/"
            );
            assert_eq!(
                escape_path_for_shell("~/My Projects/app/", shell),
                "~/'My Projects/app/'"
            );
            assert_eq!(
                escape_path_for_shell("$HOME/My Projects/app/", shell),
                "$HOME/'My Projects/app/'"
            );
            assert_eq!(
                escape_path_for_shell("${PWD}/src/my file.rs", shell),
                "${PWD}/'src/my file.rs'"
            );
        }
    }

    #[test]
    fn pwd_prefixed_candidates_scan_and_fill_without_quoting_the_variable() {
        let cwd = tempfile::tempdir().expect("cwd");
        fs::create_dir(cwd.path().join("nested project")).expect("nested directory");
        let output = provider(Arc::new(SpecRegistry::default())).complete(&context(
            cwd.path(),
            "cd $PWD/n",
            1,
        ));
        let candidate = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "$PWD/nested project/")
            .expect("PWD-prefixed candidate");
        assert_eq!(
            candidate.edit.as_ref().expect("edit").replacement,
            "$PWD/'nested project/'"
        );
    }

    #[test]
    fn broken_symlinks_are_paths_but_not_executables_or_directories() {
        let directory = tempfile::tempdir().expect("directory");
        symlink("missing-target", directory.path().join("missing-link")).expect("broken symlink");
        let provider = provider(Arc::new(SpecRegistry::default()));

        let path_rows = provider
            .complete(&context(directory.path(), "cat m", 1))
            .candidates;
        assert!(
            path_rows
                .iter()
                .any(|candidate| candidate.display.primary == "missing-link")
        );

        for buffer in ["cd m", "./m"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .iter()
                    .all(|candidate| candidate.display.primary != "missing-link"),
                "broken symlink leaked into {buffer:?}"
            );
        }
    }

    fn provider(specs: Arc<SpecRegistry>) -> FilesystemProvider {
        FilesystemProvider::new(
            false,
            specs,
            Arc::new(CommandHelpCache::default()),
            Arc::new(AliasCache::default()),
        )
    }

    #[test]
    fn value_flags_suppress_file_rows_and_path_flags_offer_them() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        fs::write(directory.path().join("main.go"), b"package main\n").expect("go file");
        fs::write(directory.path().join("main.rs"), b"fn main() {}\n").expect("rust file");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        let provider = provider(Arc::new(SpecRegistry::default()));

        // Literal-text slots: no filesystem rows at all.
        for buffer in [
            "git commit -m ",
            "ssh -p ",
            "curl -H ",
            "python -W ",
            "bash -o ",
            "bash -O pl",
            "zsh -o pl",
            "grep -A ",
            "curl --data-binary literal",
            "gcc -D ",
            "clang -std ",
            "rustc --edition ",
            "cargo build --target ",
            "go test -run ",
            "cmake -G ",
            "gradle -P ",
            "mvn -P ",
            "kotlinc -jvm-target ",
            "swiftc -module-name ",
            "xcodebuild -scheme ",
            "rustfmt --edition ",
            "clang-format --style ",
            "goimports -local ",
            "ktlint --reporter ",
        ] {
            let context = context(directory.path(), buffer, 1);
            let output = provider.complete(&context);
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not offer file rows: {:?}",
                output
                    .candidates
                    .iter()
                    .map(|candidate| candidate.display.primary.as_str())
                    .collect::<Vec<_>>()
            );
        }

        // Path slots: files and directories are offered (trailing space and
        // mid-typing alike).
        for buffer in [
            "curl -o ",
            "curl -o pl",
            "sftp -b pl",
            "ssh -E pl",
            "bash --rcfile pl",
            "bash --init-file=pl",
            "clang -include pl",
            "cargo --manifest-path pl",
            "go build -modfile pl",
            "ctest --resource-spec-file pl",
            "cpack --config pl",
            "meson --cross-file pl",
            "conan --lockfile pl",
            "gradle -b pl",
            "mvn -f pl",
            "kotlinc -cp pl",
            "swiftc -import-bridging-header pl",
            "xcodebuild -xcconfig pl",
            "java -jar pl",
            "swift-format --configuration pl",
            "clang-tidy --config-file pl",
            "ktlint --editorconfig pl",
        ] {
            let context = context(directory.path(), buffer, 1);
            let output = provider.complete(&context);
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "{buffer:?} must offer files"
            );
        }

        // Directory-only slots.
        for buffer in [
            "make -C ",
            "just -d ",
            "just -dne",
            "just --working-directory=ne",
            "gcc -I ne",
            "gcc -Ine",
            "clang --sysroot=ne",
            "cmake -S ne",
            "cmake -Bne",
            "cmake ne",
            "ctest --test-dir ne",
            "cpack -B ne",
            "ninja -C ne",
            "meson setup ne",
            "meson -C ne",
            "conan --output-folder ne",
            "vcpkg --overlay-ports ne",
            "cargo install --path ne",
            "go work use ne",
            "rustup toolchain link custom ne",
            "gradle -p ne",
            "swiftc -I ne",
            "clang-cl /I ne",
            "rustfmt --config-path ne",
            "clang-tidy -p ne",
            "goimports -srcdir ne",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "nested/"),
                "directory slot missing for {buffer:?}"
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt")
            );
        }

        for buffer in ["just -f pl", "just --justfile=pl", "just --dotenv-path pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "just path slot missing for {buffer:?}"
            );
        }

        for buffer in [
            "env -C ne",
            "env -Cne",
            "sudo -D ne",
            "sudo -Dne",
            "sudo --chdir=ne",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 2));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "nested/"),
                "wrapper directory missing for {buffer:?}"
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt")
            );
        }
        for buffer in ["sudo -C ", "sudo -C3 ", "sudo --close-from=3 "] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 3))
                    .candidates
                    .is_empty(),
                "sudo close-from is numeric, not a directory slot: {buffer:?}"
            );
        }
        for buffer in ["sudo make -Dne", "time make -opl", "time -o pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 4))
                    .candidates
                    .is_empty(),
                "nested command flags must not be claimed by an outer wrapper: {buffer:?}"
            );
        }
        for buffer in [
            "/usr/bin/time -o pl",
            "command time -o pl",
            "sudo time -o pl",
            "xargs -apl",
            "doas -C pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 3))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "wrapper file missing for {buffer:?}"
            );
        }

        // The flag word itself is still a flag position, not its value.
        let flag_context = context(directory.path(), "curl -", 1);
        let output = provider.complete(&flag_context);
        assert!(output.candidates.is_empty());

        for buffer in [
            "cargo build --target ./pl",
            "rustc --target=./pl",
            "rustdoc --target ./pl",
            "cargo --config=./pl",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 5));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "./plain.txt"),
                "explicit hybrid path missing for {buffer:?}"
            );
        }

        let response = provider.complete(&context(directory.path(), "rustc @pl", 6));
        assert!(response.candidates.iter().any(|candidate| {
            candidate
                .edit
                .as_ref()
                .is_some_and(|edit| edit.replacement == "@plain.txt")
        }));
        for (buffer, replacement) in [
            ("kotlinc -cp nested:pl", "nested:plain.txt"),
            (
                "javac --class-path=nested:pl",
                "--class-path=nested:plain.txt",
            ),
            ("rustc -Ldependency=ne", "-Ldependency=nested/"),
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 6))
                    .candidates
                    .iter()
                    .any(|candidate| candidate
                        .edit
                        .as_ref()
                        .is_some_and(|edit| edit.replacement == replacement)),
                "structured path value missing for {buffer:?}"
            );
        }
        for buffer in [
            "go run main.go pl",
            "go doc . pl",
            "go build ./cmd ",
            "rustc main.rs ",
            "rustc +nightly main.rs ",
            "rustc - ",
            "rustdoc main.rs ",
            "kotlin MainKt ",
            "kotlin -e expression ",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 7))
                    .candidates
                    .is_empty(),
                "literal Go argument position leaked filesystem rows: {buffer:?}"
            );
        }
        for buffer in ["go build main.go pl", "rustc +nightly ma", "rustdoc ma"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 8))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "main.rs"
                        || candidate.display.primary == "plain.txt"),
                "compiler/source input path missing for {buffer:?}"
            );
        }
        for buffer in [
            "gofmt pl",
            "rustfmt pl",
            "swift-format format pl",
            "ktlint pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 8))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "formatter input path missing for {buffer:?}"
            );
        }
    }

    #[test]
    fn versioned_python_and_compiler_path_slots_follow_the_command_family() {
        let directory = tempfile::tempdir().expect("directory");
        for name in ["script.py", "app.jar", "Main.java", "Main.kt", "main.rs"] {
            fs::write(directory.path().join(name), b"").expect("file");
        }
        fs::create_dir(directory.path().join("classes")).expect("classes directory");
        fs::write(directory.path().join("plain.txt"), b"").expect("plain file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for (buffer, expected) in [
            ("python3.14 sc", "script.py"),
            ("pypy3.11 sc", "script.py"),
            ("./bin/python3.14 sc", "script.py"),
            ("java -jar ap", "app.jar"),
            ("javac Ma", "Main.java"),
            ("rustc ma", "main.rs"),
            ("rustdoc +nightly ma", "main.rs"),
            ("gcc-14 ma", "main.rs"),
            ("aarch64-linux-gnu-g++-13 ma", "main.rs"),
            ("kotlin Ma", "Main.java"),
            ("kotlinc-native Ma", "Main.java"),
            ("rustc -o cl", "classes/"),
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 10))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == expected),
                "path slot missing for {buffer:?}"
            );
        }

        assert!(
            provider
                .complete(&context(directory.path(), "javac -d cl", 11))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "classes/")
        );
        for buffer in ["python3.14 -m sc", "pypy3.11 -W ", "./bin/ssh host pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 12))
                    .candidates
                    .is_empty(),
                "literal or remote slot leaked local files for {buffer:?}"
            );
        }
        for buffer in ["rustc main.rs ", "rustdoc main.rs ", "kotlin MainKt "] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 13))
                    .candidates
                    .is_empty(),
                "program argument slot leaked filesystem rows for {buffer:?}"
            );
        }
    }

    #[test]
    fn directory_creation_removal_and_pushd_use_directory_slots() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("plain file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["mkdir ", "rmdir ", "pushd ", "command pushd "] {
            let rows: Vec<_> = provider
                .complete(&context(directory.path(), buffer, 1))
                .candidates
                .into_iter()
                .map(|candidate| candidate.display.primary)
                .collect();
            assert_eq!(rows, ["nested/"], "directory rows for {buffer:?}");
        }

        assert!(
            provider
                .complete(&context(directory.path(), "sudo pushd ", 2))
                .candidates
                .is_empty(),
            "an external wrapper cannot invoke the shell-only pushd builtin"
        );
        assert!(
            provider
                .complete(&context(directory.path(), "pushd +", 3))
                .candidates
                .is_empty(),
            "directory-stack indices are values, not filesystem paths"
        );
    }

    #[test]
    fn shell_builtin_path_slots_respect_the_resolution_domain() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        fs::write(directory.path().join("plain.sh"), b"echo ok\n").expect("source file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["cd ne", "command cd ne", "builtin cd ne", "time cd ne"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 1))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "nested/"),
                "shell cd directory missing for {buffer:?}"
            );
        }
        for buffer in [
            "source pl",
            "command source pl",
            "builtin source pl",
            "time source pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.sh"),
                "shell source file missing for {buffer:?}"
            );
        }
        for buffer in [
            "sudo cd ne",
            "exec cd ne",
            "sudo source pl",
            "exec source pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 3))
                    .candidates
                    .is_empty(),
                "external wrapper borrowed shell-builtin path semantics for {buffer:?}"
            );
        }
    }

    #[test]
    fn completed_flag_values_do_not_shift_later_positional_slots() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["python -W ignore pl", "grep -A 2 pattern pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 1))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "later file slot missing for {buffer:?}"
            );
        }
    }

    #[test]
    fn expression_options_advance_to_input_files_in_all_value_forms() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "grep -e pattern pl",
            "grep -epattern pl",
            "grep --regexp=pattern pl",
            "rg -f patterns.txt pl",
            "rg -fpatterns.txt pl",
            "sed -e s/a/b/ pl",
            "sed -es/a/b/ pl",
            "sed -f rules.sed pl",
            "awk -f program.awk pl",
            "jq --from-file=filter.jq pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 1))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "input-file slot missing for {buffer:?}"
            );
        }

        for buffer in [
            "grep -e pattern",
            "sed -e script",
            "awk -v name=value program",
            "python -mhttp.server pl",
            "node --eval=code pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .is_empty(),
                "literal/program slot leaked files for {buffer:?}"
            );
        }
    }

    #[test]
    fn curl_data_file_syntax_preserves_the_at_prefix() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for (buffer, replacement) in [
            ("curl --data-binary @pl", "@plain.txt"),
            ("curl --data-binary=@pl", "--data-binary=@plain.txt"),
            ("curl -d@pl", "-d@plain.txt"),
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            let file = output
                .candidates
                .iter()
                .find(|candidate| candidate.display.primary == "plain.txt")
                .unwrap_or_else(|| panic!("curl data file missing for {buffer:?}"));
            assert_eq!(file.edit.as_ref().expect("edit").replacement, replacement);
        }
    }

    #[test]
    fn attached_flag_values_and_double_dash_use_the_correct_path_slice() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        let provider = provider(Arc::new(SpecRegistry::default()));

        let output = provider.complete(&context(directory.path(), "curl --output=pl", 1));
        let file = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "plain.txt")
            .expect("attached output path");
        assert_eq!(
            file.edit.as_ref().expect("edit").replacement,
            "--output=plain.txt"
        );

        let output = provider.complete(&context(directory.path(), "make -Cne", 2));
        let nested = output
            .candidates
            .iter()
            .find(|candidate| candidate.display.primary == "nested/")
            .expect("attached directory path");
        assert_eq!(nested.edit.as_ref().expect("edit").replacement, "-Cnested/");

        assert!(
            provider
                .complete(&context(directory.path(), "curl --header=pl", 3))
                .candidates
                .is_empty(),
            "literal attached values must not scan the cwd"
        );
        assert!(
            provider
                .complete(&context(directory.path(), "cat -- pl", 4))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );
    }

    #[test]
    fn unknown_literal_commands_stay_quiet_until_path_intent_is_explicit() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        fs::create_dir(directory.path().join("src")).expect("source directory");
        fs::write(directory.path().join("src/plain.txt"), b"plain").expect("nested file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["claude ", "codex ", "echo ", "curl ", "ssh host "] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 1))
                    .candidates
                    .is_empty(),
                "literal slot leaked cwd files for {buffer:?}"
            );
        }
        assert!(
            provider
                .complete(&context(directory.path(), "claude ./pl", 2))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "./plain.txt")
        );
        assert!(
            provider
                .complete(&context(directory.path(), "claude src/pl", 3))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "src/plain.txt")
        );
        for buffer in ["claude https://example.com/a", "claude host:/remote/a"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 4))
                    .candidates
                    .is_empty(),
                "non-local path syntax must stay quiet for {buffer:?}"
            );
        }
        assert!(
            provider
                .complete(&context(directory.path(), "cat pl", 3))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt")
        );
    }

    #[test]
    fn redirect_targets_are_paths_but_fd_duplication_is_not() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["kill 123 > pl", "pnpm dev 2> pl", "echo hi &>> pl", "> pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 1))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "redirect path missing for {buffer:?}"
            );
        }
        for buffer in [
            "echo hi 2>&1",
            "echo hi 2>&",
            "cat <& pl",
            "cat << EO",
            "cat <<-EO",
            "cat <<< pl",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .is_empty(),
                "fd duplication must not offer files for {buffer:?}"
            );
        }
    }

    #[test]
    fn sudo_edit_operands_are_file_slots_not_executable_slots() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["sudo -e pl", "sudo --edit pl", "sudo -u root -e pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 1))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "sudo edit file missing for {buffer:?}"
            );
        }
    }

    #[test]
    fn command_working_directory_options_change_the_scan_root() {
        let root = tempfile::tempdir().expect("root");
        fs::create_dir(root.path().join("repo")).expect("repo");
        fs::create_dir(root.path().join("app")).expect("app");
        fs::write(root.path().join("repo/inside.txt"), b"git").expect("git file");
        fs::write(root.path().join("app/input.ts"), b"node").expect("node file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        assert!(
            provider
                .complete(&context(root.path(), "git -C repo add in", 1))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "inside.txt")
        );
        assert!(
            provider
                .complete(&context(root.path(), "pnpm -C app dev ./in", 2))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "./input.ts")
        );
    }

    #[test]
    fn ref_host_and_target_slots_suppress_file_rows() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        // Ref-taking git slots, the ssh host slot, and build-tool target
        // slots produce no filesystem rows.
        for buffer in [
            "git checkout ma",
            "git checkout ",
            "git log ",
            "ssh ",
            "ssh de",
            "sftp ",
            "mosh ",
            "make ",
            "make b",
            "just ",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not offer file rows"
            );
        }

        for buffer in ["git diff ./pl", "git checkout ./pl", "git log ./pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "./plain.txt"),
                "explicit git path missing for {buffer:?}"
            );
        }
        assert!(
            provider
                .complete(&context(directory.path(), "git switch -c feature/pl", 3))
                .candidates
                .is_empty(),
            "new branch names containing slashes must not become paths"
        );
        for buffer in ["ssh host ./pl", "sftp host ./pl", "mosh host ./pl"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 4))
                    .candidates
                    .is_empty(),
                "remote operand leaked local paths for {buffer:?}"
            );
        }

        // Path-oriented slots still offer files.
        for buffer in ["git add ", "git add ma", "ssh -i ", "scp "] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "{buffer:?} must offer files"
            );
        }
        assert!(
            provider
                .complete(&context(directory.path(), "scp user@host:/pa", 2))
                .candidates
                .is_empty(),
            "remote scp paths must not scan the local cwd"
        );
    }

    #[test]
    fn package_managers_only_offer_files_at_real_path_slots() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("main.ts"), b"").expect("source file");
        fs::create_dir(directory.path().join("src")).expect("source directory");
        fs::write(directory.path().join("src/main.ts"), b"").expect("nested source file");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "deno run ma",
            "bun build ma",
            "bun build src/ma",
            "pnpm dev ./ma",
            "pnpm dev src/ma",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary.ends_with("main.ts")),
                "{buffer:?} must offer source paths"
            );
        }

        for buffer in ["pnpm ", "pnpm install ", "npm run ", "bun run "] {
            let output = provider.complete(&context(directory.path(), buffer, 2));
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not leak cwd files into command/script selection"
            );
        }

        for buffer in [
            "pnpm add @scope/pkg",
            "pnpm dlx @scope/pkg",
            "npm exec @scope/pkg",
            "npx @scope/pkg",
            "deno run https://deno.land/x/main.ts",
            "deno run jsr:@scope/pkg",
            "rsync host:/remote/ma",
        ] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 3))
                    .candidates
                    .is_empty(),
                "package/URL/remote syntax must not scan the local cwd: {buffer:?}"
            );
        }

        assert!(
            provider
                .complete(&context(directory.path(), "cat src/ma", 4))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "src/main.ts"),
            "known file-taking commands keep relative nested paths"
        );
    }

    #[test]
    fn typed_prefix_on_a_spec_covered_command_offers_files() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("Dockerfile"), b"").expect("file");
        let provider = provider(Arc::new(SpecRegistry::load(None)));
        // `ls` recipes are flag-only: with a typed prefix the spec rows die
        // on match and file rows must appear.
        let output = provider.complete(&context(directory.path(), "ls Do", 1));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "Dockerfile"),
            "`ls Do` must offer files"
        );
        // The empty slot still belongs to the spec.
        assert!(
            provider
                .complete(&context(directory.path(), "ls ", 2))
                .candidates
                .is_empty()
        );
    }

    #[test]
    fn package_manager_slots_offer_no_file_rows() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));
        for buffer in [
            "pnpm ",
            "pnpm bu",
            "npm ",
            "yarn ",
            "bun ",
            "npm run ",
            "pnpm run ",
            "pnpm run bu",
            "deno task ",
            "pnpm --filter ",
            "pnpm --filter @acme/api ",
            "yarn workspace @acme/we",
            "npm --workspace=@acme/we",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not offer file rows"
            );
        }
    }

    #[test]
    fn git_double_dash_resumes_file_rows_and_dash_b_stays_quiet() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        let provider = provider(Arc::new(SpecRegistry::default()));
        // After `--` the slot is a pathspec again: file rows resume.
        for buffer in ["git checkout -- ", "git checkout -- pl"] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "{buffer:?} must offer files"
            );
        }
        // After `-b`/`-c` the slot is a new branch name: no file rows.
        for buffer in ["git checkout -b ", "git checkout -b ne", "git switch -c "] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not offer file rows"
            );
        }

        for buffer in ["git checkout main ", "git reset main ", "git restore "] {
            let output = provider.complete(&context(directory.path(), buffer, 2));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "completed ref must open a path slot for {buffer:?}"
            );
        }
        for buffer in ["git branch new main ", "git branch -m old "] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 3))
                    .candidates
                    .is_empty(),
                "branch completion must stop for {buffer:?}"
            );
        }
    }

    #[test]
    fn wrapper_chdir_and_find_exec_use_the_correct_executable_or_path_root() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir_all(directory.path().join("app/sub")).expect("nested app");
        fs::write(directory.path().join("root.txt"), b"root").expect("root file");
        fs::write(directory.path().join("app/inside.txt"), b"inside").expect("inside file");
        fs::write(directory.path().join("app/sub/deep.txt"), b"deep").expect("deep file");
        let runner = directory.path().join("app/runner");
        fs::write(&runner, b"#!/bin/sh\n").expect("runner");
        fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).expect("runner mode");
        fs::write(directory.path().join("app/rubbish"), b"plain\n").expect("non-executable decoy");
        fs::write(directory.path().join("app/script.py"), b"print('ok')\n").expect("Python script");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["env -C app cat in", "sudo -D app cat in"] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "inside.txt"),
                "nested cwd file missing for {buffer:?}"
            );
            assert!(
                !output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "root.txt"),
                "outer cwd leaked for {buffer:?}"
            );
        }
        assert!(
            provider
                .complete(&context(directory.path(), "builtin ls ", 4))
                .candidates
                .is_empty()
        );

        let wrapper_value =
            provider.complete(&context(directory.path(), "sudo -D app env -C su", 2));
        assert!(
            wrapper_value
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "sub/")
        );

        for buffer in [
            "env -C app ./ru",
            "find . -exec ./app/ru",
            "find . -exec env -C app ./ru",
            "uv --directory app run ./ru",
            "poetry -C app run ./ru",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 3));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary.ends_with("runner")),
                "explicit executable missing for {buffer:?}"
            );
            assert!(
                output
                    .candidates
                    .iter()
                    .all(|candidate| !candidate.display.primary.ends_with("rubbish")),
                "non-executable leaked for {buffer:?}"
            );
        }

        let script = provider.complete(&context(
            directory.path(),
            "uv --directory app run --script scr",
            5,
        ));
        assert!(
            script
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "script.py")
        );
        assert!(
            provider
                .complete(&context(
                    directory.path(),
                    "uv --directory app run --module scr",
                    6,
                ))
                .candidates
                .is_empty(),
            "Python module names must not degrade into filesystem rows"
        );
    }

    #[test]
    fn package_manager_path_flags_and_literal_values_have_distinct_slots() {
        let directory = tempfile::tempdir().expect("directory");
        fs::create_dir(directory.path().join("cache-dir")).expect("cache directory");
        fs::write(directory.path().join("npmrc"), b"").expect("npmrc");
        fs::write(directory.path().join("1000"), b"").expect("numeric decoy");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in [
            "pnpm --store-dir ca",
            "npm --cache ca",
            "yarn --cache-folder ca",
        ] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "cache-dir/"),
                "directory flag missing for {buffer:?}"
            );
        }
        assert!(
            provider
                .complete(&context(directory.path(), "npm --userconfig np", 2))
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "npmrc")
        );
        for buffer in ["deno run --location 1", "bun test --timeout 1"] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 3))
                    .candidates
                    .is_empty(),
                "literal manager value leaked files for {buffer:?}"
            );
        }
    }

    #[test]
    fn find_start_paths_accept_files_and_repeat_until_the_expression_begins() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("start.txt"), b"").expect("start file");
        fs::create_dir(directory.path().join("nested")).expect("nested");
        let provider = provider(Arc::new(SpecRegistry::default()));

        for buffer in ["find ", "find -L ", "find start.txt "] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "start.txt"),
                "find start file missing for {buffer:?}"
            );
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "nested/"),
                "find directory missing for {buffer:?}"
            );
        }
        for buffer in ["find . -name ", "find . -type f "] {
            assert!(
                provider
                    .complete(&context(directory.path(), buffer, 2))
                    .candidates
                    .is_empty(),
                "find expression leaked start paths for {buffer:?}"
            );
        }
    }

    #[test]
    fn make_and_ssh_family_flag_tables() {
        let directory = tempfile::tempdir().expect("directory");
        fs::write(directory.path().join("plain.txt"), b"plain").expect("file");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        let provider = provider(Arc::new(SpecRegistry::default()));

        // `make -f` takes a file (Path), `-C` stays directory-only.
        let output = provider.complete(&context(directory.path(), "make -f ", 1));
        assert!(
            output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt"),
            "`make -f ` must offer files"
        );
        let output = provider.complete(&context(directory.path(), "make -C ", 2));
        assert!(
            !output
                .candidates
                .iter()
                .any(|candidate| candidate.display.primary == "plain.txt"),
            "`make -C ` must stay directory-only"
        );

        // Attached jobs flag `make -j4`: no file rows at the next slot.
        let output = provider.complete(&context(directory.path(), "make -j4 ", 3));
        assert!(
            output.candidates.is_empty(),
            "`make -j4 ` must not offer file rows"
        );

        // ssh-family literal value flags stay quiet; path flags offer files.
        for buffer in ["mosh -p ", "sftp -P ", "ssh -Q ", "scp -J "] {
            let output = provider.complete(&context(directory.path(), buffer, 1));
            assert!(
                output.candidates.is_empty(),
                "{buffer:?} must not offer file rows"
            );
        }
        for buffer in ["ssh -S ", "ssh -I ", "sftp -D ", "scp -D "] {
            let output = provider.complete(&context(directory.path(), buffer, 4));
            assert!(
                output
                    .candidates
                    .iter()
                    .any(|candidate| candidate.display.primary == "plain.txt"),
                "{buffer:?} must offer files"
            );
        }
    }

    fn context(directory: &std::path::Path, text: &str, query: u64) -> CompletionContext {
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
