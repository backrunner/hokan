use std::{
    collections::BTreeMap,
    env, fmt, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

use super::{AiAuth, AiConfig};

const CREDENTIAL_FILE_MAX_BYTES: u64 = 16 * 1024;
const API_KEY_MAX_BYTES: usize = 8 * 1024;
/// Slug under which a legacy v1 single-key file is migrated when a v2 write
/// merges it into the per-provider store. The v1 format carries no provider
/// name, so a reserved slug keeps the key reachable without inventing one.
const LEGACY_PROVIDER_SLUG: &str = "default";

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
struct CredentialFileV1 {
    version: u32,
    api_key: Zeroizing<String>,
}

#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialFileV2 {
    version: u32,
    #[serde(default)]
    providers: BTreeMap<String, ProviderEntry>,
}

#[derive(Serialize)]
struct CredentialFileV2Ref<'a> {
    version: u32,
    providers: &'a BTreeMap<String, ProviderEntry>,
}

/// Serde mirror of [`ProviderCredential`]: exactly one of `api_key` or the
/// `access_token`/`refresh_token`/`expires_at` triple is set per entry.
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProviderEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<Zeroizing<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_token: Option<Zeroizing<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<Zeroizing<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
}

/// One provider's credential as stored in the v2 credentials file.
pub enum ProviderCredential {
    ApiKey(Zeroizing<String>),
    OAuth(OAuthTokens),
}

#[derive(Clone)]
pub struct OAuthTokens {
    pub access_token: Zeroizing<String>,
    pub refresh_token: Zeroizing<String>,
    /// Token expiry as seconds since the Unix epoch.
    pub expires_at: u64,
    /// Only Codex needs this (ChatGPT account id); other providers leave it `None`.
    pub account_id: Option<String>,
}

// Secrets must never reach logs via `{:?}`, so Debug redacts every token field.
impl fmt::Debug for ProviderCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => formatter.write_str("ProviderCredential::ApiKey(<redacted>)"),
            Self::OAuth(tokens) => formatter
                .debug_tuple("ProviderCredential::OAuth")
                .field(tokens)
                .finish(),
        }
    }
}

impl fmt::Debug for OAuthTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthTokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("account_id", &self.account_id)
            .finish()
    }
}

impl ProviderEntry {
    fn into_credential(self) -> Result<ProviderCredential, CredentialError> {
        match (
            self.api_key,
            self.access_token,
            self.refresh_token,
            self.expires_at,
        ) {
            (Some(key), None, None, None) => {
                validate_secret(&key)?;
                Ok(ProviderCredential::ApiKey(key))
            }
            (None, Some(access_token), Some(refresh_token), Some(expires_at)) => {
                validate_secret(&access_token)?;
                validate_secret(&refresh_token)?;
                Ok(ProviderCredential::OAuth(OAuthTokens {
                    access_token,
                    refresh_token,
                    expires_at,
                    account_id: self.account_id,
                }))
            }
            _ => Err(CredentialError::InvalidFormat),
        }
    }
}

impl From<&ProviderCredential> for ProviderEntry {
    fn from(credential: &ProviderCredential) -> Self {
        match credential {
            ProviderCredential::ApiKey(key) => Self {
                api_key: Some(Zeroizing::new(key.as_str().to_owned())),
                ..Self::default()
            },
            ProviderCredential::OAuth(tokens) => Self {
                access_token: Some(Zeroizing::new(tokens.access_token.as_str().to_owned())),
                refresh_token: Some(Zeroizing::new(tokens.refresh_token.as_str().to_owned())),
                expires_at: Some(tokens.expires_at),
                account_id: tokens.account_id.clone(),
                ..Self::default()
            },
        }
    }
}

