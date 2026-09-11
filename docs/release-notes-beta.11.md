# v0.1.0-beta.11

This beta improves nested command help coverage and ships the new
documentation site.

- Probe `<cmd> <sub> --help` for any command whose own `--help` exposed a
  complete Commands section, so self-describing CLIs — including most
  npm-installed tools such as codegraph — now offer scoped subcommands and
  flags instead of going silent after a subcommand. Man-derived subcommand
  lists stay on the curated allowlist.
- Offer `-s`/`--signal` and `-l`/`--list` rows for `kill -`, and show the full
  process command line plus parent PID when selecting a PID.
- Switch the overlay to a restrained truecolor palette matching the
  documentation preview, with a consistent panel background on partially
  filled pages.
- Add the svedocs-powered documentation site under `website/` with a
  path-scoped CI workflow.

Existing beta users whose configuration explicitly tracks stable can switch once:

```sh
hokan upgrade --channel beta --yes
```

Automatic updates run silently when a Hokan session starts, at most once per
configured interval (30 minutes by default). The next launch uses the updated
binary. Set `[update].enabled = false` or `HOKAN_NO_AUTO_UPDATE=1` to disable them.
