# Claude Science 真实入站请求抓取（2026-08-06）

## 方法

用临时录制转发进程冒充 provider 端点，在 CSSwitch 中配「自定义 Anthropic」profile 指向它：

```
Science ──▶ CSSwitch Gateway ──▶ 录制代理(127.0.0.1) ──▶ https://api.kimi.com/coding
```

选 `custom-anthropic` 通路的依据：`is_kimi_relay(Some("custom-anthropic"), _)` 为 false，
`filter_kimi_server_tools` 不生效，**`tools` 数组原样透传**，因此工具声明就是 Science 的原始声明。
该通路对请求体唯一的确定性改写是模型名替换与 `thinking: auto → adaptive`。

代理脚本位于 `/tmp`，不入仓库；key 由代理注入，profile 里填的是占位符。

## 抓到的会话

| seq | 上游状态 | tools | web_search | thinking | tool_choice |
| --- | --- | --- | --- | --- | --- |
| 8 | **400** | 1 | 否 | 字段缺失 | `{"type":"tool","name":"create_work_item"}` |
| 9 | **429** | 25 | 是 | `{"type":"adaptive"}` | 无 |
| 10 | **429** | 25 | 是 | `{"type":"adaptive"}` | 无 |
| 11 | **429** | 25 | 是 | `{"type":"adaptive"}` | 无 |
| 12 | **429** | 25 | 是 | `{"type":"adaptive"}` | 无 |
| 13 | **429** | 25 | 是 | `{"type":"adaptive"}` | 无 |

seq 9~13 是**同一轮对话**（用户输入"纯文本一轮，比如'简单介绍一下你自己'"）。
Science 侧界面显示 `Claude is at capacity — retrying in 33s (attempt 6)`，
即它把 429 当成容量问题做指数退避重试，每次重试携带同样的请求形状，因此必然连续失败。
**这一轮对话从未成功过，用户可见的表现是永久卡在重试。**

## R6 问题的答案

### Q1：Science 是否每轮都声明 `web_search_20250305`？

**是，每轮都声明，且不受用户开关控制。**

它是 Science 标准工具集里的**第 1 个工具，共 25 个**：

```
web_search_20250305(server)  ← 唯一的 server tool
bash, python, r, repl, save_artifacts, read_file, edit_file,
manage_environments, manage_packages, fetch_article_fulltext,
list_compute, compute_details, ask_about_compute, skill, ask_user,
generate_plan, search_skills, request_network_access, list_host_grants,
request_host_access, delete_host_files, update_step_status,
wait_for_notification, boundary                          ← 其余 24 个均为客户端工具
```

这条答案排除了"影响面小"的可能：由于 Kimi 对"声明 web_search 但本轮未搜索"返回 429，
而普通对话轮几乎都不搜索，**Science 的每一轮对话都会失败**。观测到的失败率为 100%。

### Q2：`thinking` 的实际取值

抓到的是 `{"type":"adaptive"}`。`custom-anthropic` 通路只把 `auto` 改写成 `adaptive`，
其余取值原样透传，映射单向确定，因此 **Science 原始发送的是 `{"type":"auto"}`**，与 PR #73 的描述一致。

另有一类请求（seq 8，Science 内部 helper 调用）**完全不带 `thinking` 字段**。

### Q3：历史中 server 块的形态

**未观测到。** 由于没有任何一轮成功，Science 从未收到过含 `server_tool_use` /
`web_search_tool_result` 的响应，也就无从回传。该项仍为未验证，需在链路打通后补测。

### Q4：其他非标准字段

| 字段 | 形态 | Kimi 是否接受 |
| --- | --- | --- |
| `system` | 数组形式的 text 块，带 `cache_control: {type:"ephemeral", ttl:"1h"}` | 200 |
| `tools[].cache_control` | `{type:"ephemeral", ttl:"1h", scope:"global"}`（`scope` 非标准） | 200 |
| `temperature` | 数值 | 200 |
| `metadata` | 对象 | 200 |
| 首条 system 块 | `x-anthropic-billing-header: cc_version=…; cc_entrypoint=operon_internal; operon_channel=public` | 200 |

**这些非标准字段都不构成兼容问题**，无需过滤。

