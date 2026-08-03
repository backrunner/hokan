# Hokan 实施与验证计划

## 1. 实施原则

- 每个里程碑交付一条可运行的纵向链路，不先堆一批彼此未接通的模块。
- PTY、shell 同步、终端恢复是最高风险，必须先验证。
- 本地 provider 先于 AI；AI 不能掩盖本地补全质量不足。
- 命令规格按“准确、可解释、可测试”验收，不按文件数量验收。
- 所有阶段都保持 shell 可用：功能失败可以降级，终端损坏不可接受。

以下工期只用于评估相对规模：单名熟悉 Rust/Unix TTY 的工程师完成可公开 beta，预计约 14 至 20 工程周；真实终端兼容性反馈可能扩大该范围。多人并行前必须先稳定 P0 的协议和核心类型。

## 2. 里程碑总览

| 阶段 | 目标 | 主要产物 | 退出门槛 |
| --- | --- | --- | --- |
| P0 | PTY、shell 与无闪烁渲染可行性验证 | wrapper、恢复守卫、状态协议、原子/差分 overlay | 三种 shell 基本链路和 terminal protocol/分片/crash/resize/TUI 门槛通过 |
| P1 | History 纵向 MVP | history 导入、索引、统一列表、精确回填 | 100k history 性能和多会话写入通过 |
| P2 | 命令规格与文件 | schema、7 个必选命令、槽位、路径转义 | macOS/Linux golden + shell quoting property tests 通过 |
| P3 | 项目脚本 | package discovery、`pnpm run` 等 | 项目 fixture 和 cache invalidation 通过 |
| P4 | 显式 AI | detector、配置、HTTP、结果校验、风险提示 | 无隐式请求，恶意/错误响应测试通过 |
| P5 | Beta 硬化 | doctor、安装、兼容矩阵、性能和发布 | 发布清单全部通过，无 P0/P1 已知缺陷 |

## 3. P0：技术验证与骨架

### 3.1 交付内容

1. 初始化 Rust package、`rust-toolchain.toml`、格式化/lint/test 基线。
2. `hokan --shell <...>` 创建 PTY 并启动 zsh/bash/fish。
3. 双向 byte pump、foreground process group 检测和窗口尺寸同步。
4. `TerminalGuard`：raw mode、光标、signal、panic 和 child exit 恢复。
5. 最小 `AppEvent -> Reducer -> Effect` 循环；`buffer/frame/screen` revision 全链路校验。
6. `OutputActor` 独占 stdout，child output、probe、overlay 和 restore 经过同一写入序列。
7. `RenderBoundaryDecoder`、`SafeBoundaryScanner` 与基于 `vt100` 的 `TerminalModel`，覆盖 marker、UTF-8/CSI/OSC/DCS、cursor/SGR/scroll/alternate screen。
8. `RenderReadiness` 与 cross-FD gate：buffer event 可先计算候选，只有 prompt/post-redraw marker 或 model convergence 后才提交 frame。
9. 单 stdin reader + `TerminalReplyRouter`；实现 CPR anchor 和 mode 2026 DECRQM/DECRPM 探测、timeout/fallback。
10. Ratatui 离屏 `Buffer` + 3 行固定 surface + cell diff compositor；不使用 alternate screen、DECSC/DECRC 或 clear-before-paint。
11. BSU/ESU 原子路径和无 synchronized output 的非破坏性 fallback；每帧 staged write、latest-only scheduler。
12. `hokan init zsh|bash|fish` 的协议 v1 草案，含 boundary id/marker 且阻止递归启动。
13. prompt/command/CWD 事件；zsh 真实 buffer snapshot。
14. shell-native 回填 PoC：各 shell 将固定字符串精确设置到真实 buffer。
15. 可重复的 PTY harness，记录输入 bytes、随机分片、输出 token、虚拟屏幕中间态和最终 termios。

### 3.2 必测场景

