# Hokan 产品需求文档

> 文档版本：0.1  
> 产品阶段：Greenfield / v1 规划

## 1. 产品定义

Hokan 在用户输入命令时，以低延迟内联列表提供可解释、可回填的候选。候选来自本机 history、内置命令规格、当前文件系统、项目清单以及用户主动触发的 OpenAI 兼容接口。

产品价值不是替代 shell，而是减少三类成本：回忆过去命令、查阅常用参数、手工输入上下文对象。Hokan 必须保留真实 shell、PTY、作业控制、SSH 和全屏 TUI 的行为。

## 2. 目标与非目标

### 2.1 v1 目标

- 在 macOS/Linux 的常见 ANSI 终端、SSH 和 tmux 中稳定运行。
- 自动展示多条相关 history，并支持快速搜索、选择和回填。
- 为首批常用命令提供经过人工校验的“默认项、推荐配方、释义和动态参数”。
- 根据命令上下文补齐当前目录中的文件、目录和可执行文件，并正确处理空格和引号。
- 对 `pnpm run` 读取最近项目的 `package.json` 并列出 scripts。
- 本地判断自然语言；用户显式选择 AI 项后调用 OpenAI Chat Completions 兼容接口，展示多个命令结果。
- 单一 Rust 二进制运行；无后台 daemon、无账号、无默认遥测。
- shell 或 Hokan 异常退出后，终端必须恢复到可输入、可回显状态。

### 2.2 v1 非目标

- Windows Console、PowerShell、cmd.exe。
- 完整实现 Bash/Zsh/Fish 的全部语法、插件 API 或自定义行编辑器行为。
- 替代 shell 原生 glob、变量展开、管道执行和作业控制。
- 自动执行 AI 生成命令或对 AI 命令正确性作保证。
- 自动抓取任意命令的 `--help` 并立即生成可信规格。
- 云端同步 history、用户画像或命令规格。
- 在补全期间读取并上传项目文件内容；v1 AI 上下文只包含显式列出的元数据。
- 首版覆盖数百个命令。先确保规格质量和扩展机制，再增加数量。

## 3. 目标用户与核心场景

### 3.1 目标用户

- 高频使用终端、但不想记住所有参数组合的开发者和运维人员。
- 经常切换项目、工具和远程服务器，希望一个二进制到处可用的用户。
- 希望 AI 帮助生成命令，但要求在执行前检查结果的用户。

### 3.2 核心流程

#### 流程 A：history 补齐

1. 用户输入命令片段，例如 `docker comp`。
2. 列表在本地候选预算内出现多条匹配 history。
3. 用户用上下键选择，按 `Tab` 或 `Enter` 回填。
4. Hokan 不执行被回填的 history；用户再次按 `Enter` 后才由 shell 执行。

#### 流程 B：常用命令配方

1. 用户输入 `ls`。
2. 第一项为 `ls`，标记“直接运行”，因为它无必填参数且为低风险。
3. 后续项展示 `ls -la`、`ls -lah`、`ls -lt` 等配方及简短释义。
4. 当前输入为 `ls` 且第一项未被切换时，按 `Enter` 原样提交 `ls`；选择其他配方时只先回填。

#### 流程 C：文件补齐

1. 用户输入 `bash `。
2. Hokan 识别当前参数期望脚本或文件。
3. 列表优先展示当前目录的可执行文件、`.sh` 文件，再展示目录。
4. 选择包含空格的文件时，Hokan 按当前 shell 和引号上下文正确转义。

#### 流程 D：项目脚本

1. 用户在项目任意子目录输入 `pnpm run `。
2. Hokan 向上查找最近的 `package.json`，只读取 `scripts` 对象。
3. 列表展示脚本名、完整插入文本和脚本内容摘要。
4. 选择 `build` 后回填 `pnpm run build`，不立即执行。

#### 流程 E：自然语言转命令

