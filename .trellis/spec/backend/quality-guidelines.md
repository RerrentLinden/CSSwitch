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
science::start(base_url, &LaunchOverrides)
claude-science serve [--config <镜像>] --port 8686 --sandbox-port 8687 --detached --no-browser
                                                  # 全部启动参数,不多不少
  env: ANTHROPIC_BASE_URL=http://127.0.0.1:<port> # 唯一注入的变量
  env_remove: ANTHROPIC_MODEL / *_MODEL / *_MODEL_NAME
              ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN
claude-science stop | status | url
science_config::LaunchOverrides { web_search: bool, default_model: Option<String>, skip_approvals: bool }
science_config::launch_config(&overrides, source, mirror) -> Result<Option<PathBuf>, String>
  source(只读): ~/.claude-science/config.toml
  mirror(0600): ~/.csswitch/science-config/official.toml
control::launch_overrides(profile, instance) -> LaunchOverrides   # 五个启动点唯一的取值处
配置: ~/.csswitch/service.v1.json (0600)
```

### 3. 合同

| 参数 | 规则 | 理由 |
| --- | --- | --- |
| `--data-dir` | 永远禁止 | 造出隔离实例,用户对话从此分家 |
| `--no-auto-update` | 永远禁止 | 官方实例失去自动更新（用户明确红线） |
| `--config` | 只指向 CSSwitch 生成的镜像；只在「联网搜索关闭」或「这个实例处于第三方模式」时出现 | 它只换配置文件、不换数据目录（2026-10-10 用 0.1.60 隔离实测）；官方模式 + 联网搜索开启时不带它，启动参数与没有这些功能时逐项相同 |
| `--dangerously-skip-approvals` | 只在「跳过审批」总开关开着时追加在末尾(两个实例同一个值,`science::SKIP_APPROVALS_FLAG` 只定义一次) | 用户明确要的 bypass 模式;关着时参数逐项不变。不进镜像、不影响 `needs_mirror()` |
| `--port` / `--sandbox-port` | 固定 8686 / 8687 | 不用 `0`（控制台地址要稳定可收藏），也不沿用 Science 默认的 8000 / 8001（常被本机其它开发服务占用）；两个端口都显式给出，不依赖「port+1」的推导 |

- 模型类与凭证类环境变量必须清除：残留会让 Science 绕过网关的模型目录，或带着旧凭证启动。
- 不写、不读、不伪造任何 auth 状态。
- `status` 子命令**总是退出 0**，必须解析输出判断是否在跑，不能看退出码。
- 切换供应商必须重启 daemon（base_url 只在启动时读取）；重启会打断进行中的会话，
  Science 侧显示 `This session was interrupted by a restart`，因此有活跃会话时
  必须先要确认（`daemon.active_conversations > 0` 且未传 `force` → 拒绝）。
- **启动覆盖项**（`LaunchOverrides`）由调用方传给 `start`，模块内部不读配置文件。镜像里至多写两个
  **根表**键，别的一概不写；`--config` 是否出现由 `launch_config` 的返回值唯一决定：

  | `web_search` | `default_model` | 结果 |
  | --- | --- | --- |
  | true | None | `Ok(None)`：不生成、不带参数，并尽力删除残留镜像 |
  | false | None | 镜像 = 源 + `enable_web_search = false` |
  | true | Some(id) | 镜像 = 源 + `default_model = "<id>"` |
  | false | Some(id) | 镜像 = 源 + 两个键 |

  - `web_search` 是联网搜索总开关（`Profile.web_search`，全局、默认开）。开启时不写这个键：
    Science 自己配置里的任何值（包括手写的 `enable_web_search = false`）原样生效，CSSwitch 不替用户放宽。
  - `default_model` 只在实例处于第三方模式时有值：该渠道清单里承载默认槽的那一条的 id
    （`StaticProfileResolver::default_menu_id`，见「生成模型清单」场景）。官方模式下必须是
    `None` —— 那是用户自己的设置；沙箱总是第三方模式。渠道默认槽为空取不到 id 时也是 `None`，不因此拦截启动。
  - 取值只有 `control::launch_overrides(profile, instance)` 一处，`switch_mode`、`start_science`、
    `set_web_search`（含存盘前的镜像预生成）、`start_sandbox`、`switch_sandbox_mode` 都用它。
    在某个启动点另写一份判断，两个实例就会拿到不一致的默认模型。
  - 镜像 = 源文件全部内容 + 覆盖的键（原本有就覆盖，没有就新增），用 `toml_edit` 读写；
    源不存在时镜像只有覆盖的键。每次启动都重新生成。
  - 对真实 `~/.claude-science/` 只读 `config.toml` 一个文件，不写、不建任何东西。
  - 写出走同目录临时文件 → `0600` → rename，目录 `0700`：镜像带着用户原配置的全部内容。
- 切换总开关（`POST /control/web-search`）的顺序固定：判定（值没变 → 不动；官方实例在运行且
  有活跃会话且未带 `force` → 拒绝）→ 关闭时先试生成镜像 → 存盘并更新运行时快照 → 官方实例
  在运行就 stop → start。拒绝与镜像失败都在存盘**之前**，开关不会停在「已保存但没生效」；
  重启失败**不回滚**（回滚再重启可能循环），原文报出当前真实状态。免登录沙箱不自动重启，
  只在响应里置 `sandbox_restart_required`。
- 旧的渠道级 `kimi.web_search` 只在解析入口迁移一次（`Profile::parse`）：顶层没有 `web_search`
  且 `kimi.web_search == false` → 总开关为关；顶层字段一旦存在就以它为准。`POST /control/config`
  收到 `web_search` 显式报错并指向新端点，不静默忽略。

### 4. 校验与错误矩阵

| 条件 | 要求行为 |
| --- | --- |
| 找不到 claude-science | 明确报错并列出已查找路径；`CLAUDE_SCIENCE_BIN` 无效时 fail closed，不回退其它候选 |
| daemon 未运行时 stop | 视为成功（stderr 含 not running / no daemon） |
| 退出服务（`/control/quit`） | 先并行停掉**正在运行的**官方实例与沙箱再退进程；未安装 / 未运行不算失败，沙箱未运行时不碰其目录；停止失败不拦截退出，原文随响应 `errors` 返回 |
| 有活跃会话且未确认 | 拒绝切换（模式、联网搜索总开关同一套规矩）并报告会话数，不静默打断；总开关被拒绝时配置与 Science 都不变 |
| 渠道配置不完整 | 切换前就拒绝，不要等推理时才 400 |
| 源 `config.toml` 读不了或解析不了（需要镜像时：联网搜索关闭，或第三方模式） | 报错并拒绝启动 / 拒绝切换开关，不写镜像、不回退成空配置；报错给路径、行列与解析器原话，**不带出错行的内容**（可能是带口令的代理地址）。报错里按实际的覆盖项说明为什么需要这个文件，不笼统地只提联网搜索。第三方模式下**开启**联网搜索同样要预生成镜像，所以同样会被拒 |
| 源 `config.toml` 不存在（需要镜像时） | 正常：镜像只有覆盖的键，不替用户创建源文件 |
| 关闭开关后 Science 起不来 | 原文报出；官方模式下把开关拨回开启再启动，就是不带 `--config` 的原始启动方式 |
| 切回「官方模式 + 联网搜索开启」而 Science 没在运行 | `switch_mode` 顺手删掉残留镜像（否则它会留到下次启动，看起来像仍在生效） |

### 5. 正常 / 基线 / 错误案例

- 正常：官方模式下 Science 设置页 About 显示自动更新可用，历史项目与会话完整。
- 基线：切换模式后拿到新登录链接，旧会话仍在列表里。
- 错误：为"隔离干净"传 `--data-dir`——用户打开后发现对话全没了。
- 错误：为了关闭联网搜索去改写 `~/.claude-science/config.toml`——坏文件会留在 Science 自己的
  目录里，不经 CSSwitch 也起不来；开启时还要删键，可能覆盖别人手写的 `false`。

### 6. 必需测试

- `science::tests::model_env_clear_list_covers_auth_and_role_overrides`：断言清除列表
  覆盖模型与凭证变量，且 **`ANTHROPIC_BASE_URL` 不在其中**（它是要注入的）。
- `science::tests::official_serve_args_pin_ports_and_never_isolate`：无镜像时参数向量与字面量
  逐项相等；有镜像时 `--config <镜像>` 恰好一次、去掉这一对后与无镜像时相同；`--data-dir`、
  `--no-auto-update` 两种状态下都不出现。这条测试是官方模式的冻结线，改启动方式时不改它。
- `science_config::tests`：断言一律**用 TOML 解析器把镜像读回来**比较，不拿实现里的字符串当预期。
  覆盖：上表四格（无覆盖时不写文件并清残留）；无源文件；含根键 / 注释 / 表 / 行内表 / 多行字符串 /
  表数组的源（除覆盖的键外逐值相等、键在根表）；源里原本就有 `enable_web_search = true` 或自己的
  `default_model`（被覆盖，不得出现重复键）；源无法解析（报错、不写镜像、报错不含文件内容）；
  源读不了不等于空配置；源目录零写入；权限 `0600` / `0700`。
- `control::tests`：切换判定纯函数的四条分支；请求体的 `enabled` / `force` 必须是布尔；
  `launch_overrides` 对两个实例各取各的模式（正式实例官方、沙箱走渠道 → 正式实例 `None`、沙箱
  `claude-sonnet-19`；把沙箱写成取 `profile.mode`，或把正式实例写成取 `sandbox_mode`，这条会红）。
- loopback 层断言配置文件权限为 `0600`、且未写出 `~/.csswitch/config.json`。
- loopback 层用假 `CLAUDE_SCIENCE_BIN` 与命令行同形的假沙箱进程，断言退出服务停掉两个实例且进程退出；
  loopback 全程不得解析到本机真实 `claude-science`。
- loopback 层的启动覆盖项：官方模式 + 开启态 `serve` 整行等于主线；官方模式 + 关闭态整行带
  `--config <镜像>`，镜像在根表含 `enable_web_search` 并保留临时 HOME 里源配置的其它键，源目录不变；
  官方模式下重新开启后整行回到主线、镜像被删。第三方模式下整行总是带 `--config`，镜像含
  `default_model`，开关联网搜索只增减 `enable_web_search`；切回官方模式后镜像被删。
  活跃会话未确认、源配置无法解析两种情况下配置文件逐字节不变且没有 stop / serve。
  假 `claude-science` 的 `status` 输出形状必须与真实一致
  （`{"running":true,"daemon":{"active_conversations":N}}`），否则测的是假设。

### 7. Wrong vs Correct

#### Wrong

```rust
// 为了"隔离"与"稳定"，把用户的实例改造成另一个实例
&["serve", "--port", "0", "--detached", "--no-browser",
  "--data-dir", &managed_dir, "--no-auto-update"]
