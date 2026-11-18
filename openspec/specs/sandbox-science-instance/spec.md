# sandbox-science-instance Specification

## Purpose

免登录沙箱是 CSSwitch 自有隔离目录里的第二个 Science 实例：全部写入落在 `~/.csswitch/science-sandbox/`，
用虚拟登录让用户不登录 Claude 账号也能用第三方渠道。本规格约束隔离实例的铸造、起停、钥匙串、桥接、遥测与独立渠道。
它与 [official-science-instance](../official-science-instance/spec.md) 的规则互不豁免——真实实例路径（`science.rs`）一行都不因沙箱而改变行为。
改动 `sandbox.rs` / `sandbox_forge.rs` 时适用。

## Requirements

### Requirement: 真实 Science 目录铁律

写入根解析后落在真实 `~/.claude-science` 之内或本身时 MUST 拒绝。这个检查 MUST 先于一切写入，
也先于「隔离根内」检查（防隔离根本身是符号链接指向真实树）。

#### Scenario: 隔离根是指向真实目录的符号链接

- **WHEN** 沙箱根解析后落在真实 `~/.claude-science` 之内
- **THEN** 操作被拒绝，零写入：真实目录逐字节不变，符号链接目标不被建目录

### Requirement: 虚拟登录身份不借用真实账号

假账号 email MUST 以 `localhost.invalid` 结尾。MUST NOT 从可解密 token 借 org 身份。
`orgs/` 多于一个候选时 MUST 直接报错，MUST NOT 静默选择。

#### Scenario: orgs 下有多个候选

- **WHEN** `orgs/` 多于一个候选
- **THEN** 铸造报错，不选择其中任何一个

### Requirement: 铸造写入的安全方式

全部写入 MUST 拒符号链接、用 `O_EXCL` 临时文件、rename、`0600`。删除 / 列举旧 `.enc` 失败 MUST 显式失败
（Science 预期 `.oauth-tokens/` 恰好一个 `.enc`）。

#### Scenario: 旧 .enc 删不掉

- **WHEN** 删除或列举旧 `.enc` 失败
- **THEN** 铸造显式失败

### Requirement: 铸造幂等三态

`forge` MUST 幂等，结果是 Reused / Repaired / 铸新三态之一。blob 被判 free、过期或结构损坏时 MUST NOT 复用，
SHALL 修复回 max 并沿用原 org。写后 MUST 解密回读自校验；契约漂移（Science 自更新后格式变更）导致自校验失败时 MUST 整体报错，
fail-closed，不静默兜底。

#### Scenario: 已有登录仍然有效

- **WHEN** 现有登录有效
- **THEN** 结果是 Reused，`.enc` 与 `encryption.key` 逐字节不变

#### Scenario: 已有登录需要修复

- **WHEN** blob 被判 free、过期或结构损坏
- **THEN** 结果是 Repaired，登录被修复回 max，org 保留

#### Scenario: 写后回读校验失败

- **WHEN** 写后解密回读自校验失败
- **THEN** 整体报错，不带病继续

### Requirement: 沙箱钥匙串

沙箱钥匙串 MUST 用随机密码（空密码在当前 macOS 解锁失败），密码文件落盘即 `0600`。forge 修复 / 铸新后 MUST 先清沙箱钥匙串旧条目
（Science 0.1.48 以 Keychain 为权威，不清会被回写吃掉修复形成死循环）。启动后 MUST 正向校验密钥确在沙箱钥匙串内，不过即停 daemon。

#### Scenario: 启动后钥匙串校验失败

- **WHEN** 启动后密钥不在沙箱钥匙串内
- **THEN** 先停 daemon 再报错，不带病运行

#### Scenario: 健康检查超时

- **WHEN** daemon 健康检查超时
- **THEN** 先停 daemon 再报错

### Requirement: 固定端口且只听回环

沙箱 MUST 用固定端口 8790 / 8791，占用即报错，MUST NOT 自动改绑；全部监听回环（`--host 127.0.0.1`）。

#### Scenario: 端口被占用

