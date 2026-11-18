# gateway-routing Specification

## Purpose

CSSwitch 是一个本地 Rust 服务进程，把用户自己的 Claude Science 实例的推理流量转到官方 Anthropic、Kimi 或 DeepSeek，并在必要处做窄补偿。
本规格约束这个进程对外的路由表、三种模式各自怎么转发、两个实例怎么各走各的、CONNECT 隧道，以及上游失败与凭证的处理。
改动 `control.rs` 的路由、`server.rs` 的模式分支与上游失败的回复、`connect.rs` 或 `messages.rs` 的错误处理时适用。
总览见 [ARCHITECTURE.md](../../ARCHITECTURE.md)。

## Requirements

### Requirement: 一个常驻进程一张路由表

`serve` MUST 是唯一的常驻进程，监听 `127.0.0.1:<端口>`（默认 8788），路由只有下表：

| 路径 | 用途 |
| --- | --- |
| `/` | WebUI（内嵌 HTML，单文件） |
| `/control/*` | 控制面 API（含 `/control/quit` 退出服务） |
| `/v1/messages` | 正式实例的推理 |
| `/v1/models` | 正式实例的模型清单 |
| `/sandbox/v1/messages` | 免登录沙箱的推理 |
| `/sandbox/v1/models` | 免登录沙箱的模型清单 |
| 其余 | 显式 404（含 `/sandbox`、`/sandbox/v1/别的`、`/sandboxx/…`） |

安装版与从源码运行（`./csswitch`）起的是同一个 `serve`。

#### Scenario: 未知路径

- **WHEN** 请求到达路由表之外的路径
- **THEN** 返回显式 404

#### Scenario: 两种启动方式

- **WHEN** 分别从安装版与 `./csswitch` 启动
- **THEN** 两者对外的路由表相同

### Requirement: 推理端点只有两个

Science 经 base_url 只调 `/v1/messages` 与 `/v1/models` 两个端点（2026-08 实测；登录、会话存储、内核、Reviewer 都走 daemon 自身）。
网关 MUST NOT 为其它端点兜底：其余一律显式 404——新版本 Science 若开始调别的端点，会立刻暴露而不是被静默兜底。
免登录沙箱调的是同样两个端点，只是前面多一段 `/sandbox`；前缀下的其它路径同样显式 404。

#### Scenario: Science 调了第三个端点

- **WHEN** 某个版本的 Science 经 base_url 调用这两个端点之外的路径
- **THEN** 网关返回显式 404

### Requirement: 三种模式各自的转发方式

| 模式 | `/v1/messages` | `/v1/models` |
| --- | --- | --- |
| 官方 | 零改写转发 `api.anthropic.com`，鉴权头原样透传 | 转发 |
| Kimi | relay 契约 + Kimi 补偿链 | 由用户四槽配置本地合成 |
| DeepSeek | deepseek 契约 + DeepSeek 补偿链 | 同上 |

官方模式 MUST 逐字节直通，MUST NOT 做任何补偿——官方 API 就是协议本身，没有缺口可补。第三方模式才进入补偿链
（见 [kimi-channel](../kimi-channel/spec.md)、[deepseek-channel](../deepseek-channel/spec.md)、[model-menu](../model-menu/spec.md)）。

#### Scenario: 官方模式的推理请求

- **WHEN** 正式实例处于官方模式并发来推理请求
- **THEN** 请求零改写地转发到 `api.anthropic.com`，鉴权头原样透传

#### Scenario: 第三方模式的模型清单

- **WHEN** 实例处于 Kimi 或 DeepSeek 模式并请求 `/v1/models`
- **THEN** 清单由用户的四槽配置本地合成，不向上游转发

### Requirement: 每个实例各有一个模式

