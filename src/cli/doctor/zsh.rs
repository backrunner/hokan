use std::{
    env, fs,
    path::{Path, PathBuf},
};

use super::{Check, CheckLevel};

/// Read the user's zsh rc files ($ZDOTDIR or $HOME: `.zshenv`, `.zshrc`) and
/// report the installed setup mode plus any known-conflicting plugin
/// integrations. Never fails: unreadable files degrade to an info note.
/// Also checks `.zprofile`/`.zshrc` for prompt-theme initializers that the
/// inner shell might skip (login_shell = false).
pub(super) fn inspect_zsh_rc_files(login_shell: bool) -> (Check, Vec<Check>, Check) {
    // An empty ZDOTDIR is treated as unset, matching zsh's behavior.
    let base = env::var_os("ZDOTDIR")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from);
    let Some(base) = base else {
        return (
            Check::new(
                CheckLevel::NotApplicable,
                "neither $ZDOTDIR nor $HOME is set",
            ),
            vec![Check::new(
                CheckLevel::NotApplicable,
                "cannot locate zsh rc files",
            )],
            Check::new(CheckLevel::NotApplicable, "cannot locate zsh rc files"),
        );
    };
    let mut setup_mode = Check::new(
        CheckLevel::NotApplicable,
        "no Hokan integration block in .zshrc",
    );
    let mut conflicts = Vec::new();
    let mut saw_file = false;
    for name in [".zshenv", ".zshrc"] {
        let path = base.join(name);
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                conflicts.push(Check::new(
                    CheckLevel::NotApplicable,
                    format!("{}: cannot read ({error}); skipped", path.display()),
                ));
                continue;
            }
        };
        saw_file = true;
        if name == ".zshrc"
            && let Some(mode) = detect_setup_mode(&contents)
        {
            setup_mode = Check::new(CheckLevel::Ok, format!("{mode} in {}", path.display()));
        }
        conflicts.extend(scan_plugin_conflicts(&path, &contents));
    }
    if conflicts.is_empty() {
        let detail = if saw_file {
            "no known plugin conflicts in zsh rc files"
        } else {
            "no zsh rc files found"
        };
        conflicts.push(Check::new(CheckLevel::Ok, detail));
    }
    (setup_mode, conflicts, inspect_zsh_theme(&base, login_shell))
}

/// Detect prompt-theme initializers (oh-my-posh, starship, powerlevel10k) in
/// the user's zsh startup files. A theme initialized only in `.zprofile` is
/// loaded by login shells only, so a non-login inner shell
/// (`core.login_shell = false`) never sees it.
pub(super) fn inspect_zsh_theme(base: &Path, login_shell: bool) -> Check {
    let zshrc = base.join(".zshrc");
    if let Ok(contents) = fs::read_to_string(&zshrc)
        && let Some(theme) = theme_for_contents(&contents)
    {
        return Check::new(
            CheckLevel::Ok,
            format!("{theme} initializes in .zshrc; the inner shell loads it"),
        );
    }
    let zprofile = base.join(".zprofile");
    if let Ok(contents) = fs::read_to_string(&zprofile)
        && let Some(theme) = theme_for_contents(&contents)
    {
        if login_shell {
            return Check::new(
                CheckLevel::Ok,
                format!("{theme} initializes in .zprofile; loaded because login_shell = true"),
            );
        }
        return Check::new(
            CheckLevel::Warn,
            format!(
                "{theme} initializes in .zprofile, which only login shells read; Hokan's inner zsh is not a login shell (core.login_shell = false), so the theme will not appear inside Hokan. Set `login_shell = true` in ~/.config/hokan/config.toml or move the init to .zshrc"
            ),
        );
    }
    Check::new(
        CheckLevel::NotApplicable,
        "no prompt theme initializer found in .zshrc/.zprofile",
    )
}

/// Return the theme name when the file contents initialize a known prompt
/// theme. Comment lines are ignored.
pub(super) fn theme_for_contents(contents: &str) -> Option<&'static str> {
    for line in contents.lines() {
        let line = line.trim_start();
        if line.starts_with('#') {
            continue;
        }
        if line.contains("oh-my-posh init") {
            return Some("oh-my-posh");
        }
        if line.contains("starship init") {
            return Some("starship");
        }
        if line.contains("powerlevel10k") || line.contains("p10k") {
            return Some("powerlevel10k");
        }
    }
    None
}

/// Classify the managed integration block as auto-start or on-demand.
pub(super) fn detect_setup_mode(contents: &str) -> Option<&'static str> {
    let start = contents.find(crate::cli::integration::START)?;
    let end = contents.find(crate::cli::integration::END)?;
    if end < start {
        return None;
    }
    let block = &contents[start..end];
    if block.contains("alias hk=") {
        Some("on-demand (`hk` alias)")
    } else if block.contains("exec \"$__hokan_bin\"") {
        Some("auto-start (exec)")
    } else {
        None
    }
}

/// Scan rc file contents for plugin integrations known to conflict with
/// Hokan's overlay. Comment lines and lines already guarded with
/// `HOKAN_ACTIVE` are ignored; each plugin is reported once.
pub(super) fn scan_plugin_conflicts(path: &Path, contents: &str) -> Vec<Check> {
    let mut found: Vec<(&'static str, usize)> = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim_start();
        if line.starts_with('#') || line.contains("HOKAN_ACTIVE") {
            continue;
        }
        if let Some(plugin) = plugin_for_line(line)
            && !found.iter().any(|(name, _)| *name == plugin)
        {
            found.push((plugin, index + 1));
        }
    }
    found
        .into_iter()
        .map(|(plugin, line)| {
            Check::new(
                CheckLevel::Warn,
                format!(
                    "{plugin} detected at {}:{line}; guard it with `[[ -z $HOKAN_ACTIVE ]] && <plugin init line>` so it stays active in normal shells but inactive inside Hokan, or switch to on-demand mode (`hokan setup --shell zsh --on-demand`)",
                    path.display()
                ),
            )
        })
        .collect()
}

fn plugin_for_line(line: &str) -> Option<&'static str> {
    if line.contains("zsh-autosuggestions") {
        Some("zsh-autosuggestions")
    } else if line.contains("zsh-autocomplete") {
        Some("zsh-autocomplete")
    } else if line.contains("zsh-vi-mode") {
        Some("zsh-vi-mode")
    } else if line.contains("atuin init") {
        Some("atuin")
    } else if line.contains("fzf")
        && ["--zsh", ".fzf.zsh", "/shell/", "key-bindings", "completion"]
            .iter()
            .any(|needle| line.contains(needle))
    {
        Some("fzf shell integration")
    } else {
        None
    }
}
