# CLI help completion coverage / CLI 帮助补全覆盖

The help provider reads the installed application's help instead of maintaining
an AIPass-only command list. Commands without descriptions, grouped tables,
single-space columns, long names, and terminal styling use shared parser rules.
Documented nested command scopes are probed as the user types them. Cargo's
abbreviated root help is supplemented by its installed command list.

帮助补全读取已安装应用的文档，使用共享规则处理空描述、分组、单空格列、长名称和终端
样式。用户输入已确认的子命令后，后台探测对应帮助层级。Cargo 的精简帮助会合并已安装
命令列表，Python 模块推荐保留全部已发现的匹配项。

The [captured corpus](../tests/fixtures/command-help/README.md) covers 15 CLIs,
32 help samples, and 785 expected recommendations. Default tests compare exact
canonical command sets and editable candidates, excluding category headings and
description prose. A separate opt-in test checks the installed applications
against those expectations. Native macOS live checks passed on 2026-09-30.

[共享样本](../tests/fixtures/command-help/README.md)覆盖 15 个 CLI、32 份帮助输出和
785 项预期推荐。默认测试比较完整命令集合与可回填候选，排除分组标题和描述正文；
可选的真实应用测试检查已安装版本。2026-09-30 的 macOS 原生真实应用验证已通过。

Help discovery is limited to recognized documentation. Undocumented commands
or opaque layouts may need specifications. Probes retain time/output bounds and
run in the background. Explicitly abbreviated tables remain non-exhaustive when
validating history. Existing positive candidate limits remain respected.

发现范围以可识别的帮助文档为准；隐藏命令或不透明的格式可能需要命令规格。探测在后台
执行，保留超时及输出边界。精简命令表不会被当作完整集合来过滤历史命令，用户显式设置
的候选数量限制仍然生效。

Docker CLI checks read help text only, without creating or using containers.
Local verification is native macOS; Linux verification belongs to GitHub Actions.
This corpus does not expand the [real-terminal certification](compatibility.md).

Docker CLI 检查只读取帮助，不创建或使用容器。本地只验证 macOS 原生环境，Linux 由
GitHub Actions 验证。这些样本不扩大[真实终端认证](compatibility.md)范围。
