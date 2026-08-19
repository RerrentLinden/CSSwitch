use serde_json::{json, Map, Value};

const RULE_TOOL_RELAY_INPUT_SCHEMA_NORMALIZE: &str = "tool.relay.input-schema-normalize";
const RULE_TOOL_KIMI_UNSUPPORTED_SERVER_TOOL_FILTER: &str =
    "tool.kimi.unsupported-server-tool-filter";
const RULE_TOOL_KIMI_WEB_SEARCH_SERVER_TOOL_PRESERVE: &str =
    "tool.kimi.web_search.server-tool-preserve";
const RULE_TOOL_DEEPSEEK_WEB_SEARCH_SERVER_TOOL_PRESERVE: &str =
    "tool.deepseek.web_search.server-tool-preserve";
const RULE_TOOL_DEEPSEEK_UNSUPPORTED_SERVER_TOOL_FILTER: &str =
    "tool.deepseek.unsupported-server-tool-filter";
const RULE_TOOL_UNKNOWN_SERVER_TOOL_PRESERVE: &str = "tool.anthropic.unknown-server-tool-preserve";
const RULE_PROVIDER_KIMI_THINKING_UPSTREAM_DEFAULT: &str =
    "provider.kimi.thinking-upstream-default";
const RULE_PROVIDER_KIMI_SPECIFIED_TOOL_CHOICE_DISABLES_THINKING: &str =
    "provider.kimi.specified-tool-choice-disables-thinking";
const RULE_PROVIDER_KIMI_WEB_SEARCH_RESULT_PAIRING_REPAIR: &str =
    "provider.kimi.web-search-result-pairing-repair";
const RULE_PROVIDER_KIMI_DOCUMENT_PLACEHOLDER: &str = "provider.kimi.document-block-placeholder";
const RULE_PROVIDER_KIMI_SCIENCE_CONTEXT_TAIL_REORDER: &str =
    "provider.kimi.science-context-tail-reorder";
const MAX_RELAY_HISTORY_BLOCKS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicMetadata {
    pub target_model: String,
    pub rule_ids: Vec<String>,
    pub flavor: RelayFlavor,
    pub dropped_server_tools: usize,
}

/// Anthropic relay contracts that need provider-specific compensation.
/// Kimi's open platform and its coding subscription share one contract: the
/// compensations are identical and only the endpoint and model catalog differ,
/// and both of those live on the template rather than the contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RelayFlavor {
    #[default]
    Generic,
    Kimi,
    DeepSeek,
}

impl RelayFlavor {
    pub fn detect(provider_contract_id: Option<&str>, target_model: &str) -> Self {
        match provider_contract_id {
            Some("kimi-anthropic-relay") => Self::Kimi,
            Some("deepseek-native") => Self::DeepSeek,
            Some(_) => Self::Generic,
            // Standalone gateways have no bound contract and fall back to the
            // historical model-name heuristic.
            None if target_model.to_ascii_lowercase().contains("kimi") => Self::Kimi,
            None if target_model.to_ascii_lowercase().contains("deepseek") => Self::DeepSeek,
            None => Self::Generic,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicServerToolPolicy {
    Kimi,
    DeepSeek,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnthropicServerToolKind {
    WebSearch,
    Unsupported,
    Unknown,
}

pub(crate) fn split_frame(buf: &[u8]) -> Option<(Vec<u8>, usize, Vec<u8>)> {
    let lf = buf.windows(2).position(|window| window == b"\n\n");
    let crlf = buf.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (None, None) => None,
        (Some(i), None) => Some((buf[..i].to_vec(), 2, buf[i + 2..].to_vec())),
        (None, Some(i)) => Some((buf[..i].to_vec(), 4, buf[i + 4..].to_vec())),
        (Some(lf_i), Some(crlf_i)) if lf_i <= crlf_i => {
            Some((buf[..lf_i].to_vec(), 2, buf[lf_i + 2..].to_vec()))
        }
        (Some(_), Some(crlf_i)) => Some((buf[..crlf_i].to_vec(), 4, buf[crlf_i + 4..].to_vec())),
    }
}

pub(crate) fn event_and_data(frame: &[u8]) -> (Option<String>, Vec<u8>) {
    let normalized = String::from_utf8_lossy(frame).replace("\r\n", "\n");
    let mut event = None;
    let mut data = Vec::new();
    for line in normalized.split('\n') {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.trim_start().as_bytes().to_vec());
        }
    }
    (event, data.join(b"\n".as_slice()))
}

pub(crate) fn render_sse(event: Option<&str>, obj: &Value) -> Vec<u8> {
    let data = serde_json::to_vec(obj).unwrap_or_else(|_| b"{}".to_vec());
    let mut out = Vec::new();
    if let Some(event) = event {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(event.as_bytes());
        out.extend_from_slice(b"\n");
    }
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(&data);
    out.extend_from_slice(b"\n\n");
    out
}

pub(crate) fn passthrough(frame: &[u8], sep: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + sep.len());
    out.extend_from_slice(frame);
    out.extend_from_slice(sep);
    out
}

fn append_rule_id(rule_ids: &mut Vec<String>, rule_id: &str) {
    if !rule_ids.iter().any(|existing| existing == rule_id) {
        rule_ids.push(rule_id.to_string());
    }
}

fn enabled_budget(max_tokens: Option<u64>) -> u64 {
    let default = 1024;
    match max_tokens {
        Some(value) if value > 0 => default.min(value.saturating_sub(1)).max(1),
        _ => default,
    }
}

fn is_forced_tool_choice(body: &Value) -> bool {
    body.get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        .map(|kind| kind == "any" || kind == "tool")
        .unwrap_or(false)
}

/// `tool_choice: {"type": "tool", ...}` — Anthropic's "specified" form.
/// Kimi for Coding rejects it whenever thinking is on, and its thinking is on by
/// default, so a request that merely omits `thinking` still fails.
fn is_specified_tool_choice(body: &Value) -> bool {
    body.get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        == Some("tool")
}

/// Hand thinking control back to the upstream default.
///
/// Claude Science sends a non-standard `{"type": "auto"}`. Kimi's k3 family does
/// not recognise it and silently stops thinking, while omitting the field
/// entirely leaves thinking on. Rewriting to `enabled` would work too, but it
/// drags in the thinking-continuity store that this upstream does not need —
/// history with stripped thinking blocks is accepted here even alongside
/// `tool_use`.
fn normalize_upstream_default_thinking(body: &mut Value, rule_ids: &mut Vec<String>) {
    if is_specified_tool_choice(body) {
        // Keep the forced tool — Science's internal classifier calls depend on
        // it — and give up thinking for that one request instead. This decides
        // the thinking field outright, so the auto/adaptive handling below
        // would have no net effect and is skipped rather than logged.
        body["thinking"] = json!({"type": "disabled"});
        append_rule_id(
            rule_ids,
            RULE_PROVIDER_KIMI_SPECIFIED_TOOL_CHOICE_DISABLES_THINKING,
        );
        return;
    }
    let declared = body
        .get("thinking")
        .and_then(Value::as_object)
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str);
    if matches!(declared, Some("auto") | Some("adaptive")) {
        if let Some(object) = body.as_object_mut() {
            object.remove("thinking");
        }
        append_rule_id(rule_ids, RULE_PROVIDER_KIMI_THINKING_UPSTREAM_DEFAULT);
    }
}

