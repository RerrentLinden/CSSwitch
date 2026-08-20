# 当前已知问题与证据缺口

最后整理：2026-08-19。已解决历史放入 CHANGELOG 或 dated evidence，不在这里重复。

## v0.8.2 已发布后的边界

- OpenCode Go、Grok 与 Gemini 的门禁覆盖文本、多轮、tools / `tool_choice`、模型发现或手填、标题和 classifier；图片、厂商专有 reasoning、原生流式与结构化输出仍为 limited，Gemini native API 不在本版范围。
- 当前已建立 clean source、单测、隔离 mock / loopback、最终安装 UI 和公开附件回读。最终 artifact 没有执行 OpenCode Go、Grok、Gemini、Kimi、DeepSeek 或 Codex 的用户 key / OAuth live 推理，不能由本地门禁替代。
- 多历史组织恢复已有精确 marker / choice / identity 测试，最终安装版没有构造真实多组织 Sign out 数据集；SSH 已覆盖 Science 隔离 HOME 的具体 Host 配置生成，但没有真实 SSH server 的远端连通证据。

## 分发

- v0.8.2 公开附件只有经过完整性验证的 ad-hoc seal，没有建立 Developer ID、notarization、stapled ticket 或 Gatekeeper acceptance 证据。首次打开可能需要用户右键选择“打开”。

## Codex

- Codex 是默认关闭的实验能力。上游账号权限、动态模型目录和 Responses 协议可能变化；单账号、浏览器登录、macOS Apple Silicon 是当前边界。
- 动态目录校验是一次最长 130,750 ms 的有界请求；极端上游持续无响应时 UI 会保持忙碌直到超时，不支持中途取消。
- 不支持设备码、多账号、代理认证、PAC、自定义 CA、系统代理自动发现或 TUN 检测；Finder 启动的环境变量与终端可能不同，`direct` 也可能仍由系统 TUN 接管。
- v0.7.0 曾观察到浏览器失败页在有效 callback 后只显示通用安全错误；v0.8.0 增加了结构化通知与浏览器 fallback，但最终公开 DMG 未重跑该真实账号失败路径，不能据此宣布上游或本地提交根因已被穷尽。
- 历史 Acceptance 候选已有真实 CSSwitch OAuth、模型和 Science 最小文本成功证据，但最终公开 v0.8.0 DMG 没有重新执行真实 OAuth / 模型 / 推理；两者不能合并为同一层证据。
- v4 配置回滚到 v0.7.0 或更早版本前，必须先在 v0.8.0 导出并降级到 v2，或停止全部 CSSwitch 进程后恢复兼容备份；删除 profile 本身不会降低 schema。

## Science / Skill / SSH

- 安装、attach、load 与重启持久化不证明任一 Skill 的脚本、资产、网络、依赖或领域功能可用。
- 仅给名称时的来源搜索由 provider / Agent 能力决定；私有仓库、更新 / 覆盖、永久删除、恢复 UI 和 bundle 成员级物理删除不受支持。
- route attachment、nonce / CSRF control plane 与 `OPERON` Skill 绑定是观察到的 Science 合同；Science App 更新后必须重跑聚焦兼容性验证。
- 第三方 Science 以 `--no-auto-update` 运行；其设置页更新不会应用。先在官方 Science 完成更新，再停止并启动 CSSwitch 管理链。
- Agent 控制面配置是多个顺序请求，不是原子事务；失败只降级为 warning，已完成步骤不会自动回滚。
- 系统 SSH 默认关闭；opt-in 后 config / wrapper 校验 fail closed，未对特定用户的真实 SSH server 做连通性验证。
- `BUG-083-SSH-LATE` 已完成源码级修复：SSH 可预检项先于 OAuth，生命周期串行区会重检精确候选与既有受管 Gateway 上下文，真正晚失败会精确补偿 OAuth、active profile、Gateway、Science、managed stub、journal 与耐久清理状态。当前证据仅来自临时 HOME、假凭证、假 Science、本地 Gateway 和 loopback；production artifact/runtime、已安装 App、真实 provider、真实 SSH server、签名和公开发布仍未验证，产品 gate 保持 open。
- fresh xhigh 复审接受一个与本 bug 分离的 P3 威胁模型边界：最终校验后，若同 UID 对 CSSwitch 私有根实施恶意 pathname 替换，当前实现并非全程 fd-relative。该边界沿用[既有 runtime / 安全记录](../../docs/evidence/investigations/2026-07-18-v070-ui-redesign-runtime-security-review.md)，在当前私有根威胁模型内不阻断 `BUG-083-SSH-LATE`，本窗口不另建 bug。

## Kimi 渠道（开放平台 / for Coding）

最后更新：2026-08-20（恢复统一 query-tool bridge 并完成三模型真机验收）。

