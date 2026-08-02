use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::{
    config::{Config, ConfigPaths},
    shell::{PROTOCOL_VERSION, ShellKind},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum CheckLevel {
    Ok,
    Error,
    NotApplicable,
}

impl CheckLevel {
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::NotApplicable => "n/a",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct Check {
    level: CheckLevel,
    detail: String,
}

impl Check {
    fn new(level: CheckLevel, detail: impl Into<String>) -> Self {
        Self {
            level,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ShellIntegrationReport {
    active: bool,
    hook: Check,
    protocol: Check,
    session_directory: Check,
    control_channel: Check,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    hokann_version: &'static str,
    protocol_version: u8,
    os: &'static str,
    architecture: &'static str,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
    term: Option<String>,
    configured_shell: Option<String>,
    shells: BTreeMap<&'static str, bool>,
    shell_capabilities: BTreeMap<&'static str, &'static str>,
    terminal_session_ready: bool,
    synchronized_output: &'static str,
    config_path: Option<String>,
    config: Check,
    key_bindings: Check,
    data_directories: BTreeMap<&'static str, Check>,
    debug_logging: Check,
    ai: Check,
    shell_integration: ShellIntegrationReport,
}

pub fn write_report(output: &mut dyn Write, json: bool) -> crate::Result<()> {
    let report = collect();
    if json {
        serde_json::to_writer_pretty(&mut *output, &report)?;
        writeln!(output)?;
        return Ok(());
    }

    writeln!(
        output,
        "Hokann {} (protocol v{})",
        report.hokann_version, report.protocol_version
    )?;
    writeln!(output, "platform: {} / {}", report.os, report.architecture)?;
    writeln!(
        output,
        "tty: stdin={} stdout={}",
        yes_no(report.stdin_is_tty),
        yes_no(report.stdout_is_tty)
    )?;
    writeln!(
        output,
        "TERM: {}",
        report.term.as_deref().unwrap_or("not set")
    )?;
    writeln!(
        output,
        "shells: zsh={} bash={} fish={}",
        yes_no(report.shells["zsh"]),
        yes_no(report.shells["bash"]),
        yes_no(report.shells["fish"])
    )?;
    writeln!(
        output,
        "shell sync: zsh={} bash={} fish={}",
        report.shell_capabilities["zsh"],
        report.shell_capabilities["bash"],
        report.shell_capabilities["fish"]
    )?;
    writeln!(
        output,
        "terminal session: {}",
        if report.terminal_session_ready {
            "ready"
        } else {
            "unavailable in this process"
        }
    )?;
    writeln!(
        output,
        "synchronized output: {}",
        report.synchronized_output
    )?;
    write_check(output, "config", &report.config)?;
    if let Some(path) = &report.config_path {
        writeln!(output, "config path: {path}")?;
    }
    write_check(output, "key bindings", &report.key_bindings)?;
    for (name, check) in &report.data_directories {
        write_check(output, &format!("{name} directory"), check)?;
    }
    write_check(output, "debug logging", &report.debug_logging)?;
    write_check(output, "AI", &report.ai)?;
    write_check(output, "shell hook", &report.shell_integration.hook)?;
    write_check(output, "shell protocol", &report.shell_integration.protocol)?;
    write_check(
        output,
        "session directory",
        &report.shell_integration.session_directory,
    )?;
    write_check(
        output,
        "control channel",
        &report.shell_integration.control_channel,
    )?;
    Ok(())
}

fn write_check(output: &mut dyn Write, name: &str, check: &Check) -> std::io::Result<()> {
    writeln!(output, "{name}: {} - {}", check.level.label(), check.detail)
}

fn collect() -> DoctorReport {
    let term = env::var("TERM").ok();
    let stdin_is_tty = std::io::stdin().is_terminal();
    let stdout_is_tty = crate::terminal::process_stdout_is_terminal();
    let mut shells = BTreeMap::new();
    for shell in ["zsh", "bash", "fish"] {
        shells.insert(shell, find_on_path(shell).is_some());
    }
    let shell_capabilities = BTreeMap::from([
        ("bash", "mirrored-emacs"),
        ("fish", "mirrored-default"),
        ("zsh", "exact-zle"),
    ]);
    let (paths, config, config_check, key_bindings) = inspect_config();
    let data_directories = inspect_data_directories(paths.as_ref());
    let debug_logging = inspect_debug_logging(config.as_ref(), paths.as_ref());
    let ai = inspect_ai(config.as_ref(), paths.as_ref());
    let shell_integration = inspect_shell_integration();
    DoctorReport {
        hokann_version: env!("CARGO_PKG_VERSION"),
        protocol_version: PROTOCOL_VERSION,
        os: env::consts::OS,
        architecture: env::consts::ARCH,
        stdin_is_tty,
        stdout_is_tty,
        term: term.clone(),
        configured_shell: env::var("SHELL").ok(),
        terminal_session_ready: stdin_is_tty
            && stdout_is_tty
            && term.as_deref().is_some_and(|value| value != "dumb")
            && configured_shell_ready(&shells),
        synchronized_output: "runtime-probe-required",
        config_path: paths
            .as_ref()
            .map(|paths| paths.config_file.display().to_string()),
        config: config_check,
        key_bindings,
        data_directories,
        debug_logging,
        ai,
        shell_integration,
        shells,
        shell_capabilities,
    }
}

fn configured_shell_ready(shells: &BTreeMap<&'static str, bool>) -> bool {
    env::var("SHELL")
        .ok()
        .and_then(|shell| shell.parse::<ShellKind>().ok())
        .is_some_and(|shell| shells.get(shell.name()).copied().unwrap_or(false))
}

fn inspect_config() -> (Option<ConfigPaths>, Option<Config>, Check, Check) {
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

fn inspect_data_directories(paths: Option<&ConfigPaths>) -> BTreeMap<&'static str, Check> {
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
enum DirectoryPolicy {
    Private,
    OwnerOnlyWrites,
}

fn inspect_directory(path: &Path, policy: DirectoryPolicy) -> Check {
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

fn inspect_ai(config: Option<&Config>, paths: Option<&ConfigPaths>) -> Check {
    let (Some(config), Some(paths)) = (config, paths) else {
        return Check::new(CheckLevel::Error, "configuration is unavailable");
    };
    let file_configured = config.ai.api_key_file.is_some();
    if !config.ai.enabled && !file_configured {
        return Check::new(CheckLevel::NotApplicable, "disabled; no credential is read");
    }
    match crate::config::load_api_key(&config.ai, &paths.credentials_file) {
        Ok(_) if config.ai.enabled => Check::new(
            CheckLevel::Ok,
            "enabled; endpoint, model, and credential are valid",
        ),
        Ok(_) => Check::new(
            CheckLevel::Ok,
            "disabled; configured credential file is private and valid",
        ),
        Err(error) => Check::new(CheckLevel::Error, error.to_string()),
    }
}

fn inspect_debug_logging(config: Option<&Config>, paths: Option<&ConfigPaths>) -> Check {
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

fn inspect_shell_integration() -> ShellIntegrationReport {
    let active = env::var_os("HOKANN_ACTIVE").is_some();
    if !active {
        let inactive = || {
            Check::new(
                CheckLevel::NotApplicable,
                "not inside a running Hokann child shell",
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

    let hook = if env::var_os("HOKANN_SESSION_TOKEN").is_some() {
        Check::new(CheckLevel::Ok, "session marker and token are present")
    } else {
        Check::new(CheckLevel::Error, "HOKANN_SESSION_TOKEN is missing")
    };
    let protocol = match env::var("HOKANN_PROTOCOL_VERSION") {
        Ok(value) if value == PROTOCOL_VERSION.to_string() => {
            Check::new(CheckLevel::Ok, format!("protocol v{PROTOCOL_VERSION}"))
        }
        Ok(value) => Check::new(
            CheckLevel::Error,
            format!("hook protocol {value:?} does not match v{PROTOCOL_VERSION}"),
        ),
        Err(_) => Check::new(CheckLevel::Error, "HOKANN_PROTOCOL_VERSION is missing"),
    };
    let session_path = env::var_os("HOKANN_SESSION_DIR").map(PathBuf::from);
    let session_directory = session_path.as_ref().map_or_else(
        || Check::new(CheckLevel::Error, "HOKANN_SESSION_DIR is missing"),
        |path| inspect_directory(path, DirectoryPolicy::Private),
    );
    let control_channel = match (
        session_path.as_deref(),
        env::var_os("HOKANN_CONTROL_FIFO").map(PathBuf::from),
    ) {
        (_, None) => Check::new(CheckLevel::Error, "HOKANN_CONTROL_FIFO is missing"),
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
fn inspect_control_channel(session: Option<&Path>, path: &Path) -> Check {
    use std::os::unix::fs::FileTypeExt;

    if session.is_none_or(|session| path.parent() != Some(session)) {
        return Check::new(
            CheckLevel::Error,
            "control FIFO is outside HOKANN_SESSION_DIR",
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
fn inspect_control_channel(_: Option<&Path>, _: &Path) -> Check {
    Check::new(CheckLevel::NotApplicable, "FIFO checks unavailable")
}

fn find_on_path(command: &str) -> Option<PathBuf> {
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

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn directory_checks_enforce_private_state_and_non_writable_config() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private permissions");
        assert_eq!(
            inspect_directory(directory.path(), DirectoryPolicy::Private).level,
            CheckLevel::Ok
        );
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755))
            .expect("broad permissions");
        assert_eq!(
            inspect_directory(directory.path(), DirectoryPolicy::Private).level,
            CheckLevel::Error
        );
        assert_eq!(
            inspect_directory(directory.path(), DirectoryPolicy::OwnerOnlyWrites).level,
            CheckLevel::Ok
        );
    }

    #[cfg(unix)]
    #[test]
    fn control_channel_must_be_a_fifo_inside_the_private_session() {
        use nix::{sys::stat::Mode, unistd::mkfifo};

        let directory = tempfile::tempdir().expect("directory");
        let fifo = directory.path().join("control.fifo");
        mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).expect("FIFO");
        assert_eq!(
            inspect_control_channel(Some(directory.path()), &fifo).level,
            CheckLevel::Ok
        );
        assert_eq!(
            inspect_control_channel(Some(Path::new("/elsewhere")), &fifo).level,
            CheckLevel::Error
        );
    }

    #[test]
    fn disabled_ai_does_not_require_or_read_a_default_environment_key() {
        let directory = tempfile::tempdir().expect("directory");
        let paths = ConfigPaths {
            config_file: directory.path().join("config.toml"),
            credentials_file: directory.path().join("credentials.toml"),
            specs_directory: directory.path().join("specs"),
            state_directory: directory.path().join("state"),
            cache_directory: directory.path().join("cache"),
        };
        assert_eq!(
            inspect_ai(Some(&Config::default()), Some(&paths)).level,
            CheckLevel::NotApplicable
        );
    }

    #[test]
    fn debug_logging_reports_disabled_and_enabled_policy() {
        let directory = tempfile::tempdir().expect("directory");
        let paths = ConfigPaths {
            config_file: directory.path().join("config.toml"),
            credentials_file: directory.path().join("credentials.toml"),
            specs_directory: directory.path().join("specs"),
            state_directory: directory.path().join("state"),
            cache_directory: directory.path().join("cache"),
        };
        let mut config = Config::default();
        let disabled = inspect_debug_logging(Some(&config), Some(&paths));
        assert_eq!(disabled.level, CheckLevel::NotApplicable);
        assert!(disabled.detail.contains("no log file is created"));

        config.logging.enabled = true;
        let enabled = inspect_debug_logging(Some(&config), Some(&paths));
        assert_eq!(enabled.level, CheckLevel::Ok);
        assert!(enabled.detail.contains("1048576 bytes per file"));
        assert!(enabled.detail.contains("exclude query text"));
    }
}
