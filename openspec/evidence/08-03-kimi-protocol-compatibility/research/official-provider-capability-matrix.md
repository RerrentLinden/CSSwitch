# Research: Kimi Code 订阅 API 与 DeepSeek 官方 API 能力矩阵

- Query: 仅依据当前官方一手资料，对比 Kimi Code 会员订阅 API 与 DeepSeek 官方 API 的协议、多模态、client/provider tool 所有权、Web Search/Fetch、代码执行、MCP/connectors、tool ID 与多轮历史合同，并给出隔离 Science 8990 的无凭据真机验收观测项。
- Scope: mixed（官方外部合同 + CSSwitch 内部适配边界）
- Date: 2026-08-03

## Findings

### 结论摘要

1. Kimi Code 订阅 API 官方公开的是 OpenAI-compatible Chat Completions 与 Anthropic-compatible Messages；官方 Codex 指南反而明确指出 Codex Responses 需要本地层转换到 Chat Completions，不能把 Kimi Code 当成原生 Responses provider。
2. Kimi Code CLI 另有宿主编排的 `WebSearch`、`FetchURL`、`Bash` 与 MCP。CLI 的 `/coding/v1/search` 是独立 host-service 端点，不是 `/coding/v1/messages` 的 Claude server tool，也不能证明当前 Coding Plan K3 已适配 `web_search_20250305`。Science/CSSwitch 单变量 A/B 为保留则 429、删除则 200；但同一 Coding Plan 经 Claude Code + upstream cc-switch 原样透传可成功，因此当前 drop 只是请求形状对齐前的 workaround。
3. DeepSeek 当前明确支持 OpenAI Chat Completions、Anthropic Messages，并新增 OpenAI Responses（截至本次核对仅 `deepseek-v4-flash`）；Responses 与 Claude Code/Anthropic 路径明确具有 provider-side Web Search。
4. DeepSeek 明确没有 provider-side code execution 或 MCP：Anthropic `code_execution_tool_result`、`mcp_tool_use/result` 不支持且 `mcp_servers` 被忽略；Responses 的 `code_interpreter` 与 `mcp` 被忽略。普通 function/client tool 仍可由调用方执行。
5. Kimi Code 与 DeepSeek 都要求工具调用历史保持配对；Kimi Code thinking 模式还要求 assistant tool-call message 保留 `reasoning_content`。不过 Kimi Code 订阅 API 没有公开完整字段级规范，Anthropic 路径的精确 ID、邻接和 server-tool 历史规则不能从“兼容 Anthropic”四字外推。

### 判定口径

- `支持`：官方页面明确声明，或给出可执行字段/示例。
- `不支持/忽略`：官方兼容表明确写明，或官方指南明确要求协议转换。
- `宿主侧`：模型只产生 tool call，CLI/Agent/应用实际执行工具并回传结果。
- `Provider-side`：提供方服务器直接执行工具；不能与同名 client function 混为一谈。
- `未知`：本次官方资料没有形成该端点的公开合同；`未写明` 不自动等于支持，也不自动等于永久不支持。

Anthropic 官方基线：`web_search_20250305` 是 GA server tool 的完整版本化 schema，日期后缀不可删除；标准 Messages 示例不要求 `?beta=true` 或专用 `anthropic-beta` header。该事实只定义 Anthropic 请求合同，不自动证明 Kimi Coding Plan 接受同一形状。

### 主能力矩阵

