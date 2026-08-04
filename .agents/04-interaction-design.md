# Hokan 交互设计

## 1. 体验原则

- **像 shell 的一部分，而不是第二个 shell**：提示符、输入行和执行仍由真实 shell 负责。
- **先本地、后网络**：列表先稳定显示本地结果；AI 只在用户选择动作后出现加载状态。
- **回填与执行分离**：用户可以快速选，但不能因列表变化意外执行另一条命令。
- **解释紧邻命令**：参数组合、来源、待补槽位和风险在同一视线内可见。
- **失败时安静降级**：provider 失败不弹 modal，不阻断键入；终端状态不可靠时隐藏列表。
- **布局稳定**：异步结果到达、标签变化和选中态都不能让提示符或输入行跳动。

## 2. 默认外观

Hokan 在输入行下方使用内联 overlay，不进入全屏：

```text
~/project $ ls▌
  > [CMD]  ls                 直接运行 · 列出目录内容
    [USE]  ls -la             长格式显示全部条目
    [USE]  ls -lah            显示隐藏条目和易读大小
    [HIS]  ls -la src         2 分钟前
           1 / 4
```

视觉约束：

- 列表是一个紧凑圆角框（iris modern 风格）：单边框、选中箭头、Nerd Font 命令图标、来源图标和弱色描述建立层级；描述列跨行对齐，风险标记紧邻选中箭头，选中行主命令加粗。
- 默认最多 8 行（6 行候选 + 上下边框）；分页计数内嵌顶边，键位提示或状态文字内嵌底边；不足空间时自动减少。
- 默认宽度为当前终端宽度和 76 cells 的较小值，最小支持 40 cells；框左边缘跟随光标列，靠右时自动左移防溢出。
- overlay 打开时一次确定专用 surface 的行数；连续键入、新 query 和 provider 增量只更新 surface 内 cells，不反复插入/删除终端行。
- 主命令优先保留；描述在普通列表中单行截断（`…`），不挤压主命令到不可读。
- 命令与来源均使用 Nerd Font 图标（`ui.nerd_fonts` 默认开启；命令图标按首词查表，来源图标按 `SPEC`/`HIS`/`HELP`/`FILE` 等标签映射，fallback `❯`）；关闭后图标列消失，退回 `HIS`、`FILE`、`PROJ`、`AI` 等 ASCII 标签。
- 颜色使用 ANSI 自适应调色板（边框/键位品红、选中与高亮绿、描述暗灰、状态黄），随终端主题变化；无颜色时依靠标签和文字表达，不只靠颜色。
- 所有宽度按 terminal cell 计算；CJK、组合字符和 emoji 不得破坏列对齐。

## 3. 打开、关闭与刷新

### 3.1 自动打开

- 用户在 prompt 中产生有效可补全输入后，首批本地候选非空则打开。
- 空输入默认不自动打开，避免每个新 prompt 都占据屏幕；按 `Ctrl-R` 显示最近 history。
- bracketed paste 期间关闭或冻结列表，paste end 后只查询一次。
- 当前 buffer 为 `Uncertain`、shell 正执行命令或 alternate screen 开启时不打开。

### 3.2 关闭

- `Esc` 只关闭当前 query 的列表，不清空 shell buffer。
- `Shift-Tab` 手动切换列表；关闭状态持续到 buffer 再次发生实质变化。
- 提交命令、`Ctrl-C`、进入前台进程、失去 prompt anchor 时立即清除。
- provider 返回空结果时清除上一帧，不留下空壳。

### 3.3 异步更新稳定性

- 首批结果确定默认选中项。
- 后续批次到达时保留相同 candidate id 和尽量相同的屏幕行。
- 用户一旦使用上下键，本 query 的现有顺序冻结；新结果追加到合适分组末尾，不把选中项顶走。
- buffer 改变会创建新 query，此时才允许完整重排。

### 3.4 Surface 稳定性

