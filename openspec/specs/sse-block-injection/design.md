# sse-block-injection 设计注记

## 签名

```text
crate::anthropic_sse::Validator          校验输出流生命周期，message_stop 扣留至干净 EOF
StreamFilter::feed(&[u8]) -> Result<Vec<u8>, String>
render_block(index: u64, block: &Value) -> Vec<u8>
```

## 理由与教训

整块内容塞进 `content_block_start` 就以为完成——本地看着对，客户端拿到的是空块。

## 必需测试

- 用"模拟规范客户端"的还原函数做往返断言：从 start 取壳并清空流式字段，仅靠 delta 累积，断言还原结果与原块**完全相等**。
- 断言注入块在 `message_delta` 之前、`message_stop` 之后不出现，且索引连续。
- 断言上游调用失败时产出终止 error 而非内容。

## Wrong vs Correct

### Wrong

```rust
// payload 只在 start 帧；规范客户端还原出 input:{} 与空 thinking，
// 并把这些残缺块回传给上游
out.extend(render_sse(Some("content_block_start"),
    &json!({"index": i, "content_block": block})));
out.extend(render_sse(Some("content_block_stop"), &json!({"index": i})));
```

### Correct

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
