# kimi-context-tail-reorder Specification

## Purpose

真实 Science 在用户问题后追加独立 `role=user` 的 compute snapshot；Kimi 主调用的 query planner 会把最后一条 user 当搜索主题。
网关在精确的 Science 签名下交换末尾两条 user 消息的顺序，让上游首次生成的查询就是用户的问题。
本规格约束交换的触发条件与边界。direct A/B 必须只交换两条消息顺序，以排除响应过滤器、system 内容与工具声明等变量。

## Requirements

### Requirement: 只在 Kimi 且声明了 typed 搜索时生效

重排 MUST 仅在 Kimi + typed `web_search_*` 时生效，发生在 typed → 私有 query tool 之前
（见 [kimi-web-search-bridge](../kimi-web-search-bridge/spec.md)）。DeepSeek 与 Generic MUST 零改写。

#### Scenario: 兄弟 provider 或 client tool

- **WHEN** 请求走 DeepSeek / Generic，或搜索是 client tool 而非 typed search
- **THEN** 消息原样

### Requirement: 只交换末尾相邻两条纯文本 user 消息

只看末尾相邻两条 user message，且两条都 MUST 是 string 或全 text blocks。前一条含 `tool_result` 等非 text 块时 MUST NOT 交换，
避免拆断 assistant / tool_result 历史配对。

#### Scenario: 前条或末条含非 text 块

- **WHEN** 末尾两条 user 消息里有一条含非 text 块（例如历史工具结果）
- **THEN** 消息原样

#### Scenario: 单条消息或普通连续 user

- **WHEN** 只有单条 compute 问题，或末尾是普通的两条连续 user 消息
- **THEN** 消息保持顺序，规则不记入日志

### Requirement: 末条必须是精确的 compute snapshot 签名

末条 MUST 大小写无关地包含 `compute snapshot`，并且：同时含独立词 `cores` 与独立词 `RAM`，或含带数字容量的 `GiB`（`32GiB` / `32 GiB`）。
裸 `GiB` 与 `hardcores` MUST NOT 命中。

#### Scenario: 精确的 compute tail

- **WHEN** 请求声明了 typed search，末尾是「文本问题 + 精确 compute snapshot」
- **THEN** 末两条被交换：`[Rust 问题, compute snapshot]` → `[compute snapshot, Rust 问题]`，并记录规则

#### Scenario: 近似但不命中的词

- **WHEN** 末条只含 `program`、`hardcores` 或裸 `GiB`
- **THEN** 消息原样

### Requirement: 交换完整消息且不改写内容

交换的 MUST 是完整的 `Value`，MUST NOT 删除、重建或改写正文，更 MUST NOT 在响应后伪造 query。

#### Scenario: 交换前后的内容

- **WHEN** 发生交换
- **THEN** 两条消息的内容逐值相等，只有顺序变了

### Requirement: 只有实际交换才记录规则

只有实际交换才 SHALL 记录规则 ID `provider.kimi.science-context-tail-reorder`；所有边界外形态的 `rule_ids` MUST NOT 出现该规则。

#### Scenario: 边界外形态

- **WHEN** 请求不满足交换条件
- **THEN** `rule_ids` 里没有 `provider.kimi.science-context-tail-reorder`