fn normalize_relay_thinking(body: &mut Value, relay_thinking: Option<&str>) {
    if relay_thinking == Some("enabled") {
        if is_forced_tool_choice(body) {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("tool_choice");
            }
        }
        let already_enabled = body
            .get("thinking")
            .and_then(Value::as_object)
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str)
            == Some("enabled");
        if !already_enabled {
            let budget = enabled_budget(body.get("max_tokens").and_then(Value::as_u64));
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        }
        return;
    }

    if body
        .get("thinking")
        .and_then(Value::as_object)
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        == Some("auto")
    {
        if let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut) {
            thinking.insert("type".to_string(), Value::String("adaptive".to_string()));
        }
    }
}

fn normalize_relay_input_schema(schema: Option<&Value>) -> Value {
    let Some(Value::Object(obj)) = schema else {
        return json!({"type": "object", "properties": {}});
    };
    if obj.is_empty() {
        return json!({"type": "object", "properties": {}});
    }
    let mut out = obj.clone();
    let has_properties = out.get("properties").map(Value::is_object).unwrap_or(false);
    match out.get("type").and_then(Value::as_str) {
        None if has_properties => {
            out.insert("type".to_string(), Value::String("object".to_string()));
        }
        Some("object") => {}
        _ => return json!({"type": "object", "properties": {}}),
    }
    if !out.get("properties").map(Value::is_object).unwrap_or(false) {
        out.insert("properties".to_string(), json!({}));
    }
    if out.get("required").map(Value::is_array) == Some(false) {
        out.remove("required");
    }
    Value::Object(out)
}

fn degrade_missing_tool_choice(body: &mut Value) {
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if !has_tools {
        if let Some(object) = body.as_object_mut() {
            object.remove("tool_choice");
        }
        return;
    }
    let Some(choice) = body.get("tool_choice").and_then(Value::as_object) else {
        return;
    };
    if choice.get("type").and_then(Value::as_str) != Some("tool") {
        return;
    }
    let choice_name = choice.get("name").and_then(Value::as_str).unwrap_or("");
    let exists = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|tool| tool.get("name").and_then(Value::as_str) == Some(choice_name));
    if !exists {
        body["tool_choice"] = json!({"type": "auto"});
    }
}

pub fn is_anthropic_client_tool(tool: &Value) -> bool {
    let typed = tool
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|tool_type| !tool_type.is_empty());
    !typed
        && tool
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.is_empty())
}

fn classify_anthropic_server_tool(tool: &Value) -> Option<AnthropicServerToolKind> {
    let tool_type = tool
        .get("type")
        .and_then(Value::as_str)
        .filter(|tool_type| !tool_type.is_empty())?;
    let name = tool.get("name").and_then(Value::as_str);
    let kind = match (tool_type, name) {
        (tool_type, Some("web_search")) if tool_type.starts_with("web_search_") => {
            AnthropicServerToolKind::WebSearch
        }
        (tool_type, Some("web_fetch")) if tool_type.starts_with("web_fetch_") => {
            AnthropicServerToolKind::Unsupported
        }
        (tool_type, Some("code_execution")) if tool_type.starts_with("code_execution_") => {
            AnthropicServerToolKind::Unsupported
        }
        ("mcp_toolset" | "mcp_servers", _) => AnthropicServerToolKind::Unsupported,
        (tool_type, Some(name))
            if tool_type.starts_with("tool_search_tool_")
                && name.starts_with("tool_search_tool_") =>
        {
            AnthropicServerToolKind::Unsupported
        }
        (tool_type, Some(name))
            if tool_type.starts_with("advisor_") && name.starts_with("advisor") =>
        {
            AnthropicServerToolKind::Unsupported
        }
        _ => AnthropicServerToolKind::Unknown,
    };
    Some(kind)
}

fn has_typed_web_search(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                classify_anthropic_server_tool(tool) == Some(AnthropicServerToolKind::WebSearch)
            })
        })
}

fn text_only_content(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return (!text.trim().is_empty()).then(|| text.to_string());
    }
    let blocks = content.as_array()?;
    if blocks.is_empty() {
        return None;
    }
    let mut text = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            return None;
        }
        let part = block.get("text").and_then(Value::as_str)?;
        text.push_str(part);
        text.push('\n');
    }
    (!text.trim().is_empty()).then_some(text)
}

fn is_science_compute_snapshot_message(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(text) = message.get("content").and_then(text_only_content) else {
        return false;
    };
    let text = text.to_ascii_lowercase();
    text.contains("compute snapshot")
        && contains_ascii_word(&text, "cores")
        && (contains_ascii_word(&text, "ram") || contains_gib_spec(&text))
}

fn contains_ascii_word(text: &str, word: &str) -> bool {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| token == word)
}

fn contains_gib_spec(text: &str) -> bool {
    let mut previous_was_size = false;
    for token in text
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
    {
        if (token == "gib" && previous_was_size)
            || token
                .strip_suffix("gib")
                .is_some_and(|size| !size.is_empty() && size.bytes().all(|ch| ch.is_ascii_digit()))
        {
            return true;
        }
        previous_was_size = token.bytes().all(|ch| ch.is_ascii_digit());
    }
    false
}

/// Science 把本机 compute snapshot 作为第二条 user message 追加在真实问题之后。
/// Kimi 原生搜索会把最后一条 user 当成搜索意图;仅在声明 typed web_search 且
/// 尾部形状与 live A/B 证据完全一致时交换两条消息,上下文内容本身不变。
fn reorder_science_context_before_user_intent(body: &mut Value, rule_ids: &mut Vec<String>) {
    if !has_typed_web_search(body) {
        return;
    }
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let len = messages.len();
    if len < 2
        || messages[len - 2].get("role").and_then(Value::as_str) != Some("user")
        || messages[len - 2]
            .get("content")
            .and_then(text_only_content)
            .is_none()
        || !is_science_compute_snapshot_message(&messages[len - 1])
        || is_science_compute_snapshot_message(&messages[len - 2])
    {
        return;
    }
    messages.swap(len - 2, len - 1);
    append_rule_id(rule_ids, RULE_PROVIDER_KIMI_SCIENCE_CONTEXT_TAIL_REORDER);
}

