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

## 历史：从 429 到统一 bridge

下文沿用原渠道文档的小节编号（§3、§3b–§3e），对照见 [kimi-channel](../kimi-channel/design.md)「原章节对照」。

### 曾经的理由:429

历史上只要**声明了 web_search 却没实际搜索**就返回 429(引擎过载),而 Science
每轮都声明它,于是真实会话每轮都失败。当时的对策是把服务端工具换成同名客户端工具
(920 行的 `kimi_coding_search.rs`)。

2026-08-19 复测 **32 次全部 200,零 429**，一度退役桥接改为原生 inline。随后 review
发现功能合同仍不成立：K2.7/Science 会只发幻影对并声称搜索不可用，K3-256k 也会忽略 typed
search；即使 direct 能搜索，各模型对 search result→final answer 的行为仍不一致。因此
2026-08-20 恢复早期桥接语义，但没有机械恢复旧 920 行实现。

当前规则 `provider.kimi.web-search.query-tool-adapter`：

```text
Science typed web_search
  -> main: 私有 ordinary query tool（模型决定是否搜索）
  -> 不搜索: 1 call，原回答/普通工具返回
  -> 搜索: nested typed web_search（指定工具 + thinking disabled）
       -> 有正文: 2 calls
       -> 只有真实 pair: bounded synthesis，第 3 call 生成正文
  -> Science: 单一 message lifecycle、真实 Web Search 卡片/正文；内部工具零泄漏
```

主调用保留 Science 完整 system/messages/compute/client tools；query/evidence 以 untrusted data
传递，不作为指令执行。nested/synthesis 输出上限分别 4096/8192，evidence 总量 512 KiB，
三阶段共享一次 contract 180s deadline，无 retry、command fallback 或伪造成功。原生 typed 路径
仍保留为 nested executor，并继续使用噪声/幻影/采钥规则。

## 联网搜索总开关（网关一侧）

控制台「当前连接」卡片有一个「联网搜索」开关,存进 `~/.csswitch/service.v1.json` 顶层的
`web_search`,**默认开启**,对官方 / Kimi / DeepSeek 与免登录沙箱都生效。它同时管两层:
Science 启动时是否带上 `enable_web_search = false` 的镜像配置(机制与生效时机见
[science-launch-overrides](../science-launch-overrides/design.md)「联网搜索总开关」),以及本节关心的网关一侧 —— 桥接不接通。

关闭时,typed `web_search` 声明在 relay 请求入口、**补偿链之前**被摘除,记规则
`tool.relay.web-search-disabled-by-config`。摘在链首而不是链尾的 server tool policy 里,
是因为 §3e 的尾随上下文重排以"本轮声明了 typed search"为触发条件:在链尾摘,重排会先在一个
根本不会搜索的轮次里交换末尾两条 user message。链首摘除后整条链一致地按"本轮没有搜索声明"
处理,§3 的 bridge 自然不触发,上游只有一次调用。

网关一侧保留为独立保证,不依赖 Science 一侧:Science 还没重启、免登录沙箱还没重启、或
Science 版本不认识 `enable_web_search` 时,声明仍会到达网关,仍在这里被摘除。Science 一侧
生效之后请求里本就没有声明,规则不再命中 —— 所以日志里**有没有这条规则**正好区分
「Science 自己不再声明」与「网关在兜」。

开关只有「走桥」与「关闭」两档,**不提供原生直通**,依据见下文「为什么不提供原生直通」。

配置是每请求读快照的,所以网关一侧在开关保存后下一次请求即生效,不需要重启 Science
(地址与模型槽仍需重启,因为 Science 只在启动时读它们)。

历史:2026-08-20 起这个开关是渠道级的,存在 `kimi.web_search`,界面只在「渠道配置 → Kimi」
出现,只有网关一侧的效果。2026-10-10 并入全局总开关:存量配置里顶层没有 `web_search` 且
`kimi.web_search == false` 的迁移为总开关关闭(用户显式关掉的东西不能在升级后悄悄打开),
其余读成开启;保存后只写顶层键。缺字段时必须读成开启 —— `bool` 的 serde 原生默认是 `false`,
直接用它等于给老用户静默关掉搜索。

已知边界:关闭态**不清洗历史**里已有的 `server_tool_use` / `web_search_tool_result` 块。
§3 的请求侧配对修复对 Kimi 流量无条件运行,配对完整性不受影响;但"本轮不声明该工具却回放
它的历史"这一形状**未单独取证**,判据保留在 `test/LIVE_ACCEPTANCE.md`。

