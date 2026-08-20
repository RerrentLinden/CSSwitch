# Backend Quality Guidelines

## 场景：判定上游是否真的不支持某能力

### 1. 范围 / 触发条件

上游返回无信息量的错误（如 `Invalid request Error`）时适用。仅凭"我们发的格式可能不对"就改造请求，会掩盖真实的能力缺口；仅凭"另一家返回 200"就断定对方支持，同样不成立。

### 2. 签名

```text
控制组 A：同一请求形状的全部 source 变体      → 是否同样失败
控制组 B：一个**不存在的**块/字段类型          → 报文是否与被测特性一致
控制组 C：同一请求发往兄弟 provider            → 是否为本渠道独有
控制组 D：能否读到内容（不是能否被接受）        → 用唯一标记做可证伪提问
```

### 3. 合同

- 报文相同 ≠ 原因相同；必须用控制组 B 把"未实现"与"格式不符"分开。
- HTTP 200 ≠ 内容被使用。用带唯一标记的载荷提问，并要求"读不到就回固定串"，才能区分**接受**与**真的读到**。
- 结论写入 capability 规则的 `reason` 时，必须同时写明控制组，否则后人无法判断该结论的强度。

### 4. 校验与错误矩阵

| 观察 | 可下的结论 | 不可下的结论 |
| --- | --- | --- |
| 所有 source 变体同样失败 | 与 source 形态无关 | 上游不支持该能力 |
| 不存在的类型报同一错 | 走的是"未知类型"分支 | —— |
| 兄弟 provider 200 | 非通用规范问题 | 兄弟 provider 支持该能力 |
| 兄弟 provider 带/不带载荷回答一致 | 它并未读取该载荷 | —— |

### 5. 正常 / 基线 / 错误案例

- 正常：四组控制齐备后判定"上游未实现 `document` 块"，并据此选择降级方式。
- 基线：只测一种 source 形态就改造请求格式，反复试错。
- 错误：因兄弟 provider 返回 200 就宣称"这是我们的格式问题"，把能力缺口误报成 bug。

### 6. 必需测试

- 结论必须落成 capability 规则并附 `evidence` 指向研究记录；规则的 `tests` 字段列出锁住该行为的单测。
- 降级行为需有单测断言"兄弟渠道不受影响"（如 `other_relay_contracts_keep_their_document_blocks`）。

### 7. Wrong vs Correct

#### Wrong

```text
上游 400 → 猜测是格式问题 → 反复调整请求形状 → 偶然绕过 → 记为"已修复"
```

#### Correct

```text
上游 400 → 跑四组控制 → 定性为"未实现" → 选择显式降级 → 写入 capability 规则并附控制组证据
```

---

## 场景：向 Anthropic SSE 流注入内容块

### 1. 范围 / 触发条件

在 gateway 内合成或改写 `content_block_*` 帧时适用（如补发上游调用后把结果拼进同一条消息）。

### 2. 签名

```text
crate::anthropic_sse::Validator          校验输出流生命周期，message_stop 扣留至干净 EOF
StreamFilter::feed(&[u8]) -> Result<Vec<u8>, String>
render_block(index: u64, block: &Value) -> Vec<u8>
```

### 3. 合同

- **注入必须发生在过滤流内部。** validator 夹在 filter 与 socket 之间；在转发循环结束后追加帧，会被判为流被截断并抛 `upstream SSE protocol error`。需要发起上游调用的过滤器应自持 `GatewayConfig` / transport 的克隆，内联完成。
- **凡上游按增量下发的字段，都必须发 delta。** 规范客户端从 `content_block_start` 取块壳、再累积 delta；只放在 start 帧里的内容会被丢弃。受影响字段：`text`→`text_delta`、`thinking`→`thinking_delta`、`signature`→`signature_delta`、`server_tool_use.input`→`input_json_delta`。`web_search_tool_result` 无 delta 形式，整块放在 start 帧是正确的。
- **content block index 必须连续无空洞。** 被吞掉的块不占用输出索引；注入块从当前输出索引继续。
- 替换上游终止帧时只替换 `message_delta`，`message_stop` 仍用上游的，以保持 validator 的生命周期判定。

### 4. 校验与错误矩阵

| 条件 | 结果 |
| --- | --- |
| 转发循环结束后追加帧 | `SSE stream ended before message_stop` → 终止 error |
| 只在 start 帧携带 payload | 客户端还原为空块；**且会在下一轮被回传给上游**，可能触发上游 400 |
| 输出索引出现空洞 | validator 判定 block index 非法 |
| 注入所需的上游调用失败 | 必须发终止 SSE error，不得伪造助手内容 |

### 5. 正常 / 基线 / 错误案例