| 能力 | Kimi Code 会员订阅 API | DeepSeek 官方 API | 所有权与适配含义 |
|---|---|---|---|
| OpenAI 协议 | `支持` OpenAI-compatible Chat Completions；Base URL `https://api.kimi.com/coding/v1`，常用端点 `/chat/completions`。原生 Responses 未公开；官方 Codex 指南要求本地把 Responses 转成 Chat Completions。 | `支持` OpenAI Chat Completions；另支持 Responses API，截至 2026-08-03 Responses 仅支持 `deepseek-v4-flash`。 | CSSwitch 不应把 Kimi Chat compatibility 宣称为 Responses compatibility；DeepSeek 的 Chat 与 Responses 要按不同字段合同处理。 |
| Anthropic 协议 | `支持` Anthropic-compatible Messages；Base URL `https://api.kimi.com/coding/`，常用端点 `/v1/messages`。 | `支持`；Base URL `https://api.deepseek.com/anthropic`，官方给出字段级兼容表。 | “Anthropic compatible”只证明已列字段，不代表全套 Anthropic server tools、MCP 或 connectors。 |
| 文本输入 | `支持`，全部当前 Kimi Code 模型。 | `支持`，Chat、Anthropic、Responses 均有明确文本示例。 | 无特殊转换结论。 |
| 图片输入 | `支持`：`k3`、`k3-256k`、`kimi-for-coding`、`kimi-for-coding-highspeed` 均支持图片；官方错误页列出 URL/base64 约束。 | `不支持`：Anthropic `image` 明确不支持；Responses 的 `input_image` 会被替换为 placeholder；Chat reference 仅公开文本 user content。 | 图片是 message content，不是 tool；Kimi server-tool 过滤不得删除媒体块。DeepSeek 不能因 HTTP 成功就宣称看到了图片。 |
| 视频输入 | `支持但按模型`：`k3` 与两款 K2.7 Code 支持；`k3-256k` 不支持。 | `未找到官方 API 支持`；Anthropic/Responses 兼容表也未给出视频输入合同。 | Kimi `k3-256k` 视频应明确失败，不能静默降级成已理解视频。 |
| 普通 function/client tools | `支持`：官方 CLI 模型能力含 `tool_use`，Kimi Code 错误参考覆盖 tool-call message；完整低层 schema 未单独发布。 | `支持`：Chat function tools 与 Anthropic `tools`/`tool_use`/`tool_result`；Responses 支持 `function`。 | 工具由 host/client 实际执行；DeepSeek Tool Calls 指南明确“模型本身不执行函数”。 |
| Provider-side Web Search | `当前公开 K3 合同未证明；Science 请求形状失败，但存在成功对照`：Science/CSSwitch A/B 为保留 `web_search_20250305` 则 429、删除则 200；同一 Coding Plan 经 Claude Code + upstream cc-switch 原样透传可成功。2025 年 K2/Open Platform 公告不足以建立当前 K3 合同。 | `支持`：Claude Code/Anthropic 集成明确称由 DeepSeek API 执行 Web Search；Responses 明确支持 `web_search` / `web_search_2025_08_26` 且 server-side 执行。Chat Completions 的 built-in web search 未单独公开。 | CSSwitch 当前对精确 Kimi contract 临时 drop，待请求形状对齐后复核；DeepSeek 保留。两者都不得按名称误删普通 client function。 |
| Web Search（宿主工具） | `支持` Kimi Code CLI `WebSearch`，但必须由 host 注入搜索实现；未注入时工具不出现在 tool list。`https://api.kimi.com/coding/v1/search` 是独立服务。 | 调用方可把任意搜索器作为普通 function tool，模型只请求调用。 | Kimi CLI host-service 与 Anthropic Messages server tool 是不同端点和协议；CSSwitch 不调用它，也不能仅改名伪造执行器。 |
| Web Fetch | Kimi Code CLI `FetchURL` 同样要求 host 实现；managed CLI 配置另有 `/coding/v1/fetch` 服务。LLM endpoint 的 Anthropic `web_fetch_*` server-tool 支持 `未知`。 | `未找到 dedicated provider-side fetch 合同`；Responses 只列 `web_search`，Anthropic 集成只承诺 Web Search。普通 client function 可自行 fetch。 | 不得把 Web Search 推断为任意 URL Fetch。 |
| 代码执行 | Kimi Code CLI 的 `Bash` 是宿主本地工具；Kimi Code API provider-side code execution `未公开`。 | `不支持 provider-side`：Anthropic `code_execution_tool_result` 不支持；Responses `code_interpreter` 被忽略。Responses 的 custom `apply_patch` 是 Codex 兼容 tool call，不是 provider 沙箱。 | 本地 Bash/Python/Compute 或普通 client function 必须保留；不能因删除 `code_execution_*` 而误删它们。 |
| MCP | Kimi Code CLI 是 MCP client，可连 stdio/HTTP/SSE server；这是 CLI runtime 能力。Kimi Code API 没有公开 provider-hosted MCP/toolset 合同。 | `不支持 provider-side`：Anthropic `mcp_servers` 被忽略，`mcp_tool_use/result` 不支持；Responses `mcp` 被忽略。 | 展开的普通 MCP function 仍可由宿主调用；provider-hosted `mcp_toolset`/`mcp_servers` 是另一能力。 |
| Connectors | Kimi Code API `未找到官方 connector 合同`；CLI 可通过 MCP/插件连接外部服务。 | `未找到官方 connector 合同`；Responses/Anthropic 对 MCP 的明确状态是忽略/不支持。 | 不因 agent 客户端能接 GitHub/数据库就宣称 provider endpoint 有 connectors。 |
| Unsupported 字段行为 | Kimi Code 订阅 API 没有完整公开兼容表，行为 `未知`，应靠隔离 mock/loopback 和显式错误验证。 | Responses 官方写明 unsupported parameters 会静默忽略；Anthropic 表逐项说明 ignored/not supported。 | DeepSeek HTTP 2xx 不能单独证明工具生效；验收必须看实际 tool event/result。 |

