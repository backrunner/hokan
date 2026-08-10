mod ai;
mod config;
mod doctor;
mod history;
mod install;
mod integration;
mod specs;
mod upgrade;

use std::{io::Write, path::PathBuf};

use clap::{Parser, Subcommand};

use crate::{app::SessionOptions, shell::ShellKind};

#[derive(Debug, Parser)]
#[command(
    name = "hokan",
    version,
    about = "Shell-aware terminal completion overlay"
)]
pub struct Cli {
    /// Interactive child shell; defaults to $SHELL.
    #[arg(long, global = true, value_enum)]
    shell: Option<ShellKind>,

    /// Start the child as a login shell.
    #[arg(long, global = true)]
    login: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print shell integration code.
    Init {
        #[arg(value_enum)]
        shell: ShellKind,
    },

    /// Install Hokan for the selected shell, with an rc-file backup.
    #[command(visible_alias = "setup")]
    Install {
        /// Write integration to this file instead of the shell default.
        #[arg(long)]
        rc_file: Option<PathBuf>,
        /// Install only an `hk` command instead of auto-starting Hokan.
        #[arg(long)]
        on_demand: bool,
        /// Record a release-installer-owned binary for complete uninstallation.
        #[arg(long, hide = true)]
        managed_install: bool,
        /// Man page owned by the release installer.
        #[arg(long, hide = true, requires = "managed_install")]
        man_page: Option<PathBuf>,
    },

    /// Remove Hokan; installer-managed binaries are removed, while user data is preserved.
    Uninstall {
        /// Remove shell integration only, preserving the executable.
        #[arg(long)]
        integration_only: bool,
        /// Remove integration from this file instead of the shell default.
        #[arg(long)]
        rc_file: Option<PathBuf>,
    },

    /// Inspect whether the current process can host a Hokan terminal session.
    Doctor {
        /// Emit a stable JSON object for automation.
        #[arg(long)]
        json: bool,
    },

    /// Inspect or initialize configuration.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Configure AI providers and credentials interactively.
    #[command(subcommand)]
    Ai(AiCommand),

    /// Import and maintain Hokan's history store.
    #[command(subcommand)]
    History(HistoryCommand),

    /// Inspect and validate command specifications.
    #[command(subcommand)]
    Spec(SpecCommand),

    /// Check for and install updates from GitHub releases.
    Upgrade {
        /// Only report what is available; never download or install.
        #[arg(long, conflicts_with = "force")]
        check: bool,
        /// Track this release channel (stable or beta); persisted to the config.
        #[arg(long)]
        channel: Option<String>,
        /// Reinstall even when the latest version equals the current one.
        #[arg(long)]
        force: bool,
        /// Skip the confirmation prompt (scripted usage).
        #[arg(long)]
        yes: bool,
        /// Headless background check spawned at session start.
        #[arg(long, hide = true)]
        auto: bool,
    },

    /// Internal shell IPC.
    #[command(hide = true, subcommand)]
    Ipc(IpcCommand),
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Path,
    Show,
    Validate,
    Init,
    /// Inspect or update the OpenAI-compatible endpoint and credential source.
    Ai {
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, conflicts_with = "api_key_stdin")]
        api_key_env: Option<String>,
        /// Read one API key from stdin and store it in private credentials.toml.
        #[arg(long, conflicts_with = "api_key_env")]
        api_key_stdin: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AiCommand {
    /// Interactive guided setup for AI providers and credentials.
    Setup,
}

