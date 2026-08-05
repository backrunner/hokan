# Hokan 故障排查

## 首先运行 doctor

```bash
hokan doctor
hokan doctor --json
```

`doctor` 会检查 TTY、`TERM`、shell 可执行文件、配置和键位、XDG 目录权限、AI
endpoint/model/凭据、诊断日志状态，以及活跃会话中的 hook 协议、私有 session 目录和
control FIFO。

在管道或 IDE task 中运行时，stdin/stdout 通常不是 TTY；这只会让交互 session 判定为
不可用，不影响 `config`、`history`、`spec` 和 `doctor` 子命令。

## zsh 自动启动与临时旁路

执行 `hokan setup --shell zsh` 后，当前已经运行的 shell 不会被替换；重新打开一个真实
终端窗口后，交互式 zsh 会自动 `exec` 安装时记录的 Hokan 二进制。外层 zsh 不加载
hook，hook 只在 Hokan 管理的内层 zsh 中加载，因此不会形成递归 session。

需要临时进入普通 zsh 排查 rc 文件时运行：

```bash
HOKAN_AUTO_START=0 zsh -l
```

带 `-c` 的 zsh、stdin/stdout 不是 TTY、`TERM=dumb` 或显式设置
`HOKAN_AUTO_START=0` 时不会自动启动。移动或重装二进制后重新运行 setup，以更新受管块
中固定的可执行文件路径。

## 终端没有回显或输入模式异常

Hokan 会在正常退出、panic 和可处理信号路径恢复 canonical mode、echo、SGR 和光标。
若进程被 `SIGKILL` 或宿主终端异常中断，可运行：

```bash
stty sane
printf '\033[0m\033[?25h'
reset
```

`reset` 会重置更多终端状态，通常只在前两条仍无法恢复时需要。

## 列表不显示

1. 确认正在直接运行 `hokan`，而不是在现有 Hokan 子 shell 内递归启动。
2. 用 `hokan doctor` 检查 `TERM`、shell 和集成协议。
3. 确认配置中的相关键位没有禁用，且没有冲突。
4. 前台命令、alternate screen、未知按键、失去 CPR anchor 或正在等待 shell redisplay 时，Hokan 会主动隐藏列表。
5. bash/fish 的自定义键位或 vi mode 可能触发 mirrored sync 降级；先用默认 emacs/default 模式复现。

`Ctrl-L`、resize 和从 `vim`/`less` 返回后会重新获取 CPR anchor；在锚点可靠前不绘制。

## 图标显示为方块或问号

候选列表的命令图标使用 Nerd Font 字形（私有区码点）。终端字体不含这些字形时会显示为
方块或问号。两种处理方式：

1. 在 `~/.config/hokan/config.toml` 的 `[ui]` 中设置 `nerd_fonts = false`，列表退回纯
   文本标签；或
2. 给终端换用一款 Nerd Font（如 MesloLGS NF、JetBrainsMono Nerd Font）并重启终端。

边框的圆角字符（`╭│╰`）来自 Unicode 制表符区，任何等宽字体都支持；若连边框都异常，
检查终端的 UTF-8  locale（`LANG`/`LC_CTYPE`）。

## 看不到 oh-my-posh / starship 等主题

这些主题常把初始化写在 `~/.zprofile`，而 `.zprofile` 只有 login shell 才会读取。Hokan
的内层 zsh 默认不是 login shell（`core.login_shell = false`），因此主题不会加载。
处理：在 `~/.config/hokan/config.toml` 的 `[core]` 中设置 `login_shell = true`，或把主题
初始化移到 `~/.zshrc`。`hokan doctor` 的 `zsh theme` 检查会自动发现这种情况并给出提示。

## powerline 主题下 overlay 不出现或闪烁

常见原因：

1. 主题发出白名单外的 VT 序列（如 DECSCNM、DECSTR、窗口操作），Hokan 会 fail-safe
   隐藏 overlay 并重新锚定；序列停止后 overlay 会在下一次 redraw 恢复。
2. p10k instant prompt 阶段（.zshrc 顶部打印并擦除缓存 prompt 块期间）overlay
   本就不会出现；从第一个真实 precmd 起才锚定，这是预期行为。
3. 主题的 async segment 在 prompt 附近频繁重绘，会造成 overlay 反复隐藏并重新
   锚定；这属于保守设计，字节始终逐字节透传。

