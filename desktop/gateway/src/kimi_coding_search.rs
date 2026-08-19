//! Web-search bridge for the Kimi for Coding channel.
//!
//! The upstream executes `web_search_20250305` server-side, but it answers 429
//! whenever that tool is declared and the model then decides not to search —
//! which is most turns. Claude Science declares it on every request, so leaving
//! the declaration in place fails every ordinary turn.
//!
//! The bridge moves the decision to the only party that knows it. The request
//! carries `web_search` as an ordinary *client* tool, which never trips the
//! defect. If the model wants a search it emits a `tool_use` for it — that call
//! is the signal. Claude Science cannot execute that block (it declared
//! web_search as a server tool and owns no local executor), so the bridge
//! swallows it, runs the real search upstream, and appends genuine
//! `server_tool_use` / `web_search_tool_result` blocks to the same message so
//! Science renders the result natively.

use serde_json::{json, Value};

use crate::anthropic_compat::{event_and_data, passthrough, render_sse, split_frame};

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_TOOL_INPUT_BYTES: usize = 64 * 1024;
const MAX_QUERY_BYTES: usize = 2 * 1024;
/// Bounds the extra upstream work a single turn can trigger.
pub(crate) const MAX_QUERIES: usize = 4;
pub(crate) const BRIDGE_TOOL_NAME: &str = "web_search";

/// The client-tool stand-in installed in place of the server declaration.
pub(crate) fn bridge_tool_declaration() -> Value {
    json!({
        "name": BRIDGE_TOOL_NAME,
        "description": concat!(
            "Search the web for current information. ",
            "Provide a focused query; results are fetched and returned automatically."
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "The search query"}
            },
            "required": ["query"]
        }
    })
}

/// Build the follow-up request that actually performs the search.
///
/// It replays the original conversation, appends an explicit instruction naming
/// the queries the model just asked for, and restores the real server tool. The
/// model therefore does search on this call, so the 429 defect cannot fire.
pub(crate) fn search_request(original: &Value, queries: &[String]) -> Result<Value, String> {
    let mut body = original.clone();
    let object = body
        .as_object_mut()
        .ok_or("web search bridge requires a JSON object request")?;

    let messages = object
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or("web search bridge requires a 'messages' array")?;
    let instruction = if queries.len() == 1 {
        format!("Use web_search to look up: {}", queries[0])
    } else {
        let listed = queries
            .iter()
            .map(|query| format!("- {query}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Use web_search to look up each of the following, then answer:\n{listed}")
    };
    messages.push(json!({"role": "user", "content": instruction}));

    // Restore the genuine server tool and drop the client stand-in so the model
    // cannot ask for another round of bridging.
    let mut tools: Vec<Value> = object
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter(|tool| tool.get("name").and_then(Value::as_str) != Some(BRIDGE_TOOL_NAME))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    tools.insert(
        0,
        json!({"type": "web_search_20250305", "name": "web_search"}),
    );
    object.insert("tools".to_string(), Value::Array(tools));
    object.remove("tool_choice");
    object.insert("stream".to_string(), Value::Bool(false));
    Ok(body)
}

/// Content blocks from the follow-up response that belong in Science's message.
///
/// The follow-up regenerates its own preamble; the caller has already streamed
/// the first call's preamble, so only the search evidence and the answer that
/// follows it are appended.
pub(crate) fn continuation_blocks(response: &Value) -> Result<Vec<Value>, String> {
    let content = response
        .get("content")
        .and_then(Value::as_array)
        .ok_or("web search follow-up response has no content array")?;
    let first_evidence = content.iter().position(|block| {
        matches!(
            block.get("type").and_then(Value::as_str),
            Some("server_tool_use" | "web_search_tool_result")
        )
    });
    let Some(start) = first_evidence else {
        return Err("web search follow-up returned no server search blocks".into());
    };
    Ok(content[start..].to_vec())
}

fn block_start(index: u64, content_block: Value) -> Vec<u8> {
    render_sse(
        Some("content_block_start"),
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": content_block
        }),
    )
}

fn block_delta(index: u64, delta: Value) -> Vec<u8> {
    render_sse(
        Some("content_block_delta"),
        &json!({"type": "content_block_delta", "index": index, "delta": delta}),
    )
}