### Tool ID 与多轮历史矩阵

| 路径 | 官方明确要求 | 仍未知/不可外推 |
|---|---|---|
| Kimi Code OpenAI-compatible | Kimi Code 错误参考：thinking 开启时，assistant tool-call message 必须带 `reasoning_content`，缺失返回 400。官方 Kimi Code CLI changelog：缺少配对 tool result 会导致下一请求 `missing tool_call_id`；中断恢复、重复 ID、`tool_use/tool_result` 邻接均需要修复。 | Kimi Code 订阅端点没有独立公开 schema 说明 `role=tool.tool_call_id` 是否必须逐字匹配、允许哪些 ID 字符、并行工具的排序/邻接规则。Kimi 开放平台的 exact-match 说明属于另一套 Base URL/凭据，不能作为这里的合同。 |
| Kimi Code Anthropic-compatible | 官方确认 Claude Code 可接入，且 Kimi Code CLI 会在 Anthropic provider 上保留先前 thinking；Kimi Code 错误页证明 tool-call history 对 thinking 内容敏感。 | 未公开 `tool_use.id`、`tool_result.tool_use_id`、Anthropic server-tool ID 和多轮 adjacency 的字段级 Kimi Code 合同。兼容名称不能填补这些空白。 |
| DeepSeek OpenAI Chat | API 是 stateless；每轮必须重发全部历史。Tool Calls 示例把完整 assistant response 加入 `messages`，再加入 `role=tool` 且 `tool_call_id=tool.id` 的结果。thinking + tools 时，历次 `reasoning_content` 必须完整回传，否则 400。 | 官方未单列 tool ID 的字符/长度限制；不要改写、截断或跨 provider 复用 ID。 |
| DeepSeek Anthropic | `tool_use.id` 与 `tool_result.tool_use_id` 均为 fully supported；API 仍由调用方提交 messages 历史。 | 官方兼容页未说明 orphan/duplicate ID 的具体错误、严格邻接规则或 thinking block signature 规则。应保持原始 assistant blocks 与对应 result，不自行合成。 |
| DeepSeek Responses | `previous_response_id`、`conversation`、`store` 均不支持，API stateless；调用方必须在后续 input 中重发 `function_call`/`function_call_output`，`web_search_call` 要原样传回，服务器会恢复搜索结果。 | 官方指南未列 `call_id` 的额外校验规则。不能依赖 server-side stored response state。 |

### CSSwitch 当前代码模式

