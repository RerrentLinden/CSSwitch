# 现场证据:联网轮报错(2026-08-19)

采集时间:2026-08-19 20:20–20:26,故障窗口仍在同一天的日志内。
按父任务约定,本文件只记录**结构、计数与时间**,不摘录任何对话正文、查询词、
frame / org / workspace 标识。

## 采集口径

| 来源 | 路径 | 说明 |
|---|---|---|
| 网关中继日志 | `~/.csswitch/service.log` | 每次 relay 的目标、msgs 计数、生效规则 ID;**无时间戳** |
| Science 服务日志 | `~/.claude-science/logs/server-<date>.log` | 带毫秒时间戳,含上游错误正文 |
| Science spawn 日志 | `~/.claude-science/logs/spawn.log` | 沙箱授权诊断 |

两份日志只能靠**进程启动时刻**与顺序对齐,这一点本身构成可诊断性缺陷(见 D2)。

## 关键时刻

| 时刻 | 事件 | 依据 |
|---|---|---|
| 19:25:20 | 提交 `a90c9f5`(重新落地 bridge 退休) | `git log --date=format` |
| 19:35:01 | 提交 `2ec625c`(噪声头剥离;**移除 502 护栏字符串**) | 同上 |
| 19:55:38 | 二进制构建 | `stat` on `target/release/csswitch-gateway` |
| 19:56:04 | `anthropic_compat.rs` 落盘 | `stat`,**晚于构建** |
| 19:56:29 | `kimi_search_noise.rs` 落盘 | `stat`,**晚于构建** |
| 19:56:54 | 提交 `4730f27`(幻影空搜索对 + 无 id 孤儿修复) | `git log` |
| 19:56:55 | 8788 网关进程重启 | `ps -o lstart`,PID 51547 |
| 20:24:46 | 本轮重新构建 | 本次会话 |
| 20:25:33 | 8788 网关替换为 HEAD 构建 | PID 56158,text 段 inode 46245245 |

**19:56:55 那次重启加载的是 19:55:38 的中间态二进制**,`4730f27` 的最终改动
从未进入运行进程,直到 20:25:33。

## 错误计数

Science 日志,以 19:56:55(旧重启)为切分点:

| 错误族 | 正文 | 总数 | 窗口 | 切分点之后 |
|---|---|---|---|---|
| 502 响应护栏 | `Kimi server-tool response has no safe terminal answer` | 141 | 15:01:03 → 19:07:08 | **0** |
| SSE 协议错 | `upstream SSE protocol error` | 30 | 17:23:09 → 19:05:54 | **0** |
| 429 上游过载 | `rate_limit_error` / `The engine is currently overloaded` | 14 | 16:03:39 → 20:24:00 | 4 |

网关日志侧交叉验证(启动横幅第 38 行之后):`upstream_failure` 仅 4 条,
全部为 `status=429`,**无 400、无 502**。两侧计数一致。

## 分项定性

### 1. 502 护栏风暴 —— 已修复,证据充分

`no safe terminal answer` 是上一个子任务定性的旧响应护栏。
`git log -S` 显示该字符串最后一次变更在 `2ec625c`;当前源码与
**当前运行二进制**中均已不含该字符串(`grep -a` 直接验证二进制)。
141 → 0 的跨重启断崖即为修复生效证据。

### 2. 两次 400 —— 已修复,pairing-repair 生效

网关日志第 34–39 行给出干净对照:同一 `msgs=9` 请求

- 重启前两次:规则清单**不含** `provider.kimi.web-search-result-pairing-repair` → 400 ×2
- 重启后一次:规则清单**含**该规则 → 正常完成

同一历史形状、同一 msgs 计数,唯一变量是补偿规则是否生效。

### 3. 429 —— 上游真实过载,非本项目缺陷

Science 日志中的上游正文为
`{"error":{"type":"rate_limit_error","message":"The engine is currently overloaded, please try again later"}}`。
这是 Kimi 侧容量过载,**不是** README 所述「声明 web_search 却未搜索」的那种 429。
Science 自身按指数退避重试(首次 5.86s)并成功恢复。
链路行为正确:错误如实透传、上层重试,符合无静默 fallback 原则。**不应修改。**

### 4. `/dev/tty` 沙箱行 —— Science 自身噪声,与本项目无关