- 正常：过滤器内联发起补发调用，注入块按 delta 下发，替换 `message_delta` 后放行上游 `message_stop`。
- 基线：不注入、原样透传。
- 错误：整块内容塞进 `content_block_start` 就以为完成——本地看着对，客户端拿到的是空块。

### 6. 必需测试

- 用"模拟规范客户端"的还原函数做往返断言：从 start 取壳并清空流式字段，仅靠 delta 累积，断言还原结果与原块**完全相等**。
- 断言注入块在 `message_delta` 之前、`message_stop` 之后不出现，且索引连续。
- 断言上游调用失败时产出终止 error 而非内容。

### 7. Wrong vs Correct

#### Wrong

```rust
// payload 只在 start 帧；规范客户端还原出 input:{} 与空 thinking，
// 并把这些残缺块回传给上游
out.extend(render_sse(Some("content_block_start"),
    &json!({"index": i, "content_block": block})));
out.extend(render_sse(Some("content_block_stop"), &json!({"index": i})));
```

#### Correct

```rust
let mut shell = block.clone();
shell["input"] = json!({});                      // start 只给块壳
out.extend(block_start(i, shell));
out.extend(block_delta(i, json!({                // 内容走 delta
    "type": "input_json_delta",
    "partial_json": serde_json::to_string(&block["input"])?
})));
out.extend(render_sse(Some("content_block_stop"), &json!({"index": i})));
```

---

## 场景：退役 provider 专属补偿 / 合并两个渠道

### 0. 先决条件：判据是功能，不是报错

动手删之前先确认**该不该删**。**"不报错"不等于"能用"**——退役判据必须是
**该功能在真实会话里正常工作**，而不是"没有 4xx/5xx"。

2026-08-19 的真实教训：Kimi 的 web_search 客户端工具桥，逐条验证了它当初被写下来的
两个理由——429 不再复现（32 次请求全 200）、历史配对 400 可用一条窄规则修好
（400 → 200）——于是删掉 920 行改为原生透传。真机跑到 16 轮对话、**零 upstream_failure**，
看起来完全成功。

实际上功能是废的：上游向助手内容注入空的 `Search results for query:` 头、返回与查询
无关的内容，模型因此判定"web_search 工具在当前会话中不可用"，转去写 Python 调学术 API，
整轮任务被带偏。已 `git revert` 整笔回滚。

还有一条:一条补偿的**注释里写的理由，可能只是它最初被写下来的动机，不是它的全部价值**。
退役前先问：除了这条记录在案的缺陷，它是否还顺带绕开了别的东西？上例中桥接的写法是
"不向上游声明服务端工具、自己发补发请求拿结果"，这个形状顺带绕开了上游整条搜索通道的
质量问题——而文档里只记了 429。

| 条件 | 要求行为 |
| --- | --- |
| 退役后的真机验收 | 必须由**人**确认功能输出正确，不能只看网关日志 |
| 只验证了"无报错" | 不足以退役，继续保留 |
| 验收会话 | 必须是**全新会话**——旧会话的历史里可能已积累坏结构，会把"新代码有没有问题"和"旧数据有没有问题"混在一起 |
| 退役失败 | `git revert` 整笔，并把失败原因与**正确的退役判据**写进渠道文档 |

确认该删之后，再按下面的步骤删干净。

### 1. 范围 / 触发条件

某个渠道积累了一批只服务它的补偿，之后判定这些补偿不必要、或两个渠道应当行为一致时适用。这是「接入一个新的 provider 渠道」的逆过程，但注册点更分散——补偿会沿着**传输层、请求改写层、运行期状态、capability 目录、测试夹具**五条线扩散，而其中传输层那条最不显眼。

### 2. 先做的事：穷举「专属」的真实边界

不要凭 contract 定义判断两个渠道差在哪里。用 contract id 反查全部分支：

```bash
grep -rn "<contract-id>" desktop/gateway/src/ desktop/src-tauri/src/ catalog/
```

2026-08-07 合并两个 Kimi 渠道时，contract 定义只差 `thinking_policy` 一个字段，但实际专属行为有六项，其中四项藏在 `messages.rs` 的传输层且都由同一个 `is_<channel>()` 谓词驱动：强制 `?beta=true` 查询参数、强制注入 `anthropic-beta` 标识、把 User-Agent 改写成 `claude-cli/*` 并丢弃入站 UA、强制 `x-app: cli`。只看 contract 会把合并误判为「改一个字段」。

### 3. 合同

