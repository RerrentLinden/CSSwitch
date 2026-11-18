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

## 幻影空搜索对（原 §3c）

下文沿用原渠道文档的小节编号（§3、§3b–§3e），对照见 [kimi-channel](../kimi-channel/design.md)「原章节对照」。

模型被告知"无需搜索"时,上游仍常发一对空壳:`server_tool_use` **连 id 都没有**、
无 input,紧跟 `content: []` 的 `web_search_tool_result`(同样无 tool_use_id)。
真实会话与探测中,不搜索的轮次几乎每轮都带这一对。

危害有两层:Science 把它渲染成一个空的 Server Tool 框;落盘后只剩无 id 的孤儿
结果块,**此后每一轮都 400 `tool_call_id is not found`**(真实会话第 3 轮当场复现,
Science 侧表现为 Agent Failed + 中断,这正是配对修复表里"无 id 孤儿"一行的来源)。

补偿:响应侧规则 `provider.kimi.empty-search-pair-strip` 只把"前置 `name=web_search`、
两半都无键、结果 `content` 字段存在且精确为数组 `[]`"的整对剥掉；缺失/string/object/null
不按空结果删除。请求侧
`web-search-result-pairing-repair` 的无 id 分支负责救活修复落地前已中毒的历史
(实测同一会话 Resume 后 400 → 200)。**带键的真实零结果搜索不剥离**;
两半键不一致时由 §3d 的采钥规则归一后保留。

> 另:搜索轮的下一轮里,Science 会把上一轮的搜索结果存盘瘦身,并在 user 消息里
> 附一段 `[System] Prior-turn server_tool(…) — results persisted.` 说明,UI 渲染成
> 一个输出为空的 Server Tool 折叠框。**这是 Science 平台自己的历史压缩行为**,
> 与渠道无关(官方渠道同样出现),CSSwitch 不改写它。

## 3c/3d 只发生在流式路径（2026-09-16 直连取证）

一次退役评估里,非流式探针得出了"上游已修"的错误结论。同一问题、同一模型、只切
`stream`,两条路径的形态完全不同:

| 轮次形态 | `stream: false` | `stream: true` |
| --- | --- | --- |
| 搜索轮的配对键 | 两半都是 `srvtoolu_…`,**恒匹配**(23/23) | `tool_…` vs `srvtoolu_…`,**恒不匹配** |
| 不搜索轮的幻影对 | **0 次**(12 轮) | **每轮都有**(无键 `server_tool_use` + 空 `web_search_tool_result`) |
| 搜索轮噪声头 | 每轮都有 | 每轮都有 |

Science 全程走流式,所以 §3b / §3c / §3d 三条补偿一条都不能退役。真机日志同期佐证:
`adopted=1`(采钥开火)与 `pair=2`(幻影剥离开火)都出现在 `relay stream` 行上,
`relay nonstream` 行则恒为 `adopted=0 pair=0`。

**方法论**:验一条 Kimi 补偿能否退役,探针必须用 `stream: true`,并且最终要在真实 Science 里复现——隔离探针的历史形态和 Science 不同(Science 会删掉历史搜索块、附 `Prior-turn` 说明、在同一条 user 消息里拼 skill_discovery 与 compute snapshot),直连多轮探针 8/8 复现不出上面的问题。真实会话用 `CSSWITCH_DEBUG_CAPTURE_DIR=<目录>` 抓包:每条 relay 请求落盘 Science 原始请求、上游请求、上游原始响应与回给 Science 的响应(文件含完整对话正文,权限 0600,排障后删除;桥接的内部 nested 调用不落盘)。非流式通过不构成证据——
这是继"只验报错不验功能"之后,同一条退役路径上踩到的第二个坑。

## 搜索对配对键采钥的成因（原 §3d）

上面已经出现过两次的事实:上游自己发出的真实搜索对,两半配对键**恒不匹配**
(`server_tool_use.id` 为 `tool_…`,`web_search_tool_result.tool_use_id` 为
`srvtoolu_…`)。原样放行时,Science 落盘会丢弃无法配对的 `server_tool_use`,
只留孤儿结果块——**流式期间可见的 Web Search 卡片(含查询词)在流结束后消失**,
且此后每一轮请求都要靠请求侧配对修复以 `input: {}` 空壳兜底(2026-08-19
真机复现,live 证据 D5)。

补偿:规则 `provider.kimi.search-pair-id-adopt`,响应侧在放行真实搜索对之前把
`server_tool_use.id` 改写为同对结果块的 `tool_use_id`——**只采用上游已有的
`srvtoolu_…` 键,不发明新键**;result 侧无键而 use 侧有键时反向补齐。
两半都无键的非空对不归一,如实放行、仅记日志(`unkeyed=N`)。带键的空结果
被判定为真实零结果搜索,走采钥保留(这同时收窄了 §3c 幻影剥离的判据:
幻影 = 两半都无键 **且** 内容为空)。流式与非流式同一矩阵,采钥不改变块数与
索引,命中记日志 `adopted=N`。

归一之后 Science 可正常配对、落盘、渲染,查询词保留;§3 的请求侧配对修复
对**新产生的**轮次不再必要,继续保留用于救活历史存量。