/// Render one content block as a self-contained SSE block at `index`.
///
/// Every field that Anthropic streams incrementally must be emitted as a delta.
/// A spec-compliant client — Claude Science included — seeds the block from
/// `content_block_start` and then *accumulates* deltas, so a payload delivered
/// only inside the start frame is dropped: the block reaches the client with an
/// empty `input` / `thinking` / `signature`, and echoing that stripped block
/// back on a later turn makes the upstream reject the whole request.
pub(crate) fn render_block(index: u64, block: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    match block.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => {
            let text = block.get("text").and_then(Value::as_str).unwrap_or("");
            out.extend_from_slice(&block_start(index, json!({"type": "text", "text": ""})));
            if !text.is_empty() {
                out.extend_from_slice(&block_delta(
                    index,
                    json!({"type": "text_delta", "text": text}),
                ));
            }
        }
        "thinking" => {
            let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or("");
            let signature = block.get("signature").and_then(Value::as_str).unwrap_or("");
            out.extend_from_slice(&block_start(
                index,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            ));
            if !thinking.is_empty() {
                out.extend_from_slice(&block_delta(
                    index,
                    json!({"type": "thinking_delta", "thinking": thinking}),
                ));
            }
            if !signature.is_empty() {
                out.extend_from_slice(&block_delta(
                    index,
                    json!({"type": "signature_delta", "signature": signature}),
                ));
            }
        }
        "server_tool_use" => {
            let mut shell = block.clone();
            if let Some(object) = shell.as_object_mut() {
                object.insert("input".to_string(), json!({}));
            }
            let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
            out.extend_from_slice(&block_start(index, shell));
            if let Ok(partial) = serde_json::to_string(&input) {
                out.extend_from_slice(&block_delta(
                    index,
                    json!({"type": "input_json_delta", "partial_json": partial}),
                ));
            }
        }
        // `web_search_tool_result` has no delta form upstream; it arrives whole
        // inside content_block_start and clients read it from there.
        _ => out.extend_from_slice(&block_start(index, block.clone())),
    }
    out.extend_from_slice(&render_sse(
        Some("content_block_stop"),
        &json!({"type": "content_block_stop", "index": index}),
    ));
    out
}

/// The terminal delta that replaces the upstream one. `message_stop` still
/// comes from upstream so the SSE lifecycle validator sees a normal ending.
pub(crate) fn render_message_delta(stop_reason: &str, usage: Option<&Value>) -> Vec<u8> {
    let mut delta = json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": null}
    });
    if let Some(usage) = usage {
        delta["usage"] = usage.clone();
    }
    render_sse(Some("message_delta"), &delta)
}

/// Performs the follow-up search. Boxed so the streaming filter stays free of
/// borrows from the request scope, and so tests can drive it without a network.
pub type Searcher = Box<dyn FnMut(&[String]) -> Result<Value, String> + Send>;

/// Issue the follow-up search request against the same upstream.
pub(crate) fn run_follow_up(
    cfg: &crate::config::GatewayConfig,
    transport: Option<&crate::messages::AnthropicTransport>,
    original: &Value,
    queries: &[String],
) -> Result<Value, String> {
    let request = search_request(original, queries)?;
    let body = serde_json::to_vec(&request)
        .map_err(|error| format!("web search follow-up request is invalid: {error}"))?;
    let response = crate::messages::post_nonstream(cfg, body, transport).map_err(|error| {
        format!(
            "web search follow-up failed upstream ({}): {}",
            error.status, error.detail
        )
    })?;
    serde_json::from_slice::<Value>(&response.body)
        .map_err(|_| "web search follow-up returned invalid JSON".to_string())
}

#[derive(Debug)]
struct SwallowedToolUse {
    partial_json: String,
}

/// Streaming filter that swallows the bridge tool's `tool_use` blocks and
/// splices the real search in before the message ends.
///
/// The splice has to happen inside the filtered stream: the gateway validates
/// the Anthropic SSE lifecycle on the way out and withholds `message_stop`
/// until a clean EOF, so appending frames after the forwarding loop would be
/// rejected as a truncated stream.
pub struct WebSearchBridge {
    buf: Vec<u8>,
    next_upstream_index: u64,
    next_output_index: u64,
    active_output_block: Option<(u64, u64)>,
    swallowed: Option<SwallowedToolUse>,
    queries: Vec<String>,
    swallowed_blocks: usize,
    searcher: Searcher,
    resolved: bool,
}

