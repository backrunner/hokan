# Hokan

Hokan 是一个完全使用 Rust 实现的 shell-aware 终端补全 overlay。它包装真实的
zsh、bash 或 fish 子 shell，在当前 TTY 内提供 history、命令配方、文件、项目脚本和
显式 AI 命令候选，不依赖 GUI、Electron、账号或遥测服务。

当前版本是 `0.1.0` beta candidate：五类核心功能和终端恢复链路已经接通，自动化测试
覆盖真实 PTY、tmux 3.6 fallback、mode 2026、信号、resize、Unicode 和异常退出。
Fish、SSH、tmux 3.7+ 以及完整真实终端矩阵仍需在发布前认证，详见
[兼容矩阵](docs/compatibility.md)。zsh powerline 类主题（p10k、oh-my-zsh
agnoster）下 Nerd Font 字形逐字节透传；instant/transient prompt 下的行为说明
同样见兼容矩阵。

## 功能

- 候选列表是跟随光标列的圆角边框盒子：Nerd Font 图标、已输入前缀高亮、右侧来源标签，分页与键位提示内嵌在边框中。
- 排序融合失败惩罚（上次执行失败降权）、命令转移 bigram（如 `git add` 后提升 `git commit`）和项目上下文加成（git/Node/Cargo/Make/just）。
- 自动混排并模糊检索 zsh、bash、fish history；`Ctrl-R` 切换 history 专注视图。
- 为 `ls`、`df`、`tar`、`lsof`、`ifconfig`、`ps`、`kill` 等命令提供带释义、槽位和风险级别的配方。
- 按当前命令槽位补齐文件、目录、可执行脚本、PID 和网络接口，正确处理空格、引号和 Unicode 路径。
- 从最近的 `package.json` 补齐 `pnpm run`、`npm run`、`yarn run` 和 `bun run` 脚本。
- 本地识别自然语言；只有用户选中 AI 动作后才请求 OpenAI-compatible 接口，结果始终先回填、不会自动执行。
- 提供配置、history 维护、用户规格校验、shell 集成安装和结构化 `doctor` 诊断。

Hokan 不进入 alternate screen。Ratatui 只生成固定高度的离屏 surface，唯一 stdout
actor 在安全 VT 边界提交 cell diff；运行时支持 mode 2026 时使用同步输出，不支持时走
无全屏清除、无 clear-before-paint 的 fallback。

## 安装

要求 macOS 或 Linux、Rust 1.96+，以及 zsh、bash、fish 中至少一个：

```bash
cargo install --path . --locked
hokan --version
```

发布包解压后包含 `bin/hokan` 和 `share/man/man1/hokan.1`。把二进制和 man page
安装到本机前，先用同一 release 中的 `SHA256SUMS` 校验归档。

为当前 shell 安装幂等集成块；命令会先备份已有 rc 文件：

```bash
hokan setup --shell zsh
```

setup 完成后，重新打开一个真实的交互式终端窗口即可；zsh 会自动 `exec` Hokan，内层
shell 再加载补全 hook。脚本执行模式、非 TTY 和 `TERM=dumb` 不会自动启动。需要临时
进入普通 zsh 时可运行：

```bash
HOKAN_AUTO_START=0 zsh -l
```

不修改 rc 文件时仍可直接运行 `hokan --shell zsh`；`hokan init zsh` 可用于检查当前
版本生成的 hook。集成 hook 只有在 Hokan 启动的内层 shell 中才激活。卸载只删除受
管理的集成块，不删除配置或 history：

```bash
hokan uninstall --shell zsh --integration-only
```

## 与 zsh 插件共存

Hokan 包装真实 zsh：内层 shell 照常加载你的 .zshrc，oh-my-zsh、powerline 主题和大多数
插件都在 Hokan 内运行，prompt 字节逐字节透传。会自绘提示或抢键位的补全类插件
（zsh-autosuggestions、atuin、zsh-autocomplete、fzf 键位等）建议加守卫，让它只在
普通 shell 中激活：

```zsh
[[ -z $HOKAN_ACTIVE ]] && source /path/to/zsh-autosuggestions.zsh
```

不想改动默认 shell 时可用按需模式：`hokan setup --shell zsh --on-demand` 只安装
`hk` alias，默认终端完全保持原样，输入 `hk` 才进入 Hokan；两种模式用同一对 marker
管理，重复 setup 即可切换。`hokan doctor` 会扫描 .zshrc/.zshenv 并点名已知冲突插件。

## 使用

默认键位：

| 键位 | 行为 |
| --- | --- |
| `Up` / `Down` | 移动选择 |
| `PageUp` / `PageDown` | 翻页 |
| `Tab` | 回填候选，永不执行 |
| `Enter` | 无选中时执行当前输入；有选中时执行选中候选（High/Unknown 风险需二次确认） |
| `Esc` | 关闭列表或取消 AI 请求 |
| `Ctrl-R` | 切换 history 专注视图 |
| `Shift-Tab` | 显示或关闭列表 |

初始化并检查配置：

```bash
hokan config init
hokan config show
hokan config validate
hokan spec validate
```

配置文件遵循 XDG 路径，默认在 `~/.config/hokan/config.toml`。history 状态默认在
`~/.local/state/hokan`，所有凭据和 history 文件都会执行 owner/mode 检查。

列表外观的默认值（含圆角边框和图标）：

```toml
[ui]
max_rows = 8        # 含上下边框，即最多 6 行候选；最小 3
max_width = 76
nerd_fonts = true   # 命令图标需要终端使用 Nerd Font；图标显示为方块时设为 false
```

诊断日志默认关闭。需要复现运行时问题时可显式启用有限 JSONL 日志；修改后需重启
Hokan：

```toml
[logging]
enabled = false
max_bytes = 1048576
rotations = 3
```

启用后日志写入 `${XDG_STATE_HOME:-~/.local/state}/hokan/debug.log`，只包含 session、
provider 耗时/候选数、AI 结果分类和配置重载等类型化事件，不记录 query、history、CWD、
HTTP body 或环境变量值。

## AI

AI 默认关闭。推荐让 API key 只存在于环境变量中：

```bash
export OPENAI_API_KEY='...'
hokan config ai --enable \
  --endpoint https://api.openai.com/v1 \
  --model gpt-5-mini \
  --api-key-env OPENAI_API_KEY
```

也可让密码管理器经 stdin 写入权限为 `0600` 的独立凭据文件：

```bash
password-manager read openai-key | hokan config ai \
  --enable --model gpt-5-mini --api-key-stdin
```

普通自然语言输入只产生一个本地 AI 动作，不会联网。选择该动作后请求最多携带请求
文本、OS/架构、shell、可选 CWD basename 和项目类型；不会携带 history、环境变量值、
文件内容或完整目录列表。AI 命令经单行/控制字符校验和本地风险分类后仅作为回填项。

## 维护

```bash
hokan doctor
hokan doctor --json
hokan history stats
hokan history repair
hokan history compact
hokan spec list
```

`hokan history clear` 必须显式加 `--yes`，且只清除 Hokan 自己的 store。故障恢复、
权限修复和 tmux 降级说明见 [故障排查](docs/troubleshooting.md)。

## 开发验证

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo test --release
cargo audit
cargo deny check advisories licenses sources
```

需求、架构和终端渲染研究位于 [`.agents/`](.agents/README.md)。发布流程与仍需人工完成
的真实终端项目见 [发布清单](docs/release-checklist.md)。

## License

Hokan 使用 `MIT OR Apache-2.0` 双许可证。
