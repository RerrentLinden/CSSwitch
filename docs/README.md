# CSSwitch 文档

产品概览在根目录 [README](../README.md)。这里放需要展开的合同与背景。

- [架构](ARCHITECTURE.md) —— 边界、进程与安装版启动器、数据流、端点合同、失败处理。
- [消息块的组装与适配](message-block-flow.md) —— Science 发出什么形状的消息,两个渠道各自改了什么(含流程图)。
- [Kimi 渠道](features/kimi-channels.md) —— Kimi 的四条补偿与各自的实测证据。
- [DeepSeek 渠道](features/deepseek-channel.md) —— DeepSeek 的六条补偿与来源。
- [真机验收清单](../test/LIVE_ACCEPTANCE.md) —— 离线门禁覆盖不到的部分。
- [任务证据](evidence/) —— 各次任务的研究与真机记录;capability 规则的 `evidence` 指向这里。
- [行为规格](../openspec/specs/) —— 每个能力一个目录:`spec.md` 是需求与场景,`design.md` 是签名、必需测试与反例。
- [编码规范](../openspec/conventions/README.md) —— 目录结构、上游能力判定、补偿退役、三层门禁、控制台验收。

Agent 规则在 [AGENTS.md](../AGENTS.md) 与 [.agents/rules/](../.agents/rules/)。