impl std::fmt::Debug for WebSearchBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSearchBridge")
            .field("queries", &self.queries)
            .field("swallowed_blocks", &self.swallowed_blocks)
            .field("resolved", &self.resolved)
            .finish()
    }
}

impl WebSearchBridge {
    pub fn new(searcher: Searcher) -> Self {
        Self {
            buf: Vec::new(),
            next_upstream_index: 0,
            next_output_index: 0,
            active_output_block: None,
            swallowed: None,
            queries: Vec::new(),
            swallowed_blocks: 0,
            searcher,
            resolved: false,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((frame, sep_len, rest)) = split_frame(&self.buf) {
            let sep = self.buf[frame.len()..frame.len() + sep_len].to_vec();
            out.extend_from_slice(&self.rewrite_frame(&frame, &sep)?);
            self.buf = rest;
        }
        if self.buf.len() > MAX_FRAME_BYTES {
            return Err("Kimi for Coding SSE frame exceeds the bounded buffer".into());
        }
        Ok(out)
    }

    pub fn finalize(&mut self) -> Result<Vec<u8>, String> {
        if self.swallowed.is_some() || self.active_output_block.is_some() {
            return Err("Kimi for Coding content block ended before content_block_stop".into());
        }
        if !self.buf.iter().all(u8::is_ascii_whitespace) {
            return Err("Kimi for Coding SSE stream ended with a partial frame".into());
        }
        self.buf.clear();
        Ok(Vec::new())
    }

    /// Queries the model asked for, in order, deduplicated.
    pub fn queries(&self) -> &[String] {
        &self.queries
    }

    pub fn swallowed_blocks(&self) -> usize {
        self.swallowed_blocks
    }

    fn rewrite_frame(&mut self, frame: &[u8], sep: &[u8]) -> Result<Vec<u8>, String> {
        let (event, data) = event_and_data(frame);
        if data.is_empty() {
            return Ok(passthrough(frame, sep));
        }
        let Ok(mut obj) = serde_json::from_slice::<Value>(&data) else {
            return Ok(passthrough(frame, sep));
        };
        let Some(kind) = obj.get("type").and_then(Value::as_str).map(str::to_string) else {
            return Ok(passthrough(frame, sep));
        };
        if event.as_deref().is_some_and(|event| event != kind) {
            return Err("Kimi for Coding SSE event and JSON type do not match".into());
        }

        if self.swallowed.is_some() {
            return self.rewrite_swallowed_frame(&kind, &obj);
        }
        if self.active_output_block.is_some() {
            return self.rewrite_active_frame(&kind, event, obj);
        }
        if kind == "content_block_start" {
            let index = obj
                .get("index")
                .and_then(Value::as_u64)
                .ok_or("Kimi for Coding content block start index is invalid")?;
            if index != self.next_upstream_index {
                return Err("Kimi for Coding content block start index is invalid".into());
            }
            let block = obj.get("content_block").and_then(Value::as_object);
            let is_bridge_tool = block
                .map(|block| {
                    block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && block.get("name").and_then(Value::as_str) == Some(BRIDGE_TOOL_NAME)
                })
                .unwrap_or(false);
            if is_bridge_tool {
                if self.queries.len() >= MAX_QUERIES {
                    return Err("Kimi for Coding web search requested too many queries".into());
                }
                // Seed with any input already present on the start frame.
                let seed = block
                    .and_then(|block| block.get("input"))
                    .filter(|input| input.as_object().is_some_and(|input| !input.is_empty()))
                    .map(|input| input.to_string())
                    .unwrap_or_default();
                self.swallowed = Some(SwallowedToolUse { partial_json: seed });
                self.swallowed_blocks += 1;
                return Ok(Vec::new());
            }
            let mapped = self.next_output_index;
            if let Some(object) = obj.as_object_mut() {
                object.insert("index".to_string(), Value::Number(mapped.into()));
            }
            self.active_output_block = Some((index, mapped));
            return Ok(render_sse(event.as_deref(), &obj));
        }
        if kind == "message_delta" && !self.queries.is_empty() && !self.resolved {
            return self.resolve_search();
        }
        Ok(passthrough(frame, sep))
    }

    /// Run the real search and emit its evidence plus a fresh terminal delta in
    /// place of the upstream one, which reported an unfulfillable `tool_use`.
    fn resolve_search(&mut self) -> Result<Vec<u8>, String> {
        self.resolved = true;
        let follow_up = (self.searcher)(&self.queries)?;
        let blocks = continuation_blocks(&follow_up)?;
        let mut out = Vec::new();
        for block in &blocks {
            out.extend_from_slice(&render_block(self.next_output_index, block));
            self.next_output_index = self
                .next_output_index
                .checked_add(1)
                .ok_or("Kimi for Coding output block index overflow")?;
        }
        let stop_reason = follow_up
            .get("stop_reason")
            .and_then(Value::as_str)
            .filter(|reason| *reason != "tool_use")
            .unwrap_or("end_turn");
        out.extend_from_slice(&render_message_delta(stop_reason, follow_up.get("usage")));
        Ok(out)
    }

    fn rewrite_active_frame(
        &mut self,
        kind: &str,
        event: Option<String>,
        mut obj: Value,
    ) -> Result<Vec<u8>, String> {
        if matches!(kind, "error" | "ping") {
            if kind == "error" {
                self.active_output_block = None;
            }
            return Ok(render_sse(event.as_deref(), &obj));
        }
        let (upstream, mapped) = self
            .active_output_block
            .ok_or("Kimi for Coding content block state is missing")?;
        if obj.get("index").and_then(Value::as_u64) != Some(upstream) {
            return Err("Kimi for Coding content block index changed".into());
        }
        if !matches!(kind, "content_block_delta" | "content_block_stop") {
            return Err("Kimi for Coding content block ended before content_block_stop".into());
        }
        if let Some(object) = obj.as_object_mut() {
            object.insert("index".to_string(), Value::Number(mapped.into()));
        }
        if kind == "content_block_stop" {
            self.active_output_block = None;
            self.advance_upstream()?;
            self.next_output_index = self
                .next_output_index
                .checked_add(1)
                .ok_or("Kimi for Coding output block index overflow")?;
        }
        Ok(render_sse(event.as_deref(), &obj))
    }

    fn rewrite_swallowed_frame(&mut self, kind: &str, obj: &Value) -> Result<Vec<u8>, String> {
        match kind {
            "content_block_delta" => {
                let fragment = obj
                    .pointer("/delta/partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let swallowed = self
                    .swallowed
                    .as_mut()
                    .ok_or("Kimi for Coding swallowed block state is missing")?;
                swallowed.partial_json.push_str(fragment);
                if swallowed.partial_json.len() > MAX_TOOL_INPUT_BYTES {
                    return Err(
                        "Kimi for Coding web search input exceeds the bounded buffer".into(),
                    );
                }
                Ok(Vec::new())
            }
            "content_block_stop" => {
                let swallowed = self
                    .swallowed
                    .take()
                    .ok_or("Kimi for Coding swallowed block state is missing")?;
                self.advance_upstream()?;
                let query = serde_json::from_str::<Value>(&swallowed.partial_json)
                    .ok()
                    .and_then(|input| {
                        input
                            .get("query")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .map(str::to_string)
                    })
                    .filter(|query| !query.is_empty() && query.len() <= MAX_QUERY_BYTES);
                let Some(query) = query else {
                    return Err("Kimi for Coding web search request has no usable query".into());
                };
                if !self.queries.contains(&query) {
                    self.queries.push(query);
                }
                Ok(Vec::new())
            }
            "ping" => Ok(Vec::new()),
            _ => Err("Kimi for Coding web search block ended before content_block_stop".into()),
        }
    }

    fn advance_upstream(&mut self) -> Result<(), String> {
        self.next_upstream_index = self
            .next_upstream_index
            .checked_add(1)
            .ok_or("Kimi for Coding content block index overflow")?;
        Ok(())
    }
}

/// Queries requested by a non-streaming response, if any.
pub(crate) fn nonstream_queries(response: &Value) -> Vec<String> {
    let mut queries = Vec::new();
    let Some(content) = response.get("content").and_then(Value::as_array) else {
        return queries;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use")
            || block.get("name").and_then(Value::as_str) != Some(BRIDGE_TOOL_NAME)
        {
            continue;
        }
        let query = block
            .pointer("/input/query")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !query.is_empty()
            && query.len() <= MAX_QUERY_BYTES
            && !queries.contains(&query.to_string())
        {
            queries.push(query.to_string());
        }
        if queries.len() >= MAX_QUERIES {
            break;
        }
    }
    queries
}

/// Splice the follow-up's search evidence and answer into the first response,
/// dropping the bridge tool call that Science cannot execute.
pub(crate) fn merge_nonstream(first: &Value, follow_up: &Value) -> Result<Value, String> {
    let mut merged = first.clone();
    let kept: Vec<Value> = first
        .get("content")
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .filter(|block| {
                    !(block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && block.get("name").and_then(Value::as_str) == Some(BRIDGE_TOOL_NAME))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut content = kept;
    content.extend(continuation_blocks(follow_up)?);
    let object = merged
        .as_object_mut()
        .ok_or("web search bridge requires a JSON object response")?;
    object.insert("content".to_string(), Value::Array(content));
    let stop_reason = follow_up
        .get("stop_reason")
        .cloned()
        .unwrap_or(Value::String("end_turn".to_string()));
    object.insert("stop_reason".to_string(), stop_reason);
    if let Some(usage) = follow_up.get("usage") {
        object.insert("usage".to_string(), usage.clone());
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A searcher that never runs; for cases that must not reach the follow-up.
    fn no_search() -> Searcher {
        Box::new(|_| Err("searcher must not be called".into()))
    }

    /// A searcher returning a canned follow-up response.
    fn canned_search() -> Searcher {
        Box::new(|queries| {
            Ok(json!({
                "stop_reason": "end_turn",
                "usage": {"output_tokens": 11},
                "content": [
                    {"type": "text", "text": "let me look"},
                    {"type": "server_tool_use", "id": "s1", "name": "web_search",
                     "input": {"query": queries[0]}},
                    {"type": "web_search_tool_result", "tool_use_id": "s1", "content": []},
                    {"type": "text", "text": "the answer"}
                ]
            }))
        })
    }

    fn feed_all(bridge: &mut WebSearchBridge, frames: &[&str]) -> String {
        let mut out = Vec::new();
        for frame in frames {
            out.extend_from_slice(&bridge.feed(frame.as_bytes()).unwrap());
        }
        bridge.finalize().unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn ordinary_stream_passes_through_untouched_and_arms_no_search() {
        let mut bridge = WebSearchBridge::new(no_search());
        let text = feed_all(
            &mut bridge,
            &[
                "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ],
        );
        assert!(bridge.queries().is_empty());
        assert_eq!(bridge.swallowed_blocks(), 0);
        assert!(text.contains("text_delta"));
        assert!(text.contains("message_stop"));
    }

    #[test]
    fn bridge_tool_use_is_swallowed_and_its_query_captured() {
        let mut bridge = WebSearchBridge::new(no_search());
        let text = feed_all(
            &mut bridge,
            &[
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"web_search\",\"input\":{}}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"query\\\":\\\"rust \"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"version\\\"}\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            ],
        );
        assert_eq!(bridge.queries(), ["rust version".to_string()]);
        assert_eq!(bridge.swallowed_blocks(), 1);
        // Science must never see the client-tool call.
        assert!(!text.contains("web_search"));
        assert!(!text.contains("tool_use"));
        // The surviving thinking block keeps output index 0.
        assert!(text.contains("\"index\":0"));
    }

    #[test]
    fn indexes_are_compacted_after_a_swallowed_block() {
        let mut bridge = WebSearchBridge::new(no_search());
        let text = feed_all(
            &mut bridge,
            &[
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"web_search\",\"input\":{\"query\":\"a\"}}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            ],
        );
        // The text block was upstream index 1 but must reach Science as index 0.
        assert!(text.contains("\"index\":0"));
        assert!(!text.contains("\"index\":1"));
        assert_eq!(bridge.queries(), ["a".to_string()]);
    }

    #[test]
    fn repeated_queries_are_deduplicated_and_bounded() {
        let mut bridge = WebSearchBridge::new(no_search());
        let mut frames = Vec::new();
        for index in 0..3u64 {
            frames.push(format!(
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":{{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"web_search\",\"input\":{{\"query\":\"same\"}}}}}}\n\n"
            ));
            frames.push(format!(
                "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{index}}}\n\n"
            ));
        }
        let refs: Vec<&str> = frames.iter().map(String::as_str).collect();
        feed_all(&mut bridge, &refs);
        assert_eq!(bridge.queries(), ["same".to_string()]);
        assert_eq!(bridge.swallowed_blocks(), 3);
    }

    #[test]
    fn a_bridge_tool_call_without_a_query_fails_closed() {
        let mut bridge = WebSearchBridge::new(no_search());
        bridge
            .feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"web_search\",\"input\":{}}}\n\n")
            .unwrap();
        let err = bridge
            .feed(b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n")
            .unwrap_err();
        assert!(err.contains("no usable query"));
    }

    #[test]
    fn search_request_restores_the_server_tool_and_names_the_queries() {
        let original = json!({
            "model": "kimi-for-coding",
            "stream": true,
            "tool_choice": {"type": "auto"},
            "messages": [{"role": "user", "content": "latest rust?"}],
            "tools": [bridge_tool_declaration(), {"name": "bash", "input_schema": {"type": "object"}}]
        });
        let follow_up = search_request(&original, &["rust version".to_string()]).unwrap();
        assert_eq!(follow_up["stream"], json!(false));
        assert!(follow_up.get("tool_choice").is_none());
        assert_eq!(follow_up["tools"][0]["type"], "web_search_20250305");
        assert!(!follow_up["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |tool| tool.get("name").and_then(Value::as_str) == Some("web_search")
                    && tool.get("type").is_none()
            ));
        let messages = follow_up["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("rust version"));
    }

    #[test]
    fn continuation_starts_at_the_first_search_block_and_requires_evidence() {
        let response = json!({"content": [
            {"type": "text", "text": "let me look"},
            {"type": "server_tool_use", "id": "s1", "name": "web_search", "input": {"query": "q"}},
            {"type": "web_search_tool_result", "tool_use_id": "s1", "content": []},
            {"type": "text", "text": "answer"}
        ]});
        let blocks = continuation_blocks(&response).unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "server_tool_use");

        let without = json!({"content": [{"type": "text", "text": "no search happened"}]});
        assert!(continuation_blocks(&without).is_err());
    }

    #[test]
    fn nonstream_merge_drops_the_bridge_call_and_appends_real_evidence() {
        let first = json!({
            "type": "message",
            "stop_reason": "tool_use",
            "content": [
                {"type": "thinking", "thinking": "need a search", "signature": "sig"},
                {"type": "tool_use", "id": "t1", "name": "web_search", "input": {"query": "q"}}
            ]
        });
        let follow_up = json!({
            "stop_reason": "end_turn",
            "usage": {"output_tokens": 7},
            "content": [
                {"type": "text", "text": "searching"},
                {"type": "server_tool_use", "id": "s1", "name": "web_search", "input": {"query": "q"}},
                {"type": "web_search_tool_result", "tool_use_id": "s1", "content": []},
                {"type": "text", "text": "answer"}
            ]
        });
        let merged = merge_nonstream(&first, &follow_up).unwrap();
        let kinds: Vec<&str> = merged["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "thinking",
                "server_tool_use",
                "web_search_tool_result",
                "text"
            ]
        );
        assert_eq!(merged["stop_reason"], "end_turn");
        assert_eq!(merged["usage"]["output_tokens"], 7);
        assert_eq!(nonstream_queries(&first), ["q".to_string()]);
    }

    /// Rebuild a block the way a spec-compliant Anthropic client does: seed from
    /// `content_block_start`, then accumulate deltas. Anything the renderer puts
    /// only in the start frame is therefore invisible here — which is exactly
    /// the bug this guards against.
    fn reconstruct_like_a_client(frames: &[u8]) -> Value {
        let text = String::from_utf8(frames.to_vec()).unwrap();
        let mut block = json!({});
        let mut partial_json = String::new();
        for line in text.lines() {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let event: Value = serde_json::from_str(data).unwrap();
            match event["type"].as_str() {
                Some("content_block_start") => {
                    block = event["content_block"].clone();
                    // A real client treats the streamed fields as empty seeds.
                    for field in ["text", "thinking", "signature"] {
                        if block.get(field).is_some() {
                            block[field] = json!("");
                        }
                    }
                    if block.get("input").is_some() {
                        block["input"] = json!({});
                    }
                }
                Some("content_block_delta") => {
                    let delta = &event["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => append(&mut block, "text", delta["text"].as_str()),
                        Some("thinking_delta") => {
                            append(&mut block, "thinking", delta["thinking"].as_str())
                        }
                        Some("signature_delta") => {
                            append(&mut block, "signature", delta["signature"].as_str())
                        }
                        Some("input_json_delta") => {
                            partial_json.push_str(delta["partial_json"].as_str().unwrap_or(""))
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if !partial_json.is_empty() {
            block["input"] = serde_json::from_str(&partial_json).unwrap();
        }
        block
    }

    fn append(block: &mut Value, field: &str, value: Option<&str>) {
        let current = block[field].as_str().unwrap_or("").to_string();
        block[field] = json!(format!("{current}{}", value.unwrap_or("")));
    }

    #[test]
    fn spliced_blocks_survive_a_spec_compliant_client_round_trip() {
        // Science accumulates deltas and ignores payload left in the start
        // frame. Blocks that arrive stripped get echoed back stripped on the
        // next turn, and the upstream rejects the request.
        for original in [
            json!({"type": "text", "text": "the answer"}),
            json!({"type": "thinking", "thinking": "reasoning", "signature": "sig-abc"}),
            json!({
                "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                "input": {"query": "latest rust version"}
            }),
            json!({
                "type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
                "content": [{"type": "web_search_result", "url": "https://example.test"}]
            }),
        ] {
            let rebuilt = reconstruct_like_a_client(&render_block(0, &original));
            assert_eq!(
                rebuilt, original,
                "block lost content in transit: {original}"
            );
        }
    }

    #[test]
    fn rendered_blocks_are_well_formed_sse() {
        let frames = render_block(3, &json!({"type": "text", "text": "hello"}));
        let text = String::from_utf8(frames).unwrap();
        assert!(text.contains("event: content_block_start"));
        assert!(text.contains("\"index\":3"));
        assert!(text.contains("text_delta"));
        assert!(text.contains("event: content_block_stop"));

        let terminal = String::from_utf8(render_message_delta("end_turn", None)).unwrap();
        assert!(terminal.contains("\"stop_reason\":\"end_turn\""));
        // message_stop must stay upstream's, so the lifecycle validator sees it.
        assert!(!terminal.contains("message_stop"));
    }

    #[test]
    fn the_search_is_spliced_into_the_same_message_before_message_stop() {
        let mut bridge = WebSearchBridge::new(canned_search());
        let text = feed_all(
            &mut bridge,
            &[
                "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"looking\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"web_search\",\"input\":{\"query\":\"rust\"}}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ],
        );
        // Science sees real server evidence, never the client-tool call.
        assert!(text.contains("server_tool_use"));
        assert!(text.contains("web_search_tool_result"));
        assert!(!text.contains("\"type\":\"tool_use\""));
        // The upstream's tool_use terminal is replaced by a completed one.
        assert!(text.contains("\"stop_reason\":\"end_turn\""));
        assert!(!text.contains("\"stop_reason\":\"tool_use\""));
        // Ordering: evidence, then terminal delta, then upstream's message_stop.
        let evidence = text.find("server_tool_use").unwrap();
        let delta = text.find("\"stop_reason\":\"end_turn\"").unwrap();
        let stop = text.find("event: message_stop").unwrap();
        assert!(evidence < delta && delta < stop);
        // Blocks keep a gapless index sequence: text 0, then 1 and 2 spliced in.
        assert!(text.contains("\"index\":1"));
        assert!(text.contains("\"index\":2"));
    }

    #[test]
    fn a_failing_follow_up_surfaces_instead_of_inventing_an_answer() {
        let mut bridge = WebSearchBridge::new(Box::new(|_| Err("upstream 429".into())));
        bridge
            .feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"web_search\",\"input\":{\"query\":\"q\"}}}\n\n")
            .unwrap();
        bridge
            .feed(b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n")
            .unwrap();
        let err = bridge
            .feed(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n")
            .unwrap_err();
        assert_eq!(err, "upstream 429");
    }
}
