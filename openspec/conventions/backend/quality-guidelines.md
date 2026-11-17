# Backend Quality Guidelines

这里只留做法类的规范。各项行为合同(官方实例接管、启动覆盖项、免登录沙箱、模型清单、配置目录、
provider 补偿、SSE 块注入、Kimi 搜索、安装版启动器)在 [`../../specs/`](../../specs/)。

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
- capability 规则的 `evidence` 路径会随被引用文档移动或归档而失效。合并/改名规则时同批核对文件真实存在——死链的 evidence 等于没有 evidence。

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
