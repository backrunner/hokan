---
title: Configuration
description: Configure Hokan's XDG paths, appearance, completion limits, key bindings, diagnostics, and updates.
order: 2
keywords:
  - Hokan configuration
  - config.toml
  - XDG
---

Hokan reads TOML configuration and applies defaults for omitted settings. Unknown fields are rejected, so validate after editing.

```bash
hokan config path
hokan config init
hokan config show
hokan config validate
```

`config init` creates a default file and refuses to overwrite an existing one.

## File locations

| Data | Default location | Root override |
| --- | --- | --- |
| Configuration | `~/.config/hokan/config.toml` | `XDG_CONFIG_HOME` |
| Private credentials | `~/.config/hokan/credentials.toml` | `XDG_CONFIG_HOME` |
| Custom command specs | `~/.config/hokan/specs/` | `XDG_CONFIG_HOME` |
| History and diagnostics | `~/.local/state/hokan/` | `XDG_STATE_HOME` |
| Cache | `~/.cache/hokan/` | `XDG_CACHE_HOME` |

Credentials and history files must be owned by the current user and private (`0600` or stricter); the state directory is `0700`. See [AI setup](/docs/guides/ai) for the provider wizard and environment-variable credentials.

## Shell and appearance

```toml
[core]
login_shell = false

[ui]
max_rows = 8
max_width = 76
color = "auto"
nerd_fonts = true
show_hidden = false
```

- `core.login_shell = true` loads login-shell configuration, useful for themes initialized in `.zprofile`.
- `max_rows` is the total overlay height, including two border rows; range `3–50`.
- `max_width` has range `40–240` columns.
- `color` accepts `auto`, `always`, or `never`. `NO_COLOR=1` also selects a plain terminal for the AI wizard.
- Set `nerd_fonts = false` if icons appear as empty squares.

## Keys

```toml
[keys]
accept = "tab"
activate = "enter"
up = "up"
down = "down"
page_up = "page-up"
page_down = "page-down"
dismiss = "escape"
history = "ctrl-r"
toggle = "back-tab"
```

Use `disabled` to turn off a binding. Enabled bindings cannot conflict. See [your first session](/docs/getting-started/first-shell#keys) for each action's behavior.

## Completion and history

```toml
[completion]
local_timeout_ms = 100
max_candidates = 1000

[history]
enabled = true
max_command_bytes = 16384
exclude = []
```

Completion timeout accepts `10–5000` milliseconds; the candidate limit accepts `10–10000`. History's command size limit accepts `100–100000` bytes. History exclusions use regular expressions. Local command history can contain sensitive text, so treat it as private data.

## Diagnostic logging

```toml
[logging]
enabled = false
max_bytes = 1048576
rotations = 3
```

Logs are opt-in and bounded. When enabled, they are written to `${XDG_STATE_HOME:-~/.local/state}/hokan/debug.log`. They contain typed event categories and timing data, not query text, history entries, full working directories, HTTP bodies, or environment variable values.

## Automatic updates

```toml
[update]
enabled = true
channel = "beta"
interval_secs = 1800
```

New beta installations use the beta channel; explicit channel choices are preserved. Set `enabled = false` to disable automatic updates, or `HOKAN_NO_AUTO_UPDATE=1` for one session. The updated binary takes effect on the next launch. See [updates and maintenance](/docs/guides/updates).
