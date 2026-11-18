# gateway-routing 设计注记

## 签名

```text
server::relay_failure_reply(&GatewayConfig, &UpstreamError) -> RelayFailureReply { status, body, retry_after }
  # relay 路径上一次上游失败回给 Science 的样子,唯一的构造处:非流式、开流前失败、
  # Kimi 搜索桥各阶段共用;upstream_status == Some(401) → 403 + permission_error,其余保留状态 + api_error
server::report_upstream_failure(stream, cfg, operation, &UpstreamError)      # 非桥路径
server::adapter_upstream_failure(stream, cfg, head_written, operation, …)   # 搜索桥,见 kimi-web-search-bridge
server::with_display_message(Value) -> Value   # 在正文顶层复制一份 error.message
messages::UpstreamError { status, upstream_status, detail, retry_after: Option<String> }
messages::forwardable_retry_after(&[u8]) -> Option<String>   # 1–5 位数字,或 IMF-fixdate
messages::upstream_failure_metadata(operation, status, &UpstreamError)
  # 日志行;status 取 RelayFailureReply.status,即回给 Science 的状态码
official_passthrough::forward(...)   # 官方直通,不经过 relay_failure_reply
```

渠道名取 `profile::Mode::for_adapter(&cfg.provider)` 的 `display_name()`(`relay` → Kimi,`deepseek` → DeepSeek),
反查不到时写「第三方」。

## 端点合同的由来

2026-08 实测:Science 经 base_url 只调这两个端点,登录 / 会话存储 / 内核 /
Reviewer 都走 daemon 自身。所以没有第三类端点需要处理,其余一律显式 404 ——
新版本 Science 若开始调别的端点,会立刻暴露而不是被静默兜底。
免登录沙箱调的是同样两个端点,只是前面多一段 `/sandbox`;前缀下的其它路径同样显式 404。
`csswitch-gateway official-passthrough --port N --log F` 是诊断子命令,
catch-all 转发并记录脱敏端点清单,用于回归这个结论。

## 上游 401 与 retry-after 的由来

依据是对 Science 0.1.72 的静态分析,加 2026-11-18 的真机验收。

静态分析得到的 Science 行为:

- 推理接口回 401:一律当成自己的 Claude 登录失效,丢掉正文,以「请登出再登录」结束这一轮。
- 不带 `error_code` 的 403:不重试、不碰登录状态,把正文显示给用户。带 `error.details`
  (从而带 `error_code`)的 403 会被归进别的分支,所以改报的正文不带它。
- 内嵌 SDK 生成错误文字时,正文顶层有 `message` 就用它,否则把整段 JSON 字符串化后显示;
  错误分类读的是 `error.message`,不受顶层副本影响。顶层副本只为显示,哪天 Science 自己能好好
  显示错误正文,删掉 `with_display_message` 与它的调用即可。
- SSE 里的 `api_error` 与 5xx 被当成可重试的服务端错误。
- `anthropic-ratelimit-*`、`x-should-retry` 被当成 Claude 订阅的用量暂停 / 额度不足。

真机验收(2026-11-18,真实 Science 0.1.72 + 假上游,临时目录里的隔离免登录沙箱,
四个分支合并后的开发构建;逐项记录在 [test/LIVE_ACCEPTANCE.md](../../../test/LIVE_ACCEPTANCE.md) §J):

| 场景 | 改动前 Science 显示 | 改动后 |
| --- | --- | --- |
| Key 被拒,联网搜索开着(走搜索桥) | `Claude is temporarily unavailable — retrying in Ns`,3 分钟内重试 9 次仍在继续 | 只失败一次,显示点名 Kimi 的说明 |
| Key 被拒,联网搜索关着 | `Your Claude session is no longer valid. Please sign out and sign in again.` | 显示 `403` 与同一句说明 |
| 上游回 429 并带 `retry-after: 12` 与两个会被误读的限额头 | — | `Claude is at capacity — retrying`,12.45 秒后重试成功;没有订阅额度类提示 |

`retry-after` 只认 IMF-fixdate 一种日期写法:发送方本就只该用它;两种旧写法与其它取值一律不转。
合规的值不含 CR / LF,可以直接写进响应头 —— 放宽校验就可能把换行写进回给 Science 的头里。

已知边界:

- 官方直通的 401 原样到达只有单测(`official_passthrough::forward` 打本地假上游),没有离线的整链路测试。
- DeepSeek 的改报只有单测与 loopback(沙箱入口非流式),真机只看过 Kimi。
- Kimi 搜索桥的流式路径带不了 `retry-after`(见 kimi-web-search-bridge),Science 按自己的节奏重试。

## 必需测试

- `server::tests::relay_upstream_401_is_reported_as_a_readable_403_naming_the_channel`:`relay` 与 `deepseek`
  两种 provider 下 401 → 403,正文逐字段等于点名渠道的 `permission_error` 形状(含顶层 `message`),
  不含 Key 与上游正文,`retry_after` 保留。
- `server::tests::other_relay_failures_keep_their_status_and_body`:403 / 404 / 429 / 500 / 529、上游 307 映射的 502、
  传输层 502,状态与 `api_error` 正文不变,`retry_after` 保留。
- `messages::tests::retry_after_is_forwarded_only_as_delay_seconds_or_an_imf_date`:合规的数字与 IMF-fixdate 原样返回;
  空串、小数、负数、6 位数、带 CR / LF、尾部多余文字、两种旧日期写法、`UTC`、错拼月份、非 UTF-8 一律 `None`。
- `official_passthrough::tests::official_401_reaches_science_with_status_and_body_bytes_unchanged`:官方直通的 401
  状态行与正文字节原样。
- loopback「上游错误回给 Science 的样子」:两个渠道都指向本地假上游(`nc` 回一条固定响应)。Kimi 非流式、
  Kimi 流式开流前、沙箱入口(DeepSeek 非流式)的 401 都是 403 + 点名渠道的正文,流式开流前失败以 JSON 返回;
  429 只多 `retry-after: 17`,不带 `anthropic-ratelimit*` / `x-should-retry` / `retry-after-ms` / `request-id`,
  正文仍带上游原文;日志里的失败行记 `status=403 upstream_status=Some(401)`,日志里没有 Key 与请求、上游正文。

## Wrong vs Correct

### Wrong

```rust
// 把上游状态与上游响应头原样回给 Science
api_error_json(stream, error.status, &error.detail);      // 401 → Science 让用户重新登录 Claude
for (name, value) in upstream.headers() { /* 照抄 */ }    // 第三方限流被当成订阅额度用完
```

### Correct

```rust
// relay 的失败一律经 relay_failure_reply:401 → 403 + 点名渠道的 permission_error,
// 响应头只带校验过的 retry-after;日志行的 status= 也取这里
let reply = relay_failure_reply(cfg, &error);
write_relay_failure(stream, reply);
// 官方直通走 official_passthrough::forward,不经过这里
```