`spawn.log` 全量 3 次。语义为:daemon 派生的子进程没有控制终端,
打开 `/dev/tty` 返回 ENXIO,沙箱按 fail-closed 丢弃该写授权。
被丢弃的只是一个无法规范化的授权,命令本身正常执行(用户现场 stdout 有正常输出)。
不经由 CSSwitch,不影响中继。

## 结论

用户界面上看到的「server tools 连续报错」由两部分构成:

1. **19:07 之前旧二进制留下的历史残骸**,已持久化进会话历史,回看仍会显示;
2. **20:1x–20:2x 的 4 次真实上游过载**,Science 已自动重试恢复。

当前 HEAD 构建下,前者不可能复现;后者不属于缺陷。

## 反馈 3:新对话 web search 完全不出现(20:30 frame)

采集时间 2026-08-19 20:4x,新网关(20:25:33 上线)运行期间。

### 事实

- 该 frame(20:30:21 创建,9 条消息)落盘内容里**没有任何**
  `server_tool_use` / `web_search_tool_result` 块;唯一的工具调用是一次 python
  `tool_use`(带 id,配对完整)。通过 org 库 `frame_messages` 结构化扫描确认,
  未读取正文。
- 网关日志对应三轮:msgs=3(k3)、msgs=5(k3)、msgs=8(k3-256k,末轮换了槽位),
  **每轮请求都带** `server_tool_types=web_search_20250305:1`、`dropped_server_tools=0`、
  `tool.kimi.web_search.server-tool-preserve` —— 声明完整到达上游。
- 三轮响应侧全部是 `noise=1 pair=2 bytes=739`:上游只发了幻影空壳
  (无 id `server_tool_use` + 空 content 结果),按 `4730f27` 的规则整对剥离。

### 幻影 vs 真实搜索的日志判据(本次沉淀)

- `pair=2 bytes=739`(字节数恒定):幻影空壳轮,上游没有真实搜索。
  739 恒定说明壳形状固定、与话题无关。
- `pair=0 bytes=3xx-4xx`(字节数随查询变化):噪声头回显了真实 query,
  真实搜索对被保留放行。

### 内置浏览器真机验证(20:44,HEAD 构建)

经用户授权,在内置浏览器新建 K3 会话,显式要求联网搜索(Rust 最新稳定版)。
结果:

- **搜索真实执行**。网关日志该轮 `noise=1 pair=0 bytes=423`(真实特征);
  流式期间 UI 出现 Web Search 卡片(有截图);答案给出版本号、日期与来源链接。
- **落盘丢框**。frame(30787b31…)msg[3] 落盘为:
  `web_search_tool_result(tool_use_id=srvtoolu_…, items=5)` + thinking + text,
  **没有 `server_tool_use` 块**。带查询词的搜索框在流结束后消失,
  用户复核时看不到任何 Web Search 的 Server Tool 框。
- 下一轮请求(msgs=6)触发 `web-search-result-pairing-repair` 的
  「无伴侣孤儿结果」分支,补回的是 `input: {}` 的空壳伴侣——**查询词永久丢失**,
  修复只保住了不 400。
- 重启后至 20:5x,网关 `upstream_failure` 为零。

### 判定(修订)

两层叠加,第 4 条是本层真缺陷:

1. 9c16b9f7 那个对话三轮全是幻影空壳(`pair=2 bytes=739`),模型未执行搜索,
   属上游行为,时间与 Kimi 搜索后端 20:19–20:24 四次「engine overloaded」相邻。不修。
2. 幻影空壳不再渲染为空框是 1a 修复的预期效果。
3. 声明直通(`server_tool_types=web_search_20250305:1`、`dropped=0`)、
   剥离未误伤(真机验证真实搜索通过)。
4. **真实搜索的框放行后活不过落盘(D5,真缺陷)**:上游两半配对键恒不匹配
   (归档探测记录第 3 条:`server_tool_use.id` 为 `tool_…`,
   `web_search_tool_result.tool_use_id` 为 `srvtoolu_…`);网关响应侧原样放行,
   Science 落盘时丢弃无法配对的 `server_tool_use`,只留孤儿结果块,UI 不再渲染。
   每一轮真实搜索的查询词证据都在此丢失,且此后每轮请求都依赖 pairing-repair 兜底。

