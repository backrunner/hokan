//! Bounded man/help probes and merging of complementary documentation.
use super::{
    CommandHelp,
    parsing::{parse_help_output_for_scope, parse_man_page},
};
use std::{collections::HashSet, path::Path, time::Duration};

// `ps`/`ifconfig`-style probe precedent: the `man` binary is a fixed trusted
// program, receives no shell and no user-controlled argv beyond the command
// name (placed after `--`), reads null stdin, and is bounded in time and
// output. macOS `man` always runs the troff pipeline: warm `man -P cat cp`
// measures 120-150 ms here and up to ~550 ms under full-suite test load, so
// a ~150 ms cap negative-caches most cold fetches. 1200 ms stays bounded
// while covering loaded machines; the fetch runs at most once per command
// per session on a background thread, outside the interactive query path.
const MAN_TIMEOUT: Duration = Duration::from_millis(1200);
// `--help` fallback for modern CLIs without a (useful) man page: the binary
// itself is a fixed program resolved on PATH, receives no shell, a single
// literal `--help` argument, null stdin, and is bounded in time and output —
// the same discipline as the `man` probe. 800 ms covers warm `kubectl
// --help`-style runs without letting a cold binary stall the applies pass.
const HELP_TIMEOUT: Duration = Duration::from_millis(800);
const MAN_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub(super) fn command_basename(command: &str) -> &str {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
}

pub(super) fn help_probe_allowed(command: &str, executable: Option<&Path>) -> bool {
    executable.is_some()
        && !command.contains('/')
        // Wrapper scripts can download runtimes and execute project-owned
        // bootstrap logic even for `--help`. Their static completion surfaces
        // are handled without launching the wrapper.
        && !matches!(command_basename(command), "gradlew" | "mvnw")
}

