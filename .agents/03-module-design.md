# Hokann 模块设计

## 1. 代码组织原则

v1 从一个 Cargo package 开始，同时提供 library 和 binary target。模块边界先在进程内保持清晰，不为了形式拆成多个 crate；只有编译时间、独立复用或 feature 隔离出现实际需求时再拆 workspace。

```text
hokann/
  Cargo.toml
  Cargo.lock
  src/
    main.rs
    lib.rs
    cli/
      mod.rs
      commands.rs
      doctor.rs
      init.rs
    app/
      mod.rs
      event.rs
      state.rs
      reducer.rs
      effect.rs
      supervisor.rs
    terminal/
      mod.rs
      guard.rs
      input.rs
      reply.rs
      render_boundary.rs
      safe_boundary.rs
      model.rs
      surface.rs
      compositor.rs
      scheduler.rs
      output.rs
      capabilities.rs
    pty/
      mod.rs
      child.rs
      process_group.rs
      signals.rs
    shell/
      mod.rs
      protocol.rs
      session.rs
      zsh.rs
      bash.rs
      fish.rs
    parser/
      mod.rs
      lexer.rs
      syntax.rs
      quote.rs
      edit.rs
    completion/
      mod.rs
      context.rs
      candidate.rs
      engine.rs
      registry.rs
      dedupe.rs
      ranking.rs
      cancellation.rs
    providers/
      mod.rs
      history.rs
      command_spec.rs
      filesystem.rs
      path_command.rs
      process.rs
      network_interface.rs
      project.rs
      ai_action.rs
      ai_client.rs
    specs/
      mod.rs
      model.rs
      loader.rs
      compiler.rs
      validate.rs
    history/
      mod.rs
      import.rs
      format_bash.rs
      format_zsh.rs
      format_fish.rs
      index.rs
      store.rs
      compact.rs
    project/
      mod.rs
      discover.rs
      package_json.rs
      cache.rs
    ai/
      mod.rs
      detector.rs
      context.rs
      protocol.rs
      validate.rs
    safety/
      mod.rs
      classifier.rs
      policy.rs
      redact.rs
    config/
      mod.rs
      model.rs
      load.rs
      paths.rs
      watch.rs
    platform/
      mod.rs
      command_probe.rs
      os.rs
    error.rs
  assets/
    specs/
      common/*.toml
      linux/*.toml
      macos/*.toml
  tests/
    fixtures/
    pty/
    golden/
    compatibility/
  benches/
```

依赖方向固定为：

```text
cli -> app -> terminal / pty / shell / completion / history / config
                         completion -> parser / providers / specs / safety
                                      providers -> history / project / ai / platform

terminal 内部依赖固定为：

input/reply -> capabilities
surface -> ratatui Buffer/layout/widget
compositor -> surface/model/capabilities
output -> render_boundary/safe_boundary/model/compositor/scheduler/guard

底层模块不得反向依赖 app 或 provider 实现。跨边界只传不可变快照、领域事件和 channel handle，不能传 `Stdout`、TTY 可写 fd 或 backend 的可写引用。
```

## 2. 核心领域类型

以下是设计级 Rust API，编码时允许调整命名，但不得破坏注释中的不变量。

### 2.1 输入快照

```rust
pub struct BufferSnapshot {
    pub text: Arc<str>,
    pub cursor: usize,          // UTF-8 byte offset
    pub revision: Revision,
    pub sync: SyncQuality,
}

pub enum SyncQuality {
    Exact,                     // 来自 shell hook
    Mirrored,                  // 来自受支持按键的本地镜像
    Uncertain,                 // 禁止产生或激活候选
}

pub struct CompletionContext {
    pub query_id: QueryId,
    pub shell: ShellKind,
    pub platform: Platform,
    pub cwd: Arc<Path>,
    pub buffer: BufferSnapshot,
    pub active_segment: Range<usize>,
    pub tokens: Arc<[Token]>,
    pub replacement: Range<usize>,
    pub quote: QuoteContext,
    pub command: Option<CommandIdentity>,
    pub expected_slot: Option<SlotRequest>,
    pub project: ProjectSnapshot,
}
```

`CompletionContext` 创建后不可变。Provider 不得重新读取“当前全局 CWD”或“当前缓冲区”；需要的新状态必须进入下一 query。

### 2.2 候选与动作

```rust
pub struct Candidate {
    pub id: CandidateId,
    pub display: DisplayText,
    pub edit: Option<TextEdit>,
    pub action: CandidateAction,
    pub source: CandidateSource,
    pub kind: CandidateKind,
    pub completeness: Completeness,
    pub risk: RiskLevel,
    pub score: ScoreSignals,
    pub provenance: Provenance,
}

pub struct DisplayText {
    pub primary: Arc<str>,
    pub description: Arc<str>,
    pub annotation: Option<Arc<str>>,
}

pub struct TextEdit {
    pub range: Range<usize>,
    pub replacement: Arc<str>,
    pub cursor_after: CursorPlacement,
}

pub enum CandidateAction {
    Insert,
    InsertAndContinue { next_slot: SlotKind },
    RunCurrent { expected_revision: Revision, expected_hash: BufferHash },
    RequestAi,
    ConfigureAi,
    RetryProvider(ProviderId),
}

pub enum Completeness {
    Runnable,
    NeedsInput { slot: SlotKind },
    ActionOnly,
}

pub enum RiskLevel {
    ReadOnly,
    Low,
    Medium,
    High,
    Unknown,
}
```