1. 用户输入“查找当前目录 7 天内修改过的 rs 文件”。
2. 本地分类器判断其更像自然语言，列表出现“使用 AI 生成命令”项。
3. 用户选择该项后才发起请求；列表进入可取消的加载状态。
4. 返回 1 至 5 个候选，每项包含命令、简短解释和风险标签。
5. 用户选择后只回填命令，检查后再次按 `Enter` 执行。

## 4. 功能需求

### 4.1 TTY、PTY 与 shell 生命周期

- **FR-TTY-001**：`hokan` 必须能够启动配置的交互式子 shell，并在真实终端与该 shell 的 PTY 之间双向代理字节。
- **FR-TTY-002**：v1 必须支持 zsh、bash、fish；未识别 shell 时给出明确诊断，不静默假定兼容。
- **FR-TTY-003**：shell 正在执行前台命令时，Hokan 必须隐藏列表并原样转发输入输出，包括控制字符。
- **FR-TTY-004**：子进程进入 alternate screen 或前台进程组改变时，不得绘制候选层；退出后恢复提示符状态。
- **FR-TTY-005**：必须处理 `SIGWINCH`、`SIGINT`、`SIGTERM`、`SIGHUP`、`SIGTSTP` 和 `SIGCONT`，并以 RAII 守卫恢复 termios、光标和可见性。
- **FR-TTY-006**：正常退出、panic、子 shell 崩溃和可处理信号路径都必须恢复终端；无法恢复时输出一条可直接运行的 `stty sane` 恢复提示。
- **FR-TTY-007**：必须识别 bracketed paste；粘贴期间不逐字符触发 provider，请求在粘贴结束后合并执行。
- **FR-TTY-008**：shell hook 至少同步命令开始、命令结束、退出码和 CWD；能读取真实编辑缓冲区的 shell 应同步文本与光标。
- **FR-TTY-009**：通过 `hokan init <shell>` 输出适配代码；递归启动必须由 session 环境标志阻止。
- **FR-TTY-010**：child output、overlay、terminal probe 和 restore 必须由唯一 stdout writer 排序；不得在未结束的 UTF-8/CSI/OSC/DCS/control string 中插入 Hokan bytes。
- **FR-TTY-011**：control FD 的 prompt/buffer event 不得直接视为 terminal 已完成 redisplay。frame 必须等待同一 revision 的 PTY render marker 或经过验证的 terminal-model convergence；超时只能隐藏，不能强制绘制。

### 4.2 History

- **FR-HIS-001**：首次进入 history 功能时，按当前 shell 解析现有历史文件；支持 zsh extended history、Bash timestamp 和 Fish YAML-like history 的常见格式。
- **FR-HIS-002**：本次会话提交的命令必须立即可搜索，不依赖 shell 将 history 刷盘。
- **FR-HIS-003**：普通输入视图自动混入相关 history；`Ctrl-R` 切换到只看 history 的专注视图。
- **FR-HIS-004**：空查询时按最近使用排序；有查询时综合前缀、子串、模糊匹配、频率和最近使用时间排序。
- **FR-HIS-005**：默认按规范化后的完整命令去重，但保留次数、最近时间、来源 shell 和可选 CWD 统计。
- **FR-HIS-006**：history 候选必须展示来源标签；选择只回填，不自动执行。
- **FR-HIS-007**：不得索引包含 NUL、超过配置长度或匹配用户隐私排除规则的条目。
- **FR-HIS-008**：提供 `hokan history import|stats|prune|clear`；`clear` 必须二次确认且只清除 Hokan 自身 history store，不修改 shell 原 history。

### 4.3 常用命令规格与参数配方