- **传输层身份伪装属于渠道专属项，不属于兼容补偿**。它们决定上游是否放行，去掉之后失败形态是「请求被拒」而不是「能力降级」，必须在渠道文档中单列并标注未验证状态。
- 补偿退役后，仅服务它的**运行期状态机制**一并退役，但只删本渠道那一项：`contract_requires_reasoning_continuity`（gateway/config.rs）与 `contract_requires_reasoning_state`（src-tauri/runtime/proxy_lifecycle.rs）是两个独立函数，必须同改；`RestorePolicy` 的对应变体与其分支随之删除。
- 落盘的运行期状态（thinking 续写存储）**不主动清理**：网关无权代用户删数据目录内容，残留文件不影响新逻辑。
- 一个元数据布尔量常常同时门控多件事。退役前先确认它到底门控了什么：`AnthropicMetadata.kimi_compatibility` 同时门控了续写存储**和**响应侧 server-tool 块过滤，直接删会静默关掉后者。响应过滤应改挂到语义谓词（`flavor.filters_server_tool_blocks()`）上。
- 合并契约时，`capabilities.v1.json` 的 `match.provider_contract` 是**标量精确匹配**（`capability_catalog.rs::text_match`）。合并成单一 contract 后一条规则即可覆盖两个 template，无需扩展匹配器；若坚持保留两个 contract id，则必须先让该字段支持数组。
- 归档任务后，capability 规则 `evidence` 里指向 `.trellis/tasks/<task>/research/` 的路径会失效。合并/改名规则时同批修正为 `.trellis/tasks/archive/<YYYY-MM>/<task>/research/`，并逐条核对文件真实存在——死链的 evidence 等于没有 evidence。

### 4. 校验与错误矩阵

| 遗漏点 | 报错 |
| --- | --- |
| 只删 contract、未删 tauri `expected_ids` | `provider contract catalog 缺少必需 contract 或含未知 contract` |
| 未更新 contract 总数断言 | `assertion left == right` (12 vs 13) |
| 规则改名后 Rust 常量与测试内的字符串字面量不同步 | 断言 `rule_ids.contains(...)` 失败，而常量本身编译通过 |
| 删除枚举变体后留下单元素 `for` 循环 | `clippy::single_element_loop`，rust 层 fail |
| 传输层测试仍断言旧的注入行为 | loopback 层单点 fail，掩盖在 118 个用例里 |

### 5. 正常 / 基线 / 错误案例

- 正常：先 grep 穷举专属分支，一次性改完五条线，`run_all.sh` 五层全绿。
- 基线：删补偿后 rust 层过、loopback 层 fail —— 传输层与端到端断言是两处独立的旧行为副本。
- 错误：为了让单元素循环通过 clippy 而把同一条目复制成两份凑数，得到的是伪造的覆盖率。

### 6. 必需测试

- 断言两个 template 派生出**同一个** contract id 与同一组策略取值。
- 同一请求体经两个端点改写后逐字段相等（合并的直接证据）。
- 传输层断言从「注入了什么」翻转为「不再注入什么」，并显式断言入站头被透传。
- 兄弟渠道（DeepSeek）的既有断言**零改动**；若某测试同时覆盖两者，拆开而不是放宽。

### 7. Wrong vs Correct

复用旧策略的通用存储测试时，不要因为策略变了就删掉整组测试——指纹、防篡改、容量回滚这些性质与策略无关。

#### Wrong

```rust
// 策略枚举里 KimiAll 没了，就把这些测试一起删掉，
// 连带丢失指纹 / 防篡改 / 容量回滚的覆盖。
```

#### Correct

```rust
// 改挂到仍然存在的策略上；DeepSeekToolUse 要求 visible content 带工具绑定，
// 因此把响应内容从纯 text 换成 tool_use 即可，断言本身不动。
let response = complete_response(
    json!([
        {"type": "thinking", "thinking": "capacity-plan", "signature": "sig"},
        {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {"code": "1"}}
    ]),
    "tool_use",
);
store.capture_message(&first_request(), &response, "k3", RestorePolicy::DeepSeekToolUse)
```

---


---

## 场景：接管用户自己的官方 Claude Science 实例

### 1. 范围 / 触发条件

改动 Science daemon 启动、停止、状态判定或配置存储时适用。CSSwitch 接管的是
**用户自己的实例、自己的登录、自己的对话库**，不做数据隔离。这里每一条被违反，
损害的都是用户的真实数据。

### 2. 签名

```text
claude-science serve --detached --no-browser      # 全部启动参数,不多不少
  env: ANTHROPIC_BASE_URL=http://127.0.0.1:<port> # 唯一注入的变量
  env_remove: ANTHROPIC_MODEL / *_MODEL / *_MODEL_NAME
              ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN
claude-science stop | status | url
配置: ~/.csswitch/service.v1.json (0600)
```

### 3. 合同

| 禁止传的参数 | 后果 |
| --- | --- |
| `--data-dir` / `--config` | 造出隔离实例,用户对话从此分家 |
| `--no-auto-update` | 官方实例失去自动更新（用户明确红线） |
| `--port` | 端口随机化,控制台地址每次重启都变；用它自己的默认端口 |

