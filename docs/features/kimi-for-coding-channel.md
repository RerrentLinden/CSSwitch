# Kimi for Coding 渠道

订阅制编码端点 `https://api.kimi.com/coding`，与开放平台 Kimi（`api.moonshot.cn/anthropic`）是两个独立服务，
鉴权、模型清单、服务端搜索支持和缺陷集都不同，因此使用独立的 provider contract，不共用任何分支。

## 后端契约（已交付）

| 项 | 值 |
| --- | --- |
| template id | `kimi-coding` |
| provider contract | `kimi-coding-anthropic` |
| adapter / transport | `relay` / `anthropic_messages` |
| 鉴权 | `bearer` |
| 端点拼接 | `anthropic_v1`（base + `/v1/messages`） |
| 默认 base_url | `https://api.kimi.com/coding`（可编辑） |
| 模型发现 | `anthropic_models_or_manual`（`/v1/models` 实测可用） |
| thinking policy | `upstream_default` |
| 模型预设 id | `kimi-coding` |

模型预设（来自实测 `/v1/models`）：

| upstream_model | display_name | context | 角色绑定 |
| --- | --- | --- | --- |
| `kimi-for-coding` | K2.7 Coding | 262k | opus / sonnet，默认 |
| `kimi-for-coding-highspeed` | K2.7 Coding Highspeed | 262k | haiku |
| `k3` | K3 | 1M | fable |
| `k3-256k` | K3-256k | 262k | — |

## 上游缺陷与已实施的补偿

四条都由真实 live 请求确认，并各自绑定 capability 规则与单测。

### 1. 非标准 `thinking: {"type":"auto"}` 导致静默不思考

Claude Science 发送非标准的 `auto`。`k3` 系列不认识该取值，返回 200 但**不含任何 thinking 块**；
而省略该字段时四个模型都正常思考。

补偿：删除 `auto` / `adaptive`，交还上游默认。标准的 `enabled` / `disabled` 原样透传。
规则 `provider.kimi-coding.thinking-upstream-default`。

**不引入** thinking 续写机制：实测该端点接受 thinking 块被剥离的历史（含带 `tool_use` 的消息），
DeepSeek 的链断裂 400 问题不适用于此渠道。

### 2. 指定工具型 `tool_choice` 与默认思考冲突

`tool_choice: {"type":"tool", "name": …}` 在思考开启时返回
`400 tool_choice 'specified' is incompatible with thinking enabled`。
由于上游思考默认开启，**即使请求完全不带 `thinking` 字段也会失败**。
Science 的工作项分类器（`create_work_item`）正是这个形状，每建一个工作项触发一次。

补偿：仅当 `tool_choice.type == "tool"` 时置 `thinking: {"type":"disabled"}`，保留强制工具本身
（分类器依赖拿到结构化输出）。`any` 与 `auto` 上游无此问题，不做任何补偿。
规则 `provider.kimi-coding.specified-tool-choice-disables-thinking`。

### 3. 声明 web_search 但未搜索时返回 429

上游确实原生支持 `web_search_20250305`，但只要声明了该工具而模型本轮**没有实际发起搜索**，
就返回 `429 rate_limit_error: "The engine is currently overloaded"`。
已排除 max_tokens、轮次、system prompt、客户端工具、`max_uses`、`tool_choice`、提示复杂度等变量；
DeepSeek 的 Anthropic 端点无此缺陷，属 Kimi 独有。

Science 每轮都声明该工具（25 个工具里的第 1 个），因此真实会话**每一轮都失败**，
Science 侧表现为把 429 当容量问题无限退避重试。

补偿：**客户端工具桥接**（规则 `tool.kimi-coding.web_search.client-tool-bridge`，
实现见 `desktop/gateway/src/kimi_coding_search.rs`）。

```
Science 声明 web_search_20250305(server)
        │
   gateway 换成同名客户端工具 web_search{query}      ← 客户端工具从不触发 429
        │
   上游调用 ①
        ├─ 模型没调 web_search → 原样透传，零额外开销（绝大多数轮次）
        └─ 模型发出 tool_use{web_search, query}      ← 这是"本轮需要搜索"的确定信号
                │
           gateway 截获，不转发给 Science
                │
           上游调用 ②：原对话 + "Use web_search to look up: Q" + 真 server 工具
                │
           把返回的 server_tool_use / web_search_tool_result 拼进同一条消息
                │
           Science 按原生 web_search 渲染
```

关键约束与已实现的处理：

- **拼接必须发生在过滤流内部。** gateway 会用 `anthropic_sse::Validator` 校验输出流的生命周期，
  并把 `message_stop` 扣留到干净 EOF；在转发循环结束后追加帧会被判为流被截断。
  因此过滤器自己持有配置并内联发起补发调用，替换掉上游那个 `stop_reason: tool_use` 的终止帧，
  `message_stop` 仍用上游的。
- **客户端 `tool_use` 绝不泄漏给 Science**：Science 把 web_search 声明为 server 工具，没有本地执行器，
  泄漏会让该轮永久挂起。
