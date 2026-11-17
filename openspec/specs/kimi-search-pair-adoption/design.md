# kimi-search-pair-adoption 设计注记

## 签名

```text
SearchNoiseFilter::feed(&[u8]) -> Result<Vec<u8>, String>  # 流式
strip_nonstream_noise(&mut Value) -> StripStats            # 非流式
StripStats::{any_activity, rewrote_body}
RULE_PROVIDER_KIMI_SEARCH_PAIR_ID_ADOPT =
  "provider.kimi.search-pair-id-adopt"
```

## 判定矩阵

| use.id | result.tool_use_id | result.content | 行为 |
| --- | --- | --- | --- |
| 无或不等于 `K` | `K` | 任意 | `use.id := K`，`adopted_pairs += 1` |
| `U` | 无 | 任意 | `result.tool_use_id := U`，`adopted_pairs += 1` |
| 无 | 无 | 显式数组 `[]` | 剥离整对，`pair_blocks += 2` |
| 无 | 无 | 缺失/非数组 | 原样或显式协议失败，不当幻影 |
| 无 | 无 | 非空 | 原样放行，`unkeyed_pairs += 1` |
| `K` | `K` | 任意 | 字节级原样放行，零命中 |

## 理由与教训

- 基线：已经配对的搜索对保持原字节，兄弟 provider 不进入过滤器。
- 错误：为无键非空对生成新 ID，或只在流式路径修复，导致非流式仍丢卡片。

## 必需测试

- 流式与非流式都覆盖：双键不匹配、反向采钥、带键空结果、无键非空、已配对零改写、
  malformed content 与非 Web Search server tool 负例。
- 断言采钥不改变块索引、查询输入与结果内容；非流式 `adopted_pairs > 0` 后 body 被重序列化。
- 断言规则日志包含 `search-pair-id-adopt` 与 `adopted=N`；只有无钥对时 `rules=-`
  且包含 `unkeyed=N`。
- 真机用全新会话跑“显式搜索 → 追问 → 不搜索 → 再追问”，然后整页重载；
  同时检查无 400 / `upstream_failure`、无历史 pairing repair，并人工核对查询语义。

## Wrong vs Correct

### Wrong

```text
看到相邻搜索对 → 无条件生成统一的新 ID → 卡片看似恢复，但伪造了上游身份且掩盖无键异常
```

### Correct

```text
优先采用结果侧现有键；仅单侧有键时向另一侧补齐；两侧无键则不发明身份
```
