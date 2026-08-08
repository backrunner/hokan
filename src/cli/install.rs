use std::{
    env, fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use super::integration::{self, IntegrationTarget};
use crate::shell::ShellKind;

const RECEIPT_FILE: &str = ".hokan-install.toml";
const RECEIPT_VERSION: u8 = 1;
const RECEIPT_MAX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ReceiptTarget {
    shell: ShellKind,
    path: PathBuf,
}

impl From<IntegrationTarget> for ReceiptTarget {
    fn from(target: IntegrationTarget) -> Self {
        Self {
            shell: target.shell,
            path: target.path,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct InstallReceipt {
    version: u8,
    binary: PathBuf,
    man_page: Option<PathBuf>,
    integrations: Vec<ReceiptTarget>,
}

pub fn run_install(
    output: &mut dyn Write,
    shell: Option<ShellKind>,
    rc_file: Option<&Path>,
    on_demand: bool,
    managed_install: bool,
    man_page: Option<&Path>,
) -> crate::Result<()> {
    let executable = env::current_exe()?;
    run_install_with_executable(
        output,
        shell,
        rc_file,
        on_demand,
        managed_install,
        man_page,
        &executable,
    )
}

fn run_install_with_executable(
    output: &mut dyn Write,
    shell: Option<ShellKind>,
    rc_file: Option<&Path>,
    on_demand: bool,
    managed_install: bool,
    man_page: Option<&Path>,
    executable: &Path,
) -> crate::Result<()> {
    let receipt_path = receipt_path(executable)?;
    let existing = read_receipt(&receipt_path)?;
    if let Some(receipt) = &existing {
        validate_receipt(receipt, executable)?;
    }
    if let Some(path) = man_page {
        validate_man_page_path(path)?;
    }
    let target =
        integration::install_with_executable(output, shell, rc_file, on_demand, executable)?;
    if managed_install || existing.is_some() {
        let mut receipt = existing.unwrap_or_else(|| InstallReceipt {
            version: RECEIPT_VERSION,
            binary: executable.to_owned(),
            man_page: None,
            integrations: Vec::new(),
        });
        if let Some(path) = man_page {
            receipt.man_page = Some(path.to_owned());
        }
        validate_receipt(&receipt, executable)?;
        let target = ReceiptTarget::from(target);
        receipt
            .integrations
            .retain(|existing| existing.path != target.path);
        receipt.integrations.push(target);
        write_receipt(&receipt_path, &receipt)?;
    }
    writeln!(output, "ready: open a new terminal session to start Hokan")?;
    Ok(())
}

pub fn run_uninstall(
    output: &mut dyn Write,
    shell: Option<ShellKind>,
    rc_file: Option<&Path>,
    integration_only: bool,
) -> crate::Result<()> {
    let executable = env::current_exe()?;
    run_uninstall_with_executable(output, shell, rc_file, integration_only, &executable)
}

fn run_uninstall_with_executable(
    output: &mut dyn Write,
    shell: Option<ShellKind>,
    rc_file: Option<&Path>,
    integration_only: bool,
    executable: &Path,
) -> crate::Result<()> {
    let receipt_path = receipt_path(executable)?;
    let mut receipt = read_receipt(&receipt_path)?;
    if let Some(installed) = &receipt {
        validate_receipt(installed, executable)?;
        validate_owned_binary(executable)?;
    }

    let targeted = shell.is_some() || rc_file.is_some();
    let removed = if !targeted && let Some(installed) = &receipt {
        let mut removed = Vec::with_capacity(installed.integrations.len());
        for target in &installed.integrations {
            let target =
                integration::uninstall(output, Some(target.shell), Some(target.path.as_path()))?;
            removed.push(ReceiptTarget::from(target));
        }
        removed
    } else {
        vec![ReceiptTarget::from(integration::uninstall(
            output, shell, rc_file,
        )?)]
    };

    let remove_managed_files = receipt.is_some() && !integration_only && !targeted;
    if remove_managed_files {
        let installed = receipt.as_ref().ok_or_else(|| {
            crate::Error::Config("managed uninstall lost its install receipt".into())
        })?;
        if let Some(man_page) = &installed.man_page
            && remove_man_page(man_page)?
        {
            writeln!(output, "removed man page: {}", man_page.display())?;
        }
        let backup = executable_backup(executable);
        if remove_optional_file(&backup)? {
            writeln!(output, "removed update backup: {}", backup.display())?;
        }
        remove_owned_binary(executable)?;
        writeln!(output, "removed binary: {}", executable.display())?;
        remove_optional_file(&receipt_path)?;
        writeln!(
            output,
            "configuration and history were preserved; remove them manually only if you no longer need them"
        )?;
        return Ok(());
    }

    if let Some(installed) = receipt.as_mut() {
        installed.integrations.retain(|target| {
            !removed
                .iter()
                .any(|removed| removed.shell == target.shell && removed.path == target.path)
        });
        write_receipt(&receipt_path, installed)?;
    }
    writeln!(output, "binary preserved: {}", executable.display())?;
    if receipt.is_none() && !integration_only {
        if executable
            .components()
            .any(|component| component.as_os_str() == ".cargo")
        {
            writeln!(
                output,
                "remove the Cargo installation with: cargo uninstall hokan"
            )?;
        } else {
            writeln!(
                output,
                "remove the binary with the package manager or installation method that placed it there"
            )?;
        }
    } else if receipt.is_some() && targeted && !integration_only {
        writeln!(
            output,
            "run `hokan uninstall` without --shell or --rc-file to remove installer-managed files"
        )?;
    }
    writeln!(output, "configuration and history were preserved")?;
    Ok(())
}

fn receipt_path(executable: &Path) -> crate::Result<PathBuf> {
    let parent = executable.parent().ok_or_else(|| {
        crate::Error::Config(format!(
            "cannot determine the install directory for {}",
            executable.display()
        ))
    })?;
    Ok(parent.join(RECEIPT_FILE))
}

fn read_receipt(path: &Path) -> crate::Result<Option<InstallReceipt>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > RECEIPT_MAX_BYTES
    {
        return Err(crate::Error::Config(format!(
            "refusing unsafe install receipt {}",
            path.display()
        )));
    }
    let mut file = fs::File::open(path)?;
    let mut contents = String::new();
    Read::by_ref(&mut file)
        .take(RECEIPT_MAX_BYTES + 1)
        .read_to_string(&mut contents)?;
    if contents.len() as u64 > RECEIPT_MAX_BYTES {
        return Err(crate::Error::Config(format!(
            "install receipt is too large: {}",
            path.display()
        )));
    }
    let receipt: InstallReceipt = toml::from_str(&contents).map_err(|error| {
        crate::Error::Config(format!(
            "invalid install receipt {}: {error}",
            path.display()
        ))
    })?;
    Ok(Some(receipt))
}

fn validate_receipt(receipt: &InstallReceipt, executable: &Path) -> crate::Result<()> {
    if receipt.version != RECEIPT_VERSION || !same_file_path(&receipt.binary, executable) {
        return Err(crate::Error::Config(format!(
            "install receipt does not belong to {}",
            executable.display()
        )));
    }
    if let Some(man_page) = &receipt.man_page {
        validate_man_page_path(man_page)?;
    }
    Ok(())
}

fn same_file_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn write_receipt(path: &Path, receipt: &InstallReceipt) -> crate::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        crate::Error::Config(format!("{} has no parent directory", path.display()))
    })?;
    fs::create_dir_all(parent)?;
    let contents = toml::to_string_pretty(receipt)
        .map_err(|error| crate::Error::Config(format!("cannot encode install receipt: {error}")))?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn remove_man_page(path: &Path) -> crate::Result<bool> {
    validate_man_page_path(path)?;
    remove_optional_file(path)
}

fn validate_man_page_path(path: &Path) -> crate::Result<()> {
    if path.file_name().and_then(|name| name.to_str()) == Some("hokan.1") {
        return Ok(());
    }
    Err(crate::Error::Config(format!(
        "refusing to remove unexpected man page path {}",
        path.display()
    )))
}

fn remove_owned_binary(path: &Path) -> crate::Result<()> {
    validate_owned_binary(path)?;
    fs::remove_file(path)?;
    Ok(())
}

fn validate_owned_binary(path: &Path) -> crate::Result<()> {
    if path.file_name().and_then(|name| name.to_str()) != Some("hokan") {
        return Err(crate::Error::Config(format!(
            "refusing to remove unexpected executable path {}",
            path.display()
        )));
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.uid() != nix::unistd::geteuid().as_raw() {
        return Err(crate::Error::Config(format!(
            "refusing to remove an unowned or non-file executable {}",
            path.display()
        )));
    }
    Ok(())
}

fn executable_backup(executable: &Path) -> PathBuf {
    let mut backup = executable.as_os_str().to_owned();
    backup.push(".bak");
    PathBuf::from(backup)
}

fn remove_optional_file(path: &Path) -> crate::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_stub_executable(path: &Path) {
        fs::create_dir_all(path.parent().expect("binary parent")).expect("binary directory");
        fs::write(path, "#!/bin/sh\nexit 0\n").expect("stub binary");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("executable mode");
    }

    #[test]
    fn managed_install_receipt_enables_complete_uninstall() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let executable = directory.path().join("bin/hokan");
        let man_page = directory.path().join("share/man/man1/hokan.1");
        let rc_file = directory.path().join("home/.zshrc");
        write_stub_executable(&executable);
        fs::create_dir_all(man_page.parent().expect("man parent")).expect("man directory");
        fs::write(&man_page, "manual").expect("man page");
        fs::create_dir_all(rc_file.parent().expect("rc parent")).expect("home");
        fs::write(&rc_file, "export USER_VALUE=1\n").expect("rc file");

        run_install_with_executable(
            &mut Vec::new(),
            Some(ShellKind::Zsh),
            Some(&rc_file),
            false,
            true,
            Some(&man_page),
            &executable,
        )
        .expect("managed install");
        assert!(receipt_path(&executable).expect("receipt path").is_file());
        let backup = executable_backup(&executable);
        fs::write(&backup, "older Hokan").expect("update backup");

        let mut output = Vec::new();
        run_uninstall_with_executable(&mut output, None, None, false, &executable)
            .expect("complete uninstall");
        assert!(!executable.exists());
        assert!(!backup.exists());
        assert!(!man_page.exists());
        assert!(!receipt_path(&executable).expect("receipt path").exists());
        assert_eq!(
            fs::read_to_string(&rc_file).expect("restored rc"),
            "export USER_VALUE=1\n"
        );
        let output = String::from_utf8(output).expect("UTF-8 output");
        assert!(output.contains("removed binary"));
        assert!(output.contains("configuration and history were preserved"));
    }

    #[test]
    fn integration_only_keeps_managed_files_and_receipt() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let executable = directory.path().join("bin/hokan");
        let man_page = directory.path().join("share/man/man1/hokan.1");
        let rc_file = directory.path().join("home/.bashrc");
        write_stub_executable(&executable);
        fs::create_dir_all(man_page.parent().expect("man parent")).expect("man directory");
        fs::write(&man_page, "manual").expect("man page");

        run_install_with_executable(
            &mut Vec::new(),
            Some(ShellKind::Bash),
            Some(&rc_file),
            false,
            true,
            Some(&man_page),
            &executable,
        )
        .expect("managed install");
        run_uninstall_with_executable(&mut Vec::new(), None, None, true, &executable)
            .expect("integration-only uninstall");

        assert!(executable.exists());
        assert!(man_page.exists());
        let receipt = read_receipt(&receipt_path(&executable).expect("receipt path"))
            .expect("read receipt")
            .expect("receipt");
        assert!(receipt.integrations.is_empty());
    }

    #[test]
    fn unmanaged_uninstall_never_deletes_the_binary() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let executable = directory.path().join("bin/hokan");
        let rc_file = directory.path().join("home/config.fish");
        write_stub_executable(&executable);

        run_install_with_executable(
            &mut Vec::new(),
            Some(ShellKind::Fish),
            Some(&rc_file),
            false,
            false,
            None,
            &executable,
        )
        .expect("unmanaged install");
        let mut output = Vec::new();
        run_uninstall_with_executable(
            &mut output,
            Some(ShellKind::Fish),
            Some(&rc_file),
            false,
            &executable,
        )
        .expect("unmanaged uninstall");

        assert!(executable.exists());
        assert!(
            String::from_utf8(output)
                .expect("UTF-8 output")
                .contains("binary preserved")
        );
    }

    #[test]
    fn managed_install_rejects_an_unexpected_man_page_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let executable = directory.path().join("bin/hokan");
        let unrelated = directory.path().join("share/keep-me.txt");
        let rc_file = directory.path().join("home/.zshrc");
        write_stub_executable(&executable);
        fs::create_dir_all(unrelated.parent().expect("unrelated parent"))
            .expect("unrelated directory");
        fs::write(&unrelated, "keep").expect("unrelated file");

        let error = run_install_with_executable(
            &mut Vec::new(),
            Some(ShellKind::Zsh),
            Some(&rc_file),
            false,
            true,
            Some(&unrelated),
            &executable,
        )
        .expect_err("unexpected man path must be rejected");

        assert!(error.to_string().contains("unexpected man page path"));
        assert!(unrelated.exists());
        assert!(executable.exists());
        assert!(!rc_file.exists());
        assert!(!receipt_path(&executable).expect("receipt path").exists());
    }
}
