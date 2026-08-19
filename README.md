# CSSwitch

给 [Claude Science](https://claude.com) 换模型供应商的本地小工具:官方 Claude、
Kimi、DeepSeek 三选一,配置在浏览器里点,对话仍然留在你自己的 Science 实例里。

## 它是什么形态

一个本地服务进程 + 一个浏览器控制台。没有安装包,没有常驻图标:
跑一个脚本,浏览器里操作,用完在控制台点「退出服务」。

```
Claude Science(你自己的实例、你自己的登录)
        │  ANTHROPIC_BASE_URL=http://127.0.0.1:8788
        ▼
CSSwitch 服务  ── /v1/messages、/v1/models  按当前模式路由
        │       ── /control/*               控制台后端
        │       ── /                        WebUI
        ▼
官方 api.anthropic.com  ·  Kimi  ·  DeepSeek
```

关键点:**不做数据隔离**。CSSwitch 启动的是你平时用的那个 Science
(`~/.claude-science`,真实登录),只在启动时注入一个 base_url。
所以历史对话、项目、MCP、Skill 全都还在原处,换供应商不会让对话分家。

## 用法

```bash
./csswitch
```

首次运行会自己构建,随后打开控制台。在「渠道配置」里填好 base_url 与 API Key,
点上方的模式卡片切换。切换会重启 Science daemon(它只在启动时读 base_url),
控制台会给出新的登录链接。

```bash
./csswitch status    # 当前模式与 Science 状态
./csswitch stop      # 停止服务(等同控制台里的「退出服务」)
```

端口默认 8788,`CSSWITCH_PORT` 可改。

## 模型配置

每个渠道有四个槽:默认(均衡)、高质量、快速、Fable。
默认槽必填,其余留空自动继承默认槽。每槽可分别填上游模型 ID 与显示名——
显示名就是 Science 模型菜单里看到的名字。「获取可用模型」直接向上游要清单。

## 供应商补偿

第三方端点与官方 Anthropic API 有真实差异,直接转发会 400。补偿都是窄规则,
带规则 ID 和端点门控,日志里能看到哪条生效:

- **Kimi**:非标准 `thinking: auto` 剥离(K3 收到它会静默不思考)、
  指定型 `tool_choice` 与思考冲突时禁思考、`document` 块换署名占位文本、
  声明 web_search 却未搜索时的 429 用客户端工具桥接绕开。
- **DeepSeek**:`thinking auto` → `adaptive`、tool_choice 与思考互斥、
  thinking disabled 与 effort 互斥、带 tool_use 的历史补 thinking 块、
  畸形 server tool 块修复、孤儿工具配对按计数补齐。
  语义借鉴自 [cc-switch](https://github.com/farion1231/cc-switch)(MIT)及
  biociao 的 fork,按本仓库的规则体系重写。

补偿策略来自 provider contract,不靠环境变量传递。

## 开发

```bash
bash test/run_all.sh     # 三层门禁:fmt+clippy / 单测 / 真实服务进程 loopback
```

真机验收(官方登录、真实 provider key、工具轮次)见 [test/LIVE_ACCEPTANCE.md](test/LIVE_ACCEPTANCE.md)。

架构与开发约定见 [docs/](docs/README.md);Agent 规则见 [AGENTS.md](AGENTS.md)。