模式 MUST 是每个实例各一个，都存在配置里：正式实例是 `mode`（三种都可选），免登录沙箱是 `sandbox_mode`（只有 Kimi / DeepSeek，没有官方）。
渠道配置（地址 / API Key / 四槽）每个渠道一份，两个实例共用。切换其中一个实例 MUST NOT 改另一个的模式，也 MUST NOT 重启另一个。
切换正式实例的模式需重启 Science daemon（它只在启动时读 base_url）。

| 实例 | 注入的 `ANTHROPIC_BASE_URL` | 请求落在 | 按哪个模式路由 |
| --- | --- | --- | --- |
| 正式实例 | `http://127.0.0.1:8788` | `/v1/*` | `mode` |
| 免登录沙箱 | `http://127.0.0.1:8788/sandbox` | `/sandbox/v1/*` | `sandbox_mode` |

#### Scenario: 正式实例走官方而沙箱走 Kimi

- **WHEN** `mode` 是官方、`sandbox_mode` 是 Kimi
- **THEN** `/v1/*` 的请求走官方直通，`/sandbox/v1/*` 的请求走 Kimi 渠道

#### Scenario: 切换其中一个实例

- **WHEN** 用户切换正式实例的模式
- **THEN** 沙箱的模式不变，沙箱不被重启

### Requirement: 渠道之间没有共享状态

两个渠道同时有流量 MUST 是安全的：渠道运行配置与模型缓存都是每请求新建，网关里 MUST NOT 有按渠道共享的缓存或会话状态。

#### Scenario: 两个实例同时走不同渠道

- **WHEN** 正式实例与沙箱同时向不同渠道发请求
- **THEN** 两边的请求各自按自己的渠道配置处理，互不影响

### Requirement: CONNECT 请求的处理

控制台收到 `CONNECT` 时，Anthropic / Claude 主机与不合法的目标 MUST 显式 404（快速失败）；其余主机 MUST 直连打隧道
（`connect::should_tunnel`），MUST NOT 经用户自己的上游代理。

#### Scenario: CONNECT 到第三方主机

- **WHEN** 沙箱经 `https_proxy` 向 GitHub 发 `CONNECT`
- **THEN** 网关直连打隧道，返回 200

#### Scenario: CONNECT 到 Anthropic 或 Claude 主机

- **WHEN** `CONNECT` 的目标是 Anthropic / Claude 主机，或目标不合法
- **THEN** 返回 404

### Requirement: 上游错误如实返回

上游错误 MUST 保留 status，向客户端返回可诊断的错误体；MUST NOT 伪造成功。补偿补不了时 MUST 把上游错误原样交回。
唯一的例外是第三方渠道上游回的 401，见下一条「relay 把上游 401 改报 403」。

#### Scenario: 上游返回 4xx 或 5xx

- **WHEN** 上游返回 401 以外的错误状态
- **THEN** 客户端收到同样的 status 与可诊断的错误体（第三方渠道上是 `api_error` 加上游原文）

### Requirement: relay 把上游 401 改报 403

第三方渠道（Kimi / DeepSeek，relay 路径）的上游回 401，说明的是渠道 Key 被拒，与 Claude 登录无关；
而 Science 把推理接口的 401 一律当成自己的登录失效。网关 MUST 改报 403，正文 MUST 是下面的形状：

```json
{"type":"error","error":{"type":"permission_error","message":"<说明>"},"message":"<说明>"}
```

`<说明>` 是「`<渠道>` 渠道拒绝了 API Key(上游返回 401)。请在 CSSwitch 控制台的「渠道配置」里检查这个渠道的 Key。」，
`<渠道>` 与控制台的模式名一致（`Kimi` / `DeepSeek`）。顶层的 `message` 是同一句的副本，供 Science 直接显示。
正文 MUST NOT 带 `error.details`，MUST NOT 带出上游正文或 Key。

#### Scenario: Kimi 或 DeepSeek 的上游拒绝 Key

- **WHEN** 第三方渠道的上游对非流式推理请求回 401
- **THEN** Science 收到 403，`error.type` 是 `permission_error`，说明里点名该渠道，顶层带同一句 `message`
- **AND** 正文里没有 `error.details`、上游正文与 Key

