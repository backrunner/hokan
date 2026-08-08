# Hokan 总体架构

## 1. 架构目标

Hokan 最难的部分是保持终端和 shell 行编辑器一致，而不是生成候选。架构优先保证以下不变量：

1. **shell 是执行真相源**：Hokan 不解释或执行用户命令，只向 shell 提交最终字节。
2. **单一状态所有者**：输入缓冲区、选中项、query revision 和 overlay 状态只由 reducer 修改。
3. **单一终端写入序列**：child output、surface 清理、probe 和 overlay frame 必须经过同一 `OutputActor` 排序，不能并发写 stdout。
4. **候选有版本**：任何异步结果都携带 query id；过期结果被丢弃。
5. **执行需显式意图**：`Enter` 无选中时只提交用户亲手输入的 buffer；执行候选改写文本必须先由用户显式选中（必要时经二次确认），其余动作最多修改输入。
6. **本地功能不依赖网络**：AI 超时、断网或配置错误不会阻塞 history/spec/files/project。
7. **终端可恢复优先**：所有退出和挂起路径先恢复 termios 与光标，再处理日志或错误输出。

## 2. 系统上下文

```text
                    local reads                 optional HTTPS
              +-------------------+          +-------------------+
              | history / specs   |          | OpenAI-compatible |
              | files / manifests |          | chat completions  |
              +---------+---------+          +---------+---------+
                        |                              ^
                        v                              |
+----------+ raw I/O  +--------------------------------------+  PTY  +-----------+
| terminal |<-------->|                hokan                |<----->| child     |
| / SSH    |          | input + reducer + providers + render |       | shell     |
+----------+          +------------------+-------------------+       +-----+-----+
                                           ^                              |
                                           | control protocol             | exec
                                           +------------------------------+
```

`hokan` 是前台父进程。它创建 PTY、启动用户 shell、将真实终端切到 raw mode，并根据 shell 状态决定“拦截用于候选的输入”还是“完全透传给前台程序”。不使用常驻 daemon。

## 3. 进程与 I/O 模型

### 3.1 启动序列

1. CLI 解析配置并运行 preflight：stdin/stdout 是否为 TTY、`TERM`、shell 路径、数据目录权限。
2. 创建 session id、双向控制通道和 child PTY。
3. 为子 shell 设置 `HOKAN_ACTIVE=1`、session token、控制 FD 等环境。
4. 启动 shell。用户 shell 配置再次执行 `hokan init <shell>` 时，因 `HOKAN_ACTIVE` 已存在，只安装内层 hook，不递归 `exec hokan`。
5. 收到 control prompt event、对应 PTY render marker 并建立 anchor 后，进入 `Editing`；单独的 control event 不能证明 prompt 已经呈现在 terminal，在此之前所有 I/O 透明转发。
6. 安装终端恢复守卫和 signal handler 后才启用 overlay。

推荐安装入口：

```sh
# zsh / bash 的配置文件中
eval "$(hokan init zsh)"

# fish
hokan init fish | source
```

`hokan init` 输出的脚本必须幂等、可读、带版本号；`hokan doctor` 检查脚本协议版本。自动修改 rc 文件只由显式的 `hokan install` 执行，并在修改前展示目标路径和创建备份；`hokan setup` 仅作为兼容别名。

### 3.2 数据面和控制面

- **数据面**：真实终端 stdin/stdout 与 child PTY 之间的原始字节。命令运行期间不得经过命令 parser。
- **控制面**：shell hook 发送结构化事件，至少包括 `PromptBoundary`、`BufferSnapshot`、`CommandStart`、`CommandEnd`、`CwdChanged`。
- **呈现边界**：adapter 在能力允许时把带 session token 和 boundary id 的零宽 marker 写入 child PTY。`OutputActor` 在转发前只消费严格匹配的 marker，用它建立 control event 与 PTY bytes 的全序；其他 child bytes 保持原样。
- **回填面**：Hokan 将待回填文本放入 session-scoped 队列，然后向 shell 发送保留键序列；shell widget 从 `hokan ipc take <session>` 读取文本，并通过 `BUFFER/CURSOR`、`READLINE_LINE/READLINE_POINT` 或 `commandline` 原语设置真实缓冲区。

