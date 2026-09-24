//! Download, verify, smoke-test, and atomically install a release binary.
//!
//! The sequence is deliberately ordered so a failure at any step leaves the
//! current executable untouched: writability probe → installation lock →
//! download → Ed25519 signature and SHA256 checks → extract → smoke test →
//! `{exe}.bak` backup → atomic rename.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use ed25519_dalek::{Signature, VerifyingKey};
use fs2::FileExt;
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
const SIGNATURE_MAX_BYTES: usize = 128;
const SMOKE_TIMEOUT: Duration = Duration::from_secs(10);
const SMOKE_MAX_OUTPUT: usize = 4 * 1024;

// One reviewed trust root, shared by the release signing script and verifier.
const RELEASE_SIGNING_PUBLIC_KEY: &str = include_str!("../../assets/update-signing-key.hex");

pub(crate) fn download_and_install(
    release: &ReleaseInfo,
    paths: &UpgradePaths,
    current_version: &Version,
) -> Result<UpgradeOutcome, UpdateError> {
    let mut paths = paths.clone();
    paths.current_exe = fs::canonicalize(&paths.current_exe)?;
    if managed_install(&paths.current_exe) {
        return Ok(UpgradeOutcome::ManagedInstall {
            path: paths.current_exe,
        });
    }
    let parent = paths
        .current_exe
        .parent()
        .ok_or(UpdateError::InvalidResponse)?;
    if !directory_writable(parent) {
        return Ok(UpgradeOutcome::NotWritable {
            path: parent.to_owned(),
        });
    }
    // All sessions updating this installation share one persistent lock.
    // Never unlink it: a waiter may already hold the old lock file open.
    let lock = super::local_io::open_lock(&parent.join(".hokan-update.lock"))?;
    let started = Instant::now();
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() >= Duration::from_secs(5) {
                    return Err(UpdateError::Busy);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
    let original = super::local_io::read_file(&paths.current_exe)?;
    let permissions = original.metadata()?.permissions();
    let installed = binary_version(&paths.current_exe)?;
    if release.version.cmp_precedence(current_version).is_lt()
        || installed.cmp_precedence(&release.version).is_gt()
        || (installed.cmp_precedence(current_version).is_gt()
            && installed.cmp_precedence(&release.version).is_eq())
    {
        return Ok(UpgradeOutcome::AlreadyCurrent { version: installed });
    }
    let downloads = paths.cache_dir.join("downloads");
    super::local_io::private_directory(&downloads)?;
    let archive_name = api::archive_name(&release.version)?;
    let target = api::target_triple().ok_or(UpdateError::UnsupportedPlatform)?;
    let mut archive_file = tempfile::Builder::new()
        .prefix(".archive.")
        .tempfile_in(&downloads)?;
    // Atomic replacement requires the staged file to share the executable's
    // filesystem, even when XDG_CACHE_HOME is on another disk.
    let staged = tempfile::Builder::new()
        .prefix(".hokan-update.")
        .tempfile_in(parent)?
        .into_temp_path();

    let client = api::download_client()?;
    let checksums = api::block_on(api::download_with_fallback(
        &client,
        &release.checksums_url,
        CHECKSUMS_MAX_BYTES,
    ))??;
    let signature = api::block_on(api::download_with_fallback(
        &client,
        &release.signature_url,
        SIGNATURE_MAX_BYTES,
    ))??;
    verify_checksums_signature(&checksums, &signature)?;
    let checksums = String::from_utf8(checksums).map_err(|_| UpdateError::InvalidResponse)?;
    let expected =
        expected_sha256(&checksums, &archive_name).ok_or(UpdateError::InvalidResponse)?;
    let archive = api::block_on(api::download_with_fallback(
        &client,
        &release.archive_url,
        DOWNLOAD_MAX_BYTES,
    ))??;
    archive_file.write_all(&archive)?;
    archive_file.as_file().sync_all()?;

    let actual = format!("{:x}", Sha256::digest(&archive));
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(UpdateError::ChecksumMismatch);
    }

    let package = format!("hokan-{}-{target}", release.version);
    extract_binary(&archive, &staged, &package)?;
    fs::set_permissions(&staged, permissions.clone())?;
    smoke_test(&staged, &release.version)?;

    let mut backup_name = paths.current_exe.as_os_str().to_owned();
    backup_name.push(".bak");
    let backup = PathBuf::from(backup_name);
    let mut backup_tmp = tempfile::Builder::new()
        .prefix(".hokan-backup.")
        .tempfile_in(parent)?;
    std::io::copy(&mut &original, &mut backup_tmp)?;
    backup_tmp.as_file().set_permissions(permissions)?;
    backup_tmp.as_file().sync_all()?;
    backup_tmp.persist(&backup).map_err(|error| error.error)?;
    staged
        .persist(&paths.current_exe)
        .map_err(|error| error.error)?;
    if let Ok(directory) = fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(UpgradeOutcome::Upgraded {
        from: installed,
        to: release.version.clone(),
    })
}

