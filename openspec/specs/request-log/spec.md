# request-log Specification

## Purpose

服务会记录每条推理请求走了哪个渠道、命中了哪些补偿规则，供控制台「请求日志」卡片与服务日志使用。
本规格约束日志的去向、请求明细的开关，以及明细里带什么、不带什么。
改动 `log_line!` / `request_log_line!` 的调用点、控制台的请求日志卡片或 `request_log` 配置时适用。
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

### Requirement: 明细里的实例与渠道

卡片里的每条请求 MUST 带着它来自哪个实例、走了哪个渠道（`正式 · kimi` / `沙箱 · deepseek`）。
服务日志里的 relay 明细行不带实例，两个实例的请求路径在那里同为 `/v1/messages`。

#### Scenario: 沙箱发来一次请求

- **WHEN** 沙箱经 `/sandbox/v1/messages` 发来请求
- **THEN** 卡片里这一条显示「沙箱 · <渠道>」，服务日志的明细行里路径是 `/v1/messages`

### Requirement: 桥接统计行不记录对话内容

Web Search 桥接统计行 MUST 只记录 rule / model、bridged / query / call 数与阶段耗时，MUST NOT 记录 query、结果或正文。

#### Scenario: 一次搜索轮

- **WHEN** 一次请求经过 Web Search 桥接
- **THEN** 日志只有 rule / model、bridged / query / call 数与阶段耗时
