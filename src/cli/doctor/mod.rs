mod checks;
#[cfg(test)]
mod tests;
mod zsh;

use std::{
    collections::BTreeMap,
    env,
    io::{IsTerminal, Write},
    path::PathBuf,
};

use serde::Serialize;

use crate::shell::PROTOCOL_VERSION;

use checks::{
    configured_shell_ready, find_on_path, inspect_ai, inspect_ai_details, inspect_config,
    inspect_data_directories, inspect_debug_logging, inspect_shell_integration, inspect_update,
};
use zsh::inspect_zsh_rc_files;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum CheckLevel {
    Ok,
    Warn,
    Error,
    NotApplicable,
}

impl CheckLevel {
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::NotApplicable => "n/a",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct Check {
    pub(super) level: CheckLevel,
    pub(super) detail: String,
}

impl Check {
    pub(super) fn new(level: CheckLevel, detail: impl Into<String>) -> Self {
        Self {
            level,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct ShellIntegrationReport {
    pub(super) active: bool,
    pub(super) hook: Check,
    pub(super) protocol: Check,
    pub(super) session_directory: Check,
    pub(super) control_channel: Check,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    hokan_version: &'static str,
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
    update: Check,
    #[serde(skip_serializing_if = "Option::is_none")]
    update_channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update_interval_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    update_latest_known: Option<String>,
    update_exe: Check,
    ai: Check,
    #[serde(skip_serializing_if = "Option::is_none")]
    ai_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ai_auth: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ai_credential: Option<String>,
    shell_integration: ShellIntegrationReport,
    zsh_setup_mode: Check,
    zsh_plugin_conflicts: Vec<Check>,
    zsh_theme: Check,
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
        "Hokan {} (protocol v{})",
        report.hokan_version, report.protocol_version
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
    write_check(output, "auto-update", &report.update)?;
    if let Some(channel) = &report.update_channel {
        writeln!(output, "update channel: {channel}")?;
    }
    if let Some(interval) = report.update_interval_secs {
        writeln!(output, "update interval: {interval}s")?;
    }
    if let Some(latest) = &report.update_latest_known {
        writeln!(output, "update latest known: v{latest}")?;
    }
    write_check(output, "update install path", &report.update_exe)?;
    write_check(output, "AI", &report.ai)?;
    if let Some(provider) = &report.ai_provider {
        writeln!(output, "AI provider: {provider}")?;
    }
    if let Some(auth) = report.ai_auth {
        writeln!(output, "AI auth: {auth}")?;
    }
    if let Some(credential) = &report.ai_credential {
        writeln!(output, "AI credential: {credential}")?;
    }
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
    write_check(output, "zsh setup mode", &report.zsh_setup_mode)?;
    for check in &report.zsh_plugin_conflicts {
        write_check(output, "zsh plugin conflicts", check)?;
    }
    write_check(output, "zsh theme", &report.zsh_theme)?;
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
    let current_exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("hokan"));
    let update = inspect_update(config.as_ref(), paths.as_ref(), &current_exe);
    let ai = inspect_ai(config.as_ref(), paths.as_ref());
    let ai_details = inspect_ai_details(config.as_ref(), paths.as_ref());
    let shell_integration = inspect_shell_integration();
    let login_shell = config
        .as_ref()
        .is_some_and(|config| config.core.login_shell);
    let (zsh_setup_mode, zsh_plugin_conflicts, zsh_theme) = inspect_zsh_rc_files(login_shell);
    DoctorReport {
        hokan_version: env!("CARGO_PKG_VERSION"),
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
        update: update.check,
        update_channel: update.channel,
        update_interval_secs: update.interval_secs,
        update_latest_known: update.latest_known,
        update_exe: update.exe,
        ai,
        ai_provider: ai_details
            .as_ref()
            .and_then(|details| details.provider.clone()),
        ai_auth: ai_details.as_ref().map(|details| details.auth),
        ai_credential: ai_details.and_then(|details| details.credential),
        shell_integration,
        zsh_setup_mode,
        zsh_plugin_conflicts,
        zsh_theme,
        shells,
        shell_capabilities,
    }
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}