重要约束：

- `display.primary` 永远不能被当作执行文本；实际修改只读取 `TextEdit`。
- `TextEdit.range` 必须在生成它的 revision 上有效，且起止位于 UTF-8/grapheme 边界。
- `RunCurrent` 不携带替换文本。Reducer 只有在 revision/hash 未变、当前输入完整、风险不高于 `Low`、spec provenance 有效时才写入 Enter。
- `AI`、`History`、`RiskLevel::High/Unknown` 和含未解槽位的候选不能构造 `RunCurrent`。
- 文件名展示值与 shell escaped replacement 分开保存。

### 2.3 Provider 接口

```rust
pub trait CandidateProvider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn class(&self) -> ProviderClass;
    fn applies(&self, context: &CompletionContext) -> bool;
    fn complete(
        &self,
        context: Arc<CompletionContext>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<ProviderOutput, ProviderError>>;
}

pub struct ProviderOutput {
    pub candidates: Vec<Candidate>,
    pub diagnostics: Vec<ProviderDiagnostic>,
    pub cache_hint: CacheHint,
}
```

Provider 必须满足：

- 响应取消；阻塞系统调用必须放到 bounded blocking pool。
- 不直接执行用户命令，不写终端，不持有 reducer lock。
- 返回数量有上限，错误带 provider id 和稳定 error code。
- 对相同 context 和相同外部快照尽量确定性输出。
- 外部程序探测使用 argv 和超时，不使用 `sh -c`。

### 2.4 视图、终端版本与帧请求

```rust
pub struct ViewModel {
    pub query_id: QueryId,
    pub buffer_revision: Revision,
    pub mode: ViewMode,
    pub rows: Arc<[CandidateRowView]>,
    pub selected: Option<CandidateId>,
    pub status: Option<StatusRowView>,
    pub page: PageView,
    pub interaction_enabled: bool,
}

pub struct CandidateRowView {
    pub id: CandidateId,
    pub kind_label: MessageId,
    pub primary: SanitizedText,
    pub description: SanitizedText,
    pub annotation: Option<SanitizedText>,
    pub risk: RiskLevel,
    pub style_role: StyleRole,
}

pub enum ViewMode {
    Unified,
    HistoryOnly,
    AiLoading,
    AiResults,
}

pub struct StatusRowView {
    pub message: MessageId,
    pub detail: Option<SanitizedText>,
    pub tone: StatusTone,
    pub action: Option<StatusAction>,
}

pub enum StatusAction {
    RetryProvider(ProviderId),
    RetryAi,
    ConfigureAi,
}

pub struct PageView {
    pub index: usize,
    pub total: usize,
}

pub struct ScreenRevision(u64); // 任意可能改变 terminal model 的 child 输出后递增
pub struct ScreenEpoch(u64);    // resize/scroll/失步/屏幕切换后递增
pub struct FrameRevision(u64);  // reducer 可见状态变化后递增

pub struct Anchor {
    pub shell_cursor: CellPos,
    pub overlay_origin: CellPos,
    pub terminal_size: Size,
    pub screen_revision: ScreenRevision,
    pub screen_epoch: ScreenEpoch,
    pub confidence: AnchorConfidence,
}

pub enum AnchorConfidence {
    Exact,   // prompt boundary + CPR 已确认
    Derived, // 只经过 TerminalModel 能完整解释的 VT delta
    Unknown, // 禁止显示 overlay
}

pub enum SyncOwnership {
    None,
    External,            // 启动前或 child 持有，Hokann 不得 ESU
    MayBeOpenByHokann,   // BSU 可能已写出，恢复路径必须 ESU
}

pub enum RenderReadiness {
    AwaitingPromptMarker { boundary_id: BoundaryId },
    AwaitingRedisplay {
        buffer_revision: Revision,
        boundary_id: BoundaryId,
    },
    Ready {
        buffer_revision: Revision,
        screen_revision: ScreenRevision,
    },
    Unknown,
}

pub struct SurfaceKey {
    pub screen_epoch: ScreenEpoch,
    pub rect: Rect,
    pub theme_revision: u64,
    pub width_policy: WidthPolicy,
}

pub struct FrameRequest {
    pub query_id: QueryId,
    pub frame_revision: FrameRevision,
    pub buffer_revision: Revision,
    pub screen_revision: ScreenRevision,
    pub screen_epoch: ScreenEpoch,
    pub view: Arc<ViewModel>,
}

pub struct RenderGateRequest {
    pub boundary_id: BoundaryId,
    pub buffer: BufferSnapshot,
    pub anchor: Anchor,
    pub deadline: Instant,
}

pub struct ChildOutputBatch {
    pub read_cycle: u64,
    pub bytes: Bytes,
    pub drain: DrainState,
}

pub enum DrainState {
    MoreInCurrentCycle,
    DrainedToEagain,
}
```

