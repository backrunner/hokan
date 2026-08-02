# Hokann 故障排查

## 首先运行 doctor

```bash
hokann doctor
hokann doctor --json
```

`doctor` 会检查 TTY、`TERM`、shell 可执行文件、配置和键位、XDG 目录权限、AI
endpoint/model/凭据、诊断日志状态，以及活跃会话中的 hook 协议、私有 session 目录和
control FIFO。

在管道或 IDE task 中运行时，stdin/stdout 通常不是 TTY；这只会让交互 session 判定为
不可用，不影响 `config`、`history`、`spec` 和 `doctor` 子命令。

## zsh 自动启动与临时旁路

执行 `hokann setup --shell zsh` 后，当前已经运行的 shell 不会被替换；重新打开一个真实
终端窗口后，交互式 zsh 会自动 `exec` 安装时记录的 Hokann 二进制。外层 zsh 不加载
hook，hook 只在 Hokann 管理的内层 zsh 中加载，因此不会形成递归 session。

需要临时进入普通 zsh 排查 rc 文件时运行：

```bash
HOKANN_AUTO_START=0 zsh -l
```

带 `-c` 的 zsh、stdin/stdout 不是 TTY、`TERM=dumb` 或显式设置
`HOKANN_AUTO_START=0` 时不会自动启动。移动或重装二进制后重新运行 setup，以更新受管块
中固定的可执行文件路径。

## 终端没有回显或输入模式异常

Hokann 会在正常退出、panic 和可处理信号路径恢复 canonical mode、echo、SGR 和光标。
若进程被 `SIGKILL` 或宿主终端异常中断，可运行：

```bash
stty sane
printf '\033[0m\033[?25h'
reset
```

`reset` 会重置更多终端状态，通常只在前两条仍无法恢复时需要。

## 列表不显示

1. 确认正在直接运行 `hokann`，而不是在现有 Hokann 子 shell 内递归启动。
2. 用 `hokann doctor` 检查 `TERM`、shell 和集成协议。
3. 确认配置中的相关键位没有禁用，且没有冲突。
4. 前台命令、alternate screen、未知按键、失去 CPR anchor 或正在等待 shell redisplay 时，Hokann 会主动隐藏列表。
5. bash/fish 的自定义键位或 vi mode 可能触发 mirrored sync 降级；先用默认 emacs/default 模式复现。

`Ctrl-L`、resize 和从 `vim`/`less` 返回后会重新获取 CPR anchor；在锚点可靠前不绘制。

## tmux 中没有 synchronized output

Hokann 不按 `$TERM` 或 tmux 版本硬编码能力，而是发送 DECRQM probe。tmux 3.6b 会走
cell-diff fallback，这是预期行为。fallback 不使用全屏 clear、alternate screen、
DECSC/DECRC 或 clear-before-paint。

若 tmux detach/attach 或 pane resize 后出现异常，先关闭当前 Hokann session，运行
`hokann doctor`，并记录 tmux、外层终端和 shell 的精确版本。

## 配置或凭据被拒绝

```bash
hokann config path
hokann config validate
hokann config ai
```

AI 凭据文件必须是当前用户拥有的普通文件，权限为 `0600` 或更严格，且不能是 symlink。
状态目录必须为 `0700`。运行一次需要 history 的 Hokann 命令会创建并收紧状态目录；也可
手动执行 `chmod 700 ~/.local/state/hokann`。

Hokann history、snapshot 和 lock 必须是当前用户拥有的普通文件，不能是 symlink。升级
时，Hokann 会把符合这两个条件的旧文件自动收紧为 `0600`；异主文件或非普通文件会被
拒绝，且不会读取或覆盖其内容。

AI 错误只报告稳定分类，不回显 API key、Authorization header 或完整请求。401/403
检查凭据，429 等待配额恢复，timeout/network 检查 endpoint；本地候选不会因此停用。

## 诊断日志

诊断日志默认关闭，也不会创建空日志文件。为复现短暂的 provider、AI 或配置重载问题，
可在 `config.toml` 中设置 `[logging].enabled = true` 后重启 Hokann。日志写入
`${XDG_STATE_HOME:-~/.local/state}/hokann/debug.log`，单文件默认上限 1 MiB，保留三个
轮转文件；`hokann doctor` 会显示当前策略和状态目录权限。

日志只记录类型化运行元数据，不记录 query、history、CWD、HTTP body 或环境变量值，
并对常见凭据格式再次脱敏。提交问题前仍应人工检查日志和 `doctor --json`，不要附上
私有路径内容或任何凭据。问题复现结束后将 `enabled` 改回 `false` 并重启。

## History 损坏或体积过大

```bash
hokann history stats
hokann history repair
hokann history compact
hokann history prune --keep 10000
```

`repair` 只截断不完整的末尾记录；中段或 snapshot 损坏会被隔离，shell 仍可启动。
`clear --yes` 只清除 Hokann store，不修改 shell 自己的 history 文件。

## Shell 集成冲突

安装和卸载只操作两个受管理 marker 之间的内容，并在修改前创建备份：

```bash
hokann setup --shell zsh
hokann uninstall --shell zsh --integration-only
```

重复或残缺 marker 会导致拒绝修改。此时先对照备份人工修复 rc 文件，不要删除用户配置。
重复运行 setup 会升级旧协议的受管块，并在内容已是最新时保持文件不变。

## 收集最小诊断

问题报告应包含 `hokann --version`、脱敏后的 `hokann doctor --json`、OS/架构、shell、
终端、tmux/SSH 版本和可重复按键步骤。必要时附上人工检查过的诊断日志；不要附上 API
key、完整 history、完整 prompt、环境变量值或私有路径内容。