- **FR-SPEC-001**：命令规格必须表达名称、别名、平台、描述、是否可无参运行、风险等级、配方、槽位和动态 provider。
- **FR-SPEC-002**：输入精确命令名时，若命令存在、无需必填参数且规格风险为 `read_only` 或 `low`，第一项必须是与当前缓冲区相同的“直接运行”项。
- **FR-SPEC-003**：若命令需要参数，第一项必须是推荐的下一步或配方，不得伪装成可运行命令；尚未补齐的槽位必须可见。
- **FR-SPEC-004**：每个配方必须有不超过一行的中文或本地化释义；仅列裸 flag 不算合格配方。
- **FR-SPEC-005**：首批必须覆盖 `ls`、`df`、`tar`、`lsof`、`ifconfig`、`ps`、`kill`，并区分 GNU/Linux 与 BSD/macOS 差异。
- **FR-SPEC-006**：仅展示本机可用命令或有明确替代说明的命令；可执行文件探测结果必须缓存，不能每次按键扫描 `$PATH`。
- **FR-SPEC-007**：危险配方（例如 `kill -9`、递归删除、覆盖写入）必须有风险标签；规格默认项（`default = "run_current"`）仅限 ReadOnly/Low 的无参命令，选中执行 High/Unknown 候选必须二次确认。
- **FR-SPEC-008**：用户可在配置目录增加或覆盖规格；启动或 `hokan spec validate` 时必须验证 schema、重复 ID、槽位和风险约束。
- **FR-SPEC-009**：规格加载失败不得阻止 shell 启动；禁用错误规格并记录可定位诊断。

首批行为基线：

| 输入 | 默认项 | 其他示例 | 说明 |
| --- | --- | --- | --- |
| `ls` | `ls`，直接运行 | `ls -la`、`ls -lah`、`ls -lt` | 仅当本机存在 `ls`。 |
| `df` | `df`，直接运行 | `df -h`、`df -h <path>` | GNU/BSD 共同配方优先。 |
| `tar` | `tar -czf `，继续补参 | `tar -xzf `、`tar -tf ` | 必须继续选择 archive/路径。 |
| `lsof` | `lsof`，直接运行 | `lsof -i`、`lsof -i :<port>`、`lsof +D <dir>` | `+D` 标记可能较慢。 |
| `ifconfig` | `ifconfig`，直接运行 | `ifconfig <interface>` | Linux 若只有 `ip`，给出 `ip addr` 替代项。 |
| `ps` | `ps`，直接运行 | Linux `ps aux`；macOS `ps aux`、`ps -ef` | 由平台 guard 决定。 |
| `kill` | “选择进程”，继续补参 | `kill <pid>`、`kill -TERM <pid>`、`kill -9 <pid>` | 不任意默认选中一个 PID；`-9` 为高风险。 |

### 4.4 文件与上下文对象

- **FR-FS-001**：当命令规格的当前槽位需要文件、目录、可执行文件或 socket/path 时，扫描当前 CWD 产生候选。
- **FR-FS-002**：未知命令存在尾随空格时，可使用保守的通用文件 provider；命令名本身尚未确定时不得混入大量文件。
- **FR-FS-003**：`bash `、`zsh `、`sh ` 优先展示可执行文件和 shell 脚本；目录仍可用于继续导航。
- **FR-FS-004**：目录候选以 `/` 结尾并继续补全，不关闭列表。
- **FR-FS-005**：默认不显示隐藏文件；用户已输入 `.` 或配置允许时显示。
- **FR-FS-006**：插入文本必须针对 shell 和当前 quote 状态转义；展示文本保持人类可读，不能把转义后的字符串当展示名。
- **FR-FS-007**：目录读取必须有数量和时间预算；超大目录先返回已排序的部分结果，后台结果只在 query id 仍有效时合并。
- **FR-FS-008**：不得跟随目录递归扫描，除非某个明确 provider 请求且配置了上限。

### 4.5 项目命令

- **FR-PROJ-001**：输入 `pnpm run ` 时，从 CWD 向上查找到工作区边界内最近的 `package.json`，解析 `scripts` 对象。
- **FR-PROJ-002**：输入部分脚本名时支持前缀和模糊筛选，插入完整 `pnpm run <script>`。
- **FR-PROJ-003**：描述展示 script 对应命令的安全截断摘要，但不得执行 script 或 package manager 来发现候选。
- **FR-PROJ-004**：同一 provider 支持 `npm run`、`yarn run`、`bun run`；是否显示取决于命令是否安装和项目 lockfile 信号。
- **FR-PROJ-005**：`package.json` 修改后缓存必须失效；JSON 错误显示一条非阻塞诊断项。
- **FR-PROJ-006**：pnpm workspace 的跨 package 聚合属于 v1.1；v1 只承诺最近 package 的 scripts。