- **WHEN** 8790 或 8791 已被占用
- **THEN** 启动报错，不改用其它端口

### Requirement: 沙箱的启动参数

沙箱的启动参数 MUST 是 `serve --data-dir <沙箱> --config <镜像> --host 127.0.0.1 --port 8790 --sandbox-port 8791 --no-browser --no-auto-update --detached`。
启动覆盖项与官方实例走同一机制（[science-launch-overrides](../science-launch-overrides/spec.md)）；沙箱总是第三方模式，`default_model` 总有值，
所以实际启动时**总是**带一对 `--config <镜像>`（渠道默认槽为空的配置进不了启动流程）；数据目录仍由 `--data-dir` 钉死。
镜像 MUST 在钥匙串清理**之后**生成：forge 已重写登录而旧钥匙串条目还没清就中途退出，下次启动会判成 Reused 跳过清理。

启动流程的顺序是：forge（幂等三态）→ SSH / gh 桥接 → 沙箱钥匙串 → 端口检查 → 镜像配置 → daemon → 健康 → 钥匙串校验。

#### Scenario: 启动沙箱

- **WHEN** 沙箱以完整的渠道配置启动
- **THEN** 命令行带 `--data-dir <沙箱>` 与恰好一对 `--config <镜像>`，去掉这一对后与不带镜像时的字面量逐项相等

#### Scenario: 沙箱已在运行

- **WHEN** 沙箱已在运行时再次调用 `start`
- **THEN** 原样返回、不重启；开关的新值要等用户重启沙箱才到它那一侧（网关一侧的摘除不依赖它）

### Requirement: 入口链接用 127.0.0.1

沙箱对外入口链接的主机名 MUST 固定 `127.0.0.1`，`localhost` 留给真实实例。浏览器 cookie 只按主机名划作用域、不看端口，
而 Science 的 `operon_auth` / `operon_csrf` 都是 host-only，两实例同主机时后登录的一方会顶掉另一方。
daemon 的 Host 闸门与写操作 Origin 闸门本就同时放行这两个主机名，换主机名 MUST NOT 给 daemon 加任何参数；
改沙箱一侧而非真实实例（真实实例的 claude.ai OAuth 回调固定归一到 `localhost`）。

#### Scenario: daemon 给出 localhost 链接

- **WHEN** 沙箱 daemon 返回 `localhost` 的入口链接（可带路径前缀）
- **THEN** 链接被改写为 `127.0.0.1`

#### Scenario: 已是 127.0.0.1 或非法 URL

- **WHEN** 链接已是 `127.0.0.1`，或不是合法 URL
- **THEN** 改写幂等；非法 URL 原样返回

### Requirement: 兜底停止只杀确属沙箱的进程

兜底停止 MUST 只杀同时满足以下条件的进程：pid 来自沙箱 `operon.lock`、进程存活、argv[0] 是 `claude-science`、含 `serve`、
`--data-dir` 逐 token 精确等于沙箱目录。带 `--config <镜像>` 的命令行 MUST 仍被认出（沙箱实际启动时总是带着它，认不出就停不掉）。

#### Scenario: CLI 报未运行但进程仍活

- **WHEN** CLI 报 not running 而 lock 里的 pid 仍存活且匹配
- **THEN** 兜底 SIGTERM 收口，不漏杀

#### Scenario: 不属于沙箱的进程

- **WHEN** pid 被复用、是真实实例、数据目录只是前缀相似，或是瞬时 CLI
- **THEN** 进程不被杀

### Requirement: 跨出隔离边界的只有两个桥接

刻意跨出隔离边界的动作 MUST 只有 SSH 与 gh 两个桥接，二者都 MUST NOT 把真实登录钥匙串挂进沙箱搜索表
（真实实例的 Science 加密密钥也在里面，挂进去沙箱 daemon 就可能读到它）。

#### Scenario: 启动沙箱后的钥匙串搜索表

- **WHEN** 沙箱启动完成，两个桥接都已建立
- **THEN** 沙箱的钥匙串搜索表里没有真实登录钥匙串

### Requirement: SSH 桥接只链接两个非秘密文件