- 模型类与凭证类环境变量必须清除：残留会让 Science 绕过网关的模型目录，或带着旧凭证启动。
- 不写、不读、不伪造任何 auth 状态。
- `status` 子命令**总是退出 0**，必须解析输出判断是否在跑，不能看退出码。
- 切换供应商必须重启 daemon（base_url 只在启动时读取）；重启会打断进行中的会话，
  Science 侧显示 `This session was interrupted by a restart`，因此有活跃会话时
  必须先要确认（`daemon.active_conversations > 0` 且未传 `force` → 拒绝）。

### 4. 校验与错误矩阵

| 条件 | 要求行为 |
| --- | --- |
| 找不到 claude-science | 明确报错并列出已查找路径；`CLAUDE_SCIENCE_BIN` 无效时 fail closed，不回退其它候选 |
| daemon 未运行时 stop | 视为成功（stderr 含 not running / no daemon） |
| 有活跃会话且未确认 | 拒绝切换并报告会话数，不静默打断 |
| 渠道配置不完整 | 切换前就拒绝，不要等推理时才 400 |

### 5. 正常 / 基线 / 错误案例

- 正常：官方模式下 Science 设置页 About 显示自动更新可用，历史项目与会话完整。
- 基线：切换模式后拿到新登录链接，旧会话仍在列表里。
- 错误：为"隔离干净"传 `--data-dir`——用户打开后发现对话全没了。

### 6. 必需测试

- `science::tests::model_env_clear_list_covers_auth_and_role_overrides`：断言清除列表
  覆盖模型与凭证变量，且 **`ANTHROPIC_BASE_URL` 不在其中**（它是要注入的）。
- loopback 层断言配置文件权限为 `0600`、且未写出 `~/.csswitch/config.json`。

### 7. Wrong vs Correct

#### Wrong

```rust
// 为了"隔离"与"稳定"，把用户的实例改造成另一个实例
&["serve", "--port", "0", "--detached", "--no-browser",
  "--data-dir", &managed_dir, "--no-auto-update"]
```

#### Correct

```rust
// 只注入路由，其余一律沿用用户手动启动时的行为
&["serve", "--detached", "--no-browser"]
// env: ANTHROPIC_BASE_URL=<gateway>
```

---

## 场景：与既有应用共用配置目录

### 1. 范围 / 触发条件

新组件要在 `~/.csswitch/` 之类的既有目录下落配置时适用。

### 2. 合同

**配置文件名不得与既有应用重名**。旧桌面端拥有 `~/.csswitch/config.json`，
它会把任何不认识的 schema 当成自己的**旧版本**去迁移——实测结果是它把自身的
provider 列表迁移成了空数组，用户配置当场丢失（已从备份恢复）。

新服务用 `service.v1.json`。文件名里带版本号，为将来的 schema 变更留出并存空间。

### 3. 校验与错误矩阵

| 条件 | 要求行为 |
| --- | --- |
| 配置文件不存在或解析失败 | 回落到默认值，**不猜测、不尝试迁移**别人的 schema |
| 写入后 | 权限收紧到 0600 |

### 4. 必需测试

loopback 层断言：保存配置后 `~/.csswitch/config.json` **不存在**（沙箱 HOME 内）。
这条断言是为防止重蹈覆辙而存在的，删除它等于删掉护栏。

---

## 场景：为某个 provider 写补偿规则

### 1. 范围 / 触发条件

新增或修改 provider 专属的请求改写时适用。补偿的本质是"上游拒收某个组合，
我们改成它能接受的形状"，因此**判据必须由实测边界决定**。

### 2. 合同：判据来自实测，不来自他人实现

从其它项目移植语义时，**必须独立复测边界**再落判据。本轮的真实教训：

| 照搬的判据 | 实测边界 | 代价 |
| --- | --- | --- |
| 任何非 `none` 的 `tool_choice` 都禁思考 | 只有 `{"type":"tool"}` 会 400；`auto` / `any` 完全正常 | `auto` 是带工具时最普通的形态——等于每轮都白白关掉推理 |

复测方法见「判定上游是否真的不支持某能力」。最小成本是对上游直接发几个变体，
一次对比就能定出边界。

### 3. 合同：策略来自 provider contract，不来自环境变量

`thinking_policy` 等策略字段的权威来源是 `catalog/provider-contracts.v1.json`，
由 `GatewayConfig` 从契约读取。环境变量只能作为**显式 override 并留日志**。

曾经只读环境变量：契约里明明声明了策略，任何忘记注入该变量的启动路径都会
**静默丢掉全部补偿**，表现为上游 400——而配置文件看起来完全正确。

### 4. 合同：规则日志只记净效果

规则 ID 列表的用途是回答"哪条补偿正在起效"。**被后续规则覆盖、净效果为零的
改写不得记入**，否则排查时会以为请求是以那个中间形态发出去的。

