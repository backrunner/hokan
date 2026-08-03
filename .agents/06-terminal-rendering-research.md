# Hokan 终端渲染专项研究

> 状态：Architecture baseline v0.1  
> 调研日期：2026-08-01  
> IRIS 基线：`efc49bacfe7249880e3d2d2410abc43df51f5c18`（v0.4.21）  
> 目标：在真实 shell、SSH、tmux 和常见终端中提供低延迟、无残影、尽可能原子呈现的内联 TUI。

## 1. 结论先行

Hokan 的渲染不能只是“拼一段 ANSI 字符串并尽快写出”。它必须是一套带屏幕版本、锚点置信度和提交协议的 compositor。最终选型如下：

1. 使用 Ratatui 的 `Buffer`、layout、style 和 widgets 构造离屏 surface，复用其 Unicode cell 模型与 `Buffer::diff_iter`。
2. 不让 Ratatui 的 `Terminal<Viewport::Inline>` 长期拥有真实 stdout。child shell 会在 Ratatui render pass 之外写屏幕，使它的双 buffer 假设失效。
3. 自建 `OverlayCompositor`、`FrameScheduler` 和唯一的 `OutputActor`。只有 `OutputActor` 可以写真实 stdout。
4. 用 Crossterm 编码 raw mode、光标、颜色和 synchronized output 控制序列；每一帧先完整编码到 `Vec<u8>`，再一次 `write_all` 和一次 `flush`。
5. 支持并主动探测 synchronized output（DEC private mode 2026）。支持时帧为 `BSU -> diff -> 恢复 shell 状态 -> ESU`，终端只展示完成态。
6. 不支持 mode 2026 时仍使用 cell diff、非破坏性覆盖和单次 staged write；正常导航不得先清空 surface 再重画。
7. 使用 `vt100` 作为 P0 首选 child-output 观察模型，跟踪光标、SGR、滚动和 alternate screen；用 `avt` 做差分测试和备选。裸 `vte` 只提供 parser state machine，不足以单独维护屏幕语义。
8. 不使用 `ESC 7`/`ESC 8` 保存恢复光标，因为它们通常只有一个槽位，会和 child 的保存状态互相覆盖。Hokan 使用经过校验的绝对坐标恢复 shell 光标。
9. 任意可能改变 terminal model 的 child output 推进 `screen_revision`；resize、滚动、prompt boundary、未知 VT 序列和屏幕切换推进 `screen_epoch`。旧 revision/epoch 的 frame 永远不得提交。
10. 无法证明锚点或安全注入边界时隐藏 UI。宁可少显示一次候选，也不能覆盖 prompt、污染 scrollback 或切断 child 的控制序列。
11. shell control FD 事件不是“已经画完”的证明。buffer snapshot 只启动 query；frame 必须等待 PTY 内的 prompt marker 或 terminal-model redisplay convergence，不能用固定 sleep 猜测回显完成。

“所有终端都无条件原子呈现”在协议层不可承诺：有些终端不支持 mode 2026，PTY write 也不等于一次屏幕 present。Hokan 的可验证承诺应是：

- 支持 mode 2026 的路径不展示半帧；
- fallback 路径不设计空白中间帧，不进行全屏清除，并通过逐 byte/chunk 审计和真实终端矩阵限制可见 tearing；
- 任一支持矩阵组合出现可复现闪白、残影、光标跳动或 prompt 损坏，都视为发布阻断问题。

## 2. 调研范围与方法

本次不是只阅读 IRIS README，而是检查了以下实现和历史：

- `integration/overlay.go`：菜单、ghost text、清除、宽度和光标序列；
- `root/wrapper.go`：PTY bridge、stdout 串行化、render timer、shell 回显与状态同步；
- `root/term_sync.go`：额外 writer；
- `integration/overlay_test.go`、`integration/cursor_test.go`：现有渲染测试边界；
- IRIS PR #12、#14、#15、#20：提示符重复、导航冻结、debounce、data race、ghost 残留和 Unicode 修复历史；
- Ratatui 0.30.2、Crossterm 0.29.0、`vt100` 0.16.2、`avt` 0.18.0 和 `vte` 0.15.0 源码；
- synchronized output 规范、tmux 3.7 相关实现和常见终端支持现状。

本文件引用的版本是调研快照，不代表未来可以无审计升级。bootstrap 时必须把版本、MSRV 和 transcript corpus 一起锁定。

## 3. IRIS 实现审查

### 3.1 当前渲染路径

IRIS 的主路径可以简化为：

```text
shell/PTY output --------------------+
                                      +--> stdoutMu --> os.Stdout.Write
20 ms render timer -> clear/ghost/UI -+
```

它做对了几件重要的事：

- overlay 的每次绘制先在 `strings.Builder` 中拼好，再调用一次 `writeStdout`；
- `stdoutMu` 避免 PTY bridge 和 overlay 在 Go 层并发写 stdout；
- 绘制期间关闭 autowrap，并在最后恢复；
- 首次绘制通过追加空行给菜单留出可见空间；
- 20 ms timer 合并部分高频更新，避免每个状态变化都立即重画。

这些思路应保留为“单 writer、离屏构造、批量提交”，但它们还不足以形成无闪烁协议。

### 3.2 会造成闪烁或失步的实现

