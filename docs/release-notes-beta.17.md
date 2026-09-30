# v0.1.0-beta.17

This beta improves command discovery across CLI help formats and adds a shared
regression corpus covering 15 applications, rather than special-casing AIPass.

- Recommend documented commands even when their descriptions are empty. This
  fixes AIPass command groups and Codex MCP subcommands, among other CLIs.
- Recognize single-space description columns, long command names, nested command
  categories, ANSI colors, and OSC hyperlinks. Deno, GitHub CLI, and Swift
  examples are included in the regression corpus.
- Keep an indented `usage` command separate from the help's usage heading, and
  avoid treating category headings or description continuations as commands.
- Merge complementary help probes. Cargo's `--list` discovers installed
  extensions omitted from its root help while preserving flags and aliases.
  Explicitly abbreviated command tables no longer reject undocumented commands
  in history as if the table were exhaustive.
- Keep all discovered Python module recommendations instead of silently stopping
  at 500. An explicit `completion.max_candidates` limit still applies globally.
- Allow background help probes up to 1200 ms for cold CLIs; completion queries
  continue without waiting for these subprocesses.

The shared fixtures cover 32 help samples and 785 expected recommendations from
AIPass, Cargo, rustup, uv, GitHub CLI, Docker CLI, Go, npm, pnpm, Bun, Deno,
Codex CLI, Claude Code, RTK, and Swift. This validates the documented samples;
undocumented commands and unrecognized help layouts can still require specs.
Real-terminal certification remains subject to the compatibility matrix.

本次 beta 改善通用 CLI 命令发现，覆盖空描述、单空格列、长名称、命令分组及彩色帮助，
并补齐 Cargo 已安装扩展命令，移除 Python 模块推荐的 500 项隐藏上限。
共享回归样本覆盖 15 个应用、32 份帮助输出和 785 项预期推荐；不扩大真实终端认证声明。

Update an existing beta installation with:

```sh
hokan upgrade --channel beta --yes
```

Restart Hokan after upgrading to use the new binary.