具体做法是让优先级高的规则先判并短路：指定工具型 `tool_choice` 会把 thinking
直接定为 `disabled`，此时 thinking 取值的归一化不再执行也不记录。

### 5. 正常 / 基线 / 错误案例

- 正常：`thinking enabled + budget + effort` 上游能接受 → 原样透传，`rules=-`。
- 基线：只有会被拒收的组合被改写，日志里每条规则都确有净效果。
- 错误：因为"另一家这么写"就扩大判据，在能过的形态上也做补偿。

### 6. 必需测试

- 每条补偿至少一条单测断言「拒收形态 → 归一化后形态」。
- 每条**收窄过**的判据要有一条反向测试，断言边界外的形态**不被触碰**
  （如 `only_the_specified_form_costs_thinking`）。
- 规则覆盖场景要断言被覆盖的规则**不出现**在 rule_ids 里
  （如 `specified_tool_choice_supersedes_the_auto_rewrite`）。
- 契约接线要有一条不依赖任何 env 的测试（`thinking_policy_comes_from_the_contract_without_any_env`）。

### 7. Wrong vs Correct

#### Wrong

```rust
// 照搬来源实现的宽判据,未复测边界
match body.get("tool_choice") {
    Some(Value::Object(obj)) => obj.get("type") != Some("none"),  // auto 也中招
    ...
}
```

#### Correct

```rust
// 判据 == 实测出的拒收边界
body.get("tool_choice").and_then(Value::as_object)
    .and_then(|c| c.get("type")).and_then(Value::as_str) == Some("tool")
```

---

## 场景：通过 query-tool bridge 执行 Kimi Web Search

### 1. 范围 / 触发条件

`kimi-anthropic-relay` 收到 typed `web_search_*` 时适用。K3、K3-256k、K2.7 对 inline
server search 的 planner/answer 行为不一致；Science 主请求统一先把能力映射为私有 ordinary query tool，
只有模型实际请求搜索时才发 nested server search。

### 2. 签名

```text
kimi_web_search_adapter::prepare_request(&mut Value, RelayFlavor)
kimi_web_search_adapter::resolve_with(main, prepared, first, upstream_call)
kimi_web_search_adapter::render_stream(&Value) -> Result<Vec<u8>, String>
messages::inference_deadline(&GatewayConfig) -> Instant
messages::post_nonstream_before(..., deadline)
RULE_PROVIDER_KIMI_WEB_SEARCH_QUERY_TOOL_ADAPTER =
  "provider.kimi.web-search.query-tool-adapter"
```

### 3. 合同

- 主调用保留完整 Science system/messages/compute/client tools；只把唯一 typed Web Search 声明换成
  内部 ordinary `{query:string}` tool，**其名字就是 `web_search`**——模型思考自然提到工具名时
  从构造上无泄漏（2026-08-20 真机 2/2 复现过改名前的 thinking 泄漏；清洗式方案被否）。
  同请求存在任何其它 `name=web_search` 工具（裸名 / `type:"custom"` / 异类 typed）时显式 400，
  守卫按 name 匹配、不看 type。未调用它时只有一次上游请求，普通回答/工具原样返回。
- 调用内部 tool 时，query 必须非空、唯一 ID、只含 `query` 字段，最多四次且 query 去重不能绕过调用数上限。
- nested 只含原 typed Web Search，**对任何 Kimi 模型无条件**使用指定型 `tool_choice` 与显式
  `thinking: disabled`（禁止模型名白名单——2026-08-20 review 抓到 fail-open 白名单让 catalog
  出厂 id 静默不强制）；`max_tokens=min(main,4096)`。它必须返回 1..4 组唯一 `name=web_search` pair。
- nested 有正文时两调用完成；只有真实 pair 时才做 synthesis。synthesis 使用 exact main tool IDs 的
  `tool_result`、保留其它 Science tools、移除内部 tool 防递归，随后必须经
  `degrade_missing_tool_choice`（防 `{"type":"any"}` 等 forced choice 被继承进已改动的 tools），
  `max_tokens=min(main,8192)`。
- nested 无正文且 main 含可见 client tool_use 时**跳过 synthesis 直接 merge**，
  以 `stop_reason=tool_use` 把搜索证据与未决工具一并交回 Science 续轮；不得 fail closed
  （2026-08-20 修复：旧硬错把可服务的混合轮变确定性 502，且与 merge 的显式支持自相矛盾）。
- query 与网页 evidence 都是 **untrusted data**：以 JSON/tool_result 数据传递，回答指令不得拼接或执行其中指令。
  evidence 总量最多 512 KiB，该上限**同时作用于 synthesis 与 nested-has-text 直通合并两条路径**，
  超限失败，不截断；上限检查先于任何全量物化。