/// On-disk credential store, dispatched on the top-level `version` field:
/// v1 is the legacy single-key layout, v2 the per-provider layout.
enum CredentialStore {
    V1(Zeroizing<String>),
    V2(BTreeMap<String, ProviderEntry>),
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
        // Legacy configs carry no provider slug; their keys live under the
        // reserved migration slug in a v2 store (or as the single v1 entry).
        let provider = config.provider.trim();
        if provider.is_empty() {
            return read_api_key_file(&path, LEGACY_PROVIDER_SLUG);
        }
        // Keys written before the provider slug was honored (and legacy v1
        // migrations) live under the reserved slug; fall back to it when the
        // provider has no entry of its own.
        return match read_api_key_file(&path, provider) {
            Err(CredentialError::Missing) => read_api_key_file(&path, LEGACY_PROVIDER_SLUG),
            result => result,
        };
    }
    let key = env::var(&config.api_key_env).map_err(|_| CredentialError::Missing)?;
    validate_secret(&key)?;
    Ok(Zeroizing::new(key))
}

#[must_use]
pub fn credential_available(config: &AiConfig, default_path: &Path) -> bool {
    load_api_key(config, default_path).is_ok()
}

/// Availability check that follows the configured auth method: `oauth` is
/// satisfied by a stored OAuth token set for `ai.provider`, `api-key` by the
/// legacy file/env resolution of [`load_api_key`].
#[must_use]
pub fn configured_credential_available(config: &AiConfig, default_path: &Path) -> bool {
    if config.auth == AiAuth::OAuth {
        let provider = config.provider.trim();
        if provider.is_empty() {
            return false;
        }
        let path = resolve_credential_path(config, default_path)
            .unwrap_or_else(|| default_path.to_owned());
        return matches!(
            read_credential(&path, provider),
            Ok(ProviderCredential::OAuth(_))
        );
    }
    credential_available(config, default_path)
}

/// Writes a legacy single API key. Stored under the reserved migration slug
/// in the v2 layout so existing entries for other providers are preserved.
pub fn write_api_key(path: &Path, key: &str) -> Result<(), CredentialError> {
    write_credential(
        path,
        LEGACY_PROVIDER_SLUG,
        &ProviderCredential::ApiKey(Zeroizing::new(key.to_owned())),
    )
}

/// Reads the credential stored for `slug`. A v1 file holds exactly one
/// credential with no provider name, so it answers for any slug.
pub fn read_credential(path: &Path, slug: &str) -> Result<ProviderCredential, CredentialError> {
    match read_credential_store(path)? {
        CredentialStore::V1(key) => Ok(ProviderCredential::ApiKey(key)),
        CredentialStore::V2(mut providers) => providers
            .remove(slug)
            .ok_or(CredentialError::Missing)?
            .into_credential(),
    }
}

/// Inserts or replaces `slug`'s credential, preserving every other provider's
/// entry. A first v2 write against a v1 file migrates the legacy key under
/// [`LEGACY_PROVIDER_SLUG`] before merging.
pub fn write_credential(
    path: &Path,
    slug: &str,
    credential: &ProviderCredential,
) -> Result<(), CredentialError> {
    validate_credential(credential)?;
    let _lock = lock_credential_store(path)?;
    let mut providers = match read_credential_store(path) {
        Ok(CredentialStore::V1(key)) => {
            let mut providers = BTreeMap::new();
            providers.insert(
                LEGACY_PROVIDER_SLUG.to_owned(),
                ProviderEntry::from(&ProviderCredential::ApiKey(key)),
            );
            providers
        }
        Ok(CredentialStore::V2(providers)) => providers,
        Err(CredentialError::Missing) => BTreeMap::new(),
        Err(error) => return Err(error),
    };
    providers.insert(slug.to_owned(), ProviderEntry::from(credential));
    write_credential_store(path, &providers)
}

/// Removes `slug`'s credential; deletes the file once no entries remain.
/// v1 files carry no per-provider entries, so deleting from them reports
/// [`CredentialError::Missing`] rather than guessing at the single key.
pub fn delete_credential(path: &Path, slug: &str) -> Result<(), CredentialError> {
    let _lock = lock_credential_store(path)?;
    let mut providers = match read_credential_store(path)? {
        CredentialStore::V1(_) => return Err(CredentialError::Missing),
        CredentialStore::V2(providers) => providers,
    };
    if providers.remove(slug).is_none() {
        return Err(CredentialError::Missing);
    }
    if providers.is_empty() {
        fs::remove_file(path)?;
        let parent = path.parent().ok_or(CredentialError::InvalidFormat)?;
        fs::File::open(parent)?.sync_all()?;
        return Ok(());
    }
    write_credential_store(path, &providers)
}

