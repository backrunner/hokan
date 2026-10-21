# v0.1.0

Hokan 0.1.0 is the first stable release. It includes the command discovery
improvements from beta.17 and strengthens the boundaries between shell editing,
foreground programs, and local filesystem completion.

- Filesystem completion now requires a known local-path argument. Package names,
  repository names, image names, expressions, and unknown command arguments no
  longer trigger directory scans merely because they contain `/` or `./`.
  Known file arguments, redirections, and supported local installation paths
  retain completion. Native shell Tab remains available when no Hokan list is open.
- Each prompt owns its completion tasks, AI results, pending Tab actions, and
  confirmations. Results from an earlier prompt cannot act on a later one, and
  foreground confirmation input stays with the running program.
- Delayed terminal color replies are tracked across program exit, including
  fragmented headers, so recognized replies do not become shell input within
  the two-second grace period. Active programs still receive their replies.
- Missing or misspelled paths offer explicit repairs and readable parent paths.
  Accepting a repair only fills the command; it never creates a directory or
  executes the corrected command.
- Command-help probes use the interactive shell's PATH, including script
  interpreters and helpers. Cached help is refreshed when PATH or the executable
  changes.
- Duplicate shell start notifications are handled without disrupting command
  history. Recoverable protocol diagnostics stay in opt-in debug logs.
- Dependencies have been refreshed within the Rust 1.96 compatibility boundary.

发布后的默认更新渠道为 stable。已有 beta 用户可运行：

```sh
hokan upgrade --channel stable --yes
```

Restart Hokan after upgrading. Existing configuration and history are retained.
Unknown command arguments no longer receive speculative filesystem candidates;
define an explicit path argument contract when adding support for a new command.

macOS and Linux automated checks cover shell/PTY behavior, release-mode tests,
formatting, Clippy, packaging, and dependency policy. Terminal-application,
theme, remote-SSH, and hardware certification remains limited to the evidence in
the [compatibility matrix](https://github.com/backrunner/hokan/blob/v0.1.0/docs/compatibility.md).
Publishing a stable version does not mark pending combinations as certified.

Release assets include macOS and Linux archives for x86_64 and aarch64, SPDX
SBOMs, SHA-256 checksums, and an Ed25519 signature verified against the updater's
pinned public key.