2026-08-20 真机验收(渠道级开关,只覆盖网关一侧):用户在真实 Science + Kimi 渠道上实测通过,
开关行为符合预期,未发现问题。上面那条历史回放形状不在该次取证范围内,仍按未验证记录。
2026-10-10 真机验收(总开关,免登录沙箱 + Kimi `k3`,Science 0.1.60):关闭并重启沙箱后,
Science 自己不再声明 typed `web_search`(relay 日志里没有摘除规则,也没有搜索工具);
沙箱未重启时由网关摘除;重新开启并重启后搜索经桥恢复。官方模式与 DeepSeek 未测。
逐项记录与判据在 `test/LIVE_ACCEPTANCE.md` §E。

## 当前净结果

三个模型统一走一个主 bridge；搜索轮才增加 nested/synthesis，非搜索轮仍只有一次 main call。
早期约 920 行状态机没有原样恢复；当前实现复用 `messages` transport、SSE validator、
`kimi_search_noise` 与历史修复。日志只记录 rule/model、bridged/query/call 数与阶段耗时，不记录 query、
结果或正文。

> 第一次退役(2026-08-19 上午)在这里就收手了,真机验收 16 轮零报错,
> 功能却是废的——助手内容里反复出现空的 `Search results for query:` 头,
> 模型自检认为工具不可用,当场回滚。教训:**验一条补偿能否退役,要验功能,
> 不能只验报错**。当天傍晚的隔离探测把"功能废"拆成了三个可修的窄毛病
> (下两节),才完成第二次、真正的退役。
>
> 旧实现仍保留在 git 历史（提交 `1bdbbbd` 之前）作为证据，不是当前恢复方式的源码副本。

## 真机验收记录

### 历史 native 真机验收(2026-08-19,内置浏览器驱动真实 Science + 真实订阅 key)

同一 K3 会话连续 6 轮:原生搜索(组件渲染、来源齐全、开头无噪声)→ 凭历史追问
(配对修复命中)→ 中毒历史 Resume 后再搜索(400 → 200)→ 无需搜索轮(幻影对
剥离命中、落盘干净)→ 搜索历史 + python 工具循环(3 次 LLM 调用零失败,这正是
当初 502 死循环的形态)→ 搜索 + 工具混合轮。K2.7 新起一轮原生搜索,正常回答。
全程 `upstream_failure` 仅出现在修复落地前的中毒历史上。

### 统一 bridge 真机验收(2026-08-20)

- K3：Elixir 搜索 14 results → 不联网追问 → 重载，PASS；搜索 2 calls，追问 1 call。
- K3-256k：Vite 搜索 17 results → 不联网追问 → 重载，PASS；搜索约 145s、追问约 156s，
  均在共享 180s deadline 内。高延迟是用户选择统一 bridge 的明确代价。
- K2.7：Lua 搜索 13 results → 不联网追问 → 重载，PASS；搜索 2 calls。
- 三者 query/card/text 均正确，追问 `bridged=0`，无 pairing repair / 400 / upstream failure。

## 为什么不提供原生直通（2026-09-16 真机取证）

2026-09-16 曾临时加过第三档「原生直通」(typed 声明原样发给上游,不映射 query tool,其余补偿
照常运行),真机验证后撤回,开关回到两档。

现象:直通下搜索「用一次就不可用」,模型回答「当前会话的 web_search 工具已不可用」。

根因:**Kimi 不把 typed web_search 作为可调用函数交给模型**,而是在每次请求开头由服务端
预搜索规划器决定搜不搜。证据全部来自真实 Science 会话的抓包(`CSSWITCH_DEBUG_CAPTURE_DIR`):

- 发往上游的请求里 `tools[0]` 就是 `web_search_20250305`,模型的原始思考却是
  "I don't see a web_search tool in my available functions"。
- 上游返回的块顺序恒为「噪声头 → `server_tool_use` → 结果 → **thinking** → 正文」:搜索发生在
  模型开始思考之前。事后模型的思考是 "it wasn't in my function list at the top, but the system
  executed it"。
- 搜索词带规划器改写的痕迹:「智谱」被直译成 `Wisdom spectrum`,开头拼上当天日期。
- 规划器判定不搜时,留下的就是空噪声头 + §3c 的幻影对——流式下每个不搜索的步骤都有。

后果:用户消息本身就是搜索请求时(第一轮常见),规划器会搜;任务中途才出现的搜索需求
(先算出年份再搜、先查技能再搜)模型没有办法触发,只能改走网络请求或声称工具不可用。
同一问题「先用 python 随机选年份,再搜索那年诺贝尔物理学奖」的 K3 真机对照:

| | 原生直通 | 走桥 |
| --- | --- | --- |
| 模型可见 web_search | 否 | 是 |
| 步数 | 5 步,4 步浪费(调 API → 拦截 → 申请权限 → 502) | 2 步(python → 搜索) |
| 搜索词作者 | 规划器 | 模型,年份正确 |

这正是 §3 bridge 存在的理由:把 typed 声明换成模型可见的私有 query tool,搜索时机和搜索词
交还给模型。