- K3 在完整 pair 后偶发尾随 client `tool_use(name=web_search)`；仅当它是最后一个块且
  `stop_reason=tool_use` 时按 type/name 剥离并记数（不逐字段校验将被丢弃的块），
  安全性由随后的单次 nested 校验兜底。任何其它位置/工具 fail closed。
- main/nested/synthesis 共享一次 contract total deadline；每阶段不得重置 180 秒。无 retry、command fallback
  或伪造正文。usage 数值字段跨阶段 checked-add（溢出显式失败）；同键**类型**跨阶段不一致时取最后阶段值
  并记 log_line 告警，不得为元数据差异丢弃整轮已完成的回答。
- 对 Science 只输出一个 Anthropic lifecycle；merged 可见内容不得含任何 client
  `tool_use(name=web_search)` 块（server_tool_use 除外），内部查询调用必须全部被消费。
  流式路径进入即写 200 头 + `: bridge processing` SSE 注释帧，main/nested 完成后各一条阶段注释帧
  （SSE 注释协议合法、事件解析器忽略）；写头后的失败以流内 `event: error` 呈现。
  渲染后的 SSE 以 64 KiB 分块 feed 校验器（只看 Err，不做 round-trip 字节比对）。
- 原生 typed 路径保留作为 nested executor；nested 响应继续经过 noise/phantom/adoption。
  非 adapter 的 Kimi 响应路径**无条件**建 `SearchNoiseFilter`（零命中字节级原样）——
  门控谓词一律从**变换前**的请求状态推导，禁止读被 `prepare_request` 改写后的 body
  （2026-08-20 review：后置谓词恒为 false，整条过滤主路径成了死代码而测试仍绿）。
- adapter 日志行携带 `merged_shape=<块类型序列>` 与 `pair_key_prefix=<键前缀段>`
  诊断投影（只记形状与前缀，不记查询词或 id 全文），是排查 Science 落盘行为的一手数据。

### 4. 校验与错误矩阵

| 条件 | 行为 |
| --- | --- |
| 无 typed search / 兄弟 provider | bridge 不触发，原样路径 |
| 私有 tool 名冲突、多个 typed search、非法 query/ID | 明确 400/502，不覆盖用户工具 |
| nested 429/5xx/超时/非 JSON/缺 pair/异名 pair | 保留 stage 与状态，显式失败，不 retry |
| nested 有 pair + text | 2 calls，合并真实 pair/text |
| nested 有 pair、无 text、无未决普通 tool | bounded synthesis，第 3 call 生成正文/普通工具 |
| nested 无 text 且 main 有未决普通 tool | 跳过 synthesis，merge 后 `stop_reason=tool_use` 交回 Science 续轮 |
| evidence > 512 KiB（两条路径同判）/ usage 数值溢出 | 502，零截断、零假成功 |
| usage 同键类型跨阶段不一致 | 取最后阶段值 + log_line 告警，不 502 |
| shared deadline 过期 | 504，下一阶段联网前失败 |

### 5. 正常 / 基线 / 错误案例

- 正常：Science K2.7 搜索 → query tool → forced nested → 原生卡片 + 正文；下一轮不搜索
  `bridged=0/upstream_calls=1`。
- 基线：Kimi 普通轮即使声明 typed search，也由模型决定不调用私有 tool，零 nested 开销。
- 错误：每轮强制 server search；拿完整用户 prompt 当 query；nested 失败后改走 Bash；把网页正文当指令；
  或对下游发送两套 message lifecycle。

### 6. 必需测试

- 三模型 fake upstream：搜索严格 2/3 POST、不搜索 1 POST；nested forced/disabled、token caps、shared deadline。
- query 数量/字段/唯一 ID、pair/result 1..4 唯一性、512 KiB、usage checked-add、stage-specific 429/协议错误。
- JSON 与规范 SSE：merged 可见内容零 client `tool_use(name=web_search)`、查询调用全消费、
  真实 pair/input delta、普通 tool 的 `stop_reason=tool_use`、单 `message_start/message_stop`、
  阶段注释帧先于 `message_start`、大合并流可渲染而超大单帧仍显式失败。
- K3 尾随 client search 正例与位置/name/id/input/无 pair 负例。
- 真机必须在同一最终构建上分别跑 K3、K3-256k、K2.7：搜索 → 不联网追问 → 重载；
  核对 query/card/text、`bridged`、calls、无 400/repair/upstream failure。direct/provider 与 Science 证据分栏。

### 7. Wrong vs Correct

#### Wrong

```text
Science 每轮声明 web_search → 每轮直接强制搜索 → 非搜索轮多余调用/延迟/失败
```

#### Correct

```text
主模型通过私有 query tool 决定是否搜索 → nested 只执行真实 server search
→ 有正文直接返回；无正文才用 exact tool_result synthesis → Science 单一生命周期
```

