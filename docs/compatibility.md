# Hokann 兼容矩阵

更新日期：2026-08-02。

本页区分实现能力、自动化验证和真实环境认证。只有标记为“实测通过”的组合才代表当前
机器完成了端到端验证；“待认证”不能被发布说明改写为已支持。

## 支持边界

v1 面向 macOS 和 Linux 上支持 UTF-8 与常见 ANSI/VT 控制序列的 POSIX TTY。
`TERM=dumb`、Windows console、PowerShell 和非交互 stdin/stdout 不在支持范围。

| 维度 | 组合 | 状态 | 证据或降级 |
| --- | --- | --- | --- |
| OS | macOS 27.0 arm64 | 实测通过 | 本地 debug/release、真实 PTY harness |
| OS | macOS x86_64 | 待 CI/实机 | release workflow 交叉构建，尚无本轮实机记录 |
| OS | Ubuntu/Fedora x86_64 | 待 CI/实机 | CI job 已定义，当前机器未运行 Linux |
| OS | Linux aarch64 | 待 CI/实机 | `cross` release job 已定义 |
| Shell | zsh 5.9 | 实测通过 | ZLE 精确 buffer/cursor 同步与原生回填 |
| Shell | bash 3.2.57 | 实测通过 | emacs 标准键镜像与 Readline 回填 |
| Shell | fish 3.6+ | 待认证 | adapter 和 fixture 已实现，本机未安装 fish |
| Multiplexer | tmux 3.6b | 实测通过 | runtime probe 判定不支持 mode 2026，走 diff fallback |
| Multiplexer | tmux 3.7+ | 待认证 | 必须按 runtime probe 选择路径，不能按版本猜测 |
| Transport | SSH | 待认证 | timeout/分片策略已实现，尚无真实远端记录 |
| Rendering | mode 2026 | 自动化通过 | 强制 capability 的外层 PTY 与双 emulator 测试 |
| Rendering | unsupported fallback | 实测通过 | 外层 PTY 与真实 tmux 3.6b 测试 |
| Input | Unicode/CJK/emoji | 自动化通过 | parser、surface 和真实 PTY 输入测试 |
| Input | 1 MiB bracketed paste | 自动化通过 | 单事件上限及超限 raw streaming 测试 |
| Recovery | panic/TERM/HUP/TSTP/CONT | 自动化通过 | 子进程恢复、termios 与 signal integration 测试 |

## 终端应用

Terminal.app、iTerm2、Ghostty、Kitty、WezTerm 和 Alacritty 均在发布阻断矩阵内，但
本轮尚未逐一完成 120 FPS 录制、overlay rect blank-frame 检测和人工 cursor/prompt
检查。因此当前版本应称为 beta candidate，而不是已经认证的 beta。

## Shell 能力

- zsh：通过 `line-pre-redraw` 获取真实 `BUFFER`/`CURSOR`。v1 的 redisplay marker 是
  redraw-start marker；它之后还必须看到屏幕字节、模型收敛和 `DrainedToEagain` 才能绘制。
- bash：标准 emacs 编辑键使用本地镜像；未知键序列会把同步状态降为 uncertain 并隐藏列表。
- fish：默认键位使用本地镜像；在真实 fish 认证完成前不承诺自定义 key binding 或 vi mode。

所有 shell 在前台程序、alternate screen、未知 VT 状态、失去锚点或 buffer 不确定时都会
隐藏 overlay 并保持字节透传。

## 认证记录模板

每次发布需记录：OS 和架构、shell 精确版本、终端名称/版本、`TERM`、直接或 SSH、tmux
版本、mode 2026 DECRQM 结果、测试脚本版本、录制位置、blank-frame 结果和已知降级。
