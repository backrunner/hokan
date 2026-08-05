use std::{collections::BTreeMap, fmt, io::Read, path::Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

use super::io::open_without_following_symlinks;

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
pub(super) struct CredentialFileV2 {
    version: u32,
    #[serde(default)]
    pub(super) providers: BTreeMap<String, ProviderEntry>,
}

#[derive(Serialize)]
pub(super) struct CredentialFileV2Ref<'a> {
    pub(super) version: u32,
    pub(super) providers: &'a BTreeMap<String, ProviderEntry>,
}

/// Serde mirror of [`ProviderCredential`]: exactly one of `api_key` or the
/// `access_token`/`refresh_token`/`expires_at` triple is set per entry.
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderEntry {
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
    pub(super) fn into_credential(self) -> Result<ProviderCredential, CredentialError> {
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
pub(super) enum CredentialStore {
    V1(Zeroizing<String>),
    V2(BTreeMap<String, ProviderEntry>),
}

pub(super) fn validate_credential(credential: &ProviderCredential) -> Result<(), CredentialError> {
    match credential {
        ProviderCredential::ApiKey(key) => validate_secret(key),
        ProviderCredential::OAuth(tokens) => {
            validate_secret(&tokens.access_token)?;
            validate_secret(&tokens.refresh_token)
        }
    }
}

pub(super) fn read_credential_store(path: &Path) -> Result<CredentialStore, CredentialError> {
    let mut file = open_without_following_symlinks(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(CredentialError::NotRegular);
    }
    super::io::validate_private_metadata(&metadata)?;
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