fn read_api_key_file(path: &Path, slug: &str) -> Result<Zeroizing<String>, CredentialError> {
    match read_credential(path, slug)? {
        ProviderCredential::ApiKey(key) => Ok(key),
        ProviderCredential::OAuth(_) => Err(CredentialError::InvalidFormat),
    }
}

/// Opens (creating if needed) the sibling lock file next to `path` and takes
/// an exclusive flock held until the returned file is dropped. Every
/// read-modify-write on the store runs under this lock so concurrent
/// processes cannot lose each other's entries; read paths stay lock-free.
/// The lock file itself holds no data and is kept 0600.
fn lock_credential_store(path: &Path) -> Result<fs::File, CredentialError> {
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

fn validate_credential(credential: &ProviderCredential) -> Result<(), CredentialError> {
    match credential {
        ProviderCredential::ApiKey(key) => validate_secret(key),
        ProviderCredential::OAuth(tokens) => {
            validate_secret(&tokens.access_token)?;
            validate_secret(&tokens.refresh_token)
        }
    }
}

fn read_credential_store(path: &Path) -> Result<CredentialStore, CredentialError> {
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
    let probe: VersionProbe = toml::from_str(text).map_err(|_| CredentialError::InvalidFormat)?;
    match probe.version {
        1 => {
            let value: CredentialFileV1 =
                toml::from_str(text).map_err(|_| CredentialError::InvalidFormat)?;
            validate_secret(&value.api_key)?;
            Ok(CredentialStore::V1(value.api_key))
        }
        2 => {
            let value: CredentialFileV2 =
                toml::from_str(text).map_err(|_| CredentialError::InvalidFormat)?;
            if value.version != 2 {
                return Err(CredentialError::InvalidFormat);
            }
            Ok(CredentialStore::V2(value.providers))
        }
        _ => Err(CredentialError::InvalidFormat),
    }
}

fn write_credential_store(
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

/// Shared secret rules (length, whitespace, control bytes); the setup wizard
/// reuses them for interactive API-key entry.
pub(crate) fn validate_secret(key: &str) -> Result<(), CredentialError> {
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

    /// Writes a legacy v1 file directly; `write_api_key` itself now emits v2.
    fn write_v1_file(path: &Path, key: &str) {
        let parent = path.parent().expect("parent");
        fs::create_dir_all(parent).expect("parent directories");
        fs::write(path, format!("version = 1\napi_key = \"{key}\"\n")).expect("write v1 file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("permissions");
        }
    }

    fn oauth_tokens() -> ProviderCredential {
        ProviderCredential::OAuth(OAuthTokens {
            access_token: Zeroizing::new("access-token".to_owned()),
            refresh_token: Zeroizing::new("refresh-token".to_owned()),
            expires_at: 1_735_689_600,
            account_id: Some("account-1".to_owned()),
        })
    }

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
            read_api_key_file(&path, LEGACY_PROVIDER_SLUG),
            Err(CredentialError::InsecurePermissions)
        ));

        let link = directory.path().join("linked.toml");
        symlink(&path, &link).expect("symlink");
        assert!(matches!(
            read_api_key_file(&link, LEGACY_PROVIDER_SLUG),
            Err(CredentialError::NotRegular)
        ));

        let fifo = directory.path().join("credentials.fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("credential FIFO");
        assert!(matches!(
            read_api_key_file(&fifo, LEGACY_PROVIDER_SLUG),
            Err(CredentialError::NotRegular)
        ));
    }

    #[test]
    fn rejects_invalid_secret_without_echoing_it() {
        let error = write_api_key(Path::new("unused"), "secret\nvalue")
            .expect_err("newline must be rejected");
        assert!(!error.to_string().contains("secret"));

        let error = write_credential(
            Path::new("unused"),
            "grok-oauth",
            &ProviderCredential::OAuth(OAuthTokens {
                access_token: Zeroizing::new("access\nsecret".to_owned()),
                refresh_token: Zeroizing::new("refresh-secret".to_owned()),
                expires_at: 1_735_689_600,
                account_id: None,
            }),
        )
        .expect_err("newline in token must be rejected");
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn v1_file_reads_as_api_key_for_any_slug() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("credentials.toml");
        write_v1_file(&path, "legacy-secret");

        for slug in ["deepseek", LEGACY_PROVIDER_SLUG, "anything"] {
            match read_credential(&path, slug).expect("read v1 credential") {
                ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "legacy-secret"),
                ProviderCredential::OAuth(_) => panic!("v1 files only hold API keys"),
            }
        }
    }

    #[test]
    fn v2_write_read_roundtrip_per_variant() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("credentials.toml");

        write_credential(
            &path,
            "deepseek",
            &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
        )
        .expect("write api key credential");
        write_credential(&path, "grok-oauth", &oauth_tokens()).expect("write oauth credential");

        match read_credential(&path, "deepseek").expect("read api key credential") {
            ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "deepseek-key"),
            ProviderCredential::OAuth(_) => panic!("deepseek entry must stay an API key"),
        }
        match read_credential(&path, "grok-oauth").expect("read oauth credential") {
            ProviderCredential::OAuth(tokens) => {
                assert_eq!(tokens.access_token.as_str(), "access-token");
                assert_eq!(tokens.refresh_token.as_str(), "refresh-token");
                assert_eq!(tokens.expires_at, 1_735_689_600);
                assert_eq!(tokens.account_id.as_deref(), Some("account-1"));
            }
            ProviderCredential::ApiKey(_) => panic!("grok-oauth entry must stay OAuth tokens"),
        }
        assert!(matches!(
            read_credential(&path, "gemini"),
            Err(CredentialError::Missing)
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn configured_credential_available_follows_the_auth_method() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("credentials.toml");
        write_credential(&path, "grok-oauth", &oauth_tokens()).expect("write oauth credential");
        write_credential(
            &path,
            "deepseek",
            &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
        )
        .expect("write api key credential");

        let oauth_config = AiConfig {
            provider: "grok-oauth".into(),
            auth: AiAuth::OAuth,
            ..AiConfig::default()
        };
        // An OAuth entry for the configured provider counts as available.
        assert!(configured_credential_available(&oauth_config, &path));

        // No entry for the configured provider does not.
        let missing = AiConfig {
            provider: "gemini-oauth".into(),
            ..oauth_config.clone()
        };
        assert!(!configured_credential_available(&missing, &path));

        // An API-key entry cannot satisfy an OAuth config.
        let wrong_kind = AiConfig {
            provider: "deepseek".into(),
            ..oauth_config
        };
        assert!(!configured_credential_available(&wrong_kind, &path));

        // API-key auth keeps the legacy resolution. The env fallback uses
        // PATH as an always-set variable (`env::set_var` is unsafe in edition
        // 2024 and forbidden in this crate).
        let env_config = AiConfig {
            api_key_env: "PATH".into(),
            ..AiConfig::default()
        };
        assert!(configured_credential_available(&env_config, &path));
        let file_config = AiConfig {
            provider: "deepseek".into(),
            api_key_file: Some(PathBuf::from("credentials.toml")),
            ..AiConfig::default()
        };
        assert!(configured_credential_available(&file_config, &path));
    }

    #[test]
    fn merge_write_preserves_other_providers() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("credentials.toml");

        write_credential(
            &path,
            "deepseek",
            &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
        )
        .expect("write deepseek credential");
        write_credential(
            &path,
            "gemini",
            &ProviderCredential::ApiKey(Zeroizing::new("gemini-key".to_owned())),
        )
        .expect("write gemini credential");
        write_credential(
            &path,
            "deepseek",
            &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key-2".to_owned())),
        )
        .expect("replace deepseek credential");

        match read_credential(&path, "gemini").expect("gemini entry preserved") {
            ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "gemini-key"),
            ProviderCredential::OAuth(_) => panic!("gemini entry must stay an API key"),
        }
        match read_credential(&path, "deepseek").expect("deepseek entry replaced") {
            ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "deepseek-key-2"),
            ProviderCredential::OAuth(_) => panic!("deepseek entry must stay an API key"),
        }
    }

    #[test]
    fn first_v2_write_migrates_v1_key_under_legacy_slug() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("credentials.toml");
        write_v1_file(&path, "legacy-secret");

        write_credential(&path, "grok-oauth", &oauth_tokens()).expect("write oauth credential");

        // The migrated key stays reachable for legacy configs (no provider set).
        let config = AiConfig {
            api_key_file: Some(PathBuf::from(&path)),
            ..AiConfig::default()
        };
        assert_eq!(
            load_api_key(&config, &path)
                .expect("migrated key loads")
                .as_str(),
            "legacy-secret"
        );
        match read_credential(&path, LEGACY_PROVIDER_SLUG).expect("migrated entry") {
            ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "legacy-secret"),
            ProviderCredential::OAuth(_) => panic!("migrated entry must be an API key"),
        }
        // The newly written entry is intact as well.
        assert!(matches!(
            read_credential(&path, "grok-oauth").expect("oauth entry"),
            ProviderCredential::OAuth(_)
        ));
    }

    #[test]
    fn delete_credential_removes_entry_then_file() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("credentials.toml");
        write_credential(
            &path,
            "deepseek",
            &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
        )
        .expect("write deepseek credential");
        write_credential(&path, "grok-oauth", &oauth_tokens()).expect("write oauth credential");

        delete_credential(&path, "deepseek").expect("delete deepseek");
        assert!(matches!(
            read_credential(&path, "deepseek"),
            Err(CredentialError::Missing)
        ));
        assert!(matches!(
            read_credential(&path, "grok-oauth").expect("oauth entry preserved"),
            ProviderCredential::OAuth(_)
        ));

        delete_credential(&path, "grok-oauth").expect("delete grok-oauth");
        assert!(!path.exists(), "empty store removes the file");
        assert!(matches!(
            delete_credential(&path, "deepseek"),
            Err(CredentialError::Missing)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn v2_write_enforces_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("nested/credentials.toml");
        write_credential(
            &path,
            "deepseek",
            &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
        )
        .expect("write credential");
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().expect("parent"))
                .expect("parent metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("permissions");
        assert!(matches!(
            read_credential(&path, "deepseek"),
            Err(CredentialError::InsecurePermissions)
        ));
    }

    #[test]
    fn load_api_key_falls_back_to_legacy_slug_when_provider_entry_is_missing() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("credentials.toml");
        // Legacy rotation wrote the key under the reserved slug only.
        write_api_key(&path, "rotated-secret").expect("write legacy credential");

        let config = AiConfig {
            provider: "deepseek".into(),
            api_key_file: Some(PathBuf::from(&path)),
            ..AiConfig::default()
        };
        assert_eq!(
            load_api_key(&config, &path)
                .expect("provider miss falls back to the legacy slug")
                .as_str(),
            "rotated-secret"
        );

        // A provider's own entry still wins over the fallback.
        write_credential(
            &path,
            "deepseek",
            &ProviderCredential::ApiKey(Zeroizing::new("deepseek-key".to_owned())),
        )
        .expect("write provider credential");
        assert_eq!(
            load_api_key(&config, &path)
                .expect("provider entry loads")
                .as_str(),
            "deepseek-key"
        );
    }

    #[test]
    fn concurrent_writes_to_different_slugs_lose_no_entries() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("credentials.toml");
        let iterations = 100;
        let mut handles = Vec::new();
        for slug in ["alpha", "beta"] {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                for round in 0..iterations {
                    write_credential(
                        &path,
                        slug,
                        &ProviderCredential::ApiKey(Zeroizing::new(format!("{slug}-key-{round}"))),
                    )
                    .expect("concurrent write");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("writer thread");
        }
        for slug in ["alpha", "beta"] {
            match read_credential(&path, slug).expect("entry survives concurrent writes") {
                ProviderCredential::ApiKey(key) => {
                    assert_eq!(key.as_str(), format!("{slug}-key-{}", iterations - 1));
                }
                ProviderCredential::OAuth(_) => panic!("entry must stay an API key"),
            }
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let lock = directory.path().join("credentials.toml.lock");
            assert_eq!(
                fs::metadata(&lock)
                    .expect("lock file metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn debug_output_never_contains_secrets() {
        let credential = oauth_tokens();
        let rendered = format!("{credential:?}");
        assert!(!rendered.contains("access-token"));
        assert!(!rendered.contains("refresh-token"));
        assert!(rendered.contains("1735689600"));
    }
}