- 普通命令、管道、后台任务、`Ctrl-C`、`Ctrl-Z`/`fg`。
- `vim`/`less`/`top` 等 alternate-screen 或交互程序。
- 1 MiB bracketed paste、Unicode/CJK/emoji/组合字符、快速连续按键和长按导航。
- child stream 在每个 byte boundary 与随机位置分片，包含未结束 UTF-8、CSI、OSC、DCS、SOS/PM/APC。
- control event 在 PTY redisplay 前/中/后到达；marker 分片、缺失、重复、迟到、伪造和 id 倒退；三种 shell 的 prompt width 不受 marker 影响。
- DECRQM 返回 `0..4`、reply 分片/迟到/畸形/timeout；CPR 与真实用户按键交错，不能误吞输入。
- mode 2026 原子路径；unsupported/timeout fallback 的每个随机 chunk 中间态；startup/child-owned busy 的延后、reset 恢复和禁止 ESU。
- terminal resize storm、tmux pane resize、detach/attach、SSH 延迟/分片/断开。
- tmux 3.6 明确走 fallback；tmux 3.7+ 按 runtime probe 选择路径。
- async job notification、prompt redraw、scroll bottom、未知 control sequence 后的隐藏与重建。
- parent panic、child exit、`SIGTERM`、`SIGHUP`。
- zsh emacs mode；bash/fish 默认模式；未知自定义 key sequence 的降级。
- 相同 transcript 同时交给 `vt100` 与 `avt` adapter，比较 cursor、SGR、主/副屏、scroll 和 overlay rect。

### 3.3 Go/No-Go 条件

只有同时满足以下条件才进入 P1：

- 10,000 次脚本化输入没有丢字节或重排。
- 编译边界/静态检查证明除 `terminal::output/guard` 外没有可写 stdout/TTY fd；高频 child output 中没有 overlay bytes 插入未结束的 control sequence。
- 合法 shell marker 不进入最终 stdout；任意近似/错误 token/错误 phase 的 OSC/DCS byte-for-byte 透传，不能被误吞。
- mode 2026 路径只在 ESU 呈现，录制和虚拟终端采样中没有半帧、空白帧或 cursor jump。
- fallback 在所有固定与随机分片中没有整块空 surface、prompt 覆盖或 clear-before-paint；输出不含 `ED 2/3`、DECSC/DECRC、alternate-screen sequence 和最后一列 write。
- 所有测试退出路径都配对或恢复 Hokan-owned BSU，并恢复 canonical、echo、SGR 和可见光标；不得结束启动前/child-owned synchronized update。
- 三种 shell 均能精确回填包含空格、单引号和 CJK 的单行命令。
- `vim`/`less`/tmux 往返后能重新建立 prompt anchor。
- buffer、safe boundary 或 anchor 不确定时 UI 会隐藏，而不是继续给出错误候选或猜坐标。
- 旧 `buffer_revision`/`frame_revision`/`screen_revision`/`screen_epoch` 的 frame 在 resize、child output 和快速输入竞态下全部被拒绝。
- 穷举 control event、PTY redisplay、marker、provider batch 和 frame request 交错，新 frame 在 `RenderReadiness::Ready` 前提交次数为零；deadline 不得解锁绘制。
- 12 行、100 列 surface 的本地 input-to-frame p95 `<= 16.7 ms`、navigation p99 `<= 33 ms`、compose p95 `<= 2 ms`、diff+encode p95 `<= 1 ms`；pending frame `<= 1`，idle redraw 为 0。

若 Bash/Fish 的逐键镜像达不到门槛，P0 的可接受降级是：这两个 shell 只在明确唤起列表时查询真实 buffer，或缩小首版能力；不可接受的做法是宣称完整兼容但保留已知失步。

## 4. P1：History MVP

### 4.1 工作项

- 实现 Bash/Zsh/Fish history parser 及 fixture。
- 实现 append-only event format、短锁 append、snapshot、tail recovery 和 compaction。
- 构建 exact/prefix/substring/fuzzy 索引和 deterministic ranking。
- 在 shell `CommandEnd` 时记录 session command、CWD、exit code。
- 实现 Unified 与 HistoryOnly、上下导航、分页和稳定选中。
- 实现 history `Insert` 回填，不允许选择时执行。
- 实现 `hokan history import|stats|prune|clear`。
- 加入隐私排除、日志脱敏和最大命令长度。

