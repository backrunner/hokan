---
title: CLI reference
description: Commands and flags for Hokan sessions, installation, configuration, history, command specifications, and updates.
order: 1
keywords:
  - hokan CLI
  - command reference
---

Run `hokan --help` or append `--help` to a subcommand for the complete current argument list.

## Session and installation

| Command | Purpose |
| --- | --- |
| `hokan` | Start a session with the shell selected from `$SHELL` |
| `hokan --shell zsh` | Select `zsh`, `bash`, or `fish` explicitly |
| `hokan --login` | Start the child as a login shell |
| `hokan --version` / `hokan -V` | Print the installed version |
| `hokan install` | Install or update shell integration |
| `hokan install --on-demand` | Install the `hk` command without automatic startup |
| `hokan install --rc-file <path>` | Select an explicit rc file |
| `hokan setup` | Compatibility alias for `install` |
| `hokan init zsh` | Print shell integration code |
| `hokan doctor --json` | Inspect the environment as stable JSON |

`--shell` and `--login` are global flags. For common workflows, see [installation](/docs/getting-started/install) and [shell integration](/docs/guides/shell-integration).

## Configuration and AI

| Command | Purpose |
| --- | --- |
| `hokan config path` | Show the config path |
| `hokan config init` | Create the default config without overwriting an existing file |
| `hokan config show` | Show configuration |
| `hokan config validate` | Validate the config |
| `hokan config ai` | Inspect AI configuration |
| `hokan config ai --disable` | Disable AI |
| `hokan ai setup` | Run the interactive provider wizard |

See [explicit AI actions](/docs/guides/ai) for endpoint, model, environment-variable, and stdin credential options.

## History

| Command | Purpose |
| --- | --- |
| `hokan history import --shell zsh` | Import a supported shell's history |
| `hokan history import --path <path> --shell bash` | Import a specific history file |
| `hokan history stats --json` | Report statistics |
| `hokan history prune --keep 10000` | Retain a bounded number of entries |
| `hokan history repair` | Repair an incomplete final record |
| `hokan history compact` | Merge duplicate events into an atomic snapshot |
| `hokan history clear --yes` | Clear Hokan's own history store |

Clearing Hokan history does not clear the source shell's history file.

## Command specifications

```bash
hokan spec list
hokan spec show ls
hokan spec validate
```

Specs live in the configured `specs` directory. For the schema and examples, see the [built-in command specifications](https://github.com/backrunner/hokan/blob/main/assets/specs/common/core.toml).

## Upgrades

```bash
hokan upgrade --check
hokan upgrade
hokan upgrade --channel beta
```

`--check` only reports availability. `--channel` persists a stable or beta selection. `--force` reinstalls the current release, and `--yes` skips the upgrade confirmation. See [updates and maintenance](/docs/guides/updates).

## Uninstall

Remove installer-managed integrations, executable, man page, and updater backup:

```bash
hokan uninstall
```

Configuration, credentials, custom specs, history, and diagnostics are preserved. Keep the executable and remove only shell integration:

```bash
hokan uninstall --integration-only
```

Package-manager installations remain owned by their package manager. For Cargo:

```bash
hokan uninstall
cargo uninstall hokan
```
