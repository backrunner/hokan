# Hokan v0.1 实现与验收状态

更新日期：2026-08-02。

## 结论

原始五类功能已从 provider、排序、列表交互接通到真实 shell buffer；PTY wrapper、唯一
stdout writer、无闪烁 compositor、异常恢复和维护 CLI 也已实现。当前代码状态是 beta
candidate，不是已认证 beta：Fish、SSH、tmux 3.7+、Linux 实机和六个指定终端应用仍需
按发布矩阵完成录制与人工检查。

2026-08-02 的发布前复核又完成了以下加固：output actor 在 I/O/compositor 错误或 panic
后会封闭 mailbox 并释放同步等待者；PTY child 的异常路径会关闭 writer、终止并回收子
shell；zsh adapter 能跟随动态 `precmd` prompt 和用户 `.zshenv` 改写的 `ZDOTDIR`；setup
块固定安装二进制路径，同时在 Hokan 子 shell 中优先使用当前 `HOKAN_BIN`。

同日最终 review 进一步完成：control channel 改为可取消的阻塞 poll，provider panic 被
隔离，候选在排序和激活两处验证 edit/控制字符，history 满 256 MiB 时在写入前 compact，
IPC/edit/config/backup 的特殊文件和容量边界均有回归测试。shell control protocol 升级为
v2，以 `START`/`END` phase 配对保留含 Tab 的命令与 CWD；真实 zsh/bash PTY 已覆盖该路径。
zsh setup 块固定在 `.zshrc` 顶部，外层在用户配置加载前进入 Hokan，用户 rc 只执行一次；
`HOKAN_AUTO_START=0` 可临时旁路。

## 功能闭环

| 需求组 | 状态 | 主要实现 | 自动化证据 |
| --- | --- | --- | --- |
| `FR-TTY-*` | 已实现 | PTY child/pump、shell protocol、signal bridge、termios/panic recovery | `shell_pty`、`terminal_session`、`process_recovery` |
| `FR-HIS-*` | 已实现 | 三种 history parser、CRC event log、snapshot、repair/compact、稳定索引 | 100k 查询和双进程 100k append |
| `FR-SPEC-*` | 已实现 | schema/loader/override、九个内置命令、动态槽位和风险 gate | loader/provider golden tests，`spec validate` 为 9 项 |
| `FR-FS-*` | 已实现 | 有界目录扫描、quote-aware edit、文件/目录/可执行类型 | 5k 目录预算、特殊路径 property test |
| `FR-PROJ-*` | 已实现 | 最近 `package.json` discovery/cache，四种 package manager | nested/boundary/malformed/invalidation fixtures |
| `FR-AI-*` | 已实现 | 本地 detector、显式 action、rustls client、取消/timeout、严格 JSON、安全分类 | 本地 mock 覆盖 HTTP/redirect/body/secret/恶意响应 |
| `FR-UI-*` | 已实现 | Ratatui 离屏 fixed surface、latest-only scheduler、mode 2026/diff compositor | 双 emulator、随机 chunk、真实外层 PTY |
| `FR-CFG-*` | 已实现 | XDG TOML、热加载、0600 credential、有限轮转 debug log、doctor/config/history/spec CLI | 配置冲突、权限、日志脱敏/轮转、FIFO、AI 禁用和 JSON tests |

## 终端正确性证据

- child output、terminal probe、overlay、hide 和 restore 由 `OutputActor` 排序；静态 ownership
  测试禁止其他模块持有可写 stdout/TTY handle。
- mode 2026 路径只在完整 staged frame 外层包裹 BSU/ESU；fallback 不开启 transaction，
  不先清空 surface。
- 普通 overlay transcript 禁止 `ED 2/3`、alternate screen、DECSC/DECRC 和最后一列 write。
- zsh redisplay marker 明确为 redraw-start；marker-only drain 不解锁，必须等后续 screen byte、
  model convergence 与 `DrainedToEagain`。
- 真实 PTY 覆盖 Ctrl-L、resize、CPR、SIGTSTP/SIGCONT、SIGTERM/SIGHUP、Unicode、前台
  alternate-screen fixture、canonical/echo 恢复、动态 prompt、自定义 `ZDOTDIR` 和 tmux
  3.6b fallback。
- bracketed paste 恰好 1 MiB 保持单事件；超限后保持 raw streaming 到结束 marker，内部
  Enter/Ctrl-C/箭头不会被拆成独立输入。

## 安全与数据边界

