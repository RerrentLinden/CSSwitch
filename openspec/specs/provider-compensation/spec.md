# provider-compensation Specification

## Purpose

补偿的本质是「上游拒收某个组合，我们改成它能接受的形状」。本规格约束 provider 专属请求改写的判据范围、策略来源与规则日志，
保证补偿只落在确实会被拒收的形态上，并且日志能如实回答「哪条补偿正在起效」。
新增或修改 provider 专属的请求改写时适用。判定上游能力的方法见
[backend quality guidelines](../../conventions/backend/quality-guidelines.md)「判定上游是否真的不支持某能力」。

## Requirements

### Requirement: 补偿的统一形态

补偿 MUST 都是窄规则，统一形态：端点 / flavor 门控 + 规则 ID + 单测。命中的规则 MUST 打进请求日志，便于判断「哪条在起效」。

#### Scenario: 一条补偿命中

- **WHEN** 请求命中某条补偿
- **THEN** 请求日志里有这条补偿的规则 ID

#### Scenario: 兄弟渠道的请求

- **WHEN** 请求走的是另一个 provider
- **THEN** 这条补偿不生效

### Requirement: 判据等于实测拒收边界

补偿的判据 MUST 由实测边界决定。从其它项目移植语义时，MUST 独立复测边界再落判据；
边界之外的形态 MUST NOT 被触碰。

| 照搬的判据 | 实测边界 | 代价 |
| --- | --- | --- |
| 任何非 `none` 的 `tool_choice` 都禁思考 | 只有 `{"type":"tool"}` 会 400；`auto` / `any` 完全正常 | `auto` 是带工具时最普通的形态——等于每轮都白白关掉推理 |

#### Scenario: 上游能接受的组合

- **WHEN** 请求是 `thinking enabled + budget + effort`，上游能接受
- **THEN** 原样透传，日志 `rules=-`

#### Scenario: 指定工具型 tool_choice

- **WHEN** `tool_choice` 是 `{"type":"tool"}`
- **THEN** thinking 被定为 `disabled`

#### Scenario: auto 或 any 的 tool_choice

- **WHEN** `tool_choice` 是 `auto` 或 `any`
- **THEN** thinking 不被触碰

### Requirement: 策略来自 provider contract

`thinking_policy` 等策略字段的权威来源 MUST 是 `catalog/provider-contracts.v1.json`，由 `GatewayConfig` 从契约读取。
环境变量只能作为**显式 override**，且 MUST 留日志。

#### Scenario: 没有注入任何环境变量

- **WHEN** 启动路径没有注入任何策略类环境变量
- **THEN** 契约里声明的策略照常生效

#### Scenario: 环境变量覆盖策略

- **WHEN** 环境变量显式覆盖了契约里的策略
- **THEN** 覆盖生效，并在日志里留下记录

### Requirement: 规则日志只记净效果

规则 ID 列表的用途是回答「哪条补偿正在起效」。被后续规则覆盖、净效果为零的改写 MUST NOT 记入。
优先级高的规则 SHALL 先判并短路。

#### Scenario: 指定工具型 tool_choice 覆盖 thinking 归一化

- **WHEN** 指定工具型 `tool_choice` 把 thinking 直接定为 `disabled`
- **THEN** thinking 取值的归一化不再执行，它的规则 ID 不出现在 `rule_ids` 里

#### Scenario: 只有会被拒收的组合被改写

- **WHEN** 一个请求经过补偿链
- **THEN** 日志里的每条规则都确有净效果
