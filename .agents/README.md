# Hokan 规划文档

> 状态：v0.1 implementation baseline  
> 更新日期：2026-08-02  
> 适用范围：Hokan 首个可发布版本及后续扩展的架构基线

Hokan 是一个以 Rust 单二进制分发、直接工作在 POSIX TTY/PTY 上的终端命令补全工具。它在用户真实的 shell 会话中提供 history、命令用法、文件、项目脚本和按需 AI 命令候选，不依赖 GUI、Electron、常驻账号或遥测服务。

## 文档索引

1. [产品需求](./01-product-requirements.md)：目标、范围、用户流程、功能需求与验收标准。
2. [总体架构](./02-architecture.md)：PTY 代理、shell 同步、事件循环、候选流水线、存储与安全边界。
3. [模块设计](./03-module-design.md)：Rust 目录、核心类型、模块接口、命令规格和配置结构。
4. [交互设计](./04-interaction-design.md)：内联列表、键位、默认选择、补参流程、AI 预览和异常状态。
5. [实施计划](./05-delivery-plan.md)：技术验证、里程碑、测试矩阵、风险和发布门槛。
6. [终端渲染专项研究](./06-terminal-rendering-research.md)：IRIS 源码审查、Rust 选型、原子帧协议、fallback 和无闪烁测试体系。
7. [实现与验收状态](./07-acceptance-status.md)：功能闭环、自动化证据、发布产物和仍待真实环境认证的矩阵。

## 已确定的产品决策

| 主题 | 决策 |
| --- | --- |
| 运行形态 | Hokan 包装一个子 shell，并在父终端与子 shell 的 PTY 之间代理输入输出。 |
| 平台范围 | v1 支持 macOS、Linux；支持 zsh、bash、fish。Windows、PowerShell 和非交互 shell 不在 v1 范围。 |
| “任何终端” | 指支持 UTF-8 和常用 ANSI/VT 控制序列的 POSIX TTY，包括本地终端、SSH 和 tmux；不承诺 `TERM=dumb`。 |
| Rust 边界 | 核心、CLI、PTY、渲染、存储和 HTTP 客户端全部使用 Rust。shell 初始化命令会生成少量 zsh/bash/fish 代码，这是访问各 shell 编辑缓冲区所必需的协议胶水。 |
| 候选呈现 | 默认使用一个统一列表混排本地候选；`Ctrl-R` 可切换到 history 专注视图。 |
| 执行语义 | 列表不默认选中；`Up`/`Down` 显式进入列表。无选中时 `Enter` 执行当前输入；有选中时 `Enter` 执行选中候选，仅 High 风险需二次确认。`Tab` 始终只回填。亲手输入的命令永不触发确认。 |
| AI 调用 | 自然语言判断完全在本地完成。仅显示一个 AI 动作项；用户选中该项后才发起网络请求。AI 结果只预览和回填，永不自动执行。 |
| 数据与遥测 | 默认无遥测。history 和排序数据仅保存在本机，AI 请求内容有明确、最小化的上下文边界。 |
| 技术策略 | 先完成 PTY、终端恢复和 shell 缓冲区同步技术验证，再扩展命令规格；这三项不过关就不进入功能堆叠。 |
| 渲染策略 | Ratatui 只负责离屏 Buffer/layout/widget；自建 compositor 和唯一 stdout actor。control event 需经 PTY render-readiness gate；运行时探测 synchronized output，支持时原子提交，其他终端走无 clear-before-paint 的 diff fallback。 |

## 需求追踪

| 原始目标 | 主要需求 | 主要模块 |
| --- | --- | --- |
| 多条 history 快速补齐 | `FR-HIS-*` | `providers/history`、`history`、`completion/ranking` |
| 常用命令参数组合与释义 | `FR-SPEC-*` | `providers/command_spec`、`specs` |
| 按上下文链出文件 | `FR-FS-*` | `providers/filesystem`、`parser/quote` |
| 项目命令补齐 | `FR-PROJ-*` | `providers/project` |
| 自然语言转命令 | `FR-AI-*` | `ai/detector`、`providers/ai_action`、`providers/ai_client`、`safety` |
| TTY 内联样式 | `FR-TTY-*`、`FR-UI-*` | `terminal`、`shell`、`app` |

## 术语

- **输入缓冲区**：当前由 shell 行编辑器维护、尚未提交的命令文本及光标位置。
- **候选**：可对输入缓冲区执行一次编辑、继续补参、发起 AI 请求或提交当前命令的列表项。
- **EXEC 确认行**：选中 High 风险候选并按下 `Enter` 后出现的二次确认行，展示完整命令文本与风险原因，`Enter` 确认执行、`Esc` 取消。
- **配方**：常见且有释义的参数组合，例如 `ls -lah`。
- **槽位**：配方中尚待用户选择的动态参数，例如文件、目录、PID 或 archive 名称。
- **Provider**：根据一次不可变输入快照产生候选的模块。
- **强同步**：shell 适配器能够提供真实缓冲区和光标，并能由 Hokan 精确回填。
- **降级同步**：Hokan 根据终端按键镜像缓冲区，只保证文档列出的标准编辑键。

## 参考项目边界

交互方向参考 [IRIS](https://github.com/versenilvis/IRIS) 的 `efc49bac`（2026-08-01）：真实 PTY、内联候选、history/spec 模式、SSH/tmux 兼容和无 GUI 分发。Hokan 不直接复制 IRIS 的 Go 实现；尤其不会把“按键镜像缓冲区 + `Ctrl-U` 重写整行”作为唯一同步手段，因为该方案容易与 ZLE/Readline 自定义键位、vi 模式、Unicode 和复杂粘贴失步。
