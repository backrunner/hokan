//! Positive command contracts for the active argument. No filesystem probing
//! participates in deciding what an argument means. Unmodelled operands are Unknown.
use super::{
    argument_progress,
    filesystem::{scan_directory_with_root, split_prefix},
};
use crate::completion::{ArgumentSemantics, CompletionContext, PathOrigin, ResolvedPath, SlotKind};
use std::collections::VecDeque;
mod path_intent;

pub(crate) fn resolve(context: &CompletionContext) -> ArgumentSemantics {
    let Some(slot) = resolve_path(context) else {
        if let Some((words, position)) = argument_progress(context) {
            let command = super::executable_basename(context.command().unwrap_or_default());
            if !flags_ended_before(&words, position)
                && (flag_value_slot(command, words.get(position).copied().unwrap_or_default())
                    == Some(SlotKind::Value)
                    || attached_flag_value(command, &context.parsed.current_prefix)
                        .is_some_and(|flag| flag.kind == SlotKind::Value))
            {
                return ArgumentSemantics::Text;
            }
        }
        return ArgumentSemantics::Unknown;
    };
    let Some(literal_root) = path_intent::directory_intent(context, &slot) else {
        return ArgumentSemantics::UnresolvedPath;
    };
    let origin = if super::redirect_path_target(context) {
        PathOrigin::Redirection
    } else if super::explicit_executable_path_position(context) {
        PathOrigin::Executable
    } else {
        PathOrigin::CommandContract
    };
    let base = slot.base_dir.as_deref().unwrap_or(context.cwd.as_ref());
    ArgumentSemantics::LocalPath(ResolvedPath {
        kind: slot.kind,
        defer_to_recipes: slot.defer_to_recipes,
        prefix: slot.prefix.to_owned(),
        edit_prefix: slot.edit_prefix.to_owned(),
        directory: scan_directory_with_root(base, split_prefix(slot.prefix).0, literal_root),
        base_dir: base.to_owned(),
        literal_root,
        origin,
        file_next_slot: required_file_followup_slot(context, slot.kind),
        cd_target: context.command() == Some("cd")
            && slot.kind == SlotKind::Directory
            && wrapper_filesystem_slot(context).is_none()
            && super::command_resolution_kind(context)
                != crate::parser::EffectiveCommandKind::External,
    })
}

fn resolve_path(context: &CompletionContext) -> Option<FilesystemSlot<'_>> {
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
    let command_base = command_directory_before_active(context, command, &words, argument_position);
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
        // Package runners accept both package identifiers and executable paths.
        // Only the rooted branch of that union authorizes a local scan.
        if (super::is_package_manager(command) || matches!(command, "npx" | "pnpx"))
            && !path_intent::rooted(prefix)
        {
            return None;
        }

        let mut slot = FilesystemSlot::plain(SlotKind::Executable, prefix);
        slot.base_dir = Some(command_base);
        return Some(slot);
    }
    if let Some(value) = response_file_prefix(command, prefix) {
        return Some(FilesystemSlot {
            defer_to_recipes: false,
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
        && let Some(mut slot) =
            path_intent::contextual_flag_slot(command, &words, argument_position, prefix)
    {
        slot.base_dir = Some(command_base.clone());
        return Some(slot);
    }
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
                    defer_to_recipes: false,
                    kind: SlotKind::Path,
                    prefix: value,
                    edit_prefix,
                    base_dir: Some(command_base.clone()),
                });
            }
            (attached.kind != SlotKind::Value).then_some(FilesystemSlot {
                defer_to_recipes: false,
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
                defer_to_recipes: false,
                kind: SlotKind::Path,
                prefix,
                edit_prefix: "@",
                base_dir: Some(command_base.clone()),
            });
        }
        return (slot != SlotKind::Value).then_some(FilesystemSlot {
            defer_to_recipes: false,
            kind: slot,
            prefix,
            edit_prefix: "",
            base_dir: Some(command_base),
        });
    }
    let mut defer_to_recipes = false;
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
        "bash" | "zsh" | "sh" => None,
        "go" if positional_words_before(command, &words, argument_position)
            .is_some_and(|args| matches!(args.as_slice(), ["run"] | ["build"]))
            && !prefix.is_empty()
            && !path_intent::rooted(prefix)
            && !prefix.ends_with(".go") =>
        {
            None
        }
        "df" => Some(SlotKind::Path),
        "lsof" if words.get(argument_position).copied() == Some("+D") => Some(SlotKind::Directory),
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
        "git"
            if at_git_ref_slot(&words, argument_position)
                && explicit_git_path(prefix)
                && super::git::ref_subcommand_accepts_paths(&words) =>
        {
            Some(SlotKind::Path)
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
        "grep" | "egrep" | "fgrep" | "rg" | "ag" | "sed" | "awk" | "jq" => {
            default_path_slot(command, &words, argument_position)
        }
        "brew"
            if path_intent::rooted(prefix)
                && positional_words_before(command, &words, argument_position).is_some_and(
                    |args| matches!(args.first(), Some(&"install" | &"reinstall")),
                ) =>
        {
            Some(SlotKind::Path)
        }
        "docker" | "podman"
            if positional_words_before(command, &words, argument_position)
                .is_some_and(|args| args == ["build"])
                && !non_local_path_prefix(prefix) =>
        {
            Some(SlotKind::Path)
        }
        "gh" if positional_words_before(command, &words, argument_position)
            .is_some_and(|args| matches!(args.as_slice(), ["repo", "clone", _])) =>
        {
            Some(SlotKind::Directory)
        }
        "git"
            if path_intent::rooted(prefix)
                && positional_words_before(command, &words, argument_position)
                    .is_some_and(|args| args == ["fetch"]) =>
        {
            Some(SlotKind::Path)
        }
        // Build-tool first arguments are target names, not paths.
        "make" | "just" => None,
        // Package-manager command/script selection belongs to the project
        // provider. Only modelled file-taking commands grant a path slot.
        _ if super::filter_position(context).is_some() => None,
        _ if super::package_manager(context).is_some() => {
            package_manager_path_slot(context, command, &words, argument_position, prefix)
        }
        _ => {
            defer_to_recipes = true;
            default_path_slot(command, &words, argument_position)
        }
    }?;
    let mut slot = FilesystemSlot::plain(kind, prefix);
    slot.defer_to_recipes = defer_to_recipes;
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

