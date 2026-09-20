# v0.1.0-beta.13

This beta reduces completion work during rapid typing and keeps the suggestion
list steadier while providers and shell redraws catch up.

- Completion sources now share cumulative ranking work at approximately one
  frame per update. The first usable batch stays immediate, the final batch
  includes all collected candidates, and unread results are replaced by the
  newest batch. Suppressed queries cancel queued provider work.
- Refreshing a visible list briefly coalesces partial results instead of
  repeatedly shrinking and expanding its rows. Empty intermediate batches
  preserve the visible list; only the final result closes an empty list.
  Existing buffer checks continue to reject stale candidate activation.
- Closing an empty list also discards queued frames, preventing old rows from
  reappearing. Frames invalidated by intervening shell output are retried until
  committed, even when no further provider result arrives.
- Shell output that can scroll the screen erases the overlay before forwarding
  those bytes, preventing suggestion cells from leaking into scrollback.
- Git recommendations chosen from repository state retain their priority when
  generic command help finishes loading, keeping `git init` and `git clone`
  visible outside a repository.
- Installation checks Nerd Font availability and installs the pinned,
  checksum-verified Symbols-Only fonts into the user font directory when local
  coverage is missing. Remote sessions keep font setup on the client terminal.
- The TLS dependencies are updated to rustls 0.23.45 and rustls-webpki 0.103.15,
  addressing RUSTSEC-2026-0285.
- Selecting a `cd` suggestion with Enter now changes directory immediately;
  Tab continues to insert it. This behavior is configurable.
- Terminal titles follow the foreground program and return to the shell title
  when it exits, while preserving titles set by applications themselves.
- Update checks continue during long-running sessions. Manual and automatic
  updates follow the installed binary's channel, ignore legacy channel pins,
  and leave the current installation unchanged when a channel has no releases.
- Installation and upgrades validate writable directories, reject unsafe local
  files, serialize replacement, and preserve a backup of the previous binary.

In a local macOS release-mode benchmark with eight synthetic sources and 64
candidates per source, median completion pipeline time decreased from 2.73 ms
to 0.73 ms, and emitted batches per query decreased from eight to two. This
measures collection and ranking, not terminal transport or end-to-end input
latency. The repeatable benchmark is `tests/completion_performance.rs`.

Real-terminal certification remains subject to the
[compatibility matrix](compatibility.md); this beta does not expand those
certification claims.

Existing beta users can update with:

```sh
hokan upgrade --channel beta --yes
```

Automatic updates take effect on the next Hokan launch.
