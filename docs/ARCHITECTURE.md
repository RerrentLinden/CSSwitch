# 架构

## 一句话

一个本地 Rust 服务进程,把用户自己的 Claude Science 实例的推理流量转到
官方 Anthropic、Kimi 或 DeepSeek,并在必要处做窄补偿。同一个进程还能拉起一个免登录沙箱
(隔离的第二个 Science 实例),它自己选用 Kimi 或 DeepSeek,与正式实例互不影响。

## 进程与端口

```
安装版   CSSwitch.app ──双击──▶ CSSwitch(启动器角色,做完就退出)
                                  └─▶ CSSwitch serve --port N --log-file ~/.csswitch/service.log
                                      (脱离启动器,后台常驻)
从源码   ./csswitch ───────────▶ csswitch-gateway serve --port 8788(前台)

两条路起的是同一个 serve,它是唯一的常驻进程:
        监听 127.0.0.1:8788
                /                     WebUI(内嵌 HTML,单文件)
                /control/*            控制面 API(含 /control/quit 退出服务)
                /v1/messages          正式实例的推理
                /v1/models            正式实例的模型清单
                /sandbox/v1/messages  免登录沙箱的推理
                /sandbox/v1/models    免登录沙箱的模型清单
                其余                  显式 404(含 /sandbox、/sandbox/v1/别的、/sandboxx/…)
```

应用包里的 `CSSwitch` 与 `csswitch-gateway` 是同一个二进制,见下文「安装版与启动器」。

Science daemon 由服务用 CLI 拉起,两个实例拿到的 base_url 不同,网关就靠这一点分清请求来自谁:

| 实例 | 注入的 `ANTHROPIC_BASE_URL` | 请求落在 | 按哪个模式路由 |
| --- | --- | --- | --- |
| 正式实例 | `http://127.0.0.1:8788` | `/v1/*` | `mode` |
| 免登录沙箱 | `http://127.0.0.1:8788/sandbox` | `/sandbox/v1/*` | `sandbox_mode` |

前缀只在服务层路由这一处剥离(query 保留),推理层看到的路径与实例无关;常量只定义一次
(`sandbox::INFERENCE_PREFIX`),沙箱环境与路由共用。沙箱的 `https_proxy` / `HTTPS_PROXY`
(外联快速失败)仍是不带路径的网关地址。详见下文「两个实例各选各的」。

官方实例固定监听 `127.0.0.1:8686`(预览端口 8687),免登录沙箱监听 8790/8791。
手动 `claude-science serve` 仍用 Science 自己的默认 8000,与经 CSSwitch 启动的地址不同。
两者都绑 loopback。

启动参数(方括号里的一对只在联网搜索总开关关闭时出现,见下文「联网搜索总开关」):

```text
官方:serve [--config <镜像>] --port 8686 --sandbox-port 8687 --detached --no-browser
沙箱:serve --data-dir <沙箱> [--config <镜像>] --host 127.0.0.1 --port 8790 --sandbox-port 8791
       --no-browser --no-auto-update --detached
```

官方实例永不带 `--data-dir` 与 `--no-auto-update`:前者会造出隔离实例,后者会关掉它自己的自动更新。

退出服务(`/control/quit`,控制台按钮与 `./csswitch stop` 共用)先并行停掉正在运行的官方实例与
免登录沙箱,再退出进程:网关一走,指向它的实例只剩断掉的推理链路。停止失败不拦截退出,
失败原文随响应返回。终端里 Ctrl-C 只结束服务进程,不停实例。

日志的去向随形态不同:从源码运行时打到服务进程的 stderr(终端),不落文件;安装版没有终端,
落在 `~/.csswitch/service.log`(见「安装版与启动器」)。内容两者相同。
控制台「请求日志」卡片的「记录请求日志」开关(`service.v1.json` 的 `request_log`,默认开启,
切换立即生效)控制每条请求的明细:服务日志里的
`POST /v1/messages relay …` / `relay stream|nonstream rules=…` / Web Search 桥接统计行,
以及卡片里的请求列表。启动横幅、沙箱生命周期、警告与上游错误不受开关影响,始终输出。
卡片里的每条请求带着它来自哪个实例、走了哪个渠道(`正式 · kimi` / `沙箱 · deepseek`);
服务日志里的 relay 明细行不带实例,两个实例的请求路径在那里同为 `/v1/messages`。

## 安装版与启动器