回填不依赖 `Ctrl-U`。保留键序列由适配器选择并可配置；安装时检测明显冲突。IPC token 至少 128 bit、只通过继承环境传递，socket/临时目录权限为 `0700`。

### 3.3 shell 能力分级

| 能力 | zsh | bash | fish |
| --- | --- | --- | --- |
| 命令边界/CWD/退出码 | hook | `PROMPT_COMMAND` + hook | event hook |
| 真实缓冲区快照 | ZLE `line-pre-redraw`，强同步 | v1 以按键镜像为主 | v1 以按键镜像为主 |
| PTY render boundary | prompt marker；post-redraw 能力由 P0 验证 | prompt marker/model convergence | prompt marker/model convergence |
| 精确设置缓冲区 | ZLE widget | `bind -x` widget | `bind` + `commandline` |
| 自定义 vi/plugin 行为 | 强同步下 best effort | 明确降级 | 明确降级 |

PTY 按键镜像只处理文档化的标准编辑键、UTF-8、escape sequence 和 bracketed paste。收到未知编辑序列时，将事件透传并把本地镜像标记为 `Uncertain`；在下一次强同步或命令边界前暂停自动列表，避免基于错误缓冲区给出候选。

### 3.4 前台命令与全屏 TUI

以下任一条件成立即进入 `Passthrough`：

- shell 发送 `CommandStart`；
- PTY foreground process group 不再是 shell；
- child output parser 看到 DEC alternate-screen enable；
- shell buffer 状态不可确定且提示符边界已丢失。

进入前先清除 overlay，之后 raw bytes 双向透传。收到 `CommandEnd`、前台进程组回到 shell 且 alternate screen 关闭后，等待新的 prompt boundary 再恢复补全。

## 4. 事件驱动核心

### 4.1 事件流

```text
TTY stdin --> TerminalReplyRouter --keys--> InputDecoder --+
                  | replies                                |
                  +----------------------------------------+--> AppEvent --> Reducer --> Effect queue
Shell control --> ProtocolDecoder -------------------------+                              |
Signals -------> SignalBridge -----------------------------+                              +--> OutputHandle --> OutputActor --> stdout
Provider batches ------------------------------------------+                              +--> PTY writer
                                                                                         +--> Provider/Store
PTY bytes ----------------------------------------------------------------------------------------> OutputActor
```

`Reducer` 在单线程/单 task 上持有 `AppState`，处理纯事件并产生 effect。阻塞 I/O 在专用线程中执行，异步 provider 在 Tokio runtime 上执行。共享数据尽量使用不可变 `Arc<CompletionContext>`；不允许 provider 直接操作 surface/compositor、`OutputActor` 或 PTY。

建议的事件类别：

- `TerminalInput(KeyEvent | PasteEvent | RawBytes)`
- `PtyOutput(ChildOutputBatch { read_cycle, bytes, drain_state })`
- `RenderBoundary(PromptRendered | PostRedisplay | RedisplayConverged)`
- `ShellEvent(PromptBoundary | BufferSnapshot | CommandStart | CommandEnd | CwdChanged)`
- `ProviderBatch { query_id, source, candidates, final_batch }`
- `AiFinished { query_id, result }`
- `Signal(Resize | Suspend | Continue | Terminate)`
- `ConfigReloaded`、`Tick`、`ChildExited`

### 4.2 状态机

```text
Booting
   | prompt ready
   v
Editing <-----------------------------+
   | candidates                       | dismiss / apply
   v                                  |
MenuOpen ---- select AI ----> AiLoading ----> AiResults
   | Enter (执行输入或选中项)   | cancel/error        |
   +--------------+-------------------+---------------------+
                  v
              Executing ---- command end + prompt ----> Editing
                  |
                  +---- foreground TUI ----> Passthrough

Any state -- suspend/crash/exit --> Restoring --> Exited
```