pub(super) fn fetch_man_page(command: &str) -> CommandHelp {
    let Ok(output) = crate::platform::run_bounded(
        "man",
        ["-P", "cat", "--", command],
        MAN_TIMEOUT,
        MAN_MAX_OUTPUT_BYTES,
    ) else {
        return CommandHelp::default();
    };
    if !output.status.success() || output.stdout.is_empty() {
        return CommandHelp::default();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_man_page(command, &text)
}

/// Merge both bounded documentation sources on a cold miss. A nonempty man
/// page can still omit commands or flags documented by the installed binary.
pub(super) fn fetch_command_help(command: &str) -> CommandHelp {
    let help_command = command_basename(command);
    fetch_with_fallback(help_command, fetch_man_page, |_| fetch_help_output(command))
}

pub(super) fn fetch_command_help_from(command: &str, executable: Option<&Path>) -> CommandHelp {
    let help_command = command_basename(command);
    let parsed = fetch_man_page(help_command);
    let fallback = executable.map_or_else(
        || fetch_help_output(command),
        |path| fetch_help_program(help_command, path.as_os_str()),
    );
    merge_help_sources(help_command, parsed, fallback)
}

pub(super) fn fetch_scoped_help_from(
    command: &str,
    executable: Option<&Path>,
    scope: &[String],
) -> CommandHelp {
    let help_command = command_basename(command);
    executable.map_or_else(
        || fetch_help_program_for_scope(help_command, std::ffi::OsStr::new(command), scope),
        |path| fetch_help_program_for_scope(help_command, path.as_os_str(), scope),
    )
}

pub(super) fn fetch_with_fallback(
    command: &str,
    man: impl Fn(&str) -> CommandHelp,
    help: impl Fn(&str) -> CommandHelp,
) -> CommandHelp {
    let parsed = man(command);
    merge_help_sources(command, parsed, help(command))
}

pub(super) fn merge_help_sources(
    command: &str,
    man: CommandHelp,
    help: CommandHelp,
) -> CommandHelp {
    if command == "brew" {
        // Homebrew's concise help output is explicitly ordered around common
        // workflows; keep it first and append the broader man-page surface.
        merge_help(help, man)
    } else {
        merge_help(man, help)
    }
}

pub(super) fn merge_help(mut primary: CommandHelp, fallback: CommandHelp) -> CommandHelp {
    let mut seen_flags: HashSet<String> = primary
        .flags
        .iter()
        .map(|entry| entry.name.clone())
        .collect();
    for flag in fallback.flags {
        if seen_flags.insert(flag.name.clone()) {
            primary.flags.push(flag);
        }
    }
    let mut seen_subcommands: HashSet<String> = primary
        .subcommands
        .iter()
        .map(|entry| entry.name.clone())
        .chain(primary.subcommand_aliases.iter().cloned())
        .collect();
    for subcommand in fallback.subcommands {
        if !seen_subcommands.insert(subcommand.name.clone()) {
            continue;
        }
        primary.subcommands.push(subcommand);
    }
    for alias in fallback.subcommand_aliases {
        if !seen_subcommands.insert(alias.clone()) {
            continue;
        }
        primary.subcommand_aliases.push(alias);
    }
    primary.subcommands_exhaustive =
        primary.subcommands_exhaustive || fallback.subcommands_exhaustive;
    primary.accepts_positionals |= fallback.accepts_positionals;
    primary
}

/// Modern-CLI fallback: `<cmd> --help`, bounded exactly like the man probe —
/// the command resolves on PATH (the provider only fires for commands the
/// user can execute), gets no shell and no user-controlled argv beyond the
/// literal `--help`, reads null stdin, and dies on timeout. A failing,
/// hanging, or empty run degrades to an empty `CommandHelp`.
pub(super) fn fetch_help_output(command: &str) -> CommandHelp {
    fetch_help_program(command_basename(command), std::ffi::OsStr::new(command))
}

pub(super) fn fetch_help_program(command: &str, program: &std::ffi::OsStr) -> CommandHelp {
    fetch_help_program_for_scope(command, program, &[])
}

pub(super) fn fetch_help_program_for_scope(
    command: &str,
    program: &std::ffi::OsStr,
    scope: &[String],
) -> CommandHelp {
    for arguments in help_probe_arguments(command, scope) {
        let Ok(output) =
            crate::platform::run_bounded(program, &arguments, HELP_TIMEOUT, MAN_MAX_OUTPUT_BYTES)
        else {
            continue;
        };
        let mut parsed = CommandHelp::default();
        for bytes in [&output.stdout, &output.stderr] {
            if bytes.is_empty() {
                continue;
            }
            let text = String::from_utf8_lossy(bytes);
            if output.status.success() || looks_like_help_output(&text) {
                parsed = merge_help(parsed, parse_help_output_for_scope(command, scope, &text));
            }
        }
        if !parsed.flags.is_empty() || !parsed.subcommands.is_empty() {
            return parsed;
        }
    }
    CommandHelp::default()
}

pub(super) fn help_probe_arguments(command: &str, scope: &[String]) -> Vec<Vec<String>> {
    let command = command_basename(command);
    let prefixed_help = || {
        let mut arguments = Vec::with_capacity(scope.len() + 1);
        arguments.push("help".to_owned());
        arguments.extend(scope.iter().cloned());
        arguments
    };
    let suffixed = |flag: &str| {
        let mut arguments = Vec::with_capacity(scope.len() + 1);
        arguments.extend(scope.iter().cloned());
        arguments.push(flag.to_owned());
        arguments
    };
    match command {
        "defaults" | "diskutil" | "go" | "launchctl" | "openssl" | "security" => {
            vec![prefixed_help()]
        }
        "brew" | "gem" | "svn" if !scope.is_empty() => vec![prefixed_help()],
        "swift" | "vcpkg" if !scope.is_empty() => {
            vec![prefixed_help(), suffixed("--help")]
        }
        "pnpm" if scope.is_empty() => {
            vec![vec!["help".to_owned(), "-a".to_owned()], suffixed("--help")]
        }
        "git" if !scope.is_empty() => vec![suffixed("-h")],
        "terraform" | "tofu" if !scope.is_empty() => {
            vec![suffixed("-help"), suffixed("--help")]
        }
        "ffmpeg" | "ffplay" | "ffprobe" | "perl" | "tmux" if scope.is_empty() => {
            vec![vec!["-h".to_owned()]]
        }
        "networksetup" | "sqlite3" | "xcodebuild" if scope.is_empty() => {
            vec![vec!["-help".to_owned()]]
        }
        _ => vec![suffixed("--help")],
    }
}

pub(super) fn looks_like_help_output(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        line.starts_with("usage:")
            || line == "usage"
            || line.starts_with("commands:")
            || line.starts_with("available commands:")
            || line.starts_with("subcommands:")
            || line == "options:"
            || line == "flags:"
            || line == "standard commands"
            || line == "the commands are:"
    })
}