| 实现 | 直接后果 | Hokan 的处理 |
| --- | --- | --- |
| 每帧 `Clear()` 固定清除最多 8 行，再完整重画 | 终端可在 clear 和 paint 之间 present 空白帧 | 固定 surface + cell diff；只有消失的尾部 cell 被擦除 |
| 没有 `CSI ?2026h/l` | 单次 `Write` 也可能被终端分多次渲染 | 探测并使用 synchronized output；fallback 不先清空 |
| 多处使用 `ESC 7/8` | child 与 overlay 共享单槽 save cursor，嵌套时互相覆盖 | 绝对 `CUP`，不触碰 child save slot |
| 每帧追加 `\n` 预留空间 | 重复滚动、scrollback 噪声和锚点漂移 | surface 打开/高度变化时只预留一次 |
| prompt 宽度、rune 数和 cell width 混用 | CJK、emoji、组合字符下 target column 错误 | 所有布局统一使用 grapheme + terminal cell |
| 先把 `boxWidth` clamp 到终端宽度，再强制最小 40 | 小于 40 列时又产生越界宽度 | 响应式降级，宽度始终不超过 `cols - 1` |
| ghost text 通过保存光标、写空格、恢复光标擦除 | 与 shell redisplay 竞态，可能删 prompt 或留残影 | P0 默认不启用 ghost；后续纳入同一 surface diff |
| 从 PTY 最后一段输出推算 prompt column | 只能覆盖少数 CSI，无法可靠处理 wrap、scroll、右 prompt | 完整观察模型 + CPR 校正 + anchor confidence |
| 20 ms 是稳定性的组成部分 | 延迟掩盖跨 channel 顺序问题，不能证明正确性 | revision/epoch gate 和 output actor 排序；timer 只做帧合并 |

IRIS 在 `draw()` 中先关闭 autowrap、保存光标、输出若干空行，再逐行 `2K` 清除并绘制；`Clear()`、`HideMenu()` 和 `ClearAndDisable()` 都会循环清固定行。普通选中项变化也因此产生大量 erase/repaint 控制序列。

### 3.3 历史问题说明了什么

- PR #12 引入过 25 ms PTY throttle，并同时修复 prompt duplication 和导航冻结。
- PR #14 专门修复菜单导航时继续 debounce/移动，提交历史包含 render frequency 和 menu position 调整。
- PR #15 修复 data race、内存问题并优化 `ComputeCursorCol`。
- PR #20 再次修复 ghost text、prompt 删除、UTF-8/visual width 和 overlay race。

这不是对 IRIS 的否定，而是一个清晰信号：如果 shell 回显、控制 FD、PTY output 和 overlay frame 没有统一的版本与写入顺序，继续调 20 ms 或 25 ms 只能改变复现概率。Hokan 应借鉴 IRIS 的产品交互，不复制这套时序模型。

### 3.4 测试覆盖缺口

IRIS 当前测试主要验证简单 prompt column 和 ghost suffix，没有覆盖：

- 任意 byte 边界切分 UTF-8、CSI、OSC、DCS；
- synchronized output 的成对和失败恢复；
- resize、底部滚动、wrap-pending 和 alternate-screen 往返；
- tmux 旧版本与 3.7+；
- child 在 overlay 可见时异步输出；
- 每个可见中间态是否出现空白帧；
- 两个独立 VT emulator 对最终屏幕和光标是否一致。

Hokan 的 P0 必须先补齐这些测试，再做复杂候选样式。

### 3.5 控制 FD 与 PTY 没有天然全序

IRIS 的 zsh hook 在 `line-pre-redraw` 中把 `$LBUFFER` 写入控制 pipe；该 hook 按 zsh 语义发生在命令行重绘之前。`wrapper.go` 的 IPC goroutine收到新 query 后启动 20 ms timer，PTY goroutine则独立读取 shell redisplay bytes。`stdoutMu` 只保证两个 goroutine最终写 `os.Stdout` 时不并发，不能证明“对应这次 buffer 的 redisplay 已经先写完”。

因此仍可能出现以下顺序：

```text
control FD: BufferSnapshot(N) --> schedule frame N --------> commit frame N
child PTY:                                      redisplay N -------->
```

此时即使 frame 本身原子，随后到达的 shell redisplay 仍可能移动光标、覆盖 prompt 或使 overlay anchor 失效。把 timer 从 20 ms 调到 25 ms 只能改变概率。

Hokan 必须把“语义状态”和“屏幕状态”分开：

- `BufferSnapshot(N)` 允许启动 provider，但把 render readiness 置为 `AwaitingRedisplay`；
- shell adapter 能证明时，在 PTY 流中发一个带 session token/boundary id 的零宽 render marker；marker 由 Hokan 在到达 terminal 前消费，因此它与之前的 child bytes 有全序；
- prompt marker 必须由 zsh `%{...%}`、Bash `\[...\]` 等 shell-native 非打印包装生成，并在 P0 验证不会改变 prompt width；不具备可靠 post-redraw hook 的 adapter 不得假装支持；
- zsh v1 在 `line-pre-redraw` 发出的 redisplay marker 只是 redraw-start marker，不是 buffer 已经可见的证明。收到 marker 后必须至少观察到后续 screen byte，并在同一 nonblocking read cycle 达到 `DrainedToEagain`，同时通过 cursor/layout convergence，才能解锁对应 frame；只有未来经验证的真正 post-redraw marker 才可直接作为完成边界；
- 没有 post-redraw marker 时，`TerminalModel` 至少要观察到安全边界、期望 cursor/layout 收敛和一次 PTY readiness cycle 的 `DrainedToEagain`，才可生成 `RedisplayConverged`；control event 后到时允许匹配已经收敛的 current model/read cycle；
- 收敛失败或超时只会让列表保留旧完成态但失去激活能力，或在旧 surface 受影响时隐藏；绝不因 timeout 直接提交未证明的新 frame。

