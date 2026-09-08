use super::tests::{release, upgrade_paths};
use super::*;
use crate::update::test_support::{
    archive_asset, build_archive, build_archive_at, serve_release, write_stub_binary,
};

#[test]
fn published_package_layout_upgrades_successfully() {
    let root = tempfile::tempdir().expect("tempdir");
    let package = format!("hokan-9.9.9-{}", api::target_triple().expect("target"));
    let archive = build_archive_at(
        "#!/bin/sh\necho hokan 9.9.9\n",
        &format!("{package}/bin/hokan"),
    );
    let (base, join) = serve_release("9.9.9", archive);
    let (paths, _) = upgrade_paths(root.path(), &base);
    assert!(matches!(
        download_and_install(&release(&base, "9.9.9"), &paths, &Version::new(0, 1, 0))
            .expect("upgrade published layout"),
        UpgradeOutcome::Upgraded { .. }
    ));
    join.join().expect("server thread");
}

#[test]
fn stages_on_the_installation_filesystem() {
    let root = tempfile::tempdir().expect("tempdir");
    let archive = build_archive(
        "#!/bin/sh\ncase \"$0\" in */bin/.hokan-update.*) echo hokan 9.9.9;; *) exit 1;; esac\n",
    );
    let (base, join) = serve_release("9.9.9", archive);
    let (paths, _) = upgrade_paths(root.path(), &base);
    download_and_install(&release(&base, "9.9.9"), &paths, &Version::new(0, 1, 0))
        .expect("staged alongside executable, not in cache");
    join.join().expect("server thread");
}

#[test]
fn simultaneous_updates_preserve_the_original_backup() {
    let root = tempfile::tempdir().expect("tempdir");
    let (base, join) = serve_release("9.9.9", build_archive("#!/bin/sh\necho hokan 9.9.9\n"));
    let (paths, exe) = upgrade_paths(root.path(), &base);
    let old = fs::read(exe).expect("original");
    let release = release(&base, "9.9.9");
    let barrier = std::sync::Barrier::new(2);
    let outcomes = std::thread::scope(|scope| {
        let run = || {
            barrier.wait();
            download_and_install(&release, &paths, &Version::new(0, 1, 0)).expect("update")
        };
        let first = scope.spawn(run);
        let second = scope.spawn(run);
        [first.join().expect("first"), second.join().expect("second")]
    });
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, UpgradeOutcome::Upgraded { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, UpgradeOutcome::AlreadyCurrent { .. }))
            .count(),
        1
    );
    assert_eq!(
        fs::read(root.path().join("bin/hokan.bak")).expect("backup"),
        old
    );
    join.join().expect("server thread");
}

#[test]
fn smoke_test_requires_the_exact_program_and_version() {
    let root = tempfile::tempdir().expect("tempdir");
    let binary = root.path().join("hokan");
    for output in ["other 9.9.9", "hokan 9.9.90", "hokan 9.9.9-beta.1"] {
        write_stub_binary(&binary, &format!("#!/bin/sh\necho '{output}'\n"));
        assert!(
            smoke_test(&binary, &Version::new(9, 9, 9)).is_err(),
            "{output}"
        );
    }
}

#[test]
fn release_packager_supports_installers_and_legacy_updaters() {
    let root = tempfile::tempdir().expect("tempdir");
    let target = api::target_triple().expect("native target");
    let bin_dir = root.path().join(format!("target/{target}/release"));
    fs::create_dir_all(&bin_dir).expect("build dir");
    fs::create_dir_all(root.path().join("docs")).expect("docs");
    let binary = "#!/bin/sh\necho hokan 9.9.9\n";
    write_stub_binary(&bin_dir.join("hokan"), binary);
    for name in ["README.md", "LICENSE", "docs/hokan.1"] {
        fs::write(root.path().join(name), name).expect("packaged document");
    }
    let script = root.path().join("package-release.sh");
    fs::write(&script, include_str!("../../scripts/package-release.sh")).expect("packager");
    let output = std::process::Command::new("bash")
        .arg(&script)
        .args([target, "9.9.9"])
        .current_dir(root.path())
        .output()
        .expect("run packager");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let archive = fs::read(root.path().join("dist").join(archive_asset("9.9.9"))).expect("archive");
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive.as_slice()));
    let mut legacy_binary = Vec::new();
    for entry in tar.entries().expect("entries") {
        let mut entry = entry.expect("entry");
        if entry.path().expect("path") == Path::new("bin/hokan") {
            assert!(entry.header().entry_type().is_file());
            entry
                .read_to_end(&mut legacy_binary)
                .expect("legacy updater extraction");
        }
    }
    assert_eq!(legacy_binary, binary.as_bytes());
    let extracted = root.path().join("extracted");
    fs::create_dir(&extracted).expect("extract dir");
    tar::Archive::new(flate2::read::GzDecoder::new(archive.as_slice()))
        .unpack(&extracted)
        .expect("installer extraction");
    assert_eq!(
        fs::read(extracted.join(format!("hokan-9.9.9-{target}/bin/hokan")))
            .expect("installer binary"),
        binary.as_bytes()
    );
}
