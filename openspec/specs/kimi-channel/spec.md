# kimi-channel Specification

## Purpose

Kimi 渠道把 Claude Science 的推理流量转到 Kimi 的 Anthropic 兼容端点。上游的兼容层有若干缺口，本规格约束这个渠道的后端契约、
默认模型槽，以及请求侧与响应侧的窄补偿：每条补偿都由真实 live 请求确认，各自绑定规则 ID 与单测。
联网搜索相关的三块另有规格：[kimi-web-search-bridge](../kimi-web-search-bridge/spec.md)、
[kimi-search-pair-adoption](../kimi-search-pair-adoption/spec.md)、[kimi-context-tail-reorder](../kimi-context-tail-reorder/spec.md)。
补偿的通用规则见 [provider-compensation](../provider-compensation/spec.md)。
Key 被上游拒绝（401）与上游限流时回给 Science 的样子，见 [gateway-routing](../gateway-routing/spec.md)
「relay 把上游 401 改报 403」「relay 错误响应只转发 retry-after」；搜索桥的流已打开时见
[kimi-web-search-bridge](../kimi-web-search-bridge/spec.md)「流已打开后的上游失败以 SSE 收尾」。

## Requirements

### Requirement: 后端契约

Kimi MUST 走一个 provider contract，策略由契约声明、不靠环境变量：

| 项 | 值 |
| --- | --- |
| provider contract | `kimi-anthropic-relay` |
| adapter / transport | `relay` / `anthropic_messages` |
| 鉴权 | `bearer` |
| 端点拼接 | `anthropic_v1`（base + `/v1/messages`） |
| 默认 base_url | `https://api.kimi.com/coding`（可编辑） |
| thinking policy | `upstream_default` |

#### Scenario: 换成开放平台或其它区域端点

- **WHEN** 用户在控制台把 base_url 改成别的 Kimi 端点
- **THEN** 请求发往新地址，不需要改代码（模型清单由用户在四个槽里自己填）

### Requirement: 未验证的端点与模型失败可见

补偿证据来自 `api.kimi.com/coding`。开放平台和其它未列模型沿用同一个 contract，但没有独立 live；它们出现异常时 MUST 可见失败，
MUST NOT 被静默兜底。

#### Scenario: 未列模型返回异常

- **WHEN** 一个没有 live 证据的模型或端点返回异常
- **THEN** 失败原样可见

### Requirement: 默认模型槽

默认预填的模型槽 MUST 是下表（来自实测 `/v1/models`），且都可以在控制台改：

| 槽 | upstream_model | 显示名 |
| --- | --- | --- |
| 默认（均衡） | `k3-256k` | Kimi K3 256k |
| 高质量 | `k3` | Kimi K3 |
| 快速 | `kimi-for-coding` | Kimi K2.8 |
| Fable | 留空 | 继承默认槽 |

#### Scenario: 出厂配置

- **WHEN** 用户没有改过 Kimi 的模型槽
- **THEN** 四个槽是上表的取值

### Requirement: thinking 的 auto 与 adaptive 交还上游默认

请求里的 `thinking` 取值是 `auto` 或 `adaptive` 时，网关 MUST 删除整个字段，交还上游默认（省略字段时 K3 正常思考），
记录规则 `provider.kimi.thinking-upstream-default`。标准的 `enabled` / `disabled` 连同 `budget_tokens` MUST 原样透传，effort 类字段一概不碰。
本渠道 MUST NOT 引入 thinking 续写机制：该端点接受 thinking 块被剥离的历史（含带 `tool_use` 的消息）。

#### Scenario: Science 发来 auto

- **WHEN** 请求带 `thinking: {"type":"auto"}` 或 `{"type":"adaptive"}`
- **THEN** 发往上游的请求没有 `thinking` 字段，规则日志含 `provider.kimi.thinking-upstream-default`

#### Scenario: 标准取值

- **WHEN** 请求带 `thinking: enabled`（含 `budget_tokens`）或 `disabled`
- **THEN** 字段原样透传

### Requirement: 指定工具型 tool_choice 关闭思考

仅当 `tool_choice.type == "tool"` 时，网关 MUST 置 `thinking: {"type":"disabled"}` 并保留强制工具本身（分类器依赖拿到结构化输出），
记录规则 `provider.kimi.specified-tool-choice-disables-thinking`。`any` 与 `auto` 上游无此问题，MUST NOT 做任何补偿。

#### Scenario: Science 的工作项分类器

- **WHEN** 请求带 `tool_choice: {"type":"tool","name":"create_work_item"}`，且不带 `thinking` 字段
- **THEN** 发往上游的请求带 `thinking: disabled`，`tool_choice` 不变

