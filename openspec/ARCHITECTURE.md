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

应用包里的 `CSSwitch` 与 `csswitch-gateway` 是同一个二进制。

Science daemon 由服务用 CLI 拉起,两个实例拿到的 base_url 不同,网关就靠这一点分清请求来自谁:

| 实例 | 注入的 `ANTHROPIC_BASE_URL` | 请求落在 | 按哪个模式路由 |
| --- | --- | --- | --- |
| 正式实例 | `http://127.0.0.1:8788` | `/v1/*` | `mode` |
| 免登录沙箱 | `http://127.0.0.1:8788/sandbox` | `/sandbox/v1/*` | `sandbox_mode` |

官方实例固定监听 `127.0.0.1:8686`(预览端口 8687),免登录沙箱监听 8790/8791。
手动 `claude-science serve` 仍用 Science 自己的默认 8000,与经 CSSwitch 启动的地址不同。
两者都绑 loopback。

## 往哪里看

行为合同在 [`openspec/specs/`](specs/),每个能力一个目录:`spec.md` 是需求与场景,
`design.md` 是由来、实测证据、必需测试与反例。

| 想了解 | 能力 |
| --- | --- |
| 路由表、三种模式、CONNECT 隧道、上游失败与凭证的处理 | [gateway-routing](specs/gateway-routing/spec.md) |
| 官方实例的启动参数、环境、切换与退出 | [official-science-instance](specs/official-science-instance/spec.md) |
| 镜像配置、联网搜索总开关(Science 一侧) | [science-launch-overrides](specs/science-launch-overrides/spec.md) |
| 免登录沙箱、两个实例各选各的 | [sandbox-science-instance](specs/sandbox-science-instance/spec.md) |
| 四槽、模型名解析、模型清单 | [model-menu](specs/model-menu/spec.md) |
| Kimi 渠道的契约与补偿 | [kimi-channel](specs/kimi-channel/spec.md) |
| Kimi 联网搜索的桥接、配对键与尾随上下文 | [kimi-web-search-bridge](specs/kimi-web-search-bridge/spec.md)、[kimi-search-pair-adoption](specs/kimi-search-pair-adoption/spec.md)、[kimi-context-tail-reorder](specs/kimi-context-tail-reorder/spec.md) |
| DeepSeek 渠道的契约与补偿 | [deepseek-channel](specs/deepseek-channel/spec.md) |
| 补偿的通用规则、Science 发出的消息形状 | [provider-compensation](specs/provider-compensation/spec.md) |
| 向 SSE 流注入内容块 | [sse-block-injection](specs/sse-block-injection/spec.md) |
| 安装版、启动器、日志落盘 | [installed-app-launcher](specs/installed-app-launcher/spec.md) |
| 请求日志 | [request-log](specs/request-log/spec.md) |
| 配置文件 | [config-storage](specs/config-storage/spec.md) |
| 控制台 | [console-webui](specs/console-webui/spec.md) |
