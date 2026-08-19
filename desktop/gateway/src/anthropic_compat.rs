use serde_json::{json, Map, Value};

const RULE_TOOL_RELAY_INPUT_SCHEMA_NORMALIZE: &str = "tool.relay.input-schema-normalize";
const RULE_TOOL_KIMI_UNSUPPORTED_SERVER_TOOL_FILTER: &str =
    "tool.kimi.unsupported-server-tool-filter";
const RULE_TOOL_DEEPSEEK_WEB_SEARCH_SERVER_TOOL_PRESERVE: &str =
    "tool.deepseek.web_search.server-tool-preserve";
const RULE_TOOL_DEEPSEEK_UNSUPPORTED_SERVER_TOOL_FILTER: &str =
    "tool.deepseek.unsupported-server-tool-filter";
const RULE_TOOL_UNKNOWN_SERVER_TOOL_PRESERVE: &str = "tool.anthropic.unknown-server-tool-preserve";
const RULE_PROVIDER_KIMI_THINKING_UPSTREAM_DEFAULT: &str =
    "provider.kimi.thinking-upstream-default";
const RULE_PROVIDER_KIMI_SPECIFIED_TOOL_CHOICE_DISABLES_THINKING: &str =
    "provider.kimi.specified-tool-choice-disables-thinking";
const RULE_TOOL_KIMI_WEB_SEARCH_CLIENT_TOOL_BRIDGE: &str =
    "tool.kimi.web_search.client-tool-bridge";
