# CSSwitch 文档

产品概览在根目录 [README](../README.md)。除它之外的文档都在这个目录里。

- [架构](ARCHITECTURE.md) —— 一句话、进程与端口,以及每个主题该看哪个能力。
- [行为规格](specs/) —— 每个能力一个目录:`spec.md` 是需求与场景,`design.md` 是由来、实测证据、必需测试与反例。
  渠道从 [kimi-channel](specs/kimi-channel/spec.md) 与 [deepseek-channel](specs/deepseek-channel/spec.md) 看起。
- [编码规范](conventions/README.md) —— 目录结构、上游能力判定、补偿退役、三层门禁、控制台验收。
- [任务证据](evidence/) —— 各次任务的研究与真机记录;capability 规则的 `evidence` 指向这里。
- [真机验收清单](../test/LIVE_ACCEPTANCE.md) —— 离线门禁覆盖不到的部分。

进行中的变更(`changes/`)只留在本机,不入库。

Agent 规则在 [AGENTS.md](../AGENTS.md) 与 [.agents/rules/](../.agents/rules/)。
