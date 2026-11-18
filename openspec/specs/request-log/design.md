# request-log 设计注记

## 签名

```text
crate::log_line!(…)            # 始终输出:启动横幅、生命周期、警告、上游失败行
crate::request_log_line!(…)    # 只在「记录请求日志」开着时输出(crate::request_log_enabled())
AppState::record_request(entry)  # 控制面日志条目,同样受开关控制

call_kind::of_request(&Value) -> CallKind   # relay:Science 发来的原样请求,在补偿链之前取
call_kind::of_body(&[u8]) -> CallKind       # handle_inference:只反序列化三个字段;开关关着时不调用
call_kind::classify(metadata, max_tokens, tool_choice)   # 两个入口共用的判定(模块内私有)
CallKind::{Main, Sub, KeepAlive, Fork, Lane(String), Unknown}
CallKind::as_str()  # 明细行 lane= 的取值,Unknown → "-"
CallKind::known()   # 条目 detail.lane,Unknown → None(序列化成 null)

relay 明细行:
  POST /v1/messages relay target=… stream=… msgs=… thinking_type=… budget_tokens=… max_tokens=…
    server_tool_types=… dropped_server_tools=… lane=… rules=…        # 新字段插在 rules= 之前
上游失败行:
  POST /v1/messages upstream_failure operation=<阶段> status=<回给 Science 的> upstream_status=<上游的>
```

## 调用类型的判定

依据是对 Science 0.1.72 的静态分析(格式)与 2026-11-18 的真机核对。0.1.72 发出的 `metadata.user_id`
是一个 JSON 字符串:

```text
{"session_id":"<root32>[:<frame24>][:<代号>]","frame_id":"<frame32>"}
```

- 根段 MUST 是 32 位十六进制(大小写都认),否则整条认不出 —— 这时请求形状不算数:`max_tokens == 1`
  本身不说明这是 Science 的保活。
- 第二段是 24 位十六进制就是 frame 段,否则必须是代号(`^[a-z0-9]{1,6}$`,Science 自己的写法);
  有第三段时第二段必须是 frame、第三段必须是代号。空段、多于三段 → 认不出。
- 有代号直接用代号,不再看形状;没有代号时依次看 `max_tokens == 1` → `keepalive`、
  `tool_choice.type == "none"` → `fork`;都不是 → 有 frame 段为 `sub`,否则 `main`。
- relay 在补偿链之前取:补偿链会改写 `tool_choice`,之后再取会认错。
- 请求体不是 JSON 对象时两个入口都是认不出(结构体能按位置把数组反序列化成「保活」,所以 `of_body` 先看首字节)。

控制台把代号翻成中文标签(`LANES` 表,见 [console-webui](../console-webui/spec.md)「请求日志标出调用类型」)。

隐私:对外只有推导出的类型。原样输出的只有代号,它必须过代号写法检查(至多 6 位),最短的 id 也有 24 位,
两者不会混。会话 id、frame id 与 `metadata` 的其它内容不离开 `call_kind`。

性能与原样转发:官方直通一侧只借用请求体(`&[u8]`)读三个字段,转发的仍是同一块原样的缓冲区;
「记录请求日志」关着时根本不读。relay 一侧本来就要解析请求做补偿,取类型不额外读请求体。

已知边界:

- 开着 thinking 的缓存保活(`max_tokens` 不是 1)认不出保活,会显示成主对话。
- 官方直通一侧只有结构上的保证,没有离线的整链路测试,也没有真机记录。

## 由来与证据

控制台「请求日志」原先只分得清实例与渠道:一条消息在 Science 里会带出好几次推理请求(标题、结束判断等后台调用),
看日志分不出哪一条是用户的那一轮。

2026-11-18 真机(真实 Science 0.1.72 发出的流量,假上游,临时目录里的隔离免登录沙箱;四个分支合并后的开发构建):
一条消息带出 4 次请求,网关日志依次是 `lane=ttl`、`lane=main`、`lane=sniff`、`lane=ttl`,与假上游一侧独立推导的
结果一致;控制台显示「后台·标题」「主对话」「后台·结束判断」;日志与控制面接口里搜不到 32 位十六进制的 id。
记录在 [test/LIVE_ACCEPTANCE.md](../../../test/LIVE_ACCEPTANCE.md) §L。

上游失败行的 `status=` 原先是上游状态;2026-11-18 起 relay 把 401 改报 403,这一行跟着记回给 Science 的状态,
上游原始状态看 `upstream_status=`(见 [gateway-routing](../gateway-routing/design.md))。

## 必需测试

- `call_kind::tests::classifies_science_metadata_through_both_entry_points`:按真实格式的表驱动用例(主对话、子 frame、
  `root:代号`、`root:frame:代号`、带数字的代号、保活、子 frame 的保活、有代号时不看形状、fork、强制工具不是 fork、
  大写 id,以及各种畸形输入),每条同时走 `of_request` 与 `of_body` —— 两个入口口径不一,正式实例在官方与第三方
  模式下就会给同一条请求标出不同的类型。
- `call_kind::tests::unparseable_bodies_are_unknown`:非 JSON、空体、按位置排成数组的请求体都认不出。
- loopback「请求日志:调用类型」:经沙箱入口发一条 `root:frame24:ttl` 的请求,明细行含 ` lane=ttl rules=`,
  条目含 `"lane":"ttl"`;没有 metadata 的请求明细行是 ` lane=- rules=`、条目是 `"lane":null`;
  服务日志与 `/control/logs` 里都没有根 id 与 frame 段;关掉「记录请求日志」后没有新的明细行与条目。
  正式实例此刻是官方模式,它的入口会直通真实的 `api.anthropic.com`,这一层不打。
- loopback「上游错误回给 Science 的样子」:失败行在 `post_nonstream`、`open_stream`、
  `kimi_web_search_adapter_main` 三处都记 `status=403 upstream_status=Some(401)`,日志里没有 Key。
- `messages::tests::upstream_failure_metadata_omits_transport_detail_with_url`:传输层失败的 detail 不进失败行。

## Wrong vs Correct

### Wrong

```rust
// 在补偿链之后取类型:tool_choice 已被改写
let call_kind = call_kind::of_request(&transformed);
// 为了标类型把会话 id 打进日志
log_line!("… session={session_id} …");
// 官方直通里为取类型解析整个请求体,再把重序列化的字节转发出去
```

### Correct

```rust
// Science 发来的原样请求,补偿链之前
let call_kind = call_kind::of_request(&raw);
// 官方直通:开关开着才读,只借用原样的缓冲区
let call_kind = if request_log_enabled() { call_kind::of_body(&body) } else { CallKind::Unknown };
```
