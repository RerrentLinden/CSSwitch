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
| `provider.deepseek.tool-choice-disables-thinking` | 思考开启时拒收任何非 `none` 的 tool_choice | 保留 tool_choice(Science 的抽取请求依赖结构化输出),放弃这一轮思考,并剥掉 effort |
| `provider.deepseek.thinking-disabled-strips-effort` | `thinking: disabled` 与 effort 参数互斥,同时出现 400 | 尊重客户端的 disabled,剥掉冲突的 effort |
| `provider.deepseek.tool-thinking-history-replay` | 要求带 `tool_use` 的历史助手轮回传 thinking,客户端常剥掉 → `must be passed back` 400 | 无状态补齐:缺失插占位块,`redacted_thinking` 转普通 thinking,去掉过不了校验的 signature |
| `provider.deepseek.malformed-server-tool-block-repair` | 严格反序列化拒收缺 `tool_use_id` 的 `web_search_tool_result`(Science daemon 会把这种块落盘进历史) | 能抽文本就降级成文本,否则丢弃。**结构完好的 server tool 块保留**——DeepSeek 原生执行 web_search,降级会教模型模仿扁平化的伪工具调用 |
| `provider.deepseek.orphan-tool-pairing-repair` | 按 id 一对一配对 tool_use/tool_result,落单即拒 | 未应答的 tool_use 按**计数差额**补合成 error 结果(并行调用可能共用 id,集合去重会漏补);找不到对应 tool_use 的 tool_result 降级成文本 |

另有 max_tokens 上限钳制:pro 65536、flash 32768,超限上游直接拒。

## 相对旧实现退掉了什么

旧路径走 OpenAI 格式转换 + 落盘的 thinking 续写存储(`reasoning_state.rs`)+
DSML shim。现在全部退役:上游要的只是"历史里有 thinking 块",占位块即可满足,
不需要 CSSwitch 侧保存任何对话状态。

## 未验证

本渠道的补偿尚未用真实 DeepSeek key 做过端到端验收。历史问题
BUG-083(DeepSeek Pro 多轮 thinking 400)在新路线下是否消失,需要按
[真机验收清单](../../test/LIVE_ACCEPTANCE.md) 的 B、C 两节实测确认。
