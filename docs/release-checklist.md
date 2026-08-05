# Hokan 发布清单

本清单是 `.agents/05-delivery-plan.md` 的执行入口。自动化通过不能替代真实终端认证。

## 渠道与 tag

- stable：`v<version>`（如 `v0.2.0`）→ GitHub 正式 release，stable 渠道用户看到。
- beta：`v<version>-beta.<n>`（如 `v0.2.0-beta.1`）→ workflow 自动标记 Pre-release，
  只有 `channel = "beta"` 的用户看到；beta 渠道取 prerelease 与 stable 的较新者。
- 两种 tag 都必须与 `Cargo.toml` 的 `version` 完全一致（含 `-beta.N` 后缀），
  workflow 会拒绝版本不一致。

## 自动化门槛

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
cargo test --release
cargo audit
cargo deny check advisories licenses sources
cargo package --locked
```

- stdout ownership 测试必须证明 overlay、probe 和 restore 只有一个 writer。
- terminal harness 必须覆盖 mode 2026、unsupported fallback、真实 tmux 3.6、resize、CPR、
  Ctrl-L、alternate screen、Unicode、termios、panic 和可处理信号。
- 普通 frame 中不得出现全屏/scrollback clear、alternate screen、DECSC/DECRC 或最后一列 write。
- AI mock 必须覆盖成功、HTTP 错误、timeout、取消、body limit、恶意控制字符和 secret 扫描。
- 四个 target 的归档必须附带 SHA256 和 SPDX JSON SBOM。

## 真实环境阻断矩阵

每个组合执行固定按键脚本和至少 120 FPS 录制，检测 overlay rect 的 blank/half frame，
再人工检查 cursor、prompt、scrollback 和 termios。精确记录写入 `docs/compatibility.md`。

| 项目 | 状态 |
| --- | --- |
| Terminal.app + zsh/bash | 待认证 |
| iTerm2 + zsh/bash | 待认证 |
| Ghostty + zsh/bash | 待认证 |
| Kitty + zsh/bash | 待认证 |
| WezTerm + zsh/bash | 待认证 |
| Alacritty + zsh/bash | 待认证 |
| fish 3.6+ 默认模式 | 待认证 |
| powerlevel10k 默认配置 + zsh | 待认证 |
| powerlevel10k instant prompt 开/关 + zsh | 待认证 |
| powerlevel10k transient prompt 开/关 + zsh | 待认证 |
| oh-my-zsh + agnoster + zsh | 待认证 |
| SSH 延迟/分片/断开 | 待认证 |
| tmux 3.7+ runtime probe | 待认证 |
| macOS x86_64、Linux x86_64/aarch64 实机 | 待认证 |

任何可复现的 raw mode 恢复失败、输入丢失/重排、错误执行、空白帧、cursor jump、stale
overlay、跨 control sequence 写入、AI 隐式联网或 secret 泄漏都会阻止 beta 发布。

## Artifact 验证

1. tag 必须为 `v<package-version>`，workflow 会拒绝版本不一致。
2. 在干净机器校验 `SHA256SUMS`，解压对应 target 归档。
3. 运行 `bin/hokan --version`、`doctor --json`、`spec validate`。
4. 用临时 rc 文件验证 `setup` 幂等、备份和 `uninstall --integration-only`。
5. 检查归档包含 README、双许可证和 `share/man/man1/hokan.1`。
6. 检查每个 target 对应的 `.spdx.json` 可解析且与同一构建产物关联。

## Release profile

保留 unwind panic，以便顶层 panic guard 在恢复终端后输出通用错误；不能改为 `panic=abort`。
当前使用 thin LTO 和单 codegen unit。默认保留符号以便诊断，只有在记录体积、启动和崩溃
可诊断性的对比后才能改变 strip/LTO 策略。
