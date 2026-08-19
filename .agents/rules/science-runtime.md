# Science runtime 规则

CSSwitch 接管的是**用户自己的官方 Science 实例**,不做数据隔离。以下都是硬约束,
违反会直接伤到用户的真实实例与真实数据。

## 启动

- 只用用户自己安装的 Science(PATH / `~/.claude-science/bin` / `~/.local/bin`);
  `CLAUDE_SCIENCE_BIN` 是显式开发 override,无效时 fail closed,不隐式回退别的候选。
- 启动参数固定为 `serve --port 0 --detached --no-browser`,**禁止**追加:
  - `--data-dir` / `--config`:会造出隔离实例,用户的对话从此分家;
  - `--no-auto-update`:官方实例必须保留它自己的自动更新。
- 只注入 `ANTHROPIC_BASE_URL`;启动前清空 `ANTHROPIC_MODEL` 系列与
  `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN`,否则 Science 会绕过网关的模型目录
  或带着旧凭证跑。
- 不写、不读、不伪造任何 auth 状态。用户的登录是他自己的。

## 边界

- Science 与 CSSwitch 服务都绑 loopback;引入或暗示 `0.0.0.0` 需要单独的安全与产品决策。
- 服务配置写 `~/.csswitch/service.v1.json`(0600)。**不得**使用
  `~/.csswitch/config.json`——那是旧桌面端的文件,它会把不认识的 schema
  当成自己的旧版本去迁移,结果清空自身 provider 列表(实测踩过)。
- Science 只在启动时读 base_url,所以切换供应商必须重启 daemon;
  切换前先校验渠道配置完整,别等到推理时才炸。

## 端点面

- Science 经 base_url 只调 `/v1/messages` 与 `/v1/models`(2026-08 实测)。
  登录、会话存储、内核、Reviewer 都走 daemon 自身。
- 其余路径显式 404。新版本 Science 若调用了没见过的端点,应当立刻可见,
  而不是被一个假的成功响应掩盖。诊断用 `official-passthrough` 子命令枚举端点面。

## 状态判定

- `status` 子命令总是退出 0,靠解析输出判断是否在跑;不能只看退出码。
- 已健康的 daemon 不因版本探测或可选功能漂移而强制重启。
