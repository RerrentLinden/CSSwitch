# DeepSeek 渠道

端点:`https://api.deepseek.com/anthropic`(官方 Anthropic 兼容端点)。
走与 Kimi 相同的契约驱动中继路径,差异全在 `RelayFlavor::DeepSeek` 上。

## 契约

| 项 | 值 |
| --- | --- |
| contract id | `deepseek-native` |
| adapter / transport | `deepseek` / `anthropic_messages` |
| 鉴权 | `x-api-key` |
| 端点拼接 | `anthropic_v1`(base + `/v1/messages`) |
| thinking policy | `deepseek_native` |

默认模型预设:`deepseek-v4-pro`(默认 / 高质量 / Fable)、`deepseek-v4-flash`(快速)。
四个槽都可在 WebUI 里改。

## 六条补偿

实现在 `desktop/gateway/src/deepseek_compat.rs`,每条带规则 ID 与单测。
语义来自 [cc-switch](https://github.com/farion1231/cc-switch)(MIT,© 2025 Jason Young)
及 biociao 的 fork 中的 DeepSeek normalizer 链,按本仓库的规则体系重写。

| 规则 | 上游行为 | 处理 |
| --- | --- | --- |
| `provider.deepseek.thinking-auto-adaptive` | 请求体反序列化只认 `adaptive`/`enabled`/`disabled`,Claude Science 发的非标准 `auto` 直接 400 | 改写为 `adaptive`(语义最近) |
| `provider.deepseek.specified-tool-choice-disables-thinking` | 思考开启时拒收**指定工具型** tool_choice(`{"type":"tool","name":…}`) | 保留 tool_choice(Science 的抽取请求依赖结构化输出),放弃这一轮思考,并剥掉 effort |
| `provider.deepseek.thinking-disabled-strips-effort` | `thinking: disabled` 与 effort 参数互斥,同时出现 400 | 尊重客户端的 disabled,剥掉冲突的 effort |
| `provider.deepseek.tool-thinking-history-replay` | 要求带 `tool_use` 的历史助手轮回传 thinking,客户端常剥掉 → `must be passed back` 400 | 无状态补齐:缺失插占位块,`redacted_thinking` 转普通 thinking,去掉过不了校验的 signature |
| `provider.deepseek.malformed-server-tool-block-repair` | 严格反序列化拒收缺 `tool_use_id` 的 `web_search_tool_result`(Science daemon 会把这种块落盘进历史) | 能抽文本就降级成文本,否则丢弃。**结构完好的 server tool 块保留**——DeepSeek 原生执行 web_search,降级会教模型模仿扁平化的伪工具调用 |
| `provider.deepseek.orphan-tool-pairing-repair` | 按 id 一对一配对 tool_use/tool_result,落单即拒 | 未应答的 tool_use 按**计数差额**补合成 error 结果(并行调用可能共用 id,集合去重会漏补);找不到对应 tool_use 的 tool_result 降级成文本 |

另有 max_tokens 上限钳制:pro 65536、flash 32768,超限上游直接拒。

## 相对旧实现退掉了什么

旧路径走 OpenAI 格式转换 + 落盘的 thinking 续写存储(`reasoning_state.rs`)+
DSML shim。现在全部退役:上游要的只是"历史里有 thinking 块",占位块即可满足,
不需要 CSSwitch 侧保存任何对话状态。

## 实测记录(2026-08-19)

用真实 key 在官方 Science 实例里跑通:干净会话 + 三轮独立的 Python 内核调用
(生成随机序列 → 计算 GC → 找最高值),结果正确,**零 upstream_failure**。

规则命中情况(网关日志):

| 轮次 | 命中的规则 |
| --- | --- |
| 辅助调用(flash,带指定工具型 tool_choice) | `provider.deepseek.specified-tool-choice-disables-thinking` |
| 首轮(msgs=3) | `tool.deepseek.web_search.server-tool-preserve` |
| 后续多轮(msgs=5/7/9) | `provider.deepseek.tool-thinking-history-replay` + 上一条 |

**BUG-083 关闭**:该缺陷是"DeepSeek Pro 多轮 thinking 返回 400",根因就是历史里
缺 thinking 块。本轮多轮工具调用正是这个形状,补偿每轮命中,全程无 400。
旧的落盘续写机制不需要恢复。

模型清单不在 `/anthropic` 路径下(`/anthropic/v1/models` 返回 404 空体),
在根域 `/v1/models`;控制台的模型探测会依次尝试这两个地址。

## 长对话复测(2026-08-19)

同一会话追加到 **31 条消息**(多轮 Python 内核 + PubMed MCP connector 调用),
`provider.deepseek.tool-thinking-history-replay` 在 13 次请求里命中 9 次
(其余 4 次是首轮或无 tool_use 的辅助调用,本就不需要补),**零 upstream_failure**。
历史里的 thinking 块没有随对话变长而丢失。

## thinking / effort 的处理边界

不是"丢弃 thinking 配置"。经网关实测的行为矩阵:

| 入站请求 | 出站到上游 | 命中规则 |
| --- | --- | --- |
| `enabled` + `budget_tokens` + `reasoning_effort` | **原样透传,零改动** | 无 |
| `auto` | 改写为 `adaptive` | `thinking-auto-adaptive` |
| `disabled` + effort | 保持 `disabled`,剥掉 effort | `thinking-disabled-strips-effort` |
| `tool_choice` 为 `{"type":"tool",…}` | 置 `disabled` 并剥 effort | `specified-tool-choice-disables-thinking` |
| `tool_choice` 为 auto / any / none | **不动**,thinking 照常 | 无 |

只有上游会拒收的组合才被改写;能过的原样送。

两条规则**同时命中**时(`auto` + 指定工具),tool_choice 优先:thinking 直接压成
`disabled`,thinking 原本的取值失去意义,`auto→adaptive` 不再执行也不记入规则日志——
否则日志会显示一条净效果为零的改写,误导排查。DeepSeek 在 `adaptive` 下同样拒收
指定工具,所以这里必须是 `disabled` 而不是 `adaptive`。

tool_choice 的边界是实测出来的(2026-08-19,thinking enabled 与 adaptive 各测一轮):

| tool_choice | 上游响应 |
| --- | --- |
| 不带 / `auto` / `any` | 200,`['thinking','tool_use']` —— 思考与工具调用并存 |
| `{"type":"tool","name":…}` | **400** Thinking mode does not support this tool_choice |
| `none` | 200,`['thinking','text']` |

所以补偿只针对指定工具型这一种。早期实现照搬了"任何非 none 都禁思考"的判据,
那会在最普通的 auto 形态上白白关掉推理——已收窄。

这不是第三方独有的怪癖。Anthropic 官方文档同样限制:
["Limit tool choice to `auto` or `none` in manual mode" —— 强制工具的 tool_choice
在 manual extended thinking(`type:"enabled"`)下会报错,adaptive thinking 则支持强制工具](https://platform.claude.com/docs/en/build-with-claude/thinking-tool-workflows)。
官方的限制其实**更严**(manual 模式下连 `any` 也不允许),DeepSeek 与 Kimi 都只卡 `tool` 一种。
注意 DeepSeek 在 `adaptive` 下也拒收 `tool`,与官方 adaptive 的行为不同,这是它自己的收紧。

## 仍未验证

- `document` 块行为(历史记录称 DeepSeek 接受该块但答 `CANNOT_READ`,本轮未复测)。
- 长会话下的孤儿工具配对与畸形 server tool 块修复:本轮未构造出这两种历史形态。
