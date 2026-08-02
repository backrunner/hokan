use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

use super::AiConfig;

const CREDENTIAL_FILE_MAX_BYTES: u64 = 16 * 1024;
const API_KEY_MAX_BYTES: usize = 8 * 1024;

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("AI credential is not configured")]
    Missing,
    #[error("AI credential file is not a regular file")]
    NotRegular,
    #[error("AI credential file must be owned by the current user")]
    WrongOwner,
    #[error("AI credential file permissions must be 0600 or stricter")]
    InsecurePermissions,
    #[error("AI credential file exceeded 16 KiB")]
    TooLarge,
    #[error("AI credential file is invalid")]
    InvalidFormat,
    #[error("AI credential contains invalid bytes")]
    InvalidSecret,
    #[error("AI credential I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CredentialFile {
    version: u32,
    api_key: Zeroizing<String>,
}

pub fn resolve_credential_path(config: &AiConfig, default_path: &Path) -> Option<PathBuf> {
    let configured = config.api_key_file.as_ref()?;
    Some(if configured.is_absolute() {
        configured.clone()
    } else {
        default_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(configured)
    })
}

pub fn load_api_key(
    config: &AiConfig,
    default_path: &Path,
) -> Result<Zeroizing<String>, CredentialError> {
    if let Some(path) = resolve_credential_path(config, default_path) {
        return read_api_key_file(&path);
    }
    let key = env::var(&config.api_key_env).map_err(|_| CredentialError::Missing)?;
    validate_secret(&key)?;
    Ok(Zeroizing::new(key))
}

#[must_use]
pub fn credential_available(config: &AiConfig, default_path: &Path) -> bool {
    load_api_key(config, default_path).is_ok()
}

pub fn write_api_key(path: &Path, key: &str) -> Result<(), CredentialError> {
    validate_secret(key)?;
    let parent = path.parent().ok_or(CredentialError::InvalidFormat)?;
    fs::create_dir_all(parent)?;
    set_private_directory_permissions(parent)?;

    let value = CredentialFile {
        version: 1,
        api_key: Zeroizing::new(key.to_owned()),
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

fn read_api_key_file(path: &Path) -> Result<Zeroizing<String>, CredentialError> {
    let mut file = open_without_following_symlinks(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(CredentialError::NotRegular);
    }
    validate_private_metadata(&metadata)?;
    if metadata.len() > CREDENTIAL_FILE_MAX_BYTES {
        return Err(CredentialError::TooLarge);
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    Read::by_ref(&mut file)
        .take(CREDENTIAL_FILE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > CREDENTIAL_FILE_MAX_BYTES {
        return Err(CredentialError::TooLarge);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| CredentialError::InvalidFormat)?;
    let value: CredentialFile = toml::from_str(text).map_err(|_| CredentialError::InvalidFormat)?;
    if value.version != 1 {
        return Err(CredentialError::InvalidFormat);
    }
    validate_secret(&value.api_key)?;
    Ok(value.api_key)
}

fn validate_secret(key: &str) -> Result<(), CredentialError> {
    if key.is_empty()
        || key.len() > API_KEY_MAX_BYTES
        || key.trim() != key
        || key.chars().any(char::is_control)
    {
        return Err(CredentialError::InvalidSecret);
    }
    Ok(())
}

#[cfg(unix)]
fn open_without_following_symlinks(path: &Path) -> Result<fs::File, CredentialError> {
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
fn open_without_following_symlinks(path: &Path) -> Result<fs::File, CredentialError> {
    let metadata = fs::symlink_metadata(path).map_err(CredentialError::Io)?;
    if metadata.file_type().is_symlink() {
        return Err(CredentialError::NotRegular);
    }
    Ok(fs::File::open(path)?)
}

#[cfg(unix)]
fn validate_private_metadata(metadata: &fs::Metadata) -> Result<(), CredentialError> {
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
fn validate_private_metadata(_: &fs::Metadata) -> Result<(), CredentialError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_private_file_and_reads_key() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("config/credentials.toml");
        write_api_key(&path, "test-secret").expect("write credential");
        let config = AiConfig {
            api_key_file: Some(PathBuf::from("credentials.toml")),
            ..AiConfig::default()
        };
        assert_eq!(
            load_api_key(&config, &path)
                .expect("read credential")
                .as_str(),
            "test-secret"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_broad_permissions_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("credentials.toml");
        write_api_key(&path, "test-secret").expect("write credential");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("permissions");
        assert!(matches!(
            read_api_key_file(&path),
            Err(CredentialError::InsecurePermissions)
        ));

        let link = directory.path().join("linked.toml");
        symlink(&path, &link).expect("symlink");
        assert!(matches!(
            read_api_key_file(&link),
            Err(CredentialError::NotRegular)
        ));

        let fifo = directory.path().join("credentials.fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("credential FIFO");
        assert!(matches!(
            read_api_key_file(&fifo),
            Err(CredentialError::NotRegular)
        ));
    }

    #[test]
    fn rejects_invalid_secret_without_echoing_it() {
        let error = write_api_key(Path::new("unused"), "secret\nvalue")
            .expect_err("newline must be rejected");
        assert!(!error.to_string().contains("secret"));
    }
}