`ViewModel` 只含经过控制字符/bidi/宽度清洗的展示文本，不含 `TextEdit.replacement`、原始 history/AI command 或 secret。redisplay 等待期间可以复用旧 rows，但必须设置 `interaction_enabled = false`；reducer 仍以 query/revision gate 为最终保护。

四个版本门不能互相替代：`buffer_revision` 防止旧候选落到新输入，`frame_revision` 合并旧 UI 状态，`screen_revision` 拒绝基于旧 child model 的 frame，`screen_epoch` 防止旧坐标写入已经 resize、滚动或失步的屏幕。只有 `SurfaceKey` 完全相同时才能把 previous Ratatui `Buffer` 用作 diff 基线。

## 3. `app`：状态和编排

### 3.1 职责

- 持有唯一 `AppState`，执行状态转换。
- 根据输入 revision 启动/取消 completion query。
- 汇合 control event 与 PTY render boundary/convergence；只有 `RenderReadiness::Ready` 才构造 frame request。
- 验证 candidate activation，生成 PTY/IPC/render effect。
- 协调 shell 生命周期、provider runtime、storage 和 config reload。
- 在退出前驱动 terminal restoration。

### 3.2 非职责

- 不解析具体 history 格式。
- 不拼接 ANSI 字符串。
- 不发 HTTP 或扫描目录。
- 不自行修改 candidate 分数。

### 3.3 激活门

`reducer::activate_candidate` 是唯一执行语义入口：

```text
1. candidate 是否属于当前 query？否则拒绝。
2. buffer revision/hash 是否匹配？否则刷新。
3. action = RequestAi/ConfigureAi/Retry？执行对应 effect，不触碰 PTY Enter。
4. action = Insert/InsertAndContinue？校验 edit，交给 shell adapter 回填。
5. action = RunCurrent？再次校验 provenance、risk、completeness、文本相等；仅写 Enter。
6. 任一不确定条件都降级为 Insert 或拒绝，不能升级为执行。
```

## 4. `terminal` 与 `pty`

### 4.1 `terminal::guard`

`TerminalGuard` 记录原 termios、光标可见性、SGR、raw mode 和 `SyncOwnership`。构造完成前不改变全局状态；启用后把 terminal lease 转移给 `OutputActor` 所在线程，使正常、channel close、panic unwind 和 I/O error 的恢复仍由唯一 writer 排序。`Drop` 只在 `SyncOwnership::MayBeOpenByHokann` 时补发 ESU，随后恢复 SGR/光标/termios；不能 reset 启动前或 child 持有的 mode。signal handler 不执行复杂 Rust 逻辑，而通过 self-pipe/signal bridge 通知主循环。

挂起流程：清 overlay -> 恢复 termios/光标 -> 将 `SIGTSTP` 重新交给自身。继续流程：重新获取尺寸 -> raw mode -> 等 prompt anchor -> redraw。

不能承诺处理 `SIGKILL` 或宿主机掉电；其他支持的退出路径必须进入同一个 restore transaction。若 `OutputActor` 自身异常，guard 只允许在 unwind 的最后恢复阶段执行一次紧急直写，此例外不能用于正常 frame、日志或诊断。

### 4.2 `terminal::input`、`reply` 与 `capabilities`

真实 stdin 只有一个 reader。每个 byte chunk 先交给 `TerminalReplyRouter`：它只在存在匹配的 outstanding query 时消费 CPR/DECRPM 等终端回复，剩余字节原样交给 `InputDecoder`。不允许另起同步 cursor query 与 input reader 争抢 stdin。

`InputDecoder` 的增量状态包括：

- 待完成 UTF-8 bytes；
- ESC ambiguity timer；
- CSI/SS3 sequence；
- bracketed paste buffer 与上限；
- 配置 keymap 的 trie。

不要假定一次 `read` 等于一个按键，也不要逐 byte 修改 Unicode 文本。

`TerminalReplyRouter` 必须满足：

- 最多一个可能与用户输入混淆的 query outstanding，并有 byte 上限和 deadline；
- 先完成 router registration，再由 `OutputActor` 写 query bytes，避免 reply 在 outstanding state 建立前返回；
- 只接受与当前 query type、预期语法、deadline 和状态匹配的回复；协议没有可用 nonce 时不得假造关联能力。匹配前缀在 deadline 内有界暂存，失败后交还 input decoder，不能误吞用户按键；
- 用 `CSI ? 2026 $ p` 的 DECRQM/DECRPM 结果判断 synchronized output，不根据 `$TERM` 或品牌猜测；
- mode 2026 返回 `reset` 才标记 `AvailableIdle`；`set`/`permanently set` 视为 `BusyExternal` 并延后/隐藏，不能嵌套或擅自发送 ESU；unrecognized、timeout、permanently reset 标记 `UnsupportedFallback`；
- CPR 只用于 prompt anchor 建立和失步恢复，不进入逐键热路径。

