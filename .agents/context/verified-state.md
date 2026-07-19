# 已验证状态快照

最后复核：2026-07-19。当前维护基线为 v0.8.0；历史版本的固定证据不在本文件重复。

## v0.8.0 已发布

- 公开 peeled `v0.8.0`、发布时 `origin/main` 与 clean build source 均为 `4b163d50178791e7fbf9e085eb06fc2260baed4e`。
- 五层 release-ready Gate 退出 0；Desktop 为 350 passed / 2 explicit ignored，Gateway 为 232 lib + 1 CLI passed。这是源码与 loopback 证据，不自动证明最终 DMG 的真实 provider 行为。
- 最终 DMG 的大小、SHA-256、GitHub digest、重新下载逐字节一致性、只读挂载内容与 arm64 executable identity 已建立；完整数值见 [v0.8.0 发布证据](../../docs/evidence/releases/v0.8.0.md)。
- 最终公开 v0.8.0 DMG 没有重新执行真实账号登录、provider 推理或 Science live E2E；自动合同、历史 Acceptance 与最终公开 artifact 必须分层表述。
- Codex 认证保存在 CSSwitch 自有 data root 私有文件中，不使用 macOS Keychain，也不读取、复用、修改或删除原生 `~/.codex` 登录。
- 最终 app 的 ad-hoc seal 与 bundle integrity 已验证；没有建立 Developer ID、notarization 或 Gatekeeper acceptance 证据。

## 证据入口

- 当前发布与附件：[v0.8.0 release evidence](../../docs/evidence/releases/v0.8.0.md)
- Acceptance 候选：[2026-07-17 browser-only Acceptance](../../docs/evidence/investigations/2026-07-17-codex-browser-only-acceptance.md)
- 历史版本：[发布证据索引](../../docs/evidence/releases/README.md)

本文件不保存本机 worktree 路径、临时 artifact 位置或可漂移的工作区数量；这些状态必须实时查询。
