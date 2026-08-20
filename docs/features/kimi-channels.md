# Kimi 渠道

Kimi 走一个 provider contract（`kimi-anthropic-relay`），默认地址
`https://api.kimi.com/coding`，可在控制台改成开放平台或其它区域端点；
模型清单由用户在四个槽里自己填，所以换端点不需要改代码。

> **证据边界**：补偿证据来自 `api.kimi.com/coding`。2026-08-20 当前 query-tool bridge
> 已分别用 K3、K3-256k、K2.7 在真实 Claude Science 搜索/追问/重载通过；开放平台和其它
> 未列模型沿用同 contract，但没有独立 live，异常必须可见失败。

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

核心补偿均由 `api.kimi.com/coding` 的真实 live 请求确认，各自绑定规则 ID 与单测；
Web Search bridge 另有 K3、K3-256k、K2.7 三模型的最终 Science 验收。

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

### 3. web_search:统一 query-tool bridge + 原生 nested executor

#### 曾经的理由:429

历史上只要**声明了 web_search 却没实际搜索**就返回 429(引擎过载),而 Science
每轮都声明它,于是真实会话每轮都失败。当时的对策是把服务端工具换成同名客户端工具
(920 行的 `kimi_coding_search.rs`)。

2026-08-19 复测 **32 次全部 200,零 429**，一度退役桥接改为原生 inline。随后 review
发现功能合同仍不成立：K2.7/Science 会只发幻影对并声称搜索不可用，K3-256k 也会忽略 typed
search；即使 direct 能搜索，各模型对 search result→final answer 的行为仍不一致。因此
2026-08-20 恢复早期桥接语义，但没有机械恢复旧 920 行实现。

当前规则 `provider.kimi.web-search.query-tool-adapter`：

```text
Science typed web_search
  -> main: 私有 ordinary query tool（模型决定是否搜索）
  -> 不搜索: 1 call，原回答/普通工具返回
  -> 搜索: nested typed web_search（指定工具 + thinking disabled）
       -> 有正文: 2 calls
       -> 只有真实 pair: bounded synthesis，第 3 call 生成正文
  -> Science: 单一 message lifecycle、真实 Web Search 卡片/正文；内部工具零泄漏
```

主调用保留 Science 完整 system/messages/compute/client tools；query/evidence 以 untrusted data
传递，不作为指令执行。nested/synthesis 输出上限分别 4096/8192，evidence 总量 512 KiB，
三阶段共享一次 contract 180s deadline，无 retry、command fallback 或伪造成功。原生 typed 路径
仍保留为 nested executor，并继续使用噪声/幻影/采钥规则。

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
| 连 `tool_use_id` 都没有,且内容为空 | 整块删除(零证据损失) | 真实会话 400 → **200** |
| 连 `tool_use_id` 都没有,但有内容 | 合成确定性配对键补上两半 | 单测锁定 |

选择补块而不是丢块:丢掉结果块同样能过(实测 200),但会丢失这一轮的搜索证据。
无 id 孤儿是上游"幻影空搜索"(见 §3c)经 Science 落盘后的产物,留着它每一轮都 400;
配对键只是相关性标记——上游自己发的两半 id 本来也对不上,合成键并不比原状更假。

#### 渠道级开关(2026-08-20)

控制台「渠道配置 → Kimi」有一个联网搜索开关,存进
`~/.csswitch/service.v1.json` 的 `kimi.web_search`,**默认开启**。存量配置没有这个
字段时按开启读(自定义 serde 默认值;`bool` 的原生默认是 `false`,直接用它等于给老用户
静默关掉搜索)。

关闭时,typed `web_search` 声明在 relay 请求入口、**补偿链之前**被摘除,记规则
`tool.relay.web-search-disabled-by-config`。摘在链首而不是链尾的 server tool policy 里,
是因为 §3e 的尾随上下文重排以"本轮声明了 typed search"为触发条件:在链尾摘,重排会先在一个
根本不会搜索的轮次里交换末尾两条 user message。链首摘除后整条链一致地按"本轮没有搜索声明"
处理,§3 的 bridge 自然不触发,上游只有一次调用。

