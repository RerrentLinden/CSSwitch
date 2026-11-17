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
