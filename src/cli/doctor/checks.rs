use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    config::{AiAuth, Config, ConfigPaths, CredentialError, ProviderCredential},
    shell::{PROTOCOL_VERSION, ShellKind},
};

use super::{Check, CheckLevel, ShellIntegrationReport};

pub(super) fn configured_shell_ready(shells: &BTreeMap<&'static str, bool>) -> bool {
    env::var("SHELL")
        .ok()
        .and_then(|shell| shell.parse::<ShellKind>().ok())
        .is_some_and(|shell| shells.get(shell.name()).copied().unwrap_or(false))
}

pub(super) fn inspect_config() -> (Option<ConfigPaths>, Option<Config>, Check, Check) {
    let paths = match ConfigPaths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            let detail = error.to_string();
            return (
                None,
                None,
                Check::new(CheckLevel::Error, detail.clone()),
                Check::new(CheckLevel::Error, detail),
            );
        }
    };
    if !paths.config_file.exists() {
        return (
            Some(paths),
            Some(Config::default()),
            Check::new(CheckLevel::NotApplicable, "not created; defaults are valid"),
            Check::new(CheckLevel::Ok, "default bindings have no conflicts"),
        );
    }
    match Config::load(&paths.config_file) {
        Ok(config) => (
            Some(paths),
            Some(config),
            Check::new(CheckLevel::Ok, "TOML and values are valid"),
            Check::new(CheckLevel::Ok, "enabled bindings have no conflicts"),
        ),
        Err(error) => {
            let detail = error.to_string();
            (
                Some(paths),
                None,
                Check::new(CheckLevel::Error, detail.clone()),
                Check::new(CheckLevel::Error, format!("not validated: {detail}")),
            )
        }
    }
}

pub(super) fn inspect_data_directories(
    paths: Option<&ConfigPaths>,
) -> BTreeMap<&'static str, Check> {
    let Some(paths) = paths else {
        return BTreeMap::new();
    };
    let mut directories = BTreeMap::new();
    if let Some(config_directory) = paths.config_file.parent() {
        directories.insert(
            "config",
            inspect_directory(config_directory, DirectoryPolicy::OwnerOnlyWrites),
        );
    }
    directories.insert(
        "state",
        inspect_directory(&paths.state_directory, DirectoryPolicy::Private),
    );
    directories.insert(
        "cache",
        inspect_directory(&paths.cache_directory, DirectoryPolicy::OwnerOnlyWrites),
    );
    directories
}

#[derive(Clone, Copy)]
pub(super) enum DirectoryPolicy {
    Private,
    OwnerOnlyWrites,
}

pub(super) fn inspect_directory(path: &Path, policy: DirectoryPolicy) -> Check {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Check::new(CheckLevel::NotApplicable, "not created yet");
        }
        Err(error) => return Check::new(CheckLevel::Error, format!("cannot inspect: {error}")),
    };
    if !metadata.is_dir() {
        return Check::new(CheckLevel::Error, "path exists but is not a directory");
    }
    inspect_directory_metadata(&metadata, policy)
}

#[cfg(unix)]
fn inspect_directory_metadata(metadata: &fs::Metadata, policy: DirectoryPolicy) -> Check {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mode = metadata.permissions().mode() & 0o777;
    if metadata.uid() != nix::unistd::geteuid().as_raw() {
        return Check::new(
            CheckLevel::Error,
            format!("owner differs from the current user; mode {mode:03o}"),
        );
    }
    let forbidden = match policy {
        DirectoryPolicy::Private => mode & 0o077,
        DirectoryPolicy::OwnerOnlyWrites => mode & 0o022,
    };
    if forbidden != 0 {
        let expectation = match policy {
            DirectoryPolicy::Private => "group/other access must be disabled",
            DirectoryPolicy::OwnerOnlyWrites => "group/other write access must be disabled",
        };
        return Check::new(CheckLevel::Error, format!("mode {mode:03o}; {expectation}"));
    }
    Check::new(
        CheckLevel::Ok,
        format!("owned by current user; mode {mode:03o}"),
    )
}

#[cfg(not(unix))]
fn inspect_directory_metadata(_: &fs::Metadata, _: DirectoryPolicy) -> Check {
    Check::new(
        CheckLevel::NotApplicable,
        "ownership and mode checks unavailable",
    )
}

pub(super) fn inspect_ai(config: Option<&Config>, paths: Option<&ConfigPaths>) -> Check {
    let (Some(config), Some(paths)) = (config, paths) else {
        return Check::new(CheckLevel::Error, "configuration is unavailable");
    };
    let file_configured = config.ai.api_key_file.is_some();
    if !config.ai.enabled && !file_configured {
        return Check::new(CheckLevel::NotApplicable, "disabled; no credential is read");
    }
    match inspect_ai_credential(config, paths) {
        Ok(()) if config.ai.enabled => Check::new(
            CheckLevel::Ok,
            "enabled; endpoint, model, and credential are valid",
        ),
        Ok(()) => Check::new(
            CheckLevel::Ok,
            "disabled; configured credential file is private and valid",
        ),
        Err(error) => Check::new(CheckLevel::Error, error.to_string()),
    }
}

