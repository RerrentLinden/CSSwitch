//! DeepSeek 官方 Anthropic 兼容端点(`https://api.deepseek.com/anthropic`)的请求补偿。
//!
//! 六条规则的语义源自 cc-switch(MIT,© 2025 Jason Young)及其 fork biociao/cc-switch
//! 中 `src-tauri/src/proxy/providers/claude.rs` 的 DeepSeek normalizer 链;本文件按
//! CSSwitch 的规则体系(规则 ID + 端点门控 + 单测)重写,不是逐行拷贝。
//!
//! 相比被替代的旧实现,这里全部是**无状态**改写:不落盘保存/恢复 thinking 块。
//! DeepSeek 要求带 `tool_use` 的历史助手轮回传 thinking,靠占位块即可满足,
//! 不需要 CSSwitch 侧的续写存储。

use serde_json::{json, Value};

pub const RULE_THINKING_AUTO_ADAPTIVE: &str = "provider.deepseek.thinking-auto-adaptive";
pub const RULE_TOOL_CHOICE_DISABLES_THINKING: &str =
    "provider.deepseek.specified-tool-choice-disables-thinking";
pub const RULE_THINKING_DISABLED_STRIPS_EFFORT: &str =
    "provider.deepseek.thinking-disabled-strips-effort";
pub const RULE_TOOL_THINKING_HISTORY_REPLAY: &str =
    "provider.deepseek.tool-thinking-history-replay";
pub const RULE_MALFORMED_SERVER_TOOL_REPAIR: &str =
    "provider.deepseek.malformed-server-tool-block-repair";
pub const RULE_ORPHAN_TOOL_PAIRING_REPAIR: &str = "provider.deepseek.orphan-tool-pairing-repair";

/// 历史助手轮缺 thinking 时的占位文本。上游只校验存在性与非空。
const THINKING_PLACEHOLDER: &str = "tool call";
const REDACTED_THINKING_PLACEHOLDER: &str = "[redacted thinking]";
const ORPHAN_TOOL_RESULT_PLACEHOLDER: &str =
    "[tool execution was interrupted or its result is unavailable]";
const OMITTED_ORPHAN_TOOL_RESULT: &str = "(omitted orphan tool result)";

/// 单模型的 max_tokens 上限。超过上限上游直接拒收。
pub fn clamp_max_tokens(value: Option<u64>, model: &str) -> Option<u64> {
    let cap = match model {
        "deepseek-v4-pro" => 65_536,
        "deepseek-v4-flash" => 32_768,
        _ => 8_192,
    };
    value.map(|v| v.min(cap))
}

fn append_rule(rule_ids: &mut Vec<String>, rule: &str) {
    if !rule_ids.iter().any(|existing| existing == rule) {
        rule_ids.push(rule.to_string());
    }
}

/// 请求体上的全部 DeepSeek 补偿,按依赖顺序执行。
pub fn normalize_request(body: &mut Value, target_model: &str, rule_ids: &mut Vec<String>) {
    clamp_body_max_tokens(body, target_model);
    // 指定工具型 tool_choice 会把 thinking 直接压成 disabled,thinking 原本的取值
    // 就此失去意义;先判它,免得日志里记一条净效果为零的 auto→adaptive。
    if !disable_thinking_for_tool_choice(body, rule_ids) {
        normalize_thinking_auto(body, rule_ids);
    }
    strip_effort_when_thinking_disabled(body, rule_ids);
    replay_tool_thinking_history(body, rule_ids);
    repair_malformed_server_tool_blocks(body, rule_ids);
    repair_orphan_tool_pairing(body, rule_ids);
}

fn clamp_body_max_tokens(body: &mut Value, target_model: &str) {
    let Some(current) = body.get("max_tokens").and_then(Value::as_u64) else {
        return;
    };
    if let Some(clamped) = clamp_max_tokens(Some(current), target_model) {
        if clamped != current {
            body["max_tokens"] = json!(clamped);
        }
    }
}