```

#### Correct

```rust
// 只在启动时注入，其余一律沿用用户自己的实例
&["serve", "--port", "8686", "--sandbox-port", "8687", "--detached", "--no-browser"]
// env: ANTHROPIC_BASE_URL=<gateway>
// 联网搜索关闭、或实例处于第三方模式时,在 serve 之后多一对:"--config", <CSSwitch 生成的镜像>
// 跳过审批总开关开着时,末尾多一个:"--dangerously-skip-approvals"
```

---

## 场景：运行免登录沙箱 Science 实例

### 1. 范围 / 触发条件

改动 `sandbox.rs` / `sandbox_forge.rs`（隔离实例的铸造、起停、钥匙串）时适用。
沙箱是 CSSwitch 自有隔离目录里的第二个 Science 实例：全部写入落在
`~/.csswitch/science-sandbox/`，与「接管用户官方实例」场景的规则互不豁免——
真实实例路径（`science.rs`）一行都不因沙箱而改变行为。

### 2. 签名

```text
sandbox::start(base_url, &LaunchOverrides) → forge(幂等三态) → SSH/gh 桥接 → 沙箱钥匙串 → 端口检查
  → 镜像配置 → daemon → 健康 → 钥匙串校验
claude-science serve --data-dir <沙箱> --config <镜像> --host 127.0.0.1 --port 8790 --sandbox-port 8791
  --no-browser --no-auto-update --detached
  镜像: source = <沙箱 data-dir>/config.toml(只读)  mirror = ~/.csswitch/science-config/sandbox.toml
  env: HOME=<沙箱 home>  ANTHROPIC_BASE_URL=<网关>/sandbox  https_proxy=<网关>(不带前缀;CONNECT 到 Anthropic / Claude 主机 404,其余直连隧道)
       DO_NOT_TRACK=1(关 Science 的使用统计与错误上报)
       PATH=<沙箱>/bin:<继承>  GIT_CONFIG_COUNT/KEY_n/VALUE_n(本机装了 gh 时)