`bash packaging/build_app.sh` 产出的应用包只有三样东西:

```text
CSSwitch.app/Contents/
├── Info.plist             LSUIElement=true(不进 Dock)
│                          LSEnvironment: CSSWITCH_APP_LAUNCH=1
├── MacOS/CSSwitch         csswitch-gateway 二进制本身,包内唯一的可执行文件
└── Resources/AppIcon.icns
```

同一个二进制既是启动器也是服务,由入口区分(`main.rs`):

| 调用方式 | 角色 |
| --- | --- |
| 无参数,且环境里 `CSSWITCH_APP_LAUNCH=1` | 启动器 |
| `launch` | 启动器(同上,供终端与测试显式调用) |
| `--version` | 打印版本号后退出 |
| 无参数 / `serve [--port N] [--log-file PATH]` | 前台服务 |
| `official-passthrough …` 及其余 | 不变 |

`CSSWITCH_APP_LAUNCH` 只由 Info.plist 设置,而且只在没有参数时起作用;启动器拉起服务时显式带
`serve` 并把这个变量从子进程环境里去掉。不用「可执行文件是否在 `.app` 里」来判断:那是隐式约定,
从终端直接运行包内二进制时行为会与预期相反。

**启动器做完就退出**(`launcher.rs`)。LaunchServices 以「应用主进程是否还活着」判断应用是否在
运行;主进程若就是常驻服务,第二次双击不会再执行任何代码,控制台也就打不开。流程:

```text
1. 端口取自 service.v1.json(默认 8788)
2. GET /control/status(回环,不走代理;连接超时 1 秒,等回话最长 5 秒)
     是 CSSwitch 的状态 ─┬ version 与自己相同 → 打开控制台 → 退出 0
                         └ 不同或没有        → 提示先退出旧服务 → 打开控制台 → 退出 0
     连不上 / 不是 CSSwitch → 继续
3. 读登录 shell 的环境,按白名单并进子进程环境
4. 拉起 <自己> serve --port N --log-file ~/.csswitch/service.log
     stdin=/dev/null;stdout / stderr=日志文件(追加);setsid 脱离启动器
5. 每 200 ms 查一次,最长 10 秒;子进程提前退出即失败
     就绪 → 打开控制台 → 退出 0
     失败 → 提示框(原因、日志路径、日志最后一行)→ 退出 1
```

- **登录 shell 环境**:从 Finder 启动的进程只有 launchd 的环境(PATH 只有系统目录,没有 shell 里
  配置的代理)。网关的上游请求只认环境变量代理,免登录沙箱按 PATH 找本机 `gh` —— 不处理的话,
  安装版会比 `./csswitch` 悄悄少功能。取法是 `$SHELL -l -i -c 'printf "%s\0" <标记>; /usr/bin/env -0'`
  (`SHELL` 缺失时用 `/bin/zsh`,独立会话,5 秒超时即结束),只解析标记之后的内容,只导入
  `PATH`、`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` 及其小写形式、`CLAUDE_SCIENCE_BIN`;
  shell 里有就覆盖继承值(置空也算有),没有就保留继承值。`ANTHROPIC_*` 等模型与凭证类变量不导入。
  超时、非零退出、找不到标记都不终止启动:沿用继承环境,并在日志里写明原因。日志只记导入了
  哪些变量名,不记值(代理地址可能带口令)。
- **什么算「服务已在跑」**:状态接口回 200、是 JSON 且有字符串 `mode`。别的程序占着端口时
  不算,启动器照常去拉起服务,由它撞上端口并把系统的原始报错带进提示框。
  没有监听时回环连接立刻被拒,不会等;连上之后给 5 秒,是因为状态接口要先执行一次
  `claude-science status` 才回话,冷启动时可能超过一秒 —— 等得太短会把正在运行的服务判成
  没在跑,随后的拉起必然因端口被占而失败。
- **版本不一致**:服务常驻后台,换了新版应用再双击,连上的仍是旧服务。状态接口带 `version`
  (`CARGO_PKG_VERSION`),启动器比对;不同、或更早的服务根本不报告版本号,都提示用户到控制台
  「退出服务」后重新打开。不自动重启:退出会停掉 Science 与沙箱、打断进行中的会话。