## 新发现：Science 内部 helper 调用会 400

seq 8 是 Science 的工作项分类器调用（`create_work_item`，用于生成工作项标题与摘要）：

- 强制 `tool_choice: {"type":"tool","name":"create_work_item"}`
- **不带 `thinking` 字段**
- 上游返回 `400 invalid_request_error: "tool_choice 'specified' is incompatible with thinking enabled"`

因为 Kimi 的思考默认开启（`supports_thinking_type: "only"`），撞上"指定工具"型 tool_choice 即冲突。

补充实测（`k3`）：

| `tool_choice` | `thinking` | 结果 |
| --- | --- | --- |
| `{"type":"tool", name}` | 缺失（默认开） | **400** |
| `{"type":"tool", name}` | `enabled` | **400** |
| `{"type":"tool", name}` | `disabled` | **200** |
| `{"type":"any"}` | 缺失 | 200 |
| `{"type":"any"}` | `disabled` | 200 |
| `{"type":"auto"}` | 缺失 | 200 |

**修法：仅当 `tool_choice.type == "tool"` 时置 `thinking: {"type":"disabled"}`。**

这比现有两套实现都窄，两者直接套用都不对：

- DeepSeek 的 `normalize_thinking` 对 `any` 与 `tool` 都关思考 —— 对 Kimi 而言 `any` 无需关，属多余损失。
- Kimi relay 的 `normalize_relay_thinking` 是删掉 `tool_choice` —— 会破坏 helper "必须调用该工具"的契约。

## 方案 C 前提验证

方案 C 需要"二次请求必定触发搜索"。用极简形式 `Search the web for: <query>` 加 server tool 实测五个不同查询：

| 查询 | 结果 |
| --- | --- |
| latest stable Rust version | 200，已搜索 |
| Python 3.14 release date | 200，已搜索 |
| what is the capital of Mongolia | 200，已搜索 |
| Tauri 2 changelog | 200，已搜索 |
| current price of Bitcoin | 200，已搜索 |

**5/5 触发搜索**，前提成立。样本量有限，实现时应保留"二次请求仍 429"的失败路径而非假定不会发生。

### 端到端两阶段模拟

用类 Science 的工具集（1 个 web_search + 6 个客户端工具 + system prompt）完整模拟方案 C：

- 调用①：`web_search` 以**客户端工具**形态声明
- 调用②：仅当①返回 `tool_use: web_search` 时发起，改用真 server 工具并追加
  `Use web_search to look up: <query>`

| 场景 | 调用① | 触发 web_search | 调用② |
| --- | --- | --- | --- |
| 写代码（非搜索） | 200 `[thinking, text]` | 否 | 不需要 |
| 解释概念（非搜索） | 200 `[thinking, text]` | 否 | 不需要 |
| 查 Rust 版本 | 200 `[thinking, tool_use]` | 是 | 200，含 `server_tool_use` + `web_search_tool_result` |
| 查本月 AI 发布 | 200 `[thinking, tool_use×2]` | 是 | 200，含真实搜索块 |

**非搜索轮全程 200，零 429**。这是方案 C 相对方案 B 的实质差别：
B 每轮先撞一次 429 再退避，C 根本不产生 429。

模拟暴露的两个实现约束：

1. 调用① 可能返回**多个** `web_search` 的 `tool_use`（模型想搜多次），拼接逻辑不能假定只有一个。
2. 调用② 的响应里可能**尾随正常的客户端 `tool_use`**（模型搜完想继续用别的工具），
   这些必须原样透传给 Science，不能被当作内部块吞掉。

## 对选型的影响

Q1 的答案（每轮都声明）把方案对比彻底改变了：

| 方案 | 代价落在哪 | 在"每轮都声明"前提下的实际影响 |
| --- | --- | --- |
| A 始终丢弃 | 无 | 订阅自带搜索永久不可用 |
| B 429 后去工具重发 | 非搜索轮 +约 1s | 几乎**每一轮**都要多付约 1s，是永久性延迟税 |
| C 降级为客户端工具 + 二次调用 | 搜索轮多一次往返 | 非搜索轮零开销；仅少数搜索轮多付一次往返 |

方案 B 原本"只影响部分轮次"的假设不成立。