sandbox_forge::ensure_virtual_login(auth_dir, email, sandbox_root)  # 三道护栏先行

配置: service.v1.json 顶层 sandbox_mode = "kimi" | "deepseek"      # 与正式实例的 mode 互相独立
路由: /sandbox/v1/messages | /sandbox/v1/models → Instance::Sandbox  # 前缀在路由处剥掉
POST /control/sandbox/switch {"mode": "kimi"|"deepseek", "force"?: bool}
  → {"ok", "mode", "mode_label", "changed", "sandbox_restarted"}
GET  /control/status → 顶层 sandbox_mode / sandbox_mode_label
```

### 3. 合同

- **真实 `~/.claude-science` 铁律**：写入根解析后落在真实目录之内/本身即拒绝，
  先于一切写入，先于「隔离根内」检查（防隔离根本身是符号链接指向真实树）。
- 假账号 email 必须 `localhost.invalid` 结尾；绝不从可解密 token 借 org 身份；
  `orgs/` 多于一个候选直接报错，不静默选择。
- 全部写入：拒符号链接 + `O_EXCL` 临时文件 + rename + `0600`；删除/列举旧 `.enc`
  失败必须显式失败（Science 预期 `.oauth-tokens/` 恰好一个 `.enc`）。
- 沙箱钥匙串用随机密码（空密码在当前 macOS 解锁失败），密码文件落盘即 0600；
  forge 修复/铸新后先清沙箱钥匙串旧条目（0.1.48 以 Keychain 为权威，不清会被
  回写吃掉修复形成死循环）；启动后正向校验密钥确在沙箱钥匙串内，不过即停 daemon。
- 固定端口 8790/8791，占用即报错，永不自动改绑；全部监听回环。
- 启动覆盖项与官方实例走同一机制（`science_config::launch_config`，合同见上一场景）。沙箱总是
  第三方模式，`default_model` 总有值，所以实际启动时**总是**带一对 `--config <镜像>`（渠道默认槽为空的
  配置进不了启动流程）；数据目录仍由 `--data-dir` 钉死。镜像在钥匙串清理**之后**生成：forge 已重写登录而旧钥匙串条目还没清就中途
  退出，下次启动会判成 Reused 跳过清理。沙箱已在运行时 `start` 原样返回、不重启，开关的新值
  要等用户重启沙箱才到它那一侧（网关一侧的摘除不依赖它）。
- 对外入口链接的主机名固定 `127.0.0.1`，`localhost` 留给真实实例：浏览器 cookie 只按
  主机名划作用域、不看端口，而 Science 的 `operon_auth` / `operon_csrf` 都是 host-only，
  两实例同主机时后登录的一方会顶掉另一方。daemon 的 Host 闸门与写操作 Origin 闸门
  本就同时放行这两个主机名，换主机名不得给 daemon 加任何参数；改沙箱一侧而非真实
  实例（真实实例的 claude.ai OAuth 回调固定归一到 `localhost`）。
- 兜底停止只杀「pid 来自沙箱 `operon.lock` + 进程存活 + argv[0] 是 claude-science
  + 含 serve + `--data-dir` 逐 token 精确等于沙箱目录」的进程。
- 刻意跨出隔离边界的动作只有两个桥接，都**不得**把真实登录钥匙串挂进沙箱搜索表
  （真实实例的 Science 加密密钥也在里面，挂进去沙箱 daemon 就可能读到它）：
  - SSH 桥接：只 symlink 真实 `~/.ssh` 的 `config` 与 `known_hosts` 两个非秘密文件进
    沙箱 HOME（Compute 按 `$HOME/.ssh/config` 找主机别名），私钥一律不链接（认证走继承的
    `SSH_AUTH_SOCK` / 绝对 `IdentityFile`），不修改源、目标已存在（含悬空链接）绝不覆盖。
  - gh 桥接：Science 的 Credentials 按 `gh auth token` 与 `git credential fill` 探测本机
    GitHub 登录，而 gh 令牌默认在真实登录钥匙串里，只链接 `~/.config/gh` 拿不到。做法是
    在 `<沙箱>/bin/gh` 写一个包装脚本（`HOME=<真实 home> exec <本机 gh> "$@"`，每次启动
    重写，本机没装 gh 时删除），放在 daemon PATH 最前面，只让 gh 进程用真实 HOME；
    git 访问 `https://github.com` / `https://gist.github.com` 时经 `GIT_CONFIG_*` 环境先写
    空值清掉系统级 osxkeychain（沙箱 HOME 下只查沙箱钥匙串），再指向
    `gh auth git-credential`，不写沙箱 `~/.gitconfig`。继承环境已有 `GIT_CONFIG_COUNT`
    时不覆盖；继承的 PATH 不可用时不改 PATH（只剩包装目录会让 daemon 找不到系统命令）。
    启动日志记 `gh_bridge=on|off`。
- token/key/钥匙串密码不进日志、不进控制面响应；`security` 命令输出全丢弃。
- 沙箱不向 Anthropic 上报：`daemon_env` 带 `DO_NOT_TRACK=1`（Science 官方开关，关使用统计与
  错误上报，只在 daemon 启动时读）。快速失败代理只拦 Anthropic / Claude 域名
  （`connect::is_blocked_host`），错误上报的主机不在其中，靠代理拦不住，所以从源头关。
  取值不能是空串、`0`、`false`：Science 0.1.60 的判定函数把这三个当作没设（静态检查）。
  只加在沙箱一侧——真实实例的遥测设置是用户自己的，`science.rs` 的环境不得带它。