fn release_public_key() -> Result<[u8; 32], UpdateError> {
    let hex = RELEASE_SIGNING_PUBLIC_KEY.trim();
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UpdateError::SignatureMismatch);
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| UpdateError::SignatureMismatch)?;
    }
    Ok(bytes)
}

fn verify_with_key(
    checksums: &[u8],
    signature: &[u8],
    public_key: &[u8; 32],
) -> Result<(), UpdateError> {
    let key = VerifyingKey::from_bytes(public_key).map_err(|_| UpdateError::SignatureMismatch)?;
    let signature = Signature::from_slice(signature).map_err(|_| UpdateError::SignatureMismatch)?;
    key.verify_strict(checksums, &signature)
        .map_err(|_| UpdateError::SignatureMismatch)
}

fn verify_checksums_signature(checksums: &[u8], signature: &[u8]) -> Result<(), UpdateError> {
    let public_key = release_public_key()?;
    // Unit fixtures use a separate disposable key; never accept it in a
    // production build. The production trust root is exercised below too.
    #[cfg(test)]
    let public_key = {
        assert_ne!(
            public_key,
            crate::update::test_support::signing_key()
                .verifying_key()
                .to_bytes()
        );
        crate::update::test_support::signing_key()
            .verifying_key()
            .to_bytes()
    };
    verify_with_key(checksums, signature, &public_key)
}

/// Exactly one valid digest must bind this version and target's archive name.
fn expected_sha256(checksums: &str, archive_name: &str) -> Option<String> {
    let mut expected = None;
    for line in checksums.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        if name.strip_prefix('*').unwrap_or(name) != archive_name {
            continue;
        }
        if expected.is_some()
            || parts.next().is_some()
            || hash.len() != 64
            || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        expected = Some(hash.to_owned());
    }
    expected
}

