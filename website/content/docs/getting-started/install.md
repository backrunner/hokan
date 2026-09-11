---
title: Install Hokan
description: Install the Hokan beta on macOS or Linux and verify the shell integration.
order: 1
keywords:
  - install Hokan
  - macOS
  - Linux
  - shell integration
---

The release installer detects your operating system, CPU architecture, and current shell. It verifies the release checksum, installs the binary in your home directory, and runs `hokan install`.

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/backrunner/hokan/releases/download/v0.1.0-beta.10/hokan-installer.sh \
  | HOKAN_VERSION=0.1.0-beta.10 sh
```

Open a new terminal, or restart the current shell:

```bash
exec "$SHELL" -l
hokan --version
hokan doctor
```

The default installation uses `~/.local/bin/hokan` and `~/.local/share/man/man1/hokan.1`. It does not require `sudo`.

## On-demand mode

Use an `hk` command instead of starting Hokan for every shell:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/backrunner/hokan/releases/download/v0.1.0-beta.10/hokan-installer.sh \
  | HOKAN_VERSION=0.1.0-beta.10 HOKAN_ON_DEMAND=1 sh
```

## Build from source

Rust 1.96 or newer is required:

```bash
cargo install --git https://github.com/backrunner/hokan --locked
hokan install
```

For a checkout of this repository:

```bash
cargo install --path . --locked
hokan install
```

## Installer options

| Variable | Purpose |
| --- | --- |
| `HOKAN_VERSION` | Install an exact release such as `0.1.0-beta.10` |
| `HOKAN_INSTALL_DIR` | Override the binary directory |
| `HOKAN_MAN_DIR` | Override the man-page directory |
| `HOKAN_SHELL` | Select `zsh`, `bash`, or `fish` |
| `HOKAN_RC_FILE` | Select an explicit shell rc file |
| `HOKAN_ON_DEMAND=1` | Install `hk` without automatic startup |