- **沙箱的渠道与正式实例各选各的**（`Profile.sandbox_mode` 对 `Profile.mode`）：
  - 网关靠沙箱 base_url 的路径前缀 `/sandbox` 认出沙箱的请求。前缀只定义一次
    （`sandbox::INFERENCE_PREFIX`，沙箱环境与路由共用），只加在 `ANTHROPIC_BASE_URL` 上，
    两个代理变量不加。正式实例的 base_url 不带路径，`science.rs` 不因此改动。依据是
    Science 0.1.60 内置 SDK 以 `baseURL + path` 直接拼接（静态检查；2026-10-10 真机实测一致，
    记录见 `test/LIVE_ACCEPTANCE.md` §G）。
  - `sandbox_mode` 永不为官方：沙箱是虚拟登录，官方直通对它只会 401。两个入口归一——
    读配置（`Profile::parse`：键缺失或值为 `official` → 推导：`mode` 是渠道就取它，否则取
    第一个配置完整的渠道，都不完整取 Kimi；推导值不在读入时回写）与切换（拒绝并说明原因）。
    `handle_inference` 的官方直通只对「正式实例 + 官方模式」开放，沙箱一侧一律走
    `GatewayConfig::for_channel(profile, mode)`，它对官方模式返回错误。
  - 切换沙箱渠道的顺序固定，与联网搜索总开关同一套：解析 → 目标渠道 `validate()` →
    纯函数判定 → 存盘并更新快照 → `stop` → `start`。一切拒绝都在存盘**之前**；重启失败
    **不回滚**，报错写明渠道已保存、停在了哪一步。
  - 沙箱只要在运行，切换就要先确认（未带 `force` → 拒绝）：路由按 `sandbox_mode` 每请求
    生效，只存不重启会让运行中的沙箱立刻被改道，而它手里还是旧渠道的模型目录。沙箱没有
    可靠的会话数（`status --data-dir` 的输出形状没有实测记录），所以不区分有没有会话。
  - 启动沙箱只看 `sandbox_mode` 对应的渠道是否完整，与正式实例的模式无关。
  - 状态接口里新增的键不得叫 `mode`：`serde_json` 开了 `preserve_order`，
    `test/run_loopback.sh` 用贪婪正则取顶层 `"mode":"…"`，更靠后的同名键会被它取走。
  - 请求日志条目带 `instance`（`official` / `sandbox`）；路径一栏是剥掉前缀之后的。

### 4. 校验与错误矩阵

| 条件 | 要求行为 |
| --- | --- |
| 沙箱渠道配置不完整时点启动 | 启动前拒绝，报错点名渠道与缺项；不看正式实例的模式 |
| 把沙箱渠道切成官方 | 拒绝并说明原因（虚拟登录，官方接口会拒绝它），不改任何状态 |
| 切到配置不完整的渠道 | 拒绝，不改任何状态 |
| 沙箱在运行、未带 `force` | 拒绝（文案含「沙箱正在运行」，控制台据此转确认框），配置与沙箱都不变 |
| 切换后停止或启动沙箱失败 | 不回滚；原文报出，并写明渠道已保存、沙箱停在哪一步 |
| 沙箱入口之外的 `/sandbox…` 路径 | 显式 404，不进推理层（`/sandbox`、`/sandbox/`、`/sandboxx/…`、叠两层前缀） |
| 健康检查超时 / 钥匙串校验失败 | 先停 daemon 再报错，不带病运行 |
| CLI 报 not running 但 lock pid 仍活 | 兜底 SIGTERM 仍要收口，不漏杀 |
| blob 被判 free / 过期 / 结构损坏 | 不得复用，修复回 max 并沿用原 org |
| 契约漂移（自更新后格式变更） | 写后解密回读自校验失败 → 整体报错，fail-closed 不静默兜底 |

### 5. 必需测试

- 护栏负例断言**零写入**（真实目录逐字节不变、符号链接目标不建目录）。
- 幂等三态：Reused 断言 `.enc` 与 `encryption.key` 逐字节不变；Repaired 断言 org 保留。
- 兜底停止的命令行匹配：正例命中、pid 复用/真实实例/前缀相似目录/瞬时 CLI 四组负例拒绝。
- 启动参数：不带镜像时与字面量逐项相等；带镜像时 `--config <镜像>` 恰好一次、去掉后与不带时相同，
  且带着它的命令行仍被兜底停止的匹配认出（沙箱实际启动时总是带着它，认不出就停不掉）。
- 入口链接主机名改写：localhost → 127.0.0.1、带路径前缀、已是 127.0.0.1 时幂等、
  非法 URL 原样返回，四例都钉住；并断言沙箱主机名不等于 `localhost`。
- gh 桥接：包装脚本行为级校验（假 gh 在沙箱 HOME 下被执行时看到的是真实 HOME、参数原样
  透传、临时文件已改名）；查找本机 gh 时跳过沙箱 bin 与不可执行文件；环境注入的 PATH 前置
  与 git 凭据「先清空再指向 gh」两对都钉住；已有 `GIT_CONFIG_COUNT` / PATH 缺失两个负例。
- 独立渠道：
  - 路由函数：四条推理路径（含 query，前缀被剥、query 保留）与近似路径
    （`/sandbox`、`/sandbox/`、`/sandbox/v1/unknown`、`/sandboxx/…`、叠两层前缀、`//sandbox/…`）。
  - `daemon_env`：base_url 带前缀，两个代理变量不带；带 `DO_NOT_TRACK`，且取值不是空串 /
    `0` / `false`（这三个 Science 当作没设）。
  - 切换判定的四条分支，拒绝文案含「沙箱正在运行」；请求解析拒绝 `official`、未知模式、
    非布尔 `force`。
  - 启动前置检查要抽成函数来测（`sandbox_start_precheck`）：正式实例为官方而沙箱渠道完整 →
    通过；沙箱渠道缺 Key → 报错点名渠道。检查内联在处理函数里时，把它改回看 `profile.mode`
    没有任何测试会红（变异检查实测）。
  - `Profile::parse` 的推导四例、显式值优先、手改成 `official` 被归一、存盘读回。
  - `for_channel(profile, mode)` 取传入的模式，不读 `profile.mode`。
  - loopback：两个入口的请求日志与本地合成的模型清单分别来自各自渠道；正式实例切到官方后
    沙箱入口仍走渠道（这时不得向正式入口发推理请求，那会连到真实官方接口）；切换被拒后配置
    逐字节不变；切换沙箱渠道不触发正式实例的 stop / serve；正式实例收到的 base_url 始终不带
    路径。带 `force` 的重启路径不进 loopback：真实启动要调 `security`、占固定端口。

### 6. Wrong vs Correct

#### Wrong