- AI 普通输入不联网；只有激活 action 才创建请求，取消 token 与 query id 阻止迟到覆盖。
- client 禁止 redirect，限制 header/body timeout 和 128 KiB body；响应命令拒绝多行、NUL、
  C0/C1 和 ANSI。
- 风险分类覆盖递归/强制删除、`find -delete/-exec`、递归 chmod/chown、download-to-shell、
  `dd`/redirect 写设备、mkfs、shred/truncate、signal、shell `-c`、命令替换和进程替换；
  不能证明安全的嵌套执行语法统一为 `Unknown`。
- AI 候选只触发 action 或回填，永不作为命令执行；history/high/unknown 候选可经显式选中执行，
  High/Unknown 必须先通过二次确认（`Enter` 确认、`Esc` 取消）；亲手输入的命令永不触发确认。
  AI key 使用 zeroize、环境变量引用或
  owner-only 普通文件，错误和 panic 不输出 secret payload。
- debug log 默认关闭；启用后只记录类型化 session/provider/AI/config 元数据，私有文件按
  配置上限轮转，并排除 query、history、CWD、HTTP body 和环境变量值。
- history 使用短锁、CRC、torn-tail repair、corruption quarantine 和 atomic compaction；
  `clear --yes` 不修改 shell 原 history。state/history 文件拒绝 symlink 和异主文件；已有的
  当前用户普通文件会自动收紧到 `0600`。
- shell history 启动导入、主配置、`package.json` 和用户 spec 均有实际读取上限；manifest
  与 spec 在读取前后校验文件身份/版本，用户 TOML 解析错误不回显原始 source 行。
- shell control、PTY pump 和 provider worker 均可取消并回收；嵌套 shell 通过 owner PID
  拒绝重复 hook，畸形/超长 control frame 和单 provider panic 只降级当前能力，不终止会话。

## 发布工程

- README、man page、安装/卸载、AI 配置、故障恢复和兼容矩阵已补齐。
- CI 包含 Rust 1.96 MSRV、current stable、Linux/macOS shell matrix、fmt/check/test/release、
  strict Clippy、cargo-audit 和 cargo-deny。
- release workflow 交付 macOS/Linux 的 x86_64/aarch64 归档、SPDX JSON SBOM 和统一
  `SHA256SUMS`；tag 与 Cargo package version 不一致会失败。
- `dist/` 中先前的 `hokan-0.1.0-aarch64-apple-darwin.tar.gz` 已被本轮 protocol/setup
  修复取代，不能作为当前源码的 release artifact；正式发布必须由 release workflow 重建。
- 当前源码通过 `cargo package --locked` 的隔离重编译。本机开发二进制安装在
  `~/.cargo/bin/hokan`，SHA-256 为
  `6116bffdc3e252879b7a062248e89c017fa2b34e9d160ff72e07f79bb6771a98`，并由
  `/opt/homebrew/bin/hokan` 链入。
- 安装后二进制通过完整 `terminal_session` 8/8 和正式 setup 自动启动用例；实际 `.zshrc`
  语法有效，受管块二次 setup 保持幂等，`-c`/非 TTY 旁路与 `HOKAN_AUTO_START=0` 均验证。

## 2026-08-02 自动化结果

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo check --all-targets` | 通过 |
| `cargo test` | 178 passed；2 个 multiprocess 内部 worker invocation 按设计 filtered |
| `cargo clippy --all-targets --all-features -- -D warnings` | 通过，0 issue |
| `cargo test --release` | 178 passed；真实 terminal suite 8/8 |
| `cargo audit --deny warnings` | 通过，扫描 335 个依赖，无 advisory |
| `cargo deny check advisories licenses sources` | 全部通过 |
| `cargo package --locked` | 103 个文件；package 与隔离重编译通过 |

## 尚未完成的外部认证

以下事项不能在当前单台 macOS arm64 主机上伪造为完成，也是 beta 发布前的剩余阻断项：

- Fish 3.6+ 真实 buffer 回填与默认键位；
- tmux 3.7+ 的真实 DECRQM/mode 2026 路径；
- SSH 延迟、分片、detach 和断开恢复；
- macOS x86_64、Ubuntu/Fedora x86_64 与 Linux aarch64 实机；
- Terminal.app、iTerm2、Ghostty、Kitty、WezTerm、Alacritty 的 120 FPS 录制、像素/blank
  frame 检测和 cursor/prompt 人工检查。

精确执行模板和状态表见 `docs/release-checklist.md` 与 `docs/compatibility.md`。这些外部项
通过前可以交付源码和 beta candidate artifact，但不能发布为已经满足完整兼容承诺的 beta。
