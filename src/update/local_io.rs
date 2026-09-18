//! Bounded, non-following access to updater-owned files.
use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

use nix::fcntl::OFlag;

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsafe update file or directory",
    )
}

pub(super) fn validate_file(file: &fs::File) -> io::Result<fs::Metadata> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o6022 != 0
        || metadata.nlink() != 1
    {
        return Err(denied());
    }
    Ok(metadata)
}

pub(super) fn read_file(path: &Path) -> io::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)?;
    validate_file(&file)?;
    Ok(file)
}

pub(super) fn open_lock(path: &Path) -> io::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)?;
    validate_file(&file)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

pub(super) fn private_directory(path: &Path) -> io::Result<fs::File> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)?;
    let metadata = directory.metadata()?;
    if metadata.uid() != nix::unistd::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
        return Err(denied());
    }
    directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn locks_reject_links_and_fifos_without_changing_the_target() {
        let root = tempfile::tempdir().expect("root");
        let target = root.path().join("target");
        fs::write(&target, "preserve").expect("target");
        let link = root.path().join("link");
        symlink(&target, &link).expect("symlink");
        assert!(open_lock(&link).is_err());
        let hard = root.path().join("hard");
        fs::hard_link(&target, &hard).expect("hard link");
        assert!(open_lock(&hard).is_err());
        let fifo = root.path().join("fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("fifo");
        assert!(open_lock(&fifo).is_err());
        assert_eq!(fs::read_to_string(target).expect("target"), "preserve");
    }

    #[test]
    fn private_directory_rejects_symlinks_and_shared_directories() {
        let root = tempfile::tempdir().expect("root");
        let target = root.path().join("target");
        fs::create_dir(&target).expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).expect("mode");
        let link = root.path().join("link");
        symlink(&target, &link).expect("link");
        assert!(private_directory(&link).is_err());
        assert_eq!(
            fs::metadata(&target).expect("metadata").mode() & 0o777,
            0o755
        );
        fs::set_permissions(&target, fs::Permissions::from_mode(0o777)).expect("shared");
        assert!(private_directory(&target).is_err());
        assert_eq!(
            fs::metadata(&target).expect("metadata").mode() & 0o777,
            0o777
        );
    }
}
