These fixtures capture command and usage sections from installed CLI help on
native macOS on 2026-09-30. `matrix.json` records the expected canonical command
names for each scope. Descriptions and examples remain in their original layout;
trailing whitespace is stripped and local npm paths are replaced with examples.
Parser tests separately cover padded rows, ANSI styling, and OSC hyperlinks.

The corpus covers 15 applications and 32 help samples with 785 expected command
recommendations. These are sample expectations, not a promise to discover
undocumented commands in every application. Cargo's concise help and complete
installed-command list are separate samples; aliases validate input without
creating duplicate recommendation rows.

| CLI | Captured version |
| --- | --- |
| AIPass | 0.2.0-beta.1 |
| Cargo | 1.98.1 |
| rustup | 1.29.0 |
| uv | 0.12.5 |
| GitHub CLI | 2.98.0 |
| Docker CLI | 29.7.2 |
| Go | 1.27.0 |
| npm | 11.18.0 |
| pnpm | 12.3.4 |
| Bun | 1.1.34 |
| Deno | 2.9.5 |
| Codex CLI | 0.154.0 |
| Claude Code | 2.1.284 |
| RTK | 0.48.0 |
| Apple Swift | 6.4 |

Default tests need none of these applications or their accounts. They check
exact parsed command sets and editable completion candidates from the samples.
An opt-in live check requires installed CLIs matching the snapshots and probes
only help/list output:

```sh
cargo test --locked --lib installed_cli_help_matrix -- --ignored --nocapture --test-threads=1
```

The Docker checks read `--help` output only; they do not create or use containers.