D5 的修复方向(候选,进 PRD R5):响应侧配对归一——真实搜索对放行前,把
`server_tool_use.id` 改写为同对结果块的 `tool_use_id`(**采用上游已有的
`srvtoolu_…` 键,不发明新键**;流式侧 PendingServerTool 本就挂起到下一块揭晓,
机制现成)。归一后 Science 可正常配对、落盘、渲染,查询词保留,
请求侧 repair 对新轮次不再必要。

## R1/R2/R5 真机验收(2026-08-19 21:26–21:35,HEAD+修复 工作树构建)

- **R1 构建门实测**:`./csswitch` 重启时日志出现「源码比二进制新,正在构建…」,
  重建 2.28s;运行进程 text 段 inode 46261449 与新产物一致;无源码比产物新。
- **R2 时间戳实测**:service.log 新行均带 `[…Z]` UTC 毫秒时间戳,
  与本地 +0800 换算一致,可直接对齐 Science 日志的 epoch 毫秒。
- **R5 真机验收(frame 6124e473…,K3 显式搜索)**:
  - 网关日志两轮 `provider.kimi.search-pair-id-adopt … adopted=1`(bytes 431/409);
  - 第二轮请求**不再触发** `web-search-result-pairing-repair`;
  - **整页重载后搜索卡片仍在**:查询词可见、成功步显示 13 results
    (修复前 frame 30787b31 重载后无任何搜索框);
  - 零 `upstream_failure`。
- **采钥后的落盘形态(比预期更干净)**:配对键一致后,Science 把完整
  server 搜索对规整到消息历史之外(UI 从转录存储渲染),`frame_messages`
  里不再出现任何 server 搜索块——孤儿结果、repair 依赖、400 风险同时消失。
  代价:跨轮次上游看不到历史搜索结果,需要时模型会重新搜索。判可接受。

### 验收中的新观察(上游怪癖,不修)

frame 6124e473 第一轮末尾,K3 把下一次搜索发成了**客户端** `tool_use(name=web_search)`
并以 tool_use 收束(落盘 msg[3] 可证),Science 如实报
`Tool 'web_search' not found on agent 'OPERON'` 并回传错误(msg[4] is_error=true),
模型第二轮改回服务端搜索成功。链路各层行为都正确:网关不碰 `tool_use` 类型、
Science 错误透传、模型自愈,总代价一个来回。**不加补偿**——遮蔽它违反
debug-first;若高频复发再按证据重议。模型自己在会话内的解释(「平台抖动」)
与落盘证据不符,以落盘为准。

## 最终 release 四轮真机复跑(22:13–22:16)

用户明确授权 CSSwitch 直接消费已保存的 Kimi 配置,并允许把本轮测试消息与回复作为
验收证据。测试前先构建最终 release；8788 上的旧进程 text inode 与新产物不一致,
通过控制接口退出旧进程后再启动新产物,避免把旧二进制结果误算到本轮。

### 路径与结果

在 Claude Science 的 Getting started 项目新开 Kimi K3 会话,依次执行:

1. 显式要求联网查询 Rust 最新稳定版、发布日期与来源；
2. 根据上一轮结果复述版本与日期,无必要不再次搜索；
3. 明确要求不联网,计算 `37×41`；
4. 把上一轮结果加一。

可观察结果:

- 第一轮产生两次真实 Web Search；网关两次都记录
  `provider.kimi.search-pair-id-adopt adopted=1`,没有 pairing repair。
- 第二轮正确复述 `1.97.1 / 2026-07-16`,未新增真实搜索；第三、四轮分别返回
  `1517`、`1518`。
- 后续 K3 请求只出现既有幻影对剥离(`empty-search-pair-strip`,固定 739 bytes),
  没有 `web-search-result-pairing-repair`、400 或 `upstream_failure`。
- 整页重载后两张 Web Search 卡片、查询词与结果数仍可见；D5 的卡片/查询词丢失
  与后续配对修复依赖均未复现。
- 一次 Science Bash 步骤因自身网络白名单失败,模型随后从允许的一手来源完成回答；
  该失败未经过 CSSwitch 上游错误路径,不归入网关缺陷。

### 新观察:搜索查询与用户主题不相关

两张 Web Search 卡片显示的查询均围绕 `compute snapshot / 32GiB / 10 cores`,
结果也与该错误查询一致,而用户要求的是 Rust 版本。模型最终依靠后续命令步骤得到
Rust 官方结果。

网关本轮对真实搜索对只执行噪声文本剥离与配对键采钥；采钥实现只改
`server_tool_use.id` / `web_search_tool_result.tool_use_id`,不改 `input.query` 或结果内容。