```rust
// 沙箱跟着全局模式走,与正式实例共用一个入口
// env: ANTHROPIC_BASE_URL=http://127.0.0.1:8788
let cfg = GatewayConfig::for_channel(&profile)?; // 内部读 profile.mode

// 切换时先存盘,再判断要不要确认:被拒绝时路由已经改了
profile.sandbox_mode = mode;
profile.save()?;
if running && !force {
    return Err(sandbox_running_refusal());
}
```

#### Correct

```rust
// env: ANTHROPIC_BASE_URL=http://127.0.0.1:8788/sandbox(代理变量仍是不带路径的网关地址)
let mode = profile.mode_for(instance); // 沙箱 → sandbox_mode
let cfg = GatewayConfig::for_channel(&profile, mode)?; // 官方直通只给「正式实例 + 官方模式」

// 判定是纯函数,拒绝发生在存盘之前
match plan_sandbox_switch(&profile.sandbox_mode, &mode, running, force) {
    SandboxSwitch::NeedsConfirmation => return Err(sandbox_running_refusal()),
    // ...
}
profile.sandbox_mode = mode;
profile.save()?;
```

---

## 场景：生成模型清单

### 1. 范围 / 触发条件

改动 `static_profile.rs` 的 `models_response` / `MENU_POSITIONS`，或 `/v1/models`（含 `/sandbox/v1/models`）
返回的任何字段时适用。第三方模式下 Science 的模型菜单与默认模型全部由这份清单决定；
清单与请求解析各说各话时，用户选的模型与实际到达的模型就不是同一个。

Science 一侧的依据（0.1.60，只读静态检查，出处见 `docs/ARCHITECTURE.md`「模型解析」）：

- 主菜单三个位置按 id 的写法分配：`claude-opus*` / `claude-sonnet*` / `claude-haiku*` 各取能被
  `^claude-(opus|sonnet|haiku)-N(-M)?(-YYYYMMDD)?$` 解析的最新一条；其余进 More models。
- 新会话的默认模型只取配置里的 `default_model`，不看它在不在清单里。
- 显示名是小写连字符写法（判据 `^[a-z][a-z0-9]*(-[a-z0-9]+)+$`）或以 emoji 开头的条目，被它整条从菜单里
  丢掉（2026-10-10 真机确认 `k3-256k`）。

### 2. 签名

```text
static_profile::MENU_POSITIONS = [                        # (家族名 = 角色, id)；顺序 = 去重的先后，默认槽在前
    ("sonnet", "claude-sonnet-19"),
    ("opus",   "claude-opus-19"),
    ("haiku",  "claude-haiku-19") ]
StaticProfileResolver::models_response() -> Value
StaticProfileResolver::default_menu_id() -> &str          # 用户配置生成的目录下恒为 claude-sonnet-19
models::science_safe_display_name(id, candidate) -> String
StaticProfileResolver::resolve(requested)                 # 不因本场景而改
```

### 3. 合同

- **清单的组成**：按 `MENU_POSITIONS` 的顺序（sonnet → opus → haiku，即默认槽 → 高质量槽 → 快速槽），
  每个位置取它**解析到**的 route，已经列过的 route 不再列；其余没列过的 route（只可能来自 Fable 槽）
  以自己的 `claude-csswitch-*` selector 列出。官方 id（`claude-opus-5` 等）**不得**列出：列回去，
  菜单里同一个模型又是好几条。
- **默认条目恒在 sonnet 位**（`claude-sonnet-19`）：去重从默认槽开始，默认模型不会被别的槽挤到别的位置上。
  `default_menu_id` **不得**是名字里带 `opus` 的 id —— Science 0.1.60 对订阅档不够格的登录
  （免费档、部分 team / enterprise 席位）会把这样的默认模型换成 `claude-sonnet-4-6`，那个 id 不在清单里
  （静态检查所得；免登录沙箱是虚拟的 max 档，不受影响，所以这条只在正式实例的真实登录上才暴露）。
  Science 显示菜单时自己按家族排序，清单里的先后不决定显示顺序。
- **清单语义 = 解析语义**：位置 id 不另建别名表，经 `official_role_alias` 按家族名落到角色绑定；
  `models_response` 也经同一条解析路径取 route。清单里显示的模型名，必须属于 `resolve(id)` 返回的
  那个上游模型。
- **仍可解析的旧 id**：官方 id 与旧的 `claude-csswitch-<slug>` 不在清单里，但 `resolve` 的结果不变
  （旧会话，以及 Science 固定用官方 id 发起的辅助调用）。
- **位置 id 的版本号**（`19`，三个位置用同一个）必须满足三条，换之前逐条确认：`official_role_alias`
  返回对应角色；能被 Science 的主菜单正则解析（1–2 位数字）；不是任何真实模型的名字。
- **同一个模型出现在多个显式槽里**时只有一条 route，显示名取 `默认 → 高质量 → 快速 → Fable` 顺序里
  第一个槽的。
- **显示名**：清单里的名字一律过 `models::science_safe_display_name`。槽的显示名留空时回落到模型 ID，
  而 `deepseek-v4-pro` 这种写法会让 Science 丢掉整条 —— 默认模型若被丢掉，新会话就没有默认模型。
- **保存不重启**：`save_config` 不重启 Science（会打断会话）。保存结果的 `note` 固定是一句
  「已保存,重启 Science 后生效。」，不按改了什么另拼说明（界面文案从简，见前端规范）。
- **不新增配置键**：`ModelSlot` 只有 `model_id` 与 `display_name`，0.10.0 写出的 `service.v1.json`
  读写一轮后逐字节不变。

### 4. 校验与错误矩阵

| 条件 | 要求行为 |
| --- | --- |
| 高质量 / 快速槽留空，或与前面的槽是同一个模型 | 那个位置不列；带着那个位置的 id 来的请求仍按角色绑定解析 |
| Fable 槽的模型与前三个都不同 | 以 `claude-csswitch-<slug>` 列出（进 More models）；与其中一个相同则不列 |
| 请求带官方 id 或旧的 `claude-csswitch-<slug>` | 照常解析到对应槽位 |
| 外部生成的目录里默认 route 不绑定任何主菜单角色 | 它以自己的 selector 列出并排最前，`default_menu_id` 就是这个 selector，不悄悄换成第一个主菜单位置 |
| 槽的显示名留空，或是小写连字符写法 | 清单里换成 Science 肯显示的写法；模型 id 不变 |
| 显示名是占位词 `default` | 换成上游模型名，再过一遍显示名改写 |