`capabilities.rs` 保存已探测能力及其来源、deadline 和降级原因；resize、resume 或 terminal identity 明确改变时才允许重新探测，不能造成按键延迟。

### 4.3 `terminal::render_boundary`、`safe_boundary` 与 `model`

`RenderBoundaryDecoder` 是 child data plane 唯一允许消费 bytes 的窄例外。它只在 parser ground state、shell foreground/prompt phase 识别由 Hokann shell adapter 生成的版本化 marker：固定前缀、session token、`BoundaryId`、marker kind 和 checksum/terminator 全部匹配时产生 `PromptRendered` 或 `PostRedisplay` 事件；其他 bytes 必须 byte-for-byte 交给 child output pipeline。

marker 使用有上限的私有 OSC/DCS envelope，并由 zsh `%{...%}`、Bash `\[...\]` 等 adapter-specific 非打印包装嵌入。确切 envelope 在 P0 transcript/terminal/shell-width 测试后锁定。它不能携带 buffer、CWD 或其他用户文本；session token 不写日志。decoder 必须支持任意分片、前缀回退、重复/迟到/伪造 marker 和 channel close。token 只用于降低意外碰撞，不是安全边界，因为 child process 可能继承它；phase/id/revision gate 仍必须独立成立。

control FD 的 `PromptBoundary/BufferSnapshot` 不是 render-ready 信号：

- `PromptBoundary(boundary_id)` 等对应 PTY marker 后才允许发 CPR；
- `BufferSnapshot(redisplay_id, revision)` 立即启动 provider，但先进入 `AwaitingRedisplay`；
- adapter 能提供可靠 post-redraw marker 时按同 id 解锁；否则由 `TerminalModel` 在后续 screen revision 上验证 expected cursor/layout、安全边界和 PTY nonblocking drain；
- deadline 只把状态变为 `Unknown`/隐藏，不得把等待超时当作绘制许可。

`SafeBoundaryScanner` 只回答“当前 child byte stream 后能否插入 Hokann 控制序列”。它跟踪任意分片的 UTF-8、ESC、CSI、OSC、DCS、SOS/PM/APC 和 string terminator，不解释屏幕布局。若 chunk 结束在控制序列中，child bytes 仍应立即原样转发，但 overlay frame 保留到下一个安全边界；超长或超时 control string 触发透明透传并把 anchor 置为 `Unknown`。

`TerminalModel` 首选封装 `vt100::Parser`，观察与真实终端完全相同的 child bytes，维护：

- primary/alternate screen、absolute cursor、wrap-pending 和 scroll；
- 当前 SGR、cursor visibility、synchronized-output ownership、erase/cursor movement 对 overlay rect 的影响；
- 能否从最近的精确 anchor 推导新位置；
- 未识别的 cursor/mode/control sequence 及失步原因。

它不重新绘制 child 屏幕，也不修改或规范化待转发 bytes。未知的 cursor-affecting 序列、非法 UTF-8、无法映射的 scroll 或 alternate-screen 往返推进 `ScreenEpoch` 并使 anchor 失效。开发测试用 `avt` adapter 作为第二语义实现；裸 `vte` parser 不能单独承担 screen model。

### 4.4 `terminal::surface` 与 `compositor`

`surface.rs` 把纯 `ViewModel` 渲染到 Ratatui 离屏 `Buffer`。它负责固定 rect 内的 layout、style、cell-width 截断和 blank padding，不访问 stdout、provider 或 app state，也不在 widget 文本中嵌入 ANSI。

同一 overlay session 的 surface 高度固定。连续 query、候选减少、loading 文案变化和选中态移动只改变 rect 内 cells，不增删物理行；内容宽度最多为 `terminal_cols - 1`，永不写最后一列。首次打开、resize 或受控 layout 切换时需要的空间只预留一次。

`OverlayCompositor` 持有 previous/current `Buffer` 和 `SurfaceKey`，输出完整的 `StagedFrame<Vec<u8>>`：

```text
可用且空闲：BSU -> hide cursor -> cell diff -> restore child SGR/CUP/visibility -> ESU
fallback：   hide cursor -> non-destructive cell diff -> restore child SGR/CUP/visibility
```

frame 必须先在内存中完整编码，再交给 `OutputActor` 一次 `write_all`、一次 `flush`。正常导航禁止 clear-before-paint；禁止 `ED 2/3`、alternate screen、DECSC/DECRC（`ESC 7/8`）和最后一列 write。关闭 overlay 是 current surface 到 blank surface 的 diff；只有确认整行属于专用 rect 时才能用 `EL` 清理 stale suffix。

### 4.5 `terminal::scheduler` 与 `output`

`FrameScheduler` 只做吞吐控制，不承担正确性：pending slot 最多一个 `FrameRequest`，新 revision 覆盖旧 revision；idle 不重绘；本地默认最多 60 FPS。用户导航进入最近一帧，provider batch 可以合并，但不能让输入等待固定 20/25 ms 或等待 provider settle。

