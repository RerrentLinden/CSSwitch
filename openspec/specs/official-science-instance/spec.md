# official-science-instance Specification

## Purpose

CSSwitch 接管的是**用户自己的** Claude Science 实例、自己的登录、自己的对话库，不做数据隔离。
本规格约束 Science daemon 的启动参数、环境、状态判定、切换与退出：这里每一条被违反，损害的都是用户的真实数据。
改动 Science daemon 启动、停止、状态判定或配置存储时适用。启动覆盖项（镜像配置、联网搜索总开关）见
[science-launch-overrides](../science-launch-overrides/spec.md)。

## Requirements

### Requirement: 启动参数只有固定的一组

官方实例的启动参数 MUST 恰好是 `serve --port 8686 --sandbox-port 8687 --detached --no-browser`，不多不少；
只允许下面两种追加：`serve` 之后的一对 `--config <镜像>`，与末尾的 `--dangerously-skip-approvals`。

#### Scenario: 官方模式且联网搜索开启

- **WHEN** 实例处于官方模式、联网搜索开启、跳过审批关闭
- **THEN** 启动参数与字面量 `serve --port 8686 --sandbox-port 8687 --detached --no-browser` 逐项相等

### Requirement: 永不隔离也永不关自动更新

`--data-dir` MUST 永远禁止：它造出隔离实例，用户对话从此分家。
`--no-auto-update` MUST 永远禁止：官方实例失去自动更新（用户明确红线）。

#### Scenario: 任何状态下都不隔离、不关自动更新

- **WHEN** 以任意模式与开关组合启动官方实例
- **THEN** 参数里不出现 `--data-dir`，也不出现 `--no-auto-update`

#### Scenario: 用户看到的结果

- **WHEN** 官方模式下打开 Science
- **THEN** 设置页 About 显示自动更新可用，历史项目与会话完整

### Requirement: 镜像配置参数只在需要时出现

`--config` MUST 只指向 CSSwitch 生成的镜像，且只在「联网搜索关闭」或「这个实例处于第三方模式」时出现。
它只换配置文件、不换数据目录（2026-10-10 用 0.1.60 隔离实测）；官方模式 + 联网搜索开启时不带它，启动参数与没有这些功能时逐项相同。

#### Scenario: 需要镜像配置

- **WHEN** 联网搜索关闭，或实例处于第三方模式
- **THEN** `serve` 之后多一对 `--config <CSSwitch 生成的镜像>`，恰好出现一次
- **AND** 去掉这一对后与不带镜像时的参数相同

### Requirement: 跳过审批参数跟随总开关

`--dangerously-skip-approvals` MUST 只在「跳过审批」总开关开着时追加在末尾（两个实例同一个值，`science::SKIP_APPROVALS_FLAG` 只定义一次）。
这是用户明确要的 bypass 模式（Science 官方开关，权限卡一律自动批准）；总开关 `skip_approvals` 默认关，关着时参数逐项不变。
它不进镜像、不影响 `needs_mirror()`。切换规矩与联网搜索总开关相同。

#### Scenario: 跳过审批总开关开着

- **WHEN** 「跳过审批」总开关开着
- **THEN** 参数末尾多一个 `--dangerously-skip-approvals`，其余逐项不变

#### Scenario: 跳过审批总开关关着

- **WHEN** 「跳过审批」总开关关着
- **THEN** 参数里没有 `--dangerously-skip-approvals`

### Requirement: 端口固定

`--port` / `--sandbox-port` MUST 固定 8686 / 8687。不用 `0`（控制台地址要稳定可收藏），也不沿用 Science 默认的 8000 / 8001
（常被本机其它开发服务占用）；两个端口都 MUST 显式给出，不依赖「port+1」的推导。
手动 `claude-science serve` 仍用 Science 自己的默认 8000，与经 CSSwitch 启动的地址不同；两者都绑 loopback。

#### Scenario: 启动官方实例

- **WHEN** 官方实例启动
- **THEN** 参数里显式带 `--port 8686` 与 `--sandbox-port 8687`

### Requirement: 启动环境只注入网关地址

