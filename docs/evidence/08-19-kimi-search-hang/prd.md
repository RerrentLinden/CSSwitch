# Kimi 渠道搜索轮空转与请求卡死

## 现象（用户实测）

真实 Science 实例，Kimi `k3` 渠道，一次带联网搜索的长对话：

1. 模型连续两次宣布"再做一次搜索"，但两次都只执行了一段 `python` 占位打印，没有真正搜索的观感；
2. 第二次之后整轮停住，界面既不出结果也不报错，用户判断为卡死。

现场 frame：`7c0bcf10-…`，采集时 `status=processing` 且不再前进。

## 已确认的根因

完整现场记录见 [research/2026-08-19-live-evidence.md](research/2026-08-19-live-evidence.md)。

**卡死不是超时，是 gateway 主动回 502 后被客户端无限重试。**

- gateway 对该轮返回
  `502 {"type":"api_error","message":"Kimi server-tool response has no safe terminal answer"}`，
  Science 以指数退避重试（观察到 attempt 1→7，等待递增到 42s），frame 永远停在 `processing`。
- 该错误串来自 `desktop/gateway/src/anthropic_compat.rs:301`（流式）与 `:611`（非流式），
  判定函数是 `kimi_terminal_is_safe`（`anthropic_compat.rs:522`）。
- 流式改写器无条件丢弃上游的 `server_tool_use` / `web_search_tool_result` 块
  （`anthropic_compat.rs:230-236`）。一旦丢过块，`kimi_terminal_is_safe` 只接受
  `stop_reason == end_turn` 且有正文且没有客户端 `tool_use` 的形态。
- 本轮模型的真实形态是「原生搜索块 + 思考 + 正文 + `tool_use(python)`」，
  正好落在拒绝面上，于是整轮被判不安全。
- 触发前提：**Science 这一轮并未声明 `web_search_20250305`**
  （日志 `server_tool_types=- dropped_server_tools=0`，桥接规则未参与），
  但上游 Kimi 仍自发返回了原生搜索块。
- 该 502 当天在 Science 日志中出现 96 次，另有 23 次 `upstream SSE protocol error`，
  不是一次性偶发。

"连续搜两次却只打印一行 python"是另一层原因叠加：Science 自己会把历史里的 server-tool
结果 stub 掉（`stubbed server-tool results in N-msg history`），模型看不到上一轮搜索结果就重新宣布搜索；
而该 frame 没有可调用的 `web_search` 客户端工具，模型退而用 `python` 打印占位文本。

## 待确认

- 该 frame 第 10 条 assistant 消息里确实存活着 `server_tool_use + web_search_tool_result`，
  与"流式路径无条件丢弃"不一致。需要用可复现的请求确认块存活的实际路径，
  再决定丢弃条件是否本身就写错了。

## Requirements

- gateway 判定一轮"不安全"时，失败必须是终态且可诊断：客户端不能陷入无限退避重试，
  用户必须能在界面上看到失败原因，而不是无声挂起。
- 不得为绕开该 502 而静默放行内容被裁剪过的响应；若要放宽判定，必须说清放宽的是哪一类形态、
  为什么该形态不产生"引用了客户端看不到的证据"的风险。
- 修复范围限定在 Kimi 渠道的 server-tool 响应处理与失败暴露路径，不顺带改 DeepSeek 或官方通路。
- gateway 侧要能在日志里看出这一轮为什么被拒，而不只有入口那一行请求日志。

## 已决策

用户 2026-08-19 两次拍板：
1. **止血 + 做通**：失败要变成可见终态（不再无限重试），搜索轮要真正走通。
2. **首选适配原生 web_search 服务**（上次任务未走通的方向），且不得回归
   「每个对话开头出现 search_query 内容」；把 web_search 降级为客户端工具的
   桥接方案降为次选。真机验证多用内置浏览器驱动真实 Science。

随后的隔离探测（见 [research/2026-08-19-native-probe.md](research/2026-08-19-native-probe.md)）
确认原生通道功能可用、噪声头可窄规则剥离、配对可修复，据此技术方案定为
原生适配主线，见 [design.md](design.md)。

## Acceptance Criteria

- [x] 原生搜索轮在真实 Science 渲染为原生 Web Search 组件，回答引用真实结果，
      **助手内容开头与中途均不出现 `Search results for query:` / search_query 文本**
      （真机 K3 六轮 + K2.7 一轮，逐轮截图与日志核对）
- [x] 同一对话连续多轮搜索后继续追问：无 400、无卡死（配对修复逐轮命中；
      含把修复前中毒的历史 Resume 救活：400 → 200）
- [x] 原卡死路径不再复现：搜索历史 + python 工具循环 3 次调用零失败；
      修复前的确属失败轮在 Science 界面显示 Agent Failed（终态可见，不再无限重试）
- [x] 噪声头剥离、幻影对剥离与配对修复各有基于真实 SSE 形态的单测（117 通过）；
      剥离规则对未命中流量零改写（字节级断言）
- [x] `bash test/run_all.sh` 三层门禁通过（static / unit / loopback 全 PASS，无 env-blocked；
      「五层」为旧仓库残留说法，已在 known-issues 修正）
- [x] 真机验收通过内置浏览器完成（design 四条 + 用户加测的工具轮、混合轮；
      第 4 条改判：卡死对话在旧版重试中自愈，同形态在新版下由工具循环轮验证不再触发）

### 验收补记

- 新发现并当场修复：幻影空搜索对（响应剥离 + 历史修复，见 research 追加节）。
- 判为平台行为：空 Server Tool 框的第二来源是 Science 存盘占位，非链路缺陷。
- 未验证与剩余风险：开放平台端点、`k3-256k`/`highspeed` 槽位按同契约沿用未单独实测；
  图片/视频输入、原生流式结构化输出未验证；上游偶发 429 依旧存在。