`OutputActor` 是真实 stdout 的唯一 owner。对外只暴露不可复制底层 fd 的 `OutputHandle`，消息至少包括：

```rust
pub enum OutputCommand {
    ChildOutput(ChildOutputBatch),
    ArmRenderGate(RenderGateRequest),
    CommitLatest(FrameRequest),
    HideOverlay {
        screen_revision: ScreenRevision,
        screen_epoch: ScreenEpoch,
        surface_key: SurfaceKey,
    },
    Probe(TerminalProbe),
    Resize(Size),
    RestoreAndSuspend,
    RestoreAndExit,
}
```

处理优先级为 restore/signal > 不可丢弃的 child output > overlay hide/invalidate > 最新 overlay frame。提交前再次校验 `frame_revision`、`buffer_revision`、`screen_revision`、`screen_epoch`、anchor confidence 和 safe boundary；任一过期或不可证明条件都丢弃 frame/隐藏 UI，不能猜坐标。

child output 到达时，actor 先经 `RenderBoundaryDecoder` 分离受信 marker，再用其余完全相同的 bytes 更新 scanner/model，最后把“必要的旧 surface 清理 + child bytes + 可用的最新 overlay repaint”组合成一个 transaction。支持 mode 2026 且当前 idle 时，整个 transaction 可由一对 Hokann-owned BSU/ESU 包围；child 或启动前状态持有 mode 时只透传/延后，不能嵌套或擅自 ESU。fallback 直接覆盖变化 cells，不能先清空列表再转发。child bytes 永不因 overlay backpressure 丢弃，overlay 队列也永不积压旧帧。

`RenderGateRequest` 带 buffer/boundary id、expected text/cursor、anchor snapshot 和 deadline。`OutputActor` 持有 gate，因为只有它同时知道最近 marker、`TerminalModel`、`screen_revision` 和 PTY drain cycle：gate 后到时可匹配已缓存 marker/当前模型，gate 先到时等待后续 batch；两种顺序都不能依赖 channel 到达先后猜测。

普通模块、日志系统、panic hook 和 Ratatui backend 都不得直接写 stdout。静态架构测试/代码审查应搜索 `stdout()`、`from_raw_fd`、TTY fd clone 和 `CrosstermBackend<Stdout>`，只有 `terminal::output/guard` 的受限实现允许出现。

### 4.6 `pty`

- 创建 child PTY、设置窗口尺寸、启动 shell process group。
- 查询 foreground pgid，转发 resize 和必要信号。
- 子进程启动后避免在多线程上下文中执行不安全的 fork 后逻辑；优先使用经过审计的 PTY crate 接口。
- PTY read/write 是独立的 bounded pump；关闭顺序必须能唤醒阻塞 read。read side 使用 nonblocking fd，在一次 readiness cycle 中读到 `EAGAIN`，最后一个 `ChildOutputBatch` 标记 `DrainedToEagain`；单次 `read` 或任意大小 chunk 不能充当 redisplay boundary。

## 5. `shell`：适配器与协议

### 5.1 接口

```rust
pub trait ShellAdapter: Send + Sync {
    fn kind(&self) -> ShellKind;
    fn executable(&self, config: &Config) -> Result<PathBuf, ShellError>;
    fn child_args(&self, login: bool) -> Vec<OsString>;
    fn init_script(&self, protocol: ProtocolVersion) -> String;
    fn capabilities(&self) -> ShellCapabilities;
    fn decode_cursor(&self, value: usize, text: &str) -> Result<usize, ShellError>;
    fn replacement_sequence(&self) -> KeySequence;
    fn render_boundary_capability(&self) -> RenderBoundaryCapability;
}
```

`RenderBoundaryCapability` 为 `PostRedisplayMarker | ModelConvergence | Unsupported`。能力由实际 adapter PoC 与 compatibility test 决定，不能仅因某 shell 存在 pre-redraw hook 就标记为 post-redraw。

### 5.2 子 shell 到 Hokann 的协议

控制 FD 使用 `NUL` 分帧，因为 shell 变量本身不能包含 NUL。v2 每帧以稳定 type 和字段
前缀开始，最后一个字段保留原始 UTF-8 payload；parser 设置单帧 64 KiB 上限。命令只在
`START` 携带一次，decoder 按 shell phase 保存并与 `END` 配对，避免 CWD 与命令都含 Tab
时产生字段歧义。

```text
HKP2\tPROMPT\t<boundary_id>\t<cwd>\0                  # zsh/fish
HKP2\tPROMPT\t<boundary_id>\t<history_control>\t<cwd>\0 # bash
HKP2\tBUFFER\t<redisplay_id>\t<cursor>\t<buffer bytes>\0
HKP2\tSTART\t<command bytes>\0
HKP2\tEND\t<exit_code>\t<cwd>\0
```

协议 parser：