### 4.2 验收

- 100,000 条含重复/Unicode/长命令 fixture 的 p95 查询不超过 30 ms。
- 两个 Hokan 进程并发追加 100,000 个事件，无损坏、死锁或丢失完整记录。
- 模拟任意 byte offset 断电，reader 忽略 torn tail，repair 后保留之前完整记录。
- shell history rotate/truncate/import 不造成指数重复。
- 输入变化时选中项与迟到 batch 行为符合交互文档。

## 5. P2：命令规格、排序与文件

### 5.1 规格基础设施

- 完成 schema model、TOML loader、compiler、validator、provenance 和用户 override。
- 建立 `hokan spec list|show|validate`。
- 实现 `$PATH` capability cache 和平台 guard。
- 建立 recipe/slot 到 `TextEdit` 的编译流程。
- 实现集中 dedupe/ranking 和激活 reducer（回填 effect；执行只来自无选中的 Enter 透传或显式选中后的提交，High/Unknown 经二次确认）。

### 5.2 首批规格

必须完成并在 Linux/macOS 校验：

1. `ls`
2. `df`
3. `tar`
4. `lsof`
5. `ifconfig`，含 Linux `ip` 替代
6. `ps`
7. `kill`，含 process provider

建议同阶段补充用于验证通用能力的少量命令：`cd`/`cat`/`bash`/`find`/`grep`/`rg`/`git`。这些不是用数量扩 scope，而是分别覆盖 dir、file、script、复杂 recipe 和子命令。

每个命令规格的 Definition of Done：

- 在目标 OS 上确认命令可用性和 flag 语义。
- 定义默认项、至少 3 个高频配方或说明为何不足 3 个。
- 标注 required slot、repeatable、平台和风险。
- direct candidate、prefix、已用 flag、缺参、错误平台均有 golden test。
- 描述是使用场景，不是照抄难懂的 help 片段。
- destructive/slow recipe 有准确风险标记；所有候选（含默认项）都只是回填，不会直接执行。

### 5.3 文件与动态对象

- 实现宽容 lexer、active segment 和 replacement span。
- 实现 POSIX/Fish quote strategy 及 property tests。
- 实现 bounded directory scanner、hidden policy、cache、partial batch。
- 实现 file/dir/executable/new-file slot。
- 实现 process/interface provider 和有界平台探测。

### 5.4 验收

- 原始需求中的 7 个命令全部通过跨平台 golden tests。
- 文件名覆盖空格、单双引号、反斜杠、`$`、CJK、emoji 和前导 `-`。
- 对每个可接受文件名，目标 shell 解析出的 argv 与原始路径一致。
- 5,000 entries 目录首批结果 p95 不超过 80 ms，输入变化取消旧扫描。
- 用户恶意 spec 无法注入启动时 shell code 或绕过风险/action validator。

## 6. P3：项目命令

### 6.1 工作项

- 向上查找最近 manifest 和 workspace boundary。
- 以大小上限解析 `package.json` 的 `scripts`。
- 实现 `pnpm run`、`npm run`、`yarn run`、`bun run` context matcher。
- 实现 manifest cache 和 metadata-based invalidation。
- 展示 script body 摘要、manifest 相对路径和解析诊断。
- 把 project affinity 纳入 ranking，但不覆盖高质量精确 history。

### 6.2 验收

- 普通项目、nested package、monorepo 子目录、symlink CWD、无 scripts、2 MiB 上限和损坏 JSON fixtures 通过。
- discovery 不执行 package manager、不运行 lifecycle、不读取 dependencies。
- 修改 `package.json` 后下一 query 必须看到新 scripts。
- `pnpm run bu` 只替换当前 token。

## 7. P4：AI

### 7.1 工作项

