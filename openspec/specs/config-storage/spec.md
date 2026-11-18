# config-storage Specification

## Purpose

CSSwitch 的服务配置落在 `~/.csswitch/` 下，这个目录同时被旧桌面端使用。本规格约束配置文件的命名、
回落与权限，保证新服务不会让同目录下的既有应用损坏自己的数据。新组件要在既有目录下落配置时适用。

## Requirements

### Requirement: 配置文件名不与既有应用重名

服务配置 MUST 写在 `~/.csswitch/service.v1.json`，MUST NOT 写出 `~/.csswitch/config.json`。
文件名里带版本号，为将来的 schema 变更留出并存空间。

旧桌面端拥有 `~/.csswitch/config.json`，它会把任何不认识的 schema 当成自己的**旧版本**去迁移。

#### Scenario: 保存配置后不出现旧应用的文件名

- **WHEN** 在隔离 HOME 内保存配置
- **THEN** `~/.csswitch/service.v1.json` 存在
- **AND** `~/.csswitch/config.json` 不存在

### Requirement: 读不到配置时回落到默认值

配置文件不存在或解析失败时，服务 MUST 回落到默认值，MUST NOT 猜测或尝试迁移别人的 schema。

#### Scenario: 配置文件不存在

- **WHEN** `service.v1.json` 不存在
- **THEN** 服务以默认值运行

#### Scenario: 配置文件解析失败

- **WHEN** `service.v1.json` 无法解析
- **THEN** 服务以默认值运行，不读取也不迁移同目录下其它应用的配置

### Requirement: 配置文件权限收紧

每次写入配置后，文件权限 MUST 是 `0600`。

#### Scenario: 写入后的权限

- **WHEN** 配置被写入
- **THEN** `service.v1.json` 的权限是 `0600`

### Requirement: 配置文件的内容

`~/.csswitch/service.v1.json` MUST 保存：`mode`（正式实例的模式）、`sandbox_mode`（免登录沙箱的渠道，`kimi` | `deepseek`）、端口、
两个渠道的 base_url / key / 四槽（两个实例共用）、`request_log`（请求日志开关）、`web_search`（联网搜索总开关）、
`skip_approvals`（跳过审批总开关）。`skip_approvals` 关着时 MUST NOT 写进文件。

#### Scenario: 跳过审批关着时保存

- **WHEN** 「跳过审批」总开关关着时保存配置
- **THEN** 文件里没有 `skip_approvals` 键

#### Scenario: 两个实例共用渠道配置

- **WHEN** 正式实例与沙箱选了同一个渠道
- **THEN** 它们用的是同一份 base_url、key 与四槽

### Requirement: 目录下的其它文件

`~/.csswitch/science-config/`（`0700`，文件 `0600`）MUST 只放交给 Science 的镜像配置，每次启动重写。
`~/.csswitch/service.log` 与 `service.log.1`（`0600`）是安装版的服务日志；从源码运行时 MUST NOT 产生。

#### Scenario: 从源码运行

- **WHEN** 用 `./csswitch` 运行服务
- **THEN** `~/.csswitch/` 下不产生 `service.log`
