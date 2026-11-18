# request-log Specification

## Purpose

服务会记录每条推理请求走了哪个渠道、是哪一类调用、命中了哪些补偿规则，供控制台「请求日志」卡片与服务日志使用。
本规格约束日志的去向、请求明细的开关，以及明细里带什么、不带什么。
改动 `log_line!` / `request_log_line!` 的调用点、`call_kind.rs`、控制台的请求日志卡片或 `request_log` 配置时适用。
日志落盘与轮转见 [installed-app-launcher](../installed-app-launcher/spec.md)。

## Requirements

### Requirement: 日志的去向随启动方式不同

从源码运行时日志 MUST 打到服务进程的 stderr（终端），不落文件；安装版没有终端，MUST 落在 `~/.csswitch/service.log`。
两者内容 MUST 相同。

#### Scenario: 从源码运行

- **WHEN** 用 `./csswitch` 启动服务
- **THEN** 日志出现在终端，不产生 `service.log`

#### Scenario: 安装版

- **WHEN** 从安装版启动服务
- **THEN** 同样的内容写进 `~/.csswitch/service.log`

### Requirement: 请求明细由开关控制

控制台「请求日志」卡片的「记录请求日志」开关（`service.v1.json` 的 `request_log`，默认开启）MUST 控制每条请求的明细，切换立即生效：

- 服务日志里的 `POST /v1/messages relay …`、`relay stream|nonstream rules=…` 与 Web Search 桥接统计行；
- 卡片里的请求列表。

#### Scenario: 关闭开关

- **WHEN** 用户关闭「记录请求日志」
- **THEN** 之后的请求不再产生明细行，卡片列表也不再增加，不需要重启

#### Scenario: 默认状态

- **WHEN** 配置里没有 `request_log`
- **THEN** 开关是开启的

### Requirement: 不受开关影响的输出

启动横幅、沙箱生命周期、警告与上游错误 MUST 不受开关影响，始终输出。

#### Scenario: 开关关闭时上游出错

- **WHEN** 开关关闭而上游返回错误
- **THEN** 上游错误仍然出现在日志里

### Requirement: 上游失败行记回给 Science 的状态

上游失败行 `POST /v1/messages upstream_failure operation=<阶段> status=<S> upstream_status=<U>` 里，
`status` MUST 是网关回给 Science 的那份回复的状态码（[gateway-routing](../gateway-routing/spec.md) 的改报之后），
`upstream_status` 是上游的原始状态。渠道 Key 被拒时这一行是 `status=403 upstream_status=Some(401)`。
Kimi 搜索桥的流已打开时，HTTP 头已经是 200，这一行仍记 403：SSE 错误事件里发出的就是 403 那份正文。
这一行属于上游错误，不受开关影响，MUST NOT 含 Key。

#### Scenario: 渠道 Key 被拒

- **WHEN** relay 路径的上游回 401（非流式、开流前、或搜索桥的某一阶段）
- **THEN** 失败行是 `upstream_failure operation=<阶段> status=403 upstream_status=Some(401)`
- **AND** 日志里没有 Key

### Requirement: 明细里的实例与渠道

卡片里的每条请求 MUST 带着它来自哪个实例、走了哪个渠道（`正式 · kimi` / `沙箱 · deepseek`）。
服务日志里的 relay 明细行不带实例，两个实例的请求路径在那里同为 `/v1/messages`。

#### Scenario: 沙箱发来一次请求

- **WHEN** 沙箱经 `/sandbox/v1/messages` 发来请求
- **THEN** 卡片里这一条显示「沙箱 · <渠道>」，服务日志的明细行里路径是 `/v1/messages`

### Requirement: 明细标出调用类型

每条推理请求（`POST /v1/messages`）SHALL 带一个推导出的调用类型：服务日志的 relay 明细行在 `rules=` 之前多一项 `lane=`
（`rules=` 仍是最后一项，有脚本按字段先后解析）；控制面日志条目（`/control/logs`）的 `detail.lane` 是同一个值，认不出时为 `null`。
其它请求（如模型清单）的条目里 `lane` 也是 `null`。

取值：`main`（主对话）、`sub`（子 frame：子 agent、Reviewer 等）、`keepalive`（缓存保活）、`fork`（同一会话的分支调用）、
Science 后台调用的代号本身（`ttl`、`mem`、`sniff`…，至多 6 位小写字母数字），认不出时 `-`。判定边界见 [design.md](./design.md)「调用类型的判定」。

#### Scenario: 子 frame 上的标题调用

- **WHEN** 请求的 `metadata.user_id` 里 `session_id` 是 `<根 id>:<frame 段>:ttl`
- **THEN** relay 明细行含 `lane=ttl rules=`，控制面条目里是 `"lane":"ttl"`

#### Scenario: 主对话与子 frame

- **WHEN** `session_id` 只有根段，或是 `<根 id>:<frame 段>`，且 `max_tokens` 不是 1、`tool_choice.type` 不是 `none`
- **THEN** 类型分别是 `main` 与 `sub`

#### Scenario: 保活与分支

- **WHEN** `session_id` 不带代号，请求的 `max_tokens == 1`，或 `tool_choice.type == "none"`
- **THEN** 类型分别是 `keepalive` 与 `fork`

#### Scenario: 有代号时不看请求形状

- **WHEN** `session_id` 带代号 `ttl`，同时 `max_tokens == 1`
- **THEN** 类型是 `ttl`

#### Scenario: 没有 metadata

- **WHEN** 请求没有 `metadata`，或 `user_id` 不是 Science 的格式（即使 `max_tokens == 1`）
- **THEN** 明细行是 `lane=- rules=`，条目里是 `"lane":null`，仍按实例与渠道记录

### Requirement: 调用类型只从请求推导且不带出 id

调用类型 MUST 只依据 `metadata.user_id` 的结构与请求形状（`max_tokens`、`tool_choice`）推导，不看请求头。
会话 id、frame id 与 `metadata` 的其它内容 MUST NOT 进日志或控制面接口：原样输出的只有过了代号写法检查的代号。
relay 一侧 MUST 在补偿链之前取类型（补偿链会改写 `tool_choice`）。取类型只读请求、不改请求：官方直通转发的仍是原样的字节；
「记录请求日志」关着时，官方直通一侧 MUST NOT 为取类型去读请求体。

#### Scenario: 会话与 frame 的 id 不外泄

- **WHEN** 请求的 `session_id` 带着根 id 与 frame 段
- **THEN** 服务日志与 `/control/logs` 里都搜不到这两个 id，只有推导出的类型

#### Scenario: 开关关着

- **WHEN** 「记录请求日志」关着时发来推理请求
- **THEN** 不产生 relay 明细行与控制面条目

### Requirement: 桥接统计行不记录对话内容

Web Search 桥接统计行 MUST 只记录 rule / model、bridged / query / call 数与阶段耗时，MUST NOT 记录 query、结果或正文。

#### Scenario: 一次搜索轮

- **WHEN** 一次请求经过 Web Search 桥接
- **THEN** 日志只有 rule / model、bridged / query / call 数与阶段耗时