配置是每请求读快照的,所以开关保存后下一次请求即生效,不需要重启 Science
(地址与模型槽仍需重启,因为 Science 只在启动时读它们)。

已知边界:关闭态**不清洗历史**里已有的 `server_tool_use` / `web_search_tool_result` 块。
§3 的请求侧配对修复对 Kimi 流量无条件运行,配对完整性不受影响;但"本轮不声明该工具却回放
它的历史"这一形状**未单独取证**,判据保留在 `test/LIVE_ACCEPTANCE.md`。

2026-08-20 真机验收:用户在真实 Science + Kimi 渠道上实测通过,开关行为符合预期,
未发现问题。上面那条历史回放形状不在本次取证范围内,仍按未验证记录。

#### 当前净结果

三个模型统一走一个主 bridge；搜索轮才增加 nested/synthesis，非搜索轮仍只有一次 main call。
早期约 920 行状态机没有原样恢复；当前实现复用 `messages` transport、SSE validator、
`kimi_search_noise` 与历史修复。日志只记录 rule/model、bridged/query/call 数与阶段耗时，不记录 query、
结果或正文。

> 第一次退役(2026-08-19 上午)在这里就收手了,真机验收 16 轮零报错,
> 功能却是废的——助手内容里反复出现空的 `Search results for query:` 头,
> 模型自检认为工具不可用,当场回滚。教训:**验一条补偿能否退役,要验功能,
> 不能只验报错**。当天傍晚的隔离探测把"功能废"拆成了三个可修的窄毛病
> (下两节),才完成第二次、真正的退役。
>
> 旧实现仍保留在 git 历史（提交 `1bdbbbd` 之前）作为证据，不是当前恢复方式的源码副本。

### 3b. 搜索轮的噪声头(响应侧剥离)

上游对声明了原生 web_search 的轮次,会在搜索前注入一个独立 text 块,内容恒为
`Search results for query: <查询词>`,与紧随其后的 `server_tool_use.input.query`
完全重复;K2.7 还会在 turn 末尾再发一个**悬挂噪声头**(宣布下一次搜索却直接
`end_turn`,后面没有答案)——这就是第一次退役时看到的"冒号后面是空的"与
"回答为空"。三次隔离探测三发三中,形态确定。

补偿:规则 `provider.kimi.search-noise-text-strip`
(`desktop/gateway/src/kimi_search_noise.rs`),流式与非流式都把该 text 块整块剥掉。
流式实现按块缓冲到凑满前缀即拍板;被吞的块不占输出索引(Science 要求 index
连续无空洞);**未命中的流量字节级零改写**。真实 K3/K2.7 会话中每个搜索轮均命中
(日志 `noise=1`),剥后回答开头即正文。

### 3c. 幻影空搜索对(响应侧剥离 + 请求侧修复)

模型被告知"无需搜索"时,上游仍常发一对空壳:`server_tool_use` **连 id 都没有**、
无 input,紧跟 `content: []` 的 `web_search_tool_result`(同样无 tool_use_id)。
真实会话与探测中,不搜索的轮次几乎每轮都带这一对。

危害有两层:Science 把它渲染成一个空的 Server Tool 框;落盘后只剩无 id 的孤儿
结果块,**此后每一轮都 400 `tool_call_id is not found`**(真实会话第 3 轮当场复现,
Science 侧表现为 Agent Failed + 中断,这正是配对修复表里"无 id 孤儿"一行的来源)。

补偿:响应侧规则 `provider.kimi.empty-search-pair-strip` 只把"前置 `name=web_search`、
两半都无键、结果 `content` 字段存在且精确为数组 `[]`"的整对剥掉；缺失/string/object/null
不按空结果删除。请求侧
`web-search-result-pairing-repair` 的无 id 分支负责救活修复落地前已中毒的历史
(实测同一会话 Resume 后 400 → 200)。**带键的真实零结果搜索不剥离**;
两半键不一致时由 §3d 的采钥规则归一后保留。

> 另:搜索轮的下一轮里,Science 会把上一轮的搜索结果存盘瘦身,并在 user 消息里
> 附一段 `[System] Prior-turn server_tool(…) — results persisted.` 说明,UI 渲染成
> 一个输出为空的 Server Tool 折叠框。**这是 Science 平台自己的历史压缩行为**,
> 与渠道无关(官方渠道同样出现),CSSwitch 不改写它。

