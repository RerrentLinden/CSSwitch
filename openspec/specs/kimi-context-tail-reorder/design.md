# kimi-context-tail-reorder 设计注记

## 签名

```text
reorder_science_context_before_user_intent(&mut Value, &mut Vec<String>)
RULE_PROVIDER_KIMI_SCIENCE_CONTEXT_TAIL_REORDER =
  "provider.kimi.science-context-tail-reorder"
```

## 理由与教训

- 正常：交换后真机首张卡片直接搜索 Rust。
- 错误：只因出现硬件词就全局移动上下文，或把 compute context 插进 `assistant tool_use → user tool_result` 中间。

## 必需测试

- 用真实 Science 脱敏形状断言顺序、正文与规则 ID。
- 反向覆盖：单消息、普通双 user、大小写与单词边界、裸/带数字 GiB、非 text、
  历史工具结果、client tool、DeepSeek、Generic。
- 真机全新会话至少覆盖两次不同主题搜索、历史复述、不联网轮与整页重载；同时确认
  query 语义正确、`search-pair-id-adopt adopted=1`，且无 repair / 400 / upstream failure。

## Wrong vs Correct

### Wrong

```text
响应卡片主题错 → 把 query 文本改成用户问题 → 结果仍来自错误搜索，只是 UI 看似正确
```

### Correct

```text
direct A/B 证明尾部消息顺序敏感 → 仅在精确 Science 签名下交换完整消息 → 上游首次生成正确 query
```

## 成因与真机证据（原 §3e）

真实 Science 会把用户问题与本机 `compute snapshot` 放成末尾相邻的两条 user message,
机器上下文在后。direct K3 A/B 保持正文不变只交换两条顺序时,原顺序让主 query planner 选择机器规格,
反向顺序稳定选择用户的 Rust 主题,因此问题位于 Kimi 主 query planner 对消息顺序的解释。

规则 `provider.kimi.science-context-tail-reorder` 只在 Kimi 请求声明 typed web_search、
末尾两条都是 user、两条均为 text-only,且最后一条大小写无关命中
`compute snapshot`、独立词 `cores` 与独立词 `RAM` 或带数字的 GiB 容量规格时,
交换两条完整消息。它不删除或重建正文,也不改写 query；普通连续 user、历史工具结果、
client tool 与兄弟 provider 均保持原样。真机候选在同一 K3 会话完成 Rust 搜索、
历史复述、Python 搜索、跨搜索不联网比较与两轮连续计算；两张 query/card 和最终结果
整页重载后均保留。该重排现在发生在 typed search 映射为私有 query tool 之前；bridge 搜索轮
与响应整形正常叠加。
