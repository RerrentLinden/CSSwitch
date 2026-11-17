# 现场证据：Kimi 搜索轮 502 重试死循环

采集时间：2026-08-19 18:43–18:58（Asia/Shanghai），采集时故障仍在持续。
渠道：Kimi（`k3`，`api.kimi.com/coding` 链路），gateway `csswitch-gateway serve --port 8788`（PID 42027）。
对话隐私：只记录结构、计数与时间，不摘录用户对话正文。

## 1. 卡死的表征

- Science frame `7c0bcf10-…`：`status=processing`，`created 18:43:36`，`updated_at` 停在 `18:53:43` 之后不再前进。
- 该 org 库中当时只有这一个 frame 处于 `processing`。
- gateway 侧只在收到请求时打日志，`~/.csswitch/service.log` 中该 frame 的四次请求为
  `msgs=3 / 5 / 7 / 9`，全部 `server_tool_types=- dropped_server_tools=0`，
  即 **Science 这一轮根本没有声明 `web_search_20250305`**，桥接规则未参与。

## 2. 真正的失败原因（来自 Science 日志 `logs/server-20260819.log`）

gateway 对该 frame 的每一次推理都返回了错误，Science 端 `result=error out=0`：

- `502 {"type":"error","error":{"type":"api_error","message":"Kimi server-tool response has no safe terminal answer"}}`
  —— 当天出现 **96 次**，首次出现在日志第 119 行，不是本次会话独有。
- `{"type":"error","error":{"type":"api_error","message":"upstream SSE protocol error"}}`
  —— 当天出现 **23 次**，流式通路上的同源失败。

两条错误串都由 CSSwitch 自己产生：

- `desktop/gateway/src/anthropic_compat.rs:301`（流式 `message_stop` 分支）
- `desktop/gateway/src/anthropic_compat.rs:611`（非流式响应过滤）
- `desktop/gateway/src/server.rs:683 / 697 / 791`（SSE 协议错误事件）

判定函数 `kimi_terminal_is_safe`（`anthropic_compat.rs:522`）：
一旦响应里丢弃过 server-tool 块（`dropped_server_tools > 0`），
就只接受 `stop_reason == end_turn` **且** 有正文 **且** 没有客户端 `tool_use`。
本轮模型的实际形态是「原生搜索块 + 思考 + 正文 + `tool_use(python)`」，
落在该规则的拒绝面上，于是整轮被判为不安全并回 502。

## 3. 为什么用户看到的是「卡死」而不是报错

Science 对 502 做指数退避重试，同一轮观察到 attempt 1→7，等待 `1.3s → 2.8s → 4.8s → 8.8s → 18.5s → 42.2s`，
重试不会停，frame 一直停在 `processing`。gateway 端因为只在入口打日志，退避期间日志不再增长，
从服务日志上看像是「没有请求了」。用户侧既没有错误提示，也没有终止路径。

## 4. 为什么会「连续搜两次 + 只打印一行 python」

- Science 自己会把历史里的 server-tool 结果做 stub 压缩：
  `[OPERON] stubbed server-tool results in N-msg history: ~13.5KB–100.4KB freed`（当天多次）。
  压缩后历史里只剩 `[System] Prior-turn web_search(...) — results persisted.` 的摘要行。
- 模型看不到自己上一轮的搜索结果，于是每轮都重新宣布「再搜一次」。
- 该 frame 没有可调用的 `web_search` 客户端工具，模型退而用 `python` 打印一行占位文本
  （`print("final search for latest Oct-Nov 2026 outcomes")`），形成「宣布搜索 → 空转」的观感。

frame 内消息形态（只记类型）：

```
 8 assistant  thinking + text + tool_use(python)
 9 user       tool_result + text([System] Prior-turn web_search — results persisted)
10 assistant  server_tool_use(web_search) + web_search_tool_result(25 条) + thinking + text + tool_use(python)
11 user       tool_result(python)
-- 之后 gateway 持续 502，无第 12 条 --
```

## 5. 现场还能确认的边界

- 上游 Kimi 在 **未声明 web_search** 的请求里也会自发返回原生搜索块，这是本次触发的前提。
- `catalog/provider-contracts.v1.json` 中 Kimi 的超时为 `connect 10s / total 30s / read_idle 300s`；
  本故障不是超时问题，是明确的 502 拒绝 + 客户端无限重试。
