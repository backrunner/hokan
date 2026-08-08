use std::{
    env, fs,
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use tempfile::NamedTempFile;

use crate::shell::{PROTOCOL_VERSION, ShellKind};

pub(crate) const START: &str = "# >>> hokan integration >>>";
pub(crate) const END: &str = "# <<< hokan integration <<<";
// Legacy markers written by the pre-rename `hokann` beta. New installs always
// use the `hokan` markers above, but install/uninstall must still recognize
// these so beta users who ran the old `hokann setup` are not stranded with an
// unremovable block.
const LEGACY_START: &str = "# >>> hokann integration >>>";
const LEGACY_END: &str = "# <<< hokann integration <<<";
const SHELL_RC_MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IntegrationTarget {
    pub shell: ShellKind,
    pub path: PathBuf,
}

#[cfg(test)]
pub fn install(
    output: &mut dyn Write,
    requested_shell: Option<ShellKind>,
    requested_path: Option<&Path>,
    on_demand: bool,
) -> crate::Result<IntegrationTarget> {
    install_with_executable(
        output,
        requested_shell,
        requested_path,
        on_demand,
        &env::current_exe()?,
    )
}

pub(crate) fn install_with_executable(
    output: &mut dyn Write,
    requested_shell: Option<ShellKind>,
    requested_path: Option<&Path>,
    on_demand: bool,
    executable: &Path,
) -> crate::Result<IntegrationTarget> {
    let shell = resolve_shell(requested_shell, requested_path)?;
    let requested_path = requested_path
        .map(Path::to_owned)
        .map_or_else(|| default_rc_path(shell), Ok)?;
    let path = resolve_rc_target(&requested_path)?;
    let original = read_optional_utf8(&path)?;
    let block = integration_block(shell, executable, &path, on_demand);
    let existing = block_range(&original)?;
    let action = if existing.is_some() {
        "updated"
    } else {
        "installed"
    };
    let mut updated = original.clone();
    if let Some(range) = existing {
        updated.replace_range(range, "");
    }
    // Auto-start must run before user configuration in every supported shell.
    // The Hokan child then loads the user's rc file once with HOKAN_ACTIVE set.
    updated.insert_str(0, &block);
    if updated == original {
        writeln!(output, "already installed: {}", path.display())?;
        return Ok(IntegrationTarget { shell, path });
    }
    let backup = backup_existing(&path)?;
    atomic_write(&path, updated.as_bytes())?;
    writeln!(output, "{action}: {}", path.display())?;
    if let Some(backup) = backup {
        writeln!(output, "backup: {}", backup.display())?;
    }
    if on_demand {
        writeln!(
            output,
            "on-demand mode: open a new shell and type `hk` to enter Hokan"
        )?;
    } else {
        writeln!(
            output,
            "tip: `hokan install --shell {} --on-demand` installs an `hk` command instead of auto-starting Hokan",
            shell.name()
        )?;
    }
    Ok(IntegrationTarget { shell, path })
}

pub fn uninstall(
    output: &mut dyn Write,
    requested_shell: Option<ShellKind>,
    requested_path: Option<&Path>,
) -> crate::Result<IntegrationTarget> {
    let shell = resolve_shell(requested_shell, requested_path)?;
    let requested_path = requested_path
        .map(Path::to_owned)
        .map_or_else(|| default_rc_path(shell), Ok)?;
    let path = resolve_rc_target(&requested_path)?;
    let original = read_optional_utf8(&path)?;
    let Some(range) = block_range(&original)? else {
        writeln!(output, "integration not present: {}", path.display())?;
        return Ok(IntegrationTarget { shell, path });
    };

    let mut updated = original;
    updated.replace_range(range, "");
    while updated.ends_with("\n\n\n") {
        updated.pop();
    }
    let backup = backup_existing(&path)?;
    atomic_write(&path, updated.as_bytes())?;
    writeln!(output, "removed integration: {}", path.display())?;
    if let Some(backup) = backup {
        writeln!(output, "backup: {}", backup.display())?;
    }
    Ok(IntegrationTarget { shell, path })
}

fn resolve_shell(
    requested: Option<ShellKind>,
    requested_path: Option<&Path>,
) -> crate::Result<ShellKind> {
    if let Some(shell) = requested {
        return Ok(shell);
    }
    if let Some(path) = requested_path
        && let Some(shell) = infer_shell(path)
    {
        return Ok(shell);
    }
    ShellKind::detect()
}

fn infer_shell(path: &Path) -> Option<ShellKind> {
    let name = path.file_name()?.to_str()?;
    if name.contains("zsh") {
        Some(ShellKind::Zsh)
    } else if name.contains("bash") {
        Some(ShellKind::Bash)
    } else if name == "config.fish" || name.contains("fish") {
        Some(ShellKind::Fish)
    } else {
        None
    }
}

fn default_rc_path(shell: ShellKind) -> crate::Result<PathBuf> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| crate::Error::Config("$HOME is not set".into()))?;
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let zdotdir = env::var_os("ZDOTDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.clone());
    Ok(default_rc_path_for(
        shell,
        &home,
        &config_home,
        &zdotdir,
        cfg!(target_os = "macos"),
    ))
}