fn unsupported_server_tool_rule(policy: AnthropicServerToolPolicy) -> &'static str {
    match policy {
        AnthropicServerToolPolicy::Kimi => RULE_TOOL_KIMI_UNSUPPORTED_SERVER_TOOL_FILTER,
        AnthropicServerToolPolicy::DeepSeek => RULE_TOOL_DEEPSEEK_UNSUPPORTED_SERVER_TOOL_FILTER,
    }
}

/// Outcome of the server-tool policy pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerToolOutcome {
    pub dropped: usize,
}

pub fn apply_anthropic_server_tool_policy(
    body: &mut Value,
    policy: AnthropicServerToolPolicy,
    rule_ids: &mut Vec<String>,
) -> usize {
    apply_anthropic_server_tool_policy_with_outcome(body, policy, rule_ids).dropped
}

pub fn apply_anthropic_server_tool_policy_with_outcome(
    body: &mut Value,
    policy: AnthropicServerToolPolicy,
    rule_ids: &mut Vec<String>,
) -> ServerToolOutcome {
    let mut dropped = 0;
    if body
        .as_object_mut()
        .is_some_and(|object| object.remove("mcp_servers").is_some())
    {
        dropped += 1;
        append_rule_id(rule_ids, unsupported_server_tool_rule(policy));
    }

    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        if dropped > 0 {
            append_rule_id(rule_ids, unsupported_server_tool_rule(policy));
        }
        degrade_missing_tool_choice(body);
        return ServerToolOutcome { dropped };
    };
    let mut filtered = Vec::with_capacity(tools.len());
    for tool in tools {
        match classify_anthropic_server_tool(tool) {
            None => filtered.push(tool.clone()),
            Some(AnthropicServerToolKind::WebSearch) => match policy {
                AnthropicServerToolPolicy::Kimi => {
                    append_rule_id(rule_ids, RULE_TOOL_KIMI_WEB_SEARCH_SERVER_TOOL_PRESERVE);
                    filtered.push(tool.clone());
                }
                AnthropicServerToolPolicy::DeepSeek => {
                    append_rule_id(rule_ids, RULE_TOOL_DEEPSEEK_WEB_SEARCH_SERVER_TOOL_PRESERVE);
                    filtered.push(tool.clone());
                }
            },
            Some(AnthropicServerToolKind::Unsupported) => {
                dropped += 1;
                append_rule_id(rule_ids, unsupported_server_tool_rule(policy));
            }
            Some(AnthropicServerToolKind::Unknown) => {
                append_rule_id(rule_ids, RULE_TOOL_UNKNOWN_SERVER_TOOL_PRESERVE);
                filtered.push(tool.clone());
            }
        }
    }
    if filtered.is_empty() {
        if let Some(object) = body.as_object_mut() {
            object.remove("tools");
        }
    } else {
        body["tools"] = Value::Array(filtered);
    }
    degrade_missing_tool_choice(body);
    ServerToolOutcome { dropped }
}

fn normalize_relay_tools(body: &mut Value, rule_ids: &mut Vec<String>) {
    let Some(tools) = body.get("tools") else {
        return;
    };
    let Some(tool_items) = tools.as_array() else {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tools");
        }
        degrade_missing_tool_choice(body);
        return;
    };

    let mut normalized = Vec::new();
    let mut normalized_client_tool = false;
    for tool in tool_items {
        if !is_anthropic_client_tool(tool) {
            if classify_anthropic_server_tool(tool).is_some() {
                normalized.push(tool.clone());
            }
            continue;
        }
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let mut clean = match tool {
            Value::Object(obj) => obj.clone(),
            _ => Map::new(),
        };
        clean.insert(
            "input_schema".to_string(),
            normalize_relay_input_schema(tool.get("input_schema")),
        );
        normalized_client_tool = true;
        normalized.push(Value::Object(clean));
    }
    if normalized_client_tool {
        append_rule_id(rule_ids, RULE_TOOL_RELAY_INPUT_SCHEMA_NORMALIZE);
    }
    if normalized.is_empty() {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tools");
        }
    } else {
        body["tools"] = Value::Array(normalized);
    }
    degrade_missing_tool_choice(body);
}

/// Replace Anthropic `document` content blocks with a visible note.
///
/// Kimi for Coding's Anthropic surface does not implement the block at all: a
/// bare `{"type":"document"}` and an entirely made-up block type both fail with
/// the same opaque `Invalid request Error`, and every source form (base64, text,
/// url) fails alike. Because the block stays in the conversation history, one
/// attachment otherwise breaks every later turn of that session.
///
/// Dropping it costs nothing that was working: DeepSeek's endpoint accepts the
/// same block but answers `CANNOT_READ` with or without it, so the PDF was never
/// reaching a model this way. Science delivers real document content through its
/// file tools instead. The note keeps that degradation visible to both the model
/// and the reader rather than silently discarding an attachment.
fn replace_kimi_document_blocks(body: &mut Value, rule_ids: &mut Vec<String>) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut replaced = 0;
    for message in messages.iter_mut() {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in blocks.iter_mut() {
            if block.get("type").and_then(Value::as_str) != Some("document") {
                continue;
            }
            let media_type = block
                .pointer("/source/media_type")
                .and_then(Value::as_str)
                .unwrap_or("unknown media type")
                .to_string();
            // Name CSSwitch as the source. Unattributed, the model reads this as
            // an assertion from nowhere: one observed session quoted it, was
            // challenged on where it came from, and retracted a true statement
            // as its own invention. Then point at the path that does work —
            // image blocks are accepted upstream, so rendering the pages locally
            // still puts the contents in front of the model.
            *block = json!({
                "type": "text",
                "text": format!(
                    concat!(
                        "[CSSwitch] Attachment omitted ({}). This channel's upstream does not ",
                        "implement Anthropic document content blocks, so the file itself was not ",
                        "forwarded; this note is inserted by the CSSwitch gateway and is not a ",
                        "tool result. Image blocks are accepted — to see the contents, read the ",
                        "file from disk and render its pages to images."
                    ),
                    media_type
                )
            });
            replaced += 1;
        }
    }
    if replaced > 0 {
        append_rule_id(rule_ids, RULE_PROVIDER_KIMI_DOCUMENT_PLACEHOLDER);
    }
    replaced
}