| 文件 | 发现与关联 |
|---|---|
| `.trellis/tasks/08-03-kimi-protocol-compatibility/prd.md:9-24` | 记录 8990 上同一 Kimi 新会话的两轮失败链，并已要求区分 Kimi Code 视频、CLI 宿主 WebSearch 与开放平台工具。 |
| `.trellis/tasks/08-03-kimi-protocol-compatibility/design.md:43-67` | 设计明确：Science 是 tool orchestration host；Kimi typed server tools 要过滤，普通 client tools/媒体要保留，错误不得伪造修复。 |
| `catalog/provider-contracts.v1.json:5-23` | CSSwitch 的 DeepSeek managed contract 当前是 Anthropic Messages。 |
| `catalog/provider-contracts.v1.json:68-86` | Kimi managed contract 是 `kimi-anthropic-relay`、Anthropic Messages、thinking enabled。 |
| `desktop/gateway/src/anthropic_compat.rs:631-641` | 本地按 Anthropic typed `mcp_toolset`、`web_search_*`、`web_fetch_*`、`code_execution_*` 等识别 provider server tools。 |
| `desktop/gateway/src/anthropic_compat.rs:643-720` | 普通 client tools 先规范化；旧逻辑仅在 model name 含 `kimi` 时过滤 server tools。 |
| `desktop/gateway/src/anthropic_compat.rs:748-811` | Kimi 历史修复同样依赖旧 model-name 判定。 |
| `desktop/gateway/src/server.rs:1477-1521` | relay 请求与 stream/nonstream Kimi response filter 的实际接入点；响应过滤仍依赖 target model name。 |
| `catalog/capabilities.v1.json` | capability catalog 以精确 `kimi-anthropic-relay` 把 typed WebSearch 记为 limited/drop 临时 workaround；DeepSeek 对照仍保留。 |

### Kimi Code CLI 宿主搜索实现补充

对官方开源仓库 `MoonshotAI/kimi-code` 当前 HEAD `29c9e2ab20a1646ad33f2b7c999b450152f9c01a` 的只读源码核对进一步确认：

- `WebSearch` 与 `FetchURL` 是 Kimi Code CLI 注册的普通 Agent tools，不是 `/v1/messages` 自动执行的 typed server tools；只有 runtime 提供 `webSearcher` / `urlFetcher` 时才注册。
- `MoonshotWebSearchProvider` 调用配置的 `/coding/v1/search`：Bearer 鉴权，POST `{"text_query":"..."}`，读取 `search_results[]` 的 title/url/snippet/date/site_name；该独立服务不能作为 Claude Messages WebSearch 适配证据。
- `MoonshotFetchURLProvider` 调用 `/coding/v1/fetch`：Bearer 鉴权，POST `{"url":"..."}`，请求 markdown；官方 CLI 在远端 fetch 失败时会转本地 fetcher。
- 因此这些 endpoint 具备公开源码可审计的独立 host-service 协议，但不属于本任务的 Kimi Anthropic Messages 路径。CSSwitch 既不调用它们，也不能只把 Anthropic typed tool 改名；后者还会缺少 Agent tool loop、独立服务授权和真实 tool result。
- CSSwitch 若将来复用这两个 service，不应复制 Kimi CLI 的静默 fetch fallback；本项目的 debug-first 合同要求远端失败明确暴露，除非用户另行批准显式 fallback。

### 8990 隔离 Science 真实验收观测清单

前提：只有在用户另行明确授权“运行真实 provider 测试”后执行。验收只消费已配置实例的调用能力，不读取、打印或导出任何 API Key、OAuth token、Keychain、cookie、nonce、进程环境、真实配置、私有日志或网络鉴权 header/body。

#### Kimi 修复主路径

1. 在 8990 新建会话，页面确认 provider 为内置 Kimi、model 为不含 `kimi` 的 `k3-256k`、thinking 保持开启；只记录这些非敏感显示值。
2. 用普通公开问题触发首轮，并从安全 metadata 确认 Science 的 typed `web_search_20250305` 被 Kimi policy 删除。
   - 请求不应再因该 declaration 返回 429，也不应泄漏 `Search results for query:`、`server_tool_use`、`web_search_tool_result` 或 raw tool ID。
   - 该结果只证明普通对话恢复；没有真实搜索 tool/result 时，不得把回答记作联网搜索成功。
3. 在同一会话发送第二轮追问。
   - 必须继续成功，不得出现 `400 invalid_request_error: tool_call_id is not found`。
   - 只看页面中的完成/错误状态与脱敏错误摘要，不读取历史存储或 wire payload。
4. 使用显式创建的无敏感临时 fixture 触发一个普通 client tool（例如读取固定文本文件），再继续一轮。
   - 普通 tool card 应完成，下一轮应保留结果；这证明 Kimi server-tool filter 没有误删 client tool。