/// Claude Science 发非标准的 `thinking: {"type": "auto"}`;DeepSeek 的请求体
/// 反序列化只接受 `adaptive` / `enabled` / `disabled`,遇到 `auto` 直接 400。
/// `adaptive` 是语义最接近的取值。
fn normalize_thinking_auto(body: &mut Value, rule_ids: &mut Vec<String>) {
    let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut) else {
        return;
    };
    if thinking.get("type").and_then(Value::as_str) != Some("auto") {
        return;
    }
    thinking.insert("type".to_string(), json!("adaptive"));
    append_rule(rule_ids, RULE_THINKING_AUTO_ADAPTIVE);
}

/// 上游在思考开启时拒收**指定工具型**的 tool_choice
/// (`{"type":"tool","name":…}` → 400 "Thinking mode does not support this
/// tool_choice")。Science 的辅助抽取请求依赖强制指定工具拿结构化输出,
/// 因此保留 tool_choice、放弃这一轮思考。
///
/// 2026-08-19 实测边界:`auto`、`any`、`none` 以及不带该字段的请求在
/// thinking enabled / adaptive 下都返回 200 且照常输出 thinking 块,
/// 因此**只对 `tool` 这一种形态补偿**——对 auto/any 也关思考是白白牺牲推理质量。
/// 返回是否已把 thinking 压成 disabled(调用方据此跳过 thinking 取值的归一化)。
fn disable_thinking_for_tool_choice(body: &mut Value, rule_ids: &mut Vec<String>) -> bool {
    if !has_thinking_incompatible_tool_choice(body) {
        return false;
    }
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    let already_disabled = obj.get("thinking") == Some(&json!({"type": "disabled"}));
    obj.insert("thinking".to_string(), json!({"type": "disabled"}));
    let removed_effort = strip_effort_fields(obj);
    if !already_disabled || removed_effort {
        append_rule(rule_ids, RULE_TOOL_CHOICE_DISABLES_THINKING);
    }
    true
}

fn has_thinking_incompatible_tool_choice(body: &Value) -> bool {
    body.get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        == Some("tool")
}

/// `thinking: disabled` 与 effort 参数在上游互斥,同时出现返回 400
/// ("thinking options type cannot be disabled when reasoning_effort is set")。
/// 尊重客户端显式的 disabled(子 agent 不需要思考块),剥掉冲突的 effort。
fn strip_effort_when_thinking_disabled(body: &mut Value, rule_ids: &mut Vec<String>) {
    let disabled = body
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
        == Some("disabled");
    if !disabled {
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if strip_effort_fields(obj) {
        append_rule(rule_ids, RULE_THINKING_DISABLED_STRIPS_EFFORT);
    }
}

fn strip_effort_fields(obj: &mut serde_json::Map<String, Value>) -> bool {
    let mut changed = obj.remove("reasoning_effort").is_some();
    let mut drop_output_config = false;
    if let Some(output_config) = obj.get_mut("output_config").and_then(Value::as_object_mut) {
        changed |= output_config.remove("effort").is_some();
        drop_output_config = output_config.is_empty();
    }
    if drop_output_config {
        obj.remove("output_config");
        changed = true;
    }
    changed
}

/// 上游要求每个带 `tool_use` 的历史助手轮回传 thinking 块;Anthropic 客户端
/// 常把 thinking 剥掉或脱敏后再回放,导致
/// `content[].thinking ... must be passed back` 400。
/// 无状态补齐:缺失则插占位块,空内容补文本,`redacted_thinking` 转普通 thinking,
/// 并去掉无法通过上游校验的 signature。
fn replay_tool_thinking_history(body: &mut Value, rule_ids: &mut Vec<String>) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let mut changed = false;
    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let has_tool_use = content
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"));
        if !has_tool_use {
            continue;
        }

        let mut has_thinking = false;
        for block in content.iter_mut() {
            match block.get("type").and_then(Value::as_str) {
                Some("thinking") => {
                    let non_empty = block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.trim().is_empty());
                    if let Some(obj) = block.as_object_mut() {
                        changed |= obj.remove("signature").is_some();
                        if !non_empty {
                            obj.insert("thinking".to_string(), json!(THINKING_PLACEHOLDER));
                            changed = true;
                        }
                    }
                    has_thinking = true;
                }
                Some("redacted_thinking") => {
                    *block = json!({
                        "type": "thinking",
                        "thinking": REDACTED_THINKING_PLACEHOLDER
                    });
                    has_thinking = true;
                    changed = true;
                }
                _ => {}
            }
        }
        if !has_thinking {
            content.insert(
                0,
                json!({"type": "thinking", "thinking": THINKING_PLACEHOLDER}),
            );
            changed = true;
        }
    }
    if changed {
        append_rule(rule_ids, RULE_TOOL_THINKING_HISTORY_REPLAY);
    }
}