关键子状态：

- `BufferSync = Exact | Mirrored | Uncertain`
- `Overlay = Hidden | Visible { anchor, surface_key, rows, selected }`
- `View = Unified | HistoryOnly | AiLoading | AiResults`
- `Execution = Prompt | Starting | Foreground { pgid }`
- `Query = { id, buffer_revision, context, cancellation_token }`
- `RenderReadiness = AwaitingPromptMarker | AwaitingRedisplay | Ready { screen_revision } | Unknown`

只有 `Execution::Prompt` 且 `BufferSync != Uncertain` 时允许启动补全。

## 5. 输入解析与缓冲区模型

### 5.1 两层解析

1. **终端输入解码**：把任意分片字节组合成 UTF-8 grapheme、控制键、CSI/SS3 键、Alt chord、mouse 和 bracketed paste。未知序列保持原始字节并透传。
2. **宽容 shell lexer**：只为补全定位当前 command segment、token、quote 状态和 replacement span；不做变量展开、不运行 command substitution，也不声称验证整条 shell 语法。

宽容 lexer 必须接受尚未闭合的引号和尾随反斜杠。管道、`;`、`&&`、`||` 后只补全当前 segment，例如：

```text
cat data.txt | rg fo
               ^^^^^ 只允许编辑这一段的当前 token
```

### 5.2 光标与 Unicode

- 文本索引内部使用 UTF-8 byte offset，编辑边界必须落在 grapheme boundary。
- 光标显示位置使用 terminal cell width，不使用字符数。
- shell hook 传来的 cursor offset 在 adapter 层转换并校验。
- 候选携带显式 `TextEdit { start, end, replacement }`，不得以“清空整行再重写”作为通用行为。

## 6. 候选流水线

### 6.1 查询生命周期

1. 缓冲区或 CWD 变化，reducer 增加 `buffer_revision`。
2. parser 生成不可变 `CompletionContext`，分配单调递增 `query_id`。
3. 取消前一 query 的 provider token。
4. control event 将 render readiness 置为 `AwaitingRedisplay`；provider 可立即运行，不等待 shell 绘制。
5. 立即 provider 在同一计算预算内返回；异步本地 provider 通过批次增量返回。
6. 每批候选经过校验、去重、排序；只有 PTY marker/model convergence 把同一 buffer revision 置为 `Ready` 后才生成 frame request。
7. provider 完成或超时后标记 query settled。输入改变后到达的旧批次直接丢弃。

Provider 预算建议：

| 类型 | 首批预算 | 硬超时 | 例子 |
| --- | ---: | ---: | --- |
| 内存 | 8 ms | 20 ms | specs、history index、PATH cache |
| 本地文件 | 30 ms | 100 ms | 当前目录、`package.json` |
| 有界系统探测 | 50 ms | 250 ms | 进程列表、接口列表 |
| AI | 用户触发 | 默认 8 s | OpenAI-compatible endpoint |

### 6.2 Provider 选择

Provider 不应全部无条件运行。`CompletionContext` 先生成意图和槽位：

- 命令 token：alias、内置 spec、`$PATH` command、history。
- 已知命令参数：spec recipe、当前 slot 对应的 file/dir/process/interface provider、history。
- `pnpm run`：project scripts、history。
- 未知命令 + 参数位置：保守 filesystem、history。
- 高置信自然语言：相关本地 history（若有）和一个 AI action；不调用 AI client。

### 6.3 去重与排序

去重键由 `replacement span + shell-normalized replacement` 组成。相同编辑结果合并来源和最高可信描述，但保留更严格的风险级别。

排序使用整数分数，避免浮点和不稳定结果：

```text
score = match_quality
      + source_trust
      + spec_priority
      + cwd_affinity
      + recency_frequency
      + sequence_affinity
      - risk_penalty
      - incompleteness_penalty
```

最终以 `score desc, source order, stable id` 排序。各 signal 先归一化到固定范围；golden fixture 锁定排序行为。

## 7. 命令规格架构