- 从列表首次打开到关闭称为一个 overlay session。session 内保持相同 rect 和 page capacity；buffer 变化虽然创建新 query，但不会按候选数量重新调整高度。
- buffer 改变后，已有完整 surface 可在 shell redisplay 收敛前短暂保留以避免闪白，但旧候选立即失去激活资格。新 frame 只有绑定到相同 buffer/screen revision 的 render boundary 后才替换；旧 rect 被 child output 触碰时直接隐藏。
- 首次打开只预留一次缺少的物理行。候选不足时用空 cells 填满 surface；关闭时清除可见内容，但不通过删除/补回行来移动 prompt。
- Unified、HistoryOnly 和 AI loading 使用各自固定 layout。切到需要更高 surface 的 AI results 时最多发生一次受控重建；重建前必须具备可靠 anchor，并在 synchronized transaction 中完成，否则保持原高度或隐藏。
- 选中标记、标签、loading/error 文案和分页数字都有稳定列宽。状态变化只能改变内容，不能推动命令列左右跳动。
- 每次可见更新先完整生成一帧。不存在“先显示空列表、下一次 write 再显示内容”的中间设计状态。

## 4. 键位语义

| 键位 | 列表打开时 | 列表关闭时 |
| --- | --- | --- |
| `Up` / `Down` | 移动选中项 | 原样交给 shell |
| `Tab` | 接受编辑，永不执行 | 原样交给 shell；若刚有候选可配置为打开并接受 |
| `Enter` | 无选中：关闭列表并提交当前输入；有选中：执行选中候选（High/Unknown 先二次确认） | 原样提交给 shell |
| `Esc` | 关闭列表或取消 AI 请求 | 原样交给 shell |
| `Ctrl-R` | Unified/HistoryOnly 切换 | 打开 HistoryOnly |
| `Shift-Tab` | 关闭列表 | 打开列表 |
| `PageUp` / `PageDown` | 候选翻页 | 原样交给 shell |

所有键位可配置或禁用。列表打开时的普通可打印字符、Backspace、左右移动、`Ctrl-A/E/W/U` 等仍交给 shell，同时由强同步或镜像更新 query。未知 escape sequence 原样透传，并使镜像状态进入不确定模式。

## 5. 候选动作

### 5.1 `Enter` 语义

列表不默认选中，`Up`/`Down` 显式进入（不改变 buffer）。无选中时 `Enter` 把用户亲手输入的 buffer 原样提交给 shell：

```text
$ df▌
    [CMD] df          显示文件系统磁盘使用量
    [USE] df -h       使用易读单位
```

有选中时 `Enter` 执行选中候选的完整命令文本：≤ Medium 风险直接执行，High/Unknown 先显示红色 EXEC 确认行（`Enter` 确认、`Esc` 取消）。仍需参数的候选（InsertAndContinue）选中后 `Enter` 退化为回填。亲手输入的命令永不触发确认。

### 5.2 `Insert`

用于 history、完整配方、文件和 AI 结果：

- `Tab`：回填，并根据新上下文继续显示候选。
- `Enter`：回填并关闭列表；用户再次按 `Enter` 才执行。
- 若候选风险为 `Medium/High/Unknown`，回填后在下一次输入前保留一行非阻塞风险提示。

### 5.3 `InsertAndContinue`

用于未完成的 recipe 和目录：

```text
$ tar▌
  > [USE] tar -czf …        创建 gzip 归档 · 还需 archive、paths
    [USE] tar -xzf …        展开 gzip 归档 · 还需 archive
    [USE] tar -tf …         查看归档内容 · 还需 archive
```

接受 `tar -czf …` 时不插入字面量 `…` 或 `${archive}`，只回填安全前缀 `tar -czf `，随后列表切到 archive 槽位。选定 archive 后继续到 paths 槽位。

### 5.4 `RequestAi`

激活动作后保留用户原始文本，列表切换为 AI loading。它不修改 shell buffer、不提交命令。`Esc` 取消并返回本地列表。

## 6. History 交互

### 6.1 Unified 视图

History 与其他来源混排，但带 `HIS` 标签和相对时间：

```text
$ docker comp▌
  > [HIS] docker compose up -d          昨天 · 当前项目使用 8 次
    [HIS] docker compose logs -f api    3 天前
    [CMD] docker compose                Docker Compose 子命令
```

高相关 history 可以排在规格前。重复命令只显示一次；次数/CWD 作为排序信号和短注释，不展示完整敏感路径。

### 6.2 HistoryOnly 视图

```text
$ git▌
    HISTORY
  > git status
    git commit -m "..."
    git log --oneline --decorate
           1 / 27
```

