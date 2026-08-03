use serde_json::{json, Map, Value};

const RULE_PROVIDER_KIMI_RELAY_THINKING_ENABLED: &str = "provider.kimi.relay-thinking-enabled";
const RULE_TOOL_RELAY_INPUT_SCHEMA_NORMALIZE: &str = "tool.relay.input-schema-normalize";
const RULE_TOOL_KIMI_WEB_SEARCH_SERVER_TOOL_PRESERVE: &str =
    "tool.kimi.web_search.server-tool-preserve";
const RULE_TOOL_KIMI_UNSUPPORTED_SERVER_TOOL_FILTER: &str =
    "tool.kimi.unsupported-server-tool-filter";
const RULE_TOOL_DEEPSEEK_WEB_SEARCH_SERVER_TOOL_PRESERVE: &str =
    "tool.deepseek.web_search.server-tool-preserve";
const RULE_TOOL_DEEPSEEK_UNSUPPORTED_SERVER_TOOL_FILTER: &str =
    "tool.deepseek.unsupported-server-tool-filter";
const RULE_TOOL_UNKNOWN_SERVER_TOOL_PRESERVE: &str = "tool.anthropic.unknown-server-tool-preserve";
const RULE_HISTORY_KIMI_FAILED_TAIL_NORMALIZE: &str = "history.kimi.failed-tail-normalize";
const RULE_TOOL_SILICONFLOW_FORCED_NAMED_TO_ANY: &str = "tool.siliconflow.forced-named-to-any";
const SILICONFLOW_API_HOSTS: [&str; 2] = ["api.siliconflow.cn", "api.siliconflow.com"];
const MAX_KIMI_FRAME_BYTES: usize = 1024 * 1024;
const MAX_KIMI_THINKING_BLOCK_BYTES: usize = 2 * 1024 * 1024;
const MAX_KIMI_THINKING_BYTES: usize = 1024 * 1024;
const MAX_KIMI_SIGNATURE_BYTES: usize = 64 * 1024;
const MAX_KIMI_CONTENT_BLOCKS: usize = 4096;
const MAX_RELAY_HISTORY_MESSAGES: usize = 2048;
const MAX_RELAY_HISTORY_BLOCKS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicMetadata {
    pub target_model: String,
    pub rule_ids: Vec<String>,
    pub kimi_compatibility: bool,
    pub dropped_server_tools: usize,
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

pub fn is_kimi_relay(provider_contract_id: Option<&str>, target_model: &str) -> bool {
    match provider_contract_id {
        Some(contract_id) => contract_id == "kimi-anthropic-relay",
        None => target_model.to_ascii_lowercase().contains("kimi"),
    }
}

#[derive(Debug, Default)]
pub struct KimiServerToolFilter {
    buf: Vec<u8>,
    next_upstream_index: u64,
    next_output_index: u64,
    active_output_block: Option<(u64, u64)>,
    dropped_server_tools: usize,
    dropped_empty_thinking: usize,
    dropped_server_block: Option<u64>,
    thinking: Option<BufferedThinkingBlock>,
    seen_web_search_ids: std::collections::BTreeSet<String>,
    pending_web_search_ids: std::collections::BTreeSet<String>,
    has_terminal_text: bool,
    has_client_tool_use: bool,
    has_web_search_result: bool,
    stop_reason: Option<String>,
}

#[derive(Debug)]
struct BufferedThinkingBlock {
    index: u64,
    frames: Vec<(Option<String>, Value)>,
    buffered_bytes: usize,
    thinking_bytes: usize,
    signature: String,
    signature_structurally_valid: bool,
}

impl KimiServerToolFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((frame, sep_len, rest)) = split_frame(&self.buf) {
            let sep = self.buf[frame.len()..frame.len() + sep_len].to_vec();
            let rewritten = self.rewrite_frame(&frame, &sep)?;
            out.extend_from_slice(&rewritten);
            self.buf = rest;
        }
        if self.buf.len() > MAX_KIMI_FRAME_BYTES {
            return Err("Kimi SSE frame exceeds the bounded buffer".into());
        }
        Ok(out)
    }

    pub fn finalize(&mut self) -> Result<Vec<u8>, String> {
        if self.thinking.is_some() {
            return Err("Kimi thinking block ended before content_block_stop".into());
        }
        if self.dropped_server_block.is_some() {
            return Err("Kimi server tool block ended before content_block_stop".into());
        }
        if self.active_output_block.is_some() {
            return Err("Kimi content block ended before content_block_stop".into());
        }
        if !self.pending_web_search_ids.is_empty() {
            return Err("Kimi web search response ended before its tool result".into());
        }
        if self.buf.iter().all(u8::is_ascii_whitespace) {
            self.buf.clear();
            Ok(Vec::new())
        } else {
            Err("Kimi SSE stream ended with a partial frame".into())
        }
    }

    pub fn dropped(&self) -> usize {
        self.dropped_server_tools + self.dropped_empty_thinking
    }

    pub fn dropped_empty_thinking(&self) -> usize {
        self.dropped_empty_thinking
    }

    pub fn dropped_server_tools(&self) -> usize {
        self.dropped_server_tools
    }

    fn validate_block_start(&self, idx: u64) -> Result<(), String> {
        if idx != self.next_upstream_index || self.active_output_block.is_some() {
            return Err("Kimi content block start index is invalid".into());
        }
        if idx >= MAX_KIMI_CONTENT_BLOCKS as u64 {
            return Err("Kimi response has too many content blocks".into());
        }
        Ok(())
    }

    fn complete_upstream_block(&mut self) -> Result<(), String> {
        self.next_upstream_index = self
            .next_upstream_index
            .checked_add(1)
            .ok_or("Kimi content block index overflow")?;
        Ok(())
    }

    fn allocate_output_index(&mut self) -> Result<u64, String> {
        let mapped = self.next_output_index;
        self.next_output_index = self
            .next_output_index
            .checked_add(1)
            .ok_or("Kimi output block index overflow")?;
        Ok(mapped)
    }

    fn rewrite_frame(&mut self, frame: &[u8], sep: &[u8]) -> Result<Vec<u8>, String> {
        let (event, data) = event_and_data(frame);
        if data.is_empty() {
            if self.thinking.is_some() {
                return Err("Kimi thinking block contains an unsupported SSE frame".into());
            }
            return Ok(passthrough(frame, sep));
        }
        let Ok(mut obj) = serde_json::from_slice::<Value>(&data) else {
            if self.thinking.is_some() {
                return Err("Kimi thinking block contains invalid JSON".into());
            }
            return Ok(passthrough(frame, sep));
        };
        let Some(kind) = obj.get("type").and_then(Value::as_str) else {
            if self.thinking.is_some() {
                return Err("Kimi thinking block event type is missing".into());
            }
            return Ok(passthrough(frame, sep));
        };
        if event.as_deref().is_some_and(|event| event != kind) {
            return Err("Kimi SSE event and JSON type do not match".into());
        }

        if self.thinking.is_some() {
            return self.rewrite_thinking_frame(event, obj, frame.len() + sep.len());
        }
        if self.dropped_server_block.is_some() {
            return self.rewrite_dropped_server_frame(event, obj);
        }
        if self.active_output_block.is_some() {
            return self.rewrite_output_block_frame(event, obj);
        }

        if kind == "content_block_start" {
            let idx = obj
                .get("index")
                .and_then(Value::as_u64)
                .ok_or("Kimi content block start index is invalid")?;
            self.validate_block_start(idx)?;
            let block_type = obj
                .get("content_block")
                .and_then(Value::as_object)
                .and_then(|block| block.get("type"))
                .and_then(Value::as_str);
            if block_type == Some("server_tool_use") {
                let block = obj
                    .get("content_block")
                    .ok_or("Kimi web search server tool is invalid")?;
                if block.get("name").and_then(Value::as_str) != Some("web_search") {
                    self.dropped_server_block = Some(idx);
                    self.dropped_server_tools += 1;
                    return Ok(Vec::new());
                }
                let id = valid_web_search_id(block.get("id"))
                    .ok_or("Kimi web search server tool id is invalid")?;
                if block
                    .get("input")
                    .is_some_and(|input| !input.is_null() && !input.is_object())
                {
                    return Err("Kimi web search server tool input is invalid".into());
                }
                if !self.seen_web_search_ids.insert(id.to_string()) {
                    return Err("Kimi web search server tool id is duplicated".into());
                }
                self.pending_web_search_ids.insert(id.to_string());
            } else if block_type == Some("web_search_tool_result") {
                let block = obj
                    .get("content_block")
                    .ok_or("Kimi web search tool result is invalid")?;
                let id = valid_web_search_tool_result_id(block)
                    .ok_or("Kimi web search tool result is invalid")?;
                if !self.pending_web_search_ids.remove(id) {
                    // Kimi stream results may use a distinct ID; one pending search is unambiguous.
                    if self.pending_web_search_ids.len() != 1 {
                        return Err("Kimi web search tool result is orphaned".into());
                    }
                    self.pending_web_search_ids.clear();
                }
                self.has_web_search_result = true;
            }
            if block_type == Some("thinking") {
                let block = obj
                    .get("content_block")
                    .and_then(Value::as_object)
                    .ok_or("Kimi thinking block start is invalid")?;
                let thinking = optional_kimi_string(block, "thinking")?;
                let signature = optional_kimi_string(block, "signature")?;
                if signature.len() > MAX_KIMI_SIGNATURE_BYTES {
                    return Err("Kimi thinking signature is too large".into());
                }
                let thinking_bytes = thinking.len();
                let signature_structurally_valid = kimi_signature_fragment_is_valid(signature);
                let signature = signature.to_string();
                if thinking_bytes > MAX_KIMI_THINKING_BYTES
                    || frame.len().saturating_add(sep.len()) > MAX_KIMI_THINKING_BLOCK_BYTES
                {
                    return Err("Kimi thinking block exceeds the bounded buffer".into());
                }
                self.thinking = Some(BufferedThinkingBlock {
                    index: idx,
                    frames: vec![(event, obj)],
                    buffered_bytes: frame.len() + sep.len(),
                    thinking_bytes,
                    signature,
                    signature_structurally_valid,
                });
                return Ok(Vec::new());
            }
            if block_type == Some("text")
                && obj
                    .get("content_block")
                    .and_then(Value::as_object)
                    .and_then(|block| block.get("text"))
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
            {
                self.has_terminal_text = true;
            }
            if block_type == Some("tool_use") {
                self.has_client_tool_use = true;
            }
            if let Some(obj_map) = obj.as_object_mut() {
                let mapped = self.next_output_index;
                obj_map.insert("index".to_string(), Value::Number(mapped.into()));
                self.active_output_block = Some((idx, mapped));
            }
            return Ok(render_sse(event.as_deref(), &obj));
        }
        if kind == "message_delta" {
            self.stop_reason = obj
                .get("delta")
                .and_then(Value::as_object)
                .and_then(|delta| delta.get("stop_reason"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if kind == "error" {
            self.pending_web_search_ids.clear();
        }
        if kind == "message_stop" {
            if !self.pending_web_search_ids.is_empty() {
                return Err("Kimi web search response ended before its tool result".into());
            }
            if !kimi_terminal_is_safe(
                self.dropped_server_tools,
                self.stop_reason.as_deref(),
                self.has_terminal_text,
                self.has_client_tool_use,
                self.has_web_search_result,
            ) {
                return Err("Kimi server-tool response has no safe terminal answer".into());
            }
        }
        Ok(passthrough(frame, sep))
    }

    fn rewrite_output_block_frame(
        &mut self,
        event: Option<String>,
        mut obj: Value,
    ) -> Result<Vec<u8>, String> {
        let kind = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or("Kimi content block event type is missing")?
            .to_string();
        if kind == "error" {
            self.active_output_block = None;
            self.pending_web_search_ids.clear();
            return Ok(render_sse(event.as_deref(), &obj));
        }
        if kind == "ping" {
            return Ok(render_sse(event.as_deref(), &obj));
        }
        let (upstream, mapped) = self
            .active_output_block
            .ok_or("Kimi content block state is missing")?;
        if obj.get("index").and_then(Value::as_u64) != Some(upstream) {
            return Err("Kimi content block index changed".into());
        }
        if !matches!(kind.as_str(), "content_block_delta" | "content_block_stop") {
            return Err("Kimi content block ended before content_block_stop".into());
        }
        if kind == "content_block_delta" && obj.get("delta").and_then(Value::as_object).is_none() {
            return Err("Kimi content block delta is invalid".into());
        }
        if kind == "content_block_delta"
            && obj
                .get("delta")
                .and_then(Value::as_object)
                .is_some_and(|delta| {
                    delta.get("type").and_then(Value::as_str) == Some("text_delta")
                        && delta
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.trim().is_empty())
                })
        {
            self.has_terminal_text = true;
        }
        if let Some(obj_map) = obj.as_object_mut() {
            obj_map.insert("index".to_string(), Value::Number(mapped.into()));
        }
        if kind == "content_block_stop" {
            self.active_output_block = None;
            self.complete_upstream_block()?;
            let allocated = self.allocate_output_index()?;
            debug_assert_eq!(allocated, mapped);
        }
        Ok(render_sse(event.as_deref(), &obj))
    }

    fn rewrite_dropped_server_frame(
        &mut self,
        event: Option<String>,
        obj: Value,
    ) -> Result<Vec<u8>, String> {
        let kind = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or("Kimi server tool block event type is missing")?;
        if kind == "error" {
            self.dropped_server_block = None;
            self.pending_web_search_ids.clear();
            return Ok(render_sse(event.as_deref(), &obj));
        }
        if kind == "ping" {
            return Ok(render_sse(event.as_deref(), &obj));
        }
        let index = obj
            .get("index")
            .and_then(Value::as_u64)
            .ok_or("Kimi server tool block index is missing")?;
        if Some(index) != self.dropped_server_block {
            return Err("Kimi server tool block index changed".into());
        }
        match kind {
            "content_block_delta" => {
                if obj.get("delta").and_then(Value::as_object).is_none() {
                    return Err("Kimi server tool delta is invalid".into());
                }
                Ok(Vec::new())
            }
            "content_block_stop" => {
                self.dropped_server_block = None;
                self.complete_upstream_block()?;
                Ok(Vec::new())
            }
            _ => Err("Kimi server tool block ended before content_block_stop".into()),
        }
    }

    fn rewrite_thinking_frame(
        &mut self,
        event: Option<String>,
        obj: Value,
        frame_bytes: usize,
    ) -> Result<Vec<u8>, String> {
        let kind = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or("Kimi thinking block event type is missing")?;
        if kind == "error" {
            self.thinking = None;
            self.pending_web_search_ids.clear();
            return Ok(render_sse(event.as_deref(), &obj));
        }
        let thinking = self
            .thinking
            .as_mut()
            .ok_or("Kimi thinking block state is missing")?;
        thinking.buffered_bytes = thinking
            .buffered_bytes
            .checked_add(frame_bytes)
            .ok_or("Kimi thinking block exceeds the bounded buffer")?;
        if thinking.buffered_bytes > MAX_KIMI_THINKING_BLOCK_BYTES {
            return Err("Kimi thinking block exceeds the bounded buffer".into());
        }
        match kind {
            "ping" => {
                thinking.frames.push((event, obj));
                Ok(Vec::new())
            }
            "content_block_delta" => {
                if obj.get("index").and_then(Value::as_u64) != Some(thinking.index) {
                    return Err("Kimi thinking block index changed".into());
                }
                let delta = obj
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or("Kimi thinking delta is invalid")?;
                match delta.get("type").and_then(Value::as_str) {
                    Some("thinking_delta") => {
                        let part = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .ok_or("Kimi thinking delta content is invalid")?;
                        thinking.thinking_bytes = thinking
                            .thinking_bytes
                            .checked_add(part.len())
                            .ok_or("Kimi thinking content is too large")?;
                        if thinking.thinking_bytes > MAX_KIMI_THINKING_BYTES {
                            return Err("Kimi thinking content is too large".into());
                        }
                    }
                    Some("signature_delta") => {
                        let part = delta
                            .get("signature")
                            .and_then(Value::as_str)
                            .ok_or("Kimi thinking signature delta is invalid")?;
                        if thinking.signature.len().saturating_add(part.len())
                            > MAX_KIMI_SIGNATURE_BYTES
                        {
                            return Err("Kimi thinking signature is too large".into());
                        }
                        thinking.signature_structurally_valid &=
                            kimi_signature_fragment_is_valid(part);
                        thinking.signature.push_str(part);
                    }
                    _ => return Err("Kimi thinking delta type is unsupported".into()),
                }
                thinking.frames.push((event, obj));
                Ok(Vec::new())
            }
            "content_block_stop" => {
                if obj.get("index").and_then(Value::as_u64) != Some(thinking.index) {
                    return Err("Kimi thinking block index changed".into());
                }
                thinking.frames.push((event, obj));
                let mut thinking = self
                    .thinking
                    .take()
                    .ok_or("Kimi thinking block state is missing")?;
                self.complete_upstream_block()?;
                let has_valid_signature =
                    thinking.signature_structurally_valid && !thinking.signature.is_empty();
                if thinking.thinking_bytes == 0 && !has_valid_signature {
                    self.dropped_empty_thinking += 1;
                    let mut pings = Vec::new();
                    for (event, frame) in thinking.frames {
                        if frame.get("type").and_then(Value::as_str) == Some("ping") {
                            pings.extend_from_slice(&render_sse(event.as_deref(), &frame));
                        }
                    }
                    return Ok(pings);
                }
                if thinking.thinking_bytes > 0 && !has_valid_signature {
                    return Err("Kimi nonempty thinking has no valid signature".into());
                }
                let mapped = self.allocate_output_index()?;
                let mut out = Vec::new();
                for (event, mut frame) in thinking.frames.drain(..) {
                    if let Some(index) = frame.get_mut("index") {
                        *index = Value::Number(mapped.into());
                    }
                    out.extend_from_slice(&render_sse(event.as_deref(), &frame));
                }
                Ok(out)
            }
            _ => Err("Kimi thinking block ended before content_block_stop".into()),
        }
    }
}

fn optional_kimi_string<'a>(block: &'a Map<String, Value>, field: &str) -> Result<&'a str, String> {
    match block.get(field) {
        Some(Value::String(value)) => Ok(value),
        None => Ok(""),
        Some(_) => Err(format!("Kimi thinking block {field} is invalid")),
    }
}

