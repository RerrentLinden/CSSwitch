# 当前已知问题与证据缺口

最后整理：2026-07-28。已解决历史放入 CHANGELOG 或 dated evidence，不在这里重复。

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

## Kimi for Coding 渠道

- 上游对"声明了 `web_search_20250305` 但本轮模型未实际搜索"的请求返回 429 `rate_limit_error`，DeepSeek 同条件无此缺陷。补偿为客户端工具桥接：换成同名客户端工具，模型主动调用时 gateway 用真 server 工具补发一次并把搜索块拼回同一条消息。已在真实 Science 验证原生 web_search 渲染。详见[渠道文档](../../docs/features/kimi-for-coding-channel.md)。
- 该端点还存在与 web_search 无关的偶发 429（一次无工具基础请求也曾 429）。任何把 429 解释为"本轮没搜索"的策略都不可靠。
- 上游搜索轮的块序为 `text, server_tool_use, web_search_tool_result, thinking, text`，thinking 不在首块，与 Anthropic 规范和 DeepSeek 的表现都不同。经桥接改写后已在真实 Science 中验证渲染正常（原生 Web Search 组件、结果列表与来源链接齐全）。
- 已验证 `kimi-for-coding`、`kimi-for-coding-highspeed`、`k3`、`k3-256k` 四个模型的 thinking 与 `tool_choice` 行为；图片、视频输入、原生流式结构化输出未验证。
- gateway 链路已用真实 Science（隔离实例）+ 真实订阅 key 端到端验证：纯文本轮、多轮 Python 工具调用循环、Reviewer 与 Notebook 均正常，Science 内部的 `create_work_item` 与 `verdict` 两类强制工具调用也都通过。桌面端 UI 入口已交付并在浏览器 mock 模式可视化验收（服务商网格、默认值预填、创建入列、编辑回读、深色主题）；**安装版 artifact 的端到端验收未执行**，UI 与真实后端合并后的链路未在打包版本中走过一遍。
- `400 relay history ends with unresolved tool calls`：当客户端工具调用在等待用户授权（如 `request_network_access`）时，Science 仍会继续发起推理，历史以未解决的 `tool_use` 结尾，被 relay 通路的 `validate_relay_tool_history` 拒绝。该校验对所有 relay contract 无差别生效，非本渠道特有（实测中由安装版应用的 custom-anthropic 通路产生）。桥接落地后模型不再被迫改用 `request_network_access`，触达概率下降，但该通用问题本身仍未修复。
- 上游不实现 Anthropic `document` 内容块：四种 source 形态与一个不存在的块类型返回完全相同的 `Invalid request Error`，属解析器未知块分支而非格式问题。该块留在历史里会让此后每轮都失败（真实会话 27 条消息成功、29 条失败）。**该块是 Science 平台 PDF 视觉通道的载荷**（`read_file(pages=[…])` → `queued_for_vision` + `[System] Attached file` + `document`），因此**该平台路径在本渠道确实不可用**。CSSwitch 替换为署名占位文本以保住该轮。读 PDF 仍可行：Agent 自行渲染页面为 `image` 块传入，上游接受，已在真实会话验证。DeepSeek 接受该块但带与不带附件都回 `CANNOT_READ`，故这不是相对同类渠道的倒退；`image` 块不受影响。
- 占位文本必须署名来源：实测中模型引用未署名占位文本后被追问出处，把一个正确结论当作自己编造的撤回了。
- 桥接的搜索轮会多一次上游往返；模型在 Science 全量工具集下有时会绕开 web_search 改用 `bash`，此时桥接保持空闲。搜索结果块的顺序为 `server_tool_use, web_search_tool_result` 紧随首轮已流出的前言之后，与上游原生顺序不同但 Science 渲染正常。

## 测试

- 真机验收矩阵描述应执行的场景，不表示最终 v0.8.2 DMG 已逐项全部执行。每次验收必须绑定 exact artifact，并把通过、失败、环境阻塞与未执行分开记录。
- 门禁总入口 `test/run_all.sh` 为五层聚合器（见 [testing.md](../../docs/operations/testing.md)），无第三方 Python 依赖、不要求 worktree 干净；`env-blocked` 计入 `current-env clean` 但阻断 `release-ready green`，报告时不得把两者混写。
- rust 层的 ignored 测试基线为 33（全部带显式理由的 Acceptance-boundary / 真机 E2E），偏离基线需人工审读，见 testing.md。