fn default_rc_path_for(
    shell: ShellKind,
    home: &Path,
    config_home: &Path,
    zdotdir: &Path,
    macos: bool,
) -> PathBuf {
    match shell {
        ShellKind::Zsh => zdotdir.join(".zshrc"),
        ShellKind::Bash if macos => home.join(".bash_profile"),
        ShellKind::Bash => home.join(".bashrc"),
        ShellKind::Fish => config_home.join("fish/config.fish"),
    }
}

fn integration_block(
    shell: ShellKind,
    executable: &Path,
    rc_file: &Path,
    on_demand: bool,
) -> String {
    if on_demand {
        return on_demand_block(shell, executable, rc_file);
    }
    let executable = quote_shell_word(executable);
    let rc_file = quote_shell_word(rc_file);
    let command = match shell {
        ShellKind::Fish => format!(
            "begin\n\
               set -l __hokan_setup_bin {executable}\n\
               set -l __hokan_setup_dir (path dirname \"$__hokan_setup_bin\")\n\
               contains -- \"$__hokan_setup_dir\" $PATH; or set -gx PATH \"$__hokan_setup_dir\" $PATH\n\
               set -l __hokan_bin \"$__hokan_setup_bin\"\n\
               if set -q HOKAN_BIN; and test -n \"$HOKAN_BIN\"\n\
                 set __hokan_bin \"$HOKAN_BIN\"\n\
               end\n\
               set -l __hokan_active ''\n\
               set -q HOKAN_ACTIVE; and set __hokan_active \"$HOKAN_ACTIVE\"\n\
               set -l __hokan_auto_start 1\n\
               set -q HOKAN_AUTO_START; and set __hokan_auto_start \"$HOKAN_AUTO_START\"\n\
               set -l __hokan_term dumb\n\
               set -q TERM; and set __hokan_term \"$TERM\"\n\
               if test \"$__hokan_auto_start\" != 0\n\
                 and test -z \"$__hokan_active\"\n\
                 and status is-interactive\n\
                 and isatty stdin\n\
                 and isatty stdout\n\
                 and test \"$__hokan_term\" != dumb\n\
                 and test -x \"$__hokan_bin\"\n\
                 exec \"$__hokan_bin\" --shell fish\n\
               end\n\
             end"
        ),
        ShellKind::Zsh => format!(
            "__hokan_setup_bin={executable}\n\
             __hokan_setup_dir=${{__hokan_setup_bin:h}}\n\
             [[ -d \"$__hokan_setup_dir\" ]] && path=(\"$__hokan_setup_dir\" ${{path:#\"$__hokan_setup_dir\"}})\n\
             __hokan_bin=${{HOKAN_BIN:-$__hokan_setup_bin}}\n\
             if [[ ${{HOKAN_AUTO_START:-1}} != 0\n\
                   && -z ${{HOKAN_ACTIVE:-}}\n\
                   && -o interactive\n\
                   && -z ${{ZSH_EXECUTION_STRING:-}}\n\
                   && -t 0\n\
                   && -t 1\n\
                   && ${{TERM:-dumb}} != dumb\n\
                   && -x \"$__hokan_bin\" ]]; then\n\
               exec \"$__hokan_bin\" --shell zsh\n\
             fi\n\
             unset __hokan_bin __hokan_setup_dir __hokan_setup_bin"
        ),
        ShellKind::Bash => format!(
            "__hokan_setup_bin={executable}\n\
             __hokan_setup_rc={rc_file}\n\
             __hokan_setup_dir=${{__hokan_setup_bin%/*}}\n\
             case :$PATH: in\n\
               *:\"$__hokan_setup_dir\":*) ;;\n\
               *) export PATH=\"$__hokan_setup_dir:$PATH\" ;;\n\
             esac\n\
             __hokan_bin=${{HOKAN_BIN:-$__hokan_setup_bin}}\n\
             if [[ ${{HOKAN_AUTO_START:-1}} != 0\n\
                   && -z ${{HOKAN_ACTIVE:-}}\n\
                   && $- == *i*\n\
                   && -z ${{BASH_EXECUTION_STRING:-}}\n\
                   && -t 0\n\
                   && -t 1\n\
                   && ${{TERM:-dumb}} != dumb\n\
                   && -x \"$__hokan_bin\" ]]; then\n\
               export HOKAN_BASH_STARTUP_FILE=\"$__hokan_setup_rc\"\n\
               exec \"$__hokan_bin\" --shell bash\n\
             fi\n\
             unset __hokan_bin __hokan_setup_dir __hokan_setup_rc __hokan_setup_bin"
        ),
    };
    format!(
        "{START}\n# protocol {PROTOCOL_VERSION}; managed by `hokan install`\n{command}\n{END}\n"
    )
}

