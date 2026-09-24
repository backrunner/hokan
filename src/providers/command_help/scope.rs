//! Resolve the active subcommand and argument position without blocking.
use super::{CommandHelp, CommandHelpCache, HelpEntry, probe::command_basename};
use crate::{completion::CompletionContext, providers::argument_progress};
use std::{path::PathBuf, sync::Arc};
pub(super) const MAX_HELP_SCOPE_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HelpPosition {
    Flags,
    Subcommands,
    BareSubcommands,
    Values(usize),
}

pub(super) struct HelpTarget {
    pub(super) help: Arc<CommandHelp>,
    pub(super) position: HelpPosition,
    pub(super) scope: Vec<String>,
}

pub(super) enum HelpLookup {
    Ready(HelpTarget),
    Pending,
    None,
}

pub(super) enum CompletedScope<'a> {
    Subcommand { word: &'a str, consumed: usize },
    None,
    Blocked,
}

pub(super) fn lookup_help_scope(
    context: &CompletionContext,
    cache: &Arc<CommandHelpCache>,
    command: &str,
    executable: Option<PathBuf>,
    request_missing: bool,
) -> HelpLookup {
    let Some(mut help) = cache.peek(command) else {
        if request_missing {
            cache.request(command, executable);
            return HelpLookup::Pending;
        }
        return HelpLookup::None;
    };
    let Some((words, position)) = argument_progress(context) else {
        return (help.has_subcommands() && bare_command_position(context, command))
            .then_some(HelpTarget {
                help,
                position: HelpPosition::BareSubcommands,
                scope: Vec::new(),
            })
            .map_or(HelpLookup::None, HelpLookup::Ready);
    };
    let before = words.get(1..=position).unwrap_or_default();
    let mut scope = Vec::new();
    let mut consumed = 0;
    loop {
        match completed_scope(command, scope.is_empty(), &help, &before[consumed..]) {
            CompletedScope::Subcommand {
                word,
                consumed: scope_consumed,
            } => {
                if !supports_scoped_help(command, &help) || scope.len() >= MAX_HELP_SCOPE_DEPTH {
                    return HelpLookup::None;
                }
                consumed += scope_consumed;
                scope.push(word.to_owned());
                if let Some(scoped) = cache.peek_scope(command, &scope) {
                    help = scoped;
                    continue;
                }
                if request_missing {
                    cache.request_scope(command, executable, scope);
                    return HelpLookup::Pending;
                }
                return if cache.scope_is_pending(command, &scope) {
                    HelpLookup::Pending
                } else {
                    HelpLookup::None
                };
            }
            CompletedScope::None => {
                let Some(position) = help_position_for_arguments(
                    command,
                    scope.is_empty(),
                    &help,
                    &before[consumed..],
                    &context.parsed.current_prefix,
                ) else {
                    return HelpLookup::None;
                };
                return HelpLookup::Ready(HelpTarget {
                    help,
                    position,
                    scope,
                });
            }
            CompletedScope::Blocked => return HelpLookup::None,
        }
    }
}

pub(super) fn completed_scope<'a>(
    command: &str,
    allow_toolchain_selector: bool,
    help: &CommandHelp,
    before: &'a [&'a str],
) -> CompletedScope<'a> {
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if word == "--" {
            return CompletedScope::Blocked;
        }
        if allow_toolchain_selector && toolchain_selector(command, word) {
            index += 1;
            continue;
        }
        if word.starts_with('-') && word != "-" {
            let Some((entry, attached_value)) = help_flag_usage(help, word) else {
                return CompletedScope::Blocked;
            };
            index += 1;
            if entry.takes_value && !attached_value {
                if index >= before.len() {
                    return CompletedScope::None;
                }
                index += 1;
            }
            continue;
        }
        if help.subcommands.iter().any(|entry| entry.name == word)
            || help.subcommand_aliases.iter().any(|alias| alias == word)
        {
            return CompletedScope::Subcommand {
                word,
                consumed: index + 1,
            };
        }
        return CompletedScope::Blocked;
    }
    CompletedScope::None
}

pub(super) fn help_position_for_arguments(
    command: &str,
    allow_toolchain_selector: bool,
    help: &CommandHelp,
    before: &[&str],
    current_prefix: &str,
) -> Option<HelpPosition> {
    let mut index = 0;
    while let Some(word) = before.get(index).copied() {
        if word == "--" {
            return None;
        }
        if allow_toolchain_selector && toolchain_selector(command, word) {
            index += 1;
            continue;
        }
        if word.starts_with('-') && word != "-" {
            let (entry_index, attached_value) = help_flag_usage_index(help, word)?;
            let entry = &help.flags[entry_index];
            index += 1;
            if entry.takes_value && !attached_value {
                if index >= before.len() {
                    return Some(HelpPosition::Values(entry_index));
                }
                index += 1;
            }
            continue;
        }
        return None;
    }

    if current_prefix.starts_with('-')
        && let Some((entry_index, attached_value)) = help_flag_usage_index(help, current_prefix)
        && attached_value
    {
        return Some(HelpPosition::Values(entry_index));
    }
    if current_prefix.starts_with('-') && !current_prefix.contains('=') {
        Some(HelpPosition::Flags)
    } else if !current_prefix.starts_with('-') {
        Some(HelpPosition::Subcommands)
    } else {
        None
    }
}