/// Science 落盘搜索历史时只保留 `web_search_tool_result`,把与之配对的
/// `server_tool_use` 丢掉了(实测骨架:`m2/assistant:web_search_tool_result=srvtoolu_…`,
/// 同消息内没有任何 `server_tool_use`)。Kimi 的兼容层要求这一对能配上,
/// 于是回 `400 tool_call_id  is not found`——**一次搜索让此后每一轮都失败**。
///
/// 另有一种形态:两个块都在,但 Kimi 自己发出的 `server_tool_use.id`(`tool_…`)
/// 与 `web_search_tool_result.tool_use_id`(`srvtoolu_…`)本就不是同一个值。
///
/// 两种都靠"让这一对配得上"解决,2026-08-19 逐条实测:
/// - 孤儿结果块 → 400;在它前面补一个同 id 的 `server_tool_use` → 200。
/// - id 不匹配 → 400;任一方向对齐 → 200。
///
/// 选择补块而不是丢块:丢掉结果块同样能过(实测 200),但会丢失这一轮的搜索证据。
/// 诊断:`CSSWITCH_DEBUG_TOOL_SKELETON=1` 时打印历史的工具块骨架。
/// 只输出块类型与 id,不含任何对话内容。定位配对类 400 时这是唯一能看清形状的手段。
pub fn debug_tool_skeleton(body: &Value) {
    if std::env::var("CSSWITCH_DEBUG_TOOL_SKELETON")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return;
    };
    let mut out = Vec::new();
    for (mi, message) in messages.iter().enumerate() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("?");
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in content {
            let Some(kind) = block.get("type").and_then(Value::as_str) else {
                continue;
            };
            if !matches!(
                kind,
                "tool_use" | "tool_result" | "server_tool_use" | "web_search_tool_result"
            ) {
                continue;
            }
            let id = block
                .get("id")
                .or_else(|| block.get("tool_use_id"))
                .and_then(Value::as_str)
                .unwrap_or("<none>");
            out.push(format!("m{mi}/{role}:{kind}={id}"));
        }
    }
    if !out.is_empty() {
        crate::log_line!("tool skeleton: {}", out.join(" | "));
    }
}

fn repair_web_search_result_pairing(body: &mut Value, rule_ids: &mut Vec<String>) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let mut changed = false;
    let mut synthesized = 0usize;
    for message in messages.iter_mut() {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let declared: Vec<String> = content
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("server_tool_use"))
            .filter_map(|block| block.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();

        let mut rebuilt: Vec<Value> = Vec::with_capacity(content.len() + 1);
        let mut nearest: Option<String> = None;
        for block in content.iter() {
            match block.get("type").and_then(Value::as_str) {
                Some("server_tool_use") => {
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        nearest = Some(id.to_string());
                    }
                    rebuilt.push(block.clone());
                }
                Some("web_search_tool_result") => {
                    let current = block.get("tool_use_id").and_then(Value::as_str);
                    if current.is_some_and(|id| declared.iter().any(|d| d == id)) {
                        rebuilt.push(block.clone());
                        continue;
                    }
                    match nearest.clone() {
                        // 同消息内有 server_tool_use,但 id 对不上:改结果块的 id。
                        Some(target) => {
                            let mut fixed = block.clone();
                            fixed["tool_use_id"] = Value::String(target);
                            rebuilt.push(fixed);
                        }
                        // 完全没有 server_tool_use:补一个同 id 的,保住搜索证据。
                        None => match current {
                            Some(id) => {
                                rebuilt.push(json!({
                                    "type": "server_tool_use",
                                    "id": id,
                                    "name": "web_search",
                                    "input": {},
                                }));
                                rebuilt.push(block.clone());
                            }
                            // 连 tool_use_id 都没有的孤儿结果块。上游的幻影空搜索
                            // (无 id 空壳对)经 Science 落盘后正是这个形态,留在
                            // 历史里每一轮都 400 "tool_call_id is not found"
                            // (2026-08-19 真实会话复现)。空内容直接删,零证据损失;
                            // 有内容则合成配对键保住证据——键只是相关性标记,
                            // 上游自己的两半 id 本来也对不上。
                            None => {
                                let empty = block
                                    .get("content")
                                    .and_then(Value::as_array)
                                    .is_none_or(|content| content.is_empty());
                                if empty {
                                    changed = true;
                                    continue;
                                }
                                synthesized += 1;
                                let id = format!("srvtoolu_csswitch_repair_{synthesized}");
                                rebuilt.push(json!({
                                    "type": "server_tool_use",
                                    "id": id.clone(),
                                    "name": "web_search",
                                    "input": {},
                                }));
                                let mut fixed = block.clone();
                                fixed["tool_use_id"] = Value::String(id);
                                rebuilt.push(fixed);
                            }
                        },
                    }
                    changed = true;
                }
                _ => rebuilt.push(block.clone()),
            }
        }
        if changed {
            *content = rebuilt;
        }
    }
    if changed {
        append_rule_id(
            rule_ids,
            RULE_PROVIDER_KIMI_WEB_SEARCH_RESULT_PAIRING_REPAIR,
        );
    }
}

fn filter_relay_server_tools(
    body: &mut Value,
    flavor: RelayFlavor,
    rule_ids: &mut Vec<String>,
) -> ServerToolOutcome {
    normalize_relay_tools(body, rule_ids);
    let policy = match flavor {
        RelayFlavor::Kimi => AnthropicServerToolPolicy::Kimi,
        RelayFlavor::DeepSeek => AnthropicServerToolPolicy::DeepSeek,
        RelayFlavor::Generic => return ServerToolOutcome::default(),
    };
    apply_anthropic_server_tool_policy_with_outcome(body, policy, rule_ids)
}

fn validate_relay_tool_history(body: &Value) -> Result<(), String> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("request body must be a JSON object with a 'messages' array")?;
    let mut seen = std::collections::BTreeSet::new();
    let mut pending = std::collections::BTreeSet::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or("relay history message role is invalid")?;
        if !matches!(role, "user" | "assistant") {
            return Err("relay history message role is invalid".into());
        }
        let blocks = match message.get("content") {
            Some(Value::Array(blocks)) => blocks.as_slice(),
            Some(Value::String(_)) | Some(Value::Null) | None => &[],
            _ => return Err("relay history message content is invalid".into()),
        };
        if blocks.len() > MAX_RELAY_HISTORY_BLOCKS {
            return Err("relay history message has too many content blocks".into());
        }
        if role == "assistant" {
            if !pending.is_empty() {
                return Err(
                    "relay history has unresolved tool calls before an assistant turn".into(),
                );
            }
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        return Err("relay history tool_result must have the user role".into())
                    }
                    Some("tool_use") => {}
                    _ => continue,
                }
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty() && id.len() <= 256)
                    .ok_or("relay history tool_use id is invalid")?;
                block
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty() && name.len() <= 256)
                    .ok_or("relay history tool_use name is invalid")?;
                if !block.get("input").is_some_and(Value::is_object) {
                    return Err("relay history tool_use input is invalid".into());
                }
                if !seen.insert(id.to_string()) {
                    return Err("relay history contains a duplicate tool_use id".into());
                }
                pending.insert(id.to_string());
            }
        } else {
            let mut results = std::collections::BTreeSet::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        return Err("relay history tool_use must have the assistant role".into())
                    }
                    Some("tool_result") => {}
                    _ => continue,
                }
                let id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty() && id.len() <= 256)
                    .ok_or("relay history tool_result id is invalid")?;
                if !pending.contains(id) || !results.insert(id.to_string()) {
                    return Err("relay history contains an orphan or duplicate tool_result".into());
                }
            }
            if !pending.is_empty() {
                if results != pending {
                    return Err("relay history has incomplete tool results".into());
                }
                pending.clear();
            } else if !results.is_empty() {
                return Err("relay history contains an orphan tool_result".into());
            }
        }
    }
    if !pending.is_empty() {
        return Err("relay history ends with unresolved tool calls".into());
    }
    Ok(())
}