/// DeepSeek 原生执行 `web_search_20250305`,其 `server_tool_use` /
/// `web_search_tool_result` 历史块**应当保留**(降级成文本会教模型模仿扁平化的
/// 伪工具调用)。但 Claude Science 的 daemon 会把缺 `tool_use_id` 的
/// `web_search_tool_result` 落盘进会话历史,上游严格反序列化会因
/// `missing field tool_use_id` 拒收——这类畸形块必须修掉:能抽出文本就降级成文本,
/// 否则整块丢弃。
fn repair_malformed_server_tool_blocks(body: &mut Value, rule_ids: &mut Vec<String>) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let mut changed = false;
    for message in messages.iter_mut() {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut rewritten = Vec::with_capacity(content.len());
        for block in std::mem::take(content) {
            let kind = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let malformed = match kind {
                "web_search_tool_result"
                | "web_fetch_tool_result"
                | "code_execution_tool_result" => {
                    block.get("tool_use_id").and_then(Value::as_str).is_none()
                }
                "server_tool_use" => block.get("id").and_then(Value::as_str).is_none(),
                _ => false,
            };
            if !malformed {
                rewritten.push(block);
                continue;
            }
            changed = true;
            if let Some(text) = extract_block_text(&block) {
                rewritten.push(json!({"type": "text", "text": text}));
            }
        }
        if rewritten.is_empty() && changed {
            rewritten.push(json!({"type": "text", "text": OMITTED_ORPHAN_TOOL_RESULT}));
        }
        *content = rewritten;
    }
    if changed {
        append_rule(rule_ids, RULE_MALFORMED_SERVER_TOOL_REPAIR);
    }
}

