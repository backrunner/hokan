# v0.1.0-beta.16

This beta expands completion lists, fixes cyclic navigation and selection
stability, and authenticates updates with signed release checksums. It also
includes the history and Zsh fixes prepared for beta.15, which was not published.

- Completion lists have no candidate-count limit by default. PATH commands,
  command help, and history no longer stop at hidden 1000, 200, or 50-entry caps.
  Every applicable provider can finish and contribute incremental results.
- Built-in command specifications are supplemented by help and man-page results.
  Typing an exact executable or subcommand still shows longer matching names;
  useful continuations of the exact command retain priority.
- Arrow and page keys wrap through the full list. Initial Page Down selects the
  first page, and Page Up selects the last page. Selection remains on the same
  command when a later provider replaces a duplicate suggestion.
- History navigation handles repeated keys while results load. Zsh clears stale
  inline suggestions after a completion is accepted.
- The updater verifies an Ed25519 signature on `SHA256SUMS` before downloading
  the archive, then verifies its SHA256 before installation. Releases include
  `SHA256SUMS.sig` alongside all four platform archives and SPDX SBOMs.
- Failed GitHub update requests can fall back to HTTPS relays. Set
  `HOKAN_UPDATE_MIRRORS` to customize relay templates, or set it to an empty
  string to disable them. All update sources use the same pinned signing key;
  `--force` cannot bypass verification or allow a downgrade.
- Large completion, history, help-parser, and terminal-test files are split into
  smaller modules without changing their public interfaces.

Existing configurations with a positive `completion.max_candidates` keep that
limit. Set `max_candidates = 0` under `[completion]` to show all matching results;
`ui.max_rows` continues to control only the visible list height.

Real-terminal certification remains subject to the
[compatibility matrix](https://github.com/backrunner/hokan/blob/v0.1.0-beta.16/docs/compatibility.md).
This beta does not expand those certification claims.

Update an existing beta installation with:

```sh
hokan upgrade --channel beta --yes
```

Restart Hokan after upgrading to use the new binary. Older clients can upgrade
directly to this first signed release; this release cannot reinstall unsigned
historical releases.