- 对未知版本/事件忽略并诊断；
- 不把 payload 写入日志；
- 验证 cursor 与 UTF-8；
- 验证 boundary/redisplay id 单调且与当前 shell phase 相符；重复、倒退或跨 epoch id 不能解锁 render；
- 拒绝没有匹配 `START` 的 `END`；若 `END` 丢失，下一 `PROMPT` 重置 phase，runtime 用
  pending command 做无 exit code 的降级记录；
- 固定字段从左侧解析，最后一个 CWD/buffer/command 字段保留 Tab 和换行；
- 超长或畸形帧使当前 buffer 进入 `Uncertain`，但不终止 shell。

### 5.3 Hokann 到子 shell 的回填

1. Reducer 将 versioned `EditPlan` 放入 session queue。
2. 向 PTY 写入 adapter 的保留键序列。
3. shell widget 调用同一二进制的轻量 `hokann ipc take --session <token>`。
4. widget 通过 shell 原生 API 原子更新 buffer/cursor。
5. shell 发送新的 `BUFFER`；收到确认前不启动下一 query。

IPC server 校验 peer/session、一次性 sequence number 和 payload 上限。Session 目录退出时删除；意外遗留由下次启动按 PID/age 清理。

## 6. `parser`

### 6.1 Token 类型

```rust
pub enum TokenKind {
    Word,
    Whitespace,
    Pipe,
    AndIf,
    OrIf,
    Separator,
    Redirect,
    Comment,
}

pub struct Token {
    pub kind: TokenKind,
    pub range: Range<usize>,
    pub cooked_prefix: Arc<str>,
    pub quote: QuoteContext,
}
```

Lexer 只执行无副作用的词法识别。`$()`、反引号、process substitution 等内容整体标记为 `Opaque` 或不安全上下文；v1 不进入其内部补全。

### 6.2 引用与转义

`quote::escape_for_shell(value, context, shell)` 返回插入文本：

- 未引用 POSIX context：优先反斜杠转义安全字符，复杂值使用单引号并正确处理内部 `'`。
- 单引号内：使用 close-escape-reopen 策略。
- 双引号内：只转义该 shell 中仍有特殊意义的字符。
- Fish 使用独立规则，不复用 POSIX 假设。
- 含换行或控制字符的路径默认不建议，除非用户显式配置。

该模块必须有 property tests：把生成文本交给目标 shell 的只解析 fixture，所得单一 argv 必须与原始路径 byte-for-byte 一致。

## 7. `completion`

### 7.1 Registry 和调度

Registry 在启动时注册 provider，并按 `ProviderClass` 设置并发与超时。Engine 根据 `applies` 生成执行计划；内存 provider 可以串行以减少调度开销，文件/系统/网络 provider 使用独立并发限额。

建议限额：

- filesystem/project：最多 4 个并发任务；
- bounded process probes：最多 2 个；
- AI：每个 session 最多 1 个，输入变化立即取消；
- 每 query 总候选在 dedupe 前不超过 1,000，最终默认展示不超过 12。

### 7.2 Ranking signal

```rust
pub struct ScoreSignals {
    pub match_quality: i16,       // 0..1000
    pub source_trust: i16,        // 0..300
    pub spec_priority: i16,       // -100..100
    pub cwd_affinity: i16,        // 0..100
    pub frecency: i16,            // 0..200
    pub sequence: i16,            // 0..100
    pub risk_penalty: i16,        // 0..300
    pub incomplete_penalty: i16,  // 0..100
}
```

不要让 provider 直接提供最终总分。Provider 只填自己有权知道的静态优先级；match/frecency/risk 由集中模块计算，防止来源间不可比较。

## 8. Provider 设计

### 8.1 `history`

- 启动后后台加载 snapshot + event tail，并构建规范化命令索引。
- shell 原始 history 延迟导入；以 canonical path、inode、size、mtime 和 byte offset 记录 checkpoint。
- 文件截断/rotate 后重新从头解析并靠 command hash 去重。
- multiline history 还原为单条记录；含真实换行的命令可以搜索，但 v1 默认不作为一键回填候选。
- 搜索分三层：exact/prefix/substring 先走索引，再用 fuzzy matcher 补齐剩余名额。
- frecency 采用按时间衰减的 count；sequence 只存规范化 command skeleton，避免把 secret argument 复制到更多索引。

隐私过滤至少支持：正则、命令名前缀和 `HISTCONTROL=ignorespace` 语义。内置启发式标记包含明显 token/password flag 的命令，不写 debug log；是否排除存储由用户配置。

### 8.2 `command_spec`

- 根据当前 command tree、已使用 option 和 expected slot 产生配方。
- 同一 flag 已存在时不重复建议，除非规格声明 repeatable。
- OS variant 由编译后 guard 过滤。
- 精确 command 输入时创建 direct/default candidate；所有 action 仍由 reducer 二次验证。

### 8.3 `filesystem`