fn extract_block_text(block: &Value) -> Option<String> {
    let content = block.get("content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => {
            let joined: Vec<String> = items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            joined.join("\n")
        }
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

/// 上游按 id 一对一配对 tool_use 与 tool_result,任何落单都会被拒。
/// 第一遍:未被应答的 tool_use 按**计数差额**补合成 error tool_result
/// (并行调用可能共用 id 或部分结果丢失,集合去重会漏补)。
/// 第二遍:前一条助手消息里找不到对应 tool_use 的 tool_result 降级为文本。
fn repair_orphan_tool_pairing(body: &mut Value, rule_ids: &mut Vec<String>) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let mut changed = false;

    let mut index = 0;
    while index < messages.len() {
        if messages[index].get("role").and_then(Value::as_str) != Some("assistant") {
            index += 1;
            continue;
        }
        let (counts, order) = tool_use_counts(&messages[index]);
        if order.is_empty() {
            index += 1;
            continue;
        }
        let next_is_user = messages
            .get(index + 1)
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("user");
        let answered = if next_is_user {
            tool_result_counts(&messages[index + 1])
        } else {
            std::collections::HashMap::new()
        };

        let mut synthetic = Vec::new();
        for id in &order {
            let needed = counts.get(id).copied().unwrap_or(0);
            let got = answered.get(id).copied().unwrap_or(0);
            for _ in 0..needed.saturating_sub(got) {
                synthetic.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "is_error": true,
                    "content": ORPHAN_TOOL_RESULT_PLACEHOLDER
                }));
            }
        }
        if synthetic.is_empty() {
            index += 1;
            continue;
        }

        if next_is_user {
            let message = &mut messages[index + 1];
            match message.get_mut("content") {
                Some(Value::Array(content)) => {
                    let mut merged = synthetic;
                    merged.append(content);
                    *content = merged;
                }
                Some(Value::String(text)) => {
                    let text = text.clone();
                    let mut merged = synthetic;
                    merged.push(json!({"type": "text", "text": text}));
                    message["content"] = json!(merged);
                }
                _ => message["content"] = json!(synthetic),
            }
        } else {
            messages.insert(index + 1, json!({"role": "user", "content": synthetic}));
        }
        changed = true;
        index += 1;
    }

    for index in 0..messages.len() {
        if messages[index].get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let previous_ids: std::collections::HashSet<String> = if index > 0
            && messages[index - 1].get("role").and_then(Value::as_str) == Some("assistant")
        {
            messages[index - 1]
                .get("content")
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool_use")
                        })
                        .filter_map(|block| {
                            block.get("id").and_then(Value::as_str).map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            std::collections::HashSet::new()
        };

        let Some(content) = messages[index]
            .get_mut("content")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        let mut message_changed = false;
        let mut rewritten = Vec::with_capacity(content.len());
        for block in std::mem::take(content) {
            let orphan = block.get("type").and_then(Value::as_str) == Some("tool_result")
                && !block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| previous_ids.contains(id));
            if orphan {
                if let Some(text) = extract_block_text(&block) {
                    rewritten.push(json!({"type": "text", "text": text}));
                }
                message_changed = true;
            } else {
                rewritten.push(block);
            }
        }
        if message_changed {
            // Anthropic 要求 content 数组非空。
            if rewritten.is_empty() {
                rewritten.push(json!({"type": "text", "text": OMITTED_ORPHAN_TOOL_RESULT}));
            }
            changed = true;
        }
        *content = rewritten;
    }

    if changed {
        append_rule(rule_ids, RULE_ORPHAN_TOOL_PAIRING_REPAIR);
    }
}

fn tool_use_counts(message: &Value) -> (std::collections::HashMap<String, usize>, Vec<String>) {
    let mut counts = std::collections::HashMap::new();
    let mut order = Vec::new();
    if let Some(blocks) = message.get("content").and_then(Value::as_array) {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if let Some(id) = block.get("id").and_then(Value::as_str) {
                let entry = counts.entry(id.to_string()).or_insert(0);
                *entry += 1;
                if *entry == 1 {
                    order.push(id.to_string());
                }
            }
        }
    }
    (counts, order)
}