内置规格是随二进制打包的声明式 TOML 数据。规格只描述静态内容和对白名单动态 provider 的引用，不能包含 shell code。

加载顺序：

1. 内置基础规格；
2. OS variant；
3. 用户配置目录中的新增/覆盖规格；
4. 运行时 capability probe 过滤不可用项。

规格解析后编译为不可变 command tree，并建立 command/alias 索引。编译阶段验证槽位、template、风险和 action：

- `default = "run_current"` 表示默认候选是裸命令回填项，只允许用于无未解槽位、风险不高于 `low` 的命令；它与其他候选一样只回填，不直接执行。
- 包含破坏性 flag 的 recipe 不能标记为直接运行。
- template 中每个 `${slot}` 必须有且仅有一个 slot 定义。
- 用户规格覆盖内置项时记录 provenance，便于 `hokan spec show` 诊断。

## 8. History 与本地状态

为保持全 Rust、无 daemon 且支持多个并行终端，v1 使用带锁的 append-only event store，而不是要求单进程独占的嵌入式数据库：

```text
$XDG_STATE_HOME/hokan/
  history.snapshot       # 版本化压缩快照
  history.events         # 长度 + checksum + payload 的追加记录
  history.lock           # 短时 advisory lock
  imports.toml           # 各 shell history 的导入 checkpoint
  logs/                  # opt-in debug logs
```

- 每次命令结束只在持有短时文件锁时追加一条事件，随后立即释放。
- 每个进程在内存维护去重记录、倒排/模糊索引、frecency 和 transition 统计。
- 在 prompt boundary 检查 event file offset，增量吸收其他终端的新事件。
- 达到大小阈值后，任一会话可竞争 compaction lock；胜者写临时快照、`fsync` 后原子替换，并保留可恢复 checkpoint。
- 尾部 checksum 不完整表示崩溃时 torn write；读取时忽略，修复只在获得独占锁后进行。
- schema version 和 codec version 独立，迁移失败时保留原文件并降级为只读导入。

Credentials 永不进入 history store。默认配置文件只记录 `api_key_env`。

## 9. 终端渲染

Hokan 不使用 alternate screen。Ratatui 只作为离屏 UI 库：widgets 渲染到 current `Buffer`，再与 previous `Buffer` 做 cell diff；它不持有真实 stdout，也不长期使用 `Terminal<Viewport::Inline>`。child shell 会在 Ratatui render pass 外改写屏幕，只有自建 compositor 才知道 previous buffer 何时仍可信。

渲染链路为：

```text
ViewModel --> FrameScheduler -------------------+
child PTY --> ChildOutputBatch -----------------+--> OutputActor --> stdout
terminal replies --> TerminalReplyRouter -------+      | owns
                                                       +-- RenderBoundaryDecoder
                                                       +-- SafeBoundaryScanner / TerminalModel
                                                       +-- Ratatui Buffer / OverlayCompositor
```

关键不变量：

1. `OutputActor` 是唯一真实 stdout writer；child bytes、overlay、探测和恢复序列都由它排序。
2. `TerminalModel` 首选基于 `vt100` 观察 child output，跟踪 absolute cursor、SGR、scroll、wrap、cursor visibility 和 alternate screen；未知 cursor-affecting 序列使锚点失效。
3. `RenderBoundaryDecoder` 只消费严格匹配 session/boundary id 的 adapter marker；其余 child bytes 原样进入 `SafeBoundaryScanner`/`TerminalModel`，防止在半个 UTF-8、CSI、OSC、DCS 或其他 control string 中插入 Hokan bytes。
4. anchor 携带 `screen_revision`、`screen_epoch` 和 `Exact | Derived | Unknown` 置信度。任意影响 terminal model 的 child output 推进 revision；resize、scroll、prompt boundary、未知序列和 alternate-screen 往返推进 epoch。旧 revision/epoch frame 不得提交。
5. overlay session 使用固定高度的专用 surface；首次打开或受控 layout 切换时只预留一次行。连续 query、候选减少和状态变化以 blank cells 填充，不让 prompt/layout 跳动。
6. surface 从 column 0 开始，宽度不超过 `terminal_cols - 1`，避免最后一列触发 deferred autowrap。
7. 不使用 DECSC/DECRC（`ESC 7/8`）。compositor 恢复 child SGR、absolute cursor 和 cursor visibility，不占用 child 的 save slot。
8. control FD 的 buffer/prompt event 只更新语义状态，不直接解锁绘制。frame 必须绑定同一 `buffer_revision` 的 PTY render boundary/convergence 和 `screen_revision`；等待 timeout 只隐藏 UI，不强制画。

