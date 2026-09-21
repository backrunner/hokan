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

Updates default to the installed binary's channel: beta builds check beta
releases, and stable builds check stable releases. Use `--channel` to target
another channel for this invocation. Only a higher semantic version can be
installed across channels; `--force` allows an equal-version reinstall but
never a downgrade. After installation, future checks follow the new binary's
channel. Checking, cancelling, or failing an upgrade never switches channels.
Legacy `[update].channel` values are accepted but ignored.

```bash
hokan upgrade --channel stable --check
hokan upgrade --channel stable --yes
hokan upgrade --channel beta --yes
```

If the requested channel has no published release yet, Hokan reports that
without changing the installation. A beta-only repository does not require
a stable release for beta updates to work.

## Network and proxies

Updates and all other built-in HTTP requests follow macOS HTTP/HTTPS system
proxy settings by default. `HTTP_PROXY` and `HTTPS_PROXY` (including lowercase
forms) override the corresponding system setting; `ALL_PROXY` is a fallback.
Linux uses these environment variables. SOCKS proxies are supported through
proxy variables, for example `HTTPS_PROXY=socks5h://127.0.0.1:1080`.

Use `NO_PROXY=localhost,127.0.0.1,::1` to connect directly to local AI providers.
The HTTP client does not evaluate PAC scripts or macOS proxy exception lists;
configure explicit proxy variables and `NO_PROXY` for those setups. A macOS
SOCKS-only system setting also needs an explicit proxy variable.

HTTP 403 means GitHub or an intervening proxy/gateway refused the request.
GitHub's unauthenticated API allowance is 60 requests per hour per public IP,
shared with other users on the same network or proxy. When response headers
identify a rate limit, Hokan reports it and includes the retry delay when
available. Otherwise, check the proxy and network access; a 403 alone does not
prove that the release channel is invalid or that the API quota is exhausted.

Set `HOKAN_NO_AUTO_UPDATE=1` to disable automatic updating for one session, or
configure `[update]` (set `enabled = false` to disable it persistently):

```toml
[update]
enabled = true
interval_secs = 1800
```
