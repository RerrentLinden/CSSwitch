# 真机验收清单

`test/run_all.sh` 的三层全部离线。下面这些只能在真实环境里得到答案:官方登录态、
第三方端点的真实行为、Science 的真实工具轮次。每次改动 provider 补偿链或
daemon 控制逻辑后跑一遍。

前提:本机已安装并登录 Claude Science;需要时由用户提供渠道 API Key
(只经 WebUI 输入,不写进仓库、不进日志)。

## A. 官方模式

1. `csswitch-gateway tray`(或 `serve`)启动,打开控制台确认「官方 Claude」为当前连接。
2. 控制台点「启动 Science」→ 打开 Science → 用自己的账号登录。
3. 检查:项目与历史会话完整、无 session 过期横幅。
4. 发一轮普通对话,确认流式输出正常。
5. 发一轮需要工具的任务(例如让它跑一段 python),确认内核执行与结果正确。
6. Science 设置页 → About:**必须显示自动更新可用**(不是 "Automatic updates are off")。

## B. 第三方模式(Kimi / DeepSeek 各跑一遍)

1. 控制台「渠道配置」填 base_url 与 API Key,点「获取可用模型」应列出真实模型。
2. 四槽按需填写;留空槽应继承默认槽。保存。
3. 切换到该模式,确认 Science 自动重启并给出新登录链接。
4. 进入 Science,**新开一个干净会话**(同一对话不混供应商),确认模型菜单里
   显示的是你填的显示名。
5. 发一轮带工具的任务,确认执行与结果正确。
6. 回控制台看「请求日志」:不应有 error 行;日志里不得出现任何 key 或消息内容。

## C. 已知需要盯的兼容点

以下缺陷有过实测记录,回归时优先看它们:

| 现象 | 对应补偿规则 | 判据 |
| --- | --- | --- |
| Kimi:辅助调用 400 `tool_choice 'specified' is incompatible with thinking enabled` | `provider.kimi.specified-tool-choice-disables-thinking` | 日志里该规则命中,且无 upstream_failure |
| Kimi K3:回答里完全没有 thinking 块 | `provider.kimi.thinking-upstream-default` | K3 会话应能看到思考过程 |
| Kimi:带 PDF 附件后此轮及以后全部 400 | `provider.kimi.document-block-placeholder` | 该轮继续,且模型说明附件未送达 |
| DeepSeek:多轮 thinking 后 400(旧 BUG-083) | `provider.deepseek.tool-thinking-history-replay` | 连续多轮带工具的对话不报错。2026-08-19 已实测通过 |
| DeepSeek:`thinking auto` 被拒 | `provider.deepseek.thinking-auto-adaptive` | 首轮即可正常 |

## D. 不要做的事

- 不要给 Science 传 `--data-dir` / `--config`:会造出隔离实例,对话分家。
- 不要给 Science 传 `--no-auto-update`:官方实例必须保留自动更新。
- 不要把服务配置写成 `~/.csswitch/config.json`:旧桌面端占用该文件名,
  它会把不认识的 schema 当自己的旧版本迁移,结果清空自身 provider 列表。