prompt 末尾的 in-band marker 仍很有价值：它把控制 FD 的 prompt/CWD 事件与 PTY 中实际 prompt bytes 关联起来，之后才允许发 CPR。marker 协议需版本化、有长度上限、支持任意分片，并只在 parser ground state 与 shell foreground/prompt 状态消费；其他相似 OSC/DCS 必须原样透传。session token 只减少意外碰撞，不构成对 child process 的安全边界，phase/id/revision gate 仍不可省略。

## 4. 终端呈现的现实边界

### 4.1 一次 write 不等于一次画面

应用把完整字符串交给一次 `write(2)`，只能保证字节顺序，不能要求 terminal emulator 只 present 一次。以下各层都可能切分：

- Rust `write_all` 在短写后重试；
- PTY、SSH 和 tmux 分块传输；
- terminal parser 与 renderer 位于不同线程；
- emulator 可能在处理 erase 后、处理新文字前刷新窗口。

因此“先 clear，再 paint，最后 flush”即使在单 writer 下也可能闪白。正确顺序是尽量直接从旧完成态变到新完成态，并在协议支持时明确包围一次 synchronized frame。

### 4.2 Synchronized output

规范定义：

```text
CSI ? 2026 $ p     查询支持状态（DECRQM）
CSI ? 2026 h       Begin Synchronized Update（BSU）
CSI ? 2026 l       End Synchronized Update（ESU）
```

DECRPM 返回值处理：

| 值 | 含义 | Hokan 决策 |
| ---: | --- | --- |
| `0` | mode 不识别 | 不支持，走 fallback |
| `1` | mode 当前 set | 协议可用但当前 busy；不擅自发送 ESU |
| `2` | mode 当前 reset | 可安全使用 |
| `3` | permanently set | 语义未定义，保守禁用 |
| `4` | permanently reset | 不支持 |
| timeout | 未实现 DECRQM 或 reply 丢失 | 不支持，不能按终端名字猜测 |

探测必须由异步 `TerminalReplyRouter` 完成，不能调用会和 input actor 争抢 stdin 的同步 cursor API。最多只允许一个有歧义的 query outstanding；reply 有长度上限和 deadline，并在匹配后消费，不能转发给 child shell。

mode 2026 支持表只能用于测试计划，不能代替运行时探测。调研时 Kitty、iTerm2、WezTerm、Ghostty、Alacritty、foot 等已有实现，而 VTE/GNOME Terminal 等组合仍可能不支持。具体版本必须进入兼容矩阵。

### 4.3 tmux

tmux 有两个不同层面的 synchronized output：

1. tmux 自己向外层 terminal 批量刷新；
2. tmux 识别 pane 内应用发出的 DECSET 2026，并延迟 pane redraw。

后者在 tmux `CHANGES FROM 3.6b TO 3.7` 中加入。因此：

- tmux 3.7+ 才能作为“应用 mode 2026 支持”基线；
- 旧 tmux 必须通过 DECRQM 得到 unsupported/timeout 后走 fallback；
- 即使 tmux 3.7+ 识别应用 frame，外层 terminal 是否原子 present 仍取决于 tmux 和外层能力；
- `TERM=tmux-*` 不是启用 mode 2026 的充分条件。

### 4.4 Fallback 的明确目标

没有 mode 2026 时，Hokan 仍遵守：

- 不发送 `ED 2`/`ED 3`，不清全屏和 scrollback；
- 普通导航直接覆盖改变的 cells，不先 erase 整个列表；
- 行缩短时只覆盖 stale suffix，专用空白行才允许 `EL`；
- 不写物理终端最后一列，避免 deferred autowrap；
- 一个 frame 先完整编码，再一次 `write_all`；
- frame queue 最多保留最新一帧，慢终端不会积压旧动画；
- fallback 兼容测试观察随机 chunk 后的每个中间态，不能只检查最终截图。

## 5. Rust 技术选型

### 5.1 方案比较

| 方案 | 优点 | 问题 | 决策 |
| --- | --- | --- | --- |
| 手拼 ANSI 字符串 | 依赖少、初始快 | diff、宽字符、样式恢复和 stale cell 都要自研 | 不采用 |
| Ratatui `Terminal<Inline>` | 成熟 layout/widget/double buffer | child 在 render pass 外写屏幕后 previous buffer 失真 | 不作为终端 owner |
| Ratatui 离屏 `Buffer` + 自建 compositor | 复用 cell/layout/widget，又能控制 writer、anchor 和 epoch | 需要少量集成代码 | 采用 |
| Termwiz `Surface/BufferedTerminal` | screen diff 和终端抽象完整 | 依赖面较大，仍无法感知外部 child 写入，偏全屏 owner | 不作为 v1 主路径 |
| 裸 `vte` | byte parser 小且成熟 | 官方说明它不解释序列语义，不是 terminal emulator | 不能单独使用 |
| `vt100` | 直接处理 bytes，提供 cursor、attrs、alt screen 和 unhandled callbacks | 不是所有现代扩展都覆盖，Unicode width 仍需校正 | P0 首选观察模型 |
| `avt` | 现代、聚焦、primary/alternate buffer 和 reflow 完整 | 主 API 接收 Unicode chars，state 恢复接口不如 `vt100` 直接 | 测试 oracle/备选 |

