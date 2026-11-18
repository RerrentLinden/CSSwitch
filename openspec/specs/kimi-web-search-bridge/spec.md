# kimi-web-search-bridge Specification

## Purpose

K3、K3-256k、K2.7 对 inline server search 的 planner / answer 行为不一致。网关在 Kimi 渠道上把 Science 的 typed `web_search_*`
先映射为私有 ordinary query tool，只有模型实际请求搜索时才发 nested server search，再把结果合并成 Science 看到的单一消息。
本规格约束这座桥的请求改写、嵌套调用、合成、合并输出、失败处理与联网搜索总开关关闭时的行为。
`kimi-anthropic-relay` 收到 typed `web_search_*` 时适用。相关规格：
[kimi-context-tail-reorder](../kimi-context-tail-reorder/spec.md)、[kimi-search-pair-adoption](../kimi-search-pair-adoption/spec.md)。

## Requirements

### Requirement: typed 搜索声明换成同名的私有查询工具

主调用 MUST 保留完整的 Science system / messages / compute / client tools，只把唯一的 typed Web Search 声明换成内部 ordinary `{query:string}` tool，
**其名字就是 `web_search`**——模型思考自然提到工具名时从构造上无泄漏（2026-08-20 真机 2/2 复现过改名前的 thinking 泄漏；清洗式方案被否）。
改写记录规则 `provider.kimi.web-search.query-tool-adapter`。

同请求存在任何其它 `name=web_search` 工具（裸名 / `type:"custom"` / 异类 typed）时 MUST 显式 400；守卫按 name 匹配、不看 type。
模型未调用它时 MUST 只有一次上游请求，普通回答 / 工具原样返回。

#### Scenario: 模型决定不搜索

- **WHEN** Kimi 普通轮声明了 typed search，而模型没有调用私有 tool
- **THEN** 上游只有 1 次请求，回答与普通工具原样返回，零 nested 开销（`bridged=0/upstream_calls=1`）

#### Scenario: 没有 typed search 或是兄弟 provider

- **WHEN** 请求没有 typed search，或请求走的是兄弟 provider
- **THEN** bridge 不触发，走原样路径

#### Scenario: 工具名冲突或多个 typed search

- **WHEN** 同请求存在其它 `name=web_search` 的工具，或有多个 typed search
- **THEN** 返回明确的 400，不覆盖用户工具

### Requirement: 内部查询调用的校验

模型调用内部 tool 时，query MUST 非空、ID 唯一、输入只含 `query` 字段，最多四次；query 去重 MUST NOT 绕过调用数上限。

#### Scenario: 非法的查询调用

- **WHEN** 内部 tool 调用的 query 为空、ID 重复、含 `query` 之外的字段，或调用次数超过四次
- **THEN** 返回明确的 400 / 502，不继续执行搜索

### Requirement: nested 调用强制执行真实搜索

nested 请求 MUST 只含原 typed Web Search，并**对任何 Kimi 模型无条件**使用指定型 `tool_choice` 与显式 `thinking: disabled`
（禁止模型名白名单——2026-08-20 review 抓到 fail-open 白名单让 catalog 出厂 id 静默不强制）；`max_tokens = min(main, 4096)`。
它 MUST 返回 1..4 组唯一的 `name=web_search` pair。

#### Scenario: nested 请求的形状

- **WHEN** 模型调用了内部查询工具
- **THEN** nested 请求只含原 typed Web Search，带指定型 `tool_choice`、`thinking: disabled`，`max_tokens` 不超过 4096

#### Scenario: nested 失败

- **WHEN** nested 返回 429 / 5xx、超时、非 JSON、缺 pair 或异名 pair
- **THEN** 保留 stage 与状态，显式失败，不 retry

### Requirement: nested 有正文时两次调用完成

nested 有正文时 MUST 两调用完成，合并真实 pair 与正文，不做 synthesis。

#### Scenario: nested 有 pair 也有正文

- **WHEN** nested 返回 pair 与正文
- **THEN** 共 2 次上游调用，合并真实 pair 与正文返回

### Requirement: 只有 pair 时做有界合成

只有真实 pair、无正文、且 main 没有未决普通 tool 时，MUST 做 bounded synthesis（第 3 次调用生成正文 / 普通工具）。
synthesis 使用 exact main tool IDs 的 `tool_result`、保留其它 Science tools、移除内部 tool 防递归，随后 MUST 经 `degrade_missing_tool_choice`
（防 `{"type":"any"}` 等 forced choice 被继承进已改动的 tools），`max_tokens = min(main, 8192)`。

#### Scenario: nested 只有 pair

- **WHEN** nested 有 pair、无正文，main 没有未决普通 tool
- **THEN** 第 3 次调用做 bounded synthesis，生成正文或普通工具
- **AND** synthesis 请求里没有内部查询工具，`max_tokens` 不超过 8192

