use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use fs2::FileExt;
use zeroize::{Zeroize, Zeroizing};

use super::{
    CredentialError,
    store::{CredentialFileV2Ref, ProviderEntry},
};

/// Opens (creating if needed) the sibling lock file next to `path` and takes
/// an exclusive flock held until the returned file is dropped. Every
/// read-modify-write on the store runs under this lock so concurrent
/// processes cannot lose each other's entries; read paths stay lock-free.
/// The lock file itself holds no data and is kept 0600.
pub(super) fn lock_credential_store(path: &Path) -> Result<fs::File, CredentialError> {
    let parent = path.parent().ok_or(CredentialError::InvalidFormat)?;
    fs::create_dir_all(parent)?;
    set_private_directory_permissions(parent)?;
    let mut lock_name = path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(PathBuf::from(lock_name))?;
    set_private_file_permissions(&lock)?;
    lock.lock_exclusive()?;
    Ok(lock)
}

pub(super) fn write_credential_store(
    path: &Path,
    providers: &BTreeMap<String, ProviderEntry>,
) -> Result<(), CredentialError> {
    let parent = path.parent().ok_or(CredentialError::InvalidFormat)?;
    fs::create_dir_all(parent)?;
    set_private_directory_permissions(parent)?;

    let value = CredentialFileV2Ref {
        version: 2,
        providers,
    };
    let mut rendered =
        Zeroizing::new(toml::to_string(&value).map_err(|_| CredentialError::InvalidFormat)?);
    let mut temporary = tempfile::Builder::new()
        .prefix(".credentials.toml.")
        .tempfile_in(parent)?;
    set_private_file_permissions(temporary.as_file())?;
    temporary.write_all(rendered.as_bytes())?;
    rendered.as_mut_str().zeroize();
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    set_private_file_permissions(&fs::File::open(path)?)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn open_without_following_symlinks(path: &Path) -> Result<fs::File, CredentialError> {
    use nix::{
        fcntl::{OFlag, open},
        sys::stat::Mode,
    };

    open(
        path,
        OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(|error| {
        if error == nix::errno::Errno::ELOOP {
            CredentialError::NotRegular
        } else if error == nix::errno::Errno::ENOENT {
            CredentialError::Missing
        } else {
            CredentialError::Io(std::io::Error::from_raw_os_error(error as i32))
        }
    })
}

#[cfg(not(unix))]
pub(super) fn open_without_following_symlinks(path: &Path) -> Result<fs::File, CredentialError> {
    let metadata = fs::symlink_metadata(path).map_err(CredentialError::Io)?;
    if metadata.file_type().is_symlink() {
        return Err(CredentialError::NotRegular);
    }
    Ok(fs::File::open(path)?)
}

#[cfg(unix)]
pub(super) fn validate_private_metadata(metadata: &fs::Metadata) -> Result<(), CredentialError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if metadata.uid() != nix::unistd::geteuid().as_raw() {
        return Err(CredentialError::WrongOwner);
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(CredentialError::InsecurePermissions);
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn validate_private_metadata(_: &fs::Metadata) -> Result<(), CredentialError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), CredentialError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_: &Path) -> Result<(), CredentialError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(file: &fs::File) -> Result<(), CredentialError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_: &fs::File) -> Result<(), CredentialError> {
    Ok(())
}