#### 历史 native 真机验收(2026-08-19,内置浏览器驱动真实 Science + 真实订阅 key)

同一 K3 会话连续 6 轮:原生搜索(组件渲染、来源齐全、开头无噪声)→ 凭历史追问
(配对修复命中)→ 中毒历史 Resume 后再搜索(400 → 200)→ 无需搜索轮(幻影对
剥离命中、落盘干净)→ 搜索历史 + python 工具循环(3 次 LLM 调用零失败,这正是
当初 502 死循环的形态)→ 搜索 + 工具混合轮。K2.7 新起一轮原生搜索,正常回答。
全程 `upstream_failure` 仅出现在修复落地前的中毒历史上。

#### 统一 bridge 真机验收(2026-08-20)

- K3：Elixir 搜索 14 results → 不联网追问 → 重载，PASS；搜索 2 calls，追问 1 call。
- K3-256k：Vite 搜索 17 results → 不联网追问 → 重载，PASS；搜索约 145s、追问约 156s，
  均在共享 180s deadline 内。高延迟是用户选择统一 bridge 的明确代价。
- K2.7：Lua 搜索 13 results → 不联网追问 → 重载，PASS；搜索 2 calls。
- 三者 query/card/text 均正确，追问 `bridged=0`，无 pairing repair / 400 / upstream failure。

### 3d. 搜索对配对键采钥(响应侧归一)

上面已经出现过两次的事实:上游自己发出的真实搜索对,两半配对键**恒不匹配**
(`server_tool_use.id` 为 `tool_…`,`web_search_tool_result.tool_use_id` 为
`srvtoolu_…`)。原样放行时,Science 落盘会丢弃无法配对的 `server_tool_use`,
只留孤儿结果块——**流式期间可见的 Web Search 卡片(含查询词)在流结束后消失**,
且此后每一轮请求都要靠请求侧配对修复以 `input: {}` 空壳兜底(2026-08-19
真机复现,live 证据 D5)。

补偿:规则 `provider.kimi.search-pair-id-adopt`,响应侧在放行真实搜索对之前把
`server_tool_use.id` 改写为同对结果块的 `tool_use_id`——**只采用上游已有的
`srvtoolu_…` 键,不发明新键**;result 侧无键而 use 侧有键时反向补齐。
两半都无键的非空对不归一,如实放行、仅记日志(`unkeyed=N`)。带键的空结果
被判定为真实零结果搜索,走采钥保留(这同时收窄了 §3c 幻影剥离的判据:
幻影 = 两半都无键 **且** 内容为空)。流式与非流式同一矩阵,采钥不改变块数与
索引,命中记日志 `adopted=N`。

归一之后 Science 可正常配对、落盘、渲染,查询词保留;§3 的请求侧配对修复
对**新产生的**轮次不再必要,继续保留用于救活历史存量。

### 3e. Science 尾随机器上下文重排(请求侧)

真实 Science 会把用户问题与本机 `compute snapshot` 放成末尾相邻的两条 user message,
机器上下文在后。direct K3 A/B 保持正文不变只交换两条顺序时,原顺序让主 query planner 选择机器规格,
反向顺序稳定选择用户的 Rust 主题,因此问题位于 Kimi 主 query planner 对消息顺序的解释。

规则 `provider.kimi.science-context-tail-reorder` 只在 Kimi 请求声明 typed web_search、
末尾两条都是 user、两条均为 text-only,且最后一条大小写无关命中
`compute snapshot`、独立词 `cores` 与独立词 `RAM` 或带数字的 GiB 容量规格时,
交换两条完整消息。它不删除或重建正文,也不改写 query；普通连续 user、历史工具结果、
client tool 与兄弟 provider 均保持原样。真机候选在同一 K3 会话完成 Rust 搜索、
历史复述、Python 搜索、跨搜索不联网比较与两轮连续计算；两张 query/card 和最终结果
整页重载后均保留。该重排现在发生在 typed search 映射为私有 query tool 之前；bridge 搜索轮
与响应整形正常叠加。

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
