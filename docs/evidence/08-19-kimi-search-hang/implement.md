# 执行计划

前置：读 [design.md](design.md)。分支：直接在 `main` 工作树（仓库惯例，无并行任务）。
gateway 代码规范见 `.trellis/spec/backend/index.md`（实施前过一遍）。

## 阶段 A：恢复 native 基线

- [ ] `git revert 21337be`（恢复 d49871a：native 透传 + 配对修复 + 桥接退役）
- [ ] 解决与其后提交的冲突（`913493b`/`17b53aa` 动过 catalog 与测试 fixture）
- [ ] `cargo test -p csswitch-gateway` 全绿，确认恢复干净
- 回滚点：revert 本身可单独 revert

## 阶段 B：噪声头剥离规则

- [ ] 非流式：`filter` 响应 content，删除 `Search results for query:` 前缀 text 块，
      记 rule id `provider.kimi.search-noise-text-strip`
- [ ] 流式：新建窄过滤器（缓冲判定前缀 → 吞块 → index 补洞），挂到 Kimi relay 流路径
- [ ] 单测：命中剥离（首位/中位/末位悬挂）、前缀分片跨 delta、未命中原样、
      index 补洞后连续、4KB 缓冲上限
- [ ] 单测素材直接取自探测存档的真实 SSE 形态（scratchpad `probe/resp1s.sse` 等，
      入测试时同样只保留结构不含对话正文）

## 阶段 C：止血

- [ ] `server.rs` 流环：过滤器 `Err` 不再丢弃信息——SSE error 事件带真实 message，
      `eprintln!` 带规则上下文
- [ ] 检查恢复后的非流式失败路径同样带原因入日志
- [ ] 单测：过滤器错误透出真实文案

## 阶段 D：合同超时

- [ ] `catalog/provider-contracts.v1.json`：`kimi-anthropic-relay.timeouts.total_ms`
      30000 → 180000（只动这一个字段）
- [ ] 有 golden fixture 的同步更新（`913493b` 先例）

## 阶段 E：门禁与真机验收

- [ ] `bash test/run_all.sh` 五层，逐层记录
- [ ] 重启用户 8788 服务换新二进制（先征得同意，用户对话仍在重试中）
- [ ] 内置浏览器驱动 Science 按 design「验收路径」四条走完，截图留档
- [ ] 清理 8799 探测实例

## 阶段 F：文档与规格

- [ ] `docs/features/kimi-channels.md`：第 3/3b 节改写为「桥接已退役 + 本轮证据」，
      保留恢复路径指引
- [ ] `.agents/context/known-issues.md`：更新 Kimi 渠道段（429 未复现口径、新边界）
- [ ] `docs/message-block-flow.md` 流程图同步
- [ ] `test/LIVE_ACCEPTANCE.md` 增补本轮验收行
- [ ] 视获得的教训跑 trellis-update-spec / break-loop