每一帧先完整编码到内存。`OutputActor` 在 `TerminalReplyRouter` 注册 outstanding query 后写出 `CSI ?2026$p`，仅在明确支持且 mode 空闲时提交：

```text
BSU -> hide cursor -> cell diff -> restore child SGR/cursor/visibility -> ESU
```

随后一次 `write_all`、一次 `flush`。写 BSU 前记录 `sync_ownership = MayBeOpen`，完整写出 ESU 后清除；`TerminalGuard` 只在该状态下补发 ESU，不能 reset 启动前或 child 持有的 mode。探测 timeout、mode unknown、tmux 旧版本或终端不支持时走 fallback：同样 staged write，但不发送 BSU/ESU；正常导航直接覆盖改变 cells，禁止先清空整个列表。mode 已 set/busy 时延后或隐藏 overlay，直到观察到 reset/重新探测为空闲。

CPR（`CSI 6n`）只在对应 prompt marker 已从 PTY 到达后，或失步恢复时异步查询，不在每个按键上查询，避免 SSH 往返进入输入延迟。无法建立可靠 anchor、redisplay 尚未收敛、cursor 处于 wrap-pending、或当前控制序列未结束时主动隐藏列表。

完整协议、IRIS 对照和测试门槛见 [终端渲染专项研究](./06-terminal-rendering-research.md)。

## 10. AI 边界

AI 是一个由候选动作触发的独立子流程：

```text
NL detector -> AI action row -> explicit activate -> context builder
           -> HTTP client -> strict JSON parser -> local validator/risk classifier
           -> AI result candidates -> insert only
```

HTTP client 使用 rustls，支持自定义 base URL，但不自动跟随跨 origin 的 Authorization redirect。请求有 connect/read/total timeout、body 上限和 cancellation token。Provider error 经过脱敏后转换为 UI status，不向 reducer 暴露 secret。

本地风险分类不是安全沙箱，只用于降低误操作概率。至少检测：

- `rm`/`find -delete`/磁盘写入/格式化；
- `sudo`、权限和所有权修改；
- `kill -9`、宽泛进程匹配；
- 下载后直接 pipe 到 shell；
- 覆盖重定向、递归操作、通配符作用域；
- 多命令连接和 command substitution。

任一高风险或解析不确定结果默认只能回填并显示警告；经显式选中执行时，High/Unknown 必须先通过二次确认。

## 11. 配置和路径

采用 XDG 语义；macOS 也保持一致，便于 SSH/跨机器文档统一：

```text
$XDG_CONFIG_HOME/hokan/config.toml       # fallback ~/.config/hokan
$XDG_CONFIG_HOME/hokan/specs/*.toml      # 用户规格
$XDG_CONFIG_HOME/hokan/credentials.toml  # 可选，0600
$XDG_STATE_HOME/hokan/                   # fallback ~/.local/state/hokan
$XDG_CACHE_HOME/hokan/                   # PATH/project/capability cache
$XDG_RUNTIME_DIR/hokan/<session>/        # socket；无 XDG_RUNTIME_DIR 时用 0700 临时目录
```

配置由一个 `Arc<ConfigSnapshot>` 原子替换。热加载失败保留上一个有效快照，并生成非阻塞诊断。

## 12. 依赖选择原则

具体版本在 bootstrap 时锁定并审计，建议能力如下：

