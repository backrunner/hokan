use std::{
    env, fs,
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use tempfile::NamedTempFile;

use crate::shell::{PROTOCOL_VERSION, ShellKind};

const START: &str = "# >>> hokann integration >>>";
const END: &str = "# <<< hokann integration <<<";
const SHELL_RC_MAX_BYTES: u64 = 4 * 1024 * 1024;

pub fn setup(
    output: &mut dyn Write,
    requested_shell: Option<ShellKind>,
    requested_path: Option<&Path>,
) -> crate::Result<()> {
    let shell = resolve_shell(requested_shell, requested_path)?;
    let requested_path = requested_path
        .map(Path::to_owned)
        .map_or_else(|| default_rc_path(shell), Ok)?;
    let path = resolve_rc_target(&requested_path)?;
    let original = read_optional_utf8(&path)?;
    let block = integration_block(shell, &env::current_exe()?);
    let existing = block_range(&original)?;
    let action = if existing.is_some() {
        "updated"
    } else {
        "installed"
    };
    let mut updated = original.clone();
    if shell == ShellKind::Zsh {
        if let Some(range) = existing {
            updated.replace_range(range, "");
        }
        updated.insert_str(0, &block);
    } else if let Some(range) = existing {
        let replacement = if updated[range.clone()].starts_with('\n') {
            format!("\n{block}")
        } else {
            block
        };
        updated.replace_range(range, &replacement);
    } else {
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        if !updated.is_empty() {
            updated.push('\n');
        }
        updated.push_str(&block);
    }
    if updated == original {
        writeln!(output, "already installed: {}", path.display())?;
        return Ok(());
    }
    let backup = backup_existing(&path)?;
    atomic_write(&path, updated.as_bytes())?;
    writeln!(output, "{action}: {}", path.display())?;
    if let Some(backup) = backup {
        writeln!(output, "backup: {}", backup.display())?;
    }
    Ok(())
}

pub fn uninstall(
    output: &mut dyn Write,
    requested_shell: Option<ShellKind>,
    requested_path: Option<&Path>,
) -> crate::Result<()> {
    let shell = resolve_shell(requested_shell, requested_path)?;
    let requested_path = requested_path
        .map(Path::to_owned)
        .map_or_else(|| default_rc_path(shell), Ok)?;
    let path = resolve_rc_target(&requested_path)?;
    let original = read_optional_utf8(&path)?;
    let Some(range) = block_range(&original)? else {
        writeln!(output, "integration not present: {}", path.display())?;
        return Ok(());
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
    Ok(())
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
    Ok(match shell {
        ShellKind::Zsh => home.join(".zshrc"),
        ShellKind::Bash => home.join(".bashrc"),
        ShellKind::Fish => env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("fish/config.fish"),
    })
}

fn integration_block(shell: ShellKind, executable: &Path) -> String {
    let executable = quote_shell_word(executable);
    let command = match shell {
        ShellKind::Fish => format!(
            "set -l __hokann_setup_bin {executable}\n\
             if set -q HOKANN_BIN\n\
               command \"$HOKANN_BIN\" init fish | source\n\
             else\n\
               command \"$__hokann_setup_bin\" init fish | source\n\
             end"
        ),
        ShellKind::Zsh => format!(
            "__hokann_setup_bin={executable}\n\
             __hokann_bin=${{HOKANN_BIN:-$__hokann_setup_bin}}\n\
             if [[ ${{HOKANN_AUTO_START:-1}} != 0\n\
                   && -z ${{HOKANN_ACTIVE:-}}\n\
                   && -o interactive\n\
                   && -z ${{ZSH_EXECUTION_STRING:-}}\n\
                   && -t 0\n\
                   && -t 1\n\
                   && ${{TERM:-dumb}} != dumb\n\
                   && -x \"$__hokann_bin\" ]]; then\n\
               exec \"$__hokann_bin\" --shell zsh\n\
             fi\n\
             unset __hokann_bin __hokann_setup_bin"
        ),
        ShellKind::Bash => format!(
            "__hokann_setup_bin={executable}\n\
             eval \"$(\"${{HOKANN_BIN:-$__hokann_setup_bin}}\" init bash)\"\n\
             unset __hokann_setup_bin"
        ),
    };
    format!("{START}\n# protocol {PROTOCOL_VERSION}; managed by `hokann setup`\n{command}\n{END}\n")
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
    let starts: Vec<_> = contents
        .match_indices(START)
        .map(|(index, _)| index)
        .collect();
    let ends: Vec<_> = contents
        .match_indices(END)
        .map(|(index, _)| index)
        .collect();
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([start], [end]) if start < end => {
            let mut range_start = *start;
            if range_start > 0 && contents.as_bytes().get(range_start - 1) == Some(&b'\n') {
                range_start -= 1;
            }
            let mut range_end = end + END.len();
            if contents.as_bytes().get(range_end) == Some(&b'\n') {
                range_end += 1;
            }
            Ok(Some(range_start..range_end))
        }
        _ => Err(crate::Error::Config(
            "malformed or duplicate Hokann integration markers; refusing to modify the file".into(),
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
            "hokann.bak".to_owned()
        } else {
            format!("hokann.bak.{suffix}")
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
    fn setup_is_idempotent_and_uninstall_restores_user_content() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(".zshrc");
        fs::write(&path, "export USER_VALUE=1\n").expect("fixture");
        let mut output = Vec::new();
        setup(&mut output, Some(ShellKind::Zsh), Some(&path)).expect("setup");
        let once = fs::read_to_string(&path).expect("installed file");
        assert!(once.starts_with(START));
        assert!(once.find(END) < once.find("export USER_VALUE=1"));
        assert!(once.contains("__hokann_setup_bin='"));
        assert!(!once.contains("init zsh"));
        assert!(once.contains("HOKANN_AUTO_START"));
        assert!(once.contains(r#"exec "$__hokann_bin" --shell zsh"#));
        setup(&mut output, Some(ShellKind::Zsh), Some(&path)).expect("idempotent setup");
        assert_eq!(fs::read_to_string(&path).expect("same file"), once);
        uninstall(&mut output, Some(ShellKind::Zsh), Some(&path)).expect("uninstall");
        assert_eq!(
            fs::read_to_string(&path).expect("restored content"),
            "export USER_VALUE=1\n"
        );
        assert!(path.with_extension("hokann.bak").exists());
        assert!(path.with_extension("hokann.bak.1").exists());
    }

    #[test]
    fn setup_upgrades_managed_blocks_and_preserves_symlinked_rc_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("dotfiles/zshrc");
        fs::create_dir(target.parent().expect("target parent")).expect("target directory");
        fs::write(
            &target,
            format!(
                "export USER_VALUE=1\n\n{START}\n# protocol 1; managed by `hokann setup`\nold integration\n{END}\n"
            ),
        )
        .expect("old integration");
        let path = directory.path().join(".zshrc");
        std::os::unix::fs::symlink(&target, &path).expect("rc symlink");

        let mut output = Vec::new();
        setup(&mut output, Some(ShellKind::Zsh), Some(&path)).expect("upgrade setup");
        assert!(
            fs::symlink_metadata(&path)
                .expect("symlink")
                .file_type()
                .is_symlink()
        );
        let upgraded = fs::read_to_string(&target).expect("upgraded target");
        assert!(upgraded.contains("HOKANN_AUTO_START"));
        assert!(!upgraded.contains("old integration"));
        assert!(target.with_extension("hokann.bak").exists());
        assert!(
            String::from_utf8(output)
                .expect("output")
                .contains("updated:")
        );
    }

    #[test]
    fn setup_never_follows_or_overwrites_an_existing_backup_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join(".zshrc");
        fs::write(&path, "export USER_VALUE=1\n").expect("fixture");
        let first_backup = path.with_extension("hokann.bak");
        let symlink_target = directory.path().join("must-not-be-created");
        std::os::unix::fs::symlink(&symlink_target, &first_backup)
            .expect("dangling backup symlink");

        setup(&mut Vec::new(), Some(ShellKind::Zsh), Some(&path)).expect("setup");

        assert!(
            fs::symlink_metadata(&first_backup)
                .expect("backup symlink")
                .file_type()
                .is_symlink()
        );
        assert!(!symlink_target.exists());
        assert!(path.with_extension("hokann.bak.1").is_file());
    }

    #[test]
    fn malformed_markers_are_never_modified() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("config.fish");
        fs::write(&path, format!("{START}\nmissing end\n")).expect("fixture");
        let before = fs::read(&path).expect("before");
        assert!(setup(&mut Vec::new(), Some(ShellKind::Fish), Some(&path)).is_err());
        assert_eq!(fs::read(&path).expect("after"), before);
    }
}
