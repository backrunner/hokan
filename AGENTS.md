# 开发规范

本规范适用于整个仓库的开发、审查与验证工作。

## 本地验证与 CI

- 本地只在 macOS 原生环境进行构建检查、测试、性能和 TUI 交互验证。
- 禁止为本项目的 Linux 验证在本地创建、启动或使用 Linux 容器或虚拟机，包括 Docker、Podman、Apple `container` 以及通过 `cross` 启动的容器。
- Linux 构建与测试交给 GitHub Actions，在 `.github/workflows/ci.yml` 和 `.github/workflows/release.yml` 对应的 Linux runner 上执行。本地验证不需要复刻 Linux CI 环境。
- 如果本次任务已经创建了用于 Linux 验证的本地容器，必须停止并删除，确认其不再存在；清理范围仅限本次任务创建的资源。
- 汇报验证结果时，明确区分本地 macOS 检查与 GitHub Actions 的实际结果；远端检查尚未运行时，应标明未验证。