### Requirement: 401 改报只在 relay 路径、开流前以 HTTP 返回

非流式请求，以及流式请求在开流之前就失败时，改报 MUST 以 HTTP 403 + `application/json` 正文返回，流式也不包成 SSE。
Kimi 搜索桥在流已打开之后的失败见 [kimi-web-search-bridge](../kimi-web-search-bridge/spec.md)「流已打开后的上游失败以 SSE 收尾」。
官方直通 MUST NOT 做这项改报：那条路上的 401 确实是 Claude 登录问题。

#### Scenario: 流式请求在开流前被拒

- **WHEN** 第三方渠道的流式请求在上游开流之前收到 401
- **THEN** Science 收到 HTTP 403 与 `application/json` 正文，正文与非流式相同

#### Scenario: 官方模式下上游回 401

- **WHEN** 正式实例处于官方模式，上游回 401
- **THEN** 状态与正文逐字节原样到达 Science

### Requirement: relay 错误响应只转发 retry-after

relay 路径上，上游错误响应带 `retry-after` 且取值合规时，网关 MUST 把它原样带给 Science（401 改报成 403 时同样带）。
合规指：1–5 位十进制数字（秒，含 `0`），或 IMF-fixdate 写法的 HTTP 日期（如 `Wed, 21 Oct 2026 07:28:00 GMT`）。
取值不合规时——包括 HTTP 日期的两种旧写法、超过 5 位的数字、小数、带 CR / LF 的值——MUST NOT 带，状态与正文不变。

其它上游响应头 MUST NOT 转发，尤其是 `anthropic-ratelimit-*` 与 `x-should-retry`：Science 会把它们当成 Claude 订阅的用量暂停或额度不足。

#### Scenario: 429 同时带着会被误读的响应头

- **WHEN** 上游回 429，带 `retry-after: 17`、`anthropic-ratelimit-unified-status`、`x-should-retry`、`retry-after-ms` 与 `request-id`
- **THEN** Science 收到 429，响应头里只多了 `retry-after: 17`，其余几个一个不带
- **AND** 正文照旧是 `api_error` 加上游原文

#### Scenario: retry-after 取值不合规

- **WHEN** 上游的 `retry-after` 既不是 1–5 位数字也不是 IMF-fixdate（例如 `1.5`、`123456`、带换行的值）
- **THEN** 回给 Science 的响应没有 `retry-after`，状态与正文与不带这个头时相同

### Requirement: 上游错误正文的调试输出

`CSSWITCH_DEBUG_UPSTREAM_ERROR=1` 时 SHALL 额外打印上游错误正文，但仅当上游真的返回了 HTTP 状态；
传输层失败的 detail 可能带 URL 凭证，那条路径 MUST 保持静默。

#### Scenario: 开着调试且上游返回了状态

- **WHEN** `CSSWITCH_DEBUG_UPSTREAM_ERROR=1` 且上游返回了 HTTP 错误状态
- **THEN** 日志里多出上游错误正文

#### Scenario: 开着调试但传输层失败

- **WHEN** `CSSWITCH_DEBUG_UPSTREAM_ERROR=1` 而请求在传输层失败
- **THEN** 不打印 detail

### Requirement: 凭证不进日志与控制面响应

凭证 MUST NOT 出现在日志或控制面响应里：配置接口只报告「是否已配置」。

#### Scenario: 读取渠道配置

- **WHEN** 请求控制面的配置或状态接口
- **THEN** 响应只说明 key 是否已配置，不含 key 本身

### Requirement: 官方直通诊断子命令

`csswitch-gateway official-passthrough --port N --log F` SHALL 是诊断子命令：catch-all 转发并记录脱敏的端点清单，
用于回归「Science 只调两个端点」这个结论。

#### Scenario: 回归端点清单

- **WHEN** 用 `official-passthrough` 跑一段 Science 会话
- **THEN** 日志文件里是脱敏后的端点清单