- **打开控制台与提示**用 `/usr/bin/open` 与 `/usr/bin/osascript`(`display alert`,标题与正文经
  argv 传入,不拼进脚本)。提示只出现在两种情况:服务起不来、版本不一致;提示框 120 秒无人
  理会自动消失,启动器不会一直挂着。`CSSWITCH_APP_HELPER_DIR` 可把这两个命令换成别的目录里的
  同名文件,只供隔离测试使用,生效时写日志。
- **日志落盘**(`service_log.rs`):带 `--log-file` 的服务把自己的 stdout / stderr 接到该文件
  (追加,0600,路径是符号链接时拒绝),并起一个后台线程每 60 秒检查一次正在写的那个文件,超过
  5 MiB 就改名为 `service.log.1`(覆盖上一份)、0600 新建同名文件、`dup2` 到 fd 1 / 2。
  所以上限是「5 MiB + 一分钟的写入量」再加一份历史。轮转必须在服务进程内做:fd 重定向之后,
  外部改名不会改变进程正在写的文件。启动器在 exec 之前也把这两个 fd 接到同一个文件,
  参数解析失败、panic 这类最早期的输出不会丢。不带 `--log-file` 时这些都不发生。
- **版本号**只有一个来源:`desktop/gateway/Cargo.toml`。它流向 `--version`、状态接口的
  `version`、控制台页眉,以及构建脚本写进 Info.plist 的两个版本键。

应用是 ad-hoc 签名(`codesign --sign -`),bundle id `io.github.rerrentlinden.csswitch`,架构跟随
构建机。没有 Developer ID 签名与公证,经下载分发时首次打开需要用户在系统设置里放行。

## 三种模式

| 模式 | `/v1/messages` | `/v1/models` |
| --- | --- | --- |
| 官方 | 零改写转发 `api.anthropic.com`,鉴权头原样透传 | 转发 |
| Kimi | relay 契约 + Kimi 补偿链 | 由用户四槽配置本地合成 |
| DeepSeek | deepseek 契约 + DeepSeek 补偿链 | 同上 |

模式是**每个实例各一个**,都存在配置里:正式实例是 `mode`(三种都可选),免登录沙箱是
`sandbox_mode`(只有 Kimi / DeepSeek,没有官方)。上表对两个实例同样适用,沙箱的路径多一段
`/sandbox` 前缀。渠道配置(地址 / API Key / 四槽)每个渠道一份,两个实例共用。
切换正式实例的模式需重启 Science daemon(它只在启动时读 base_url)。

## 两个实例各选各的

正式实例与免登录沙箱各自选择渠道,互不影响:可以正式实例走官方、沙箱走 Kimi,也可以两边各用
一个第三方渠道,或者选同一个。切换其中一个不改另一个的模式,也不重启另一个。

**沙箱没有官方这一项。** 沙箱的登录是 CSSwitch 在隔离目录里铸造的虚拟登录,没有真实的 Claude
凭证;官方模式是零改写直通、鉴权头原样透传,带着它去 `api.anthropic.com` 只会 401。把真实
登录借给沙箱不在选项里(不读 OAuth token 与 Keychain)。这条约束在两个入口归一:

- 读配置(`Profile::parse`):`sandbox_mode` 不存在或被手改成 `official` 时按下面的规则推导;
- 切换(`POST /control/sandbox/switch`):目标是 `official` 时拒绝并说明原因。

推理层里官方直通只对正式实例开放;沙箱的请求一律经 `GatewayConfig::for_channel` 装配渠道,
它对官方模式返回错误,所以沙箱一侧不存在通往官方直通的路径。

**旧配置没有 `sandbox_mode` 时**:正式实例的 `mode` 是 Kimi / DeepSeek 就沿用它(升级前沙箱
本来就跟着它走);是官方时取第一个配置完整的渠道(Kimi → DeepSeek),都不完整取 Kimi。
推导结果不立刻回写,之后任何一次存盘都会把它写出去。

**启动沙箱**(`POST /control/sandbox/start`)只看 `sandbox_mode` 对应的渠道是否配置完整,
不完整时报错点名渠道与缺项;正式实例处于什么模式与它无关。

**切换沙箱渠道**(`POST /control/sandbox/switch`,请求体 `{"mode": "kimi" | "deepseek", "force"?: bool}`):

```text
解析(official / 未知模式 / 非布尔 force → 拒绝)
  → 目标渠道配置完整性校验(不完整 → 拒绝,点名渠道)
  → 判定:
       目标 == 当前            不存盘、不重启
       沙箱没在运行            只存盘,下次启动沙箱时生效
       沙箱在运行、未带 force  拒绝(文案含「沙箱正在运行」,控制台据此弹确认框)
       沙箱在运行、带 force    存盘 → 停止沙箱 → 启动沙箱
```

