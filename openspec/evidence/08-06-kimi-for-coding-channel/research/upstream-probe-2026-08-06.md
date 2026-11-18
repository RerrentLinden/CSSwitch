# Kimi for Coding 上游实测（2026-08-06）

端点 `https://api.kimi.com/coding`，用户提供的 24h 临时订阅 key（**未写入仓库**，仅置于仓库外 `/tmp/csswitch-kimi-coding.key`，0600）。
所有结论均来自真实 live 请求，非推断。

## 1. 端点与鉴权

- `GET /coding/v1/models` → 200，`Authorization: Bearer <key>`。**不接受也不需要 `x-api-key`**（未测反例，仅确认 Bearer 可用）。
- `POST /coding/v1/messages` 接受标准 Anthropic Messages 请求体，返回标准 Anthropic 响应结构。
- `endpoint_join` 语义与现有 `anthropic_v1` 一致：base `https://api.kimi.com/coding` + `/v1/messages`。

## 2. 模型目录（实测返回）

| id | display_name | context_length | think_efforts |
| --- | --- | --- | --- |
| `kimi-for-coding` | K2.7 Coding | 262144 | 无 |
| `kimi-for-coding-highspeed` | K2.7 Coding Highspeed | 262144 | 无 |
| `k3` | K3 | 1048576 | `low/high/max`，默认 `high` |
| `k3-256k` | K3-256k | 262144 | `low/high/max`，默认 `high` |

四个模型均 `supports_reasoning: true`、`supports_thinking_type: "only"`（思考不可关闭语义），
`kimi-for-coding*` 支持图片与视频输入，`k3-256k` 不支持视频。

## 3. thinking 行为（决定性）

请求体 `thinking` 字段取值对 k3 的影响：

| 请求 `thinking` | 结果 |
| --- | --- |
| 省略该字段 | 200，**返回 thinking 块**（上游默认开启） |
| `{"type":"enabled","budget_tokens":N}` | 200，返回 thinking 块 |
| `{"type":"auto"}`（**Claude Science 实际发送值**） | 200，但**不返回任何 thinking 块** |
| `{"type":"disabled"}` | 200，不返回 thinking 块 |
| `{"type":"enabled","effort":"high"/"max"/"bogus"}` | 200，非法 effort 不报错 |

**结论 A：** Science 发的非标准 `auto` 会让 Kimi 静默关闭思考。这就是"thinking 丢失"的实际机制 ——
不是参数在链路里丢了，是上游不认 `auto` 而退化为不思考。

**结论 B：** 历史中 assistant 消息的 thinking 块被剥离（Science 按 Anthropic 规范的行为，见 PR #73）后，
即使消息含 `tool_use`，Kimi for Coding **依然返回 200**。实测四种组合（剥离/保留 × enabled/omit）全部 200。

因此 DeepSeek 那条"thinking 链断裂 → 400"的兼容问题**不适用于本渠道**，
也**不需要**引入现有 Kimi 通路的 `auto → enabled` 重写与 `reasoning_state` 续写机制
（该机制当前存在 thinking 不连续报错）。最小正确做法是**移除非标准的 `thinking:{type:auto}`**，让上游用默认值。

**结论 C：** `tool_choice: {"type":"any"}` 与 thinking 可以共存（Anthropic 规范禁止，Kimi 允许），
两种 thinking 取值下均 200 且返回 `['thinking','tool_use']`。现有 relay 里"强制工具时删除 tool_choice"的
补偿对本渠道非必需。

## 4. 服务端 web_search（决定性）

Kimi for Coding **原生支持** `web_search_20250305`，返回标准 `server_tool_use` + `web_search_tool_result`
（含 `encrypted_content`、`page_age`）。

但存在一条硬约束，实测可复现：

| 声明 `web_search_20250305` | 本轮模型是否真的发起搜索 | 结果 |
| --- | --- | --- |
| 是 | 是 | 200（多次采样全部成功） |
| 是 | 否 | **429 `rate_limit_error: "The engine is currently overloaded"`**（9/9 复现） |
| 否 | — | 200 |

对照组已排除的混淆变量：

- `max_tokens`（64 / 512 / 4096 三档，结果不变）
- 轮次（单轮与多轮同样表现）
- 是否带 system prompt
- 是否同时声明客户端工具（只带客户端工具恒 200）
- `max_uses` 是否存在
- 显式 `tool_choice: auto`
- 提示复杂度（真实编码类提示同样 429，非"提示太短"所致）
- 历史中是否回传 `server_tool_use` / `web_search_tool_result` 块（不声明工具时回传完整 server 块 → 200）

**结论 D：** 429 的真因是"声明了 web_search 但本轮没实际搜索"，与请求格式、历史内容、轮次均无关。
Claude Science 每轮都声明该工具，而绝大多数轮次不需要搜索，因此表现为"始终 429"。
Kimi 用了误导性的 `rate_limit_error` 文案，实际不是配额限流：429 响应**不带** `retry-after`
或任何 `x-ratelimit-*` 头，且约 0.9~1.3s 快速返回（正常生成 4~19s）。

**结论 E：** 补偿策略可行且已实测通过 —— 首发带 web_search，遇 429 则去掉 server tool 重发：