- `Ctrl-R` 打开或在 Unified/HistoryOnly 间切换。
- 空 buffer 展示最近命令；继续键入直接修改真实 shell buffer，并过滤列表。
- `Tab` 或 `Enter` 都只回填；HistoryOnly 关闭后回到 Unified。
- 多行 history 默认显示但不可直接接受，标记“多行”；v1 可通过配置完全隐藏。

## 7. 常用命令交互

### 7.1 无必填参数

输入精确命令名时，首项为直接运行；后续是人工配方：

```text
$ lsof▌
  > [CMD] lsof             直接运行 · 列出打开的文件
    [USE] lsof -i          查看网络连接
    [USE] lsof -i :3000    查看占用 3000 端口的进程
    [USE] lsof +D …        递归检查目录 · 可能较慢
```

输入只是前缀 `lso` 时，`lsof` 是 `Insert` 候选；`Enter` 只执行当前输入的 `lso`（shell 报 command not found），想用 `lsof` 需先 `Tab` 回填。

### 7.2 需要参数

`kill` 不随机选择一个进程作为可执行默认项：

```text
$ kill ▌
  > [PROC] 4821  node server.js          使用 TERM 请求停止
    [PROC] 5170  cargo watch             使用 TERM 请求停止
    [USE]  kill -TERM …                  正常终止 · 还需 PID
    [RISK] kill -9 …                     强制终止 · 高风险
```

选择进程只回填 `kill 4821`（进程候选是 InsertAndContinue，Enter 不执行）。`kill -9` 标为高风险，选中执行必须先经二次确认。

### 7.3 平台差异

不适用于当前系统的 recipe 不显示。若 Linux 没有 `ifconfig` 但有 `ip`：

```text
$ ifconfig▌
  > [ALT] ip addr          本机未安装 ifconfig · 显示网络接口
```

替代项只回填，不直接执行，因为它改变了用户输入。

## 8. 文件交互

```text
$ bash ▌
  > [FILE] deploy.sh            shell script · executable
    [FILE] bootstrap script.sh  shell script
    [DIR]  scripts/             进入目录继续选择
```

- 排序：精确 prefix > executable > 期望扩展名 > 普通文件 > 目录；当输入本身是目录 prefix 时目录优先。
- 展示 `bootstrap script.sh`，实际未引用上下文可插入 `bootstrap\ script.sh`；两者严格分离。
- 选择目录始终 `InsertAndContinue`。
- 同名 symlink 可显示 `link` 注释，不在补全时解析其目标内容。
- 扫描超过预算时先显示首批，状态行使用静态 `scanning` 标签；不能用 spinner 改变整体宽度。

## 9. 项目脚本交互

```text
~/app/packages/web $ pnpm run ▌
  > [PROJ] dev          vite --host
    [PROJ] build        tsc -b && vite build
    [PROJ] test         vitest run
    [PROJ] lint         eslint .
           package.json · packages/web
```

- 主体只显示 script 名，但 `TextEdit` 生成完整 `pnpm run <name>`。
- manifest 来源显示相对 workspace 路径，避免长绝对路径占满列表。
- script body 去除控制字符并截断；它仅是说明，不参与执行。
- JSON 损坏时显示一条 `package.json: invalid JSON` 诊断，history 和文件候选仍正常。
- 输入 `pnpm run bu` 时只替换 `bu`，不重写前面的 `pnpm run `。

## 10. 自然语言与 AI

### 10.1 AI 动作出现

```text
$ 查找当前目录 7 天内修改过的 rs 文件▌
  > [AI] 使用 AI 生成命令
    [HIS] find . -name '*.rs' -mtime -7    相似 history
```

出现规则：

- 自然语言 detector 达到阈值，或输入以配置的 `??` 开头；
- AI 已启用、endpoint/model/key source 可用；
- 同一输入只显示一个 AI 动作，不预先请求、不显示虚构命令。

未配置时默认不显示 AI 动作；`hokan ai setup`（交互向导）、`hokan config ai`（脚本化路径）和 `hokan doctor` 提供设置与诊断入口。

### 10.2 Loading 与取消

```text
$ 查找当前目录 7 天内修改过的 rs 文件▌
  > [AI] 正在生成...                  可取消
```