预期行为：Nerd Font/PUA 字形、RPROMPT、多行 PROMPT 和 transient prompt 的改写
都应原样出现在外层终端，overlay 稳定在编辑行下方。需要上报的情况：overlay
永久不出现、prompt 字节被改写或丢失、出现全屏清除/alternate screen 序列。上报
时请附上主题名称与版本、`p10k configure` 的关键选项（instant/transient prompt
开关）和最小 .zshrc。

## tmux 中没有 synchronized output

Hokan 不按 `$TERM` 或 tmux 版本硬编码能力，而是发送 DECRQM probe。tmux 3.6b 会走
cell-diff fallback，这是预期行为。fallback 不使用全屏 clear、alternate screen、
DECSC/DECRC 或 clear-before-paint。

若 tmux detach/attach 或 pane resize 后出现异常，先关闭当前 Hokan session，运行
`hokan doctor`，并记录 tmux、外层终端和 shell 的精确版本。

## 配置或凭据被拒绝

```bash
hokan config path
hokan config validate
hokan config ai
```

AI 凭据文件必须是当前用户拥有的普通文件，权限为 `0600` 或更严格，且不能是 symlink。
状态目录必须为 `0700`。运行一次需要 history 的 Hokan 命令会创建并收紧状态目录；也可
手动执行 `chmod 700 ~/.local/state/hokan`。

Hokan history、snapshot 和 lock 必须是当前用户拥有的普通文件，不能是 symlink。升级
时，Hokan 会把符合这两个条件的旧文件自动收紧为 `0600`；异主文件或非普通文件会被
拒绝，且不会读取或覆盖其内容。

AI 错误只报告稳定分类，不回显 API key、Authorization header 或完整请求。401/403
检查凭据，429 等待配额恢复，timeout/network 检查 endpoint；本地候选不会因此停用。

## AI 配置与错误码

查看当前 AI 状态和凭据来源（不回显 secret）：

```bash
hokan config ai
hokan doctor
```

重新配置（向导要求 stdin/stdout 是 TTY，脚本环境用 `hokan config ai --…`）：

```bash
hokan ai setup
```

向导会按服务商写入 `~/.config/hokan/credentials.toml`（`version = 2`，`0600`）。OAuth
服务商的 token 过期前自动刷新；刷新失败时该次请求会带旧 token 并收到 401，重新运行
`hokan ai setup` 登录即可。

AI 请求错误（`AiClientError`）的稳定错误码：

| 错误码 | 原因与处理 |
| --- | --- |
| `HK-AI-CFG` | endpoint 或客户端配置无效；检查 `[ai]` 配置并运行 `hokan config validate` |
| `HK-AI-CRED` | 凭据环境变量未设置，或凭据文件被拒绝；运行 `hokan config ai` 查看诊断 |
| `HK-AI-401` | endpoint 拒绝凭据；换 key 或重新运行 `hokan ai setup` 登录 |
| `HK-AI-429` | endpoint 限流；等待配额恢复 |
| `HK-AI-TIMEOUT` / `HK-AI-NET` | 请求超时或网络失败；检查 endpoint、代理和网络 |
| `HK-AI-SIZE` | 响应超过大小上限 |
| `HK-AI-JSON` | 响应不是有效命令 JSON；模型或 endpoint 不兼容 |
| `HK-AI-CANCEL` | 请求被 `Esc` 取消 |
| `HK-AI-HTTP` | endpoint 返回其他 HTTP 错误，消息中附状态码 |
| `HK-AI-PROJECT` | Gemini Code Assist 未返回 Google Cloud project；为该 Google 账号启用 Gemini Code Assist 后重新运行向导登录 |

OAuth 登录过程错误（`OAuthError`）的稳定错误码：

| 错误码 | 原因与处理 |
| --- | --- |
| `HK-AUTH-CANCEL` | 用户取消登录（输入 `q`、EOF 或 Ctrl-C） |
| `HK-AUTH-EXPIRED` | 设备码在登录完成前过期；重新运行 `hokan ai setup` |
| `HK-AUTH-DENIED` | 用户或服务器拒绝授权 |
| `HK-AUTH-NET` / `HK-AUTH-TIMEOUT` | 网络失败或超时；检查网络后重试 |
| `HK-AUTH-JSON` | 授权服务器响应无效 |
| `HK-AUTH-HTTP` | 授权服务器拒绝请求，消息中附 HTTP 状态码 |
| `HK-AUTH-429` | 授权服务器限流；稍后重试 |