fn kimi_signature_fragment_is_valid(signature: &str) -> bool {
    !signature
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
}

fn valid_web_search_id(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
}

fn valid_web_search_server_tool_id(block: &Value) -> Option<&str> {
    (block.get("type").and_then(Value::as_str) == Some("server_tool_use")
        && block.get("name").and_then(Value::as_str) == Some("web_search")
        && block.get("input").is_some_and(Value::is_object))
    .then(|| valid_web_search_id(block.get("id")))
    .flatten()
}

fn valid_web_search_tool_result_id(block: &Value) -> Option<&str> {
    (block.get("type").and_then(Value::as_str) == Some("web_search_tool_result")
        && block
            .get("content")
            .is_some_and(|content| !content.is_null()))
    .then(|| valid_web_search_id(block.get("tool_use_id")))
    .flatten()
}

fn kimi_terminal_is_safe(
    dropped_server_tools: usize,
    stop_reason: Option<&str>,
    has_terminal_text: bool,
    has_client_tool_use: bool,
    has_web_search_result: bool,
) -> bool {
    if dropped_server_tools > 0 {
        return stop_reason == Some("end_turn") && has_terminal_text && !has_client_tool_use;
    }
    match stop_reason {
        Some("tool_use") => has_client_tool_use,
        Some("end_turn") => (has_terminal_text || has_web_search_result) && !has_client_tool_use,
        Some("max_tokens" | "stop_sequence" | "refusal") => {
            has_terminal_text && !has_client_tool_use
        }
        _ => false,
    }
}

