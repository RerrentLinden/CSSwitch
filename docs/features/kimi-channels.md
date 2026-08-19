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

### 3. web_search:桥接已退役,改为原生透传 + 历史配对修复

#### 曾经的理由:429

历史上只要**声明了 web_search 却没实际搜索**就返回 429(引擎过载),而 Science
每轮都声明它,于是真实会话每轮都失败。当时的对策是把服务端工具换成同名客户端工具
(920 行的 `kimi_coding_search.rs`)。

2026-08-19 复测 **32 次全部 200,零 429**(k3 与 kimi-for-coding 各一轮,含
"只声明 web_search"与"web_search + 24 个客户端工具"两种形态)。该缺陷不再复现。

#### 真正的硬缺陷:搜索结果送不回历史

关掉桥接原生透传后,第 1 轮搜索成功,第 2 轮必然
`400 tool_call_id  is not found`。逐步定位到根因,过程中推翻了两个旧说法:

| 曾经的说法 | 实测结论 |
| --- | --- |
| "两个空格说明 id 为空" | **错**。普通工具用一个非空但不匹配的 id,报错完全相同。该报错从不插入具体 id,双空格只是文案格式缺陷 |
| "结果块不携带 tool_use_id" | **错**。Kimi 发出的块携带它 |

真实骨架(诊断 `CSSWITCH_DEBUG_TOOL_SKELETON=1` 打出):

```
m3/assistant: web_search_tool_result = srvtoolu_da2nr326d89s73enoue0
m3/assistant: tool_use              = tool_lATdsqlOGD0n8Cgv
m4/user:      tool_result           = tool_lATdsqlOGD0n8Cgv
```

**同一条消息里没有任何 `server_tool_use`** —— Science 落盘时只保留了结果块,
把与之配对的请求块丢了。Kimi 的兼容层要求这一对能配上,于是拒收。

另有一种形态:两个块都在,但 Kimi 自己发出的 `server_tool_use.id`(`tool_…`)
与 `web_search_tool_result.tool_use_id`(`srvtoolu_…`)本就不是同一个值 ——
把它自己的输出原样回传,它自己拒收。

#### 修复:让这一对配得上

规则 `provider.kimi.web-search-result-pairing-repair`,约六十行:

| 历史形态 | 动作 | 实测 |
| --- | --- | --- |
| 结果块孤立(无 `server_tool_use`) | 在它前面补一个同 id 的 `server_tool_use` | 400 → **200** |
| 两块都在但 id 不匹配 | 把结果块的 id 改成前面最近的 `server_tool_use.id` | 400 → **200** |
| 已经配得上 | 不动,也不记规则 | — |
| 连 `tool_use_id` 都没有 | 不瞎编 | — |

选择补块而不是丢块:丢掉结果块同样能过(实测 200),但会丢失这一轮的搜索证据。

#### 净结果

删除 920 行桥接 + 响应侧剥离过滤器 + 13 个相关测试,换成一条约六十行的窄规则。
`web_search_20250305` 现在原样透传给上游(规则
`tool.kimi.web_search.server-tool-preserve`),Science 按原生 web_search 渲染。

真机验收:全新会话连续多轮联网搜索,历史推进到 5 条消息,修复规则命中,
**零 upstream_failure**。

> 若 429 卷土重来(它是负载相关的),桥接的完整实现保留在 git 历史里,
> 提交 `1bdbbbd` 之前的 `desktop/gateway/src/kimi_coding_search.rs`。

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
