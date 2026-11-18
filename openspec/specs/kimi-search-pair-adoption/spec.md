# kimi-search-pair-adoption Specification

## Purpose

Science 按 `server_tool_use` 与 `web_search_tool_result` 两块的配对键决定是否保留搜索卡片；键不一致会让搜索框和查询词在流结束后消失，
并使后续请求依赖历史配对修复。本规格约束网关如何把 Kimi 原生 web_search 响应里的配对键归一，以及幻影对与无键对的处理。
Kimi 响应出现相邻的 `server_tool_use` + `web_search_tool_result` 时适用，流式与非流式同判。

## Requirements

### Requirement: 采用上游已有的配对键

配对键归一 MUST 只采用上游已有的键，记录规则 `provider.kimi.search-pair-id-adopt`：

- `result.tool_use_id = K` 存在且与 `use.id` 不同时，只把 `use.id` 改为 `K`。
- 只有 `use.id` 存在时，反向补到 `result.tool_use_id`。

归一 MUST NOT 改 `input.query`、结果内容或块索引。全部组合的判定矩阵见 [design.md](./design.md)。

#### Scenario: 两侧键不一致

- **WHEN** `use.id`（如 `tool_*`）与 `result.tool_use_id`（如 `srvtoolu_*`）不一致
- **THEN** `use.id` 改为结果侧的键，`adopted_pairs` 加一
- **AND** 查询输入、结果内容与块索引不变

#### Scenario: 只有 use 侧有键

- **WHEN** `use.id` 存在而 `result.tool_use_id` 缺失
- **THEN** `result.tool_use_id` 补成 `use.id`，`adopted_pairs` 加一

#### Scenario: 已经配对

- **WHEN** 两侧键相同
- **THEN** 字节级原样放行，零命中

#### Scenario: 归一后的用户可见结果

- **WHEN** 键被归一后流结束并整页重载
- **THEN** 查询词仍可见，下一轮不再触发 `web-search-result-pairing-repair`

### Requirement: 只有 web_search 的 server tool 参与

只有前置块 `type=server_tool_use, name=web_search` 才能参与采钥 / 幻影判定；其它 server tool MUST 原样。兄弟 provider 不进入过滤器。

#### Scenario: 其它 server tool

- **WHEN** 相邻块里的 server tool 不是 `web_search`
- **THEN** 两块原样放行

### Requirement: 幻影对的判定

两半都无键且 result 的 `content` **存在、为数组并显式 `[]`** 才是幻影对，SHALL 剥离整对（规则 `provider.kimi.empty-search-pair-strip`）。
字段缺失、null、string、object 等 schema 异常 MUST NOT 按空数组删除。带键的空结果是合法零结果搜索，MUST 保留。

#### Scenario: 无键且结果是显式空数组

- **WHEN** 两半都无键，`result.content` 是 `[]`
- **THEN** 整对被剥离，`pair_blocks` 加二

#### Scenario: 无键且 content 异常

- **WHEN** 两半都无键，`result.content` 缺失或不是数组
- **THEN** 原样放行或显式协议失败，不当作幻影对删除

#### Scenario: 带键的空结果

- **WHEN** 配对有键而结果为空
- **THEN** 保留（合法的零结果搜索）

### Requirement: 不为无键对发明身份

两半都无键但结果非空时 MUST NOT 发明键：原样放行并计 `unkeyed_pairs`。

#### Scenario: 无键非空

- **WHEN** 两半都无键而结果非空
- **THEN** 原样放行，`unkeyed_pairs` 加一

### Requirement: 改写与日志如实对应

`adopted_pairs > 0` 时非流式 body MUST 重序列化，规则日志含 `search-pair-id-adopt` 与 `adopted=N`。
`unkeyed_pairs` 单独出现时是零改写，`rules=-`，但日志 MUST 仍记 `unkeyed=N`。

#### Scenario: 非流式路径有采钥

- **WHEN** 非流式响应里 `adopted_pairs > 0`
- **THEN** body 被重序列化，规则日志含 `search-pair-id-adopt` 与 `adopted=N`

#### Scenario: 只有无键对

- **WHEN** 响应里只有无键非空对
- **THEN** 零改写，`rules=-`，日志含 `unkeyed=N`

### Requirement: 真机验收要确认搜索语义

真机验收 MUST 既确认卡片在整页重载后仍在，也由人工确认查询与结果语义相关。
「无 400 / 有 Web Search 卡片」不足以证明搜索功能正确。

#### Scenario: 真机验收

- **WHEN** 用全新会话跑「显式搜索 → 追问 → 不搜索 → 再追问」后整页重载
- **THEN** 无 400 / `upstream_failure`、无历史 pairing repair，卡片仍在，且人工核对查询语义正确