- 保留固定行高，不用动画字符导致布局抖动。
- `Esc` 取消；用户继续编辑也自动取消并开始新的本地 query。
- 超过 timeout 后显示一条可重试状态，不隐藏本地结果。

### 10.3 结果与检查

```text
$ 查找当前目录 7 天内修改过的 rs 文件▌
    AI RESULTS
  > [AI] find . -type f -name '*.rs' -mtime -7
         查找 7 天内修改的 Rust 源文件 · read-only

    [AI] fd -e rs --changed-within 7d
         使用 fd 查找；仅在本机存在 fd 时显示
```

- AI 结果视图允许每项使用最多三行，以便完整展示命令和解释；总 overlay 高度仍固定在上限内。
- 本机不存在的首 token 默认降权或隐藏，并在可替代时说明。
- 选中结果后只回填，退出 AI 结果视图；绝不直接执行。
- 含危险结构的结果显示 `RISK` 和原因。Hokan 不替用户修改 AI 命令来“自动变安全”。

错误示例：

```text
  [AI] 请求超时              retry
  [AI] 凭据被拒绝            run: hokan ai setup
  [AI] 响应不是有效命令列表  retry
```

错误中不显示 endpoint query、key 或完整响应体。

## 11. 风险反馈

风险信息必须具体，不只显示抽象颜色：

```text
  [RISK] rm -rf target/       递归删除目录 · high
  [RISK] curl ... | sh        下载内容直接交给 shell · high
  [RISK] command > file       覆盖写入文件 · medium
```

接受后：

```text
$ rm -rf target/▌
  ! 已回填高风险命令：递归删除目录；再次 Enter 才会执行
```

提示在用户继续编辑、关闭或提交后消失。Hokan 不阻止用户手工输入并执行同一命令，也不把风险分类宣传成安全保证。

## 12. 特殊终端状态

### 12.1 窄终端

- 40 至 59 列：隐藏 annotation，保留类型、主命令和最短描述。
- 小于 40 列：只显示选中标记、主命令和风险标签；description 隐藏。
- 可用行少于 3：P0 直接隐藏。后续若实现单行模式，它也必须是 compositor 管理的固定 surface，不能另写 ghost text 覆盖 prompt。

### 12.2 Resize

收到 resize 后立即丢弃未提交 frame，并推进 screen epoch。只有旧 rect 坐标仍可靠时才把它 diff 到 blank；位置不可靠时不盲目 erase。Hokan 先同步 child PTY size，随后隐藏列表，等待新的 prompt boundary/CPR anchor，再按新尺寸 full paint。候选 id 和选中项保留，旧 row index 与 previous buffer 不保留。

resize storm 只处理最后一个尺寸；过程中不能反复清屏、追加空行或让光标逐次跳动。

### 12.3 原子呈现与 fallback

Hokan 启动时异步查询 DEC private mode 2026：

- 明确支持且 mode 空闲时，一个 overlay 更新由 BSU/ESU 包围；用户只能看到旧完成态或新完成态，不能看到空白中间帧。
- 不支持或 reply timeout 时走 fallback。fallback 直接覆盖发生变化的 cells，行缩短时只清 stale suffix；普通导航绝不先清空整块 surface。mode busy/externally owned 时延后或隐藏，不能把 Hokan frame 嵌入他人的 synchronized transaction。
- 两条路径都必须恢复 child 的 SGR、绝对光标和 cursor visibility；都不使用 `ESC 7/8`，不写最后一列，不清全屏或 scrollback。
- 若 compositor 无法证明 anchor、screen epoch 或控制序列边界安全，预期行为是暂时隐藏列表，让 shell 保持可用，而不是尝试一次“可能正确”的绘制。
- shell control event 先到而 PTY redisplay 尚未收敛时同样不画新 frame；等待 deadline 只会保留不可激活的旧完成态或隐藏，不以固定延迟后强行重画。

无 mode 2026 的终端无法在协议层保证每个物理 present 都原子，但 Hokan 保证不主动制造 clear-before-paint 的空白帧。兼容验收会检查随机分片后的每个中间态，而不只检查最终截图。

### 12.4 SSH/tmux