- 使用 `read_dir` 单层扫描，不 follow symlink 递归。
- 先按 slot kind 过滤，再按 prefix/fuzzy 排序；目录永远保留。
- metadata 查询按需执行，避免对每个 entry 调用 `stat`；`Executable` slot 才检查 mode/extension。
- cache key 包含 canonical CWD、目录 metadata fingerprint、hidden policy 和 slot kind。
- 返回 `display` 原始文件名与经过 `parser::quote` 生成的 edit。

### 8.4 `process` 与 `network_interface`

- `kill` 的 process provider 使用平台原生 `/proc` 或有界 `ps` argv，显示 PID、owner、command。
- 默认仅展示当前用户可操作进程，排除 Hokann/self 和当前 shell；用户查询精确 PID 时可显示更多。
- 进程候选是 `InsertAndContinue/Insert`，永远不是 `RunCurrent`。
- 接口 provider 在 Linux 读取系统接口或调用固定 argv，在 macOS 使用系统 API/固定探测；结果按接口状态排序。

### 8.5 `project`

- `discover` 从 CWD 向上查找 manifest，默认在 filesystem root 停止；可配置在最近 `.git` root 停止。
- `package_json` 只读取普通文件，默认最大 2 MiB，使用 `serde_json` 只提取 `scripts: Map<String, String>`。
- cache key 为 canonical manifest path + file identity + mtime + size；文件事件只是优化，正确性依靠 key。
- script 名按 exact/prefix/fuzzy 排序，description 是去控制字符并按 cell width 截断的 script body。
- 不运行 `pnpm`、不执行 lifecycle hook、不读取依赖树。

### 8.6 `ai_action` 与 `ai_client`

`ai_action` 是本地 provider，只根据 detector 和配置产生动作项。`ai_client` 只能由 `RequestAi` effect 调用。

OpenAI 兼容请求模型：

```rust
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,       // 默认 0.1
    max_tokens: u32,        // 小上限
}

struct AiCommand {
    command: String,
    explanation: String,
}
```

若 endpoint 已是 `/chat/completions` 则原样使用；否则在规范化 base URL 后追加。响应 body 有硬上限。v1 不依赖 provider-specific function calling 或 streaming，以保持兼容。

## 9. `specs` 与 schema

### 9.1 示例：`ls`

```toml
schema = 1

[command]
id = "core.ls"
name = "ls"
description = "列出目录内容"
platforms = ["linux", "macos"]
requires_arguments = false
risk = "read_only"

[command.default]
kind = "run_current"

[[recipes]]
id = "long_all"
template = "ls -la"
description = "以长格式显示全部条目"
risk = "read_only"
activation = "insert"

[[recipes]]
id = "human_all"
template = "ls -lah"
description = "显示隐藏条目和易读大小"
risk = "read_only"
activation = "insert"

[[slots]]
position = 1
kind = "path_any"
provider = "filesystem"
optional = true
repeatable = true
```

### 9.2 示例：`tar`

```toml
schema = 1

[command]
id = "core.tar"
name = "tar"
description = "创建或展开归档"
platforms = ["linux", "macos"]
requires_arguments = true
risk = "medium"

[command.default]
kind = "recipe"
recipe_id = "create_gzip"

[[recipes]]
id = "create_gzip"
template = "tar -czf ${archive} ${paths}"
description = "创建 gzip 压缩归档"
risk = "medium"
activation = "continue"

[[recipes.slots]]
name = "archive"
kind = "new_file"
provider = "filesystem"

[[recipes.slots]]
name = "paths"
kind = "path_any"
provider = "filesystem"
repeatable = true
```

### 9.3 编译与验证

- 内置 TOML 通过 `include_bytes!` 打包；编译/测试阶段运行同一 validator。
- `id` 全局唯一；用户覆盖必须显式写 `replaces = "core.ls"`。
- `platforms`、`activation`、`risk`、slot kind 和 provider id 使用封闭 enum。
- `description` 不允许 ANSI/control bytes；长度有上限。
- 未解 slot 不会把 `${name}` 原样插入 shell。Engine 只插入 slot 之前的安全 prefix，并打开下一 slot 候选。
- 规格版本不兼容时拒绝单文件，不影响其他规格。

## 10. `history::store`

### 10.1 事件

```rust
pub enum HistoryEventV1 {
    Executed {
        event_id: EventId,
        timestamp_ms: i64,
        command: String,
        cwd: Option<PathBuf>,
        shell: ShellKind,
        exit_code: Option<i32>,
        session_id: SessionId,
    },
    Imported { /* source fingerprint + record */ },
    Tombstone { command_hash: CommandHash },
}
```

磁盘记录格式为 magic/version/length/checksum/payload。Payload codec 必须显式版本化，不能直接依赖 Rust struct layout。单条 command 有长度上限；path 按平台字节语义保存，展示时 lossy 转换但不用于执行。

### 10.2 内存索引

- `CommandId -> CommandRecord`
- normalized command hash -> count/last-used/last-cwd
- token/trigram -> compact command id posting list
- previous skeleton + next skeleton -> transition stats
- recency ring -> empty-query 列表

