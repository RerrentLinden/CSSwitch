# provider-compensation 设计注记

## 理由与教训

- 复测方法见 [backend quality guidelines](../../conventions/backend/quality-guidelines.md)「判定上游是否真的不支持某能力」。
  最小成本是对上游直接发几个变体，一次对比就能定出边界。
- 策略曾经只读环境变量：契约里明明声明了策略，任何忘记注入该变量的启动路径都会**静默丢掉全部补偿**，
  表现为上游 400——而配置文件看起来完全正确。
- 规则日志记入被覆盖的改写，排查时会以为请求是以那个中间形态发出去的。
- 错误：因为"另一家这么写"就扩大判据，在能过的形态上也做补偿。

## 必需测试

- 每条补偿至少一条单测断言「拒收形态 → 归一化后形态」。
- 每条**收窄过**的判据要有一条反向测试，断言边界外的形态**不被触碰**
  （如 `only_the_specified_form_costs_thinking`）。
- 规则覆盖场景要断言被覆盖的规则**不出现**在 rule_ids 里
  （如 `specified_tool_choice_supersedes_the_auto_rewrite`）。
- 契约接线要有一条不依赖任何 env 的测试（`thinking_policy_comes_from_the_contract_without_any_env`）。

## Wrong vs Correct

### Wrong

```rust
// 照搬来源实现的宽判据,未复测边界
match body.get("tool_choice") {
    Some(Value::Object(obj)) => obj.get("type") != Some("none"),  // auto 也中招
    ...
}
```

### Correct

```rust
// 判据 == 实测出的拒收边界
body.get("tool_choice").and_then(Value::as_object)
    .and_then(|c| c.get("type")).and_then(Value::as_str) == Some("tool")
```