### Requirement: 有未决普通工具时跳过合成

nested 无正文且 main 含可见 client tool_use 时 MUST 跳过 synthesis 直接 merge，以 `stop_reason=tool_use` 把搜索证据与未决工具一并交回 Science 续轮；
MUST NOT fail closed（2026-08-20 修复：旧硬错把可服务的混合轮变确定性 502，且与 merge 的显式支持自相矛盾）。

#### Scenario: nested 无正文而 main 有未决普通 tool

- **WHEN** nested 无正文且 main 含可见 client tool_use
- **THEN** 跳过 synthesis，merge 后以 `stop_reason=tool_use` 交回 Science 续轮

### Requirement: 查询与网页证据按不可信数据处理

query 与网页 evidence 都是 **untrusted data**：MUST 以 JSON / tool_result 数据传递，回答指令 MUST NOT 拼接或执行其中指令。
evidence 总量最多 512 KiB，该上限 MUST 同时作用于 synthesis 与 nested-has-text 直通合并两条路径；超限 MUST 失败，不截断；
上限检查 MUST 先于任何全量物化。

#### Scenario: evidence 超过上限

- **WHEN** evidence 总量超过 512 KiB（两条路径同判）
- **THEN** 返回 502，零截断、零假成功

### Requirement: K3 尾随的 client 搜索块

K3 在完整 pair 后偶发尾随 client `tool_use(name=web_search)`。仅当它是最后一个块且 `stop_reason=tool_use` 时 SHALL 按 type / name 剥离并记数
（不逐字段校验将被丢弃的块），安全性由随后的单次 nested 校验兜底。任何其它位置 / 工具 MUST fail closed。

#### Scenario: 尾随块在最后

- **WHEN** 完整 pair 之后的最后一个块是 client `tool_use(name=web_search)` 且 `stop_reason=tool_use`
- **THEN** 该块被剥离并记数

#### Scenario: 其它位置或其它工具

- **WHEN** 这样的块出现在其它位置，或是别的工具，或前面没有 pair
- **THEN** fail closed

### Requirement: 各阶段共享一次总时限

main / nested / synthesis MUST 共享一次 contract total deadline；每阶段 MUST NOT 重置 180 秒。
MUST NOT 有 retry、command fallback 或伪造正文。

#### Scenario: 共享时限过期

- **WHEN** shared deadline 在某个阶段后过期
- **THEN** 返回 504，下一阶段在联网前失败

### Requirement: usage 跨阶段合并

usage 数值字段 MUST 跨阶段 checked-add，溢出显式失败。同键**类型**跨阶段不一致时 SHALL 取最后阶段值并记 log_line 告警，
MUST NOT 为元数据差异丢弃整轮已完成的回答。

#### Scenario: usage 数值溢出

- **WHEN** usage 数值相加溢出
- **THEN** 返回 502

#### Scenario: usage 同键类型不一致

- **WHEN** 同一个 usage 键在不同阶段的类型不一致
- **THEN** 取最后阶段的值并记告警，不返回 502

### Requirement: 对 Science 只输出一个消息生命周期

对 Science MUST 只输出一个 Anthropic lifecycle。merged 可见内容 MUST NOT 含任何 client `tool_use(name=web_search)` 块（server_tool_use 除外），
内部查询调用 MUST 全部被消费。

流式路径进入即写 200 头 + `: bridge processing` SSE 注释帧，main / nested 完成后各一条阶段注释帧
（SSE 注释协议合法、事件解析器忽略）；写头后的失败 MUST 以流内 `event: error` 呈现。
渲染后的 SSE 以 64 KiB 分块 feed 校验器（只看 Err，不做 round-trip 字节比对）。

#### Scenario: 搜索轮的输出

- **WHEN** 一次搜索轮完成（例如 Science K2.7 搜索 → query tool → forced nested）
- **THEN** Science 收到原生搜索卡片与正文，恰好一个 `message_start` 与一个 `message_stop`
- **AND** 可见内容里没有 client `tool_use(name=web_search)`

#### Scenario: 流式路径的注释帧

- **WHEN** 请求走流式路径
- **THEN** 进入即写 200 头与 `: bridge processing` 注释帧，阶段注释帧先于 `message_start`

#### Scenario: 写头之后失败

- **WHEN** 200 头已写出后某阶段失败
- **THEN** 失败以流内 `event: error` 呈现

#### Scenario: 大合并流与超大单帧

- **WHEN** 合并后的流很大
- **THEN** 可以渲染；超大单帧仍显式失败

### Requirement: 原生路径与响应过滤

