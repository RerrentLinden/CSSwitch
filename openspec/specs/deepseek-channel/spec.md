# deepseek-channel Specification

## Purpose

DeepSeek 渠道把 Claude Science 的推理流量转到 DeepSeek 官方的 Anthropic 兼容端点 `https://api.deepseek.com/anthropic`。
它走与 Kimi 相同的契约驱动中继路径，差异全在 `RelayFlavor::DeepSeek` 上。本规格约束这个渠道的契约、默认模型槽、六条补偿、
thinking / effort 的处理边界与模型探测。补偿实现在 `desktop/gateway/src/deepseek_compat.rs`，每条带规则 ID 与单测。
补偿的通用规则见 [provider-compensation](../provider-compensation/spec.md)。

## Requirements

### Requirement: 后端契约

DeepSeek MUST 走下表的契约：

| 项 | 值 |
| --- | --- |
| contract id | `deepseek-native` |
| adapter / transport | `deepseek` / `anthropic_messages` |
| 鉴权 | `x-api-key` |
| 端点拼接 | `anthropic_v1`（base + `/v1/messages`） |
| thinking policy | `deepseek_native` |

#### Scenario: 发往上游的推理请求

- **WHEN** DeepSeek 渠道转发一次推理请求
- **THEN** 请求发往 base + `/v1/messages`，凭证在 `x-api-key` 头里

### Requirement: 默认模型槽

默认模型预设 MUST 是 `deepseek-v4-pro`（默认 / 高质量 / Fable）与 `deepseek-v4-flash`（快速）。四个槽都可在 WebUI 里改。

#### Scenario: 出厂配置

- **WHEN** 用户没有改过 DeepSeek 的模型槽
- **THEN** 默认、高质量、Fable 是 `deepseek-v4-pro`，快速是 `deepseek-v4-flash`

### Requirement: thinking 的 auto 改写为 adaptive

上游的请求体反序列化只认 `adaptive` / `enabled` / `disabled`，Claude Science 发的非标准 `auto` 直接 400。
网关 MUST 把 `auto` 改写为 `adaptive`（语义最近），规则 `provider.deepseek.thinking-auto-adaptive`。

#### Scenario: Science 发来 auto

- **WHEN** 请求带 `thinking: {"type":"auto"}`
- **THEN** 发往上游的是 `adaptive`，规则日志含 `thinking-auto-adaptive`

### Requirement: 指定工具型 tool_choice 关闭思考

上游在思考开启时拒收**指定工具型** tool_choice（`{"type":"tool","name":…}`），`adaptive` 下同样拒收。
网关 MUST 保留 tool_choice（Science 的抽取请求依赖结构化输出），把 thinking 置为 `disabled` 并剥掉 effort，
规则 `provider.deepseek.specified-tool-choice-disables-thinking`。`tool_choice` 为 auto / any / none 时 MUST NOT 改动，thinking 照常。

#### Scenario: 辅助调用带指定工具

- **WHEN** 请求带 `tool_choice: {"type":"tool",…}`
- **THEN** 发往上游的请求 thinking 是 `disabled`、没有 effort，`tool_choice` 不变

#### Scenario: auto、any 或 none

- **WHEN** `tool_choice` 是 auto、any 或 none
- **THEN** 请求不被改写

### Requirement: 两条 thinking 规则同时命中时 tool_choice 优先

`auto` 与指定工具同时出现时，tool_choice MUST 优先：thinking 直接压成 `disabled`，`auto→adaptive` 不再执行，也 MUST NOT 记入规则日志
——否则日志会显示一条净效果为零的改写，误导排查。这里 MUST 是 `disabled` 而不是 `adaptive`。

#### Scenario: auto 加指定工具

- **WHEN** 请求同时带 `thinking: auto` 与指定工具型 `tool_choice`
- **THEN** thinking 是 `disabled`，规则日志只有 `specified-tool-choice-disables-thinking`

### Requirement: thinking disabled 时剥掉 effort