fn on_demand_block(shell: ShellKind, executable: &Path, rc_file: &Path) -> String {
    let executable = quote_shell_word(executable);
    let rc_file = quote_shell_word(rc_file);
    let command = match shell {
        ShellKind::Fish => format!(
            "begin\n\
               set -l __hokan_setup_bin {executable}\n\
               set -l __hokan_setup_dir (path dirname \"$__hokan_setup_bin\")\n\
               contains -- \"$__hokan_setup_dir\" $PATH; or set -gx PATH \"$__hokan_setup_dir\" $PATH\n\
               function hk --description 'Start Hokan'\n\
                 command {executable} --shell fish $argv\n\
               end\n\
             end"
        ),
        ShellKind::Zsh => format!(
            "__hokan_setup_bin={executable}\n\
             __hokan_setup_dir=${{__hokan_setup_bin%/*}}\n\
             case :$PATH: in\n\
               *:\"$__hokan_setup_dir\":*) ;;\n\
               *) export PATH=\"$__hokan_setup_dir:$PATH\" ;;\n\
             esac\n\
             hk() {{\n\
               command {executable} --shell zsh \"$@\"\n\
             }}\n\
             unset __hokan_setup_dir __hokan_setup_bin"
        ),
        ShellKind::Bash => format!(
            "__hokan_setup_bin={executable}\n\
             __hokan_setup_dir=${{__hokan_setup_bin%/*}}\n\
             case :$PATH: in\n\
               *:\"$__hokan_setup_dir\":*) ;;\n\
               *) export PATH=\"$__hokan_setup_dir:$PATH\" ;;\n\
             esac\n\
             hk() {{\n\
               HOKAN_BASH_STARTUP_FILE={rc_file} command {executable} --shell bash \"$@\"\n\
             }}\n\
             unset __hokan_setup_dir __hokan_setup_bin"
        ),
    };
    format!(
        "{START}\n# protocol {PROTOCOL_VERSION}; managed by `hokan install` (on-demand)\n{command}\n{END}\n"
    )
}