### 5. 正常 / 基线 / 错误案例

| 默认 / 高质量 / 快速 / Fable | 清单 | `default_menu_id` |
| --- | --- | --- |
| K3 / 空 / K2.8 / 空 | sonnet-19=K3，haiku-19=K2.8 | `claude-sonnet-19` |
| k3-256k / k3 / kimi-for-coding / 空（出厂的 Kimi） | sonnet-19=k3-256k，opus-19=k3，haiku-19=kimi-for-coding | `claude-sonnet-19` |
| A / B / C / D | sonnet-19=A，opus-19=B，haiku-19=C，`claude-csswitch-D`=D | `claude-sonnet-19` |
| A / A / A / 空 | sonnet-19=A | `claude-sonnet-19` |
| A / B / B / 空 | sonnet-19=A，opus-19=B | `claude-sonnet-19` |
| A / 空 / C / C | sonnet-19=A，haiku-19=C | `claude-sonnet-19` |
| A / 空 / 空 / D | sonnet-19=A，`claude-csswitch-D`=D | `claude-sonnet-19` |

- 错误：为了让 Science 的默认模型「找得到」，把官方 id 列回清单 —— 两个真实模型在菜单里出现七条。
- 错误：去重从高质量槽开始（opus → sonnet → haiku），想让主力模型显示在第一位 —— 高质量槽留空时
  默认条目变成 `claude-opus-19`，在免登录沙箱里一切正常，换成订阅档不够格的真实登录，Science 就把
  默认模型换成清单里没有的 id。

### 6. 必需测试

- `static_profile`：上表各种配置的清单与 `default_menu_id`（恒为 `claude-sonnet-19`）；
  清单里没有 Science 自己会用的那几个官方 id；
  **清单里每个 id 经 `resolve` 得到的上游模型等于该条目显示的模型**；两个显式槽共用一个模型而显示名不同时
  只列一次、用第一个槽的名字；官方 id 与旧 selector 仍可解析；外部目录的默认 route 不在主菜单位置时
  以自己的 selector 作默认条目；三个位置 id 是「家族名 + 同一个 1–2 位数字版本号」并按家族名解析到
  对应角色；显示名留空的槽仍出现在清单里。
- `profile`：0.10.0 写出的 `service.v1.json` 读写一轮逐字节不变。
- `control`：`launch_overrides` 的默认条目对两个实例各取各的渠道。
- `models`：小写连字符写法的名字被换成带空格的写法且换出来的不再是那种写法；没有连字符、带大写、
  带空格或点号、非 ASCII 的名字原样不动。
- loopback：第三方模式的清单含位置 id，不含 Science 自己会用的官方 id，也不把同一个模型用
  `claude-csswitch-*` 再列一遍；以清单里每个 id 为 model 的请求到达它显示的模型；以官方 id 或旧 selector
  为 model 的请求仍到达对应槽位，不认识的 id 不被解析；保存槽位不重启 Science、保存结果说明重启后生效；
  另配或清空高质量槽 / 快速槽并重启，镜像里的 `default_model` 都是 `claude-sonnet-19`。
- 这些测试只证明网关一侧。Science 的菜单规则变了，它们不会自己变红：Science 升级后要在真机上
  重跑 `test/LIVE_ACCEPTANCE.md` §H。

### 7. Wrong vs Correct

#### Wrong

```rust
// 清单与解析各走各的:清单读角色绑定,另给位置 id 建一张别名表去解析
data.push(json!({"id": "claude-opus-19", "display_name": quality_slot.label(), ..}));
aliases.insert("claude-opus-19", default_selector.clone()); // 两处一旦不同步,菜单显示 A、请求到 B
```

#### Correct

```rust
// 清单里的每一条,都是「这个 id 实际会解析到的那条 route」
for (_role, id) in MENU_POSITIONS {
    let Some(index) = self.resolved_index(id) else { continue };
    // 已列过的 route 跳过;显示名取自这条 route
}
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
- **联网搜索总开关关闭时，声明在补偿链之前摘除**（`strip_typed_web_search_tools`，规则
  `tool.relay.web-search-disabled-by-config`），不在链尾的 server tool policy 里摘：
  §「重排 Science 尾随机器上下文」等规则以「本轮声明了 typed search」为触发条件且跑在更早，
  链尾摘除会让它们先按"会搜索"改写一个根本不会搜索的轮次。链首摘除后整条链一致地落到
  「无 typed search → bridge 不触发，原样路径」这一既有分支，既有规则一条都不用改。
  开关是全局的（`GatewayConfig.web_search` 取 `Profile.web_search`，不分渠道）：摘除不按 flavor
  分支，DeepSeek 平时留给上游原生执行的搜索声明同样被摘。它是独立于 Science 一侧
  （`enable_web_search = false` 的镜像配置）的保证：Science 还没重启、沙箱还没重启、或
  Science 版本不认识该选项时，桥仍然是断开的；Science 一侧生效后请求里没有声明，规则不再命中。
  开关默认开，serde 默认值必须显式写 `true`——`bool` 原生默认是 `false`，存量配置没有该字段，
  用原生默认等于给所有老用户静默停掉该能力。关闭态不清洗历史工具块，
  「未声明该工具却回放其历史」是否被上游接受属未验证形状，交真机判定，不预先兜底。

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
| 联网搜索总开关关闭 | 链首摘声明 + 记规则，bridge 与 tail-reorder 均不触发，上游 1 call；无声明的轮次零改写；Kimi 与 DeepSeek 同判 |

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
- 联网搜索总开关：**从 `handle_messages` 打到 fake 上游**看真正发出去的报文，断言上游 tools 里既没有
  typed search 也没有 bridge 的私有查询工具（它同名 `web_search`，只断言"没有 typed"会漏掉桥仍触发）。
  只测摘除函数本身证明不了 `cfg` 里的开关被读到。DeepSeek 单独一条，并先用开启态确认同一请求
  确实会带着搜索声明出去（否则关闭态的断言不说明问题）。另需：链首摘除后 tail-reorder 不触发、
  无声明轮次逐字段原样；loopback 层在真实进程里断言关闭态日志含规则 id、不含 adapter，
  重新开启后同一请求不经重启即重新走桥。

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

## 场景：打包为安装版与启动器

### 1. 范围 / 触发条件

改动 `launcher.rs`、`service_log.rs`、`main.rs` 的入口判定、状态接口的 `version`,或
`packaging/` 下的构建脚本、Info.plist 模板、图标时适用。安装版是在 `./csswitch` 之外新增的
一种启动方式:服务本身(`serve`)两者共用,脚本形态的行为不因安装版而改变。

### 2. 签名

```text
main.rs::entry(args, app_launch) -> Launcher | Version | Serve | OfficialPassthrough | Gateway
launcher::run() -> ExitCode                       # 0 = 已请求打开控制台;1 = 起不来(已提示)
serve [--port N] [--log-file PATH]
service_log::open(path) -> io::Result<File>       # 追加、0600、路径是符号链接时拒绝
service_log::attach(path) -> Result<(), String>   # fd 1 / 2 接到文件 + 轮转线程
GET /control/status -> {"version": <CARGO_PKG_VERSION>, "mode": …}    # version 是第一个键
bash packaging/build_app.sh [--allow-dirty]
  -> dist/CSSwitch.app、dist/CSSwitch-<版本>-<架构>.dmg