> **Warning（已知边界，2026-08-20）**：Science 的历史规整会在会话每推进一轮后,
> 把**非最新** assistant 消息裁剪为纯 text（thinking 与 server 搜索对被追溯剥离,
> `frame_messages` 可证）。桥接路径的搜索卡片因此只在「自己还是最新轮」期间可见;
> 原生 typed 流式路径的卡片则由 Science 的持久转录层跨轮保留（2026-08-19 R6
> 六轮验收可证）。两者在所有可黑盒检视的线上维度（块序、`srvtoolu` 键前缀、
> usage 透传、shell 空 input + delta 单发）等形,Science 持久层的选收依据未知。
> 诊断靠 adapter 日志的 `merged_shape` / `pair_key_prefix`;要根治需原生 vs 桥接
> SSE 字节级抓包对差（原生流式已不对 Science 服务,需临时构建）。
> 判「卡片消失」缺陷前,先确认是否只是这一语义:同轮/最新轮可见 + 正文永久保留 = 非缺陷。
> 另注意由此派生的测试陷阱:验收「重载后卡片仍在」必须在**发过追问之后**再重载,
> 只在最新轮重载会得到假阳性（2026-08-19 的 K3/K2.7 分化即此成因）。

---

## 场景：重排 Science 尾随机器上下文以保住 Kimi 搜索意图

### 1. 范围 / 触发条件

真实 Science 在用户问题后追加独立 `role=user` 的 compute snapshot；Kimi 主调用的
query planner 会把最后一条 user 当搜索主题时适用。direct A/B 必须只交换两条消息顺序，
以排除响应过滤器、system 内容与工具声明等变量。

### 2. 签名

```text
reorder_science_context_before_user_intent(&mut Value, &mut Vec<String>)
RULE_PROVIDER_KIMI_SCIENCE_CONTEXT_TAIL_REORDER =
  "provider.kimi.science-context-tail-reorder"
```

### 3. 合同

- 仅 Kimi + typed `web_search_*` 生效；发生在 typed→私有 query tool 之前；DeepSeek 与 Generic 零改写。
- 只看末尾相邻两条 user message，且两条都必须是 string 或全 text blocks。前一条含
  `tool_result` 等非 text 块时不得交换，避免拆断 assistant/tool_result 历史配对。
- 末条大小写无关包含 `compute snapshot`，并同时含独立词 `cores` 与独立词 `RAM`，
  或含带数字容量的 `GiB`（`32GiB` / `32 GiB`）。裸 `GiB` 与 `hardcores` 不命中。
- 交换完整 `Value`，不得删除、重建或改写正文，更不得在响应后伪造 query。
- 只有实际交换才记录规则 ID；所有边界外形态 `rule_ids` 不得出现该规则。

### 4. 校验与错误矩阵

| 条件 | 行为 |
| --- | --- |
| typed search + text intent + 精确 compute tail | 交换末两条，记录规则 |
| 单条 compute 问题 / 普通双 user | 原样 |
| `program` / `hardcores` / 裸 `GiB` | 原样 |
| 前条或末条含非 text block | 原样 |
| client tool / 兄弟 provider | 原样 |

### 5. 正常 / 基线 / 错误案例

- 正常：`[Rust 问题, compute snapshot]` → `[compute snapshot, Rust 问题]`，两条内容逐值相等；
  真机首张卡片直接搜索 Rust。
- 基线：普通连续 user message 保持顺序，规则不记入日志。
- 错误：只因出现硬件词就全局移动上下文，或把 compute context 插进
  `assistant tool_use → user tool_result` 中间。

### 6. 必需测试

- 用真实 Science 脱敏形状断言顺序、正文与规则 ID。
- 反向覆盖：单消息、普通双 user、大小写与单词边界、裸/带数字 GiB、非 text、
  历史工具结果、client tool、DeepSeek、Generic。
- 真机全新会话至少覆盖两次不同主题搜索、历史复述、不联网轮与整页重载；同时确认
  query 语义正确、`search-pair-id-adopt adopted=1`，且无 repair / 400 / upstream failure。

### 7. Wrong vs Correct

#### Wrong

```text
响应卡片主题错 → 把 query 文本改成用户问题 → 结果仍来自错误搜索，只是 UI 看似正确
```

#### Correct

```text
direct A/B 证明尾部消息顺序敏感 → 仅在精确 Science 签名下交换完整消息 → 上游首次生成正确 query
```

---

## 场景：归一 Kimi 原生 web_search 响应配对

### 1. 范围 / 触发条件

Kimi 响应出现相邻的 `server_tool_use` + `web_search_tool_result` 时适用。Science
按两块的配对键决定是否保留搜索卡片；键不一致会让搜索框和查询词在流结束后消失，
并使后续请求依赖历史配对修复。