### 4.6 自然语言与 AI

- **FR-AI-001**：自然语言检测必须是本地、确定性和低成本的，不得为了分类请求网络。
- **FR-AI-002**：分类应综合已知命令、shell 运算符、路径/flag 特征、词数、中文比例和疑问/祈使表达；低置信度时不显示 AI 项。
- **FR-AI-003**：仅当 AI 已启用且凭据配置有效时显示可调用项；未配置时可显示一条“配置 AI”动作，但不得弹出或联网。
- **FR-AI-004**：用户选择 AI 动作后，调用 OpenAI 兼容的 `/v1/chat/completions`，支持自定义 base URL、model、API key 环境变量和 timeout。
- **FR-AI-005**：请求最多携带：自然语言文本、OS/架构、shell、CWD basename、已检测项目类型和用户明确开启的少量上下文。默认不发送 history、环境变量值、文件内容和完整目录清单。
- **FR-AI-006**：要求模型返回结构化 JSON，包含 1 至 5 个 `{command, explanation}`；解析失败可执行一次严格提取，不得直接把 Markdown 代码块当命令执行。
- **FR-AI-007**：结果必须拒绝 NUL、控制序列、多行文本和超过长度上限的内容，并通过本地 shell lexer 与风险分类器。
- **FR-AI-008**：AI 结果列表必须标记 `AI` 和风险级别；任何结果都只能回填，至少再按一次 `Enter` 才能执行。
- **FR-AI-009**：输入变化、`Esc`、命令提交和视图关闭必须取消在途请求；迟到响应不得覆盖新查询。
- **FR-AI-010**：401、429、超时、网络失败和无效响应必须在列表内给出可操作但不泄露 secret 的错误，且不影响本地补全。
- **FR-AI-011**：日志、panic report 和错误字符串不得包含 API key、Authorization header 或完整 AI 请求体。

### 4.7 候选列表与键盘交互

- **FR-UI-001**：候选列表绘制在当前提示符下方，不进入 alternate screen，不永久污染 scrollback。
- **FR-UI-002**：每行至少包含候选主体、来源或类型、简短释义；风险和“仍需参数”状态必须可见。
- **FR-UI-003**：默认键位为：上下键导航、`Tab` 回填候选、`Enter` 执行（无选中=当前输入，有选中=选中候选）、`Esc` 关闭、`Ctrl-R` history 专注视图、`Shift-Tab` 切换列表显示。
- **FR-UI-004**：列表不默认选中；`Up`/`Down` 显式进入列表且不改变 buffer。无选中时 `Enter` 把用户亲手输入的 buffer 原样提交给 shell；有选中时 `Enter` 执行选中候选的完整命令文本，High/Unknown 风险先经二次确认（`Enter` 确认、`Esc` 取消）。亲手输入的命令永不触发确认；`Tab` 只做候选回填，永不执行。
- **FR-UI-005**：列表关闭时，除 Hokan 明确拥有的全局键位外，按键必须原样交给 shell；键位均可配置或禁用。
- **FR-UI-006**：宽度适配 40 至 240 列；Unicode 宽度按 grapheme/cell 计算；过长文本截断而不换行破坏提示符。
- **FR-UI-007**：颜色不可作为唯一状态信息；命令图标是可选 Nerd Font 皮肤（`ui.nerd_fonts` 默认开启），关闭后退回纯文本标签。
- **FR-UI-008**：窗口 resize、终端滚动和 child output 到达时必须使旧 screen epoch/frame 失效；只有锚点和控制序列边界可靠时才重绘，否则隐藏列表，不能与 shell 输出交叠或猜测坐标。
- **FR-UI-009**：overlay session 使用固定高度的专用 surface；异步候选、选中态和状态文字变化不得反复增删终端行或推动 prompt/layout 跳动。
- **FR-UI-010**：运行时必须通过 DECRQM 探测 synchronized output。明确支持且 mode 空闲时使用 BSU/ESU 提交完整帧；unsupported 或 timeout 时使用不先清空 surface 的 cell-diff fallback；mode busy/externally owned 时延后或隐藏，不能嵌套/结束他人的 transaction。
- **FR-UI-011**：普通 frame 禁止全屏/scrollback clear、alternate screen、DECSC/DECRC 和最后一列 write；每帧必须先完整编码，再由唯一 writer 顺序提交并恢复 child SGR、绝对光标和可见性。
- **FR-UI-012**：pending overlay frame 最多一帧；旧 `buffer_revision`、`frame_revision`、`screen_revision` 或 `screen_epoch` 不得提交。child output 不得因 UI backpressure 丢失或重排。
- **FR-UI-013**：P0 默认不启用独立 ghost-text writer。后续 ghost text 必须作为同一 compositor 管理的 surface，与列表一起 diff 和恢复。