Compaction 只合并统计，不保留无限事件。用户删除记录时写 tombstone，下一 compaction 物理删除。

## 11. `ai`

### 11.1 自然语言 detector

Detector 输出 `NaturalLanguageScore { score, reasons }`。建议特征：

| 特征 | 方向 |
| --- | --- |
| 首 token 是已知 command/alias/path | 强负 |
| 包含 `--flag`、pipe、redirect、assignment | 负 |
| 多个自然语言词或较高 CJK 比例 | 正 |
| 包含“查找/显示/删除/如何/文件”等意图词 | 正 |
| 仅一个短单词 | 负 |
| 配置的强制前缀 `??` | 直接触发 |

规则和阈值用 fixture 测试，不引入模型。强制前缀在发送 AI 前移除。

### 11.2 上下文构建

默认 payload 只包含：

```text
request text
OS + architecture
shell name
current directory basename
detected project kinds (node/rust/python/...)
```

不包含环境变量值、history、Git diff、文件内容。后续每一类扩展上下文都必须是单独 opt-in 配置。

### 11.3 响应验证

顺序为：body size -> JSON shape -> item count -> UTF-8/control/newline -> length -> tolerant lexer -> risk classifier -> candidate conversion。任何步骤失败都不能把原始模型输出当作 command。

## 12. `safety`

`RiskClassifier` 接收 parser tokens，不使用简单 substring 作为唯一依据。规则返回最高风险和 reasons；未知语法为 `Unknown`。

Policy 硬约束：

```text
AI source                         -> never RunCurrent
History source                    -> never RunCurrent
High / Unknown                    -> insert only + visible warning
NeedsInput                        -> InsertAndContinue
Spec direct + exact current text
  + risk <= Low + current query   -> RunCurrent allowed
```

风险标签是交互保护，不是安全保证。Hokann 不尝试 sandbox 用户最终执行的 shell。

## 13. `config`

建议初始配置：

```toml
version = 1

[core]
# 省略 shell 时自动检测；也可显式设为 "zsh"、"bash" 或 "fish"
login_shell = false

[ui]
max_rows = 12
max_width = 100
color = "auto"
ascii_icons = true
show_hidden = false

[keys]
accept = "tab"
activate = "enter"
up = "up"
down = "down"
page_up = "page-up"
page_down = "page-down"
dismiss = "escape"
history = "ctrl-r"
toggle = "back-tab"

[history]
enabled = true
max_command_bytes = 16384
exclude = []

[completion]
local_timeout_ms = 100
max_candidates = 1000

[logging]
enabled = false
max_bytes = 1048576
rotations = 3

[ai]
enabled = false
endpoint = "https://api.openai.com/v1"
model = ""
api_key_env = "OPENAI_API_KEY"
timeout_ms = 8000
trigger_prefix = "??"
send_cwd_basename = true
```

未知字段默认拒绝；错误 enum/越界值使配置验证失败并保留 last-known-good。`hokann config show` 对所有 secret 值显示 `<redacted>`。日志配置需要重启才生效；日志默认关闭，启用后只写入有界、轮转、脱敏的类型化事件，不记录 query、history、CWD、HTTP body 或环境变量值。

## 14. CLI 表面

```text
hokann [--shell zsh] [--login]
hokann init <zsh|bash|fish>
hokann setup [--shell ...]
hokann uninstall --integration-only
hokann doctor [--json]
hokann config <path|show|validate|init>
hokann config ai
hokann history <import|stats|prune|clear>
hokann spec <list|show|validate>
hokann ipc <emit|take>            # internal, hidden from normal help
hokann --version
```

`setup`、`uninstall`、`history clear` 涉及写入或删除，必须精确显示目标并保持可恢复性。`uninstall --integration-only` 不删除 history/config。

## 15. 错误与诊断

库模块返回 typed error；只有 CLI/supervisor 决定如何显示或退出。错误需要稳定 code，例如：

```text
HK-TTY-001 not_a_tty
HK-SHL-003 protocol_mismatch
HK-SPC-010 invalid_recipe_slot
HK-HIS-007 corrupt_tail_record
HK-AI-401 unauthorized
HK-AI-429 rate_limited
```

Provider 诊断分为 `Debug`、`Notice`、`ActionRequired`；只有后两者可进入 UI，且同一 query/source 去重。错误消息不得包含完整用户输入、secret 或 Authorization header。

## 16. 测试接缝

只在真正的外部边界定义替身：

- `Clock`：frecency、timeout、compaction。
- `FileSystemView`：manifest、history import、directory fixture。
- `HttpTransport`：AI 响应与取消。
- `CommandProbe`：固定 argv 的平台探测。
- `PtyHarness`：真实 PTY integration，而不是为 PTY 本身写空 mock。

Parser、ranking、spec compiler、risk policy 和 reducer 应尽量为纯函数，使用 table/golden/property/fuzz tests。Surface/compositor 使用 tokenized ANSI snapshot 和双虚拟终端语义测试；完整交互使用真实 shell 的 PTY harness，并对 child/output bytes 做随机分片。