原生 typed 路径 SHALL 保留作为 nested executor；nested 响应继续经过 noise / phantom / adoption。
非 adapter 的 Kimi 响应路径 MUST **无条件**建 `SearchNoiseFilter`（零命中时字节级原样）。
门控谓词 MUST 从**变换前**的请求状态推导，MUST NOT 读被 `prepare_request` 改写后的 body
（2026-08-20 review：后置谓词恒为 false，整条过滤主路径成了死代码而测试仍绿）。

#### Scenario: 非 adapter 的 Kimi 响应

- **WHEN** Kimi 响应不经过 adapter
- **THEN** 响应仍经过 `SearchNoiseFilter`；没有命中时字节级原样

### Requirement: adapter 日志的诊断投影

adapter 日志行 MUST 携带 `merged_shape=<块类型序列>` 与 `pair_key_prefix=<键前缀段>`，只记形状与前缀，MUST NOT 记查询词或 id 全文。
它是排查 Science 落盘行为的一手数据。

#### Scenario: 一次桥接完成

- **WHEN** adapter 完成一次合并
- **THEN** 日志行含 `merged_shape` 与 `pair_key_prefix`，不含查询词与完整 id

### Requirement: 联网搜索总开关关闭时在链首摘除声明

联网搜索总开关关闭时，typed 搜索声明 MUST 在补偿链**之前**摘除（`strip_typed_web_search_tools`，规则 `tool.relay.web-search-disabled-by-config`），
MUST NOT 在链尾的 server tool policy 里摘：[kimi-context-tail-reorder](../kimi-context-tail-reorder/spec.md) 等规则以「本轮声明了 typed search」为触发条件且跑在更早，
链尾摘除会让它们先按「会搜索」改写一个根本不会搜索的轮次。链首摘除后整条链一致地落到「无 typed search → bridge 不触发，原样路径」这一既有分支，既有规则一条都不用改。

#### Scenario: 总开关关闭时的搜索声明

- **WHEN** 总开关关闭，请求带 typed search 声明
- **THEN** 声明在链首被摘除并记规则 `tool.relay.web-search-disabled-by-config`，bridge 与 tail-reorder 均不触发，上游 1 次调用
- **AND** 发往上游的 tools 里既没有 typed search，也没有 bridge 的私有查询工具

#### Scenario: 总开关关闭而本轮没有声明

- **WHEN** 总开关关闭，请求没有搜索声明
- **THEN** 请求零改写

### Requirement: 联网搜索只有走桥与关闭两档

开关 MUST 只有「走桥」与「关闭」两档，MUST NOT 提供原生直通：Kimi 不把 typed web_search 作为可调用函数交给模型，
而是在每次请求开头由服务端预搜索规划器决定搜不搜（2026-09-16 真机取证，见 [design.md](./design.md)）。

#### Scenario: 开关开启

- **WHEN** 总开关开启，Kimi 请求带 typed search 声明
- **THEN** 声明被换成私有查询工具走桥，不原样发给上游

### Requirement: 联网搜索总开关不分渠道

开关是全局的（`GatewayConfig.web_search` 取 `Profile.web_search`，不分渠道）：摘除 MUST NOT 按 flavor 分支，
DeepSeek 平时留给上游原生执行的搜索声明同样被摘。

#### Scenario: DeepSeek 同判

- **WHEN** 总开关关闭，DeepSeek 请求带搜索声明
- **THEN** 声明同样被摘除

### Requirement: 网关一侧的摘除独立于 Science 一侧

网关一侧的摘除 MUST 是独立于 Science 一侧（`enable_web_search = false` 的镜像配置，见 [science-launch-overrides](../science-launch-overrides/spec.md)）的保证：
Science 还没重启、沙箱还没重启、或 Science 版本不认识该选项时，桥仍然是断开的；Science 一侧生效后请求里没有声明，规则不再命中。
关闭态 MUST NOT 清洗历史工具块：「未声明该工具却回放其历史」是否被上游接受属未验证形状，交真机判定，不预先兜底。

#### Scenario: Science 还没重启

- **WHEN** 总开关已关闭而 Science 或沙箱还没重启，请求仍带搜索声明
- **THEN** 网关摘除声明，桥不触发

#### Scenario: Science 一侧已经生效

- **WHEN** 总开关关闭且 Science 已按新设置重启
- **THEN** 请求里没有搜索声明，摘除规则不命中——日志里有没有这条规则正好区分「Science 自己不再声明」与「网关在兜」

#### Scenario: 重新开启

- **WHEN** 总开关被重新开启
- **THEN** 同一请求不经重启即重新走桥

### Requirement: 联网搜索总开关默认开启

开关默认开，serde 默认值 MUST 显式写 `true`——`bool` 原生默认是 `false`，存量配置没有该字段，用原生默认等于给所有老用户静默停掉该能力。

#### Scenario: 存量配置没有该字段

- **WHEN** 配置文件里没有 `web_search` 字段
- **THEN** 总开关是开启的