### 5.2 建议依赖快照

| 层 | 调研版本 | 使用范围 |
| --- | --- | --- |
| Ratatui | `0.30.2` | `Buffer`、layout、style、widgets、`diff_iter` |
| Crossterm | `0.29.0` | raw mode、控制序列编码、BSU/ESU |
| `vt100` | `0.16.2` | child output screen/cursor/SGR 观察模型 |
| `avt` | `0.18.0` | dev dependency、差分语义测试、备选模型 |
| `unicode-segmentation` | `1.13.3` | grapheme boundary |
| `unicode-width` | `0.2.2` | 明确的 cell width policy 与辅助校验 |

Ratatui 0.30.2 的 MSRV 是 Rust 1.88。若项目 MSRV 低于 1.88，就必须在 P0 明确选择更早版本并重新验证 wide-cell diff，不能静默降级。

### 5.3 Ratatui 的使用边界

采用：

- `Buffer::empty(Rect)` 作为 current/previous surface；
- `Widget::render` 或 `StatefulWidget::render` 生成结构化 cells；
- `Buffer::diff_iter` 处理宽字符、VS16 emoji 和宽字符缩窄后的 trailing cells；
- `CrosstermBackend<Vec<u8>>::draw(diff)` 或等价自有 encoder 把 diff 写入内存。

不采用：

- `ratatui::init()`；
- alternate screen；
- 长期存在的 `Terminal<Inline>`；
- 让 Ratatui 自己读取 cursor、append lines 或 flush stdout；
- 在 widget 文本里夹带 ANSI。

Ratatui 源码明确警告：在正常 render pass 外直接写 backend 或移动 cursor 后，下一次 draw 可能基于 stale assumptions。Hokan 的 child shell 正是这种外部 writer，所以必须由自建 compositor 决定何时复用 previous buffer。

### 5.4 `vt100` 的使用边界

`TerminalModel` 只观察 child output，不用它重新渲染整个 shell 屏幕：

- 每个 child byte 按原样继续转发；
- 同一 byte stream 喂给 `vt100::Parser`；
- 读取 `cursor_position()`、`attributes_formatted()`、`alternate_screen()` 和 cursor visibility；
- unhandled cursor/mode/erase sequence 降低 anchor confidence；
- prompt boundary 上用 CPR 修正 absolute cursor，避免 width policy 长期漂移。

因为 `vt100` parser 不公开“当前是否处于半个 CSI/OSC/DCS 中”，另外实现窄职责的 `SafeBoundaryScanner`。它只回答当前是否可以插入 Hokan bytes，不能解释屏幕语义。该 scanner 必须对任意 chunking、UTF-8、C0/C1、CSI、OSC、DCS、SOS/PM/APC 做 fuzz，并与 `vte`/`avt` parser corpus 差分验证。

## 6. 渲染架构

### 6.1 数据流

```text
real stdin --> TerminalReplyRouter --keys--> InputDecoder --+
                    +-- reply event ------------------------+--> Reducer --> FrameScheduler --+
Provider batches -------------------------------------------+                                |
child PTY ---------------------------------------------------------------------------------+--> OutputActor --> stdout
                                                                                                  | owns
                                                                                                  +-- SafeBoundaryScanner
                                                                                                  +-- TerminalModel
                                                                                                  +-- OverlayCompositor
                                                                                                  +-- Ratatui Buffers
```

`OutputActor` 独占 stdout，并按以下优先级处理：

1. terminal restore、suspend、terminate；
2. child output，不允许丢弃；
3. overlay hide/invalidate；
4. 最新 overlay frame，旧请求可覆盖。

任何其他模块拿到的都只能是 channel handle，不能拿 `Stdout`、TTY fd 的可写 clone 或 `CrosstermBackend<Stdout>`。

### 6.2 核心状态

```rust
struct ScreenRevision(u64);
struct ScreenEpoch(u64);
struct FrameRevision(u64);

struct Anchor {
    shell_cursor: CellPos,
    overlay_origin: CellPos,
    terminal_size: Size,
    screen_revision: ScreenRevision,
    screen_epoch: ScreenEpoch,
    confidence: AnchorConfidence,
}

enum AnchorConfidence {
    Exact,       // CPR + prompt/buffer revision 已确认
    Derived,     // 从已确认位置和完整已知 VT delta 推导
    Unknown,     // 禁止显示 overlay
}

struct SurfaceKey {
    screen_epoch: ScreenEpoch,
    rect: Rect,
    theme_revision: u64,
    width_policy: WidthPolicy,
}

enum RenderReadiness {
    AwaitingPromptMarker { boundary_id: BoundaryId },
    AwaitingRedisplay {
        buffer_revision: BufferRevision,
        boundary_id: BoundaryId,
    },
    Ready {
        buffer_revision: BufferRevision,
        screen_revision: ScreenRevision,
    },
    Unknown,
}
```