5. 给 `k3-256k` 提交合成小图片，应正常理解；合成视频应明确拒绝/报不支持，不能声称已理解。若另测 `k3` 视频，必须新开会话，避免模型/媒体历史混用。

#### DeepSeek 对照（仅在另有真实调用授权时）

- Anthropic/Claude Code 路径出现受控的 provider Web Search tool flow 可以是官方能力，不能因它与 Kimi 同名就过滤。
- 同一搜索会话第二轮仍需成功；若使用 thinking + client tools，应完整保留 reasoning/tool history。
- 不把 `code_execution_*`、provider MCP 或 connector 当验收成功条件；官方明确不支持这些 provider-side 能力。

#### 可留存证据与禁止项

| 可留存 | 禁止读取/留存 |
|---|---|
| 测试时间、CSSwitch/Science 显示版本、provider/model 非敏感标签、页面成功/错误状态、已脱敏错误类别、8990 loopback health、managed daemon PID 在测试前后是否稳定 | API Key/OAuth/Keychain、cookie/nonce、进程环境、真实 `~/.claude-science`、真实 CSSwitch 配置、私有日志、网络请求/响应正文或 header、含用户数据的截图 |

该观测范围与项目规则一致：`.agents/rules/safety.md:5-12` 禁止读取凭据、真实 Science 数据与私有日志；`.trellis/spec/backend/quality-guidelines.md:46-50` 将 live acceptance 限定为页面状态、loopback health 与 daemon PID。

### External references

全部来源均为提供方官方页面；访问日期均为 2026-08-03。

#### Anthropic