fn tool_result_counts(message: &Value) -> std::collections::HashMap<String, usize> {
    let mut counts = std::collections::HashMap::new();
    if let Some(blocks) = message.get("content").and_then(Value::as_array) {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                *counts.entry(id.to_string()).or_insert(0) += 1;
            }
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalize(mut body: Value, model: &str) -> (Value, Vec<String>) {
        let mut rules = Vec::new();
        normalize_request(&mut body, model, &mut rules);
        (body, rules)
    }

    #[test]
    fn thinking_auto_becomes_adaptive() {
        let (body, rules) = normalize(
            json!({"thinking": {"type": "auto"}, "messages": []}),
            "deepseek-v4-pro",
        );
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(rules.contains(&RULE_THINKING_AUTO_ADAPTIVE.to_string()));
    }

    #[test]
    fn standard_thinking_types_pass_through() {
        for kind in ["enabled", "adaptive"] {
            let (body, rules) = normalize(
                json!({"thinking": {"type": kind}, "messages": []}),
                "deepseek-v4-pro",
            );
            assert_eq!(body["thinking"]["type"], kind);
            assert!(!rules.contains(&RULE_THINKING_AUTO_ADAPTIVE.to_string()));
        }
    }

    #[test]
    fn specified_tool_choice_disables_thinking_and_strips_effort() {
        let (body, rules) = normalize(
            json!({
                "thinking": {"type": "auto"},
                "tool_choice": {"type": "tool", "name": "classify"},
                "reasoning_effort": "high",
                "output_config": {"effort": "high"},
                "messages": []
            }),
            "deepseek-v4-pro",
        );
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("output_config").is_none());
        assert!(rules.contains(&RULE_TOOL_CHOICE_DISABLES_THINKING.to_string()));
    }

    #[test]
    fn specified_tool_choice_supersedes_the_auto_rewrite() {
        // auto + 指定工具:最终必须是 disabled(上游在 adaptive 下同样拒收指定工具),
        // 且不该记录那条被立刻覆盖、净效果为零的 auto→adaptive。
        let (body, rules) = normalize(
            json!({
                "thinking": {"type": "auto"},
                "tool_choice": {"type": "tool", "name": "classify"},
                "messages": []
            }),
            "deepseek-v4-pro",
        );
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(rules.contains(&RULE_TOOL_CHOICE_DISABLES_THINKING.to_string()));
        assert!(
            !rules.contains(&RULE_THINKING_AUTO_ADAPTIVE.to_string()),
            "被覆盖的改写不该出现在规则日志里"
        );
    }

    #[test]
    fn only_the_specified_form_costs_thinking() {
        // 实测:auto / any / none 在 thinking 开启时都返回 200 且带 thinking 块,
        // 对它们补偿等于白白关掉推理。
        for choice in [
            json!({"type": "auto"}),
            json!({"type": "any"}),
            json!({"type": "none"}),
        ] {
            let (body, rules) = normalize(
                json!({
                    "thinking": {"type": "enabled", "budget_tokens": 1024},
                    "tool_choice": choice.clone(),
                    "messages": []
                }),
                "deepseek-v4-pro",
            );
            assert_eq!(
                body["thinking"]["type"], "enabled",
                "tool_choice {choice} 不该牺牲思考"
            );
            assert!(!rules.contains(&RULE_TOOL_CHOICE_DISABLES_THINKING.to_string()));
        }
    }

    #[test]
    fn client_disabled_thinking_loses_effort_not_the_disable() {
        let (body, rules) = normalize(
            json!({
                "thinking": {"type": "disabled"},
                "output_config": {"effort": "medium", "keep": true},
                "messages": []
            }),
            "deepseek-v4-pro",
        );
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert_eq!(body["output_config"], json!({"keep": true}));
        assert!(rules.contains(&RULE_THINKING_DISABLED_STRIPS_EFFORT.to_string()));
    }

    #[test]
    fn assistant_tool_use_without_thinking_gets_placeholder() {
        let (body, rules) = normalize(
            json!({"messages": [{
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "bash", "input": {}}]
            }, {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]
            }]}),
            "deepseek-v4-pro",
        );
        assert_eq!(body["messages"][0]["content"][0]["type"], "thinking");
        assert_eq!(body["messages"][0]["content"][1]["type"], "tool_use");
        assert!(rules.contains(&RULE_TOOL_THINKING_HISTORY_REPLAY.to_string()));
    }

    #[test]
    fn redacted_thinking_and_signature_are_normalized() {
        let (body, _) = normalize(
            json!({"messages": [{
                "role": "assistant",
                "content": [
                    {"type": "redacted_thinking", "data": "opaque"},
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": {}}
                ]
            }]}),
            "deepseek-v4-pro",
        );
        assert_eq!(body["messages"][0]["content"][0]["type"], "thinking");
        assert_eq!(
            body["messages"][0]["content"][0]["thinking"],
            REDACTED_THINKING_PLACEHOLDER
        );

        let (body, _) = normalize(
            json!({"messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "real reasoning", "signature": "sig"},
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": {}}
                ]
            }]}),
            "deepseek-v4-pro",
        );
        assert_eq!(
            body["messages"][0]["content"][0]["thinking"],
            "real reasoning"
        );
        assert!(body["messages"][0]["content"][0].get("signature").is_none());
    }

    #[test]
    fn assistant_without_tool_use_keeps_history_untouched() {
        let original = json!({"messages": [{
            "role": "assistant",
            "content": [{"type": "text", "text": "plain answer"}]
        }]});
        let (body, rules) = normalize(original.clone(), "deepseek-v4-pro");
        assert_eq!(body["messages"], original["messages"]);
        assert!(rules.is_empty());
    }

    #[test]
    fn malformed_search_result_without_tool_use_id_is_repaired() {
        let (body, rules) = normalize(
            json!({"messages": [{
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "before"},
                    {"type": "web_search_tool_result", "content": [{"text": "found it"}]},
                    {"type": "web_search_tool_result", "content": []}
                ]
            }]}),
            "deepseek-v4-pro",
        );
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[1], json!({"type": "text", "text": "found it"}));
        assert!(rules.contains(&RULE_MALFORMED_SERVER_TOOL_REPAIR.to_string()));
    }

    #[test]
    fn well_formed_server_tool_blocks_are_preserved() {
        let history = json!([
            {"type": "server_tool_use", "id": "s1", "name": "web_search", "input": {"query": "q"}},
            {"type": "web_search_tool_result", "tool_use_id": "s1", "content": [{"text": "hit"}]}
        ]);
        let (body, rules) = normalize(
            json!({"messages": [{"role": "assistant", "content": history.clone()}]}),
            "deepseek-v4-pro",
        );
        assert_eq!(body["messages"][0]["content"], history);
        assert!(!rules.contains(&RULE_MALFORMED_SERVER_TOOL_REPAIR.to_string()));
    }

    #[test]
    fn parallel_tool_use_shortfall_is_filled_by_count() {
        let (body, rules) = normalize(
            json!({"messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": {}},
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": {}},
                    {"type": "tool_use", "id": "t2", "name": "bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                ]}
            ]}),
            "deepseek-v4-pro",
        );
        let results = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(
            results
                .iter()
                .filter(|b| b["is_error"] == json!(true))
                .count(),
            2
        );
        assert!(rules.contains(&RULE_ORPHAN_TOOL_PAIRING_REPAIR.to_string()));
    }

    #[test]
    fn orphan_tool_result_becomes_text() {
        let (body, rules) = normalize(
            json!({"messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "gone", "content": "stale output"}
                ]}
            ]}),
            "deepseek-v4-pro",
        );
        assert_eq!(
            body["messages"][0]["content"][0],
            json!({"type": "text", "text": "stale output"})
        );
        assert!(rules.contains(&RULE_ORPHAN_TOOL_PAIRING_REPAIR.to_string()));
    }

    #[test]
    fn matched_tool_pairs_are_left_alone() {
        let original = json!({"messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "why"},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
            ]}
        ]});
        let (body, rules) = normalize(original.clone(), "deepseek-v4-pro");
        assert_eq!(body["messages"], original["messages"]);
        assert!(!rules.contains(&RULE_ORPHAN_TOOL_PAIRING_REPAIR.to_string()));
    }

    #[test]
    fn max_tokens_clamped_per_model() {
        let (body, _) = normalize(
            json!({"max_tokens": 200000, "messages": []}),
            "deepseek-v4-pro",
        );
        assert_eq!(body["max_tokens"], 65_536);
        let (body, _) = normalize(
            json!({"max_tokens": 200000, "messages": []}),
            "deepseek-v4-flash",
        );
        assert_eq!(body["max_tokens"], 32_768);
        let (body, _) = normalize(
            json!({"max_tokens": 500, "messages": []}),
            "deepseek-v4-pro",
        );
        assert_eq!(body["max_tokens"], 500);
    }
}
