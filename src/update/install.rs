//! Download, verify, smoke-test, and atomically install a release binary.
//!
//! The sequence is deliberately ordered so a failure at any step leaves the
//! current executable untouched: download → SHA256 check against the
//! published SHA256SUMS → extract → smoke test → writability probe →
//! `{exe}.bak` backup → atomic rename.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use semver::Version;
use sha2::{Digest, Sha256};

use super::{
    UpdateError, UpgradeOutcome, UpgradePaths,
    api::{self, ReleaseInfo},
};

/// Release archives are a few MiB; cap downloads so a broken server cannot
/// make us buffer unbounded data.
const DOWNLOAD_MAX_BYTES: usize = 64 * 1024 * 1024;
const CHECKSUMS_MAX_BYTES: usize = 1024 * 1024;
const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);
const SMOKE_MAX_OUTPUT: usize = 4 * 1024;

pub(crate) fn download_and_install(
    release: &ReleaseInfo,
    paths: &UpgradePaths,
    current_version: &Version,
) -> Result<UpgradeOutcome, UpdateError> {
    let downloads = paths.cache_dir.join("downloads");
    fs::create_dir_all(&downloads)?;
    let archive_name = api::archive_name(&release.version)?;
    let target = api::target_triple().ok_or(UpdateError::UnsupportedPlatform)?;
    let pid = std::process::id();
    let archive_tmp = downloads.join(format!("{archive_name}.tmp-{pid}"));
    let staged = downloads.join(format!("hokan-{}-{target}.tmp", release.version));
    // Removed on drop; a successful rename makes the staged path vanish and
    // remove_file on a missing path is ignored anyway.
    let temp = TempFiles(vec![archive_tmp.clone(), staged.clone()]);

    let client = api::download_client()?;
    let archive = api::block_on(api::download(
        &client,
        &release.archive_url,
        DOWNLOAD_MAX_BYTES,
    ))??;
    let mut archive_file = fs::File::create(&archive_tmp)?;
    archive_file.write_all(&archive)?;
    archive_file.sync_all()?;

    let checksums = api::block_on(api::download(
        &client,
        &release.checksums_url,
        CHECKSUMS_MAX_BYTES,
    ))??;
    let checksums = String::from_utf8(checksums).map_err(|_| UpdateError::InvalidResponse)?;
    let expected =
        expected_sha256(&checksums, &archive_name).ok_or(UpdateError::InvalidResponse)?;
    let actual = format!("{:x}", Sha256::digest(&archive));
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(UpdateError::ChecksumMismatch);
    }

    extract_binary(&archive, &staged)?;
    smoke_test(&staged, &release.version)?;

    let parent = paths
        .current_exe
        .parent()
        .map_or_else(|| paths.current_exe.clone(), Path::to_path_buf);
    if !directory_writable(&parent) {
        return Ok(UpgradeOutcome::NotWritable { path: parent });
    }

    let mut backup_name = paths.current_exe.as_os_str().to_owned();
    backup_name.push(".bak");
    let backup = PathBuf::from(backup_name);
    fs::copy(&paths.current_exe, &backup)?;
    fs::File::open(&backup)?.sync_all()?;
    fs::rename(&staged, &paths.current_exe)?;
    if let Ok(directory) = fs::File::open(&parent) {
        let _ = directory.sync_all();
    }
    drop(temp);
    Ok(UpgradeOutcome::Upgraded {
        from: current_version.clone(),
        to: release.version.clone(),
    })
}

/// Finds the expected hex digest for `archive_name` in a SHA256SUMS file.
fn expected_sha256(checksums: &str, archive_name: &str) -> Option<String> {
    checksums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?;
        (name.trim_start_matches('*') == archive_name).then(|| hash.to_owned())
    })
}

/// Extracts `bin/hokan` from the tar.gz archive to `staged`, mode 0755.
fn extract_binary(archive: &[u8], staged: &Path) -> Result<(), UpdateError> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    let mut binary = None;
    for entry in tar.entries().map_err(|_| UpdateError::InvalidResponse)? {
        let mut entry = entry.map_err(|_| UpdateError::InvalidResponse)?;
        let path = entry
            .path()
            .map_err(|_| UpdateError::InvalidResponse)?
            .into_owned();
        if path == Path::new("bin/hokan") {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|_| UpdateError::InvalidResponse)?;
            binary = Some(bytes);
            break;
        }
    }
    let bytes = binary.ok_or(UpdateError::InvalidResponse)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(staged)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

/// The staged binary must run and report the version we think we downloaded.
fn smoke_test(staged: &Path, version: &Version) -> Result<(), UpdateError> {
    let program = staged.to_str().ok_or(UpdateError::InvalidResponse)?;
    let output =
        crate::platform::run_bounded(program, ["--version"], SMOKE_TIMEOUT, SMOKE_MAX_OUTPUT)
            .map_err(|_| UpdateError::SmokeTest)?;
    let expected = version.to_string();
    if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(&expected) {
        Ok(())
    } else {
        Err(UpdateError::SmokeTest)
    }
}

/// Probe-writes the executable's directory: package-manager installs live
/// in system paths we must not touch. Also used by `hokan doctor`.
pub(crate) fn directory_writable(directory: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".hokan-write-probe.")
        .tempfile_in(directory)
        .is_ok()
}