pub fn transform_relay_request(
    body: Value,
    target_model: &str,
    relay_thinking: Option<&str>,
) -> Result<(Value, AnthropicMetadata), String> {
    transform_relay_request_for_contract(body, target_model, relay_thinking, None)
}

pub fn transform_relay_request_for_contract(
    mut body: Value,
    target_model: &str,
    relay_thinking: Option<&str>,
    provider_contract_id: Option<&str>,
) -> Result<(Value, AnthropicMetadata), String> {
    let obj = body
        .as_object_mut()
        .ok_or("request body must be a JSON object with a 'messages' array")?;
    if !obj.get("messages").map(Value::is_array).unwrap_or(false) {
        return Err("request body must be a JSON object with a 'messages' array".to_string());
    }

    if target_model.is_empty() {
        return Err("resolved upstream model is required".into());
    }
    let target_model = target_model.to_string();
    let flavor = RelayFlavor::detect(provider_contract_id, &target_model);
    let mut rule_ids = Vec::new();
    obj.insert("model".to_string(), Value::String(target_model.clone()));
    if flavor == RelayFlavor::Kimi {
        reorder_science_context_before_user_intent(&mut body, &mut rule_ids);
        replace_kimi_document_blocks(&mut body, &mut rule_ids);
        debug_tool_skeleton(&body);
        repair_web_search_result_pairing(&mut body, &mut rule_ids);
    }
    validate_relay_tool_history(&body)?;
    // DeepSeek 的官方 /anthropic 端点自带一整套请求形态约束(thinking 取值、
    // tool_choice 与思考互斥、历史 thinking 回传、工具配对),由专属补偿链处理;
    // 通用 relay thinking 策略不适用。
    if flavor == RelayFlavor::DeepSeek {
        crate::deepseek_compat::normalize_request(&mut body, &target_model, &mut rule_ids);
    } else if relay_thinking == Some("upstream_default") {
        normalize_upstream_default_thinking(&mut body, &mut rule_ids);
    } else {
        normalize_relay_thinking(&mut body, relay_thinking);
    }
    let server_tools = filter_relay_server_tools(&mut body, flavor, &mut rule_ids);
    Ok((
        body,
        AnthropicMetadata {
            target_model,
            rule_ids,
            flavor,
            dropped_server_tools: server_tools.dropped,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        transform_relay_request, transform_relay_request_for_contract, AnthropicMetadata,
        RelayFlavor,
    };
    use serde_json::{json, Value};

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../test/golden/relay_anthropic.json")).unwrap()
    }

    #[test]
    fn relay_history_rejects_orphan_duplicate_and_unresolved_tools() {
        for messages in [
            json!([{"role": "user", "content": [{"type": "tool_result", "tool_use_id": "missing", "content": "x"}]}]),
            json!([{"role": "user", "content": [{"type": "tool_use", "id": "wrong_role", "name": "lookup", "input": {}}]}]),
            json!([{"role": "assistant", "content": [{"type": "tool_result", "tool_use_id": "wrong_role", "content": "x"}]}]),
            json!([{"role": "assistant", "content": [{"type": "tool_use", "id": "missing_name", "input": {}}]}]),
            json!([{"role": "assistant", "content": [{"type": "tool_use", "id": "missing_input", "name": "lookup"}]}]),
            json!([
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "dup", "name": "a", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "dup", "content": "x"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "dup", "name": "b", "input": {}}]},
            ]),
            json!([
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "pending", "name": "a", "input": {}}]},
            ]),
        ] {
            assert!(transform_relay_request(
                json!({"messages": messages}),
                "kimi-k3",
                Some("enabled"),
            )
            .is_err());
        }
    }

    #[test]
    fn relay_snaps_bare_model_and_preserves_max_tokens() {
        let fixture = fixture();
        let (mapped, metadata) = transform_relay_request(
            fixture["plain_request"].clone(),
            fixture["plain_target_model"].as_str().unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(mapped, fixture["plain_mapped"]);
        assert_eq!(metadata.target_model, fixture["plain_target_model"]);
        assert_eq!(metadata.rule_ids, Vec::<String>::new());
    }

    #[test]
    fn relay_force_model_overrides_shell() {
        let fixture = fixture();
        let (mapped, metadata) =
            transform_relay_request(fixture["force_request"].clone(), "k3", None).unwrap();
        assert_eq!(mapped, fixture["force_mapped"]);
        assert_eq!(metadata.target_model, fixture["force_target_model"]);
        assert_eq!(metadata.rule_ids, Vec::<String>::new());
    }

    #[test]
    fn relay_kimi_thinking_and_tool_quirks_match_python_fixture() {
        let fixture = fixture();
        let (mapped, metadata) = transform_relay_request(
            fixture["kimi_request"].clone(),
            "kimi-k2.7-code",
            Some("upstream_default"),
        )
        .unwrap();
        assert_eq!(mapped, fixture["kimi_mapped"]);
        assert_eq!(metadata.target_model, fixture["kimi_target_model"]);
        assert_eq!(
            metadata.rule_ids,
            vec![
                "provider.kimi.specified-tool-choice-disables-thinking".to_string(),
                "tool.relay.input-schema-normalize".to_string(),
            ]
        );
    }

    /// Both Kimi templates resolve to one contract: the open platform and the
    /// coding endpoint share the same compensations, and native web_search
    /// server tools pass through to either upstream unchanged.
    fn kimi(body: Value) -> (Value, AnthropicMetadata) {
        transform_relay_request_for_contract(
            body,
            "kimi-for-coding",
            Some("upstream_default"),
            Some("kimi-anthropic-relay"),
        )
        .unwrap()
    }

    /// The same request through the open-platform endpoint must come out
    /// identical: one contract, one set of compensations, only the URL differs.
    fn kimi_open_platform(body: Value) -> (Value, AnthropicMetadata) {
        transform_relay_request_for_contract(
            body,
            "kimi-for-coding",
            Some("upstream_default"),
            Some("kimi-anthropic-relay"),
        )
        .unwrap()
    }

    #[test]
    fn kimi_moves_science_compute_context_before_the_real_search_intent() {
        // Live A/B 根因形状:Science 把本机上下文作为第二条 user 追加,
        // Kimi 因此搜索了最后的机器规格而不是第一条 Rust 问题。
        let intent = json!({"role": "user", "content": [
            {"type": "text", "text": "联网查询 Rust 最新稳定版与发布日期"}
        ]});
        let context = json!({"role": "user", "content": [
            {"type": "text", "text": "Compute Snapshot\nCPU: 10 cores\nRAM: 32GiB"}
        ]});
        let (mapped, metadata) = kimi(json!({
            "messages": [intent.clone(), context.clone()],
            "tools": [{"type": "web_search_20250305", "name": "web_search"}]
        }));

        assert_eq!(mapped["messages"], json!([context, intent]));
        assert!(metadata
            .rule_ids
            .contains(&"provider.kimi.science-context-tail-reorder".to_string()));

        // `GiB` 必须是带数字的容量规格；空格分隔的 `32 GiB` 同样命中。
        let intent = json!({"role": "user", "content": "查询 Rust"});
        let context = json!({
            "role": "user",
            "content": "Compute Snapshot\nCPU: 10 cores\nMemory: 32 GiB"
        });
        let (mapped, metadata) = kimi(json!({
            "messages": [intent.clone(), context.clone()],
            "tools": [{"type": "web_search_20250305", "name": "web_search"}]
        }));
        assert_eq!(mapped["messages"], json!([context, intent]));
        assert!(metadata
            .rule_ids
            .contains(&"provider.kimi.science-context-tail-reorder".to_string()));
    }

    #[test]
    fn science_context_reorder_does_not_escape_its_narrow_kimi_search_boundary() {
        let typed_search = json!([{"type": "web_search_20250305", "name": "web_search"}]);
        let context = json!({"role": "user", "content": [
            {"type": "text", "text": "COMPUTE SNAPSHOT\n10 CORES\n32GIB RAM"}
        ]});
        let kimi_untouched = [
            // 用户单独询问 compute snapshot:没有前一条真实意图,不交换。
            json!({"messages": [context.clone()], "tools": typed_search.clone()}),
            // 普通连续 user message 不凭角色形状猜测。
            json!({"messages": [
                {"role": "user", "content": "第一条"},
                {"role": "user", "content": "第二条"}
            ], "tools": typed_search.clone()}),
            // 空白占位不算真实用户意图。
            json!({"messages": [
                {"role": "user", "content": "   "}, context.clone()
            ], "tools": typed_search.clone()}),
            // 单词子串不算 RAM 规格(`program` 含 "ram"),不得宽匹配。
            json!({"messages": [
                {"role": "user", "content": "Rust"},
                {"role": "user", "content": "compute snapshot cores program"}
            ], "tools": typed_search.clone()}),
            // `cores` 也必须是独立词，裸 `GiB` 不是容量规格。
            json!({"messages": [
                {"role": "user", "content": "Rust"},
                {"role": "user", "content": "compute snapshot hardcores GiB"}
            ], "tools": typed_search.clone()}),
            // 末尾混入非 text 块时不是 text-only synthetic context。
            json!({"messages": [
                {"role": "user", "content": "Rust"},
                {"role": "user", "content": [
                    {"type": "text", "text": "Compute Snapshot 10 cores 32GiB RAM"},
                    {"type": "image", "source": {"type": "base64", "data": "x"}}
                ]}
            ], "tools": typed_search.clone()}),
            // 普通 client tool 不代表原生搜索轮。
            json!({"messages": [
                {"role": "user", "content": "Rust"}, context.clone()
            ], "tools": [{"name": "web_search", "input_schema": {"type": "object"}}]}),
            // 前一条 user 是工具结果时不能交换，否则会拆断 assistant/tool_result 配对。
            json!({"messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
                ]},
                context.clone()
            ], "tools": typed_search.clone()}),
        ];
        for body in kimi_untouched {
            let before = body["messages"].clone();
            let (mapped, metadata) = kimi(body);
            assert_eq!(mapped["messages"], before);
            assert!(!metadata
                .rule_ids
                .contains(&"provider.kimi.science-context-tail-reorder".to_string()));
        }

        for (contract, model) in [
            (Some("deepseek-native"), "deepseek-chat"),
            (Some("generic-anthropic"), "other-model"),
        ] {
            let body = json!({
                "messages": [
                    {"role": "user", "content": "Rust"}, context.clone()
                ],
                "tools": typed_search.clone()
            });
            let before = body["messages"].clone();
            let (mapped, metadata) = transform_relay_request_for_contract(
                body,
                model,
                Some("upstream_default"),
                contract,
            )
            .unwrap();
            assert_eq!(mapped["messages"], before);
            assert!(!metadata
                .rule_ids
                .contains(&"provider.kimi.science-context-tail-reorder".to_string()));
        }
    }

    #[test]
    fn both_kimi_endpoints_produce_identical_requests_and_rules() {
        let body = json!({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": [
                {"type": "document", "source": {"media_type": "application/pdf"}}
            ]}],
            "thinking": {"type": "auto"},
            "tools": [{"type": "web_search_20250305", "name": "web_search"}],
        });
        let (coding_mapped, coding_meta) = kimi(body.clone());
        let (open_mapped, open_meta) = kimi_open_platform(body);
        assert_eq!(coding_mapped, open_mapped);
        assert_eq!(coding_meta.rule_ids, open_meta.rule_ids);
        assert_eq!(coding_meta.flavor, open_meta.flavor);
    }

    #[test]
    fn kimi_drops_science_auto_thinking_so_the_upstream_default_applies() {
        // Upstream silently stops thinking on the non-standard `auto`, but keeps
        // thinking on when the field is absent.
        for declared in [json!({"type": "auto"}), json!({"type": "adaptive"})] {
            let (mapped, metadata) = kimi(json!({
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hi"}],
                "thinking": declared,
            }));
            assert!(mapped.get("thinking").is_none());
            assert!(metadata
                .rule_ids
                .contains(&"provider.kimi.thinking-upstream-default".to_string()));
        }
    }

    #[test]
    fn kimi_passes_standard_thinking_values_through_untouched() {
        for declared in [
            json!({"type": "enabled", "budget_tokens": 2048}),
            json!({"type": "disabled"}),
        ] {
            let (mapped, metadata) = kimi(json!({
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hi"}],
                "thinking": declared.clone(),
            }));
            assert_eq!(mapped["thinking"], declared);
            assert!(!metadata
                .rule_ids
                .contains(&"provider.kimi.thinking-upstream-default".to_string()));
        }
    }

    #[test]
    fn kimi_specified_tool_choice_disables_thinking_but_keeps_the_forced_tool() {
        // Science's work-item classifier forces one named tool and omits
        // `thinking`; upstream thinking defaults to on and rejects the pair.
        let (mapped, metadata) = kimi(json!({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": "classify"}],
            "tool_choice": {"type": "tool", "name": "create_work_item"},
            "tools": [{"name": "create_work_item", "input_schema": {"type": "object"}}],
        }));
        assert_eq!(mapped["thinking"], json!({"type": "disabled"}));
        assert_eq!(
            mapped["tool_choice"],
            json!({"type": "tool", "name": "create_work_item"})
        );
        assert!(metadata
            .rule_ids
            .contains(&"provider.kimi.specified-tool-choice-disables-thinking".to_string()));
    }

    #[test]
    fn kimi_leaves_any_and_auto_tool_choice_thinking_alone() {
        // Only the "specified" form conflicts upstream; blanket-disabling
        // thinking for `any`/`auto` would give up thinking for no reason.
        for choice in [json!({"type": "any"}), json!({"type": "auto"})] {
            let (mapped, metadata) = kimi(json!({
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": "hi"}],
                "tool_choice": choice,
                "tools": [{"name": "python", "input_schema": {"type": "object"}}],
            }));
            assert!(mapped.get("thinking").is_none());
            assert!(!metadata
                .rule_ids
                .contains(&"provider.kimi.specified-tool-choice-disables-thinking".to_string()));
        }
    }

    #[test]
    fn kimi_aligns_the_mismatched_web_search_result_id() {
        // 真实形状:Kimi 自己发出的这两个 id 就是配不上的(tool_… 对 srvtoolu_…),
        // 原样回传会被它自己以 400 tool_call_id  is not found 拒收。
        let (body, meta) = kimi(json!({
            "messages": [
                {"role": "user", "content": "搜一下 CRISPR"},
                {"role": "assistant", "content": [
                    {"type": "server_tool_use", "id": "tool_lqvW0dv4wjst",
                     "name": "web_search", "input": {"query": "CRISPR"}},
                    {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_da2ngroj",
                     "content": [{"type": "web_search_result", "url": "https://example.com"}]},
                    {"type": "text", "text": "找到一条。"}
                ]},
                {"role": "user", "content": "再总结一句"}
            ]
        }));
        let blocks = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks[1]["tool_use_id"], "tool_lqvW0dv4wjst");
        assert!(meta
            .rule_ids
            .contains(&"provider.kimi.web-search-result-pairing-repair".to_string()));
    }

    #[test]
    fn kimi_leaves_a_matching_web_search_pair_alone() {
        // 已经配得上就不动,也不记规则——日志只记净效果。
        let (body, meta) = kimi(json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "srv_01", "name": "web_search", "input": {}},
                {"type": "web_search_tool_result", "tool_use_id": "srv_01", "content": []}
            ]}]
        }));
        assert_eq!(body["messages"][0]["content"][1]["tool_use_id"], "srv_01");
        assert!(!meta
            .rule_ids
            .contains(&"provider.kimi.web-search-result-pairing-repair".to_string()));
    }

    #[test]
    fn kimi_binds_each_result_to_the_nearest_preceding_server_tool_use() {
        // 一条消息里两次搜索:各自绑到自己前面那个,不能全绑到第一个。
        let (body, _) = kimi(json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "tool_A", "name": "web_search", "input": {}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_x", "content": []},
                {"type": "server_tool_use", "id": "tool_B", "name": "web_search", "input": {}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_y", "content": []}
            ]}]
        }));
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[1]["tool_use_id"], "tool_A");
        assert_eq!(blocks[3]["tool_use_id"], "tool_B");
    }

    #[test]
    fn kimi_synthesizes_the_server_tool_use_science_dropped() {
        // Science 落盘时只留结果块。实测骨架就是这个形状,补一个同 id 的
        // server_tool_use 之后上游接受(丢块也能过,但会丢搜索证据)。
        let (body, meta) = kimi(json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_da2nmt8d",
                 "content": [{"type": "web_search_result", "url": "https://example.com"}]},
                {"type": "text", "text": "找到一条。"}
            ]}]
        }));
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(
            blocks.len(),
            3,
            "补块后应有 server_tool_use + result + text"
        );
        assert_eq!(blocks[0]["type"], "server_tool_use");
        assert_eq!(blocks[0]["id"], "srvtoolu_da2nmt8d");
        assert_eq!(blocks[1]["type"], "web_search_tool_result");
        assert_eq!(blocks[1]["tool_use_id"], "srvtoolu_da2nmt8d");
        assert!(meta
            .rule_ids
            .contains(&"provider.kimi.web-search-result-pairing-repair".to_string()));
    }

    #[test]
    fn kimi_repairs_idless_orphan_results_by_content() {
        // 无 tool_use_id 的孤儿结果块留在历史里每一轮都 400
        // "tool_call_id is not found"(2026-08-19 真实会话:上游幻影空搜索
        // 经 Science 落盘后正是这个形态)。空内容直接删,零证据损失。
        let (body, meta) = kimi(json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "web_search_tool_result", "content": []},
                {"type": "text", "text": "答案"}
            ]}]
        }));
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert!(meta
            .rule_ids
            .contains(&"provider.kimi.web-search-result-pairing-repair".to_string()));

        // 有内容则合成配对键保住证据:键只是相关性标记,上游自己的
        // 两半 id 本来也对不上。
        let (body, meta) = kimi(json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "web_search_tool_result", "content": [
                    {"type": "web_search_result", "url": "https://example.test"}
                ]}
            ]}]
        }));
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "server_tool_use");
        let synthesized = content[0]["id"].as_str().unwrap();
        assert!(synthesized.starts_with("srvtoolu_csswitch_repair_"));
        assert_eq!(content[1]["tool_use_id"], synthesized);
        assert!(meta
            .rule_ids
            .contains(&"provider.kimi.web-search-result-pairing-repair".to_string()));
    }

    #[test]
    fn kimi_now_preserves_the_server_web_search_declaration() {
        // 桥接退役后 web_search 原样送上游,不再换成客户端工具。
        let (body, meta) = kimi(json!({
            "messages": [{"role": "user", "content": "搜一下"}],
            "tools": [{"type": "web_search_20250305", "name": "web_search"}]
        }));
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "web_search_20250305");
        assert_eq!(meta.dropped_server_tools, 0);
        assert!(meta
            .rule_ids
            .contains(&"tool.kimi.web_search.server-tool-preserve".to_string()));
    }

    #[test]
    fn kimi_replaces_document_blocks_with_a_visible_note() {
        // A single attachment anywhere in history otherwise fails every later
        // turn: upstream rejects the block type outright.
        let (mapped, metadata) = kimi(json!({
            "model": "claude-opus-4-8",
            "messages": [
                {"role": "user", "content": [
                    {"type": "document", "source": {
                        "type": "base64", "media_type": "application/pdf", "data": "JVBER"
                    }},
                    {"type": "text", "text": "what is this"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "a file"},
                    {"type": "tool_use", "id": "t1", "name": "read_file", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
                    {"type": "document", "source": {"type": "text", "media_type": "text/plain"}}
                ]}
            ]
        }));
        let blocks: Vec<&str> = mapped["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap())
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert!(!blocks.contains(&"document"));
        assert_eq!(blocks.iter().filter(|kind| **kind == "text").count(), 4);
        // The note names what was lost instead of silently dropping it.
        let note = mapped["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(note.contains("Attachment omitted"));
        assert!(note.contains("application/pdf"));
        // Attribution matters: an unattributed note gets read as an assertion
        // from nowhere and disbelieved, and the model needs the route that works.
        assert!(note.contains("CSSwitch"));
        assert!(note.contains("not a tool result"));
        assert!(note.contains("render its pages to images"));
        assert!(mapped["messages"][2]["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("text/plain"));
        assert!(metadata
            .rule_ids
            .contains(&"provider.kimi.document-block-placeholder".to_string()));
    }

    #[test]
    fn kimi_leaves_images_and_other_content_untouched() {
        // Images are accepted upstream; only the document block is unsupported.
        let image = json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "iVBOR"}
        });
        let (mapped, metadata) = kimi(json!({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": [image.clone(), {"type": "text", "text": "hi"}]}]
        }));
        assert_eq!(mapped["messages"][0]["content"][0], image);
        assert!(!metadata
            .rule_ids
            .contains(&"provider.kimi.document-block-placeholder".to_string()));
    }

    #[test]
    fn other_relay_contracts_keep_their_document_blocks() {
        let document = json!({
            "type": "document",
            "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBER"}
        });
        let (mapped, metadata) = transform_relay_request_for_contract(
            json!({
                "model": "claude-opus-4-8",
                "messages": [{"role": "user", "content": [document.clone()]}]
            }),
            "deepseek-v4-pro",
            Some("adaptive"),
            Some("deepseek-native"),
        )
        .unwrap();
        assert_eq!(mapped["messages"][0]["content"][0], document);
        assert!(metadata.rule_ids.is_empty());
    }

    #[test]
    fn relay_flavor_covers_both_kimi_endpoints_from_one_contract() {
        for model in ["kimi-k3", "kimi-for-coding"] {
            assert_eq!(
                RelayFlavor::detect(Some("kimi-anthropic-relay"), model),
                RelayFlavor::Kimi,
                "{model}"
            );
        }
        assert_eq!(
            RelayFlavor::detect(Some("custom-anthropic"), "kimi-for-coding"),
            RelayFlavor::Generic
        );
    }

    #[test]
    fn relay_server_tools_are_not_client_schema_normalized() {
        let (mapped, metadata) = transform_relay_request(
            json!({
                "model": "claude-sonnet-5",
                "messages": [],
                "tools": [
                    {"type": "web_search_20250305", "name": "web_search"},
                    {"type": "web_fetch_20260209", "name": "web_fetch", "max_uses": 2},
                    {"type": "mcp_toolset", "mcp_server_name": "pubmed"},
                    {"type": "vendor_server_tool_20990101", "vendor_option": true},
                    {"name": "lookup", "input_schema": {"properties": {"q": {"type": "string"}}}}
                ]
            }),
            "relay-model",
            None,
        )
        .unwrap();
        let tools = mapped["tools"].as_array().unwrap();
        assert_eq!(
            tools[0],
            json!({"type": "web_search_20250305", "name": "web_search"})
        );
        assert_eq!(
            tools[1],
            json!({"type": "web_fetch_20260209", "name": "web_fetch", "max_uses": 2})
        );
        assert!(tools[0].get("input_schema").is_none());
        assert!(tools[1].get("input_schema").is_none());
        assert_eq!(
            tools[2],
            json!({"type": "mcp_toolset", "mcp_server_name": "pubmed"})
        );
        assert!(tools[2].get("input_schema").is_none());
        assert_eq!(
            tools[3],
            json!({"type": "vendor_server_tool_20990101", "vendor_option": true})
        );
        assert!(tools[3].get("input_schema").is_none());
        assert_eq!(
            tools[4]["input_schema"],
            json!({"type": "object", "properties": {"q": {"type": "string"}}})
        );
        assert_eq!(
            metadata.rule_ids,
            vec!["tool.relay.input-schema-normalize".to_string()]
        );
    }

    #[test]
    fn kimi_contract_identity_covers_raw_k3_models_without_leaking_to_other_relays() {
        for model in ["k3", "k3-256k"] {
            let request = json!({
                "messages": [{"role": "user", "content": "search"}],
                "tools": [{"type": "web_search_20250305", "name": "web_search"}],
            });
            let (_, metadata) = transform_relay_request_for_contract(
                request.clone(),
                model,
                Some("upstream_default"),
                Some("kimi-anthropic-relay"),
            )
            .unwrap();
            assert_eq!(metadata.flavor, RelayFlavor::Kimi, "{model}");

            let (_, metadata) = transform_relay_request_for_contract(
                request,
                model,
                None,
                Some("custom-anthropic"),
            )
            .unwrap();
            assert_eq!(metadata.flavor, RelayFlavor::Generic, "{model}");
        }

        // Standalone gateways carry no contract and fall back to the model name.
        let (_, standalone) =
            transform_relay_request(json!({"messages": []}), "kimi-legacy", None).unwrap();
        assert_eq!(standalone.flavor, RelayFlavor::Kimi);
    }

    #[test]
    fn kimi_all_unsupported_tools_remove_every_tool_choice_shape() {
        for tool_choice in [
            json!({"type": "any"}),
            json!({"type": "auto"}),
            json!({"type": "tool", "name": "web_fetch"}),
        ] {
            let (mapped, metadata) = transform_relay_request_for_contract(
                json!({
                    "messages": [],
                    "tools": [
                        {"type": "web_fetch_20260209", "name": "web_fetch"},
                        {"type": "code_execution_20250825", "name": "code_execution"}
                    ],
                    "tool_choice": tool_choice
                }),
                "k3",
                None,
                Some("kimi-anthropic-relay"),
            )
            .unwrap();
            assert!(mapped.get("tools").is_none());
            assert!(mapped.get("tool_choice").is_none());
            assert_eq!(metadata.dropped_server_tools, 2);
        }
    }
}