SSH 桥接 MUST 只 symlink 真实 `~/.ssh` 的 `config` 与 `known_hosts` 两个非秘密文件进沙箱 HOME
（Compute 按 `$HOME/.ssh/config` 找主机别名）。私钥 MUST 一律不链接（认证走继承的 `SSH_AUTH_SOCK` / 绝对 `IdentityFile`），
MUST NOT 修改源，目标已存在（含悬空链接）MUST NOT 覆盖。

#### Scenario: SSH 桥接

- **WHEN** 真实 `~/.ssh` 下有 `config`、`known_hosts` 与私钥
- **THEN** 沙箱 HOME 里只出现前两个文件的符号链接，私钥不被链接，源不被修改

#### Scenario: SSH 桥接目标已存在

- **WHEN** 沙箱 HOME 里的目标已存在（含悬空链接）
- **THEN** 不覆盖

### Requirement: gh 桥接用包装脚本

Science 的 Credentials 按 `gh auth token` 与 `git credential fill` 探测本机 GitHub 登录，而 gh 令牌默认在真实登录钥匙串里，只链接 `~/.config/gh` 拿不到。
gh 桥接 MUST 在 `<沙箱>/bin/gh` 写一个包装脚本（`HOME=<真实 home> exec <本机 gh> "$@"`，每次启动重写，本机没装 gh 时删除），
放在 daemon PATH 最前面，只让 gh 进程用真实 HOME。继承的 PATH 不可用时 MUST NOT 改 PATH（只剩包装目录会让 daemon 找不到系统命令）。
启动日志 MUST 记 `gh_bridge=on|off`。

#### Scenario: 本机装了 gh

- **WHEN** 本机装了 gh
- **THEN** 包装脚本被重写到 `<沙箱>/bin/gh`，gh 在沙箱 HOME 下被执行时看到的是真实 HOME，参数原样透传
- **AND** daemon 的 PATH 以 `<沙箱>/bin` 开头，启动日志记 `gh_bridge=on`

#### Scenario: 本机没装 gh

- **WHEN** 本机没装 gh
- **THEN** 包装脚本被删除，启动日志记 `gh_bridge=off`

#### Scenario: 继承的 PATH 不可用

- **WHEN** 继承的 PATH 不可用
- **THEN** 不改 PATH

### Requirement: git 凭据经环境指向 gh

git 访问 `https://github.com` / `https://gist.github.com` 时 MUST 经 `GIT_CONFIG_*` 环境先写空值清掉系统级 osxkeychain
（沙箱 HOME 下只查沙箱钥匙串），再指向 `gh auth git-credential`；MUST NOT 写沙箱 `~/.gitconfig`。
继承环境已有 `GIT_CONFIG_COUNT` 时 MUST NOT 覆盖。

#### Scenario: 本机装了 gh 时的 git 凭据

- **WHEN** 本机装了 gh，继承环境没有 `GIT_CONFIG_COUNT`
- **THEN** daemon 环境里的 git 凭据配置「先清空再指向 gh」，沙箱 `~/.gitconfig` 不被写入

#### Scenario: 继承环境已有 GIT_CONFIG_COUNT

- **WHEN** 继承环境已有 `GIT_CONFIG_COUNT`
- **THEN** 不覆盖它

### Requirement: 秘密不进日志与响应

token、key、钥匙串密码 MUST NOT 进日志或控制面响应；`security` 命令输出 MUST 全丢弃。

#### Scenario: 启动沙箱

- **WHEN** 沙箱启动并调用 `security`
- **THEN** 日志与控制面响应里没有 token、key 或钥匙串密码

### Requirement: 沙箱不向 Anthropic 上报

沙箱的 daemon 环境 MUST 带 `DO_NOT_TRACK=1`（Science 官方开关，关使用统计与错误上报，只在 daemon 启动时读）。
取值 MUST NOT 是空串、`0`、`false`：Science 0.1.60 的判定函数把这三个当作没设（静态检查）。
快速失败代理只拦 Anthropic / Claude 域名（`connect::is_blocked_host`），错误上报的主机不在其中，靠代理拦不住，所以从源头关。
这个变量 MUST 只加在沙箱一侧——真实实例的遥测设置是用户自己的，`science.rs` 的环境 MUST NOT 带它。