- 实现 deterministic natural-language detector 和 `??` 显式前缀。
- 实现 `hokan config ai`、env secret 和可选 `0600` credentials 文件。
- 实现最小 AI context builder 和 prompt version。
- 实现 OpenAI-compatible chat completions client：rustls、timeout、body limit、redirect policy、取消。
- 实现严格 JSON response parser、command validator 和 risk classifier。
- 实现 AI action/loading/results/error/retry 交互。
- 为日志和 error chain 加 secret/request redaction。

### 7.2 测试服务场景

- 200 正常、多候选、Markdown 包裹 JSON、空 choices、超大 body。
- 401、403、404、429、500、连接失败、慢 header、慢 body、取消。
- 返回 NUL、ANSI、换行、多命令、超长 command、错误 JSON。
- 返回 `rm -rf`、`sudo`、`curl | sh`、覆盖重定向等高风险内容。
- 输入改变后旧响应到达，验证 query id 隔离。
- redirect 到不同 origin，验证 Authorization 不被转发。

### 7.3 验收

- 测试 HTTP server 证明：只键入自然语言不会产生请求，激活 AI action 才有一次请求。
- AI 未配置/失败时 history、spec、files、project 延迟和结果不变。
- AI 候选不存在任何直接执行路径（与其他候选一样只能回填）。
- 默认 payload snapshot 不含 history、环境变量值、完整 CWD、文件内容或 API key。
- debug/error/panic fixture 中 secret 扫描结果为零。

## 8. P5：Beta 硬化与发布

### 8.1 产品化

- `hokan doctor` 文本和 JSON 输出。
- 幂等 `init/setup/uninstall --integration-only`，带备份与协议版本检查。
- 配置 last-known-good、热加载、错误定位和 migration。
- shell/terminal capability fallback 与用户可读诊断。
- man page、安装说明、兼容矩阵、故障恢复说明。
- release artifact：macOS x86_64/aarch64、Linux x86_64/aarch64；生成 checksum/SBOM。

### 8.2 工程门槛

- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- unit/integration/golden/property tests
- `cargo audit` 与 license/source policy
- MSRV build 和当前 stable build
- release profile panic/strip/LTO 决策有基准数据，不凭感觉开启
- 所有 unsafe block 有局部安全注释和测试；无必要 unsafe 为目标
- terminal transcript fuzz 覆盖 reply router、safe boundary、model 和 compositor，且固定发布预算内无 crash/hang/invariant violation
- 固定交互脚本在发布矩阵中录制至少 120 FPS，完成 overlay rect blank-frame/pixel-hash 检测和人工 cursor/prompt 检查
- 兼容矩阵记录 terminal/tmux 精确版本、mode 2026 probe 结果、直接/SSH 路径和已知降级，不只记录 `$TERM`

### 8.3 发布阻断项

以下任一存在都不得发布 beta：

- 可复现的终端 raw mode/光标恢复失败。
- 可复现的输入丢失、重排或候选错误执行。
- 支持矩阵中可复现的空白帧、半帧、cursor jump、prompt 抖动、stale cell 或 overlay 残留。
- 任一路径存在未恢复的 Hokan-owned BSU、错误结束 child/外层 synchronized update、向未结束 child control sequence 插入 bytes，或提交过期 screen revision/epoch frame。
- 普通 overlay frame 使用全屏 clear、DECSC/DECRC、alternate screen、最后一列 write 或 clear-before-paint。
- 除受限 restore 例外外存在第二个 stdout writer，或 overlay pending queue 可超过一帧。
- AI 在未显式选择时请求网络。
- secret 进入日志、history store 或 UI error。
- 候选在未经显式选中（且 High/Unknown 未经二次确认）的情况下被执行，或 AI 候选产生任何 shell 执行路径。
- tmux/SSH/任一承诺 shell 的 P0 核心流程失败。
- 性能基准较基线回退超过阈值且无批准记录。

## 9. 测试体系

### 9.1 分层