| 场景 | 实测 |
| --- | --- |
| 非搜索类编码提示 | 429 (0.9s) → 去工具重发 → 200 (9.1s) |
| 非搜索类解释提示 | 429 (1.3s) → 去工具重发 → 200 (3.8s) |
| 需要搜索的提示 | 首发即 200 (18.5s)，返回完整 server 搜索块 |

代价是非搜索轮增加约 1s；收益是真正可用的服务端搜索。该 429 在任何 token 生成前返回，重发不产生重复计费内容。

## 5. 流式

- `content-type: text/event-stream;charset=utf-8`，帧为标准 Anthropic 事件序列
  （`message_start` / `content_block_start` / `content_block_delta`(`thinking_delta`,`signature_delta`,`text_delta`) / `message_stop`）。
- **格式差异：** Kimi 发 `event:message_start`（冒号后无空格），Anthropic 惯例为 `event: message_start`。
  现有 gateway 用 `strip_prefix("event:")` 再去可选空格解析，**已兼容**，无需改动。
- 流式下 429 行为与非流式一致（同样约 0.8s 返回 JSON 错误体，不进入流）。

## 6. 响应块顺序差异

搜索轮返回的块顺序为 `text, server_tool_use, web_search_tool_result, thinking, text` ——
`thinking` 不在首位，且开头会多一个"Search results for query: …"的 text 块。
Anthropic 规范要求 thinking 为首块。回传该顺序给 Kimi 自身不报错（已实测），
但如果 Science 侧对块顺序敏感，需要在写回时验证。**该项尚未在真实 Science 上验证。**

## 6.1 四个模型的行为差异（补验）

| 行为 | `kimi-for-coding` | `-highspeed` | `k3` | `k3-256k` |
| --- | --- | --- | --- | --- |
| 省略 `thinking` | 有 thinking 块 | 有 | 有 | 有 |
| `thinking: {"type":"auto"}` | **有 thinking 块** | **有** | **无** | **无** |
| 指定工具型 `tool_choice` + 默认思考 | 400 | 400 | 400 | 400 |
| 指定工具型 `tool_choice` + `disabled` | 200 | 200 | 200 | 200 |
| 声明 WS 但不搜索 | 429 | 429 | 429 | 429 |

**结论 G：** `auto` 导致的静默不思考只发生在 `k3` 系列（models 接口里只有它们带 `think_efforts`，
对 `thinking.type` 解析更严）。但"删除非标准 `auto`"这一设计对四个模型都正确且安全，无需按模型分支。

**结论 H：** `kimi-for-coding-highspeed` 的一次**无工具**基础请求也返回了 429。
说明该端点存在**与 web_search 无关的真实偶发 429**。

这条推翻了方案 B 的可靠性基础：方案 B 把 429 解释为"本轮模型没搜索"，
而真实偶发 429 会让它误判并错误地永久丢弃该轮的 web_search，属于不可解释的错误降级。
方案 C 不依赖 429 语义（判断信号是模型主动发出的 `tool_use`），天然不受影响。

## 7. DeepSeek 对照组（同日实测）

用用户提供的 DeepSeek 临时 key 打 `https://api.deepseek.com/anthropic/v1/messages`（`deepseek-v4-pro`），
验证"声明 web_search 但模型不搜索"是否是 Anthropic 兼容端点的普遍问题：

| 场景 | 结果 |
| --- | --- |
| 无工具 | 200 |
| 仅客户端工具 | 200 |
| 声明 WS + 无需搜索（`say OK`） | **200** |
| 声明 WS + 真实编码提示 | **200** |
| 声明 WS + 需要搜索 | 200，返回 `server_tool_use` + `web_search_tool_result` |
| 声明 WS + 客户端工具 + 无需搜索 | **200** |

**结论 F：** DeepSeek 没有该缺陷。"声明 web_search 但本轮未搜索 → 429" 是 **Kimi for Coding 独有的实现缺陷**，
不是 Anthropic 兼容端点的普遍行为，也不是规范模糊地带。

因此：

- 现有 `tool.deepseek.web_search.server-tool-preserve` 规则**正确，无需改动**，也不存在同类潜在问题。
- 本任务对 Kimi 做的补偿必须绑定在新 contract 上，不得泛化成"Anthropic relay 通用行为"。
- 该缺陷值得向 Kimi 反馈；补偿逻辑应可在上游修复后单独移除。

附带差异：DeepSeek 的搜索轮块序为 `thinking, server_tool_use, web_search_tool_result, thinking, text`，
`thinking` 位于首块（符合 Anthropic 规范）；Kimi 为 `text, server_tool_use, web_search_tool_result, thinking, text`，
首块是 text 且 thinking 排在搜索结果之后。

DeepSeek 临时 key 同样置于仓库外 `/tmp/csswitch-deepseek.key`（0600），任务结束需一并删除。

## 8. 尚未验证

- Claude Science 实际发出的原始请求体（是否每轮都声明 `web_search_20250305`、`thinking` 具体取值、
  历史中 server 块的实际形态）—— 需要在 gateway 入站侧抓取。
- 响应经 gateway 写回 Science 后的真实渲染效果，尤其是搜索块与非首位 thinking 块。
- `kimi-for-coding` / `kimi-for-coding-highspeed` 两个模型的行为（本轮探测集中在 `k3`）。
- 长时间跨度下 429 是否还有真实配额维度（当前样本集中在单次会话内）。