- 一切拒绝都发生在存盘**之前**:被拒绝、或用户在确认框里取消时,配置与沙箱都不变。
- 沙箱在运行就必须重启:路由按 `sandbox_mode` 每请求生效,只存不重启会让运行中的沙箱立刻被
  改道到新渠道,而它手里的模型目录还是旧渠道的。沙箱没有可靠的会话数可查,所以不区分有没有
  会话 —— 只要在运行就先要确认。
- 重启失败不回滚(回滚再重启可能循环),报错写明渠道已保存为哪一个、停在了哪一步。
- 响应 `{"ok", "mode", "mode_label", "changed", "sandbox_restarted"}`;切换后不自动打开新标签页。

**状态接口**在顶层报告 `sandbox_mode` / `sandbox_mode_label`。键不叫 `mode`、也不放进
`sandbox` 对象:序列化保留写入顺序,而取顶层模式的脚本用的是贪婪匹配,任何更靠后的
`"mode":"…"` 都会被它当成正式实例的模式。

**两个渠道同时有流量是安全的**:渠道运行配置与模型缓存都是每请求新建,网关里没有按渠道共享的
缓存或会话状态。

**升级与回滚**:

- 升级时仍在运行的旧沙箱带的是不含前缀的 base_url,它的请求落在正式实例的路由上,继续跟着
  正式实例的模式走,与升级前相同;重启沙箱后才按 `sandbox_mode` 走。正常的升级路径
  (控制台「退出服务」)本来就会先停掉沙箱。
- 回滚到旧版本:旧二进制不认识 `sandbox_mode`,读入时忽略、下次存盘时丢掉,沙箱回到跟随
  正式实例的模式。新版本启动的沙箱要重启一次 —— 旧网关对 `/sandbox/v1/*` 回 404。

**证据边界**:「Science 把请求发到 `base_url + path`」来自对已安装的 0.1.60 二进制的只读静态
检查(SDK 的 `buildURL` 是直接拼接),旧版 CSSwitch 也给过 Science 带路径的 base_url。
离线门禁证明的是网关一侧:带前缀的两条路径按 `sandbox_mode` 路由、其余显式 404。真实 Science
带着前缀跑通一轮推理属于真机验收([test/LIVE_ACCEPTANCE.md](../test/LIVE_ACCEPTANCE.md) §G),
截至 2026-10-10 未执行。若真机发现 Science 不按带前缀的 base_url 发请求,备选是给沙箱开第二个
监听端口:只动沙箱环境与路由两处,用户可见行为不变。

## 联网搜索总开关

`service.v1.json` 顶层的 `web_search`(默认 `true`)是唯一真相,全局生效、不分渠道。
它对应 Science 0.1.56 起的实例级选项 `enable_web_search`:该选项写在 Science 的
`config.toml` 根表,只在启动时读取,没有应用内设置项也没有环境变量。一个值驱动两层:

```text
service.v1.json: web_search
        │
        ├─ Science 一侧(启动时一次性,science_config.rs)
        │     true  → 不带 --config,启动参数与没有这个开关时逐项相同
        │     false → 生成镜像配置 → serve --config <镜像> …
        │
        └─ 网关一侧(每请求读快照,config.rs / server.rs)
              true  → Kimi 的 typed web_search 走 query-tool bridge
              false → relay 入口摘除 typed web_search,桥不触发
```

**镜像配置**:源文件(只读)+ 根表 `enable_web_search = false`,由 `toml_edit` 解析后写出,
其余键、表与注释原样保留。

| | 官方实例 | 免登录沙箱 |
| --- | --- | --- |
| 源(只读) | `~/.claude-science/config.toml` | `<沙箱 data-dir>/config.toml` |
| 镜像(CSSwitch 写,0600) | `~/.csswitch/science-config/official.toml` | `~/.csswitch/science-config/sandbox.toml` |

- CSSwitch 对真实 `~/.claude-science/` 只读这一个文件,不写、不建任何东西。
- 镜像只会追加限制:开关开启时不带 `--config`,Science 自己配置里的任何值(包括手写的
  `enable_web_search = false`)原样生效。
- `--config` 只换配置文件,不换数据目录,登录与对话不受影响(2026-10-10 用 0.1.60 在隔离
  HOME 实测)。