只有 `SurfaceKey` 完全相同时才允许 previous/current diff。anchor 移动、height 改变、resize、scroll、terminal model 重置或 theme/width policy 改变都触发 full repaint。

### 6.3 Shell redisplay gate

control protocol 的 buffer/prompt event 与 PTY bytes 必须通过 boundary id 和 revision 汇合，不能谁先到就直接 render：

1. 收到新 `BufferSnapshot`，reducer 递增 `buffer_revision` 并启动 provider，但 frame request 暂不可提交。
2. 若 adapter 有经过验证的真正 post-redraw marker，`RenderBoundaryDecoder` 在 child stream 中消费完整 marker，并以其位置对应的 `screen_revision` 解锁。zsh v1 的 `line-pre-redraw` marker 不属于此类，它只标记 redraw 开始。
3. 对 redraw-start marker 或无 post-redraw marker 的 adapter，`TerminalModel` 依据精确 prompt anchor、snapshot text/cursor、marker 后至少一个 screen byte、统一 width policy 和 PTY delta 判断 visual cursor/layout 是否收敛；还必须在 nonblocking PTY read cycle 末尾观察到 `DrainedToEagain`，不能把单个 `read()` 返回当边界。
4. frame 绑定收敛时的 `buffer_revision + screen_revision + screen_epoch`。任一后续 child byte 使 revision 变化，commit gate 重新校验或拒绝。
5. 等待期间旧 surface 可以保持完整显示，但所有候选 activation 都按新 buffer revision 拒绝；若 child output 触碰旧 rect、发生 scroll 或进入未知状态则立即隐藏。
6. gate deadline 只决定何时放弃/隐藏，不决定何时强行绘制。不得把 PTY quiet timer 当作 post-redraw proof。

`OutputActor` 保存少量有界的 recent marker/read-cycle metadata。`ArmRenderGate` 先到时等待后续 child batch；PTY/marker 先到时可在 gate 到达后匹配当前模型与 id。缓存只包含 revision/id/几何状态，不保存或记录完整 prompt/buffer。若两种顺序都不能证明关联，状态为 `Unknown`。

P0 必须分别确认 zsh、bash、fish 能提供哪一级能力：`PostRedisplayMarker | ModelConvergence | Unsupported`。无法稳定收敛的 shell/mode 只能显式唤起或不显示自动列表，不能借“best effort”绕过 commit gate。

### 6.4 锚点建立

1. shell 发出 prompt boundary，OutputActor 等当前已读 PTY bytes 到达安全边界。
2. `TerminalReplyRouter` 先注册唯一 outstanding query，再由 `OutputActor` 发送 CPR：`CSI 6 n`。
3. 收到 `CSI <row>;<col> R` 后校正 `TerminalModel` 的 absolute cursor，建立 `Exact` anchor。
4. 之后完整且已知的 child VT delta 可以把状态降为/保持 `Derived`。
5. resize、未知 cursor-affecting sequence、非法 UTF-8、scroll mapping 不确定或 reply timeout 会变为 `Unknown`。
6. `Unknown` 状态隐藏 overlay，等待下一个安全 prompt boundary 重试；不在前台应用运行期间探测。

CPR 只用于 prompt 建立和失步恢复，不用于每个按键。这样 SSH 下不会把键入延迟绑定到网络往返。

当 shell cursor 不在 buffer 末尾时，只有在 suffix 为单行、无 tab/control、width policy 已确认时才从 suffix 推导输入底部；否则自动列表暂时隐藏。显式 History 视图可以在实现可靠的 multiline layout 后再放宽。wrap-pending cursor 同样视为不可安全恢复。

### 6.5 Surface 与空间预留

- overlay 从 column 0 开始，占用专用的完整逻辑行，视觉内容宽度最多为 `terminal_cols - 1`；
- 首次打开 overlay session 时确定 surface height，包含候选、状态和分页行；连续 query 与 provider 增量期间保持固定；
- 如果 cursor 下方空间不足，只在首次打开/resize 时追加缺少的空行，让 terminal 滚动一次；
- compositor 同步修正 shell cursor 与 `screen_epoch`，不在后续每帧重复输出空行；
- 候选减少时用空 cells 填满固定 surface，避免 layout 上下跳；
- 关闭时清除专用 rect，但保留已产生的空白行。下一条 command output 会自然覆盖它们；
- 永不写 terminal 最后一列，避免自动换行 pending state。

## 7. 帧提交协议

### 7.1 普通 overlay frame

完整 frame 在内存中准备好后，由 OutputActor 再次检查 `screen_revision`、`screen_epoch`、`buffer_revision` 和 `frame_revision`：

```text
if synchronized_output == AvailableIdle:
    BSU                         CSI ? 2026 h

hide cursor                    仅当 shell cursor 当前可见
emit Buffer diff               absolute CUP + structured SGR + symbols
reset overlay SGR
restore child SGR              来自 TerminalModel
restore shell absolute cursor  CUP；不使用 ESC 7/8
restore cursor visibility

if synchronized_output == AvailableIdle:
    ESU                         CSI ? 2026 l

write_all(staged_bytes)
flush_once()
```

