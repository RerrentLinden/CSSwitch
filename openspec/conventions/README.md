# 编码规范

这里放「怎么做」的规范。「系统必须怎样」的行为合同在 [`../specs/`](../specs/)，
每个能力一个目录：`spec.md` 是需求与场景，`design.md` 是签名、必需测试与反例。

## Backend

单个 Rust crate(`desktop/gateway`)。

| Guide | Description |
|-------|-------------|
| [Directory Structure](./backend/directory-structure.md) | 模块划分、新增补偿的落点、`packaging/` 布局 |
| [Quality Guidelines](./backend/quality-guidelines.md) | 上游能力判定、补偿退役、三层门禁合同 |

## WebUI

控制台是**单个内嵌 HTML 文件**(`desktop/gateway/ui/index.html`),
用 `include_str!` 编进二进制。没有构建链、没有框架、没有依赖。

| Guide | Description |
|-------|-------------|
| [Quality Guidelines](./frontend/quality-guidelines.md) | 可视化验收 |

## Thinking Guides

| Guide | Description |
|-------|-------------|
| [Index](./guides/index.md) | 什么时候该读哪一篇 |
| [Code Reuse Thinking Guide](./guides/code-reuse-thinking-guide.md) | 识别重复模式 |
| [Cross-Layer Thinking Guide](./guides/cross-layer-thinking-guide.md) | 跨层数据流,含 provider 渠道与上游协议适配 |