| 能力 | 首选 | 原因 |
| --- | --- | --- |
| CLI | `clap` | 成熟的 derive API 和 shell-independent CLI。 |
| PTY/Unix | `portable-pty` + `rustix`/`nix` | 封装 PTY 与必要的 termios/process group 系统调用。 |
| 离屏 TUI | `ratatui` | 只使用 Buffer/layout/style/widget 和 wide-cell diff，不让其拥有 stdout。 |
| 终端事件/编码 | `crossterm` + 精简输入 decoder | raw mode、resize、常见控制序列和 synchronized update；未知输入字节仍保留。 |
| child VT 观察 | `vt100`，`avt` 作为测试 oracle/备选 | 维护 cursor/SGR/scroll/alternate-screen 语义；裸 `vte` parser 本身不够。 |
| 异步/取消 | `tokio`、`tokio-util` | provider、HTTP 和 cancellation token。 |
| HTTP | `reqwest` + `rustls` | OpenAI 兼容请求且避免 OpenSSL 运行时依赖。 |
| 序列化 | `serde`、`toml`、`serde_json` | 配置、规格和 AI payload。 |
| 模糊匹配 | `nucleo-matcher` | 高性能、Unicode-aware 匹配核心。 |
| Unicode | `unicode-segmentation`、`unicode-width` | grapheme 和 terminal cell 宽度。 |
| 诊断 | `tracing`、`thiserror` | 结构化且可脱敏的内部错误。 |
| Secret | `secrecy`、`zeroize` | 缩短凭据在内存中的明文生命周期。 |

核心路径避免 SQLite/OpenSSL 等额外原生运行时依赖。调用 OS PTY/termios 所需的系统接口不被视为第二种应用实现语言。

## 13. 故障处理

- provider panic/error：隔离为该来源失败，保留其他候选。
- compositor 失去锚点：只清除能可靠定位的专用区域、推进 epoch、隐藏 UI，等待新 prompt boundary/CPR；禁止猜测坐标。
- shell control channel 断开：进入透明 passthrough，提示一次 `doctor` 信息，不终止 shell。
- child shell 退出：恢复终端，以 child exit status 退出 Hokan。
- storage 锁竞争：命令事件先进入进程内队列，在后续 prompt 重试；不得阻塞提交命令。
- 配置/规格损坏：保留 last-known-good，输出路径和错误位置。
- panic：panic hook 只写最小脱敏日志；恢复动作必须在 panic hook 之外也由 guard 执行。

## 14. 关键架构决策及替代方案

### ADR-001：PTY wrapper 而非纯 shell completion

PTY wrapper 才能在 SSH、tmux 和多个 shell 中提供一致内联 UI。代价是必须正确处理作业控制、raw mode 和终端输出，因此 P0 技术验证是发布前置条件。

### ADR-002：shell hook + 镜像降级，而非只镜像按键

只镜像按键无法可靠知道 ZLE/Readline 的真实状态。强同步优先；不具备逐键 hook 的 shell 明确降级并在不确定时关闭候选。

### ADR-003：声明式规格，而非实时解析 `--help`

人工规格可以表达“常用配方、是否完整、槽位和风险”，而 `--help` 文本不稳定且缺乏这些语义。后续可用离线生成器辅助维护，但生成结果必须审核后打包。

### ADR-004：AI 显式动作，而非逐键自动请求

显式动作降低延迟、费用、隐私和候选跳动；本地补全始终优先可用。

### ADR-005：追加事件存储，而非单进程数据库

用户可能同时打开多个终端，且项目不引入 daemon。短锁 append + snapshot 同时满足多进程、崩溃恢复和全 Rust 分发，代价是需要自建有限但可测试的 compaction 协议。

### ADR-006：Ratatui 离屏 Buffer + 自建 compositor

完整 Ratatui `Terminal` 适合应用独占屏幕，但 Hokan 必须与 child shell 共享主屏。离屏 Buffer 保留成熟 layout、style、widget 和 Unicode diff；自建 compositor 则负责 anchor epoch、child output 排序、synchronized output 和恢复协议。代价是需要自行维护小型 frame encoder/scheduler，但其职责远小于自研整个 TUI 或 VT emulator。