/// Validates the credential the way the configured auth method consumes it:
/// API-key configs resolve through `load_api_key`, OAuth configs require a
/// stored OAuth token set for `ai.provider` (an API-key entry does not
/// satisfy an OAuth setup).
fn inspect_ai_credential(config: &Config, paths: &ConfigPaths) -> crate::Result<()> {
    if config.ai.auth == AiAuth::OAuth {
        let provider = config.ai.provider.trim();
        let path = crate::config::resolve_credential_path(&config.ai, &paths.credentials_file)
            .unwrap_or_else(|| paths.credentials_file.clone());
        return match crate::config::read_credential(&path, provider) {
            Ok(ProviderCredential::OAuth(_)) => Ok(()),
            Ok(ProviderCredential::ApiKey(_)) => Err(CredentialError::InvalidFormat),
            Err(error) => Err(error),
        }
        .map_err(|error| crate::Error::Config(error.to_string()));
    }
    crate::config::load_api_key(&config.ai, &paths.credentials_file)
        .map(|_| ())
        .map_err(|error| crate::Error::Config(error.to_string()))
}

/// Provider/auth/credential-source lines reported for an enabled AI config.
/// Never carries secrets: OAuth reports only whether the credentials entry
/// exists, API-key configs report no credential detail (the `ai` check
/// already validates the key without echoing it).
pub(super) struct AiDetails {
    pub(super) provider: Option<String>,
    pub(super) auth: &'static str,
    pub(super) credential: Option<String>,
}

pub(super) fn inspect_ai_details(
    config: Option<&Config>,
    paths: Option<&ConfigPaths>,
) -> Option<AiDetails> {
    let config = config?;
    if !config.ai.enabled {
        return None;
    }
    let provider = config.ai.provider.trim();
    let auth = match config.ai.auth {
        AiAuth::ApiKey => "api-key",
        AiAuth::OAuth => "oauth",
    };
    let credential = if config.ai.auth == AiAuth::OAuth {
        let paths = paths?;
        let path = crate::config::resolve_credential_path(&config.ai, &paths.credentials_file)
            .unwrap_or_else(|| paths.credentials_file.clone());
        let detail = match crate::config::read_credential(&path, provider) {
            Ok(ProviderCredential::OAuth(_)) => {
                format!("{} entry for {provider} is present", path.display())
            }
            Ok(ProviderCredential::ApiKey(_)) => {
                format!(
                    "{} entry for {provider} is an API key, not OAuth tokens",
                    path.display()
                )
            }
            Err(CredentialError::Missing) => {
                format!("no entry for {provider} in {}", path.display())
            }
            Err(error) => format!("{} is unreadable: {error}", path.display()),
        };
        Some(detail)
    } else {
        None
    };
    Some(AiDetails {
        provider: (!provider.is_empty()).then(|| provider.to_owned()),
        auth,
        credential,
    })
}

pub(super) fn inspect_debug_logging(config: Option<&Config>, paths: Option<&ConfigPaths>) -> Check {
    let (Some(config), Some(paths)) = (config, paths) else {
        return Check::new(CheckLevel::Error, "configuration is unavailable");
    };
    if !config.logging.enabled {
        return Check::new(
            CheckLevel::NotApplicable,
            "disabled; no log file is created",
        );
    }
    let directory = inspect_directory(&paths.state_directory, DirectoryPolicy::Private);
    if directory.level == CheckLevel::Error {
        return Check::new(
            CheckLevel::Error,
            format!("state directory is unsafe: {}", directory.detail),
        );
    }
    Check::new(
        CheckLevel::Ok,
        format!(
            "enabled; {} bytes per file with {} rotations; typed events exclude query text",
            config.logging.max_bytes, config.logging.rotations
        ),
    )
}

/// Update-section fields reported alongside the `update` check line.
pub(super) struct UpdateDetails {
    pub(super) check: Check,
    pub(super) channel: Option<String>,
    pub(super) interval_secs: Option<u64>,
    /// Latest version the most recent check recorded in the TTL cache.
    pub(super) latest_known: Option<String>,
    pub(super) exe: Check,
}