### 4.8 配置、诊断与维护

- **FR-CFG-001**：配置使用 TOML，默认路径遵循 XDG；`hokan config path|show|validate` 可发现和验证配置。
- **FR-CFG-002**：API key 默认通过环境变量引用；若用户选择文件保存，独立 credentials 文件必须以 `0600` 创建并在权限过宽时拒绝使用。
- **FR-CFG-003**：`hokan doctor` 检查 TTY、`TERM`、shell、hook、控制 FD、数据目录权限、AI 配置和已知键位冲突。
- **FR-CFG-004**：日志默认关闭；debug 日志写入状态目录，自动脱敏，并可配置轮转上限。
- **FR-CFG-005**：配置热加载只更新安全的 UI/provider 参数；shell、存储路径等结构性变化提示重启会话。
- **FR-CFG-006**：提供 `hokan spec list|show|validate` 方便检查内置和用户规格。

## 5. 排序与去重需求

候选先满足硬规则，再计算软分数：

1. 丢弃不适用平台、命令不存在、编辑范围无效、风险规则违规和过期 query 的候选。
2. 按最终插入文本和编辑范围规范化去重；同一结果保留信息更丰富、来源更可信者，并合并来源标签。
3. 精确匹配与可继续的当前上下文优先于模糊匹配。
4. 综合 prefix、substring、fuzzy、source trust、CWD、recency、frequency、sequence 和静态配方优先级。
5. AI 动作不参与普通命令竞争；只有自然语言置信度达阈值才进入固定位置。
6. 排序必须稳定，同一输入快照和同一数据状态产生同一顺序。

默认 source trust 从高到低为：精确直接运行/项目动态对象、命令规格、当前目录对象、history、系统 `$PATH` 命令、AI 动作。History 的高相关精确前缀可以超过普通规格配方。

## 6. 非功能需求

### 6.1 性能预算

在发布基准机器和 warm cache 下：

- 进程启动到子 shell 可交互：p95 不超过 100 ms 的额外开销。
- 按键到首批本地候选可绘制：p95 不超过 50 ms。
- 本地可见状态已就绪时，input event 到 frame p95 不超过 16.7 ms；navigation event 到 frame p99 不超过 33 ms。
- 100,000 条 history 的查询：p95 不超过 30 ms。
- 5,000 项单目录的首批文件候选：p95 不超过 80 ms，完整扫描可异步继续。
- 12 行、100 列 surface compose p95 不超过 2 ms，diff + encode p95 不超过 1 ms；空闲时不产生 redraw。
- 空闲常驻内存目标小于 40 MiB；不因 history 总量线性保留所有重复字符串副本。
- AI 不计入本地延迟 SLA，但默认 timeout 不超过 8 秒且必须可取消。

### 6.2 正确性与可靠性