/// Extracts `bin/hokan` from the tar.gz archive to `staged`, mode 0755.
fn extract_binary(archive: &[u8], staged: &Path, package: &str) -> Result<(), UpdateError> {
    let decoder = flate2::read::GzDecoder::new(archive).take((DOWNLOAD_MAX_BYTES * 2) as u64);
    let mut tar = tar::Archive::new(decoder);
    let mut binary = None;
    for entry in tar.entries().map_err(|_| UpdateError::InvalidResponse)? {
        let mut entry = entry.map_err(|_| UpdateError::InvalidResponse)?;
        let path = entry
            .path()
            .map_err(|_| UpdateError::InvalidResponse)?
            .into_owned();
        if path == Path::new("bin/hokan") || path == Path::new(package).join("bin/hokan") {
            if !entry.header().entry_type().is_file() || entry.size() > DOWNLOAD_MAX_BYTES as u64 {
                return Err(UpdateError::InvalidResponse);
            }
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
    if binary_version(staged)? == *version {
        Ok(())
    } else {
        Err(UpdateError::SmokeTest)
    }
}

fn binary_version(path: &Path) -> Result<Version, UpdateError> {
    let program = path.to_str().ok_or(UpdateError::InvalidResponse)?;
    let output =
        crate::platform::run_bounded(program, ["--version"], SMOKE_TIMEOUT, SMOKE_MAX_OUTPUT)
            .map_err(|_error| {
                #[cfg(test)]
                eprintln!("version probe for {program} failed: {_error}");
                UpdateError::SmokeTest
            })?;
    if !output.status.success() {
        #[cfg(test)]
        eprintln!(
            "version probe for {program} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        return Err(UpdateError::SmokeTest);
    }
    let text = std::str::from_utf8(&output.stdout).map_err(|_| UpdateError::SmokeTest)?;
    let version = text
        .trim()
        .strip_prefix("hokan ")
        .ok_or(UpdateError::SmokeTest)?;
    Version::parse(version).map_err(|_| UpdateError::SmokeTest)
}

/// Probe-writes the executable's directory: package-manager installs live
/// in system paths we must not touch. Also used by `hokan doctor`.
pub(crate) fn directory_writable(directory: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let Ok(metadata) = fs::metadata(directory) else {
            return false;
        };
        // A writable installation directory must belong to the account doing
        // the upgrade and must not be writable by a group or other user. This
        // prevents a world-writable/shared directory from becoming an update
        // substitution point.
        if !metadata.is_dir()
            || metadata.uid() != nix::unistd::geteuid().as_raw()
            || metadata.permissions().mode() & 0o022 != 0
        {
            return false;
        }
    }
    tempfile::Builder::new()
        .prefix(".hokan-write-probe.")
        .tempfile_in(directory)
        .is_ok()
}

/// Common package-manager layouts must not be edited behind their manager's
/// back. Homebrew's Cellar is often writable by the logged-in user.
pub(crate) fn managed_install(exe: &Path) -> bool {
    let resolved = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_owned());
    resolved
        .components()
        .any(|part| part.as_os_str() == "Cellar")
        || [
            "/opt/local",
            "/nix/store",
            "/snap",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]
        .iter()
        .any(|prefix| resolved.starts_with(prefix))
}

#[cfg(test)]
#[path = "install_regressions.rs"]
mod regressions;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::test_support::{
        archive_asset, build_archive, raw_reply, serve_release, serve_release_with, sha256sums_for,
        sign_checksums, spawn_server, write_stub_binary,
    };

    pub(super) fn upgrade_paths(root: &Path, base: &str) -> (UpgradePaths, PathBuf) {
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

    pub(super) fn release(base: &str, version: &str) -> ReleaseInfo {
        ReleaseInfo {
            version: Version::parse(version).expect("version"),
            tag: format!("v{version}"),
            archive_url: format!("{base}/download/{}", archive_asset(version)),
            checksums_url: format!("{base}/download/SHA256SUMS"),
            signature_url: format!("{base}/download/SHA256SUMS.sig"),
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
        let base = "http://127.0.0.1:1";
        let (paths, current_exe) = upgrade_paths(root.path(), base);
        let bin = root.path().join("bin");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o555)).expect("read-only bin");

        let current = Version::parse("0.1.0").expect("current");
        let outcome = download_and_install(&release(base, "9.9.9"), &paths, &current)
            .expect("not writable is an outcome, not an error");
        assert_eq!(
            outcome,
            UpgradeOutcome::NotWritable {
                path: fs::canonicalize(&bin).expect("resolved bin")
            }
        );
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("restore bin");
        assert!(current_exe.exists());
    }

    #[cfg(unix)]
    #[test]
    fn shared_install_directories_are_not_update_targets() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("tempdir");
        let shared = root.path().join("shared");
        fs::create_dir(&shared).expect("shared dir");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).expect("shared mode");
        assert!(!directory_writable(&shared));
        let file = root.path().join("file");
        fs::write(&file, b"not a directory").expect("file");
        assert!(!directory_writable(&file));
    }

    #[test]
    fn expected_sha256_matches_by_archive_name() {
        let sums = sha256sums_for(&[(&"ab".repeat(32), "hokan-1.0.0-aarch64-apple-darwin.tar.gz")]);
        assert_eq!(
            expected_sha256(&sums, "hokan-1.0.0-aarch64-apple-darwin.tar.gz"),
            Some("ab".repeat(32))
        );
        assert_eq!(expected_sha256(&sums, "hokan-9.9.9-other.tar.gz"), None);
    }

    #[test]
    fn signed_checksums_are_required_before_hash_matching() {
        let sums = b"deadbeef  hokan-1.0.0-aarch64-apple-darwin.tar.gz\n";
        let signature = sign_checksums(sums);
        assert!(verify_checksums_signature(sums, &signature).is_ok());
        let mut changed = signature;
        changed[0] ^= 1;
        assert!(matches!(
            verify_checksums_signature(sums, &changed),
            Err(UpdateError::SignatureMismatch)
        ));
    }

    #[test]
    fn actual_production_public_key_verifies_only_its_own_signature() {
        let message = b"hokan update signing key verification fixture\n";
        let key = release_public_key().expect("production public key");
        let signature = include_bytes!("fixtures/trust-root.sig");
        assert!(verify_with_key(message, signature, &key).is_ok());
        assert!(verify_with_key(b"tampered", signature, &key).is_err());
        assert!(verify_with_key(message, &sign_checksums(message), &key).is_err());
    }

    #[test]
    fn forged_checksums_cannot_authorize_even_a_matching_poisoned_archive() {
        for mutation in [
            "tampered_sums",
            "wrong_key",
            "short_signature",
            "missing_signature",
        ] {
            let root = tempfile::tempdir().expect("root");
            let marker = root.path().join("poison-executed");
            let poisoned = build_archive(&format!(
                "#!/bin/sh\ntouch '{}'\necho hokan 9.9.9\n",
                marker.display()
            ));
            let sums = sha256sums_for(&[(
                &format!("{:x}", Sha256::digest(&poisoned)),
                &archive_asset("9.9.9"),
            )])
            .into_bytes();
            let signature = match mutation {
                "tampered_sums" => sign_checksums(b"authentic checksum list"),
                "wrong_key" => {
                    use ed25519_dalek::Signer;
                    ed25519_dalek::SigningKey::from_bytes(&[0x24; 32])
                        .sign(&sums)
                        .to_bytes()
                        .to_vec()
                }
                _ => vec![0; 63],
            };
            let (base, join) = spawn_server(2, move |path| match path {
                "/download/SHA256SUMS" => raw_reply("200 OK", sums.clone()),
                "/download/SHA256SUMS.sig" if mutation == "missing_signature" => {
                    raw_reply("404 Not Found", Vec::new())
                }
                "/download/SHA256SUMS.sig" => raw_reply("200 OK", signature.clone()),
                _ => panic!("must reject before downloading the archive"),
            });
            let (paths, exe) = upgrade_paths(root.path(), &base);
            let before = fs::read(&exe).expect("original");
            assert!(
                download_and_install(&release(&base, "9.9.9"), &paths, &Version::new(0, 1, 0))
                    .is_err()
            );
            join.join().expect("server");
            assert_eq!(fs::read(exe).expect("unchanged"), before);
            assert!(!marker.exists());
            assert!(!root.path().join("bin/hokan.bak").exists());
        }
    }

    #[test]
    fn signed_checksum_list_binds_exact_version_and_target() {
        for filename in [
            "hokan-1.0.0-wrong-target.tar.gz",
            "hokan-0.0.1-aarch64-apple-darwin.tar.gz",
        ] {
            let sums = sha256sums_for(&[(&"ab".repeat(32), filename)]);
            assert!(
                verify_checksums_signature(sums.as_bytes(), &sign_checksums(sums.as_bytes()))
                    .is_ok()
            );
            assert_eq!(expected_sha256(&sums, &archive_asset("9.9.9")), None);
        }
        let name = archive_asset("9.9.9");
        for sums in [
            format!("deadbeef  {name}\n"),
            format!("{}  {name}\n", "z".repeat(64)),
            format!("{}  {name} extra\n", "ab".repeat(32)),
            sha256sums_for(&[(&"ab".repeat(32), &name), (&"cd".repeat(32), &name)]),
        ] {
            assert_eq!(expected_sha256(&sums, &name), None);
        }
    }
}