#### Scenario: 沙箱 daemon 的环境

- **WHEN** 沙箱 daemon 启动
- **THEN** 环境里 `DO_NOT_TRACK` 的取值不是空串、`0` 或 `false`

#### Scenario: 真实实例的环境

- **WHEN** 官方实例启动
- **THEN** 环境里没有 CSSwitch 加的 `DO_NOT_TRACK`

### Requirement: 沙箱请求靠路径前缀识别

网关 MUST 靠沙箱 base_url 的路径前缀 `/sandbox` 认出沙箱的请求，并在路由处剥掉前缀：
`/sandbox/v1/messages`、`/sandbox/v1/models` → `Instance::Sandbox`。沙箱入口之外的 `/sandbox…` 路径 MUST 显式 404，不进推理层。

依据是 Science 0.1.60 内置 SDK 以 `baseURL + path` 直接拼接（静态检查；2026-10-10 真机实测一致，记录见 `test/LIVE_ACCEPTANCE.md` §G）。

#### Scenario: 沙箱的推理请求

- **WHEN** 请求到达 `/sandbox/v1/messages` 或 `/sandbox/v1/models`（可带 query）
- **THEN** 请求归到沙箱实例，前缀被剥掉，query 保留

#### Scenario: 近似路径

- **WHEN** 请求路径是 `/sandbox`、`/sandbox/`、`/sandbox/v1/unknown`、`/sandboxx/…`、叠两层前缀或 `//sandbox/…`
- **THEN** 返回显式 404，不进推理层

### Requirement: 前缀只定义一次且只加在推理地址上

前缀 MUST 只定义一次（`sandbox::INFERENCE_PREFIX`，沙箱环境与路由共用），MUST 只加在 `ANTHROPIC_BASE_URL` 上，两个代理变量不加
（`https_proxy=<网关>`：CONNECT 到 Anthropic / Claude 主机 404，其余直连隧道）。
正式实例的 base_url MUST NOT 带路径，`science.rs` 不因此改动。沙箱 daemon 的完整环境见 [design.md](./design.md)「签名」。

#### Scenario: 沙箱 daemon 的地址变量

- **WHEN** 沙箱 daemon 启动
- **THEN** `ANTHROPIC_BASE_URL` 带 `/sandbox` 前缀，两个代理变量不带

#### Scenario: 正式实例的地址变量

- **WHEN** 官方实例启动
- **THEN** 它收到的 base_url 始终不带路径

### Requirement: 沙箱的渠道与正式实例各选各的

沙箱的渠道（`Profile.sandbox_mode`，`service.v1.json` 顶层 `sandbox_mode = "kimi" | "deepseek"`）MUST 与正式实例的 `Profile.mode` 互相独立。
`sandbox_mode` MUST 永不为官方：沙箱是虚拟登录，官方直通对它只会 401。两个入口归一：

- 读配置（`Profile::parse`）：键缺失或值为 `official` → 推导：`mode` 是渠道就取它，否则取第一个配置完整的渠道，都不完整取 Kimi；推导值不在读入时回写。
- 切换：拒绝官方并说明原因。

`handle_inference` 的官方直通 MUST 只对「正式实例 + 官方模式」开放；沙箱一侧一律走 `GatewayConfig::for_channel(profile, mode)`，它对官方模式返回错误。
启动沙箱 MUST 只看 `sandbox_mode` 对应的渠道是否完整，与正式实例的模式无关。

#### Scenario: 配置里没有 sandbox_mode

- **WHEN** 配置缺少 `sandbox_mode`，或其值被手改成 `official`
- **THEN** 取值被推导：`mode` 是渠道就取它，否则取第一个配置完整的渠道，都不完整取 Kimi
- **AND** 推导值不在读入时回写

#### Scenario: 显式值优先

