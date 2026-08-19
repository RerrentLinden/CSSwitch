# Kimi 渠道

Kimi 走一个 provider contract（`kimi-anthropic-relay`），默认地址
`https://api.kimi.com/coding`，可在控制台改成开放平台或其它区域端点；
模型清单由用户在四个槽里自己填，所以换端点不需要改代码。

> **证据边界**：补偿的实测证据来自 `api.kimi.com/coding`，2026-08-19 在 K3 上复测。
> K2.7 系列与开放平台按用户判定沿用同一套补偿，**未独立实测**。若行为不符，
> 表现应为可见错误而非静默降级。

## 后端契约

| 项 | 值 |
| --- | --- |
| provider contract | `kimi-anthropic-relay` |
| adapter / transport | `relay` / `anthropic_messages` |
| 鉴权 | `bearer` |
| 端点拼接 | `anthropic_v1`(base + `/v1/messages`) |
| 默认 base_url | `https://api.kimi.com/coding`(可编辑) |
| thinking policy | `upstream_default`(由契约声明,不靠环境变量) |

默认预填的模型槽(来自实测 `/v1/models`,均可在控制台改):

| 槽 | upstream_model | 显示名 |
| --- | --- | --- |
| 默认(均衡) | `k3-256k` | Kimi K3 256k |
| 高质量 | `k3` | Kimi K3 |
| 快速 | `kimi-for-coding` | Kimi K2.7 |
| Fable | 留空 | 继承默认槽 |

端点当前提供:`kimi-for-coding`、`kimi-for-coding-highspeed`、`k3`、`k3-256k`。

## 上游缺陷与已实施的补偿

四条都由 `api.kimi.com/coding` 的真实 live 请求确认，各自绑定规则 ID 与单测。

### 1. 非标准 `thinking: {"type":"auto"}` 导致静默不思考

Claude Science 发送非标准的 `auto`。**上游不报错**,只是不思考——这正是它危险的地方:
Science 侧看不出任何异常,表现只是"这个模型怎么变笨了"。

2026-08-19 直连复测(同一问题,只改 thinking 字段,看返回的内容块):

| 模型 | 省略字段 | `auto` | `adaptive` | `enabled` |
| --- | --- | --- | --- | --- |
| `k3` | thinking+text,105 字 | **只有 text,0 字** | thinking+text,**4 字** | thinking+text,67 字 |
| `kimi-for-coding` | thinking+text,126 字 | thinking+text,104 字 | thinking+text,107 字 | thinking+text,172 字 |

缺陷**只在 K3 上**:`auto` 让它彻底不思考,`adaptive` 也几乎不思考(4 字对 105 字)。
K2.7 两种取值都正常。

补偿:删除 `auto` / `adaptive` 两个取值,交还上游默认(省略字段时 K3 正常思考)。
标准的 `enabled` / `disabled` 连同 `budget_tokens` 原样透传,effort 类字段一概不碰。
规则 `provider.kimi.thinking-upstream-default`。

**不引入** thinking 续写机制:实测该端点接受 thinking 块被剥离的历史(含带 `tool_use` 的消息),
DeepSeek 的链断裂 400 问题不适用于此渠道。

### 2. 指定工具型 `tool_choice` 与默认思考冲突

`tool_choice: {"type":"tool", "name": …}` 在思考开启时返回
`400 tool_choice 'specified' is incompatible with thinking enabled`。
由于上游思考默认开启，**即使请求完全不带 `thinking` 字段也会失败**。
Science 的工作项分类器（`create_work_item`）正是这个形状，每建一个工作项触发一次。

补偿：仅当 `tool_choice.type == "tool"` 时置 `thinking: {"type":"disabled"}`，保留强制工具本身
（分类器依赖拿到结构化输出）。`any` 与 `auto` 上游无此问题，不做任何补偿。
规则 `provider.kimi.specified-tool-choice-disables-thinking`。

### 3. 声明 web_search 但未搜索时返回 429(**观察期**)

> 2026-08-19 复测:连续两次"声明 web_search 但模型未搜索"均返回 200,**未复现 429**。
> 上游可能已修。桥接代码与规则保留待观察;若 K3 日常使用持续无 429,可退役该桥。
> 下面是当初的原始记录。


上游确实原生支持 `web_search_20250305`，但只要声明了该工具而模型本轮**没有实际发起搜索**，
就返回 `429 rate_limit_error: "The engine is currently overloaded"`。
已排除 max_tokens、轮次、system prompt、客户端工具、`max_uses`、`tool_choice`、提示复杂度等变量；
DeepSeek 的 Anthropic 端点无此缺陷，属 Kimi 独有（编码端点实测）。

Science 每轮都声明该工具（25 个工具里的第 1 个），因此真实会话**每一轮都失败**，
Science 侧表现为把 429 当容量问题无限退避重试。

补偿：**客户端工具桥接**（规则 `tool.kimi.web_search.client-tool-bridge`，
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
并指向真正可行的路径。规则 `provider.kimi.document-block-placeholder`。

**准确地说损失了什么**：丢失的是 Science 平台侧的 PDF→视觉通道。**读 PDF 本身仍然可行** ——
Agent 可以自己把页面渲染成图像再作为 `image` 块传入，上游接受；真实会话中一份扫描版专利正是这样读到的。
这也不是 CSSwitch 相对同类渠道的倒退：DeepSeek 端点虽接受该块，但带与不带附件都回答 `CANNOT_READ`
（合成标记 PDF 与真实 133KB 专利均如此）。

占位文本必须署名。实测中模型引用了未署名的占位文本，被追问来源后**把一个正确结论当作自己编造的撤回了**；
因此文案明确标注来自 CSSwitch 网关、不是工具返回，并给出"读文件并渲染成图像"的可行替代。

`image` 块不受影响，上游可正常接受。

## 控制台

模型配置在 WebUI 的「渠道配置 → Kimi」里:base_url、API Key、四个模型槽
(默认必填,其余留空继承默认)、「获取可用模型」直接向上游拉清单。
显示名就是 Science 模型菜单里看到的名字。
