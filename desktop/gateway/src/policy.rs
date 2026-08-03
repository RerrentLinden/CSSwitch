use serde_json::Value;

use crate::anthropic_compat::{apply_anthropic_server_tool_policy, AnthropicServerToolPolicy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepSeekMetadata {
    pub target_model: String,
    pub rule_ids: Vec<String>,
    pub dropped_server_tools: usize,
}

pub fn clamp_max_tokens(value: Option<u64>, model: &str) -> Option<u64> {
    let cap = match model {
        "deepseek-v4-pro" => 65_536,
        "deepseek-v4-flash" => 32_768,
        _ => 8_192,
    };
    value.map(|v| v.min(cap))
}

pub fn normalize_thinking(body: &mut Value) {
    let forcing = body
        .get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|tc| tc.get("type"))
        .and_then(Value::as_str)
        .map(|t| t == "any" || t == "tool")
        .unwrap_or(false);
    if forcing {
        body["thinking"] = serde_json::json!({"type": "disabled"});
    }
}

pub fn transform_request(body: Value, target_model: &str) -> Result<Vec<u8>, String> {
    transform_request_with_metadata(body, target_model).map(|(body, _)| body)
}

pub fn transform_request_with_metadata(
    mut body: Value,
    target_model: &str,
) -> Result<(Vec<u8>, DeepSeekMetadata), String> {
    let obj = body
        .as_object_mut()
        .ok_or("request body must be a JSON object with a 'messages' array")?;
    if !obj.get("messages").map(Value::is_array).unwrap_or(false) {
        return Err("request body must be a JSON object with a 'messages' array".to_string());
    }
    obj.insert("model".to_string(), Value::String(target_model.to_string()));
    if let Some(max_tokens) = obj.get("max_tokens").and_then(Value::as_u64) {
        obj.insert(
            "max_tokens".to_string(),
            Value::Number(serde_json::Number::from(
                clamp_max_tokens(Some(max_tokens), target_model).unwrap_or(max_tokens),
            )),
        );
    }
    let mut rule_ids = Vec::new();
    let dropped_server_tools = apply_anthropic_server_tool_policy(
        &mut body,
        AnthropicServerToolPolicy::DeepSeek,
        &mut rule_ids,
    );
    normalize_thinking(&mut body);
    let body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    Ok((
        body,
        DeepSeekMetadata {
            target_model: target_model.to_string(),
            rule_ids,
            dropped_server_tools,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::{clamp_max_tokens, transform_request, transform_request_with_metadata};
    use serde_json::json;

    #[test]
    fn clamps_deepseek_max_tokens() {
        assert_eq!(
            clamp_max_tokens(Some(100_000), "deepseek-v4-pro"),
            Some(65_536)
        );
        assert_eq!(clamp_max_tokens(Some(500), "deepseek-v4-pro"), Some(500));
    }

    #[test]
    fn transform_maps_model_and_normalizes_thinking() {
        let raw = json!({
            "model": "claude-opus-4-8",
            "max_tokens": 100000,
            "thinking": {"type": "auto"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let bytes = transform_request(raw, "deepseek-v4-pro").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["model"], "deepseek-v4-pro");
        assert_eq!(v["max_tokens"], 65536);
        assert_eq!(v["thinking"]["type"], "auto");
    }

    #[test]
    fn forced_tool_choice_disables_thinking() {
        let raw = json!({
            "model": "claude-opus-4-8",
            "tool_choice": {"type": "any"},
            "tools": [{"name": "python", "input_schema": {"type": "object"}}],
            "thinking": {"type": "auto"},
            "messages": []
        });
        let bytes = transform_request(raw, "deepseek-v4-pro").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["thinking"]["type"], "disabled");
    }

    #[test]
    fn deepseek_server_tool_policy_preserves_web_search_and_client_tools_only() {
        let media = json!({
            "role": "user",
            "content": [{
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}
            }]
        });
        let (bytes, metadata) = transform_request_with_metadata(
            json!({
                "model": "claude-opus-4-8",
                "messages": [media.clone()],
                "thinking": {"type": "auto"},
                "mcp_servers": [{"type": "url", "url": "https://example.invalid"}],
                "tool_choice": {"type": "tool", "name": "web_fetch"},
                "tools": [
                    {"type": "web_search_20250305", "name": "web_search"},
                    {"type": "web_fetch_20260209", "name": "web_fetch"},
                    {"type": "code_execution_20250825", "name": "code_execution"},
                    {"type": "mcp_toolset", "mcp_server_name": "pubmed"},
                    {"type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex"},
                    {"type": "advisor_20251001", "name": "advisor"},
                    {"type": "vendor_server_tool_20990101", "vendor_option": true},
                    {"name": "web_search", "input_schema": {"type": "object"}},
                    {"name": "python", "input_schema": {"type": "object"}},
                    {"name": "bash", "input_schema": {"type": "object"}},
                    {"name": "compute", "input_schema": {"type": "object"}}
                ]
            }),
            "deepseek-v4-pro",
        )
        .unwrap();
        let mapped: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            mapped["tools"],
            json!([
                {"type": "web_search_20250305", "name": "web_search"},
                {"type": "vendor_server_tool_20990101", "vendor_option": true},
                {"name": "web_search", "input_schema": {"type": "object"}},
                {"name": "python", "input_schema": {"type": "object"}},
                {"name": "bash", "input_schema": {"type": "object"}},
                {"name": "compute", "input_schema": {"type": "object"}}
            ])
        );
        assert_eq!(mapped["tool_choice"], json!({"type": "auto"}));
        assert_eq!(mapped["thinking"], json!({"type": "auto"}));
        assert_eq!(mapped["messages"][0], media);
        assert!(mapped.get("mcp_servers").is_none());
        assert_eq!(metadata.dropped_server_tools, 6);
        assert_eq!(
            metadata.rule_ids,
            vec![
                "tool.deepseek.unsupported-server-tool-filter".to_string(),
                "tool.deepseek.web_search.server-tool-preserve".to_string(),
                "tool.anthropic.unknown-server-tool-preserve".to_string(),
            ]
        );
    }

    #[test]
    fn deepseek_all_unsupported_tools_remove_every_tool_choice_shape() {
        for tool_choice in [
            json!({"type": "any"}),
            json!({"type": "auto"}),
            json!({"type": "tool", "name": "web_fetch"}),
        ] {
            let (bytes, metadata) = transform_request_with_metadata(
                json!({
                    "messages": [],
                    "thinking": {"type": "auto"},
                    "tools": [
                        {"type": "web_fetch_20260209", "name": "web_fetch"},
                        {"type": "code_execution_20250825", "name": "code_execution"}
                    ],
                    "tool_choice": tool_choice
                }),
                "deepseek-v4-pro",
            )
            .unwrap();
            let mapped: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(mapped.get("tools").is_none());
            assert!(mapped.get("tool_choice").is_none());
            assert_eq!(mapped["thinking"], json!({"type": "auto"}));
            assert_eq!(metadata.dropped_server_tools, 2);
        }
    }
}