- **WHEN** 配置里有合法的 `sandbox_mode`
- **THEN** 以它为准，存盘后读回一致

#### Scenario: 正式实例切到官方后

- **WHEN** 正式实例切到官方模式
- **THEN** 沙箱入口仍走它自己的渠道，请求日志与本地合成的模型清单来自各自渠道

#### Scenario: 沙箱渠道配置不完整时启动

- **WHEN** `sandbox_mode` 对应的渠道配置不完整时点启动
- **THEN** 启动前被拒绝，报错点名渠道与缺项，不看正式实例的模式

#### Scenario: 正式实例为官方而沙箱渠道完整

- **WHEN** 正式实例是官方模式，沙箱渠道配置完整
- **THEN** 沙箱启动前置检查通过

### Requirement: 切换沙箱渠道按固定顺序

`POST /control/sandbox/switch {"mode": "kimi"|"deepseek", "force"?: bool}` 的顺序 MUST 固定，与联网搜索总开关同一套：
解析 → 目标渠道 `validate()` → 纯函数判定 → 存盘并更新快照 → `stop` → `start`。一切拒绝 MUST 在存盘**之前**；
重启失败 MUST NOT 回滚，报错写明渠道已保存、停在了哪一步。响应是 `{"ok", "mode", "mode_label", "changed", "sandbox_restarted"}`。

沙箱只要在运行，切换就 MUST 先确认（未带 `force` → 拒绝）：路由按 `sandbox_mode` 每请求生效，只存不重启会让运行中的沙箱立刻被改道，
而它手里还是旧渠道的模型目录。沙箱没有可靠的会话数（`status --data-dir` 的输出形状没有实测记录），所以不区分有没有会话。

#### Scenario: 目标与当前相同

- **WHEN** 请求的渠道就是当前的 `sandbox_mode`
- **THEN** 不存盘、不重启

#### Scenario: 切换完成之后

- **WHEN** 沙箱渠道切换成功
- **THEN** 不自动打开新标签页

#### Scenario: 切成官方

- **WHEN** 请求把沙箱渠道切成 `official`
- **THEN** 请求被拒绝并说明原因（虚拟登录，官方接口会拒绝它），不改任何状态

#### Scenario: 切到配置不完整的渠道

- **WHEN** 目标渠道配置不完整
- **THEN** 请求被拒绝，不改任何状态

#### Scenario: 沙箱在运行且未确认

- **WHEN** 沙箱在运行且请求未带 `force`
- **THEN** 请求被拒绝，文案含「沙箱正在运行」（控制台据此转确认框），配置逐字节不变，沙箱不变

#### Scenario: 沙箱未运行

- **WHEN** 沙箱未运行时切换渠道
- **THEN** 只保存，正式实例的模式不变，不触发正式实例的 stop / serve

#### Scenario: 切换后重启失败

- **WHEN** 存盘后停止或启动沙箱失败
- **THEN** 不回滚，原文报出，并写明渠道已保存、沙箱停在哪一步

#### Scenario: 非法请求体

- **WHEN** 请求的 `mode` 未知，或 `force` 不是布尔
- **THEN** 请求被拒绝

### Requirement: 状态接口与日志的字段

`GET /control/status` MUST 在顶层给出 `sandbox_mode` / `sandbox_mode_label`。状态接口里新增的键 MUST NOT 叫 `mode`：
`serde_json` 开了 `preserve_order`，`test/run_loopback.sh` 用贪婪正则取顶层 `"mode":"…"`，更靠后的同名键会被它取走。
请求日志条目 MUST 带 `instance`（`official` / `sandbox`）；路径一栏是剥掉前缀之后的。

#### Scenario: 读取状态

- **WHEN** 请求 `GET /control/status`
- **THEN** 顶层有 `sandbox_mode` 与 `sandbox_mode_label`，除正式实例的模式外没有别的键叫 `mode`

#### Scenario: 两个入口各发一次请求

- **WHEN** 正式入口与沙箱入口各收到一次请求
- **THEN** 日志条目的 `instance` 分别是 `official` 与 `sandbox`，路径不带 `/sandbox` 前缀