- 只使用 TTY/VT 协议和运行时 capability probe，不依赖桌面 API，也不单凭 `$TERM` 判断 synchronized output。
- tmux 3.7+ 才可能识别 pane 内应用发出的 mode 2026；tmux 3.6 及更早版本预期走 cell-diff fallback。最终决策仍以 DECRQM reply 为准。
- probe/CPR 有 deadline，timeout 后安静降级；CPR 只在 prompt 建立或失步恢复时使用，绝不让每次按键等待 SSH RTT。
- 网络延迟不会影响本地候选；Hokan 运行在哪台机器，history/files/AI 请求就发生在哪台机器。

### 12.5 Shell/TUI 输出

child output 永远优先且不能因 overlay 丢失或延迟。`OutputActor` 在安全控制序列边界将受影响的旧 surface、child bytes 和必要的新 frame 排成一个 transaction；不能证明仍可复用 anchor 时就隐藏列表。fallback 中也禁止“先清整块 overlay、再转发 child output”的固定两步流程。

收到 `CommandStart`、foreground pgid 变化或 alternate-screen enable 后，清除能可靠定位的 surface 并进入纯透传；alternate screen 内不发 CPR、BSU 或候选 frame。返回主屏、foreground pgid 回到 shell 且出现新 prompt boundary 后才重新建立 anchor。

异步 job notification 或 prompt redraw 若能被 terminal model 完整解释，可以在同一 transaction 后恢复列表；未知控制序列、超长 OSC/DCS 或 scroll mapping 不确定时，列表保持隐藏直至下一个可靠边界。

## 13. 可访问性与可配置性

- 完整功能不依赖鼠标、图标或 24-bit color。
- 支持 `NO_COLOR`、`color = auto|always|never` 和 ASCII-only。
- 选中态同时使用 `>`、反色/背景和位置，不只用颜色。
- 风险同时用 `RISK`、文字原因和级别。
- 用户可以禁用自动打开、来源类型、AI、history 学习和任意全局键位。
- UI 文案集中管理，为后续中英文切换保留稳定 message id；v1 可先提供中文和英文之一，但命令规格数据不得把语言与逻辑耦合。

## 14. 关键交互验收用例

1. 输入 `ls`：无默认选中，`Enter` 执行原输入；`Down` 选中 recipe 后 `Enter` 直接执行该 recipe，`Tab` 则只回填。
2. 输入 `lso`：无选中时 `Enter` 执行 `lso`（shell 报 command not found）；`Down` 选中 `lsof` 后 `Enter` 执行 `lsof`。
3. 输入 `kill `：进程候选是 `InsertAndContinue`，选中后 `Enter` 退化为回填，不执行。
4. 选中 High/Unknown 风险候选（如 history 中的 `rm -rf …`）后 `Enter`：先出现红色 EXEC 确认行，`Enter` 确认执行，`Esc` 取消返回列表。
5. 输入 `bash bootstrap\ s`：选择带空格文件后 shell 得到一个正确 argv。
6. 输入 `pnpm run bu`：只替换当前 token，结果为 `pnpm run build`。
7. 打开 history、上下导航后异步文件结果到达：选中项不跳动。
8. 选择 AI action 后继续键入：请求取消，迟到响应不显示。
9. AI 返回 `rm -rf ...`：显示高风险；AI 候选只能回填或触发 action，不作为命令直接执行。
10. 在列表可见时运行 `vim`：overlay 在 alternate screen 前被清除，退出后 prompt 正常。
11. 任意阶段 `Ctrl-C`、resize、suspend/resume：无残留行、光标和 raw mode 问题。
12. 在支持 mode 2026 的终端连续长按上下键：录制帧中只出现完整旧/新选中态，没有空列表、半帧或 cursor jump。
13. 在不支持 mode 2026 的终端重复相同操作：随机 byte/chunk 中间态没有全空 surface、全屏清除或 prompt 覆盖。
14. tmux 3.6 走 fallback，tmux 3.7+ 按 probe 结果选择路径；两者 resize、detach/attach 后都能重新建立 anchor。
15. 列表可见时出现异步 child output 或未知 OSC/DCS：child bytes 完整有序，UI 要么可靠重绘，要么安静隐藏，不能污染输出。
16. 人为交换 `BufferSnapshot`、PTY redisplay chunk、render marker 和 provider batch 的到达顺序：新候选在 readiness 前不可见/不可激活，收敛后一次替换且无闪白。
