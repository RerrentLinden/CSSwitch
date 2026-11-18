# sandbox-science-instance 设计注记

## 签名

```text
sandbox::start(base_url, &LaunchOverrides) → forge(幂等三态) → SSH/gh 桥接 → 沙箱钥匙串 → 端口检查
  → 镜像配置 → daemon → 健康 → 钥匙串校验
claude-science serve --data-dir <沙箱> --config <镜像> --host 127.0.0.1 --port 8790 --sandbox-port 8791
  --no-browser --no-auto-update --detached
  镜像: source = <沙箱 data-dir>/config.toml(只读)  mirror = ~/.csswitch/science-config/sandbox.toml
  env: HOME=<沙箱 home>  ANTHROPIC_BASE_URL=<网关>/sandbox  https_proxy=<网关>(不带前缀;CONNECT 到 Anthropic / Claude 主机 404,其余直连隧道)
       DO_NOT_TRACK=1(关 Science 的使用统计与错误上报)
       PATH=<沙箱>/bin:<继承>  GIT_CONFIG_COUNT/KEY_n/VALUE_n(本机装了 gh 时)
sandbox_forge::ensure_virtual_login(auth_dir, email, sandbox_root)  # 三道护栏先行

配置: service.v1.json 顶层 sandbox_mode = "kimi" | "deepseek"      # 与正式实例的 mode 互相独立
路由: /sandbox/v1/messages | /sandbox/v1/models → Instance::Sandbox  # 前缀在路由处剥掉
POST /control/sandbox/switch {"mode": "kimi"|"deepseek", "force"?: bool}
  → {"ok", "mode", "mode_label", "changed", "sandbox_restarted"}
GET  /control/status → 顶层 sandbox_mode / sandbox_mode_label
```

## 必需测试

- 护栏负例断言**零写入**（真实目录逐字节不变、符号链接目标不建目录）。
- 幂等三态：Reused 断言 `.enc` 与 `encryption.key` 逐字节不变；Repaired 断言 org 保留。
- 兜底停止的命令行匹配：正例命中、pid 复用/真实实例/前缀相似目录/瞬时 CLI 四组负例拒绝。
- 启动参数：不带镜像时与字面量逐项相等；带镜像时 `--config <镜像>` 恰好一次、去掉后与不带时相同，
  且带着它的命令行仍被兜底停止的匹配认出（沙箱实际启动时总是带着它，认不出就停不掉）。
- 入口链接主机名改写：localhost → 127.0.0.1、带路径前缀、已是 127.0.0.1 时幂等、
  非法 URL 原样返回，四例都钉住；并断言沙箱主机名不等于 `localhost`。
- gh 桥接：包装脚本行为级校验（假 gh 在沙箱 HOME 下被执行时看到的是真实 HOME、参数原样
  透传、临时文件已改名）；查找本机 gh 时跳过沙箱 bin 与不可执行文件；环境注入的 PATH 前置
  与 git 凭据「先清空再指向 gh」两对都钉住；已有 `GIT_CONFIG_COUNT` / PATH 缺失两个负例。
- 独立渠道：
  - 路由函数：四条推理路径（含 query，前缀被剥、query 保留）与近似路径
    （`/sandbox`、`/sandbox/`、`/sandbox/v1/unknown`、`/sandboxx/…`、叠两层前缀、`//sandbox/…`）。
  - `daemon_env`：base_url 带前缀，两个代理变量不带；带 `DO_NOT_TRACK`，且取值不是空串 /
    `0` / `false`（这三个 Science 当作没设）。
  - 切换判定的四条分支，拒绝文案含「沙箱正在运行」；请求解析拒绝 `official`、未知模式、
    非布尔 `force`。
  - 启动前置检查要抽成函数来测（`sandbox_start_precheck`）：正式实例为官方而沙箱渠道完整 →
    通过；沙箱渠道缺 Key → 报错点名渠道。检查内联在处理函数里时，把它改回看 `profile.mode`
    没有任何测试会红（变异检查实测）。
  - `Profile::parse` 的推导四例、显式值优先、手改成 `official` 被归一、存盘读回。
  - `for_channel(profile, mode)` 取传入的模式，不读 `profile.mode`。
  - loopback：两个入口的请求日志与本地合成的模型清单分别来自各自渠道；正式实例切到官方后
    沙箱入口仍走渠道（这时不得向正式入口发推理请求，那会连到真实官方接口）；切换被拒后配置
    逐字节不变；切换沙箱渠道不触发正式实例的 stop / serve；正式实例收到的 base_url 始终不带
    路径。带 `force` 的重启路径不进 loopback：真实启动要调 `security`、占固定端口。

## Wrong vs Correct

### Wrong

```rust
// 沙箱跟着全局模式走,与正式实例共用一个入口
// env: ANTHROPIC_BASE_URL=http://127.0.0.1:8788
let cfg = GatewayConfig::for_channel(&profile)?; // 内部读 profile.mode

// 切换时先存盘,再判断要不要确认:被拒绝时路由已经改了
profile.sandbox_mode = mode;
profile.save()?;
if running && !force {
    return Err(sandbox_running_refusal());
}
```

### Correct

```rust
// env: ANTHROPIC_BASE_URL=http://127.0.0.1:8788/sandbox(代理变量仍是不带路径的网关地址)
let mode = profile.mode_for(instance); // 沙箱 → sandbox_mode
let cfg = GatewayConfig::for_channel(&profile, mode)?; // 官方直通只给「正式实例 + 官方模式」

// 判定是纯函数,拒绝发生在存盘之前
match plan_sandbox_switch(&profile.sandbox_mode, &mode, running, force) {
    SandboxSwitch::NeedsConfirmation => return Err(sandbox_running_refusal()),
    // ...
}
profile.sandbox_mode = mode;
profile.save()?;
```

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
带着前缀跑通推理已在真机上确认(2026-10-10,安装版 0.10.0,Science 0.1.60,记录见
[test/LIVE_ACCEPTANCE.md](../../../test/LIVE_ACCEPTANCE.md) §G);其它版本的 Science 没有核对过。
若某个版本不按带前缀的 base_url 发请求,备选是给沙箱开第二个监听端口:只动沙箱环境与路由两处,
用户可见行为不变。

## 已知限制与证据边界

沙箱因此到不了 claude.ai 的技能注册表。Science 在写自定义技能(从 GitHub 导入、上传、新建草稿)
之前要用它核对重名,拿不到就拒绝,界面上是 `skill catalog unreachable`;GitHub 预览本身能用。
claude.ai 的地址写死在 Science 里(0.1.71 静态检查),没有可改的配置。

沙箱的环境里还带着 `DO_NOT_TRACK=1`。这是 Science 官方的遥测开关,关掉使用统计与错误上报,
只在 daemon 启动时读。沙箱的登录是铸造的,不以这个身份向 Anthropic 上报;错误上报的主机不在
快速失败代理拦截的域名里(它只拦 Anthropic / Claude 域名),所以要从源头关。正式实例不带这个变量,
它的遥测设置是用户自己的。依据是官方文档与对 0.1.60 二进制的只读静态检查(判定函数直接读进程
环境,空串、`0`、`false` 之外的取值都算开启);没有在真机上观察上报是否真的停了。