| 层 | 目标 | 典型工具/方式 |
| --- | --- | --- |
| Unit | parser、ranking、state transition、spec validation | table tests |
| Property | quote round-trip、edit boundary、store recovery | `proptest` |
| Fuzz | render-boundary/reply router、safe boundary、terminal model、compositor、shell/history/AI/spec parser | `cargo-fuzz` |
| Golden | 候选集合、顺序、Buffer diff、ANSI semantic frame 与禁止序列 | fixture + normalized snapshot |
| Virtual terminal | 任意 byte/chunk 中间态、双实现语义一致性 | `vt100` + `avt` adapter |
| PTY integration | 真实 shell、按键、回填、job control | PTY harness + installed shells |
| Fault injection | torn write、timeout、signal、channel close | deterministic test hooks |
| Benchmark | latency、memory、large directory/history | `criterion` + process benchmark |
| Manual compatibility | 终端、SSH、tmux、输入法和主题 | release checklist + recording |

### 9.2 核心 fixture

- 100k history：重复、alias、CJK、敏感参数、multi-line、rotate。
- 文件树：5k entries、权限错误、symlink、特殊字符、深路径。
- command specs：common/GNU/BSD variant、错误 override、未知 schema。
- package projects：single/nested/workspace/malformed/large manifest。
- terminal transcripts：prompt ANSI、wrapped prompt、SGR、scroll、resize、alternate screen、C0/C1、CSI/OSC/DCS/SOS/PM/APC、截断和超长 control string。
- terminal replies：CPR、DECRQM/DECRPM `0..4`、分片、交错用户输入、迟到、畸形和 timeout。
- render boundaries：control/PTY reorder、prompt/post-redraw marker、错误 token/id/checksum、任意分片和 convergence timeout。
- AI responses：正常、兼容差异、恶意控制字节和各种 HTTP 失败。

### 9.3 CI 分层

- 每次提交：fmt、clippy、stdout ownership 检查、unit/golden、随机 chunk 双 emulator smoke、Linux PTY smoke、无网络 AI mock。
- 主分支：Linux 全集、macOS shell integration、property tests、较长 transcript fuzz、render bench smoke。
- 定期/发布候选：完整 fuzz 预算、真实 terminal compatibility matrix、tmux 3.6/3.7+、SSH、120 FPS 录制、artifact install/uninstall。

CI 不依赖真实 AI endpoint，也不保存真实 key。

## 10. 性能与资源验证

基准必须报告分布而不是单次平均：p50/p95/p99、fixture 大小、CPU、OS 和 commit。终端渲染基准固定包含 12 行、100 列的 ASCII/CJK/emoji surface，并分别报告 mode 2026 与 fallback。至少跟踪：

- startup 到 prompt-ready 的额外时间；
- input event 到 first local frame；
- history exact/prefix/fuzzy；
- directory first batch/full batch；
- spec compile 与 user override；
- render diff bytes/time；
- idle 和 100k history RSS；
- event append/compaction 及多进程锁等待。

P0/P5 的渲染硬目标：

| 指标 | 目标 |
| --- | ---: |
| 本地 input event 到首个可见 frame p95 | `<= 16.7 ms` |
| navigation event 到 frame p99 | `<= 33 ms` |
| surface compose p95 | `<= 2 ms` |
| diff + encode p95 | `<= 1 ms` |
| pending overlay frames | `<= 1` |
| idle redraw | `0 frame/s` |

SSH transport RTT/吞吐单独记录，不混入本地 compose/provider 指标。性能通过不能替代视觉正确性；出现 blank/half frame 时，即使延迟达标仍视为失败。

每个 provider 在 debug metrics 中记录 duration、candidate count、cancelled/timeout，但默认不开启持久化，也不记录 query 文本。

## 11. 风险登记

