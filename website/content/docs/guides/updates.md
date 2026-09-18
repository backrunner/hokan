---
title: Updates and maintenance
description: Check for Hokan releases, choose a release channel, and maintain local history.
order: 3
---


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

When automatic updates are enabled, each new Hokan session starts a silent
background check. Long-running sessions revisit the shared cache every minute
and check the network when the configured interval expires. Checks and failed retries are
limited to once every 30 minutes by default; running sessions keep their
current binary, and the update takes effect on the next launch. Updates use an
installation lock, keep a `.bak` backup, and stage beside the installed binary
so a cache on another disk works too. Known package-manager installations,
including writable Homebrew Cellar paths, must be upgraded through their
package manager. Self-updates require a user-owned installation directory
without group/other write access and preserve the executable's permissions.

Update locks reject links and special files; a busy installation lock times
out after five seconds. Cache reads are bounded and reject links and special
files. A failed download, checksum, or binary smoke test leaves the installed
binary unchanged. Manual upgrades install the version shown at confirmation.

New beta installations default to the beta channel; stable builds default to
stable. An explicitly configured channel is preserved. Existing beta users
whose config says `channel = "stable"` can opt into beta updates once with:

```bash
hokan upgrade --channel beta --yes
```

Set `HOKAN_NO_AUTO_UPDATE=1` to disable automatic updating for one session, or
configure `[update]` (set `enabled = false` to disable it persistently):

```toml
[update]
enabled = true
channel = "beta"
interval_secs = 1800
```