要求：

- `BSU` 和 `ESU` 必须在同一个 staged frame 中生成；
- 写 BSU 前把 ownership 标记为 `MayBeOpenByHokan`，完整写出 ESU 后清除；`TerminalGuard` 只在 ownership 未清除时补发 ESU；
- frame build error 发生在 BSU 编码前；
- stdout error 后尝试发送 ESU/restore，但不能吞掉原始 I/O error；
- frame 中禁止 `ED 2`、`ED 3`、DECSC、DECRC 和 alternate-screen sequence；
- candidate 文本中的 ESC、C0/C1、换行和 bidi control 必须显示为转义文本，不能原样进入 encoder。

### 7.2 child output 到达时

child output 永远优先。OutputActor 先用相同 bytes 预演 `TerminalModel`，再决定 transaction：

```text
old shell cursor / old overlay rect
        |
        v
parse child delta -> new cursor / mode / scroll impact / safe boundary
        |
        +-- unsafe boundary --> 原样转发 child bytes，保留最新 frame request
        |
        +-- alternate/foreground/unknown --> 清已知专用 rect，转发，隐藏 UI
        |
        +-- prompt still valid --> [清除受影响旧 rect] + child bytes + 最新 overlay frame
                                  整体可由一次 BSU/ESU 包围
```

不能在半个 UTF-8 codepoint、CSI、OSC 或 DCS 中插入 Hokan sequence。若某个超长 control string 长时间不结束，达到 byte/time 上限后立即转为透明 passthrough 并把 anchor 标为 `Unknown`，不能无限缓存 child output。

`TerminalModel` 同时跟踪 child 发出的 DECSET/DECRST 2026。child-owned 或启动时已 set 的 synchronized update 期间，Hokan 不嵌套 BSU、不发送 ESU，overlay request 延后或隐藏；只有 Hokan 自己标记为 `MayBeOpenByHokan` 的 transaction 才能由恢复路径结束。

### 7.3 打开、导航、关闭

- 打开：必要时一次性预留空间，使用 blank previous buffer 做 full paint；
- 导航：surface rect 不变，只 diff 旧选中行和新选中行；
- provider 增量：保留 candidate id/位置，diff 实际变化 cells；
- 关闭：把 current surface diff 到 blank surface；若整行属于专用 rect，可用 `EL` 优化；
- 再打开：anchor/epoch 未变才可复用 blank buffer，否则 full paint；
- ghost text：P0 默认关闭。后续只能作为同一 frame 中的独立 single-line surface，不能另开 writer 擦空格。

### 7.4 Resize 与 alternate screen

Resize：

1. 丢弃尚未提交的 frame；
2. 清除能可靠定位的旧 surface；定位不可靠时不盲目 erase；
3. 更新 PTY size 和 TerminalModel size；
4. 推进 `screen_epoch`，previous buffer 作废；
5. 等新的 prompt/CPR anchor，再 full paint；
6. selection 保留 candidate id，不保留旧 row index 假设。

Alternate screen/foreground TUI：

- 检测到 DECSET 47/1047/1049 或 foreground pgid 变化时，先关闭 overlay，然后纯透传；
- alternate screen 内不查询 CPR、不发送 BSU、不渲染候选；
- 返回主屏、foreground pgid 回到 shell 且收到新 prompt boundary 后才重新建立 anchor。

## 8. Frame scheduler 与版本协议

固定 20 ms debounce 不再承担正确性。scheduler 只负责吞吐和延迟：

- reducer 每次可见状态变化产生新的 `FrameRevision`；
- pending queue 只保存最新 `ViewModel`，不积压中间选中态；
- OutputActor commit 前校验 `FrameRevision`、`buffer_revision`、`ScreenRevision` 和 `ScreenEpoch`；
- child output 会使旧 frame request 自动过期，并在安全边界请求最新 repaint；
- idle 时首帧立即或在最近 frame interval 到期后提交；
- 默认最大 60 FPS，允许按本地/SSH profile 调整，但按键不等待 provider settle；
- provider batch 可以合并到下一 frame，用户导航必须进入最近一帧；
- render/encode 在单个小 surface 上完成，不放进无界 blocking pool。

建议默认预算：

| 指标 | P0 目标 |
| --- | ---: |
| input event 到首个本地可见 frame p95 | `<= 16.7 ms` |
| navigation event 到 frame p99 | `<= 33 ms` |
| compose p95（12 行、100 cols） | `<= 2 ms` |
| diff + encode p95 | `<= 1 ms` |
| pending overlay frames | `<= 1` |
| idle redraw | `0 frame/s` |

这些是本地、无 provider I/O 阻塞的目标。SSH 的 terminal transport 延迟单独报告，不能混入本地 provider latency。

## 9. Unicode、cell 和内容安全

### 9.1 单一宽度模型

- 内部文本编辑 offset 使用 UTF-8 byte；
- 截断和 cursor movement 只能落在 grapheme boundary；
- UI geometry 全部使用 terminal cells；
- Ratatui `Cell` 是渲染宽度的最终真相，业务层不能再按 rune count 做第二套布局；
- ambiguous-width policy 为 `auto|1|2`，进入 `SurfaceKey`；改变后强制 full repaint；
- prompt boundary 上的 CPR 用于发现 terminal 与本地 width policy 的累积偏差。