| 风险 | 概率/影响 | 早期信号 | 缓解与退路 |
| --- | --- | --- | --- |
| shell buffer 失步 | 高/高 | 自定义 key 后候选与输入不同 | 强同步优先；未知序列置 `Uncertain`；缩小 shell/mode 承诺 |
| control/PTY 跨 FD 竞态 | 高/高 | 新列表早于 shell redisplay、随后 cursor/prompt 跳动 | boundary id、in-band marker、model convergence、四重 revision gate；无证明就隐藏 |
| overlay 闪烁或污染输出 | 中/高 | resize/TUI/异步输出后空帧、残留或错行 | OutputActor、固定 surface、VT model、mode 2026/diff fallback；不可靠时隐藏 UI |
| raw mode 未恢复 | 低/极高 | crash/signal 后无回显 | RAII + signal bridge + 故障注入；发布阻断 |
| job control 被 wrapper 破坏 | 中/高 | `Ctrl-Z`/`fg`/交互程序异常 | foreground pgid 测试；P0 先行；执行期纯透传 |
| shell hook 与用户配置冲突 | 中/高 | PROMPT/ZLE/plugin 异常 | 幂等 hook、版本/能力诊断、手动禁用、集成备份 |
| 命令规格跨 OS 错误 | 高/中 | flag 在 BSD/GNU 行为不同 | variant + 实机 golden；不确定配方不发布 |
| 大 history/目录卡顿 | 中/中 | 按键延迟或 RSS 上升 | 索引、预算、partial batch、取消、基准门槛 |
| 多终端存储竞争/损坏 | 中/高 | lock timeout/torn tail | 短锁 append、checksum、snapshot、并发/断电测试 |
| AI 幻觉或危险命令 | 高/高 | 无效工具/破坏性结果 | 显式调用、strict parser、risk 标签、insert-only |
| 凭据泄漏 | 低/极高 | log/error 包含 header/key | env 默认、0600、redaction、secret scan tests |
| “任何终端”预期过宽 | 高/中 | exotic terminal issue | 明确 ANSI/POSIX contract、doctor、capability fallback |

## 12. 发布阶段

### Developer Preview

- P0 + P1 完成。
- 仅面向愿意提供终端 transcript 的开发者。
- 可以限制 shell/terminal 组合，但必须明确标注。

### Alpha

- P2 + P3 完成，覆盖全部非 AI 原始目标。
- 配置和数据格式允许迁移，但不承诺长期稳定。
- 开始收集匿名信息之前仍需单独产品决策；默认继续无遥测。

### Beta

- P4 + P5 完成。
- CLI、spec schema、history migration 有兼容策略。
- 发布阻断矩阵全部通过。

### Stable

- 至少一个 beta 周期没有 P0/P1 级别缺陷。
- 安装/升级/卸载和数据恢复路径经真实用户验证。
- 支持矩阵、隐私边界和安全语义可以作为长期承诺。

## 13. 开工前可调整但不阻塞的产品项

当前文档使用保守默认值。实现 P0 前可以调整：

- v1 UI 默认语言是中文、英文还是跟随 locale；
- 是否将 Fish 强同步设为 beta 必须项；
- `Shift-Tab` 是否作为默认 toggle；
- credentials 文件后端是否进入 v1，或仅支持环境变量；
- 首批 7 个命令之外，额外规格的优先顺序。

这些选择不会改变 PTY/reducer/provider 的核心架构。以下三项若改变则需要重审架构：是否允许 AI 自动请求、是否允许候选一步执行改写后的命令、是否要求 Windows v1 支持。

## 14. 项目级 Definition of Done

Hokan v1 完成不是“列表能显示”，而是同时满足：

- 原始五项能力从真实按键到真实 shell buffer 全链路可用。
- 用户能从候选的标签和释义理解它来自哪里、是否完整、风险如何。
- 异步结果不会覆盖新输入，选择不会因列表更新改变语义。
- 本地补全在 AI 关闭、断网和 endpoint 失败时完全可用。
- 支持矩阵中的 shell、终端、tmux/SSH、信号和 TUI 流程通过。
- mode 2026 与 fallback 都通过随机分片中间态、双 emulator、真实终端录制和无闪烁发布门槛。
- 数据、secret、日志、配置和用户规格有明确边界与恢复路径。
- 性能、测试和发布门槛被自动化，而不是只存在于文档中。
