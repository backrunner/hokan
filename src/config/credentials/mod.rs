mod io;
mod store;
#[cfg(test)]
mod tests;

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use zeroize::Zeroizing;

use super::{AiAuth, AiConfig};
use io::{lock_credential_store, write_credential_store};
use store::{CredentialStore, ProviderEntry, read_credential_store, validate_credential};

pub(crate) use store::validate_secret;
pub use store::{CredentialError, OAuthTokens, ProviderCredential};

/// Slug under which a legacy v1 single-key file is migrated when a v2 write
/// merges it into the per-provider store. The v1 format carries no provider
/// name, so a reserved slug keeps the key reachable without inventing one.
const LEGACY_PROVIDER_SLUG: &str = "default";

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