#[derive(Clone)]
pub(crate) struct FilesystemSlot<'a> {
    pub(crate) defer_to_recipes: bool,
    pub(crate) kind: SlotKind,
    pub(crate) prefix: &'a str,
    pub(crate) edit_prefix: &'a str,
    pub(crate) base_dir: Option<std::path::PathBuf>,
}

impl<'a> FilesystemSlot<'a> {
    const fn plain(kind: SlotKind, prefix: &'a str) -> Self {
        Self {
            defer_to_recipes: false,
            kind,
            prefix,
            edit_prefix: "",
            base_dir: None,
        }
    }
}

pub(super) fn wrapper_filesystem_slot(context: &CompletionContext) -> Option<FilesystemSlot<'_>> {
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
                defer_to_recipes: false,
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
                    defer_to_recipes: false,
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
        defer_to_recipes: false,
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

/// Recognize the npm scoped-package spelling while it is being typed.  The
/// package name may be empty after the scope slash (`@scope/`) and may carry a
/// version suffix (`@scope/name@latest`).
fn scoped_package_prefix(prefix: &str) -> bool {
    let Some(rest) = prefix.strip_prefix('@') else {
        return false;
    };
    let Some((scope, package)) = rest.split_once('/') else {
        return false;
    };
    if !package_name_segment(scope) {
        return false;
    }
    let package = package.trim_end_matches('/');
    if package.is_empty() {
        return true;
    }
    let package = package.split('@').next().unwrap_or_default();
    package_name_segment(package)
}