- [Web search tool](https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool)：标准 Messages 请求直接声明完整 `web_search_20250305` type。
- [Tool reference](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-reference)：`web_search_20250305` 为 Server / GA；日期后缀表示并存的 schema 版本。
- [Server tools](https://platform.claude.com/docs/en/agents-and-tools/tool-use/server-tools)：basic Web Search 当前没有专用 beta query/header 要求。

#### Kimi Code（会员订阅系统）

- [Kimi Code 产品概览](https://www.kimi.com/code/docs/)：两种协议、Base URL、当前模型 ID，以及会员订阅系统与 Kimi 开放平台的明确区分。
- [Kimi Code 模型配置](https://www.kimi.com/code/docs/kimi-code/models.html)：当前四个 model ID、图片/视频矩阵、thinking/effort 与切换会话建议。
- [Kimi Code Claude Code 接入](https://www.kimi.com/code/docs/en/third-party-tools/claude-code.html)：Anthropic-compatible endpoint 的官方接入路径。
- [Kimi Code Codex 接入](https://www.kimi.com/code/docs/third-party-tools/codex)：明确 Codex Responses 与 Kimi Chat Completions 需要本地转换；该页同时标注 CC Switch 方案为第三方、不在 Kimi 官方维护范围。
- [Kimi Code Error Reference](https://www.kimi.com/code/docs/en/kimi-code/error-reference.html)：Kimi Code endpoint、图片约束、thinking tool-call history 的 `reasoning_content` 400、工具执行错误边界。
- [Kimi Code CLI Built-in Tools](https://www.kimi.com/code/docs/en/kimi-code-cli/reference/tools.html)：`Bash`/`ReadMediaFile` 的宿主执行，以及 `WebSearch`/`FetchURL` 必须由 host 提供实现。
- [Kimi K2 模型更新公告](https://platform.kimi.com/blog/posts/kimi-k2-0905)：历史上声明 K2/Open Platform 的 Anthropic-compatible API 支持 WebSearch Tool；它不是当前 Coding Plan K3 `/coding/` 的直接合同，不能覆盖本任务 A/B 结果。
- [Kimi Code CLI Configuration files](https://www.kimi.com/code/docs/en/kimi-code-cli/configuration/config-files)：managed Kimi 模型 capability 标签与独立 `/search`、`/fetch` service 配置。
- [Kimi Code CLI official source](https://github.com/MoonshotAI/kimi-code/tree/29c9e2ab20a1646ad33f2b7c999b450152f9c01a/packages/agent-core/src/tools/providers)：`MoonshotWebSearchProvider` / `MoonshotFetchURLProvider` 的 host-side HTTP 请求与结果映射实现。
- [Kimi Code CLI MCP](https://www.kimi.com/code/docs/en/kimi-code-cli/customization/mcp.html)：CLI 是 MCP client，连接 stdio/HTTP/SSE 外部 server。
- [Kimi Code CLI changelog](https://www.kimi.com/code/docs/en/kimi-code-cli/release-notes/changelog.html)：v0.20.2（2026-06-29）增加 Kimi Anthropic-compatible/video；后续版本记录 missing tool result、重复 tool ID、tool adjacency 与 thinking history 修复。

#### DeepSeek

- [Using the Anthropic API](https://api-docs.deepseek.com/guides/anthropic_api/)：Anthropic Base URL、逐字段 tools/message/server-tool/MCP/code-execution 兼容表。
- [Using the Responses API](https://api-docs.deepseek.com/guides/responses_api/)：Responses model 范围、stateless history、server-side web search、`apply_patch`、ignored code interpreter/MCP 与图片 placeholder 行为。
- [Integrate with Claude Code](https://api-docs.deepseek.com/quick_start/agent_integrations/claude_code/)：DeepSeek Anthropic 接入，以及由 DeepSeek API 原生执行 Claude Code Web Search 的明确声明。
- [Tool Calls](https://api-docs.deepseek.com/guides/tool_calls/)：client function tool、host 执行与 `tool_call_id` 回传示例。
- [Thinking Mode](https://api-docs.deepseek.com/guides/thinking_mode/)：thinking + tools 时完整回传 `reasoning_content` 的 400 合同。
- [Multi-round Conversation](https://api-docs.deepseek.com/guides/multi_round_chat/)：Chat Completions stateless，调用方每轮重发完整历史。
- [Create Chat Completion](https://api-docs.deepseek.com/api/create-chat-completion)：当前 Chat schema、文本 message 与 function tool ID 字段。
- [DeepSeek change log](https://api-docs.deepseek.com/updates)：2026-04-24 V4-Pro/V4-Flash 同时支持 OpenAI Chat Completions 与 Anthropic interface。

### Related specs

- `.trellis/spec/backend/quality-guidelines.md:20-50`：官方/隔离 Science 所有权、loopback origin 与 live acceptance 可观察边界。
- `.trellis/spec/backend/error-handling.md:1-51`：当前仍是未填充模板，未提供本任务可复用的 provider error contract；本次以 task design 的 fail-closed 约束为准。
- `.trellis/tasks/08-03-kimi-protocol-compatibility/design.md:43-67`：本任务的 server-tool/client-tool ownership 与显式失败设计。

## Caveats / Not Found

- 本研究初版官方矩阵没有读取任何凭据、配置或私有请求正文；任务后续只通过已配置的隔离 8990 完成了脱敏 Kimi A/B。A/B 只确认当前 `web_search_20250305` 请求形状触发 429，不外推 Kimi 所有端点或未来版本永久不支持搜索。
- Kimi Code 订阅 API 没有公开类似 DeepSeek 的完整字段级兼容表；特别是 Anthropic typed server tools、`tool_use.id`/`tool_result.tool_use_id` 的精确约束、并行/邻接与 multi-turn server-tool history 都未找到正式合同。`/coding/v1/search` 的存在不能填补这个空白。
- Kimi 开放平台的 `$web_search`、Formula、tool-call exact ID 文档属于 `api.moonshot.cn`/开放平台的另一套凭据和协议；本矩阵没有把它们迁移成 Kimi Code 订阅端点能力。
- DeepSeek Anthropic 文档同时声明 `server_tool_use`/`web_search_tool_result` supported，Claude Code 指南也确认 provider-side search；但 typed web-search request 的版本化 type、ID 与失败历史细则没有在兼容表中展开。
- DeepSeek Responses 会静默忽略 unsupported parameters。仅看 2xx 无法证明 image、code interpreter、MCP 或其他 built-in tool 生效。
- 任务后续已在用户授权的隔离 8990 完成 Kimi 单变量 A/B；本文不把该局部证据扩大成全部 provider/full acceptance 已完成。