上游把 `thinking: disabled` 与 effort 参数视为互斥，同时出现 400。网关 MUST 尊重客户端的 disabled 并剥掉冲突的 effort，
规则 `provider.deepseek.thinking-disabled-strips-effort`。

#### Scenario: disabled 加 effort

- **WHEN** 请求带 `thinking: disabled` 与 effort 参数
- **THEN** thinking 保持 `disabled`，effort 被剥掉

### Requirement: 只改写上游会拒收的 thinking 组合

只有上游会拒收的组合才 SHALL 被改写；能过的 MUST 原样送。

#### Scenario: enabled 加预算与 effort

- **WHEN** 请求带 `thinking: enabled` + `budget_tokens` + `reasoning_effort`
- **THEN** 原样透传，零改动，不命中任何规则

### Requirement: 带工具的历史补齐 thinking

上游要求带 `tool_use` 的历史助手轮回传 thinking，客户端常剥掉，结果是 `must be passed back` 400。
网关 MUST 无状态补齐：缺失时插占位块，`redacted_thinking` 转普通 thinking，去掉过不了校验的 signature，
规则 `provider.deepseek.tool-thinking-history-replay`。MUST NOT 在 CSSwitch 侧保存任何对话状态。

#### Scenario: 多轮工具调用

- **WHEN** 历史里带 `tool_use` 的助手轮没有 thinking 块
- **THEN** 该轮被补上占位 thinking 块，上游不再回 400

#### Scenario: 首轮或没有 tool_use 的调用

- **WHEN** 历史里没有带 `tool_use` 的助手轮
- **THEN** 规则不命中

### Requirement: 畸形的 server tool 块修复

上游的严格反序列化拒收缺 `tool_use_id` 的 `web_search_tool_result`（Science daemon 会把这种块落盘进历史）。
网关 MUST 在能抽文本时把它降级成文本，否则丢弃，规则 `provider.deepseek.malformed-server-tool-block-repair`。
结构完好的 server tool 块 MUST 保留——DeepSeek 原生执行 web_search，降级会教模型模仿扁平化的伪工具调用。

#### Scenario: 缺 tool_use_id 的结果块

- **WHEN** 历史里有缺 `tool_use_id` 的 `web_search_tool_result`
- **THEN** 能抽出文本就降级成文本，否则丢弃

#### Scenario: 结构完好的 server tool 块

- **WHEN** 历史里的 server tool 块结构完好
- **THEN** 它被保留（日志里是 `tool.deepseek.web_search.server-tool-preserve`）

### Requirement: 落单的工具调用配对修复

上游按 id 一对一配对 tool_use / tool_result，落单即拒。网关 MUST 给未应答的 tool_use 按**计数差额**补合成 error 结果
（并行调用可能共用 id，集合去重会漏补）；找不到对应 tool_use 的 tool_result MUST 降级成文本。
规则 `provider.deepseek.orphan-tool-pairing-repair`。

#### Scenario: 未应答的 tool_use

- **WHEN** 历史里有 tool_use 没有对应的 tool_result
- **THEN** 按计数差额补上合成的 error 结果

#### Scenario: 找不到 tool_use 的 tool_result

- **WHEN** 历史里有 tool_result 找不到对应的 tool_use
- **THEN** 它被降级成文本

### Requirement: max_tokens 上限钳制

`max_tokens` MUST 被钳制到上限：pro 65536、flash 32768。超限上游直接拒。

#### Scenario: 请求超过上限

- **WHEN** 请求的 `max_tokens` 超过该模型的上限
- **THEN** 发往上游的值等于上限

### Requirement: 模型清单探测两个地址

DeepSeek 的模型清单不在 `/anthropic` 路径下（`/anthropic/v1/models` 返回 404 空体），在根域 `/v1/models`。
控制台的模型探测 MUST 依次尝试这两个地址。

#### Scenario: 获取可用模型

- **WHEN** 用户在 DeepSeek 渠道点「获取可用模型」
- **THEN** 探测依次尝试 `/anthropic/v1/models` 与根域 `/v1/models` 这两个地址