#[derive(Debug, Subcommand)]
enum HistoryCommand {
    Import {
        #[arg(long, value_enum)]
        shell: Option<ShellKind>,
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Stats {
        #[arg(long)]
        json: bool,
    },
    Prune {
        #[arg(long, default_value_t = 10_000)]
        keep: usize,
    },
    /// Repair an incomplete final record without touching earlier data.
    Repair,
    /// Merge duplicate event records into an atomic snapshot.
    Compact,
    Clear {
        /// Confirm that only Hokan's own history store should be cleared.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SpecCommand {
    List,
    Show { name: String },
    Validate,
}

#[derive(Debug, Subcommand)]
enum IpcCommand {
    Take {
        #[arg(long)]
        session: String,
    },
}

pub fn run<W: Write>(cli: Cli, output: &mut W) -> crate::Result<Option<SessionOptions>> {
    let session = SessionOptions {
        shell: cli.shell,
        login: cli.login,
    };
    match cli.command {
        None => Ok(Some(session)),
        Some(Command::Init { shell }) => {
            output.write_all(crate::shell::init_script(shell).as_bytes())?;
            Ok(None)
        }
        Some(Command::Install {
            rc_file,
            on_demand,
            managed_install,
            man_page,
        }) => {
            install::run_install(
                output,
                session.shell,
                rc_file.as_deref(),
                on_demand,
                managed_install,
                man_page.as_deref(),
            )?;
            Ok(None)
        }
        Some(Command::Uninstall {
            integration_only,
            rc_file,
        }) => {
            install::run_uninstall(output, session.shell, rc_file.as_deref(), integration_only)?;
            Ok(None)
        }
        Some(Command::Doctor { json }) => {
            doctor::write_report(output, json)?;
            Ok(None)
        }
        Some(Command::Config(command)) => {
            config::run(output, command)?;
            Ok(None)
        }
        Some(Command::Ai(command)) => {
            // The wizard prompts inline and needs immediate echo, so it owns
            // stdio directly instead of the buffered `output` path.
            ai::run(command)?;
            Ok(None)
        }
        Some(Command::History(command)) => {
            history::run(output, command, session.shell)?;
            Ok(None)
        }
        Some(Command::Spec(command)) => {
            specs::run(output, command)?;
            Ok(None)
        }
        Some(Command::Upgrade {
            check,
            channel,
            force,
            yes,
            auto,
        }) => {
            // Like the AI wizard, upgrade prompts inline and writes directly
            // to stdio, so the buffered `output` path stays empty.
            upgrade::run(upgrade::UpgradeArgs {
                check,
                channel,
                force,
                yes,
                auto,
            })?;
            Ok(None)
        }
        Some(Command::Ipc(IpcCommand::Take { session })) => {
            output.write_all(
                crate::shell::ShellSession::take_edit_from_environment(&session)?.as_bytes(),
            )?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_starts_a_session() {
        let cli = Cli::try_parse_from(["hokan"]).expect("CLI should parse");
        let mut output = Vec::new();
        let session = run(cli, &mut output)
            .expect("dispatch should succeed")
            .expect("session action");
        assert_eq!(session.shell, None);
        assert!(output.is_empty());
    }

    #[test]
    fn version_flags_report_the_package_version() {
        for flag in ["--version", "-V"] {
            let error = Cli::try_parse_from(["hokan", flag]).expect_err("version exits early");
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
            assert_eq!(
                error.to_string(),
                format!("hokan {}\n", env!("CARGO_PKG_VERSION"))
            );
        }
    }

    #[test]
    fn init_outputs_versioned_shell_code() {
        let cli = Cli::try_parse_from(["hokan", "init", "zsh"]).expect("CLI should parse");
        let mut output = Vec::new();
        assert!(run(cli, &mut output).expect("init").is_none());
        let output = String::from_utf8(output).expect("UTF-8 script");
        assert!(output.contains("protocol 2"));
        assert!(output.contains("BUFFER"));
    }

    #[test]
    fn install_is_primary_and_setup_remains_an_alias() {
        for command in ["install", "setup"] {
            let cli = Cli::try_parse_from(["hokan", command, "--on-demand"])
                .expect("install command should parse");
            assert!(matches!(
                cli.command,
                Some(Command::Install {
                    on_demand: true,
                    ..
                })
            ));
        }
    }

    #[test]
    fn uninstall_no_longer_requires_integration_only() {
        let cli =
            Cli::try_parse_from(["hokan", "uninstall"]).expect("plain uninstall should parse");
        assert!(matches!(
            cli.command,
            Some(Command::Uninstall {
                integration_only: false,
                ..
            })
        ));
    }

    #[test]
    fn doctor_json_is_machine_readable() {
        let cli = Cli::try_parse_from(["hokan", "doctor", "--json"]).expect("CLI should parse");
        let mut output = Vec::new();
        run(cli, &mut output).expect("doctor should run");
        let value: serde_json::Value = serde_json::from_slice(&output).expect("valid JSON");
        assert_eq!(value["protocol_version"], 2);
        assert!(value["shells"].is_object());
    }
}
