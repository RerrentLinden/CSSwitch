# 原生 web_search 通道探测（隔离实例，native 模式）

2026-08-19 19:00–19:20。方法：第二个 gateway 实例（`CSSWITCH_KIMI_WEB_SEARCH=native`，
端口 8799，进程自己读 `~/.csswitch` 配置，key 不经过任何人手），loopback 直发
`/v1/messages`，原始 SSE 存 scratchpad `probe/`。共 7 次上游请求。

## 探测矩阵与结果

| # | 形态 | 结果 |
|---|---|---|
| 1 | k3，声明 web_search，nonstream | **30s 超时失败**（合同 total_ms=30000；搜索轮就是要 30s+） |
| 1b | 同上，stream | 200，29.5s。块序 `噪声头text → server_tool_use → result(10条真实) → thinking → text(722, 正确答案)`，`end_turn` |
| 2 | k3，二轮：历史带修复配对的搜索块 + 追问，声明 | 200，40s。再次搜索，答案 2069 字连贯正确 |
| 3 | k3，同历史，**不声明任何工具** | 200，7s。模型幻觉调用 `tool_use(web_search)`（客户端形态，未声明也照发），`stop=tool_use` |
| 4 | kimi-for-coding (K2.7)，声明，stream | 200，30s。**块序 `噪声头 → 搜索对 → thinking → 噪声头` 直接 `end_turn`，没有答案文本** |
| 5/6 | 两个模型，声明但无需搜索（"1+1"） | 200 + 200，正常回答。**429 未复现**（与上次 32/32 一致） |

## 定性结论

1. **原生通道功能是通的**：模型能拿到真实搜索结果并据此答对（探测 1b、2）。
   上次退役失败记录里"模型看不到真实结果"不是通道级死刑，是下面两个形态叠加的观感。
2. **噪声头是确定性上游产物**：独立 text 块，前缀恒为 `Search results for query: `，
   内容与紧随其后的 `server_tool_use.input.query` 完全重复（探测 1b/2/4 三发三中）。
   K2.7 还会在 turn 末尾发**悬挂噪声头**（宣布下一次搜索却直接 end_turn，探测 4）——
   这就是上次"冒号后面是空的/答案为空"的来源。剥掉它不损失任何信息。
3. **配对 id 恒不匹配**：`server_tool_use.id`（`tool_…`）与 `web_search_tool_result.tool_use_id`
   （`srvtoolu_…`）不是同一个值，历史回传必须修复配对（d49871a 的规则，探测 2 验证修完 200）。
4. **历史里剥掉噪声头 + 剥掉 thinking 不影响下一轮**（探测 2 的历史就是这么构造的）。
5. **未声明工具时模型仍会发搜索类调用**：本次是幻觉客户端 `tool_use(web_search)`（探测 3）；
   当天真实会话则是上游自发 `server_tool_use`（见 live-evidence）。两种形态都存在。
6. **nonstream 合同 total_ms=30000 对搜索轮不够**：搜索轮实测 29.5–40s。
   Science 流式失败后会回落 nonstream，30s 上限让回落必然失败。

## 对设计的直接输入

- 恢复 d49871a（native 透传 + `provider.kimi.web-search-result-pairing-repair`），
  在其上新增噪声头剥离规则（流式按块缓冲判定前缀，命中整块吞掉）。
- K2.7 的"无答案 turn"（探测 4）剥噪后表现为「搜索对 + thinking、无文本、end_turn」：
  基线选择是如实透传（不再 502、不再无限重试），是否改写 stop_reason 为 pause_turn
  让 Science 自动续轮，留给浏览器端到端验收后单独决策。
- Kimi relay 合同 nonstream total_ms 需上调（实测证据 40s，建议 180000）。

## 追加（当日晚间，真机验收期间的新发现）

7. **幻影空搜索对**：被告知"无需搜索"的轮次，上游仍常发一对空壳——
   `server_tool_use` **无 id、无 input**，紧跟 `content: []` 的
   `web_search_tool_result`（同样无 tool_use_id）。探测 7 与真实会话第 2/4/5 轮
   均复现（第 5 轮工具循环的 3 次调用**每次**都带）。
   - 危害一：Science 渲染为空 Server Tool 框（用户当场反馈的观感问题）。
   - 危害二：落盘后成为无 id 孤儿结果块——正是 d49871a 当时"不发明配对键"
     而放过的形态——此后每轮 400 `tool_call_id is not found`。
     真实会话第 3 轮当场撞上：Science 显示 Agent Failed + 中断（终态，止血生效），
     gateway 日志两次 `upstream_failure status=400` 且配对修复规则未触发。
   - 修复：响应侧 `provider.kimi.empty-search-pair-strip` 整对剥离；
     请求侧配对修复新增无 id 分支（空内容删、有内容合成键）。
     同一会话 Resume 后实测 400 → 200，后续 6 轮全通。
8. **空 Server Tool 框的第二来源（平台行为，非缺陷）**：有搜索历史的下一轮，
   Science 把上轮结果存盘并在 user 消息附
   `[System] Prior-turn server_tool({}) — results persisted.`，UI 渲染为空输出的
   Server Tool 折叠框。与本轮搜没搜无关（第 2/4 轮有、第 3 轮无，与"承接搜索轮"
   完全对应），官方渠道同样会出现。判据：落盘的 assistant 消息干净即链路无责。
9. **噪声头/幻影对是常态而非偶发**：真机 6 轮里每个搜索轮 `noise=1`、
   多数轮 `pair=2`（字节 325–739），剥离规则每轮都在工作。
