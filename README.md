# Hokann

Hokann 是一个完全使用 Rust 实现的 shell-aware 终端补全 overlay。它包装真实的
zsh、bash 或 fish 子 shell，在当前 TTY 内提供 history、命令配方、文件、项目脚本和
显式 AI 命令候选，不依赖 GUI、Electron、账号或遥测服务。

当前版本是 `0.1.0` beta candidate：五类核心功能和终端恢复链路已经接通，自动化测试
覆盖真实 PTY、tmux 3.6 fallback、mode 2026、信号、resize、Unicode 和异常退出。
Fish、SSH、tmux 3.7+ 以及完整真实终端矩阵仍需在发布前认证，详见
[兼容矩阵](docs/compatibility.md)。

## 功能

- 自动混排并模糊检索 zsh、bash、fish history；`Ctrl-R` 切换 history 专注视图。
- 为 `ls`、`df`、`tar`、`lsof`、`ifconfig`、`ps`、`kill` 等命令提供带释义、槽位和风险级别的配方。
- 按当前命令槽位补齐文件、目录、可执行脚本、PID 和网络接口，正确处理空格、引号和 Unicode 路径。
- 从最近的 `package.json` 补齐 `pnpm run`、`npm run`、`yarn run` 和 `bun run` 脚本。
- 本地识别自然语言；只有用户选中 AI 动作后才请求 OpenAI-compatible 接口，结果始终先回填、不会自动执行。
- 提供配置、history 维护、用户规格校验、shell 集成安装和结构化 `doctor` 诊断。

Hokann 不进入 alternate screen。Ratatui 只生成固定高度的离屏 surface，唯一 stdout
actor 在安全 VT 边界提交 cell diff；运行时支持 mode 2026 时使用同步输出，不支持时走
无全屏清除、无 clear-before-paint 的 fallback。

## 安装

要求 macOS 或 Linux、Rust 1.96+，以及 zsh、bash、fish 中至少一个：

```bash
cargo install --path . --locked
hokann --version
```

发布包解压后包含 `bin/hokann` 和 `share/man/man1/hokann.1`。把二进制和 man page
安装到本机前，先用同一 release 中的 `SHA256SUMS` 校验归档。

为当前 shell 安装幂等集成块；命令会先备份已有 rc 文件：

```bash
hokann setup --shell zsh
```

setup 完成后，重新打开一个真实的交互式终端窗口即可；zsh 会自动 `exec` Hokann，内层
shell 再加载补全 hook。脚本执行模式、非 TTY 和 `TERM=dumb` 不会自动启动。需要临时
进入普通 zsh 时可运行：

```bash
HOKANN_AUTO_START=0 zsh -l
```

不修改 rc 文件时仍可直接运行 `hokann --shell zsh`；`hokann init zsh` 可用于检查当前
版本生成的 hook。集成 hook 只有在 Hokann 启动的内层 shell 中才激活。卸载只删除受
管理的集成块，不删除配置或 history：

```bash
hokann uninstall --shell zsh --integration-only
```

## 使用

默认键位：

| 键位 | 行为 |
| --- | --- |
| `Up` / `Down` | 移动选择 |
| `PageUp` / `PageDown` | 翻页 |
| `Tab` | 回填候选，永不执行 |
| `Enter` | 激活候选；只有未改写且低风险的 `RunCurrent` 项可直接提交 |
| `Esc` | 关闭列表或取消 AI 请求 |
| `Ctrl-R` | 切换 history 专注视图 |
| `Shift-Tab` | 显示或关闭列表 |

初始化并检查配置：

```bash
hokann config init
hokann config show
hokann config validate
hokann spec validate
```

配置文件遵循 XDG 路径，默认在 `~/.config/hokann/config.toml`。history 状态默认在
`~/.local/state/hokann`，所有凭据和 history 文件都会执行 owner/mode 检查。

诊断日志默认关闭。需要复现运行时问题时可显式启用有限 JSONL 日志；修改后需重启
Hokann：

```toml
[logging]
enabled = false
max_bytes = 1048576
rotations = 3
```

启用后日志写入 `${XDG_STATE_HOME:-~/.local/state}/hokann/debug.log`，只包含 session、
provider 耗时/候选数、AI 结果分类和配置重载等类型化事件，不记录 query、history、CWD、
HTTP body 或环境变量值。

## AI

AI 默认关闭。推荐让 API key 只存在于环境变量中：

```bash
export OPENAI_API_KEY='...'
hokann config ai --enable \
  --endpoint https://api.openai.com/v1 \
  --model gpt-5-mini \
  --api-key-env OPENAI_API_KEY
```

也可让密码管理器经 stdin 写入权限为 `0600` 的独立凭据文件：

```bash
password-manager read openai-key | hokann config ai \
  --enable --model gpt-5-mini --api-key-stdin
```

普通自然语言输入只产生一个本地 AI 动作，不会联网。选择该动作后请求最多携带请求
文本、OS/架构、shell、可选 CWD basename 和项目类型；不会携带 history、环境变量值、
文件内容或完整目录列表。AI 命令经单行/控制字符校验和本地风险分类后仅作为回填项。

## 维护

```bash
hokann doctor
hokann doctor --json
hokann history stats
hokann history repair
hokann history compact
hokann spec list
```

`hokann history clear` 必须显式加 `--yes`，且只清除 Hokann 自己的 store。故障恢复、
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

Hokann 使用 `MIT OR Apache-2.0` 双许可证。
