# 技术设计：Kimi 原生 web_search 适配（含搜索轮卡死修复）

前置证据：[现场证据](research/2026-08-19-live-evidence.md) ·
[原生通道探测](research/2026-08-19-native-probe.md) ·
上次退役实现 `d49871a`（回滚于 `21337be`）。

## 方向

按用户决策（2026-08-19）：**首选适配原生 web_search**，桥接方案退役；
同时把「gateway 拒绝一轮 → 客户端无限重试」的失败路径改成终态可见。

上次退役失败的三个拦路虎，本轮探测逐一有了答案：

| 上次的现象 | 本轮定性 | 对策 |
|---|---|---|
| `Search results for query:` 每轮出现 | 确定性上游噪声块，前缀固定，内容与搜索 input 重复 | 响应侧整块剥离（新规则） |
| 冒号后为空 / 答案为空 | K2.7 的悬挂噪声头（宣布下一轮搜索却 end_turn） | 同上剥离；剥后如实透传，不再 502 |
| 搜索后下一轮 400 | 配对 id 恒不匹配（`tool_…` vs `srvtoolu_…`） | 恢复 d49871a 配对修复规则 |

## 变更清单

### 1. 恢复 d49871a（`git revert 21337be`）

拿回：web_search 原样透传（`tool.kimi.web_search.server-tool-preserve`）、
历史配对修复（`provider.kimi.web-search-result-pairing-repair`）、
删除 920 行桥接与响应剥离过滤器（含 `kimi_terminal_is_safe` 严格判定——
它正是本次 502 死循环的来源）、删除 `CSSWITCH_KIMI_WEB_SEARCH` 实验开关
（native 成为唯一路径，策略回归 provider contract，不留环境变量岔路）。

放弃的能力与理由：
- 桥接本体：动机（429）在 32+2 次实测中未复现；若将来复发，表现为可见的 429
  重试而非静默降级，从 git 历史恢复桥接即可。
- 旧过滤器附带的 thinking 签名校验：这些模型 `reasoning_round_trip=none`，
  Science 不回传 thinking，校验对象不存在；探测 2 已验证剥 thinking 的历史照常 200。

### 2. 新规则：噪声头剥离 `provider.kimi.search-noise-text-strip`

判定：text 块，全文以 `Search results for query:` 开头（探测 1b/2/4 三发三中，
含悬挂形态；内容与 `server_tool_use.input.query` 重复，剥离零损失）。

- 非流式：直接从 content 数组删除该块。
- 流式：新建一个窄的 Kimi 流过滤器（替代旧 920 行过滤器的位置，只做这一件事）：
  - text 块自 `content_block_start` 起缓冲，累计文本仍是前缀的继续缓冲
    （上限 4KB，marker 仅 25 字节，正常几个 delta 内必然分辨）；
  - 命中 → 整块吞掉（start/delta/stop 全不外发），后续块 index 重映射补洞
    （Science 要求 index 连续无空洞，沿用旧过滤器验证过的补洞法）；
  - 未命中 → 原序 flush，此后该块直通。
- 命中时打日志（rule id + 吞掉的字节数），nonstream 走 metadata rules 记录。

### 3. 止血：失败必须终态、可见、可诊断

- `server.rs` 流转发环里过滤器错误当前被 `Err(_)` 丢弃、统一伪装成
  `upstream SSE protocol error`（现场 23 次全是这样来的）：改为把真实错误信息
  写进 SSE error 事件并 `eprintln!` 带上下文（哪条规则、什么形态），不再丢失。
- 随桥接退役，`kimi_terminal_is_safe` 严格判定整体消失：
  「搜索对 + thinking、无正文、end_turn」（探测 4 剥噪后的形态）如实透传，
  Science 显示一个空回答轮，用户可以直接追问——不再 502、不再无限重试。
  是否进一步把该形态改写为 `pause_turn` 让 Science 自动续轮，
  留待浏览器端到端验收观察后单独决策（默认不做，改写 stop_reason 是超出
  「窄补偿」的行为变更）。
- 常规瞬态失败（网络断流等）保持 `api_error` 可重试语义不变。

### 4. 合同超时：Kimi relay nonstream total_ms 30000 → 180000

搜索轮实测 29.5–40s，30s 上限让 Science 的 nonstream 回落必然失败
（现场 96 次 502 里有一部分正是回落请求）。catalog 为 `include_str!` 内嵌，
digest 运行时现算，改 JSON + 重编译自洽；只动 `kimi-anthropic-relay` 一个合同。
connect/read_idle 不动。

## 数据流（改后）

```
Science 声明 web_search_20250305
   │ 请求侧
   ├─ 工具声明原样透传（server-tool-preserve）
   ├─ 历史配对修复（orphan 补块 / id 对齐）
   ▼
Kimi 上游（真实搜索，自带噪声头）
   │ 响应侧
   ├─ 噪声头 text 块整块剥离 + index 补洞
   ├─ 其余块（搜索对 / thinking / text）原样转发
   ▼
Science 原生渲染 web_search；历史落盘 → 下轮回传 → 配对修复兜住
```

## 验收路径

离线门禁之外，真机验收全部通过内置浏览器驱动真实 Science（localhost:8000）：

1. 新开对话（Kimi 模式）问一个必须联网的问题 → 原生搜索组件渲染、
   回答引用真实结果、**开头不出现 search_query 文本**；
2. 同一对话连续追问两轮 → 无 400、无卡死；
3. K2.7 槽位重复 1（悬挂噪声头模型）→ 最多空回答，不挂起；
4. 回看本次卡死的对话（frame 7c0bcf10）→ 重试自然恢复或可正常终止。

## 回滚

单 revert 本次合并即可回到桥接方案；桥接完整实现保留在 git 历史（本文档与
kimi-channels.md 记录恢复路径）。配置无 schema 变更，无迁移。