const RULE_PROVIDER_KIMI_DOCUMENT_PLACEHOLDER: &str = "provider.kimi.document-block-placeholder";
const RULE_TOOL_SILICONFLOW_FORCED_NAMED_TO_ANY: &str = "tool.siliconflow.forced-named-to-any";
const SILICONFLOW_API_HOSTS: [&str; 2] = ["api.siliconflow.cn", "api.siliconflow.com"];
const MAX_KIMI_FRAME_BYTES: usize = 1024 * 1024;
const MAX_KIMI_THINKING_BLOCK_BYTES: usize = 2 * 1024 * 1024;
const MAX_KIMI_THINKING_BYTES: usize = 1024 * 1024;
const MAX_KIMI_SIGNATURE_BYTES: usize = 64 * 1024;
const MAX_RELAY_HISTORY_BLOCKS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicMetadata {
    pub target_model: String,
    pub rule_ids: Vec<String>,
    pub flavor: RelayFlavor,
    pub dropped_server_tools: usize,
    /// The request carries the client-tool web_search bridge, so the response
    /// side must fulfil any resulting tool call before Science sees it.
    pub web_search_bridged: bool,
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

    /// Server-tool blocks must be stripped from Kimi responses: the upstream
    /// cannot receive them back without breaking a later turn.
    pub fn filters_server_tool_blocks(self) -> bool {
        matches!(self, Self::Kimi)
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
    has_terminal_text: bool,
    has_client_tool_use: bool,
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
            if matches!(
                block_type,
                Some("server_tool_use" | "web_search_tool_result")
            ) {
                self.dropped_server_block = Some(idx);
                self.dropped_server_tools += 1;
                return Ok(Vec::new());
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
        if kind == "message_stop"
            && !kimi_terminal_is_safe(
                self.dropped_server_tools,
                self.stop_reason.as_deref(),
                self.has_terminal_text,
                self.has_client_tool_use,
            )
        {
            return Err("Kimi server-tool response has no safe terminal answer".into());
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

fn kimi_terminal_is_safe(
    dropped_server_tools: usize,
    stop_reason: Option<&str>,
    has_terminal_text: bool,
    has_client_tool_use: bool,
) -> bool {
    if dropped_server_tools > 0 {
        return stop_reason == Some("end_turn") && has_terminal_text && !has_client_tool_use;
    }
    match stop_reason {
        Some("tool_use") => has_client_tool_use,
        Some("end_turn" | "max_tokens" | "stop_sequence" | "refusal") => {
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
    if content.len() > 4096 {
        return Err("Kimi nonstream response has too many content blocks".into());
    }
    let mut kept = Vec::with_capacity(content.len());
    let mut dropped_server_tools = 0;
    for block in content.drain(..) {
        let object = block
            .as_object()
            .ok_or("Kimi nonstream content block is invalid")?;
        let block_type = object.get("type").and_then(Value::as_str);
        if matches!(
            block_type,
            Some("server_tool_use" | "web_search_tool_result")
        ) {
            dropped_server_tools += 1;
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
            continue;
        }
        if !thinking.is_empty() && !has_valid_signature {
            return Err("Kimi nonempty thinking has no valid signature".into());
        }
        kept.push(block);
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
    ) {
        return Err("Kimi server-tool response has no safe terminal answer".into());
    }
    let body = serde_json::to_vec(&response)
        .map_err(|_| "Kimi nonstream response serialization failed".to_string())?;
    Ok((body, dropped_server_tools))
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
    if is_specified_tool_choice(body) {
        // Keep the forced tool — Science's internal classifier calls depend on
        // it — and give up thinking for that one request instead.
        body["thinking"] = json!({"type": "disabled"});
        append_rule_id(
            rule_ids,
            RULE_PROVIDER_KIMI_SPECIFIED_TOOL_CHOICE_DISABLES_THINKING,
        );
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
    /// A server web_search declaration was replaced by the client-tool bridge,
    /// so the response side must fulfil any resulting tool call.
    pub web_search_bridged: bool,
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
    let mut bridged = false;
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
        return ServerToolOutcome {
            dropped,
            web_search_bridged: bridged,
        };
    };
    let mut filtered = Vec::with_capacity(tools.len());
    for tool in tools {
        match classify_anthropic_server_tool(tool) {
            None => filtered.push(tool.clone()),
            Some(AnthropicServerToolKind::WebSearch) => match policy {
                AnthropicServerToolPolicy::Kimi => {
                    // Kimi answers 429 whenever this tool is declared and the
                    // model then decides not to search, which is most turns.
                    // Hand the model a client tool instead: asking for a search
                    // becomes an explicit tool call the gateway fulfils.
                    dropped += 1;
                    bridged = true;
                    append_rule_id(rule_ids, RULE_TOOL_KIMI_WEB_SEARCH_CLIENT_TOOL_BRIDGE);
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
    if bridged
        && !filtered.iter().any(|tool| {
            is_anthropic_client_tool(tool)
                && tool.get("name").and_then(Value::as_str)
                    == Some(crate::kimi_coding_search::BRIDGE_TOOL_NAME)
        })
    {
        filtered.push(crate::kimi_coding_search::bridge_tool_declaration());
    }
    if filtered.is_empty() {
        if let Some(object) = body.as_object_mut() {
            object.remove("tools");
        }
    } else {
        body["tools"] = Value::Array(filtered);
    }
    degrade_missing_tool_choice(body);
    ServerToolOutcome {
        dropped,
        web_search_bridged: bridged,
    }
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
    let flavor = RelayFlavor::detect(provider_contract_id, &target_model);
    let mut rule_ids = Vec::new();
    obj.insert("model".to_string(), Value::String(target_model.clone()));
    if flavor == RelayFlavor::Kimi {
        replace_kimi_document_blocks(&mut body, &mut rule_ids);
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
    apply_siliconflow_tool_choice_compat(&mut body, upstream_url, &mut rule_ids);
    Ok((
        body,
        AnthropicMetadata {
            target_model,
            rule_ids,
            flavor,
            dropped_server_tools: server_tools.dropped,
            web_search_bridged: server_tools.web_search_bridged,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        filter_kimi_nonstream_response, filter_kimi_nonstream_response_with_count,
        is_siliconflow_anthropic_endpoint, transform_relay_request,
        transform_relay_request_for_contract, AnthropicMetadata, KimiServerToolFilter, RelayFlavor,
        MAX_KIMI_FRAME_BYTES,
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
            Some("upstream_default"),
            "",
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

    /// Both Kimi templates resolve to one contract, so the open platform now
    /// bridges web_search exactly as the coding endpoint does instead of
    /// dropping the declaration.
    fn kimi(body: Value) -> (Value, AnthropicMetadata) {
        transform_relay_request_for_contract(
            body,
            "kimi-for-coding",
            Some("upstream_default"),
            "https://api.kimi.com/coding/v1/messages",
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
            "https://api.moonshot.cn/anthropic/v1/messages",
            Some("kimi-anthropic-relay"),
        )
        .unwrap()
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
        assert_eq!(coding_meta.web_search_bridged, open_meta.web_search_bridged);
        assert!(coding_meta.web_search_bridged);
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
    fn kimi_swaps_server_web_search_for_the_client_tool_bridge() {
        let (mapped, metadata) = kimi(json!({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"type": "web_search_20250305", "name": "web_search"},
                {"name": "bash", "input_schema": {"type": "object"}},
            ],
        }));
        let tools = mapped["tools"].as_array().unwrap();
        // The typed server declaration is gone — it is what trips the upstream 429.
        assert!(!tools.iter().any(|tool| tool.get("type").is_some()));
        // A same-named client tool takes its place so the model can still ask.
        let bridge = tools
            .iter()
            .find(|tool| tool["name"] == "web_search")
            .expect("client-tool bridge");
        assert_eq!(
            bridge["input_schema"]["properties"]["query"]["type"],
            "string"
        );
        assert!(tools.iter().any(|tool| tool["name"] == "bash"));
        assert!(metadata.web_search_bridged);
        assert!(metadata
            .rule_ids
            .contains(&"tool.kimi.web_search.client-tool-bridge".to_string()));
        // The bridge replaces the declaration; nothing is silently dropped.
        assert!(!metadata
            .rule_ids
            .contains(&"tool.kimi.unsupported-server-tool-filter".to_string()));
    }

    #[test]
    fn kimi_arms_no_bridge_when_science_declares_no_web_search() {
        let (mapped, metadata) = kimi(json!({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "bash", "input_schema": {"type": "object"}}],
        }));
        assert!(!metadata.web_search_bridged);
        assert_eq!(mapped["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn kimi_bridge_does_not_duplicate_an_existing_client_web_search() {
        let (mapped, metadata) = kimi(json!({
            "model": "claude-opus-4-8",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"type": "web_search_20250305", "name": "web_search"},
                {"name": "web_search", "input_schema": {"type": "object"}},
            ],
        }));
        let named: Vec<_> = mapped["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["name"] == "web_search")
            .collect();
        assert_eq!(named.len(), 1);
        assert!(metadata.web_search_bridged);
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
            "glm-5.2",
            Some("adaptive"),
            "https://open.bigmodel.cn/api/anthropic/v1/messages",
            Some("anthropic-relay"),
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
    fn kimi_contract_policy_bridges_web_search_and_preserves_client_and_unknown_typed_tools() {
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
        // Science already declared a client tool of the same name, so the bridge
        // arms without appending a duplicate declaration.
        assert!(metadata.web_search_bridged);
        assert_eq!(metadata.dropped_server_tools, 7);
        assert_eq!(
            metadata.rule_ids,
            vec![
                "tool.relay.input-schema-normalize".to_string(),
                "tool.kimi.unsupported-server-tool-filter".to_string(),
                "tool.kimi.web_search.client-tool-bridge".to_string(),
                "tool.anthropic.unknown-server-tool-preserve".to_string(),
            ]
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
                "",
                Some("kimi-anthropic-relay"),
            )
            .unwrap();
            assert_eq!(metadata.flavor, RelayFlavor::Kimi, "{model}");
            assert!(metadata.web_search_bridged, "{model}");

            let (_, metadata) = transform_relay_request_for_contract(
                request,
                model,
                None,
                "",
                Some("custom-anthropic"),
            )
            .unwrap();
            assert_eq!(metadata.flavor, RelayFlavor::Generic, "{model}");
            assert!(!metadata.web_search_bridged, "{model}");
        }

        // Standalone gateways carry no contract and fall back to the model name.
        let (_, standalone) =
            transform_relay_request(json!({"messages": []}), "kimi-legacy", None, "").unwrap();
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
    fn kimi_stream_filter_drops_server_tool_blocks_and_compacts_indexes() {
        let sse = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"server_tool_use\",\"name\":\"web_search\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"web_search_tool_result\",\"content\":[]}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":4,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":4,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":4}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let mut filter = KimiServerToolFilter::new();
        let midpoint = sse.len() / 2;
        let mut out = filter.feed(&sse.as_bytes()[..midpoint]).unwrap();
        out.extend(filter.feed(&sse.as_bytes()[midpoint..]).unwrap());
        out.extend(filter.finalize().unwrap());
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("server_tool_use"));
        assert!(!text.contains("web_search_tool_result"));
        assert!(!text.contains("\"type\":\"thinking\""));
        assert!(text.contains("\"index\":1"));
        assert!(text.contains("\"text\":\"OK\""));
        assert_eq!(filter.dropped(), 3);
        assert_eq!(filter.dropped_empty_thinking(), 1);
    }

    #[test]
    fn kimi_stream_filter_rejects_envelope_only_and_nonterminal_server_tool_responses() {
        for (stop_reason, with_text) in [
            ("end_turn", false),
            ("pause_turn", true),
            ("tool_use", true),
        ] {
            let text = if with_text {
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"answer\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n"
            } else {
                ""
            };
            let sse = format!(
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"server_tool_use\",\"name\":\"web_search\"}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n{text}event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop_reason}\"}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
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

        let mixed_terminal = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"name\":\"web_search\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"python\",\"input\":{}}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\",\"text\":\"answer\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
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
    fn kimi_stream_filter_requires_dropped_server_tool_blocks_to_close() {
        let start = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"name\":\"web_search\"}}\n\n";
        let mut missing_stop = KimiServerToolFilter::new();
        assert!(missing_stop.feed(start.as_bytes()).unwrap().is_empty());
        assert_eq!(
            missing_stop.finalize().unwrap_err(),
            "Kimi server tool block ended before content_block_stop"
        );

        let mut wrong_index = KimiServerToolFilter::new();
        wrong_index.feed(start.as_bytes()).unwrap();
        assert_eq!(
            wrong_index
                .feed(b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":4}\n\n")
                .unwrap_err(),
            "Kimi server tool block index changed"
        );

        let mut early_terminal = KimiServerToolFilter::new();
        early_terminal.feed(start.as_bytes()).unwrap();
        assert_eq!(
            early_terminal
                .feed(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n")
                .unwrap_err(),
            "Kimi server tool block index is missing"
        );

        let mut malformed_delta = KimiServerToolFilter::new();
        malformed_delta.feed(start.as_bytes()).unwrap();
        assert_eq!(
            malformed_delta
                .feed(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0}\n\n")
                .unwrap_err(),
            "Kimi server tool delta is invalid"
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
    }

    #[test]
    fn kimi_nonstream_filter_preserves_signed_thinking_and_rejects_unsigned_content() {
        let body = json!({
            "id": "msg",
            "type": "message",
            "content": [
                {"type": "thinking", "thinking": "", "signature": ""},
                {"type": "server_tool_use", "id": "srv_1", "name": "web_search"},
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
        assert_eq!(parsed["content"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["content"][0]["thinking"], "kept");
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
        for (stop_reason, text) in [
            ("end_turn", ""),
            ("pause_turn", "answer"),
            ("tool_use", "answer"),
        ] {
            let body = json!({
                "content": [
                    {"type": "server_tool_use", "id": "srv", "name": "web_search"},
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

        let mixed_terminal = json!({
            "content": [
                {"type": "server_tool_use", "id": "srv", "name": "web_search"},
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