## 诊断日志

诊断日志默认关闭，也不会创建空日志文件。为复现短暂的 provider、AI 或配置重载问题，
可在 `config.toml` 中设置 `[logging].enabled = true` 后重启 Hokan。日志写入
`${XDG_STATE_HOME:-~/.local/state}/hokan/debug.log`，单文件默认上限 1 MiB，保留三个
轮转文件；`hokan doctor` 会显示当前策略和状态目录权限。

日志只记录类型化运行元数据，不记录 query、history、CWD、HTTP body 或环境变量值，
并对常见凭据格式再次脱敏。提交问题前仍应人工检查日志和 `doctor --json`，不要附上
私有路径内容或任何凭据。问题复现结束后将 `enabled` 改回 `false` 并重启。

临时诊断也可以不改配置：`HOKAN_DEBUG_LOG=1 hokan` 会在本次会话强制开启同一日志。

## 更新失败排查

`hokan upgrade` 与后台自动更新共用同一条链路，错误码如下：

| 错误码 | 含义与处理 |
| --- | --- |
| `HK-UPD-CHANNEL` | 渠道名无效；只支持 `stable` / `beta` |
| `HK-UPD-NET` / `HK-UPD-TIMEOUT` | 网络失败或超时；检查网络/代理后重试 |
| `HK-UPD-HTTP` | GitHub API 拒绝请求（附状态码）；未认证限额为每 IP 每小时 60 次，稍后重试 |
| `HK-UPD-JSON` | release 响应无法解析 |
| `HK-UPD-ASSET` | 当前平台（target）的归档或 SHA256SUMS 不在该 release 中 |
| `HK-UPD-HASH` | 下载的归档与 SHA256SUMS 不匹配；请重试，持续出现请上报 |
| `HK-UPD-SMOKE` | 新二进制 `--version` 冒烟测试未通过，未替换 |
| `HK-UPD-PLATFORM` | 不支持的 OS/架构组合 |
| `HK-UPD-IO` | 本地文件读写失败；检查 `~/.cache/hokan` 权限 |

替换失败时当前二进制不会被破坏：升级前会备份为 `hokan.bak`（位于 hokan 二进制
旁）。路径不可写（系统包管理器安装）时 Hokan 只提示，请用原包管理器升级。

## History 损坏或体积过大

```bash
hokan history stats
hokan history repair
hokan history compact
hokan history prune --keep 10000
```

`repair` 只截断不完整的末尾记录；中段或 snapshot 损坏会被隔离，shell 仍可启动。
`clear --yes` 只清除 Hokan store，不修改 shell 自己的 history 文件。

## Shell 集成冲突

安装和卸载只操作两个受管理 marker 之间的内容，并在修改前创建备份：

```bash
hokan setup --shell zsh
hokan uninstall --shell zsh --integration-only
```

重复或残缺 marker 会导致拒绝修改。此时先对照备份人工修复 rc 文件，不要删除用户配置。
重复运行 setup 会升级旧协议的受管块，并在内容已是最新时保持文件不变。

## 与 zsh-autosuggestions / atuin / fzf 等插件冲突

症状：重叠的 ghost text、方向键或 `Tab` 被插件抢走、全屏程序附近闪烁。

Hokan 自身在前台全屏程序和 alternate screen 下会隐藏 overlay 并逐字节透传，但第三方
补全插件自行绘制的内容不在 Hokan 控制范围内。处理顺序：

1. 运行 `hokan doctor`，它会在 .zshrc/.zshenv 中扫描已知冲突插件并逐一点名。
2. 给插件的初始化行加守卫，让它只在普通 shell 中激活、在 Hokan 内层 shell 中保持
   关闭：

   ```zsh
   [[ -z $HOKAN_ACTIVE ]] && source /path/to/zsh-autosuggestions.zsh
   ```

3. 或改用按需模式 `hokan setup --shell zsh --on-demand`：默认 shell 完全保持原样，
   只有输入 `hk` 进入的 Hokan session 使用 overlay。

## 收集最小诊断

问题报告应包含 `hokan --version`、脱敏后的 `hokan doctor --json`、OS/架构、shell、
终端、tmux/SSH 版本和可重复按键步骤。必要时附上人工检查过的诊断日志；不要附上 API
key、完整 history、完整 prompt、环境变量值或私有路径内容。