pub fn filter_kimi_nonstream_response(body: &[u8]) -> Result<Vec<u8>, String> {
    filter_kimi_nonstream_response_with_count(body).map(|(body, _)| body)
}

pub fn filter_kimi_nonstream_response_with_count(body: &[u8]) -> Result<(Vec<u8>, usize), String> {
    let mut response: Value = serde_json::from_slice(body)
        .map_err(|_| "Kimi nonstream response is not valid JSON".to_string())?;
    let stop_reason = response
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    let content = response
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .ok_or("Kimi nonstream response content is invalid")?;
    if content.len() > MAX_KIMI_CONTENT_BLOCKS {
        return Err("Kimi nonstream response has too many content blocks".into());
    }
    let mut kept = Vec::with_capacity(content.len());
    let mut dropped_server_tools = 0;
    let mut dropped_empty_thinking = 0;
    let mut seen_web_search_ids = std::collections::BTreeSet::new();
    let mut pending_web_search_ids = std::collections::BTreeSet::new();
    let mut has_web_search_result = false;
    for block in content.drain(..) {
        let object = block
            .as_object()
            .ok_or("Kimi nonstream content block is invalid")?;
        let block_type = object.get("type").and_then(Value::as_str);
        if block_type == Some("server_tool_use") {
            if block.get("name").and_then(Value::as_str) != Some("web_search") {
                dropped_server_tools += 1;
                continue;
            }
            let id = valid_web_search_server_tool_id(&block)
                .ok_or("Kimi web search server tool is invalid")?;
            if !seen_web_search_ids.insert(id.to_string()) {
                return Err("Kimi web search server tool id is duplicated".into());
            }
            pending_web_search_ids.insert(id.to_string());
            kept.push(block);
            continue;
        }
        if block_type == Some("web_search_tool_result") {
            let id = valid_web_search_tool_result_id(&block)
                .ok_or("Kimi web search tool result is invalid")?;
            if !pending_web_search_ids.remove(id) {
                return Err("Kimi web search tool result is orphaned".into());
            }
            has_web_search_result = true;
            kept.push(block);
            continue;
        }
        if block_type != Some("thinking") {
            kept.push(block);
            continue;
        }
        let thinking = optional_kimi_string(object, "thinking")?;
        let signature = optional_kimi_string(object, "signature")?;
        if thinking.len() > MAX_KIMI_THINKING_BYTES {
            return Err("Kimi thinking content is too large".into());
        }
        if signature.len() > MAX_KIMI_SIGNATURE_BYTES {
            return Err("Kimi thinking signature is too large".into());
        }
        let has_valid_signature =
            !signature.is_empty() && kimi_signature_fragment_is_valid(signature);
        if thinking.is_empty() && !has_valid_signature {
            dropped_empty_thinking += 1;
            continue;
        }
        if !thinking.is_empty() && !has_valid_signature {
            return Err("Kimi nonempty thinking has no valid signature".into());
        }
        kept.push(block);
    }
    if !pending_web_search_ids.is_empty() {
        return Err("Kimi web search response ended before its tool result".into());
    }
    *content = kept;
    let has_terminal_text = content.iter().any(|block| {
        block.get("type").and_then(Value::as_str) == Some("text")
            && block
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
    });
    let has_client_tool_use = content
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"));
    if !kimi_terminal_is_safe(
        dropped_server_tools,
        stop_reason.as_deref(),
        has_terminal_text,
        has_client_tool_use,
        has_web_search_result,
    ) {
        return Err("Kimi server-tool response has no safe terminal answer".into());
    }
    let body = serde_json::to_vec(&response)
        .map_err(|_| "Kimi nonstream response serialization failed".to_string())?;
    Ok((body, dropped_server_tools + dropped_empty_thinking))
}

