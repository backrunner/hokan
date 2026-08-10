<div align="center">

# Hokan

**Shell-aware inline completion for real terminals.**

[![CI](https://github.com/backrunner/hokan/actions/workflows/ci.yml/badge.svg)](https://github.com/backrunner/hokan/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/backrunner/hokan?include_prereleases&sort=semver)](https://github.com/backrunner/hokan/releases)
[![License: BSD-3-Clause](https://img.shields.io/badge/license-BSD--3--Clause-blue.svg)](LICENSE)
[![Rust 1.96+](https://img.shields.io/badge/rust-1.96%2B-orange.svg)](rust-toolchain.toml)
[![macOS and Linux](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg)](docs/compatibility.md)

</div>

Hokan wraps your real zsh, bash, or fish shell in a PTY and renders a compact
completion overlay inside the current terminal. It combines shell history,
command recipes, files, project scripts, Git state, aliases, functions, man
pages, and explicitly requested AI suggestions without replacing your prompt
or launching a GUI.

Hokan `0.1.0-beta.1` is the first public beta. The core terminal recovery path
is covered by real PTY tests, while several terminal, SSH, tmux, fish, and
cross-platform combinations still require release certification. See the
[compatibility matrix](docs/compatibility.md) for the exact status.

## Quick start

Install the first beta on macOS or Linux. The installer detects the OS, CPU
architecture, and current shell; verifies the release against `SHA256SUMS`;
installs into your home directory; and runs `hokan install`.

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/backrunner/hokan/releases/download/v0.1.0-beta.1/hokan-installer.sh \
  | HOKAN_VERSION=0.1.0-beta.1 sh
```

Open a new terminal, or restart the current shell:

```bash
exec "$SHELL" -l
```

Then verify the environment:

```bash
hokan --version
hokan doctor
```

`hokan --version` and its short form `hokan -V` print the installed version.

The default installation uses `~/.local/bin/hokan` and
`~/.local/share/man/man1/hokan.1`. It does not require `sudo`.

Prefer an on-demand `hk` command instead of automatic startup:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/backrunner/hokan/releases/download/v0.1.0-beta.1/hokan-installer.sh \
  | HOKAN_VERSION=0.1.0-beta.1 HOKAN_ON_DEMAND=1 sh
```

## Why Hokan

- **Your shell stays real.** Hokan launches the actual zsh, bash, or fish and
  passes prompt output, themes, Nerd Font glyphs, colors, and control sequences
  through the PTY.
- **Suggestions understand context.** Ranking uses history, previous command
  transitions, failures, project type, Git state, aliases, functions, package
  scripts, files, PIDs, network interfaces, and command argument slots.
- **Selection remains deliberate.** `Tab` inserts text and never executes it.
  High-risk generated commands require a second confirmation before execution.
- **AI is optional and explicit.** Natural language is recognized locally. A
  network request happens only after you select the AI action, and the result
  is inserted for review rather than executed.
- **Terminal recovery is a first-class feature.** Hokan avoids the alternate
  screen, restores terminal state after signals and failures, and falls back
  safely when synchronized output is unavailable.
- **No account or telemetry is required.** Local completion works offline, and
  diagnostic logging is disabled by default.

## Install command

If the binary is already available, install shell integration with:

```bash
hokan install
```

Hokan detects `$SHELL`, writes a versioned managed block at the beginning of
the appropriate rc file, and creates a backup before changing an existing
file. Re-running the command is idempotent and upgrades older managed blocks.

| Shell | Default rc file |
| --- | --- |
| zsh | `${ZDOTDIR:-$HOME}/.zshrc` |
| bash on macOS | `~/.bash_profile` |
| bash on Linux | `~/.bashrc` |
| fish | `${XDG_CONFIG_HOME:-~/.config}/fish/config.fish` |

Useful variants:

```bash
hokan install --shell zsh
hokan install --shell bash --rc-file ~/.bashrc
hokan install --shell fish --on-demand
```

`hokan setup` remains a compatibility alias for `hokan install`.

To bypass automatic startup for one shell process:

```bash
HOKAN_AUTO_START=0 zsh -l
```

You can also skip rc-file changes and launch Hokan directly:

```bash
hokan --shell zsh
```

## Uninstall

For a release installed by the Hokan installer:

```bash
hokan uninstall
```

This removes every integration recorded by the installer, the managed binary,
the man page, and any updater backup. Hokan preserves configuration,
credentials, custom specs, history, and diagnostic logs by default.

To remove only shell integration and keep the executable:

```bash
hokan uninstall --integration-only
```

Package-manager installations remain owned by their package manager. For a
Cargo installation, remove integration first and then remove the package:

```bash
hokan uninstall
cargo uninstall hokan
```

## Install from source

Building from source requires Rust 1.96 or newer:

```bash
cargo install --git https://github.com/backrunner/hokan --locked
hokan install
```

For repository development:

```bash
cargo install --path . --locked
hokan install
```

The release installer supports these optional environment variables:

| Variable | Purpose |
| --- | --- |
| `HOKAN_VERSION` | Install an exact release such as `0.1.0-beta.1` |
| `HOKAN_INSTALL_DIR` | Override the binary directory |
| `HOKAN_MAN_DIR` | Override the man-page directory |
| `HOKAN_SHELL` | Select `zsh`, `bash`, or `fish` |
| `HOKAN_RC_FILE` | Select an explicit shell rc file |
| `HOKAN_ON_DEMAND=1` | Install the `hk` command without automatic startup |

## Completion sources

Hokan merges and ranks candidates from several independent providers:

| Source | Examples |
| --- | --- |
| Shell history | zsh, bash, and fish history with fuzzy search and failure penalties |
| Command specs | Built-in recipes, flags, subcommands, argument slots, and risk levels |
| Help pages | Read-only extraction from man pages for commands without a built-in spec |
| Filesystem | Files, directories, executable scripts, quoting, spaces, and Unicode paths |
| Project metadata | npm, pnpm, yarn, bun, Deno, Cargo, Make, just, and workspace members |
| Git state | Contextual `init`, `clone`, `status`, `add`, `commit`, `push`, `pull`, and more |
| Shell definitions | Aliases, functions, abbreviations, and inferred function argument slots |
| System state | Running processes, PIDs, and network interfaces |
| AI action | An explicit OpenAI-compatible request selected by you |

## Keys

| Key | Action |
| --- | --- |
| `Up` / `Down` | Move the selection |
| `PageUp` / `PageDown` | Move between pages |
| `Tab` | Insert the selected or highest-ranked candidate; never execute it |
| `Enter` | Run typed input, or run the explicitly selected candidate |
| `Esc` | Close the overlay or cancel an AI request |
| `Ctrl-R` | Toggle the history-focused view |
| `Shift-Tab` | Show or hide the completion list |

## Shell and theme compatibility

Hokan runs themes and shell frameworks inside the managed child shell. Prompt
bytes are passed through exactly, including private-use Nerd Font glyphs used
by powerline-style themes.

Completion systems that draw their own suggestions or take over the same keys
can conflict with Hokan. Guard them so they remain active in normal shells but
stay disabled inside Hokan:

```zsh
[[ -z $HOKAN_ACTIVE ]] && source /path/to/zsh-autosuggestions.zsh
```

For zsh themes initialized only from `.zprofile`, set `core.login_shell = true`
so the managed child shell loads the same login configuration. `hokan doctor`
detects this case and known conflicting plugins.

## Configuration

Hokan follows XDG paths. The main configuration is
`~/.config/hokan/config.toml` by default, and private history state is stored
under `~/.local/state/hokan`.

```bash
hokan config init
hokan config show
hokan config validate
hokan spec validate
```

Example UI settings:

```toml
[ui]
max_rows = 8
max_width = 76
nerd_fonts = true
```

Set `nerd_fonts = false` if icons render as empty squares.

Diagnostic logging is opt-in and bounded:

```toml
[logging]
enabled = false
max_bytes = 1048576
rotations = 3
```

When enabled, logs are written to
`${XDG_STATE_HOME:-~/.local/state}/hokan/debug.log`. They contain typed event
categories and timing data, not query text, history entries, full working
directories, HTTP bodies, or environment variable values.

## Optional AI setup

Run the interactive provider wizard in a real terminal:

```bash
hokan ai setup
```

The wizard supports DeepSeek, OpenAI with OAuth, Gemini with OAuth or an API
key, xAI Grok with OAuth or an API key, local Ollama, and custom
OpenAI-compatible endpoints. It tests the connection before atomically writing
the configuration.

For scripted configuration, keep the API key in an environment variable:

```bash
export OPENAI_API_KEY='...'
hokan config ai --enable \
  --endpoint https://api.openai.com/v1 \
  --model gpt-5-mini \
  --api-key-env OPENAI_API_KEY
```

Credentials can also be piped from a password manager into a private `0600`
credentials file:

```bash
password-manager read openai-key | hokan config ai \
  --enable --model gpt-5-mini --api-key-stdin
```

## Maintenance and updates

```bash
hokan doctor --json
hokan history stats
hokan history repair
hokan history compact
hokan spec list
hokan upgrade --check
```

Writable release-installer binaries can update themselves from GitHub
releases. Updates download the matching archive, verify `SHA256SUMS`, run a
binary smoke test, back up the current executable, and replace it atomically.

```bash
hokan upgrade
hokan upgrade --channel beta
```

Automatic checks are enabled by default with a 30-minute cache interval. Set
`HOKAN_NO_AUTO_UPDATE=1` to disable the check for one session, or configure:

```toml
[update]
enabled = true
channel = "stable"
interval_secs = 1800
```

## Development

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo test --release
cargo audit
cargo deny check advisories licenses sources
```

Additional documentation:

- [Compatibility matrix](docs/compatibility.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Release checklist](docs/release-checklist.md)
- [Manual page](docs/hokan.1)
- [Architecture and delivery notes](.agents/README.md)

## Acknowledgments

Inspired by [IRIS](https://github.com/versenilvis/IRIS).

## License

Hokan is licensed under the [BSD 3-Clause License](LICENSE).
