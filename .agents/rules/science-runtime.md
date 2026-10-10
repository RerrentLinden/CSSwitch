# Science runtime 规则

CSSwitch 接管的是**用户自己的官方 Science 实例**,不做数据隔离。以下都是硬约束,
违反会直接伤到用户的真实实例与真实数据。

## 启动

- 只用用户自己安装的 Science(PATH / `~/.claude-science/bin` / `~/.local/bin`);
  `CLAUDE_SCIENCE_BIN` 是显式开发 override,无效时 fail closed,不隐式回退别的候选。
- 启动参数固定为 `serve [--config <镜像>] --port 8686 --sandbox-port 8687 --detached --no-browser`,
  端口不用 `0`(控制台地址要稳定可收藏),也不沿用 Science 默认的 8000/8001。
  - `--config` 只允许出现在联网搜索关闭态,且只指向 CSSwitch 生成的镜像
    (`~/.csswitch/science-config/official.toml` = 用户原配置 + 根表 `enable_web_search = false`)。
    联网搜索开启时不得出现,启动参数与没有这个开关时逐项相同。它只换配置文件、不换数据目录
    (2026-10-10 用 0.1.60 隔离实测;此前把它与 `--data-dir` 并列为「会造出隔离实例」的说法
    对 `--config` 单独不成立)。
  - `--data-dir`、`--no-auto-update` 对官方实例仍然**禁止**:前者会造出隔离实例,用户的对话
    从此分家;后者会关掉官方实例自己的自动更新。
- 环境变量只注入 `ANTHROPIC_BASE_URL`,值是**不带路径**的网关地址(`http://127.0.0.1:<port>`);
  启动前清空 `ANTHROPIC_MODEL` 系列与 `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`,
  否则 Science 会绕过网关的模型目录或带着旧凭证跑。带路径前缀的 base_url 是免登录沙箱的
  (见下文),官方实例的启动方式不因沙箱而改动一项。
- 不写、不读、不伪造任何 auth 状态。用户的登录是他自己的。
- 对真实 `~/.claude-science/` 不写、不建任何文件;只读其中的 `config.toml` 一个文件
  (生成镜像用)。该文件读不了或解析不了时报错并拒绝启动,不猜着改、不回退成空配置。

## 边界

- Science 与 CSSwitch 服务都绑 loopback;引入或暗示 `0.0.0.0` 需要单独的安全与产品决策。
- 服务配置写 `~/.csswitch/service.v1.json`(0600)。**不得**使用
  `~/.csswitch/config.json`——那是旧桌面端的文件,它会把不认识的 schema
  当成自己的旧版本去迁移,结果清空自身 provider 列表(实测踩过)。
- Science 只在启动时读 base_url,所以切换供应商必须重启 daemon;
  切换前先校验渠道配置完整,别等到推理时才炸。
- Science 的 `config.toml` 同样只在启动时读,所以切换联网搜索总开关也要重启官方实例;
  有会话正在进行且未确认时拒绝,并且不改任何状态。免登录沙箱不自动重启,只提示需要重启。
- 镜像配置写 `~/.csswitch/science-config/`(目录 0700、文件 0600,每次启动重写)。
  它带着用户原配置的全部内容,解析报错只给路径、行列与解析器原话,不带出错行的内容。

## 免登录沙箱的渠道

正式实例与免登录沙箱各有自己的模式(`service.v1.json` 的 `mode` 与 `sandbox_mode`),互不影响;
渠道配置每个渠道一份,两个实例共用。

- 沙箱的 `ANTHROPIC_BASE_URL` 是「网关地址 + `/sandbox`」,网关靠这段路径前缀认出请求来自沙箱,
  按 `sandbox_mode` 路由。前缀常量只定义一次(`sandbox::INFERENCE_PREFIX`),沙箱环境与路由共用;
  只加在 base_url 上,沙箱的 `https_proxy` / `HTTPS_PROXY` 仍是不带路径的网关地址。
- **沙箱永不为官方**:它的登录是 CSSwitch 铸造的虚拟登录,没有真实 Claude 凭证,官方接口必然
  拒绝。读配置时归一(键缺失或被手改成 `official` → 推导成渠道),切换时拒绝并说明原因;
  推理层的官方直通只对正式实例开放。不得为了让沙箱走官方而读取或借用真实登录。
- 启动沙箱只看 `sandbox_mode` 对应的渠道是否配置完整(报错点名渠道与缺项),
  不要求正式实例处于第三方模式。
- 切换沙箱渠道(`POST /control/sandbox/switch`):解析、渠道校验、判定都在存盘**之前**,
  被拒绝或用户取消时任何状态都不变。沙箱没在运行 → 只存盘;沙箱在运行 → 必须重启它
  (路由按 `sandbox_mode` 每请求生效,不重启会让运行中的沙箱带着旧渠道的模型目录被改道),
  而且只要在运行就先要确认(沙箱没有可靠的会话数可查);确认后存盘 → 停止 → 启动,
  重启失败不回滚,原文报出当前真实状态。
- 两个方向都不串:切换正式实例的模式不改沙箱的渠道、不重启沙箱;切换沙箱的渠道不改正式实例的
  模式、不重启正式实例。
- 状态接口用顶层键 `sandbox_mode` / `sandbox_mode_label` 报告沙箱渠道。状态里不得再出现第二个
  名为 `mode` 的键(序列化保留顺序,取顶层模式的脚本是贪婪匹配)。

## 端点面

- Science 经 base_url 只调 `/v1/messages` 与 `/v1/models`(2026-08 实测)。
  登录、会话存储、内核、Reviewer 都走 daemon 自身。
- 免登录沙箱调的是同样两个端点,落在 `/sandbox/v1/messages` 与 `/sandbox/v1/models`
  (Science 0.1.60 的 SDK 是 `baseURL + path` 直接拼接;2026-10-10 真机实测沙箱的请求确实落在
  这两条路径上,记录见 `test/LIVE_ACCEPTANCE.md` §G)。路径整条相等才算,不做前缀匹配。
- 其余路径显式 404(含 `/sandbox`、`/sandbox/v1/别的`、`/sandboxx/…`)。新版本 Science
  若调用了没见过的端点,应当立刻可见,而不是被一个假的成功响应掩盖。
  诊断用 `official-passthrough` 子命令枚举端点面。

## 状态判定

- `status` 子命令总是退出 0,靠解析输出判断是否在跑;不能只看退出码。
- 已健康的 daemon 不因版本探测或可选功能漂移而强制重启。
