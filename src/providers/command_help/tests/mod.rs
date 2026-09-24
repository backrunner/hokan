use std::{
    ffi::OsString, fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc, time::Duration,
};

use super::scope::help_position_for_arguments;
use super::*;
use super::{
    parsing::{
        MAX_DESCRIPTION_CHARS, flag_takes_separate_value, parse_help_output, parse_man_page,
        shorten, strip_overstrike,
    },
    probe::{
        fetch_help_output, fetch_help_program, fetch_with_fallback, help_probe_arguments,
        looks_like_help_output, merge_help,
    },
};
use crate::{
    completion::{BufferSnapshot, CompletionEngine, SyncQuality},
    shell::ShellKind,
    terminal::{BufferRevision, QueryId},
};

fn bold(text: &str) -> String {
    text.chars()
        .flat_map(|character| [character, '\u{8}', character])
        .collect()
}

fn context(text: &str, query: u64) -> CompletionContext {
    CompletionContext::new(
        QueryId::new(query),
        ShellKind::Zsh,
        PathBuf::from("/tmp"),
        BufferSnapshot::new(
            text,
            text.len(),
            BufferRevision::new(query),
            SyncQuality::Exact,
        )
        .expect("buffer"),
    )
    .expect("context")
}

mod cache;
mod man;
mod modern;
mod probe;
mod provider;
mod scopes;

const KUBECTL_HELP: &str = "\
kubectl controls the Kubernetes cluster manager.

Usage:
  kubectl [flags] [options]

Available Commands:
  apply         Apply a configuration to a resource by file name or stdin
  api-versions  Print the supported API versions on the server
  create        Create a resource from a file or from stdin
  get           Display one or many resources

Flags:
  -h, --help              help for kubectl
      --kubeconfig string  Path to the kubeconfig file

Use \"kubectl <command> --help\" for more information about a given command.
";

const DOCKER_HELP: &str = "\
Usage:  docker [OPTIONS] COMMAND

A self-sufficient runtime for containers

Management Commands:
  builder     Manage builds
  container   Manage containers

Commands:
  attach      Attach local standard input, output, and error streams to a running container
  build       Build an image from a Dockerfile

Global Flags:
      --config string   Location of client config files
  -D, --debug           Enable debug mode
";

const CARGO_HELP: &str = "\
Rust's package manager

Usage: cargo [OPTIONS] [COMMAND]

Commands:
  build, b    Compile the current package
  check, c    Analyze the current package and report errors
  run         Run a binary or example of the local package

Options:
  -V, --version   Print version
";

const AI_CLI_HELP: &str = "\
Usage: ai [OPTIONS] <COMMAND>

Commands:
  exec            Run non-interactively [aliases: e]
  apply           Apply the latest patch to the local
                  working tree [aliases: a]
  update|upgrade  Install the latest version

Arguments:
  [PROMPT]        Optional prompt to start a session

Options:
  -h, --help      Print help
";

const BREW_HELP: &str = "\
Example usage:
  brew search TEXT|/REGEX/
  brew info [FORMULA|CASK...]
  brew install FORMULA|CASK...
  brew update
  brew upgrade [FORMULA|CASK...]
  brew uninstall FORMULA|CASK...
  brew list [FORMULA|CASK...]

Troubleshooting:
  brew config
  brew doctor
  brew install --verbose --debug FORMULA|CASK

Contributing:
  brew create URL [--no-fetch]
  brew edit [FORMULA|CASK...]

Further help:
  brew commands
  brew help [COMMAND]
  man brew
  https://docs.brew.sh
";

const GO_HELP: &str = "\
Usage:
\tgo <command> [arguments]

The commands are:
\tbug         start a bug report
\tbuild       compile packages and dependencies
\tmod         module maintenance

Additional help topics:
\tbuildconstraint build constraints
";

const GH_HELP: &str = "\
USAGE
  gh <command> <subcommand> [flags]

CORE COMMANDS
  auth:          Authenticate gh and git with GitHub
  pr:            Manage pull requests

HELP TOPICS
  accessibility: Learn about accessibility

FLAGS
  --help      Show help for command
";

const GH_PR_HELP: &str = "\
USAGE
  gh pr <command> [flags]

GENERAL COMMANDS
  create:        Create a pull request
  list:          List pull requests

FLAGS
  --help   Show help for command
";

const OPENSSL_HELP: &str = "\
help:

Standard commands
asn1parse         ca                ciphers           x509

Message Digest commands (see the `dgst' command for more details)
md5               sha1              sha256

Cipher commands (see the `enc' command for more details)
aes-128-cbc       aes-256-cbc
";

const SIGNATURE_HELP: &str = "\
Commands:
  agents [options]                      Manage background agents
  i, install                            Install dependencies
  plugin|plugins                        Manage plugins
";