- 两个渠道共用 provider contract `kimi-anthropic-relay` 与同一套补偿；差异只有默认地址与模型预设。**实测证据全部来自 `api.kimi.com/coding`，开放平台按用户判定沿用、未独立实测。**
- 开放平台原有的一批只服务它的处理已全部移除（`?beta=true`、强制 `anthropic-beta`、伪装 `claude-cli/*` UA、强制 `x-app: cli`、失败历史尾巴清理、thinking 强制 `enabled` 与思考续写存储）。**若开放平台实际依赖这些标识放行，表现将是请求被拒或能力降级**；恢复实现见 git 历史。
- **web_search 已恢复统一 query-tool bridge**：2026-08-20 review 证明 K3、K3-256k、K2.7 的原生 inline planner/answer 行为不一致。Science 主请求把 typed search 映射为私有 query tool；需要搜索才发 forced nested server search，必要时有界 synthesis。旧 920 行实现未机械恢复；当前桥复用 transport/SSE/filter，保留原生路径作为 nested executor。逐条合同与真机证据见[渠道文档](../../docs/features/kimi-channels.md)。
- 当前已知边界：
  - 上游搜索轮**每轮**注入 `Search results for query:` 噪声头，不搜索的轮次**常发**无 id 幻影空搜索对——两条剥离规则是常态命中而非偶发补丁，日志 `noise=/pair=` 可见。
  - Science 会把 `compute snapshot` 机器上下文作为独立 user message 追加在真实问题之后；Kimi 主 query planner 会误取最后一条为主题。仅在 typed web_search 与精确硬件签名同时出现时交换尾部两条 text-only user message。
  - 幻影对在剥离规则落地前已落盘的历史，靠请求侧配对修复的无 id 分支救活（实测 Resume 后 400 → 200）；未经修复版本 gateway 的存量会话首轮可能仍撞一次 400。
  - 有搜索历史的下一轮，Science 会把上轮结果存盘并渲染一个**输出为空的 Server Tool 折叠框**（`[System] … results persisted`）。这是 Science 平台自己的历史压缩行为，与渠道无关，不是缺陷。
  - **桥接搜索卡片仅在最新轮存活**（2026-08-20）：会话每推进一轮，Science 把旧 assistant 消息规整为纯 text，桥接轮的搜索对被追溯剥离、卡片消失（答案正文永久保留）；原生 typed 流式路径的卡片则跨轮持久。两者线上形态在可黑盒检视维度等形，Science 持久层选收依据未知；诊断靠 adapter 日志 `merged_shape` / `pair_key_prefix`。验收陷阱：测"重载后卡片仍在"必须先发过追问再重载，只测最新轮是假阳性。
  - nonstream 搜索轮实测 29.5–40s，合同 `total_ms` 已从 30s 上调至 180s；流式 read_idle 维持 300s。
  - 该端点仍存在与 web_search 无关的偶发 429；任何把 429 解释为特定语义的策略都不可靠。
- `400 relay history ends with unresolved tool calls`：当客户端工具调用在等待用户授权（如 `request_network_access`）时，Science 仍会继续发起推理，历史以未解决的 `tool_use` 结尾，被 relay 通路的 `validate_relay_tool_history` 拒绝。该校验对所有 relay contract 无差别生效，非本渠道特有。当前 search bridge 不把失败降级成 network-access/command fallback，但普通工具本身仍可能触达该通用问题。
- 上游不实现 Anthropic `document` 内容块（控制组判定为解析器未知块分支）。该块是 Science 平台 PDF 视觉通道的载荷，故**该平台路径在本渠道不可用**；CSSwitch 替换为署名占位文本保住该轮。读 PDF 仍可行：Agent 渲染页面为 `image` 块传入。占位文本必须署名来源（未署名时模型曾把正确结论当编造撤回）。
- 已在最终 bridge release 上分别验证 `k3`、`k3-256k`、`kimi-for-coding` 的搜索、同会话不联网追问与整页重载（2026-08-20 内置浏览器）。未知模型、图片/视频输入、原生流式结构化输出未验证。
- **K3-256k 搜索慢已定性为上游 256k 服务池属性，非桥接开销**（2026-08-20 补正，此前"首字节延迟为已知代价"的说法不准确）：同题受控对比（宇树股价），差距全部在 nested（上游执行搜索）那一跳——`k3-256k` main 7.5s + nested 56.7s ≈ 64s，`k3` main 6.1s + nested 13.5s ≈ 20s；多次观测 `k3` nested 稳定 8–15s，`k3-256k` nested 在 15s / 26s / 57s / 100s+ 间波动。桥接固有代价只有"首字节 ≈ main 时长"（约 6–10s），对三个模型相同。网关无法改变单次上游调用时长；nested 执行端点可指定是唯一调节点（产品决策，未动）。已向 Kimi 官方反馈。
- 真机验收细节与逐条判据见 [test/LIVE_ACCEPTANCE.md](../../test/LIVE_ACCEPTANCE.md)；本轮修复的任务证据在 `.trellis/tasks/archive/2026-08/08-19-kimi-search-hang/`。

## 测试

- 真机验收矩阵描述应执行的场景，不表示最终 v0.8.2 DMG 已逐项全部执行。每次验收必须绑定 exact artifact，并把通过、失败、环境阻塞与未执行分开记录。
- 门禁总入口 `test/run_all.sh` 为三层聚合器（static / unit / loopback，见脚本头注释），全部离线可跑；找不到工具链输出 `env-blocked` 并以退出码 3 阻断。旧记录中的"五层聚合器 + testing.md + ignored 基线 33"来自 fork 前仓库，本仓库不适用（2026-08-19 实测：三层、无 testing.md、0 ignored）。