### 2. 签名

```text
SearchNoiseFilter::feed(&[u8]) -> Result<Vec<u8>, String>  # 流式
strip_nonstream_noise(&mut Value) -> StripStats            # 非流式
StripStats::{any_activity, rewrote_body}
RULE_PROVIDER_KIMI_SEARCH_PAIR_ID_ADOPT =
  "provider.kimi.search-pair-id-adopt"
```

### 3. 合同

- `result.tool_use_id=K` 存在且与 `use.id` 不同时，只把 `use.id` 改为上游已有的 `K`；
  不改 `input.query`、结果内容或块索引。
- 只有前置块 `type=server_tool_use,name=web_search` 才能参与采钥/幻影判定；其它 server tool 原样。
- 只有 `use.id` 存在时，反向补到 `result.tool_use_id`。
- 两半都无键且 result 的 `content` **存在、为数组并显式 `[]`** 才是幻影对；字段缺失、null、string、
  object 等 schema 异常不得按空数组删除。带键的空结果是合法零结果搜索，必须保留。
- 两半都无键但结果非空时不得发明键：原样放行并计 `unkeyed_pairs`。
- `adopted_pairs > 0` 时非流式 body 必须重序列化；`unkeyed_pairs` 单独出现时是零改写，
  `rules=-`，但日志仍记 `unkeyed=N`。
- 真机验收既要确认卡片在整页重载后仍在，也要人工确认查询与结果语义相关；
  “无 400 / 有 Web Search 卡片”不足以证明搜索功能正确。

### 4. 校验与错误矩阵

| use.id | result.tool_use_id | result.content | 行为 |
| --- | --- | --- | --- |
| 无或不等于 `K` | `K` | 任意 | `use.id := K`，`adopted_pairs += 1` |
| `U` | 无 | 任意 | `result.tool_use_id := U`，`adopted_pairs += 1` |
| 无 | 无 | 显式数组 `[]` | 剥离整对，`pair_blocks += 2` |
| 无 | 无 | 缺失/非数组 | 原样或显式协议失败，不当幻影 |
| 无 | 无 | 非空 | 原样放行，`unkeyed_pairs += 1` |
| `K` | `K` | 任意 | 字节级原样放行，零命中 |

### 5. 正常 / 基线 / 错误案例

- 正常：`tool_*` / `srvtoolu_*` 不匹配时采用结果侧键，流结束和整页重载后查询词仍可见，
  下一轮不再触发 `web-search-result-pairing-repair`。
- 基线：已经配对的搜索对保持原字节，兄弟 provider 不进入过滤器。
- 错误：为无键非空对生成新 ID，或只在流式路径修复，导致非流式仍丢卡片。

### 6. 必需测试

- 流式与非流式都覆盖：双键不匹配、反向采钥、带键空结果、无键非空、已配对零改写、
  malformed content 与非 Web Search server tool 负例。
- 断言采钥不改变块索引、查询输入与结果内容；非流式 `adopted_pairs > 0` 后 body 被重序列化。
- 断言规则日志包含 `search-pair-id-adopt` 与 `adopted=N`；只有无钥对时 `rules=-`
  且包含 `unkeyed=N`。
- 真机用全新会话跑“显式搜索 → 追问 → 不搜索 → 再追问”，然后整页重载；
  同时检查无 400 / `upstream_failure`、无历史 pairing repair，并人工核对查询语义。

### 7. Wrong vs Correct

#### Wrong

```text
看到相邻搜索对 → 无条件生成统一的新 ID → 卡片看似恢复，但伪造了上游身份且掩盖无键异常
```

#### Correct

```text
优先采用结果侧现有键；仅单侧有键时向另一侧补齐；两侧无键则不发明身份
```

---

## Test Gate: 三层门禁合同（2026-08-19）

`bash test/run_all.sh` 是唯一入口，三层全绿才算通过：

| 层 | 内容 | 失败含义 |
| --- | --- | --- |
| 1 static | `cargo fmt --check` + clippy（本仓库代码零告警） | 传递依赖的 future-incompat 提示不计入 |
| 2 unit | `cargo test` | 补偿链、配置、模型目录、控制面 |
| 3 loopback | 起真实服务进程打真实 HTTP | 路由、显式 404、配置权限、凭证不回显、配置文件名隔离 |

- 三层全部离线。真机验收（官方登录、真实 provider key、工具轮次）在
  `test/LIVE_ACCEPTANCE.md`，结论必须单独写，不能并入门禁结果。
- Rust 工具链发现必须走 `test/_cargo_path.sh`（本机没有 rustup shim）；
  找不到时以 env-blocked 退出，**不得计为通过**。
- loopback 层必须用隔离 HOME，不得读写用户真实的 `~/.csswitch`。