### 9.2 必测字符

- CJK 宽字符和全角标点；
- combining mark；
- VS16 emoji、ZWJ sequence 和 skin tone modifier；
- emoji/CJK 被 ASCII 替换时的 trailing cell 清理；
- bidi controls、zero-width controls 和不可打印 bytes；
- 最后一列、恰好填满一行和 wrap-pending；
- locale/terminal 对 ambiguous width 的不同选择。

### 9.3 显示内容不等于执行内容

candidate command、history 和文件名都可能含控制字符。进入 ViewModel 前必须：

- 把 ESC、C0/C1、换行、tab 和 bidi control 转成稳定可见转义；
- 限制单项 grapheme/cell/byte 长度；
- 只通过结构化 `Style` 着色；
- 保留原始 `TextEdit` 供 shell-native 回填，surface/compositor 永远看不到可执行 raw bytes。

## 10. 故障与恢复

| 故障 | 行为 |
| --- | --- |
| mode 2026 probe timeout | 标记 unsupported，走 fallback，不阻塞 shell |
| CPR timeout/畸形 | anchor `Unknown`，隐藏 UI，下一 prompt 重试 |
| unknown cursor-affecting VT | 推进 epoch，隐藏 UI；原始 child bytes 仍透传 |
| partial UTF-8/control string 超限 | 透明透传，停止注入，等待恢复边界 |
| stdout 短写/error | `write_all` 重试；最终失败走 TerminalGuard，尝试 ESU |
| frame encode error | 丢弃 frame，不影响 child output |
| resize storm | 合并尺寸，旧 frame 全部作废，只按最终 size 重建 |
| child 进入 alternate screen | 清 overlay 后纯透传 |
| suspend/crash/normal exit | 结束可能未闭合的 Hokan-owned BSU，恢复 SGR/cursor/termios，再退出或挂起 |

日志只记录 revision、epoch、rect、byte count、耗时和 error code；不记录完整 shell buffer、candidate command 或 terminal reply raw payload。

## 11. 无闪烁验证体系

### 11.1 纯逻辑与 golden

- old/new `Buffer` 的 cell diff golden；
- selected row 上下移动只改变预期行；
- surface 收缩清掉 stale cells；
- wide-to-narrow、VS16 和 styled trailing cells；
- emitted frame token snapshot，断言 BSU/ESU 配对和恢复顺序；
- 断言没有 `ED 2/3`、DECSC/DECRC、alternate screen 和最后一列 write；
- anchor epoch/revision 过期 frame 必须被拒绝。

### 11.2 随机 chunk 与虚拟终端

同一 transcript 以所有边界和随机边界切分：

1. child bytes 喂给 Hokan output pipeline；
2. Hokan 最终 bytes 分别喂给 `vt100` 与 `avt` adapter；
3. 比较 prompt cells、overlay cells、cursor、alternate-screen 和 scroll 结果；
4. mode 2026 模型只在 ESU 时采样 presented screen；
5. fallback 模型在每个 byte/随机 chunk 后采样，断言正常导航没有全空 surface 或 prompt 覆盖；
6. 穷举 `BufferSnapshot`、prompt marker、PTY redisplay、provider batch 和 frame request 的合法交错，断言 readiness 前绝不提交新 frame；
7. 断言合法 marker 被消费，近似 marker、错误 phase/token/id 和嵌套在其他 control string 中的同字节片段原样透传；
8. 对 marker decoder、reply router、safe boundary 和 compositor 运行 fuzz。

### 11.3 PTY 集成

至少覆盖：

- zsh/bash/fish 默认 emacs mode；
- 快速输入、长按导航、1 MiB bracketed paste；
- CJK/emoji/组合字符、窄终端和最后一列；
- async job notification、prompt redraw、`Ctrl-L`；
- buffer event 先于/后于/夹在 PTY redisplay chunk 中，post-redraw marker 缺失、重复、伪造、截断和迟到；
- resize storm、pane resize、scroll bottom；
- `vim`、`less`、`top` 往返；
- `Ctrl-C`、`Ctrl-Z`/`fg`、SIGHUP、panic 和 child exit；
- tmux 3.6（fallback）与 3.7+（mode 2026）；
- 本地 PTY 与带延迟/分片的 SSH transport harness。

### 11.4 真实终端发布矩阵

按精确版本记录 probe result 与行为，而不是只写终端品牌：

- macOS Terminal.app、iTerm2、Kitty、WezTerm、Ghostty；
- Linux Alacritty、Kitty、WezTerm、foot、GNOME Terminal/VTE、Konsole；
- 直接运行、tmux 3.6、tmux 3.7+、SSH 内运行；
- light/dark、`NO_COLOR`、ASCII-only 和 ambiguous width 配置。

发布候选应录制至少 120 FPS 的固定交互脚本，对每帧做 overlay rect pixel hash/blank-frame 检测，并人工检查 cursor jump、边框残影和 prompt 抖动。自动语义测试不能完全替代真实 emulator 的 present 行为。

## 12. P0 实施顺序