#### Scenario: any 或 auto

- **WHEN** `tool_choice` 是 `any` 或 `auto`
- **THEN** 请求不被改写

### Requirement: 历史里的搜索结果配对修复

请求侧 MUST 让历史里的 `server_tool_use` 与 `web_search_tool_result` 配得上，规则 `provider.kimi.web-search-result-pairing-repair`。
它对 Kimi 流量无条件运行（与联网搜索总开关无关）。

| 历史形态 | 动作 |
| --- | --- |
| 结果块孤立（无 `server_tool_use`） | 在它前面补一个同 id 的 `server_tool_use` |
| 两块都在但 id 不匹配 | 把结果块的 id 改成前面最近的 `server_tool_use.id` |
| 已经配得上 | 不动，也不记规则 |
| 连 `tool_use_id` 都没有，且内容为空 | 整块删除（零证据损失） |
| 连 `tool_use_id` 都没有，但有内容 | 合成确定性配对键补上两半 |

选择补块而不是丢块：丢掉结果块同样能过，但会丢失这一轮的搜索证据。

#### Scenario: 孤立的结果块

- **WHEN** 历史里某条 assistant 消息有 `web_search_tool_result` 而没有与之配对的 `server_tool_use`
- **THEN** 它前面被补上一个同 id 的 `server_tool_use`，上游不再回 `400 tool_call_id  is not found`

#### Scenario: 两块 id 不匹配

- **WHEN** 两块都在但 id 不同
- **THEN** 结果块的 id 被改成前面最近的 `server_tool_use.id`

#### Scenario: 已经配得上

- **WHEN** 历史里的搜索对 id 一致
- **THEN** 不改写，也不记规则

#### Scenario: 无 id 的空孤儿

- **WHEN** 结果块没有 `tool_use_id` 且内容为空
- **THEN** 整块被删除

#### Scenario: 无 id 但有内容的孤儿

- **WHEN** 结果块没有 `tool_use_id` 但有内容
- **THEN** 两半被补上合成的确定性配对键

### Requirement: 搜索轮噪声头剥离

上游对声明了原生 web_search 的轮次会注入内容恒为 `Search results for query: <查询词>` 的独立 text 块。
流式与非流式 MUST 都把该 text 块整块剥掉，规则 `provider.kimi.search-noise-text-strip`（`kimi_search_noise.rs`）。
流式实现按块缓冲到凑满前缀即拍板；被吞的块 MUST NOT 占输出索引（Science 要求 index 连续无空洞）；未命中的流量 MUST 字节级零改写。

#### Scenario: 搜索前的噪声头

- **WHEN** 上游在 `server_tool_use` 之前发了一个 `Search results for query:` 开头的 text 块
- **THEN** 该块被剥掉，回答开头即正文，输出索引连续，日志记 `noise=1`

#### Scenario: turn 末尾的悬挂噪声头

- **WHEN** 上游在 turn 末尾再发一个噪声头后直接 `end_turn`
- **THEN** 该块同样被剥掉

#### Scenario: 没有噪声头

- **WHEN** 响应里没有这样的 text 块
- **THEN** 响应字节级原样

### Requirement: document 块换成署名占位文本

上游的 Anthropic 兼容层没有实现 `document` 块，而该块会留在对话历史里，一个附件会让此后每一轮都失败。
网关 MUST 把每个 `document` 块替换为一段署名的说明文本，规则 `provider.kimi.document-block-placeholder`。
文本 MUST 注明 media type、明确标注来自 CSSwitch 网关而不是工具返回，并给出「读文件并渲染成图像」的可行替代。`image` 块 MUST NOT 受影响。

#### Scenario: 历史里有 PDF 附件

- **WHEN** 请求的某条消息带 `document` 块
- **THEN** 该块被替换为署名的占位文本，这一轮得以继续而不是整轮失败

#### Scenario: image 块

- **WHEN** 请求带 `image` 块
- **THEN** 它原样到达上游

### Requirement: 不改写 Science 自己的历史压缩

搜索轮的下一轮里，Science 会把上一轮的搜索结果存盘瘦身，并在 user 消息里附一段 `[System] Prior-turn server_tool(…) — results persisted.` 说明。
这是 Science 平台自己的历史压缩行为，与渠道无关（官方渠道同样出现），CSSwitch MUST NOT 改写它。

#### Scenario: 历史里的 Prior-turn 说明

- **WHEN** user 消息带 `[System] Prior-turn server_tool(…) — results persisted.` 文本
- **THEN** 它原样到达上游