pub(super) fn inspect_update(
    config: Option<&Config>,
    paths: Option<&ConfigPaths>,
    exe: &Path,
) -> UpdateDetails {
    let exe_check = inspect_update_exe(exe);
    let (Some(config), Some(paths)) = (config, paths) else {
        return UpdateDetails {
            check: Check::new(CheckLevel::Error, "configuration is unavailable"),
            channel: None,
            interval_secs: None,
            latest_known: None,
            exe: exe_check,
        };
    };
    let check = if config.update.enabled {
        Check::new(
            CheckLevel::Ok,
            format!(
                "enabled; channel {}, checks every {}s",
                config.update.channel, config.update.interval_secs
            ),
        )
    } else {
        Check::new(
            CheckLevel::NotApplicable,
            "disabled; no background update checks run",
        )
    };
    let latest_known =
        crate::update::read_cached_check(&paths.state_directory).map(|cached| cached.latest_known);
    UpdateDetails {
        check,
        channel: Some(config.update.channel.clone()),
        interval_secs: Some(config.update.interval_secs),
        latest_known,
        exe: exe_check,
    }
}

/// Writability of the running executable's directory: package-manager
/// installs live in system paths that self-update must not touch.
fn inspect_update_exe(exe: &Path) -> Check {
    let directory = exe.parent().unwrap_or(exe);
    if crate::update::directory_writable(directory) {
        Check::new(
            CheckLevel::Ok,
            format!(
                "{} is writable; self-updates can apply",
                directory.display()
            ),
        )
    } else {
        Check::new(
            CheckLevel::Warn,
            format!(
                "{} is not writable; upgrade through your package manager",
                directory.display()
            ),
        )
    }
}

pub(super) fn inspect_shell_integration() -> ShellIntegrationReport {
    let active = env::var_os("HOKAN_ACTIVE").is_some();
    if !active {
        let inactive = || {
            Check::new(
                CheckLevel::NotApplicable,
                "not inside a running Hokan child shell",
            )
        };
        return ShellIntegrationReport {
            active,
            hook: inactive(),
            protocol: inactive(),
            session_directory: inactive(),
            control_channel: inactive(),
        };
    }

    let hook = if env::var_os("HOKAN_SESSION_TOKEN").is_some() {
        Check::new(CheckLevel::Ok, "session marker and token are present")
    } else {
        Check::new(CheckLevel::Error, "HOKAN_SESSION_TOKEN is missing")
    };
    let protocol = match env::var("HOKAN_PROTOCOL_VERSION") {
        Ok(value) if value == PROTOCOL_VERSION.to_string() => {
            Check::new(CheckLevel::Ok, format!("protocol v{PROTOCOL_VERSION}"))
        }
        Ok(value) => Check::new(
            CheckLevel::Error,
            format!("hook protocol {value:?} does not match v{PROTOCOL_VERSION}"),
        ),
        Err(_) => Check::new(CheckLevel::Error, "HOKAN_PROTOCOL_VERSION is missing"),
    };
    let session_path = env::var_os("HOKAN_SESSION_DIR").map(PathBuf::from);
    let session_directory = session_path.as_ref().map_or_else(
        || Check::new(CheckLevel::Error, "HOKAN_SESSION_DIR is missing"),
        |path| inspect_directory(path, DirectoryPolicy::Private),
    );
    let control_channel = match (
        session_path.as_deref(),
        env::var_os("HOKAN_CONTROL_FIFO").map(PathBuf::from),
    ) {
        (_, None) => Check::new(CheckLevel::Error, "HOKAN_CONTROL_FIFO is missing"),
        (session, Some(path)) => inspect_control_channel(session, &path),
    };
    ShellIntegrationReport {
        active,
        hook,
        protocol,
        session_directory,
        control_channel,
    }
}

#[cfg(unix)]
pub(super) fn inspect_control_channel(session: Option<&Path>, path: &Path) -> Check {
    use std::os::unix::fs::FileTypeExt;

    if session.is_none_or(|session| path.parent() != Some(session)) {
        return Check::new(
            CheckLevel::Error,
            "control FIFO is outside HOKAN_SESSION_DIR",
        );
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_fifo() => {
            Check::new(CheckLevel::Ok, "private control FIFO is present")
        }
        Ok(_) => Check::new(CheckLevel::Error, "control path is not a FIFO"),
        Err(error) => Check::new(CheckLevel::Error, format!("cannot inspect FIFO: {error}")),
    }
}

#[cfg(not(unix))]
pub(super) fn inspect_control_channel(_: Option<&Path>, _: &Path) -> Check {
    Check::new(CheckLevel::NotApplicable, "FIFO checks unavailable")
}

pub(super) fn find_on_path(command: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(command))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    executable_permissions(metadata.permissions(), path.as_os_str())
}

#[cfg(unix)]
fn executable_permissions(permissions: fs::Permissions, _: &OsStr) -> bool {
    use std::os::unix::fs::PermissionsExt;
    permissions.mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable_permissions(_: fs::Permissions, path: &OsStr) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
}
