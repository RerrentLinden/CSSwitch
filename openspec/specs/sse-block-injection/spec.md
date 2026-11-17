# sse-block-injection Specification

## Purpose

网关有时要在 Anthropic SSE 流里合成或改写 `content_block_*` 帧（例如补发上游调用后把结果拼进同一条消息）。
本规格约束注入的位置、帧形状与失败处理，保证规范客户端还原出的内容与网关想表达的完全一致。
在 gateway 内合成或改写 `content_block_*` 帧时适用。

## Requirements

### Requirement: 注入发生在过滤流内部

注入 MUST 发生在过滤流内部。输出流生命周期校验器夹在 filter 与 socket 之间，`message_stop` 被扣留至干净 EOF；
需要发起上游调用的过滤器 SHALL 自持 `GatewayConfig` / transport 的克隆，内联完成调用。

#### Scenario: 过滤器内联完成补发调用

- **WHEN** 过滤器需要补发一次上游调用并把结果拼进当前消息
- **THEN** 调用与注入都在过滤流内部完成，注入块出现在 `message_delta` 之前
- **AND** `message_stop` 之后不出现任何注入块

#### Scenario: 在转发循环结束后追加帧

- **WHEN** 帧在转发循环结束后才被追加
- **THEN** 校验器判为流被截断（`SSE stream ended before message_stop`），以终止 error 结束（`upstream SSE protocol error`）

#### Scenario: 不注入

- **WHEN** 本轮没有要注入的内容
- **THEN** 上游流原样透传

### Requirement: 增量字段以 delta 下发

凡上游按增量下发的字段，注入时 MUST 发 delta：`content_block_start` 只给块壳，内容走 delta。
规范客户端从 start 帧取块壳、再累积 delta；只放在 start 帧里的内容会被丢弃。

| 字段 | delta 类型 |
| --- | --- |
| `text` | `text_delta` |
| `thinking` | `thinking_delta` |
| `signature` | `signature_delta` |
| `server_tool_use.input` | `input_json_delta` |

`web_search_tool_result` 无 delta 形式，整块放在 start 帧是正确的。

#### Scenario: 规范客户端还原注入块

- **WHEN** 客户端从 start 帧取块壳并清空流式字段，仅靠 delta 累积
- **THEN** 还原结果与原块完全相等

#### Scenario: payload 只放在 start 帧

- **WHEN** 注入块的内容只出现在 `content_block_start`
- **THEN** 客户端还原为空块，并会在下一轮把残缺块回传给上游，可能触发上游 400（这是被禁止的形状）

#### Scenario: 注入搜索结果块

- **WHEN** 注入 `web_search_tool_result`
- **THEN** 整块放在 start 帧，不发 delta

### Requirement: 输出块索引连续

输出的 content block index MUST 连续无空洞。被吞掉的块不占用输出索引；注入块从当前输出索引继续。

#### Scenario: 吞掉上游块后注入

- **WHEN** 过滤器吞掉若干上游块后注入新块
- **THEN** 输出索引从当前位置连续递增，没有空洞

#### Scenario: 输出索引出现空洞

- **WHEN** 输出流的 block index 不连续
- **THEN** 校验器判定 block index 非法

### Requirement: 替换终止帧时保留上游 message_stop

替换上游终止帧时 MUST 只替换 `message_delta`，`message_stop` SHALL 仍用上游的，以保持校验器的生命周期判定。

#### Scenario: 注入后收尾

- **WHEN** 注入改变了消息的终止信息
- **THEN** 网关替换 `message_delta`，随后放行上游的 `message_stop`

### Requirement: 注入失败以终止 error 呈现

注入所需的上游调用失败时，网关 MUST 发终止 SSE error，MUST NOT 伪造助手内容。

#### Scenario: 补发调用失败

- **WHEN** 注入所需的上游调用失败
- **THEN** 流以终止 error 结束，不产出任何合成的助手内容