启动 daemon 时 MUST 只注入 `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>` 一个变量，并 MUST 清除模型类与凭证类环境变量：
`ANTHROPIC_MODEL`、`*_MODEL`、`*_MODEL_NAME`、`ANTHROPIC_API_KEY`、`ANTHROPIC_AUTH_TOKEN`。
残留会让 Science 绕过网关的模型目录，或带着旧凭证启动。

#### Scenario: 继承环境里有模型与凭证变量

- **WHEN** 服务进程的环境里带着模型类或凭证类变量
- **THEN** daemon 的环境里没有它们
- **AND** `ANTHROPIC_BASE_URL` 指向网关（它不在清除列表里）

### Requirement: 不触碰 auth 状态

CSSwitch MUST NOT 写、读或伪造官方实例的任何 auth 状态。

#### Scenario: 切换模式后登录状态

- **WHEN** 用户切换模式
- **THEN** 用户拿到新的登录链接，旧会话仍在列表里，CSSwitch 没有改动任何 auth 状态

### Requirement: 运行状态按输出判定

`claude-science status` 总是退出 0。判断 daemon 是否在运行 MUST 解析命令输出，MUST NOT 依据退出码。

#### Scenario: daemon 未运行

- **WHEN** daemon 未运行而 `status` 仍以 0 退出
- **THEN** CSSwitch 按输出内容判定为未运行

### Requirement: 切换供应商要重启且先确认

`base_url` 只在 daemon 启动时读取，切换供应商 MUST 重启 daemon。重启会打断进行中的会话
（Science 侧显示 `This session was interrupted by a restart`），因此有活跃会话且请求未带 `force` 时 MUST 拒绝切换。

#### Scenario: 有活跃会话且未确认

- **WHEN** `daemon.active_conversations > 0` 且请求未带 `force`
- **THEN** 切换被拒绝并报告会话数，不静默打断
- **AND** 配置与 Science 都不变

#### Scenario: 已确认

- **WHEN** 有活跃会话且请求带 `force`
- **THEN** daemon 被重启并使用新的供应商

### Requirement: 渠道配置不完整时拒绝切换

渠道配置不完整时 MUST 在切换前就拒绝，MUST NOT 等到推理时才 400。

#### Scenario: 切到配置不完整的渠道

- **WHEN** 用户切换到一个配置不完整的渠道
- **THEN** 切换在发生前被拒绝

### Requirement: 找不到 claude-science 时明确失败

找不到 `claude-science` 时 MUST 明确报错并列出已查找路径。`CLAUDE_SCIENCE_BIN` 无效时 MUST fail closed，
MUST NOT 回退到其它候选。

#### Scenario: 本机没有 claude-science

- **WHEN** 所有候选路径都找不到 `claude-science`
- **THEN** 报错里列出已查找的路径

#### Scenario: CLAUDE_SCIENCE_BIN 无效

- **WHEN** `CLAUDE_SCIENCE_BIN` 指向无效路径
- **THEN** 启动失败，不使用其它候选

### Requirement: 停止未运行的 daemon 视为成功

daemon 未运行时执行 stop SHALL 视为成功（stderr 含 not running / no daemon）。

#### Scenario: 对未运行的 daemon 执行 stop

- **WHEN** stop 的 stderr 含 `not running` 或 `no daemon`
- **THEN** 结果是成功

### Requirement: 退出服务先停实例

退出服务（`/control/quit`，控制台按钮与 `./csswitch stop` 共用）MUST 先并行停掉**正在运行的**官方实例与沙箱，再退出进程。
未安装 / 未运行不算失败；沙箱未运行时不碰其目录；停止失败 MUST NOT 拦截退出，原文随响应的 `errors` 返回。

#### Scenario: 两个实例都在运行

- **WHEN** 官方实例与沙箱都在运行时收到 `/control/quit`
- **THEN** 两个实例被并行停止，随后服务进程退出

#### Scenario: 实例未安装或未运行

- **WHEN** Science 未安装或未运行
- **THEN** 退出照常进行，不报失败，沙箱目录不被触碰

#### Scenario: 停止失败

- **WHEN** 某个实例停止失败
- **THEN** 服务仍然退出，失败原文在响应的 `errors` 里

#### Scenario: 终端里按 Ctrl-C

- **WHEN** 用户在终端里用 Ctrl-C 结束服务进程
- **THEN** 只有服务进程结束，实例不被停止