- 每次启动都重新生成镜像;开启态启动时删除残留的镜像文件。源文件不存在时镜像只有这一个键;
  源文件读不了或解析不了时报错并拒绝启动,不猜着改。

**生效时机**:网关一侧存盘后下一次请求即生效,且不依赖 Science 是否已重启、是否认识这个选项。
Science 一侧要重启:`POST /control/web-search` 在官方实例运行时会重启它,有会话正在进行且未带
`force` 时拒绝并且不改任何状态;镜像生成不出来同样在存盘之前报错。重启失败不回滚,原文报出
(此时开关已改、Science 未按新设置运行;拨回开启再启动就是原始启动方式)。免登录沙箱不自动重启,
响应里用 `sandbox_restart_required` 标出。

官方模式的零改写直通不变:该模式下的关闭效果完全来自 Science 一侧。DeepSeek 平时把搜索声明
留给上游原生执行,关闭态同样在入口被摘除。

**回滚注意**:旧版本把这个开关存在渠道里(`kimi.web_search`)。新版本读到「顶层没有该键且
`kimi.web_search == false`」时迁移为总开关关闭,保存后只写顶层键。因此代码回滚到旧版本后,
旧二进制读不到渠道级旧键,会把「关」读成「开」。`~/.csswitch/science-config/` 是派生文件,
可直接删除。

## 端点合同的由来

2026-08 实测:Science 经 base_url 只调这两个端点,登录 / 会话存储 / 内核 /
Reviewer 都走 daemon 自身。所以没有第三类端点需要处理,其余一律显式 404 ——
新版本 Science 若开始调别的端点,会立刻暴露而不是被静默兜底。
免登录沙箱调的是同样两个端点,只是前面多一段 `/sandbox`;前缀下的其它路径同样显式 404。
`csswitch-gateway official-passthrough --port N --log F` 是诊断子命令,
catch-all 转发并记录脱敏端点清单,用于回归这个结论。

## 模型解析

用户在 WebUI 里为每个渠道填四个槽(默认 / 高质量 / 快速 / Fable),
每槽是「上游模型 ID + 显示名」,空槽继承默认槽。服务据此生成一份静态模型目录:

- 每个不同的模型 ID 产出一条 route,selector 形如 `claude-csswitch-<model-id>`;
- 四个角色(sonnet / opus / haiku / fable)绑到对应 selector;
- Science 发来的官方模型名(如 `claude-opus-5`)按角色映射到上游模型。

目录带指纹校验,生成与解析共用同一套算法。

## 补偿链

补偿都是窄规则,统一形态:端点/flavor 门控 + 规则 ID + 单测。
命中的规则会打进请求日志,便于判断"哪条在起效"。

- Kimi:`anthropic_compat.rs`(请求侧)+ `kimi_search_noise.rs`
  (搜索轮响应侧:噪声头 / 幻影对剥离、配对键采钥)
- DeepSeek:`deepseek_compat.rs`
- 策略(如 thinking 政策)来自 provider contract,不靠环境变量传递。
  环境变量只作显式 override 并留日志。

## 失败处理

- 上游错误保留 status,向客户端返回可诊断的错误体;不伪造成功。
- `CSSWITCH_DEBUG_UPSTREAM_ERROR=1` 时额外打印上游错误正文(仅当上游真的
  返回了 HTTP 状态;传输层失败的 detail 可能带 URL 凭证,那条路径保持静默)。
- 凭证从不出现在日志或控制面响应里:配置接口只报告"是否已配置"。

## 配置

`~/.csswitch/service.v1.json`(0600):`mode`(正式实例的模式)、`sandbox_mode`(免登录沙箱的
渠道,`kimi` | `deepseek`)、端口、两个渠道的 base_url / key / 四槽(两个实例共用)、
`request_log`(请求日志开关)、`web_search`(联网搜索总开关)。

`~/.csswitch/science-config/`(0700,文件 0600):联网搜索关闭时交给 Science 的镜像配置,
每次启动重写,见「联网搜索总开关」。

`~/.csswitch/service.log`、`service.log.1`(0600):安装版的服务日志,见「安装版与启动器」。
从源码运行时不产生。

文件名刻意避开 `config.json` —— 那是旧桌面端的文件名,它会把不认识的 schema
当作自己的旧版本迁移,结果清空自身 provider 列表(实测踩过一次)。