fn package_name_segment(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
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
                defer_to_recipes: false,
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
            defer_to_recipes: false,
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
                defer_to_recipes: false,
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
                defer_to_recipes: false,
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

/// Positive positional contracts. Unmodelled commands and operands return None;
/// spelling and filesystem existence never supply a fallback contract.
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
pub(super) fn required_file_followup_slot(
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
    // Package-manager path flags are handled before this fallback, so a
    // scoped name reaching here is a package selector rather than a directory
    // value (`npm --prefix @scope/dir` still gets normal path completion).
    if non_local_path_prefix(prefix) || scoped_package_prefix(prefix) {
        return None;
    }
    let explicit_path = path_intent::rooted(prefix);
    if argument_position == 0 {
        return None;
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
    let local_package = explicit_path && matches!(subcommand, "add" | "install" | "i");
    (file_command || local_package).then_some(SlotKind::Path)
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
        defer_to_recipes: false,
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
        defer_to_recipes: false,
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
        ("docker" | "podman", "--env-file" | "--file")
        | ("gh", "--input")
        | ("kubectl", "--filename" | "--kubeconfig")
        | ("git", "--upload-pack" | "--receive-pack") => Some(SlotKind::Path),
        ("docker" | "podman", "--config") | ("kubectl", "-k" | "--kustomize") => {
            Some(SlotKind::Directory)
        }
        (
            "gh",
            "-R" | "--repo" | "--hostname" | "-H" | "--header" | "-X" | "--method" | "-F"
            | "--field" | "-f" | "--raw-field" | "--jq" | "--template",
        )
        | (
            "kubectl",
            "-n" | "--namespace" | "--context" | "-l" | "--selector" | "--field-selector" | "-o"
            | "--output" | "-s" | "--server",
        ) => Some(SlotKind::Value),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion::{BufferSnapshot, SyncQuality},
        shell::ShellKind,
        terminal::{BufferRevision, QueryId},
    };
    fn context(root: &std::path::Path, line: &str) -> CompletionContext {
        CompletionContext::new(
            QueryId::new(1),
            ShellKind::Zsh,
            root.into(),
            BufferSnapshot::new(line, line.len(), BufferRevision::new(1), SyncQuality::Exact)
                .expect("buffer"),
        )
        .expect("context")
    }

    #[test]
    fn unknown_contract_is_independent_of_argument_spelling_and_disk_contents() {
        let root = tempfile::tempdir().expect("root");
        // No command names need to be added to an exclusion list. Changing the
        // filesystem must not change an unknown argument's meaning.
        for index in 0..128 {
            let command = format!("unmodelled-{index}");
            let path = format!("owner-{index}/repo");
            for prefix in [
                path.clone(),
                format!("./{path}"),
                format!("/{path}"),
                format!("~/{}", path),
            ] {
                let line = format!("{command} {prefix}/item");
                assert_eq!(
                    context(root.path(), &line).argument,
                    ArgumentSemantics::Unknown
                );
                std::fs::create_dir_all(root.path().join(&path)).expect("colliding directory");
                assert_eq!(
                    context(root.path(), &line).argument,
                    ArgumentSemantics::Unknown
                );
            }
        }
    }

    #[test]
    fn fragmented_expansion_names_do_not_gain_known_root_authority() {
        let root = tempfile::tempdir().expect("root");
        for line in [
            r#"cat "$HO"ME/file"#,
            r"cat $HO\ME/file",
            r#"cat ${HO"ME"}/file"#,
            r#"curl --output="$HO"ME/file"#,
            "cat ~''/file",
        ] {
            assert_eq!(
                context(root.path(), line).argument,
                ArgumentSemantics::UnresolvedPath,
                "{line}"
            );
        }
        // Quoting the whole variable keeps expansion; quoting the dollar
        // makes a literal directory. Neither should be confused with fragments.
        assert!(
            !context(root.path(), r#"cat "$HOME"/file"#)
                .argument
                .local_path()
                .expect("expanded")
                .literal_root
        );
        assert!(
            context(root.path(), "cat '$HOME'/file")
                .argument
                .local_path()
                .expect("literal")
                .literal_root
        );
    }

    #[test]
    fn contracts_preserve_provenance_and_resolve_only_safe_local_paths() {
        let root = tempfile::tempdir().expect("root");
        for (line, origin) in [
            ("anything > nested/file", PathOrigin::Redirection),
            ("./nested/tool", PathOrigin::Executable),
            ("cat nested/file", PathOrigin::CommandContract),
            ("curl --output=nested/file", PathOrigin::CommandContract),
        ] {
            let context = context(root.path(), line);
            let path = context.argument.local_path().expect(line);
            assert_eq!(path.origin, origin);
            assert_eq!(path.directory, root.path().join("nested"));
        }
        for line in ["cat $UNRESOLVED/file", "cat src/*/file", "cat $(pwd)/file"] {
            assert_eq!(
                context(root.path(), line).argument,
                ArgumentSemantics::UnresolvedPath,
                "{line}"
            );
        }
        assert_eq!(
            context(root.path(), "git commit -m owner/repo").argument,
            ArgumentSemantics::Text
        );
        let destination = context(root.path(), "gh repo clone owner/repo new/dir");
        assert_eq!(
            destination.argument.local_path().expect("destination").kind,
            SlotKind::Directory
        );
    }
}