env  CSSWITCH_APP_LAUNCH=1           只由 Info.plist 的 LSEnvironment 设置
     CSSWITCH_APP_HELPER_DIR=<目录>   只供隔离测试替换 open / osascript,生效时写日志
     CSSWITCH_LOOPBACK_BIN=<路径>     让 loopback 层去测指定的二进制(例如应用包里的那个)
```

### 3. 合同

- **应用包里只有一个可执行文件**,它就是网关二进制;启动器与服务是同一个文件的两个角色。
  不加 shell 脚本主可执行文件(签名存在扩展属性里,拷贝易丢)、不加 Swift 壳、不加托盘 /
  Dock 图标 / 原生窗口。界面只有浏览器里的控制台,退出只有控制台的「退出服务」。
- **启动器干完就退。** LaunchServices 以「应用主进程是否还活着」判断应用是否在运行;主进程
  若常驻,第二次双击不会再执行任何代码。提示框也不能无限期地挂住启动器(`giving up after`)。
- **入口靠显式的环境变量,不靠路径嗅探。** `CSSWITCH_APP_LAUNCH=1` 只在**没有参数**时把二进制
  变成启动器;显式子命令永远优先。启动器拉起服务时显式带 `serve` 并移除这个变量 —— 两道
  保险少了任何一道,服务都可能再次判成启动器、无限拉起自己。无参数且没有这个变量时必须仍是
  前台 `serve`(`./csswitch` 与 loopback 都依赖它)。
- **「服务已在跑」的判据**是状态接口回 200、JSON、带字符串 `mode`,不是「端口连得上」。
  别的程序占着端口时照常去拉起服务,由它撞上端口,把系统的原始报错带进提示框。
  探测走回环且显式不走代理(用户环境里的 `HTTP_PROXY` 会把 127.0.0.1 的请求带到代理上)。
  连接超时 1 秒;连上之后等回话 5 秒:状态接口要先执行 `claude-science status`,不是纯回环往返。
- **版本只有一个来源**(`Cargo.toml`)。启动器拿状态接口的 `version` 与自己比:不同、或没有
  这个字段(更早的服务),都先提示再打开控制台,**不自动重启** —— 退出服务会停掉 Science 与
  沙箱、打断进行中的会话,必须由用户决定。状态里嵌套的 `sandbox.version` 是 Science 的版本,
  不得拿来比。
- **登录 shell 环境按白名单导入**:`PATH`、`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` /
  `NO_PROXY` 及其小写形式、`CLAUDE_SCIENCE_BIN`。只解析标记之后的内容(rc 文件会打印横幅);
  shell 里有就覆盖继承值(置空也算有),没有就保留继承值。`ANTHROPIC_*` 等模型与凭证类变量
  永不导入。取不到(超时、非零退出、没有标记)是**显式降级**:写日志、沿用继承环境、照常启动。
  日志只记变量名,不记值(代理地址可能带口令)。shell 起在独立会话里,超时按进程组结束。
- **日志**:安装版落在 `~/.csswitch/service.log`(0600)。轮转在服务进程内做(每 60 秒按
  **正在写的那个文件**量大小,超过 5 MiB → 改名 `.1` → 0600 新建 → `dup2` 到 fd 1 / 2):
  fd 重定向之后外部改名不改变进程在写的文件;按路径量大小,文件被外部删掉后就永远「没超限」。
  `log_line!` / `request_log_line!` 不改;不带 `--log-file` 时服务行为与没有这个参数时相同。
- **提示与打开控制台**只用 `/usr/bin/open` 与 `/usr/bin/osascript`。提示的标题与正文经 argv
  传给脚本,不拼进 AppleScript 源码(正文带着日志的最后一行,内容不受控)。正常启动不弹任何东西。
- **构建脚本**只写 `dist/`,不碰 `/Applications`;工作区有任何未提交改动(含未跟踪文件 ——
  它们同样会被编进产物)时拒绝,除非显式 `--allow-dirty`;自检任一项不过即非零退出并删掉
  这次的半成品。它不并入 `test/run_all.sh`:三层门禁的合同是离线、快,release 构建与 DMG 是
  发布侧的独立结论。签名是 ad-hoc,bundle id `io.github.rerrentlinden.csswitch`
  (不沿用上游的 `com.csswitch.menubar`:不是同一个应用,不共享系统里按 bundle id 记的状态)。
- **图标**:`packaging/icon/` 里 `gen.py` → 三张 SVG → `build_icon.sh` → `AppIcon.icns`,
  全部入库;应用构建只拷贝 `.icns`,不依赖 Chrome。16 / 32 px 三档取自简化稿,其余取自主稿。
  控制台里的内联图形是 `console-glyph.svg` 的拷贝,改图后要同步。

### 4. 校验与错误矩阵

| 条件 | 要求行为 |
| --- | --- |
| 服务已在跑,版本相同 | 只打开控制台,退出 0;不拉起、不提示 |
| 服务已在跑,版本不同或没有 `version` | 先提示,提示关掉后打开控制台,退出 0;不重启、不拉起 |
| 端口被别的程序占着 | 不当成服务在跑;拉起的服务撞端口退出 → 提示(原因、日志路径、日志最后一行)→ 退出 1 |
| 登录 shell 超时 / 非零退出 / 没有标记 | 结束那个 shell,日志写原因,沿用继承环境照常启动 |
| 服务 10 秒内没有就绪 | 结束它,提示,退出 1 |
| `open` 失败(服务已在跑) | 提示标题说明服务已启动、只是没打开控制台,正文给出地址,退出 1;不附日志最后一行(与此无关) |
| 日志文件打不开 | 提示,退出 1 |
| `--log-file` 后面没有路径 | 报错并以 2 退出,不悄悄退回 stderr |
| 轮转时原路径已不存在 | 不算错,照样在原路径另起新文件 |
| 构建时工作区不干净且没有 `--allow-dirty` | 拒绝 |
| Info.plist 版本、`LSEnvironment`、`LSUIElement`、架构、图标、签名任一不对 | 自检失败,非零退出,删除半成品 |

### 5. 正常 / 基线 / 错误案例

- 正常:双击 → 后台服务起来、浏览器打开控制台、启动器退出;再双击只再开一次控制台。
- 基线:`./csswitch`、不带参数直接运行二进制,行为与引入启动器之前相同;`bash test/run_all.sh`
  不依赖 `packaging/`。
- 错误:让常驻服务做应用主进程(第二次双击没反应);把登录 shell 的全部变量导进服务(旧的
  `ANTHROPIC_*` 跟着进来);为了验证去 `open dist/CSSwitch.app`(用真实 HOME 起真实服务,
  还会打开用户的浏览器)。

### 6. 必需测试

- 单测 `launcher::tests`:标记之前的横幅不被当成变量;白名单之外(凭证、`HOME`、启动器自己的
  变量)不导入;置空的值作为「有」保留;没有标记、或标记后面不是 NUL 时是失败而不是空环境;
  版本比对三态,嵌套的 `sandbox.version` 不算;别的程序的 200 JSON 不算服务在跑。
- 单测 `main::tests`:无参数默认是 `Serve`;显式子命令压过 `CSSWITCH_APP_LAUNCH`;
  `--log-file` 缺路径是错误。
- 单测 `service_log::tests`:用一个代替 fd 1 / 2 的描述符做真实的 `dup2` —— 轮转后**沿用同一个
  描述符号**写出的内容落在新文件、旧内容在 `.1` 且覆盖更早的历史、新文件 0600;文件被外部删掉后
  仍会轮转;已有文件权限被收紧、内容不截断;符号链接被拒绝且目标不变。
- loopback「安装版启动器」一段:以应用包同样的方式执行二进制(无参数 + `CSSWITCH_APP_LAUNCH=1`),
  环境按 Finder 启动的样子给(PATH 只有系统目录),`open` / `osascript` / 登录 shell /
  `claude-science` 全是假的。断言:首次启动拉起且服务脱离启动器(父进程是 1);再次启动 pid 不变、
  日志里只有一次启动横幅且没有抢端口的报错;`service.log` 0600;假 shell 导出的
  `CLAUDE_SCIENCE_BIN` 与 `PATH` 到了服务进程、白名单外的变量没到;登录 shell 卡住时被结束、
  服务照常启动、日志有记录;**真实的 `/bin/zsh`** 读隔离 HOME 里的 `.zshrc` 同样导入成功(假 shell
  只证明管道接得上,这一条证明调用方式在真实 shell 里成立);预先撑过 5 MiB 的日志在服务启动时被
  挪到 `.1`,之后服务写的行落在新文件里;后台是别的版本时提示并只开控制台;端口被占时非零退出、
  提示里有端口、日志路径与系统原始报错。
- 启动器拉起的服务不是测试脚本的子进程:正常路径与 `trap` 都要显式收口(先 `POST /control/quit`,
  再按命令行里唯一的日志路径兜底),跑完用 `pgrep` 确认没有残留。
- 发布侧:`packaging/build_app.sh` 的自检;需要确认包内二进制的运行行为时用
  `CSSWITCH_LOOPBACK_BIN=dist/CSSwitch.app/Contents/MacOS/CSSwitch bash test/run_loopback.sh`。
- 经 Finder 的真实双击、提示框是否看得见、系统放行流程属于真机验收
  (`test/LIVE_ACCEPTANCE.md` §F),离线结论不能替代。

### 7. Wrong vs Correct

#### Wrong

```rust
// 用路径猜角色:从终端直接运行包内二进制会意外变成启动器;
// 而且 serve 子进程同样位于 .app 里,少一道保险就会无限拉起自己
if std::env::current_exe()?.to_string_lossy().contains(".app/Contents/MacOS") {
    return launcher::run();
}
```

#### Correct

```rust
// 显式变量只在没有参数时生效;显式子命令永远优先
match args.get(1).map(String::as_str) {
    None if app_launch => Entry::Launcher,
    None | Some("serve") => Entry::Serve,
    …
}
// 启动器拉起服务:显式 serve + 移除变量
command.args(["serve", "--port", &port, "--log-file"]).arg(log).env_remove(APP_LAUNCH_ENV);
```

---

## Test Gate: 三层门禁合同（2026-08-19）

`bash test/run_all.sh` 是唯一入口，三层全绿才算通过：

| 层 | 内容 | 失败含义 |
| --- | --- | --- |
| 1 static | `cargo fmt --check` + clippy（本仓库代码零告警） | 传递依赖的 future-incompat 提示不计入 |
| 2 unit | `cargo test` | 补偿链、配置、模型目录、控制面 |
| 3 loopback | 起真实服务进程打真实 HTTP | 路由、显式 404、配置权限、凭证不回显、配置文件名隔离、安装版启动器(拉起 / 不重复拉起 / 环境导入 / 失败提示) |

- 三层全部离线。真机验收（官方登录、真实 provider key、工具轮次）在
  `test/LIVE_ACCEPTANCE.md`，结论必须单独写，不能并入门禁结果。
- Rust 工具链发现必须走 `test/_cargo_path.sh`（本机没有 rustup shim）；
  找不到时以 env-blocked 退出，**不得计为通过**。
- loopback 层必须用隔离 HOME，不得读写用户真实的 `~/.csswitch`。
- 应用构建（`packaging/build_app.sh`）不属于三层门禁：它是发布侧的独立结论，单独运行、单独记录。
