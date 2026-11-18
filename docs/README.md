# CSSwitch 文档

产品概览在根目录 [README](../README.md)。合同与背景按能力放在 OpenSpec 里,这里只留总览与证据。

- [架构](ARCHITECTURE.md) —— 一句话、进程与端口,以及每个主题该看哪个能力。
- [行为规格](../openspec/specs/) —— 每个能力一个目录:`spec.md` 是需求与场景,`design.md` 是由来、实测证据、必需测试与反例。
  渠道从 [kimi-channel](../openspec/specs/kimi-channel/spec.md) 与 [deepseek-channel](../openspec/specs/deepseek-channel/spec.md) 看起。
- [编码规范](../openspec/conventions/README.md) —— 目录结构、上游能力判定、补偿退役、三层门禁、控制台验收。
- [任务证据](evidence/) —— 各次任务的研究与真机记录;capability 规则的 `evidence` 指向这里。
- [真机验收清单](../test/LIVE_ACCEPTANCE.md) —— 离线门禁覆盖不到的部分。

Agent 规则在 [AGENTS.md](../AGENTS.md) 与 [.agents/rules/](../.agents/rules/)。