- **content block index 连续无空洞**：吞掉的块不占用输出索引，拼入的块从当前输出索引继续。
- **多个查询**：调用①可能返回多个 `web_search` 调用，去重后上限 4 个。
- **失败显式暴露**：调用②失败（含与搜索无关的偶发 429）时发出终止 SSE error，
  不伪造助手内容、不静默降级。

非流式走同一套逻辑：检测到桥接工具调用后发补发请求，把搜索证据并入响应并剔除该工具调用。

另注：该端点还存在**与 web_search 无关的偶发 429**（一次无工具基础请求也曾 429）。
这也是不能采用"遇 429 就去掉工具重发"那种方案的原因——它会把偶发 429 误判成"本轮没搜索"。

模型行为观察：当 Science 的其余 24 个客户端工具同时可用时，模型有时会绕开 web_search 改用 `bash`
等工具，甚至声称环境里没有 web_search。这是模型的选择而非链路故障，桥接在这种轮次保持空闲。

### 4. 不接受 Anthropic `document` 内容块

上游的 Anthropic 兼容层没有实现 `document` 块。实测四种 source 形态全部失败且报文一致
（base64 `application/pdf`、`text/plain`、`url`、以及不带 source 的裸 `{"type":"document"}`），
均为 `400 invalid_request_error: "Invalid request Error"`。
决定性对照：一个**根本不存在的块类型**返回完全相同的报文 —— 说明 `document` 走的是解析器的"未知块"分支，
不是格式不匹配。

由于该块会留在对话历史里，**一个附件会让此后每一轮都失败**。真实捕获印证：同一会话 27 条消息时成功，
29 条时因某个 tool_result 轮携带了 PDF 而失败。

这个块是 Claude Science **平台 PDF 视觉通道**的载荷。真实抓包显示 `read_file(pages=[1])` 会返回
`tool_result {"status":"queued_for_vision"}`，随后追加一个 `[System] Attached file: …` 文本块，
再把 PDF 作为 `document` 块附上。

载荷是**真正的 PDF 而非渲染好的图片**：`media_type` 为 `application/pdf`，实测 134 KB，
而磁盘上的原文件是 1.28 MB / 21 页 —— 即 Science 抽取请求页另存为一个更小的 PDF 再发送。
`queued_for_vision` 描述的是意图（交给模型的视觉能力），不是传输形式；把 PDF 渲染成像素是
**模型服务商那一侧**的能力，官方 Anthropic 的 `document` 块原生支持 PDF。

所以**该平台路径在本渠道不可用，CSSwitch 也无法让它可用** —— 文件根本送不到上游，谈不上渲染。

补偿：把每个 `document` 块替换为一段署名的说明文本（注明 media type），让该轮得以继续而不是整轮失败，
并指向真正可行的路径。规则 `provider.kimi-coding.document-block-placeholder`。

**准确地说损失了什么**：丢失的是 Science 平台侧的 PDF→视觉通道。**读 PDF 本身仍然可行** ——
Agent 可以自己把页面渲染成图像再作为 `image` 块传入，上游接受；真实会话中一份扫描版专利正是这样读到的。
这也不是 CSSwitch 相对同类渠道的倒退：DeepSeek 端点虽接受该块，但带与不带附件都回答 `CANNOT_READ`
（合成标记 PDF 与真实 133KB 专利均如此）。

占位文本必须署名。实测中模型引用了未署名的占位文本，被追问来源后**把一个正确结论当作自己编造的撤回了**；
因此文案明确标注来自 CSSwitch 网关、不是工具返回，并给出"读文件并渲染成图像"的可行替代。

`image` 块不受影响，上游可正常接受。

## 桌面端 UI

已交付，`desktop/src/main.js`：

- **模型连接页入口** —— 「新建配置 → 选择服务商」网格中以「Kimi for Coding」出现，分类 `国内`，
  紧邻 Kimi（Moonshot）。真实后端由 `templates.rs` 的 `list_templates` 驱动；
  预览/mock 模式另有一份 `MOCK_TEMPLATES` 条目需同步，两者已一致。
- **图标** —— 复用 Kimi 品牌图标。`modelFamilyKey()` 中为 `kimi-coding` 加了显式映射，
  不再依赖 base_url 正则兜底。
- **兼容性提示** —— 模板 `compatibility_notice` 在新建与编辑表单的模型配置说明行后呈现，
  文案说明 429 缺陷**以及桥接已让联网搜索可用**，避免被误读成能力缺失。
- **模型预设** —— 四个模型按 `catalog/model-presets.v1.json` 预填到默认 / 高质量 / 快速 / Fable 四槽。
  预览模式的通用规则会把最后一个模型当作 haiku，与真实预设不符，因此加了
  `MOCK_ROLE_BINDING_OVERRIDES` 使预览与真实目录一致。
- **base_url** —— 默认 `https://api.kimi.com/coding`，可编辑。

已在浏览器 mock 模式下可视化验收：服务商网格、默认值预填、四槽模型、创建后入列与图标、
编辑视图回读、深色主题，均正常。

不需要在前端重复实现的：contract、鉴权、模型发现、thinking 与工具策略都由后端契约决定。
