# kimi-web-search-bridge 设计注记

## 签名

```text
kimi_web_search_adapter::prepare_request(&mut Value, RelayFlavor)
kimi_web_search_adapter::resolve_with(main, prepared, first, upstream_call)
kimi_web_search_adapter::render_stream(&Value) -> Result<Vec<u8>, String>
messages::inference_deadline(&GatewayConfig) -> Instant
messages::post_nonstream_before(..., deadline)
RULE_PROVIDER_KIMI_WEB_SEARCH_QUERY_TOOL_ADAPTER =
  "provider.kimi.web-search.query-tool-adapter"
```

## 理由与教训

- 正常：Science K2.7 搜索 → query tool → forced nested → 原生卡片 + 正文；下一轮不搜索 `bridged=0/upstream_calls=1`。
- 基线：Kimi 普通轮即使声明 typed search，也由模型决定不调用私有 tool，零 nested 开销。
- 错误：每轮强制 server search；拿完整用户 prompt 当 query；nested 失败后改走 Bash；把网页正文当指令；
  或对下游发送两套 message lifecycle。

## 已知边界（2026-08-20）

Science 的历史规整会在会话每推进一轮后,把**非最新** assistant 消息裁剪为纯 text（thinking 与 server 搜索对被追溯剥离,
`frame_messages` 可证）。桥接路径的搜索卡片因此只在「自己还是最新轮」期间可见;
原生 typed 流式路径的卡片则由 Science 的持久转录层跨轮保留（2026-08-19 R6 六轮验收可证）。
两者在所有可黑盒检视的线上维度（块序、`srvtoolu` 键前缀、usage 透传、shell 空 input + delta 单发）等形,
Science 持久层的选收依据未知。诊断靠 adapter 日志的 `merged_shape` / `pair_key_prefix`;
要根治需原生 vs 桥接 SSE 字节级抓包对差（原生流式已不对 Science 服务,需临时构建）。

判「卡片消失」缺陷前,先确认是否只是这一语义:同轮/最新轮可见 + 正文永久保留 = 非缺陷。
另注意由此派生的测试陷阱:验收「重载后卡片仍在」必须在**发过追问之后**再重载,
只在最新轮重载会得到假阳性（2026-08-19 的 K3/K2.7 分化即此成因）。

## 必需测试

- 三模型 fake upstream：搜索严格 2/3 POST、不搜索 1 POST；nested forced/disabled、token caps、shared deadline。
- query 数量/字段/唯一 ID、pair/result 1..4 唯一性、512 KiB、usage checked-add、stage-specific 429/协议错误。
- JSON 与规范 SSE：merged 可见内容零 client `tool_use(name=web_search)`、查询调用全消费、
  真实 pair/input delta、普通 tool 的 `stop_reason=tool_use`、单 `message_start/message_stop`、
  阶段注释帧先于 `message_start`、大合并流可渲染而超大单帧仍显式失败。
- K3 尾随 client search 正例与位置/name/id/input/无 pair 负例。
- 真机必须在同一最终构建上分别跑 K3、K3-256k、K2.7：搜索 → 不联网追问 → 重载；
  核对 query/card/text、`bridged`、calls、无 400/repair/upstream failure。direct/provider 与 Science 证据分栏。
- 联网搜索总开关：**从 `handle_messages` 打到 fake 上游**看真正发出去的报文，断言上游 tools 里既没有
  typed search 也没有 bridge 的私有查询工具（它同名 `web_search`，只断言"没有 typed"会漏掉桥仍触发）。
  只测摘除函数本身证明不了 `cfg` 里的开关被读到。DeepSeek 单独一条，并先用开启态确认同一请求
  确实会带着搜索声明出去（否则关闭态的断言不说明问题）。另需：链首摘除后 tail-reorder 不触发、
  无声明轮次逐字段原样；loopback 层在真实进程里断言关闭态日志含规则 id、不含 adapter，
  重新开启后同一请求不经重启即重新走桥。

## Wrong vs Correct

### Wrong

```text
Science 每轮声明 web_search → 每轮直接强制搜索 → 非搜索轮多余调用/延迟/失败
```

### Correct

```text
主模型通过私有 query tool 决定是否搜索 → nested 只执行真实 server search
→ 有正文直接返回；无正文才用 exact tool_result synthesis → Science 单一生命周期
```