后续脱敏入站探针与 direct gateway A/B 已把根因收窄到消息顺序:真实 Science 请求
末尾是连续两条 user message——Rust 问题在前,Science 注入的 compute snapshot /
10 cores / 32GiB 机器上下文在后。direct K3 native web_search 原序稳定生成机器规格查询;
只把两条反转成“机器上下文 → Rust 问题”即稳定生成正确 Rust 查询。普通 client-tool
对照没有发起搜索,因此不恢复旧 bridge。修复采用窄尾部重排,不改查询词或上下文正文。

### R6 真机候选复跑

同一全新 K3 会话完成 6 个用户轮次:Rust 搜索正确 query/card → 历史复述 → Python
搜索正确 query/card → 跨两次搜索不联网比较 → `29×43=1247` → 再减 7 得 `1240`。
请求日志在搜索轮命中 `provider.kimi.science-context-tail-reorder`;响应侧两轮均命中
`provider.kimi.search-pair-id-adopt adopted=1`,非搜索轮仅命中既有
`empty-search-pair-strip`。整页重载后两张搜索卡片与最终结果均保留；全程未出现
`web-search-result-pairing-repair`、400 或 `upstream_failure`。这证明请求侧重排、
响应侧采钥、历史复述与非搜索工具/计算轮可在同一真实会话稳定叠加。

### 未覆盖与剩余风险

- 未跑 K2.7、非流式真实搜索、超过四轮的混合长会话。
- K3 的无关查询若高频复现,需以新的上游原始 SSE 探测单独定性；在此之前不改变
  已通过配对/持久化验收的窄补偿。

## 质检收尾(2026-08-20)

前一轮 trellis-check 死于会话限额(遗言:「文档干净,接下来对照 HEAD 查过滤器
范围外的行为漂移」)。限额恢复后对工作树上的两文件加固 diff
(web_search 名称收窄、content_empty 收紧为显式 `[]`、请求侧 declared/nearest
顺序化)重跑完整质检:

- **隔离树(HEAD dbf2aa0 + 该 diff)权威结果:132 单测全绿,run_all.sh 三层 exit 0**。
  主树当时因子任务(08-19-kimi-web-search-review,K2.7 adapter)在途接线暂不可编译,
  与被检 diff 无关。
- 三项核查全部 pass;反向覆盖(异名 server tool 不吞不采、畸形 content 不当幻影、
  foreign id 不污染配对)已由 diff 自带的 4 个新测试锁住;质检自修了 3 处
  契约注释漂移(模块头判据与实现脱节)。
- 顺带修掉一个旧缺陷:旧流式逻辑会把非 web_search 的 server_tool_use 挂进
  pending,「异名无键 use + 空结果」会被误吞,收窄后由新测试锁死。
- 留给子任务的三条尾巴:①外部文档/spec 的幻影判据表述未含 name 门与显式 `[]`
  (子任务研究已声明为其交付物);②合成伴侣在两种**从未观测过**的形状下可能撞 id
  (前向引用 / foreign 误绑),按「判据来自实测」原则不预修;③「合成伴侣 +
  缺失/非数组 content」的上游可接受性未实测,若日后 400 是显式可诊断失败。
- 该加固 diff 与子任务研究指令一致,归其工作线,由其随自身接线一并提交;
  本任务不再触碰工作树。R1 构建门此时成为现役护栏:子任务修完编译前,
  `./csswitch` 会拒绝启动而不是静默跑旧二进制。

## 派生缺陷

| ID | 缺陷 | 依据 |
|---|---|---|
| D1 | `csswitch` 启动脚本仅在二进制**缺失**时构建(`if [ ! -x "$BIN" ]`),源码变更不触发重建,「重启」静默等于「跑旧版」 | 脚本正文;19:55:38 / 19:56:29 时间差 |
| D2 | `~/.csswitch/service.log` 无时间戳,无法与 Science 日志直接对齐 | 本次采集全程依赖进程启动时刻反推 |
| D3 | README 与 `anthropic_compat.rs:903` 注释仍描述已退休的 web_search bridge 与不存在的 `kimi_coding_search.rs` | `ls src/`;README「渠道补偿」节 |
| D4 | 父任务 #1 的真机验收跑在 19:55:38 中间态构建上,非 HEAD;`4730f27` 最终改动至 20:25:33 才首次上线 | 上表「关键时刻」 |