pub(super) fn toolchain_selector(command: &str, word: &str) -> bool {
    matches!(
        command_basename(command),
        "cargo" | "rustc" | "rustdoc" | "rustup"
    ) && word
        .strip_prefix('+')
        .is_some_and(|selector| !selector.is_empty() && !selector.contains('/'))
}

pub(super) fn bare_command_position(context: &CompletionContext, command: &str) -> bool {
    (crate::providers::command_position_open(context)
        || crate::providers::explicit_executable_path_position(context))
        && context.parsed.current_prefix == command
}

pub(crate) fn dynamic_help_owns_position(
    context: &CompletionContext,
    cache: &Arc<CommandHelpCache>,
) -> bool {
    let Some(command) = context.command() else {
        return false;
    };
    match lookup_help_scope(context, cache, command, None, false) {
        HelpLookup::Ready(target) => match target.position {
            HelpPosition::Flags | HelpPosition::Values(_) => true,
            HelpPosition::Subcommands | HelpPosition::BareSubcommands => {
                target.help.has_subcommands()
                    && (!hybrid_subcommand_path_command(command, &target.scope)
                        || context.parsed.current_prefix.is_empty()
                        || target.help.subcommands.iter().any(|entry| {
                            entry
                                .name
                                .to_ascii_lowercase()
                                .starts_with(&context.parsed.current_prefix.to_ascii_lowercase())
                        }))
            }
        },
        HelpLookup::Pending => true,
        HelpLookup::None => false,
    }
}

pub(super) fn hybrid_subcommand_path_command(command: &str, scope: &[String]) -> bool {
    scope.is_empty() && command_basename(command) == "swift"
}

/// `<cmd> <sub> --help`-style scoped probing only pays off when the CLI is
/// known — or has just proven — to document each scope. The allowlist covers
/// curated and man-derived dispatchers (`git remote`, `apt list`, …) whose
/// subcommand lists do not guarantee a working `<sub> --help`. A command
/// whose own `--help` already exposed a complete Commands section is a
/// self-describing CLI (cobra/clap/commander/yargs — including most
/// node-installed tools) where the same probe is reliable, so those commands
/// qualify without appearing in the list.
pub(super) fn supports_scoped_help(command: &str, help: &CommandHelp) -> bool {
    help.subcommands_exhaustive
        || crate::providers::is_pip_command(command)
        || matches!(
            command_basename(command),
            "ansible"
                | "apt"
                | "aws"
                | "az"
                | "brew"
                | "cargo"
                | "claude"
                | "codex"
                | "composer"
                | "conan"
                | "consul"
                | "diskutil"
                | "dnf"
                | "docker"
                | "docker-compose"
                | "dotnet"
                | "eksctl"
                | "firebase"
                | "flyctl"
                | "gcloud"
                | "gem"
                | "git"
                | "gh"
                | "glab"
                | "go"
                | "helm"
                | "heroku"
                | "hokan"
                | "istioctl"
                | "kubectl"
                | "launchctl"
                | "mise"
                | "meson"
                | "nerdctl"
                | "nix"
                | "nomad"
                | "oc"
                | "ollama"
                | "openssl"
                | "pacman"
                | "pip"
                | "pip3"
                | "pipx"
                | "pnpm"
                | "podman"
                | "poetry"
                | "railway"
                | "rustup"
                | "security"
                | "snap"
                | "svn"
                | "swift"
                | "systemctl"
                | "terraform"
                | "tofu"
                | "uv"
                | "vagrant"
                | "vault"
                | "vcpkg"
                | "vercel"
                | "wrangler"
                | "npm"
                | "yarn"
                | "bun"
                | "deno"
        )
}

pub(super) fn help_flag_usage<'a>(
    help: &'a CommandHelp,
    word: &str,
) -> Option<(&'a HelpEntry, bool)> {
    let (index, attached) = help_flag_usage_index(help, word)?;
    Some((&help.flags[index], attached))
}

pub(super) fn help_flag_usage_index(help: &CommandHelp, word: &str) -> Option<(usize, bool)> {
    if let Some((index, _)) = help
        .flags
        .iter()
        .enumerate()
        .find(|(_, entry)| entry.name == word)
    {
        return Some((index, false));
    }
    if let Some((name, _)) = word.split_once('=') {
        return help
            .flags
            .iter()
            .enumerate()
            .find(|(_, entry)| entry.name == name)
            .map(|(index, _)| (index, true));
    }
    help.flags
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.takes_value && entry.name.len() == 2)
        .find(|(_, entry)| word.len() > entry.name.len() && word.starts_with(&entry.name))
        .map(|(index, _)| (index, true))
}