fn quote_shell_word(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn resolve_rc_target(path: &Path) -> crate::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::canonicalize(path).map_err(Into::into)
        }
        Ok(metadata) if metadata.file_type().is_file() => Ok(path.to_owned()),
        Ok(_) => Err(crate::Error::Config(format!(
            "refusing to modify non-file shell configuration {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path.to_owned()),
        Err(error) => Err(error.into()),
    }
}

fn read_optional_utf8(path: &Path) -> crate::Result<String> {
    match fs::File::open(path) {
        Ok(mut file) => {
            if file.metadata()?.len() > SHELL_RC_MAX_BYTES {
                return Err(crate::Error::Config(format!(
                    "refusing to modify shell configuration larger than 4 MiB: {}",
                    path.display()
                )));
            }
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(SHELL_RC_MAX_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > SHELL_RC_MAX_BYTES {
                return Err(crate::Error::Config(format!(
                    "refusing to modify shell configuration larger than 4 MiB: {}",
                    path.display()
                )));
            }
            String::from_utf8(bytes).map_err(|_| {
                crate::Error::Config(format!(
                    "refusing to modify non-UTF-8 shell configuration {}",
                    path.display()
                ))
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn block_range(contents: &str) -> crate::Result<Option<std::ops::Range<usize>>> {
    // Match both current and legacy (pre-rename `hokann`) marker pairs; the
    // two pairs are disjoint strings, so each index belongs to exactly one
    // variant. Any combination other than a single matched pair is rejected.
    let mut starts: Vec<(usize, &str)> = contents
        .match_indices(START)
        .map(|(index, _)| (index, START))
        .chain(
            contents
                .match_indices(LEGACY_START)
                .map(|(index, _)| (index, LEGACY_START)),
        )
        .collect();
    let mut ends: Vec<(usize, &str)> = contents
        .match_indices(END)
        .map(|(index, _)| (index, END))
        .chain(
            contents
                .match_indices(LEGACY_END)
                .map(|(index, _)| (index, LEGACY_END)),
        )
        .collect();
    starts.sort_unstable_by_key(|(index, _)| *index);
    ends.sort_unstable_by_key(|(index, _)| *index);
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        // A start/end pair must use the same marker variant; mixing current
        // and legacy markers counts as malformed.
        ([(start, start_marker)], [(end, end_marker)])
            if start < end && (start_marker == &START) == (end_marker == &END) =>
        {
            let mut range_start = *start;
            if range_start > 0 && contents.as_bytes().get(range_start - 1) == Some(&b'\n') {
                range_start -= 1;
            }
            let mut range_end = end + end_marker.len();
            if contents.as_bytes().get(range_end) == Some(&b'\n') {
                range_end += 1;
            }
            Ok(Some(range_start..range_end))
        }
        _ => Err(crate::Error::Config(
            "malformed or duplicate Hokan integration markers; refusing to modify the file".into(),
        )),
    }
}

fn backup_existing(path: &Path) -> crate::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut source = fs::File::open(path)?;
    let source_mode = source.metadata()?.permissions().mode();
    for suffix in 0_u32..10_000 {
        let extension = if suffix == 0 {
            "hokan.bak".to_owned()
        } else {
            format!("hokan.bak.{suffix}")
        };
        let candidate = path.with_extension(extension);
        let mut destination = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(destination) => destination,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        let copy_result = (|| -> io::Result<()> {
            io::copy(&mut source, &mut destination)?;
            destination.set_permissions(fs::Permissions::from_mode(source_mode))?;
            destination.sync_all()
        })();
        drop(destination);
        if let Err(error) = copy_result {
            let _ = fs::remove_file(&candidate);
            return Err(error.into());
        }
        if let Some(parent) = candidate.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        return Ok(Some(candidate));
    }
    Err(crate::Error::Config(format!(
        "could not allocate a backup name for {}",
        path.display()
    )))
}

fn atomic_write(path: &Path, contents: &[u8]) -> crate::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        crate::Error::Config(format!("{} has no parent directory", path.display()))
    })?;
    fs::create_dir_all(parent)?;
    let mode = fs::metadata(path)
        .map(|metadata| metadata.permissions().mode())
        .unwrap_or(0o600);
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.write_all(contents)?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent_and_uninstall_restores_user_content() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(".zshrc");
        fs::write(&path, "export USER_VALUE=1\n").expect("fixture");
        let mut output = Vec::new();
        install(&mut output, Some(ShellKind::Zsh), Some(&path), false).expect("install");
        let once = fs::read_to_string(&path).expect("installed file");
        assert!(once.starts_with(START));
        assert!(once.find(END) < once.find("export USER_VALUE=1"));
        assert!(once.contains("__hokan_setup_bin='"));
        assert!(!once.contains("init zsh"));
        assert!(once.contains("HOKAN_AUTO_START"));
        assert!(once.contains(r#"exec "$__hokan_bin" --shell zsh"#));
        install(&mut output, Some(ShellKind::Zsh), Some(&path), false).expect("idempotent install");
        assert_eq!(fs::read_to_string(&path).expect("same file"), once);
        uninstall(&mut output, Some(ShellKind::Zsh), Some(&path)).expect("uninstall");
        assert_eq!(
            fs::read_to_string(&path).expect("restored content"),
            "export USER_VALUE=1\n"
        );
        assert!(path.with_extension("hokan.bak").exists());
        assert!(path.with_extension("hokan.bak.1").exists());
    }

    #[test]
    fn install_upgrades_managed_blocks_and_preserves_symlinked_rc_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("dotfiles/zshrc");
        fs::create_dir(target.parent().expect("target parent")).expect("target directory");
        fs::write(
            &target,
            format!(
                "export USER_VALUE=1\n\n{START}\n# protocol 1; managed by `hokan setup`\nold integration\n{END}\n"
            ),
        )
        .expect("old integration");
        let path = directory.path().join(".zshrc");
        std::os::unix::fs::symlink(&target, &path).expect("rc symlink");

        let mut output = Vec::new();
        install(&mut output, Some(ShellKind::Zsh), Some(&path), false).expect("upgrade install");
        assert!(
            fs::symlink_metadata(&path)
                .expect("symlink")
                .file_type()
                .is_symlink()
        );
        let upgraded = fs::read_to_string(&target).expect("upgraded target");
        assert!(upgraded.contains("HOKAN_AUTO_START"));
        assert!(!upgraded.contains("old integration"));
        assert!(target.with_extension("hokan.bak").exists());
        assert!(
            String::from_utf8(output)
                .expect("output")
                .contains("updated:")
        );
    }

    #[test]
    fn install_never_follows_or_overwrites_an_existing_backup_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(".zshrc");
        fs::write(&path, "export USER_VALUE=1\n").expect("fixture");
        let first_backup = path.with_extension("hokan.bak");
        let symlink_target = directory.path().join("must-not-be-created");
        std::os::unix::fs::symlink(&symlink_target, &first_backup)
            .expect("dangling backup symlink");

        install(&mut Vec::new(), Some(ShellKind::Zsh), Some(&path), false).expect("install");

        assert!(
            fs::symlink_metadata(&first_backup)
                .expect("backup symlink")
                .file_type()
                .is_symlink()
        );
        assert!(!symlink_target.exists());
        assert!(path.with_extension("hokan.bak.1").is_file());
    }

    #[test]
    fn malformed_markers_are_never_modified() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("config.fish");
        fs::write(&path, format!("{START}\nmissing end\n")).expect("fixture");
        let before = fs::read(&path).expect("before");
        assert!(install(&mut Vec::new(), Some(ShellKind::Fish), Some(&path), false).is_err());
        assert_eq!(fs::read(&path).expect("after"), before);
    }

    #[test]
    fn uninstall_removes_legacy_hokann_marked_blocks() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(".zshrc");
        fs::write(
            &path,
            format!(
                "export USER_VALUE=1\n\n{LEGACY_START}\n# protocol 1; managed by `hokann setup`\nold integration\n{LEGACY_END}\n"
            ),
        )
        .expect("legacy integration fixture");

        let mut output = Vec::new();
        uninstall(&mut output, Some(ShellKind::Zsh), Some(&path)).expect("uninstall legacy");
        assert_eq!(
            fs::read_to_string(&path).expect("restored content"),
            "export USER_VALUE=1\n"
        );

        // `install` must also upgrade a legacy-marked block in place, replacing
        // the old `hokann` markers with the current ones.
        fs::write(
            &path,
            format!(
                "export USER_VALUE=1\n\n{LEGACY_START}\n# protocol 1; managed by `hokann setup`\nold integration\n{LEGACY_END}\n"
            ),
        )
        .expect("legacy integration fixture");
        install(&mut output, Some(ShellKind::Zsh), Some(&path), false)
            .expect("install over legacy");
        let upgraded = fs::read_to_string(&path).expect("upgraded content");
        assert!(upgraded.starts_with(START));
        assert!(!upgraded.contains(LEGACY_START));
        assert!(!upgraded.contains("old integration"));
    }

    #[test]
    fn mixed_current_and_legacy_markers_are_rejected() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(".zshrc");
        fs::write(&path, format!("{LEGACY_START}\nblock\n{END}\n")).expect("fixture");
        let before = fs::read(&path).expect("before");
        assert!(uninstall(&mut Vec::new(), Some(ShellKind::Zsh), Some(&path)).is_err());
        assert_eq!(fs::read(&path).expect("after"), before);
    }

    #[test]
    fn on_demand_install_installs_command_only_block() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(".zshrc");
        fs::write(&path, "export USER_VALUE=1\n").expect("fixture");

        let mut output = Vec::new();
        install(&mut output, Some(ShellKind::Zsh), Some(&path), true).expect("on-demand install");
        let installed = fs::read_to_string(&path).expect("installed file");
        assert!(installed.starts_with(START));
        let block = &installed[..installed.find(END).expect("end marker") + END.len()];
        assert!(block.contains("(on-demand)"));
        assert!(block.contains("hk() {"));
        assert!(block.contains("--shell zsh"));
        assert!(!block.contains("exec"));
        assert!(!block.contains("HOKAN_AUTO_START"));
        // User content below the block is untouched.
        assert!(installed.ends_with("export USER_VALUE=1\n"));
        let output = String::from_utf8(output).expect("output");
        assert!(output.contains("type `hk`"));

        // Uninstall removes the on-demand block via the same markers.
        uninstall(&mut Vec::new(), Some(ShellKind::Zsh), Some(&path)).expect("uninstall");
        assert_eq!(
            fs::read_to_string(&path).expect("restored content"),
            "export USER_VALUE=1\n"
        );
    }

    #[test]
    fn install_switches_between_auto_exec_and_on_demand_modes() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(".zshrc");
        fs::write(&path, "export USER_VALUE=1\n").expect("fixture");
        let mut output = Vec::new();

        install(&mut output, Some(ShellKind::Zsh), Some(&path), false).expect("auto-exec install");
        let auto_exec = fs::read_to_string(&path).expect("auto-exec file");
        assert!(auto_exec.contains(r#"exec "$__hokan_bin" --shell zsh"#));
        assert!(!auto_exec.contains("hk() {"));

        // Downgrade to on-demand: the managed block is replaced, user content kept.
        install(&mut output, Some(ShellKind::Zsh), Some(&path), true).expect("on-demand install");
        let on_demand = fs::read_to_string(&path).expect("on-demand file");
        assert!(on_demand.contains("hk() {"));
        assert!(!on_demand.contains("exec \"$__hokan_bin\""));
        assert!(on_demand.ends_with("export USER_VALUE=1\n"));

        // Upgrade back to auto-exec.
        install(&mut output, Some(ShellKind::Zsh), Some(&path), false).expect("auto-exec again");
        assert_eq!(
            fs::read_to_string(&path).expect("auto-exec restored"),
            auto_exec
        );
    }

    #[test]
    fn on_demand_install_supports_bash_and_fish() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let bashrc = directory.path().join(".bashrc");
        fs::write(&bashrc, "export USER_VALUE=1\n").expect("fixture");
        install(&mut Vec::new(), Some(ShellKind::Bash), Some(&bashrc), true)
            .expect("bash on-demand install");
        let installed = fs::read_to_string(&bashrc).expect("installed bashrc");
        assert!(installed.contains("hk() {"));
        assert!(installed.contains("--shell bash"));
        assert!(installed.contains("HOKAN_BASH_STARTUP_FILE="));

        let fishrc = directory.path().join("config.fish");
        fs::write(&fishrc, "set USER_VALUE 1\n").expect("fixture");
        install(&mut Vec::new(), Some(ShellKind::Fish), Some(&fishrc), true)
            .expect("fish on-demand install");
        let installed = fs::read_to_string(&fishrc).expect("installed fish config");
        assert!(installed.starts_with(START));
        assert!(installed.contains("function hk"));
        assert!(installed.contains("--shell fish $argv"));
        assert!(installed.ends_with("set USER_VALUE 1\n"));
    }

    #[test]
    fn auto_start_is_installed_before_user_config_for_every_shell() {
        let directory = tempfile::tempdir().expect("temporary directory");
        for (shell, name, expected) in [
            (ShellKind::Zsh, ".zshrc", "--shell zsh"),
            (ShellKind::Bash, ".bashrc", "--shell bash"),
            (ShellKind::Fish, "config.fish", "--shell fish"),
        ] {
            let path = directory.path().join(name);
            fs::write(&path, "USER_CONFIG_SENTINEL\n").expect("fixture");
            install(&mut Vec::new(), Some(shell), Some(&path), false).expect("install");
            let installed = fs::read_to_string(&path).expect("installed config");
            assert!(installed.starts_with(START));
            assert!(installed.contains(expected));
            assert!(installed.contains("HOKAN_ACTIVE"));
            if shell == ShellKind::Bash {
                assert!(installed.contains("HOKAN_BASH_STARTUP_FILE="));
                assert!(
                    installed.contains(path.to_string_lossy().as_ref()),
                    "expected rc path {} in {installed:?}",
                    path.display()
                );
            }
            assert!(installed.ends_with("USER_CONFIG_SENTINEL\n"));
        }
    }

    #[test]
    fn default_paths_cover_macos_linux_zdotdir_and_xdg_fish() {
        let home = Path::new("/home/tester");
        let config = Path::new("/xdg/config");
        let zdotdir = Path::new("/dotfiles/zsh");
        assert_eq!(
            default_rc_path_for(ShellKind::Zsh, home, config, zdotdir, false),
            zdotdir.join(".zshrc")
        );
        assert_eq!(
            default_rc_path_for(ShellKind::Bash, home, config, zdotdir, false),
            home.join(".bashrc")
        );
        assert_eq!(
            default_rc_path_for(ShellKind::Bash, home, config, zdotdir, true),
            home.join(".bash_profile")
        );
        assert_eq!(
            default_rc_path_for(ShellKind::Fish, home, config, zdotdir, false),
            config.join("fish/config.fish")
        );
    }
}