fn split_frame(buf: &[u8]) -> Option<(Vec<u8>, usize, Vec<u8>)> {
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

fn event_and_data(frame: &[u8]) -> (Option<String>, Vec<u8>) {
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

fn render_sse(event: Option<&str>, obj: &Value) -> Vec<u8> {
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

fn passthrough(frame: &[u8], sep: &[u8]) -> Vec<u8> {
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

fn is_siliconflow_anthropic_endpoint(endpoint: &str) -> bool {
    let raw_authority = endpoint
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    if raw_authority.contains('@') {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = if let Some(without_dot) = host.strip_suffix('.') {
        if without_dot.ends_with('.') {
            return false;
        }
        without_dot
    } else {
        host
    };
    SILICONFLOW_API_HOSTS
        .iter()
        .any(|official| host.eq_ignore_ascii_case(official))
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

pub fn apply_anthropic_server_tool_policy(
    body: &mut Value,
    policy: AnthropicServerToolPolicy,
    rule_ids: &mut Vec<String>,
) -> usize {
    let mut dropped = 0;
    if body
        .as_object_mut()
        .is_some_and(|object| object.remove("mcp_servers").is_some())
    {
        dropped += 1;
        append_rule_id(
            rule_ids,
            match policy {
                AnthropicServerToolPolicy::Kimi => RULE_TOOL_KIMI_UNSUPPORTED_SERVER_TOOL_FILTER,
                AnthropicServerToolPolicy::DeepSeek => {
                    RULE_TOOL_DEEPSEEK_UNSUPPORTED_SERVER_TOOL_FILTER
                }
            },
        );
    }

    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        if dropped > 0 {
            append_rule_id(
                rule_ids,
                match policy {
                    AnthropicServerToolPolicy::Kimi => {
                        RULE_TOOL_KIMI_UNSUPPORTED_SERVER_TOOL_FILTER
                    }
                    AnthropicServerToolPolicy::DeepSeek => {
                        RULE_TOOL_DEEPSEEK_UNSUPPORTED_SERVER_TOOL_FILTER
                    }
                },
            );
        }
        degrade_missing_tool_choice(body);
        return dropped;
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
                append_rule_id(
                    rule_ids,
                    match policy {
                        AnthropicServerToolPolicy::Kimi => {
                            RULE_TOOL_KIMI_UNSUPPORTED_SERVER_TOOL_FILTER
                        }
                        AnthropicServerToolPolicy::DeepSeek => {
                            RULE_TOOL_DEEPSEEK_UNSUPPORTED_SERVER_TOOL_FILTER
                        }
                    },
                );
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
    dropped
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

fn filter_kimi_server_tools(
    body: &mut Value,
    kimi_compatibility: bool,
    rule_ids: &mut Vec<String>,
) -> usize {
    normalize_relay_tools(body, rule_ids);
    if !kimi_compatibility {
        return 0;
    }
    apply_anthropic_server_tool_policy(body, AnthropicServerToolPolicy::Kimi, rule_ids)
}

pub(crate) fn zero_information_kimi_block(block: &Value) -> bool {
    match block.get("type").and_then(Value::as_str) {
        Some("server_tool_use") => valid_web_search_server_tool_id(block).is_none(),
        Some("web_search_tool_result") => valid_web_search_tool_result_id(block).is_none(),
        Some("text") => block.get("text").and_then(Value::as_str) == Some(""),
        Some("thinking") => {
            block.get("thinking").and_then(Value::as_str) == Some("")
                && block
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .is_empty()
        }
        _ => false,
    }
}

fn message_has_no_information(message: &Value) -> bool {
    match message.get("content") {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.is_empty(),
        Some(Value::Array(blocks)) => blocks.is_empty(),
        _ => false,
    }
}

fn normalize_kimi_failed_history_tail(
    body: &mut Value,
    kimi_compatibility: bool,
    rule_ids: &mut Vec<String>,
) -> Result<(), String> {
    if !kimi_compatibility {
        return Ok(());
    }
    let messages = body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or("request body must be a JSON object with a 'messages' array")?;
    if messages.len() > MAX_RELAY_HISTORY_MESSAGES {
        return Err("Kimi history has too many messages".into());
    }
    let mut changed = false;
    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        if blocks.len() > MAX_RELAY_HISTORY_BLOCKS {
            return Err("Kimi history message has too many content blocks".into());
        }
        let before = blocks.len();
        blocks.retain(|block| !zero_information_kimi_block(block));
        changed |= blocks.len() != before;
    }

    // A failed Science turn can leave an empty assistant placeholder either at
    // the tail or immediately before the user's edited resend. Remove only that
    // zero-information placeholder; successful assistant turns are untouched.
    let trailing_empty_assistant = messages.last().is_some_and(|message| {
        message.get("role").and_then(Value::as_str) == Some("assistant")
            && message_has_no_information(message)
    });
    if trailing_empty_assistant {
        messages.pop();
        changed = true;
    }
    if messages.len() >= 2
        && messages
            .last()
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("user")
    {
        let index = messages.len() - 2;
        if messages[index].get("role").and_then(Value::as_str) == Some("assistant")
            && message_has_no_information(&messages[index])
        {
            messages.remove(index);
            changed = true;
        }
    }
    if changed && messages.is_empty() {
        return Err("Kimi history is empty after removing a failed placeholder".into());
    }
    if changed {
        append_rule_id(rule_ids, RULE_HISTORY_KIMI_FAILED_TAIL_NORMALIZE);
    }
    Ok(())
}

fn validate_relay_tool_history(body: &Value, kimi_compatibility: bool) -> Result<(), String> {
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
                    Some("server_tool_use") if kimi_compatibility => {
                        if valid_web_search_server_tool_id(block).is_none() {
                            return Err("Kimi history web search server tool is invalid".into());
                        }
                        continue;
                    }
                    Some("web_search_tool_result") if kimi_compatibility => {
                        if valid_web_search_tool_result_id(block).is_none() {
                            return Err("Kimi history web search tool result is invalid".into());
                        }
                        continue;
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
                    Some("server_tool_use" | "web_search_tool_result") if kimi_compatibility => {
                        return Err("Kimi web search history must have the assistant role".into())
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

fn apply_siliconflow_tool_choice_compat(
    body: &mut Value,
    upstream_url: &str,
    rule_ids: &mut Vec<String>,
) {
    if !is_siliconflow_anthropic_endpoint(upstream_url) {
        return;
    }
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| !tools.is_empty())
        .unwrap_or(false);
    if !has_tools {
        return;
    }
    let is_forced_named = body
        .get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        == Some("tool");
    if !is_forced_named {
        return;
    }
    body["tool_choice"] = json!({"type": "any"});
    append_rule_id(rule_ids, RULE_TOOL_SILICONFLOW_FORCED_NAMED_TO_ANY);
}

pub fn transform_relay_request(
    body: Value,
    target_model: &str,
    relay_thinking: Option<&str>,
    upstream_url: &str,
) -> Result<(Value, AnthropicMetadata), String> {
    transform_relay_request_for_contract(body, target_model, relay_thinking, upstream_url, None)
}

pub fn transform_relay_request_for_contract(
    mut body: Value,
    target_model: &str,
    relay_thinking: Option<&str>,
    upstream_url: &str,
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
    let kimi_compatibility = is_kimi_relay(provider_contract_id, &target_model);
    let mut rule_ids = Vec::new();
    if relay_thinking == Some("enabled") && kimi_compatibility {
        append_rule_id(&mut rule_ids, RULE_PROVIDER_KIMI_RELAY_THINKING_ENABLED);
    }
    obj.insert("model".to_string(), Value::String(target_model.clone()));
    normalize_kimi_failed_history_tail(&mut body, kimi_compatibility, &mut rule_ids)?;
    validate_relay_tool_history(&body, kimi_compatibility)?;
    normalize_relay_thinking(&mut body, relay_thinking);
    let dropped_server_tools =
        filter_kimi_server_tools(&mut body, kimi_compatibility, &mut rule_ids);
    apply_siliconflow_tool_choice_compat(&mut body, upstream_url, &mut rule_ids);
    Ok((
        body,
        AnthropicMetadata {
            target_model,
            rule_ids,
            kimi_compatibility,
            dropped_server_tools,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        filter_kimi_nonstream_response, filter_kimi_nonstream_response_with_count,
        is_siliconflow_anthropic_endpoint, transform_relay_request,
        transform_relay_request_for_contract, KimiServerToolFilter, MAX_KIMI_CONTENT_BLOCKS,
        MAX_KIMI_FRAME_BYTES,
    };
    use serde_json::{json, Value};

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../test/golden/relay_anthropic.json")).unwrap()
    }

    #[test]
    fn kimi_history_preserves_valid_web_search_and_removes_failed_placeholders() {
        let request = json!({
            "messages": [
                {"role": "user", "content": "round one"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "inspect", "signature": "opaque"},
                    {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "a"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "found"}
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "round one done"}]},
                {"role": "user", "content": "round two"},
                {"role": "assistant", "content": [
                    {"type": "server_tool_use", "name": "web_search"},
                    {"type": "web_search_tool_result", "content": []},
                    {"type": "thinking", "thinking": "", "signature": ""},
                    {"type": "text", "text": ""}
                ]},
                {"role": "user", "content": "round two edited and resent"}
            ]
        });
        let (mapped, metadata) = transform_relay_request(
            request,
            "kimi-k3",
            Some("enabled"),
            "https://example.invalid/v1/messages",
        )
        .unwrap();
        let messages = mapped["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 6);
        assert_eq!(messages[1]["content"][1]["id"], "toolu_1");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(messages[3]["content"][0]["text"], "round one done");
        assert_eq!(messages[5]["content"], "round two edited and resent");
        assert!(metadata
            .rule_ids
            .iter()
            .any(|rule| rule == super::RULE_HISTORY_KIMI_FAILED_TAIL_NORMALIZE));

        let history = json!([
            {"role": "user", "content": "search"},
            {"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {"query": "x"}},
                {"type": "web_search_tool_result", "tool_use_id": "srv_1", "content": []},
                {"type": "text", "text": "answer"}
            ]},
            {"role": "assistant", "content": [
                {"type": "web_search_tool_result", "tool_use_id": "srv_2", "content": []}
            ]},
            {"role": "user", "content": "continue"}
        ]);
        let (mapped, _) = transform_relay_request_for_contract(
            json!({"messages": history.clone()}),
            "k3",
            None,
            "",
            Some("kimi-anthropic-relay"),
        )
        .unwrap();
        assert_eq!(mapped["messages"], history);
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
                "https://example.invalid/v1/messages",
            )
            .is_err());
        }
    }

    #[test]
    fn siliconflow_endpoint_matching_is_exact_and_url_parsed() {
        for endpoint in [
            "https://api.siliconflow.cn",
            "https://API.SILICONFLOW.CN/v1/messages",
            "http://api.siliconflow.com./anthropic/v1/messages",
        ] {
            assert!(is_siliconflow_anthropic_endpoint(endpoint), "{endpoint}");
        }
        for endpoint in [
            "ftp://api.siliconflow.cn/v1/messages",
            "https://sub.api.siliconflow.cn/v1/messages",
            "https://api.siliconflow.cn.evil/v1/messages",
            "https://api.siliconflow.com.evil/v1/messages",
            "https://api.siliconflow.cn@evil.example/v1/messages",
            "https://evil@api.siliconflow.cn/v1/messages",
            "https://@api.siliconflow.cn/v1/messages",
            "https://:pass@api.siliconflow.cn/v1/messages",
            "https://user:@api.siliconflow.cn/v1/messages",
            "https://api.siliconflow.cn../v1/messages",
            "https://evil.example/api.siliconflow.cn/v1/messages",
            "not a url api.siliconflow.cn",
        ] {
            assert!(!is_siliconflow_anthropic_endpoint(endpoint), "{endpoint}");
        }
    }

    #[test]
    fn siliconflow_tool_choice_fixture_matrix_matches_python() {
        let fixture = fixture();
        let cases = fixture["siliconflow_tool_choice_cases"].as_array().unwrap();
        for case in cases {
            let (mapped, metadata) = transform_relay_request(
                case["request"].clone(),
                case["mapped"]["model"].as_str().unwrap(),
                None,
                case["endpoint"].as_str().unwrap(),
            )
            .unwrap();
            assert_eq!(mapped, case["mapped"], "{}", case["name"]);
            let expected_rules: Vec<String> =
                serde_json::from_value(case["rule_ids"].clone()).unwrap();
            assert_eq!(metadata.rule_ids, expected_rules, "{}", case["name"]);
        }
    }

    #[test]
    fn relay_snaps_bare_model_and_preserves_max_tokens() {
        let fixture = fixture();
        let (mapped, metadata) = transform_relay_request(
            fixture["plain_request"].clone(),
            fixture["plain_target_model"].as_str().unwrap(),
            None,
            "",
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
            transform_relay_request(fixture["force_request"].clone(), "MiniMax-M2", None, "")
                .unwrap();
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
            Some("enabled"),
            "",
        )
        .unwrap();
        assert_eq!(mapped, fixture["kimi_mapped"]);
        assert_eq!(metadata.target_model, fixture["kimi_target_model"]);
        assert_eq!(
            metadata.rule_ids,
            vec![
                "provider.kimi.relay-thinking-enabled".to_string(),
                "tool.relay.input-schema-normalize".to_string(),
            ]
        );
    }

    #[test]
    fn relay_kimi_preserves_typed_web_search_and_forced_choice() {
        let (mapped, metadata) = transform_relay_request(
            json!({
                "model": "claude-opus-4-8",
                "messages": [],
                "tool_choice": {"type": "tool", "name": "web_search"},
                "tools": [{"type": "web_search_20250305", "name": "web_search"}],
            }),
            "kimi-k2.7-code",
            None,
            "",
        )
        .unwrap();
        assert_eq!(
            mapped["tools"],
            json!([{"type": "web_search_20250305", "name": "web_search"}])
        );
        assert_eq!(
            mapped["tool_choice"],
            json!({"type": "tool", "name": "web_search"})
        );
        assert_eq!(metadata.dropped_server_tools, 0);
        assert_eq!(
            metadata.rule_ids,
            vec!["tool.kimi.web_search.server-tool-preserve".to_string()]
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
            "",
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
    fn kimi_contract_policy_preserves_typed_web_search_client_and_unknown_typed_tools() {
        let media = json!({
            "role": "user",
            "content": [{
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}
            }]
        });
        let (mapped, metadata) = transform_relay_request_for_contract(
            json!({
                "model": "claude-sonnet-5",
                "messages": [media.clone()],
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
                    {"type": "web_fetch_20260209", "name": "not_web_fetch", "vendor_option": true},
                    {"name": "web_search", "input_schema": {"type": "object"}},
                    {"name": "python", "input_schema": {"type": "object"}},
                    {"name": "bash", "input_schema": {"type": "object"}},
                    {"name": "r", "input_schema": {"type": "object"}},
                    {"name": "repl", "input_schema": {"type": "object"}},
                    {"name": "compute", "input_schema": {"type": "object"}}
                ]
            }),
            "k3-256k",
            None,
            "",
            Some("kimi-anthropic-relay"),
        )
        .unwrap();
        assert_eq!(
            mapped["tools"],
            json!([
                {"type": "web_search_20250305", "name": "web_search"},
                {"type": "vendor_server_tool_20990101", "vendor_option": true},
                {"type": "web_fetch_20260209", "name": "not_web_fetch", "vendor_option": true},
                {"name": "web_search", "input_schema": {"type": "object", "properties": {}}},
                {"name": "python", "input_schema": {"type": "object", "properties": {}}},
                {"name": "bash", "input_schema": {"type": "object", "properties": {}}},
                {"name": "r", "input_schema": {"type": "object", "properties": {}}},
                {"name": "repl", "input_schema": {"type": "object", "properties": {}}},
                {"name": "compute", "input_schema": {"type": "object", "properties": {}}}
            ])
        );
        assert_eq!(mapped["tool_choice"], json!({"type": "auto"}));
        assert_eq!(mapped["messages"][0], media);
        assert!(mapped.get("mcp_servers").is_none());
        assert!(metadata.kimi_compatibility);
        assert_eq!(metadata.dropped_server_tools, 6);
        assert_eq!(
            metadata.rule_ids,
            vec![
                "tool.relay.input-schema-normalize".to_string(),
                "tool.kimi.unsupported-server-tool-filter".to_string(),
                "tool.kimi.web_search.server-tool-preserve".to_string(),
                "tool.anthropic.unknown-server-tool-preserve".to_string(),
            ]
        );
    }

    #[test]
    fn kimi_contract_identity_covers_raw_k3_models_without_leaking_to_other_relays() {
        for model in ["k3", "k3-256k"] {
            let request = json!({
                "messages": [
                    {"role": "user", "content": "search"},
                    {"role": "assistant", "content": [
                        {"type": "server_tool_use", "name": "web_search"},
                        {"type": "web_search_tool_result", "content": []},
                        {"type": "text", "text": ""}
                    ]},
                    {"role": "user", "content": "continue"}
                ]
            });
            let (mapped, metadata) = transform_relay_request_for_contract(
                request.clone(),
                model,
                Some("enabled"),
                "",
                Some("kimi-anthropic-relay"),
            )
            .unwrap();
            assert!(metadata.kimi_compatibility, "{model}");
            assert_eq!(mapped["messages"].as_array().unwrap().len(), 2, "{model}");

            let (control, metadata) = transform_relay_request_for_contract(
                request,
                model,
                None,
                "",
                Some("custom-anthropic"),
            )
            .unwrap();
            assert!(!metadata.kimi_compatibility, "{model}");
            assert_eq!(control["messages"].as_array().unwrap().len(), 3, "{model}");
        }

        let (_, standalone) =
            transform_relay_request(json!({"messages": []}), "kimi-legacy", None, "").unwrap();
        assert!(standalone.kimi_compatibility);
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
                "",
                Some("kimi-anthropic-relay"),
            )
            .unwrap();
            assert!(mapped.get("tools").is_none());
            assert!(mapped.get("tool_choice").is_none());
            assert_eq!(metadata.dropped_server_tools, 2);
        }
    }

    #[test]
    fn kimi_stream_filter_preserves_valid_web_search_and_compacts_empty_thinking() {
        let sse = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv_other\",\"name\":\"web_fetch\",\"input\":{}}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv_1\",\"name\":\"web_search\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srv_result_1\",\"content\":[]}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":4,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":4}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":5,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":5,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":5}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let mut filter = KimiServerToolFilter::new();
        let midpoint = sse.len() / 2;
        let mut out = filter.feed(&sse.as_bytes()[..midpoint]).unwrap();
        out.extend(filter.feed(&sse.as_bytes()[midpoint..]).unwrap());
        out.extend(filter.finalize().unwrap());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("server_tool_use"));
        assert!(!text.contains("srv_other"));
        assert!(text.contains("web_search_tool_result"));
        assert!(text.contains("\"tool_use_id\":\"srv_result_1\""));
        assert!(!text.contains("\"type\":\"thinking\""));
        assert!(text.contains("\"index\":3"));
        assert!(text.contains("\"text\":\"OK\""));
        assert_eq!(filter.dropped(), 2);
        assert_eq!(filter.dropped_server_tools(), 1);
        assert_eq!(filter.dropped_empty_thinking(), 1);
    }

    #[test]
    fn kimi_stream_filter_rejects_envelope_only_and_nonterminal_server_tool_responses() {
        for stop_reason in ["pause_turn", "tool_use"] {
            let sse = format!(
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"server_tool_use\",\"id\":\"srv\",\"name\":\"web_search\",\"input\":{{}}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srv\",\"content\":[]}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":1}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":2,\"content_block\":{{\"type\":\"text\",\"text\":\"answer\"}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":2}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop_reason}\"}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
            );
            let mut filter = KimiServerToolFilter::new();
            assert_eq!(
                filter.feed(sse.as_bytes()).unwrap_err(),
                "Kimi server-tool response has no safe terminal answer"
            );
        }

        for (stop_reason, text) in [("end_turn", "   "), ("pause_turn", "answer")] {
            let sse = format!(
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"{text}\"}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop_reason}\"}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
            );
            let mut filter = KimiServerToolFilter::new();
            assert_eq!(
                filter.feed(sse.as_bytes()).unwrap_err(),
                "Kimi server-tool response has no safe terminal answer"
            );
        }

        let ordinary_tool = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"python\",\"input\":{}}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let mut filter = KimiServerToolFilter::new();
        assert!(!filter.feed(ordinary_tool.as_bytes()).unwrap().is_empty());
        assert!(filter.finalize().unwrap().is_empty());

        let server_only = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv\",\"name\":\"web_search\",\"input\":{}}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srv\",\"content\":[]}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let mut filter = KimiServerToolFilter::new();
        assert!(!filter.feed(server_only.as_bytes()).unwrap().is_empty());
        assert!(filter.finalize().unwrap().is_empty());

        let mixed_terminal = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv\",\"name\":\"web_search\",\"input\":{}}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srv\",\"content\":[]}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"python\",\"input\":{}}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"text\",\"text\":\"answer\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let mut filter = KimiServerToolFilter::new();
        assert_eq!(
            filter.feed(mixed_terminal.as_bytes()).unwrap_err(),
            "Kimi server-tool response has no safe terminal answer"
        );
    }

    #[test]
    fn kimi_stream_filter_preserves_signed_thinking_and_original_frame_order() {
        let sse = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"seed\",\"signature\":\"sig-a\"}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"thought\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"-tail\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        );
        let mut filter = KimiServerToolFilter::new();
        let mut out = Vec::new();
        for chunk in sse.as_bytes().chunks(13) {
            out.extend(filter.feed(chunk).unwrap());
        }
        out.extend(filter.finalize().unwrap());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"thinking\":\"seed\""));
        assert!(text.contains("\"thinking\":\"thought\""));
        assert!(text.contains("\"signature\":\"-tail\""));
        assert!(text.contains("\"index\":0"));
        assert!(text.contains("\"index\":1"));
        assert!(text.find("content_block_start").unwrap() < text.find("event: ping").unwrap());
        assert_eq!(filter.dropped(), 0);
    }

    #[test]
    fn kimi_stream_filter_drops_only_zero_information_thinking_and_fails_nonempty_unsigned() {
        let empty_invalid = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"bad signature\"}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        let mut filter = KimiServerToolFilter::new();
        let output = filter.feed(empty_invalid.as_bytes()).unwrap();
        assert!(!String::from_utf8_lossy(&output).contains("thinking"));
        assert!(String::from_utf8_lossy(&output).contains("event: ping"));
        assert_eq!(filter.dropped_empty_thinking(), 1);

        let unsigned = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"secret\",\"signature\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        let mut filter = KimiServerToolFilter::new();
        assert_eq!(
            filter.feed(unsigned.as_bytes()).unwrap_err(),
            "Kimi nonempty thinking has no valid signature"
        );
    }

    #[test]
    fn kimi_stream_filter_requires_web_search_blocks_to_close() {
        let start = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv\",\"name\":\"web_search\"}}\n\n";
        let mut missing_stop = KimiServerToolFilter::new();
        assert!(!missing_stop.feed(start.as_bytes()).unwrap().is_empty());
        assert_eq!(
            missing_stop.finalize().unwrap_err(),
            "Kimi content block ended before content_block_stop"
        );

        let mut wrong_index = KimiServerToolFilter::new();
        wrong_index.feed(start.as_bytes()).unwrap();
        assert_eq!(
            wrong_index
                .feed(b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":4}\n\n")
                .unwrap_err(),
            "Kimi content block index changed"
        );

        let mut early_terminal = KimiServerToolFilter::new();
        early_terminal.feed(start.as_bytes()).unwrap();
        assert_eq!(
            early_terminal
                .feed(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n")
                .unwrap_err(),
            "Kimi content block index changed"
        );

        let mut malformed_delta = KimiServerToolFilter::new();
        malformed_delta.feed(start.as_bytes()).unwrap();
        assert_eq!(
            malformed_delta
                .feed(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0}\n\n")
                .unwrap_err(),
            "Kimi content block delta is invalid"
        );

        let mut orphan = KimiServerToolFilter::new();
        assert_eq!(
            orphan
                .feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"missing\",\"content\":[]}}\n\n")
                .unwrap_err(),
            "Kimi web search tool result is orphaned"
        );

        let mut missing_result = KimiServerToolFilter::new();
        missing_result.feed(start.as_bytes()).unwrap();
        missing_result
            .feed(b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n")
            .unwrap();
        assert_eq!(
            missing_result.finalize().unwrap_err(),
            "Kimi web search response ended before its tool result"
        );

        let mut invalid_input = KimiServerToolFilter::new();
        assert_eq!(
            invalid_input
                .feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv\",\"name\":\"web_search\",\"input\":[]}}\n\n")
                .unwrap_err(),
            "Kimi web search server tool input is invalid"
        );

        let mut null_input = KimiServerToolFilter::new();
        assert!(!null_input
            .feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv\",\"name\":\"web_search\",\"input\":null}}\n\n")
            .unwrap()
            .is_empty());

        let mut ambiguous = KimiServerToolFilter::new();
        ambiguous
            .feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv_1\",\"name\":\"web_search\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv_2\",\"name\":\"web_search\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n")
            .unwrap();
        assert_eq!(
            ambiguous
                .feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"other\",\"content\":[]}}\n\n")
                .unwrap_err(),
            "Kimi web search tool result is orphaned"
        );
    }

    #[test]
    fn kimi_stream_filter_rejects_nonsequential_original_indexes_without_state_growth() {
        for invalid_start in [77_i64, -1] {
            let mut filter = KimiServerToolFilter::new();
            let frame = format!(
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{invalid_start},\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n"
            );
            assert_eq!(
                filter.feed(frame.as_bytes()).unwrap_err(),
                "Kimi content block start index is invalid"
            );
        }

        let mut filter = KimiServerToolFilter::new();
        filter
            .feed(
                b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            )
            .unwrap();
        assert_eq!(filter.next_upstream_index, 1);
        assert_eq!(filter.next_output_index, 1);
        assert!(filter.active_output_block.is_none());
        assert_eq!(
            filter
                .feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n")
                .unwrap_err(),
            "Kimi content block start index is invalid"
        );
    }

    #[test]
    fn kimi_stream_filter_bounds_incomplete_frames_without_bounding_the_whole_stream() {
        let frame = "event: ping\ndata: {\"type\":\"ping\"}\n\n";
        let complete = frame.repeat((MAX_KIMI_FRAME_BYTES / frame.len()) + 2);
        let mut filter = KimiServerToolFilter::new();
        let output_bytes = complete
            .as_bytes()
            .chunks(8192)
            .map(|chunk| filter.feed(chunk).unwrap().len())
            .sum::<usize>();
        assert_eq!(output_bytes, complete.len());
        assert!(filter.finalize().unwrap().is_empty());

        let mut filter = KimiServerToolFilter::new();
        let incomplete = vec![b'x'; MAX_KIMI_FRAME_BYTES + 1];
        assert_eq!(
            filter.feed(&incomplete).unwrap_err(),
            "Kimi SSE frame exceeds the bounded buffer"
        );

        let mut filter = KimiServerToolFilter::new();
        filter.next_upstream_index = MAX_KIMI_CONTENT_BLOCKS as u64;
        let frame = format!(
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{MAX_KIMI_CONTENT_BLOCKS},\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n"
        );
        assert_eq!(
            filter.feed(frame.as_bytes()).unwrap_err(),
            "Kimi response has too many content blocks"
        );
    }

    #[test]
    fn kimi_nonstream_filter_preserves_signed_thinking_and_rejects_unsigned_content() {
        let body = json!({
            "id": "msg",
            "type": "message",
            "content": [
                {"type": "thinking", "thinking": "", "signature": ""},
                {"type": "server_tool_use", "id": "srv_other", "name": "web_fetch", "input": {}},
                {"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {}},
                {"type": "web_search_tool_result", "tool_use_id": "srv_1", "content": []},
                {"type": "thinking", "thinking": "kept", "signature": "opaque"},
                {"type": "text", "text": "answer"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let (filtered, dropped) =
            filter_kimi_nonstream_response_with_count(&serde_json::to_vec(&body).unwrap()).unwrap();
        let parsed: Value = serde_json::from_slice(&filtered).unwrap();
        assert_eq!(dropped, 2);
        assert_eq!(parsed["content"].as_array().unwrap().len(), 4);
        assert_eq!(parsed["content"][0]["id"], "srv_1");
        assert_eq!(parsed["content"][1]["tool_use_id"], "srv_1");
        assert_eq!(parsed["content"][2]["thinking"], "kept");
        assert_eq!(parsed["stop_reason"], "end_turn");
        assert_eq!(parsed["usage"]["output_tokens"], 2);

        let invalid = json!({
            "content": [{"type": "thinking", "thinking": "secret", "signature": ""}]
        });
        assert_eq!(
            filter_kimi_nonstream_response(&serde_json::to_vec(&invalid).unwrap()).unwrap_err(),
            "Kimi nonempty thinking has no valid signature"
        );
    }

    #[test]
    fn kimi_nonstream_filter_rejects_envelope_only_and_nonterminal_server_tool_responses() {
        for (stop_reason, text) in [("pause_turn", "answer"), ("tool_use", "answer")] {
            let body = json!({
                "content": [
                    {"type": "server_tool_use", "id": "srv", "name": "web_search", "input": {}},
                    {"type": "web_search_tool_result", "tool_use_id": "srv", "content": []},
                    {"type": "text", "text": text}
                ],
                "stop_reason": stop_reason
            });
            assert_eq!(
                filter_kimi_nonstream_response(&serde_json::to_vec(&body).unwrap()).unwrap_err(),
                "Kimi server-tool response has no safe terminal answer"
            );
        }

        for body in [
            json!({"content": [], "stop_reason": "end_turn"}),
            json!({"content": [{"type": "text", "text": "   "}], "stop_reason": "end_turn"}),
            json!({"content": [{"type": "text", "text": "answer"}], "stop_reason": "pause_turn"}),
        ] {
            assert_eq!(
                filter_kimi_nonstream_response(&serde_json::to_vec(&body).unwrap()).unwrap_err(),
                "Kimi server-tool response has no safe terminal answer"
            );
        }

        let ordinary_tool = json!({
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "python", "input": {}}],
            "stop_reason": "tool_use"
        });
        assert!(
            filter_kimi_nonstream_response(&serde_json::to_vec(&ordinary_tool).unwrap()).is_ok()
        );

        let unrelated_server_tool = json!({
            "content": [
                {"type": "server_tool_use", "id": "srv_other", "name": "web_fetch", "input": {}},
                {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {}}
            ],
            "stop_reason": "tool_use"
        });
        assert!(filter_kimi_nonstream_response(
            &serde_json::to_vec(&unrelated_server_tool).unwrap()
        )
        .is_err());

        let server_only = json!({
            "content": [
                {"type": "server_tool_use", "id": "srv", "name": "web_search", "input": {}},
                {"type": "web_search_tool_result", "tool_use_id": "srv", "content": []}
            ],
            "stop_reason": "end_turn"
        });
        assert!(filter_kimi_nonstream_response(&serde_json::to_vec(&server_only).unwrap()).is_ok());

        for invalid in [
            json!({
                "content": [{"type": "web_search_tool_result", "tool_use_id": "missing", "content": []}],
                "stop_reason": "end_turn"
            }),
            json!({
                "content": [{"type": "server_tool_use", "id": "srv", "name": "web_search", "input": {}}],
                "stop_reason": "end_turn"
            }),
            json!({
                "content": [
                    {"type": "server_tool_use", "id": "srv", "name": "web_search", "input": {}},
                    {"type": "web_search_tool_result", "tool_use_id": "other", "content": []}
                ],
                "stop_reason": "end_turn"
            }),
        ] {
            assert!(
                filter_kimi_nonstream_response(&serde_json::to_vec(&invalid).unwrap()).is_err()
            );
        }

        let mixed_terminal = json!({
            "content": [
                {"type": "server_tool_use", "id": "srv", "name": "web_search", "input": {}},
                {"type": "web_search_tool_result", "tool_use_id": "srv", "content": []},
                {"type": "tool_use", "id": "toolu_1", "name": "python", "input": {}},
                {"type": "text", "text": "answer"}
            ],
            "stop_reason": "end_turn"
        });
        assert_eq!(
            filter_kimi_nonstream_response(&serde_json::to_vec(&mixed_terminal).unwrap())
                .unwrap_err(),
            "Kimi server-tool response has no safe terminal answer"
        );
    }
}