- 1 MiB bracketed paste 不丢字节、不触发请求风暴。
- 文件名包含空格、单双引号、反斜杠、CJK 和 emoji 时能生成可逆插入文本。
- PTY 高吞吐输出时不出现 Hokan UI 字节混入命令输出。
- `BufferSnapshot`、PTY redisplay 和 provider batch 任意交错时，readiness 前不提交新 frame；旧 surface 即使暂时保留也不得再激活旧候选。
- mode 2026 路径的 presented screen 不出现半帧/空白帧；fallback 在任意 byte/chunk 分片中不出现整块空 surface、prompt 覆盖或 clear-before-paint。
- resize、scroll、alternate-screen 往返和未知控制序列后，过期 screen epoch frame 必须被拒绝；无法证明安全时列表必须隐藏。
- Hokan 发出的 BSU/ESU 必须配对；正常/异常恢复只结束 Hokan 自己可能开启但未完成的 synchronized update，不能 reset child/外层持有的 mode，并恢复 SGR、绝对光标和 cursor visibility。
- 任何候选不得跨越解析器给出的可编辑范围改写管道前序命令。
- 崩溃恢复测试必须验证 canonical/echo 模式和光标可见性。

### 6.3 安全与隐私

- 不使用 `sh -c` 执行 provider 查询；外部探测必须以 argv 调用并设置超时。
- 不根据候选内容自动执行程序。
- AI、history、路径和环境数据的边界必须可配置且默认最小化。
- 用户规格不能包含启动时自动执行的 shell 代码；动态 provider 只能引用内置白名单能力。
- 数据文件使用用户私有权限，敏感字段不进入持久化状态存储。

## 7. 兼容性承诺

### 7.1 发布阻断矩阵

| OS | Shell | 最低要求 |
| --- | --- | --- |
| macOS 当前和前一主版本 | zsh | 强同步、完整功能 |
| macOS 当前和前一主版本 | bash 3.2+ | 标准编辑键完整，诊断降级能力 |
| Linux Ubuntu LTS / Fedora 当前版 | bash 4.4+ | 标准编辑键完整 |
| Linux Ubuntu LTS / Fedora 当前版 | zsh 5.8+ | 强同步、完整功能 |
| Linux Ubuntu LTS / Fedora 当前版 | fish 3.6+ | 标准编辑键完整 |

终端至少手工验证：Terminal.app、iTerm2、Ghostty、Kitty、WezTerm、Alacritty，以及 SSH、tmux 3.6 fallback 和 tmux 3.7+ runtime-probe 组合。矩阵记录精确版本和 mode 2026 探测结果；其他 ANSI/VT 终端按 best effort 支持。

### 7.2 降级行为

- 非 TTY stdin/stdout：拒绝进入交互包装，CLI 子命令仍可使用。
- `TERM=dumb` 或缺少必要能力：关闭 overlay，保留原 shell，并输出一次诊断。
- shell hook 未安装：允许手动 wrapper 模式，但只宣称支持标准按键镜像；`doctor` 明确提示。
- history 状态存储损坏：隔离损坏文件，history 学习降级到内存，shell 仍可启动。
- AI 不可用：只移除 AI 动作，本地 provider 不受影响。

## 8. v1 验收清单

- 五类核心流程均有自动化 PTY 集成测试和手工录屏证据。
- 七个指定命令在 macOS/Linux 的规格行为通过 golden tests。
- `pnpm run` 在普通项目、子目录、无 scripts、损坏 JSON 四种情况下行为正确。
- AI 请求只有在显式选中时发生；测试服务验证取消、超时、401、429、畸形 JSON 和多行恶意响应。
- crash/signal/resize/tmux/SSH 路径均不留下 raw terminal。
- mode 2026/fallback 均通过随机分片双虚拟终端测试；发布矩阵的固定交互脚本有 120 FPS 录制和 blank-frame/cursor-jump 检查证据。
- 无 API key、history 或完整 prompt 出现在默认日志和错误报告中。
- 性能预算在固定 fixture 上进入 CI；超过阈值阻止发布构建。

## 9. v1.1 候选范围

- pnpm workspace 跨 package scripts、Cargo aliases、Make/Just/Taskfile target、Git branch/container/process 等更多动态 provider。
- 用户可选的 OS keychain 凭据后端。
- 更完整的 vi-mode 与自定义 shell widget 协作。
- 可本地化的命令释义和更多平台规格。
- 用户显式授权后的项目上下文 AI，以及本地模型 provider。