1. 建立 `OutputActor` 和“其他模块无法写 stdout”的编译边界。
2. 实现 RenderBoundaryDecoder、SafeBoundaryScanner、TerminalModel、TerminalReplyRouter 与 transcript fuzz。
3. 为三种 shell 原型化 prompt/post-redraw marker；逐项确定 marker、model convergence 或 unsupported 能力。
4. 实现 `RenderReadiness`、cross-FD 交错测试和 echo-convergence gate，不接 provider。
5. 做 CPR/DECRQM probe，验证直接终端、tmux 3.6/3.7+ 和 SSH timeout。
6. 用 Ratatui Buffer 实现 3 行固定 surface，不接 provider。
7. 实现 full paint、blank paint、cell diff 和 staged writer。
8. 加入 BSU/ESU ownership 与 TerminalGuard 的条件恢复。
9. 实现 anchor epoch、一次性行预留、resize/scroll/alternate invalidation。
10. 接入 reducer revision 和 latest-only scheduler。
11. 跑双 emulator、PTY、随机 chunk、性能和真实终端录制。
12. P0 门槛全部通过后，才加入多来源候选、复杂标签和可选 ghost text。

P0 期间如果 `vt100` 在 transcript corpus 中对关键序列频繁进入 unhandled/错误状态，应在同一接口下替换为 `avt` adapter；不能为了保留选型而降低 anchor gate。

## 13. 明确拒绝的捷径

- 用 `sleep(20ms)` 或 `sleep(25ms)` 等 shell 回显；
- 把 control FD 的 buffer/prompt event 当作 PTY redisplay 已完成；
- 每次按键 `Clear(CurrentLine)` 所有菜单行；
- 把一次 `write` 当作原子 frame；
- 用 `$TERM` 字符串直接断言 synchronized output；
- 在 tmux 内无 probe 地发送 mode 2026；
- 使用 DECSC/DECRC 包住 overlay；
- 依赖字符数计算 cursor column；
- 在 partial UTF-8/CSI/OSC/DCS 后插入控制序列；
- child output 和 overlay 各持有一个 stdout mutex/writer；
- anchor 不确定时继续画“看起来大概正确”的菜单；
- 只检查最终截图，不观察中间呈现态。

## 14. 参考资料

### IRIS

- [IRIS v0.4.21 调研 commit](https://github.com/versenilvis/IRIS/tree/efc49bacfe7249880e3d2d2410abc43df51f5c18)
- [overlay draw/clear 实现](https://github.com/versenilvis/IRIS/blob/efc49bacfe7249880e3d2d2410abc43df51f5c18/integration/overlay.go#L570-L905)
- [stdout mutex 实现](https://github.com/versenilvis/IRIS/blob/efc49bacfe7249880e3d2d2410abc43df51f5c18/root/wrapper.go#L95-L108)
- [PTY stdout bridge](https://github.com/versenilvis/IRIS/blob/efc49bacfe7249880e3d2d2410abc43df51f5c18/root/wrapper.go#L314-L361)
- [20 ms render timer](https://github.com/versenilvis/IRIS/blob/efc49bacfe7249880e3d2d2410abc43df51f5c18/root/wrapper.go#L609-L634)
- [zsh `line-pre-redraw` 控制 pipe hook](https://github.com/versenilvis/IRIS/blob/efc49bacfe7249880e3d2d2410abc43df51f5c18/root/init.go#L40-L64)
- [IRIS PR #12](https://github.com/versenilvis/IRIS/pull/12)、[PR #14](https://github.com/versenilvis/IRIS/pull/14)、[PR #15](https://github.com/versenilvis/IRIS/pull/15)、[PR #20](https://github.com/versenilvis/IRIS/pull/20)

### 协议与 Rust 生态

- [Synchronized Output 规范与 DECRQM 检测](https://github.com/contour-terminal/vt-extensions/blob/master/synchronized-output.md)
- [zsh `zle-line-pre-redraw` special widget](https://zsh.sourceforge.io/Doc/Release/Zsh-Line-Editor.html#Special-Widgets)
- [Crossterm `BeginSynchronizedUpdate`/`EndSynchronizedUpdate`](https://github.com/crossterm-rs/crossterm/blob/038159857ae7960da888a237e11c5f70c0e50e24/src/terminal.rs#L399-L493)
- [tmux 3.7 synchronized output 变更](https://github.com/tmux/tmux/blob/31dccb6bc9521b0ea46307974d071ad7f09f0e9b/CHANGES#L187-L517)
- [Ratatui `BufferDiff` 宽字符处理](https://github.com/ratatui/ratatui/blob/3d8639cbb2f910200f30e680a8923ccaf99ba1bf/ratatui-core/src/buffer/diff.rs)
- [Ratatui 对外部 backend mutation 的同步警告](https://github.com/ratatui/ratatui/blob/3d8639cbb2f910200f30e680a8923ccaf99ba1bf/ratatui-core/src/terminal.rs#L147-L157)
- [Ratatui inline viewport 的行预留算法](https://github.com/ratatui/ratatui/blob/3d8639cbb2f910200f30e680a8923ccaf99ba1bf/ratatui-core/src/terminal/inline.rs#L364-L423)
- [`vt100` parser/screen model](https://github.com/doy/vt100-rust)
- [`vte` 明确说明 parser 本身不足以实现 terminal emulator](https://github.com/alacritty/vte)
- [`avt` virtual terminal](https://github.com/asciinema/avt)
