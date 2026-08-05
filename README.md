# Hokan

[![License: BSD-3-Clause](https://img.shields.io/badge/license-BSD--3--Clause-blue.svg)](LICENSE)
[![Rust 1.96+](https://img.shields.io/badge/rust-1.96%2B-orange.svg)](rust-toolchain.toml)
[![Platform: macOS & Linux](https://img.shields.io/badge/platform-macOS%20%26%20Linux-lightgrey.svg)](docs/compatibility.md)

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

- 候选列表是跟随光标列的圆角边框盒子：Nerd Font 命令图标与来源图标（  /  / ）、已输入前缀高亮，分页与键位提示内嵌在边框中。
- 排序融合失败惩罚（上次执行失败降权）、命令转移 bigram（如 `git add` 后提升 `git commit`）和项目上下文加成（git/Node/Cargo/Make/just）。
- 自动混排并模糊检索 zsh、bash、fish history；`Ctrl-R` 切换 history 专注视图；识别 `.zshrc`/`.bashrc`/fish 配置中定义的别名、函数和缩写（含一级 `source` 引入），命令位直接推荐并展示展开内容，history 里的别名条目不会被误过滤。自定义函数的参数也能被理解：`proj() { cd ~/projects/$1 }` 这类定义会推断出参数槽位，`proj <Tab>` 直接补全 `~/projects` 下的目录。
- 为 `ls`、`df`、`tar`、`lsof`、`ifconfig`、`ps`、`kill` 等命令提供带释义、槽位和风险级别的配方。
- 对没有内置规格的命令，从 man page 只读提取子命令和 flag 建议（`HELP` 来源）；flags 位和已被文档接管的首参数位不再做目录扫描。
- 按当前命令槽位补齐文件、目录、可执行脚本、PID 和网络接口，正确处理空格、引号和 Unicode 路径；识别常见取值 flag 的槽位类型（`git commit -m`、`ssh -p` 等文本槽不再误推文件，`curl -o`、`make -C` 等路径槽正常推文件）。
- 从最近的 `package.json` 补齐脚本，按历史使用次数排序。pnpm/yarn/bun 直接回填 `pnpm dev` 原生形式；npm 先出 `install`/`run` 等子命令，`npm run` 后再出 scripts；deno 出 `task` 等子命令并支持 deno.json(c) tasks。pnpm workspace 还会补 `--filter` 成员名和成员自身的 scripts。发现机制不依赖 PATH 中的包管理器（nvm/volta/corepack 晚初始化也能工作）。
- `git` 按仓库实际状态推荐：非仓库目录首推 `git init`/`git clone`；有改动时给 `status`/`add`/`commit`/`diff`，有未推送提交才给 `push`，落后时才给 `pull`，干净时给 `log`/`branch`/`fetch`。裸 `git` 无需空格即可触发。
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

主题初始化写在 `.zprofile` 时（oh-my-posh、starship 常见写法），内层 shell 需要
`core.login_shell = true` 才会加载它；doctor 的 `zsh theme` 检查会自动提示这种情况。

## 使用

默认键位：

| 键位 | 行为 |
| --- | --- |
| `Up` / `Down` | 移动选择 |
| `PageUp` / `PageDown` | 翻页 |
| `Tab` | 回填候选，永不执行；无选中时回填最推荐的一项，并让刷新后的列表自动选中第一行 |
| `Enter` | 无选中时执行当前输入；有选中时执行选中候选（仅 High 风险需二次确认） |
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

### 交互式配置向导 `hokan ai setup`

在终端中运行 `hokan ai setup` 可以交互式完成服务商、凭据和模型配置（stdin/stdout
必须是 TTY，脚本环境请继续用上面的 `hokan config ai`）。向导依次选择服务商、认证
方式、凭据和模型（先在线拉取列表，失败回退内置表），最后发一条最小请求做连接测试，
再原子写回配置；随时输入 `q` 退出且不写入任何内容。支持的 8 个服务商：

| 服务商 | 认证方式 | 默认端点 |
| --- | --- | --- |
| DeepSeek | API Key | `https://api.deepseek.com/v1` |
| OpenAI (ChatGPT) | OAuth 设备码 | `https://chatgpt.com/backend-api/codex` |
| Google Gemini (OAuth) | OAuth 粘贴授权码 | `https://cloudcode-pa.googleapis.com` |
| Google Gemini (API key) | API Key | `https://generativelanguage.googleapis.com/v1beta/openai` |
| xAI Grok (OAuth) | OAuth 设备码 | `https://api.x.ai/v1` |
| xAI Grok (API key) | API Key | `https://api.x.ai/v1` |
| Ollama（本地） | 无需凭据 | `http://localhost:11434/v1` |
| Custom | API Key，OpenAI 兼容 | 向导中输入 |

OAuth 服务商不需要 API Key：OpenAI 和 xAI 走设备码流程（打开提示的 URL 并输入显示的
代码），Gemini 打开授权 URL 后把页面显示的授权码粘贴回终端。凭据按服务商分别保存在
`~/.config/hokan/credentials.toml`（`version = 2` 的多服务商格式，权限 `0600`）；
OAuth token 过期前会自动刷新。向导摘要和任何错误信息都不回显 secret。

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

## 更新

Hokan 默认开启无感自动更新：每次会话启动时派生一个后台进程（默认每 30 分钟
一次 TTL 检查）查询 GitHub releases，发现新版本后自动下载、校验 SHA256、冒烟
测试并原子替换二进制，下次启动生效；替换前会把当前二进制备份为 `hokan.bak`。
系统包管理器安装的路径（不可写）只会提示，不会自动替换。

```bash
hokan upgrade            # 交互式检查并升级
hokan upgrade --check    # 只查看是否有新版本
hokan upgrade --channel beta  # 切换到 beta 渠道并升级（持久化）
```

两个发布渠道：`v0.2.0` 等正式 tag 对应 stable 渠道，`v0.2.0-beta.1` 等 tag 发布为
GitHub Pre-release 对应 beta 渠道（beta 渠道取 prerelease 与 stable 中的较新者，
永不自动降级）。配置与开关：

```toml
[update]
enabled = true         # false 关闭自动更新
channel = "stable"     # stable | beta
interval_secs = 1800
```

`HOKAN_NO_AUTO_UPDATE=1` 可临时禁用一次会话的后台检查；`hokan doctor` 的 update
段会显示渠道、最近检查到的版本和二进制可写性。

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

## 致谢

Inspiration by iris.

## License

Hokan 使用 [BSD-3-Clause](LICENSE) 许可证。