struct TempFiles(Vec<PathBuf>);

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::test_support::{
        archive_asset, build_archive, serve_release, serve_release_with, sha256sums_for,
        write_stub_binary,
    };

    fn upgrade_paths(root: &Path, base: &str) -> (UpgradePaths, PathBuf) {
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("bin dir");
        let current_exe = bin.join("hokan");
        write_stub_binary(&current_exe, "#!/bin/sh\necho hokan 0.1.0\n");
        (
            UpgradePaths {
                current_exe: current_exe.clone(),
                state_dir: root.join("state"),
                cache_dir: root.join("cache"),
                api_base: base.to_owned(),
                repo: "backrunner/hokan".to_owned(),
            },
            current_exe,
        )
    }

    fn release(base: &str, version: &str) -> ReleaseInfo {
        ReleaseInfo {
            version: Version::parse(version).expect("version"),
            tag: format!("v{version}"),
            archive_url: format!("{base}/download/{}", archive_asset(version)),
            checksums_url: format!("{base}/download/SHA256SUMS"),
        }
    }

    #[test]
    fn full_upgrade_replaces_exe_and_writes_backup() {
        let root = tempfile::tempdir().expect("tempdir");
        let new_binary = "#!/bin/sh\necho hokan 9.9.9\n";
        let (base, join) = serve_release("9.9.9", build_archive(new_binary));
        let (paths, current_exe) = upgrade_paths(root.path(), &base);
        let old_bytes = fs::read(&current_exe).expect("old exe");

        let current = Version::parse("0.1.0").expect("current");
        let outcome =
            download_and_install(&release(&base, "9.9.9"), &paths, &current).expect("upgrade");
        assert_eq!(
            outcome,
            UpgradeOutcome::Upgraded {
                from: current,
                to: Version::parse("9.9.9").expect("to"),
            }
        );
        join.join().expect("server thread");

        assert_eq!(
            fs::read(&current_exe).expect("new exe"),
            new_binary.as_bytes()
        );
        let backup = root.path().join("bin/hokan.bak");
        assert_eq!(fs::read(&backup).expect("backup"), old_bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&current_exe)
                .expect("exe metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "executable bit preserved");
        }
        // Temp downloads are cleaned up.
        let leftovers = fs::read_dir(paths.cache_dir.join("downloads"))
            .expect("downloads dir")
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn checksum_mismatch_fails_and_leaves_exe_untouched() {
        let root = tempfile::tempdir().expect("tempdir");
        let archive = build_archive("#!/bin/sh\necho hokan 9.9.9\n");
        // A sums file whose digest does not match the served archive.
        let bad_sums = format!("{}  {}\n", "0".repeat(64), archive_asset("9.9.9"));
        let (base, join) = serve_release_with("9.9.9", archive, bad_sums.into_bytes());
        let (paths, current_exe) = upgrade_paths(root.path(), &base);
        let old_bytes = fs::read(&current_exe).expect("old exe");

        let current = Version::parse("0.1.0").expect("current");
        let error = download_and_install(&release(&base, "9.9.9"), &paths, &current)
            .expect_err("checksum mismatch must fail");
        assert_eq!(error.code(), "HK-UPD-HASH");
        join.join().expect("server thread");
        assert_eq!(fs::read(&current_exe).expect("exe"), old_bytes);
        assert!(!root.path().join("bin/hokan.bak").exists());
    }

    #[test]
    fn smoke_failure_fails_and_leaves_exe_untouched() {
        let root = tempfile::tempdir().expect("tempdir");
        let archive = build_archive("#!/bin/sh\nexit 1\n");
        let (base, join) = serve_release("9.9.9", archive);
        let (paths, current_exe) = upgrade_paths(root.path(), &base);
        let old_bytes = fs::read(&current_exe).expect("old exe");

        let current = Version::parse("0.1.0").expect("current");
        let error = download_and_install(&release(&base, "9.9.9"), &paths, &current)
            .expect_err("smoke failure must fail");
        assert_eq!(error.code(), "HK-UPD-SMOKE");
        join.join().expect("server thread");
        assert_eq!(fs::read(&current_exe).expect("exe"), old_bytes);
        assert!(!root.path().join("bin/hokan.bak").exists());
    }

    #[cfg(unix)]
    #[test]
    fn read_only_parent_returns_not_writable() {
        use std::os::unix::fs::PermissionsExt;
        if nix::unistd::geteuid().is_root() {
            // Root ignores permission bits; the probe would succeed.
            return;
        }
        let root = tempfile::tempdir().expect("tempdir");
        let (base, join) = serve_release("9.9.9", build_archive("#!/bin/sh\necho hokan 9.9.9\n"));
        let (paths, current_exe) = upgrade_paths(root.path(), &base);
        let bin = root.path().join("bin");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o555)).expect("read-only bin");

        let current = Version::parse("0.1.0").expect("current");
        let outcome = download_and_install(&release(&base, "9.9.9"), &paths, &current)
            .expect("not writable is an outcome, not an error");
        assert_eq!(outcome, UpgradeOutcome::NotWritable { path: bin.clone() });
        join.join().expect("server thread");

        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("restore bin");
        assert!(current_exe.exists());
    }

    #[test]
    fn expected_sha256_matches_by_archive_name() {
        let sums = sha256sums_for(&[("deadbeef", "hokan-1.0.0-aarch64-apple-darwin.tar.gz")]);
        assert_eq!(
            expected_sha256(&sums, "hokan-1.0.0-aarch64-apple-darwin.tar.gz"),
            Some("deadbeef".to_owned())
        );
        assert_eq!(expected_sha256(&sums, "hokan-9.9.9-other.tar.gz"), None);
    }
}
