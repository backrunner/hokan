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
- Installation checks Nerd Font availability and installs the pinned,
  checksum-verified Symbols-Only fonts into the user font directory when local
  coverage is missing. Remote sessions keep font setup on the client terminal.
- The TLS dependencies are updated to rustls 0.23.45 and rustls-webpki 0.103.15,
  addressing RUSTSEC-2026-0285.

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
