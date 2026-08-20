use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use serde_json::{json, Value};

use crate::auth::{strip_path_secret, AuthResult};
use crate::config::GatewayConfig;
use crate::{
    anthropic_compat::{self, AnthropicMetadata},
    connect, kimi_search_noise, kimi_web_search_adapter, messages, models,
};

use crate::kimi_search_noise::{
    RULE_PROVIDER_KIMI_EMPTY_SEARCH_PAIR_STRIP as RULE_PAIR_STRIP,
    RULE_PROVIDER_KIMI_SEARCH_NOISE_TEXT_STRIP as RULE_NOISE_STRIP,
    RULE_PROVIDER_KIMI_SEARCH_PAIR_ID_ADOPT as RULE_PAIR_ADOPT,
};

struct RequestHead {
    method: String,
    target: String,
    headers: HashMap<String, String>,
}

impl RequestHead {
    fn anthropic_transport(&self) -> Result<messages::AnthropicTransport, String> {
        messages::AnthropicTransport::from_inbound(&self.target, &self.headers)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamTermination {
    NormalEof,
    UpstreamTerminalError,
    UpstreamReadError,
    ProtocolError,
    DownstreamWriteError,
}

fn read_head(stream: &mut TcpStream) -> Result<RequestHead, String> {
    let mut buf = Vec::with_capacity(4096);
    let mut byte = [0_u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("empty request".to_string());
        }
        buf.push(byte[0]);
        if buf.len() > 64 * 1024 {
            return Err("request headers too large".to_string());
        }
    }
    let text = std::str::from_utf8(&buf).map_err(|_| "invalid request headers".to_string())?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or("missing request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing method")?.to_string();
    let target = parts.next().ok_or("missing target")?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Ok(RequestHead {
        method,
        target,
        headers,
    })
}

fn content_length(headers: &HashMap<String, String>) -> Result<usize, String> {
    let Some(raw) = headers.get("content-length") else {
        return Ok(0);
    };
    let parsed = raw
        .parse::<i64>()
        .map_err(|_| "invalid Content-Length".to_string())?;
    if parsed < 0 {
        return Err("invalid Content-Length".to_string());
    }
    Ok(parsed as usize)
}

fn read_body(stream: &mut TcpStream, len: usize) -> Result<Vec<u8>, String> {
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok(body)
}

fn json_bytes(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"internal\"}".to_vec())
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> bool {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .and_then(|_| stream.write_all(body))
    .and_then(|_| stream.flush())
    .is_ok()
}

fn write_json(stream: &mut TcpStream, status: u16, reason: &str, value: Value) {
    let body = json_bytes(value);
    write_response(stream, status, reason, "application/json", &body);
}

fn typed_error_json(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    error_type: &str,
    message: &str,
) {
    write_json(
        stream,
        status,
        reason,
        json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": message,
            },
        }),
    );
}

fn forbidden_json(stream: &mut TcpStream) {
    typed_error_json(stream, 403, "Forbidden", "permission_error", "forbidden");
}

fn invalid_request_json(stream: &mut TcpStream, detail: &str) {
    typed_error_json(stream, 400, "Bad Request", "invalid_request_error", detail);
}

fn route_unknown_json(stream: &mut TcpStream, detail: &str) {
    typed_error_json(stream, 400, "Bad Request", "route_unknown", detail);
}

fn not_found_json(stream: &mut TcpStream, path: &str) {
    typed_error_json(stream, 404, "Not Found", "not_found_error", path);
}

fn api_error_json(stream: &mut TcpStream, status: u16, detail: &str) {
    typed_error_json(stream, status, status_reason(status), "api_error", detail);
}

fn report_upstream_failure(
    stream: &mut TcpStream,
    operation: &'static str,
    error: &messages::UpstreamError,
) {
    crate::log_line!("{}", messages::upstream_failure_metadata(operation, error));
    api_error_json(stream, error.status, &error.detail);
}

fn status_reason(status: u16) -> &'static str {
    reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("Error")
}

fn dequery(path: &str) -> &str {
    path.split_once('?').map(|(p, _)| p).unwrap_or(path)
}

fn models_error_json(
    stream: &mut TcpStream,
    status: u16,
    error_kind: &str,
    upstream_status: Option<u16>,
    message: &str,
) {
    write_json(
        stream,
        status,
        status_reason(status),
        json!({
            "error_kind": error_kind,
            "upstream_status": upstream_status,
            "message": message,
        }),
    );
}

fn handle_get(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    target: &str,
    relay_models: &models::RelayModelCache,
) {
    let path = match strip_path_secret(dequery(target), cfg.auth_secret.as_deref()) {
        AuthResult::Ok(path) => path,
        AuthResult::Forbidden => {
            forbidden_json(stream);
            return;
        }
    };
    match path.as_str() {
        "/health" => {
            let mut health = json!({
                "status": "ok",
                "gateway": "rust",
                "provider": cfg.provider,
                "shim": cfg.shim_mode,
                "launch_id": cfg.launch_id,
                "intent": cfg.intent.as_str(),
            });
            if let Some(resolver) = cfg.static_model_resolver.as_ref() {
                health["catalog_fp"] = Value::String(resolver.catalog_fp().to_string());
            }
            if let Some(contract) = cfg.provider_contract.as_ref() {
                let object = health
                    .as_object_mut()
                    .expect("health response is an object");
                object.insert(
                    "provider_contract_id".into(),
                    Value::String(contract.contract_id.clone()),
                );
                object.insert(
                    "provider_contract_digest".into(),
                    Value::String(contract.catalog_digest.clone()),
                );
            }
            write_json(stream, 200, "OK", health)
        }
        "/v1/models" if cfg.intent != crate::config::GatewayIntent::ScratchModels => {
            let Some(resolver) = cfg.static_model_resolver.as_ref() else {
                typed_error_json(
                    stream,
                    503,
                    "Service Unavailable",
                    "catalog_unavailable",
                    "static model catalog is unavailable",
                );
                return;
            };
            write_json(stream, 200, "OK", resolver.models_response())
        }
        "/v1/models" if cfg.provider == "relay" => {
            let Some(models_url) = cfg.models_url.as_deref() else {
                models_error_json(stream, 502, "network", None, "missing models URL");
                return;
            };
            match messages::get(cfg, models_url) {
                Ok(resp) => match serde_json::from_slice::<Value>(&resp.body) {
                    Ok(raw) => {
                        let (body, ids) = models::normalize_live_models(&raw);
                        relay_models.update_from_live_models(&cfg.provider, &ids);
                        write_json(stream, 200, "OK", body);
                    }
                    Err(e) => models_error_json(
                        stream,
                        502,
                        "protocol",
                        None,
                        &format!("upstream models JSON parse failed: {e}"),
                    ),
                },
                Err(e) => models_error_json(
                    stream,
                    e.status,
                    if e.upstream_status.is_some() {
                        "upstream"
                    } else {
                        "network"
                    },
                    e.upstream_status,
                    &e.detail,
                ),
            }
        }
        "/v1/models" => write_json(stream, 200, "OK", models::deepseek_models_response()),
        _ => not_found_json(stream, &path),
    }
}

fn write_chunk(stream: &mut TcpStream, chunk: &[u8]) -> std::io::Result<()> {
    write!(stream, "{:x}\r\n", chunk.len())?;
    stream.write_all(chunk)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn stream_strip_rules(stats: &kimi_search_noise::StripStats) -> String {
    let mut rules = Vec::new();
    if stats.noise_blocks > 0 {
        rules.push(RULE_NOISE_STRIP);
    }
    if stats.pair_blocks > 0 {
        rules.push(RULE_PAIR_STRIP);
    }
    if stats.adopted_pairs > 0 {
        rules.push(RULE_PAIR_ADOPT);
    }
    if rules.is_empty() {
        // 只有无钥对(仅记数、零改写)时,规则清单为空:净效果为零不记规则。
        "-".to_string()
    } else {
        rules.join(",")
    }
}

/// 剥离 / 采钥统计的日志尾部:`adopted=` 恒出现,`unkeyed=` 仅在非零时出现。
fn strip_stats_suffix(stats: &kimi_search_noise::StripStats) -> String {
    let mut suffix = format!(" adopted={}", stats.adopted_pairs);
    if stats.unkeyed_pairs > 0 {
        suffix.push_str(&format!(" unkeyed={}", stats.unkeyed_pairs));
    }
    suffix
}

fn stream_error_event(detail: &str) -> Vec<u8> {
    format!(
        "event: error\ndata: {}\n\n",
        json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": detail,
            },
        })
    )
    .into_bytes()
}

fn forward_stream_body<R, F>(
    upstream: &mut R,
    first: &[u8],
    filter: Option<kimi_search_noise::SearchNoiseFilter>,
    emit: F,
) -> StreamTermination
where
    R: Read,
    F: FnMut(&[u8]) -> std::io::Result<()>,
{
    let mut success_rollback = None;
    forward_stream_body_with_capture(
        upstream,
        first,
        filter,
        None,
        &mut success_rollback,
        emit,
        |_| Ok(None),
    )
}

type StreamRollback = Box<dyn FnOnce() -> Result<(), String>>;

const MAX_CONTINUITY_STREAM_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct AnthropicStreamMessageCollector {
    message: Option<Value>,
    open_block: Option<usize>,
    partial_tool_input: HashMap<usize, String>,
    total_bytes: usize,
}

impl AnthropicStreamMessageCollector {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.total_bytes = self
            .total_bytes
            .checked_add(bytes.len())
            .filter(|total| *total <= MAX_CONTINUITY_STREAM_BYTES)
            .ok_or_else(|| "thinking continuity stream exceeds the bounded buffer".to_string())?;
        let mut remaining = bytes;
        while !remaining.is_empty() {
            if remaining.iter().all(u8::is_ascii_whitespace) {
                break;
            }
            let end = sse_frame_end(remaining)
                .ok_or_else(|| "thinking continuity received a partial SSE frame".to_string())?;
            self.feed_frame(&remaining[..end])?;
            remaining = &remaining[end..];
        }
        Ok(())
    }

    fn feed_frame(&mut self, frame: &[u8]) -> Result<(), String> {
        let text = std::str::from_utf8(frame)
            .map_err(|_| "thinking continuity SSE is not valid UTF-8".to_string())?;
        let mut data_lines = Vec::new();
        for line in text.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
                continue;
            }
            if let Some(value) = line.strip_prefix("data:") {
                data_lines.push(value.strip_prefix(' ').unwrap_or(value));
            }
        }
        if data_lines.is_empty() {
            return Ok(());
        }
        let event: Value = serde_json::from_str(&data_lines.join("\n"))
            .map_err(|_| "thinking continuity SSE data is not valid JSON".to_string())?;
        match event.get("type").and_then(Value::as_str) {
            Some("ping") => Ok(()),
            Some("message_start") => self.start_message(&event),
            Some("content_block_start") => self.start_block(&event),
            Some("content_block_delta") => self.apply_block_delta(&event),
            Some("content_block_stop") => self.stop_block(&event),
            Some("message_delta") => self.apply_message_delta(&event),
            Some(other) => Err(format!(
                "thinking continuity cannot reconstruct SSE event {other}"
            )),
            None => Err("thinking continuity SSE event type is missing".into()),
        }
    }

    fn start_message(&mut self, event: &Value) -> Result<(), String> {
        if self.message.is_some() {
            return Err("thinking continuity message started twice".into());
        }
        let mut message = event
            .get("message")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| "thinking continuity message_start is invalid".to_string())?;
        match message.get("content") {
            None => {
                message.insert("content".into(), Value::Array(Vec::new()));
            }
            Some(Value::Array(content)) if content.is_empty() => {}
            _ => return Err("thinking continuity message_start content is invalid".into()),
        }
        self.message = Some(Value::Object(message));
        Ok(())
    }

    fn start_block(&mut self, event: &Value) -> Result<(), String> {
        if self.open_block.is_some() {
            return Err("thinking continuity content blocks overlap".into());
        }
        let index = event_index_usize(event)?;
        let mut block = event
            .get("content_block")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| "thinking continuity content block is invalid".to_string())?;
        if block.get("type").and_then(Value::as_str) == Some("thinking") {
            for field in ["thinking", "signature"] {
                match block.get(field) {
                    None => {
                        block.insert(field.into(), Value::String(String::new()));
                    }
                    Some(Value::String(_)) => {}
                    Some(_) => {
                        return Err(format!("thinking continuity {field} field is invalid"));
                    }
                }
            }
        }
        let content = self.message_content_mut()?;
        if index != content.len() {
            return Err("thinking continuity content block index is invalid".into());
        }
        content.push(Value::Object(block));
        self.open_block = Some(index);
        Ok(())
    }

    fn apply_block_delta(&mut self, event: &Value) -> Result<(), String> {
        let index = event_index_usize(event)?;
        if self.open_block != Some(index) {
            return Err("thinking continuity content block delta is out of order".into());
        }
        let delta = event
            .get("delta")
            .and_then(Value::as_object)
            .ok_or_else(|| "thinking continuity content block delta is invalid".to_string())?;
        let delta_type = delta
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| "thinking continuity content block delta type is missing".to_string())?;
        match delta_type {
            "text_delta" => {
                let value = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "thinking continuity text delta is invalid".to_string())?;
                append_block_text(self.message_content_mut()?, index, "text", value)
            }
            "thinking_delta" => {
                let value = delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "thinking continuity thinking delta is invalid".to_string())?;
                append_block_text(self.message_content_mut()?, index, "thinking", value)
            }
            "signature_delta" => {
                let value = delta
                    .get("signature")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "thinking continuity signature delta is invalid".to_string())?;
                append_block_text(self.message_content_mut()?, index, "signature", value)
            }
            "input_json_delta" => {
                let value = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "thinking continuity input JSON delta is invalid".to_string())?;
                let partial = self.partial_tool_input.entry(index).or_default();
                if partial.len().saturating_add(value.len()) > MAX_CONTINUITY_STREAM_BYTES {
                    return Err("thinking continuity tool input exceeds the bounded buffer".into());
                }
                partial.push_str(value);
                Ok(())
            }
            "citations_delta" => {
                let citation = delta
                    .get("citation")
                    .cloned()
                    .ok_or_else(|| "thinking continuity citation delta is invalid".to_string())?;
                let content = self.message_content_mut()?;
                let block = content
                    .get_mut(index)
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| "thinking continuity content block is invalid".to_string())?;
                let citations = block
                    .entry("citations")
                    .or_insert_with(|| Value::Array(Vec::new()))
                    .as_array_mut()
                    .ok_or_else(|| "thinking continuity citations are invalid".to_string())?;
                citations.push(citation);
                Ok(())
            }
            _ => Err(format!(
                "thinking continuity cannot reconstruct content delta {delta_type}"
            )),
        }
    }

    fn stop_block(&mut self, event: &Value) -> Result<(), String> {
        let index = event_index_usize(event)?;
        if self.open_block != Some(index) {
            return Err("thinking continuity content block stop is out of order".into());
        }
        if let Some(partial) = self.partial_tool_input.remove(&index) {
            let input: Value = serde_json::from_str(&partial)
                .map_err(|_| "thinking continuity tool input JSON is invalid".to_string())?;
            let block = self
                .message_content_mut()?
                .get_mut(index)
                .and_then(Value::as_object_mut)
                .ok_or_else(|| "thinking continuity content block is invalid".to_string())?;
            block.insert("input".into(), input);
        }
        self.open_block = None;
        Ok(())
    }

    fn apply_message_delta(&mut self, event: &Value) -> Result<(), String> {
        let delta = event
            .get("delta")
            .and_then(Value::as_object)
            .ok_or_else(|| "thinking continuity message delta is invalid".to_string())?;
        let message = self
            .message
            .as_mut()
            .and_then(Value::as_object_mut)
            .ok_or_else(|| "thinking continuity message is unavailable".to_string())?;
        for (key, value) in delta {
            message.insert(key.clone(), value.clone());
        }
        if let Some(usage) = event.get("usage") {
            message.insert("usage".into(), usage.clone());
        }
        Ok(())
    }

    fn message_content_mut(&mut self) -> Result<&mut Vec<Value>, String> {
        self.message
            .as_mut()
            .and_then(|message| message.get_mut("content"))
            .and_then(Value::as_array_mut)
            .ok_or_else(|| "thinking continuity message content is unavailable".to_string())
    }

    fn finish(self) -> Result<Value, String> {
        if self.open_block.is_some() || !self.partial_tool_input.is_empty() {
            return Err("thinking continuity stream ended with an open content block".into());
        }
        self.message
            .ok_or_else(|| "thinking continuity stream has no message".to_string())
    }
}

fn sse_frame_end(buffer: &[u8]) -> Option<usize> {
    let lf = buffer
        .windows(2)
        .position(|part| part == b"\n\n")
        .map(|index| index + 2);
    let crlf = buffer
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .map(|index| index + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn event_index_usize(event: &Value) -> Result<usize, String> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| "thinking continuity SSE block index is invalid".to_string())
}

fn append_block_text(
    content: &mut [Value],
    index: usize,
    field: &str,
    value: &str,
) -> Result<(), String> {
    let target = content
        .get_mut(index)
        .and_then(Value::as_object_mut)
        .and_then(|block| block.get_mut(field))
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("thinking continuity {field} field is invalid"))?;
    if target.len().saturating_add(value.len()) > MAX_CONTINUITY_STREAM_BYTES {
        return Err(format!(
            "thinking continuity {field} exceeds the bounded buffer"
        ));
    }
    let mut joined = String::with_capacity(target.len() + value.len());
    joined.push_str(target);
    joined.push_str(value);
    content[index]
        .as_object_mut()
        .expect("validated content block")
        .insert(field.into(), Value::String(joined));
    Ok(())
}

fn forward_stream_body_with_capture<R, F, C>(
    upstream: &mut R,
    first: &[u8],
    mut filter: Option<kimi_search_noise::SearchNoiseFilter>,
    mut collector: Option<&mut AnthropicStreamMessageCollector>,
    success_rollback: &mut Option<StreamRollback>,
    mut emit: F,
    mut on_complete: C,
) -> StreamTermination
where
    R: Read,
    F: FnMut(&[u8]) -> std::io::Result<()>,
    C: FnMut(Option<Value>) -> Result<Option<StreamRollback>, String>,
{
    let mut validator = crate::anthropic_sse::Validator::default();

    // 过滤器失败必须携带真实原因:既写日志也发给客户端,不得降级成
    // 无信息量的通用文案(2026-08-19 的诊断黑洞正是这么来的)。
    let filter_failure = |error: &str, emit: &mut F| {
        crate::log_line!("relay stream filter error={error}");
        if emit(&stream_error_event(&format!(
            "CSSwitch response filter: {error}"
        )))
        .is_err()
        {
            StreamTermination::DownstreamWriteError
        } else {
            StreamTermination::ProtocolError
        }
    };

    let process = |chunk: &[u8],
                   validator: &mut crate::anthropic_sse::Validator,
                   collector: &mut Option<&mut AnthropicStreamMessageCollector>,
                   emit: &mut F| {
        let validated = match validator.feed(chunk) {
            Ok(validated) => validated,
            Err(_) => {
                if emit(&stream_error_event("upstream SSE protocol error")).is_err() {
                    return Some(StreamTermination::DownstreamWriteError);
                }
                return Some(StreamTermination::ProtocolError);
            }
        };
        if !validated.bytes.is_empty()
            && collector
                .as_deref_mut()
                .is_some_and(|collector| collector.feed(&validated.bytes).is_err())
        {
            if emit(&stream_error_event("thinking continuity capture failed")).is_err() {
                return Some(StreamTermination::DownstreamWriteError);
            }
            return Some(StreamTermination::ProtocolError);
        }
        if !validated.bytes.is_empty() && emit(&validated.bytes).is_err() {
            return Some(StreamTermination::DownstreamWriteError);
        }
        validated
            .terminal_error
            .then_some(StreamTermination::UpstreamTerminalError)
    };

    let first_chunk = if let Some(filter) = filter.as_mut() {
        match filter.feed(first) {
            Ok(chunk) => chunk,
            Err(error) => return filter_failure(&error, &mut emit),
        }
    } else {
        first.to_vec()
    };
    if let Some(termination) = process(&first_chunk, &mut validator, &mut collector, &mut emit) {
        return termination;
    }

    let mut buf = [0_u8; 8192];
    loop {
        match upstream.read(&mut buf) {
            Ok(0) => {
                if let Some(filter) = filter.as_mut() {
                    match filter.finalize() {
                        Ok(tail) => {
                            if !tail.is_empty() {
                                if let Some(termination) =
                                    process(&tail, &mut validator, &mut collector, &mut emit)
                                {
                                    return termination;
                                }
                            }
                        }
                        Err(error) => return filter_failure(&error, &mut emit),
                    }
                    let stats = filter.stats();
                    if stats.any_activity() {
                        crate::log_line!(
                            "relay stream rules={} noise={} pair={} bytes={}{}",
                            stream_strip_rules(&stats),
                            stats.noise_blocks,
                            stats.pair_blocks,
                            stats.bytes,
                            strip_stats_suffix(&stats)
                        );
                    }
                }
                return match validator.finish() {
                    Ok(terminal) => {
                        let message = match collector.take() {
                            Some(collector) => match std::mem::take(collector).finish() {
                                Ok(message) => Some(message),
                                Err(_) => {
                                    if emit(&stream_error_event(
                                        "thinking continuity capture failed",
                                    ))
                                    .is_err()
                                    {
                                        return StreamTermination::DownstreamWriteError;
                                    }
                                    return StreamTermination::ProtocolError;
                                }
                            },
                            None => None,
                        };
                        match on_complete(message) {
                            Ok(rollback) => {
                                if !terminal.is_empty() && emit(&terminal).is_err() {
                                    if let Some(rollback) = rollback {
                                        if rollback().is_err() {
                                            crate::log_line!("thinking continuity rollback failed");
                                        }
                                    }
                                    StreamTermination::DownstreamWriteError
                                } else {
                                    *success_rollback = rollback;
                                    StreamTermination::NormalEof
                                }
                            }
                            Err(_) => {
                                if emit(&stream_error_event("thinking continuity persist failed"))
                                    .is_err()
                                {
                                    StreamTermination::DownstreamWriteError
                                } else {
                                    StreamTermination::ProtocolError
                                }
                            }
                        }
                    }
                    Err(_) => {
                        if emit(&stream_error_event("upstream SSE protocol error")).is_err() {
                            StreamTermination::DownstreamWriteError
                        } else {
                            StreamTermination::ProtocolError
                        }
                    }
                };
            }
            Ok(n) => {
                let chunk = if let Some(filter) = filter.as_mut() {
                    match filter.feed(&buf[..n]) {
                        Ok(chunk) => chunk,
                        Err(error) => return filter_failure(&error, &mut emit),
                    }
                } else {
                    buf[..n].to_vec()
                };
                if let Some(termination) =
                    process(&chunk, &mut validator, &mut collector, &mut emit)
                {
                    return termination;
                }
            }
            Err(_) => {
                if emit(&stream_error_event("upstream stream read failed")).is_err() {
                    return StreamTermination::DownstreamWriteError;
                }
                return StreamTermination::UpstreamReadError;
            }
        }
    }
}

/// The one place the streaming response head is written; both the native
/// forwarding path and the Web Search adapter path go through it.
fn write_sse_head(stream: &mut TcpStream) -> bool {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
    )
    .and_then(|_| stream.flush())
    .is_ok()
}

fn finish_chunked_stream(stream: &mut TcpStream) {
    let _ = stream.write_all(b"0\r\n\r\n").and_then(|_| stream.flush());
}

fn handle_stream(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    body: Vec<u8>,
    transport: Option<&messages::AnthropicTransport>,
    filter: Option<kimi_search_noise::SearchNoiseFilter>,
) {
    let mut upstream = match messages::open_stream(cfg, body, transport) {
        Ok(upstream) => upstream,
        Err(error) => {
            report_upstream_failure(stream, "open_stream", &error);
            return;
        }
    };
    if !write_sse_head(stream) {
        return;
    }
    let termination = forward_stream_body(&mut upstream.response, &[], filter, |chunk| {
        write_chunk(stream, chunk)
    });
    match termination {
        StreamTermination::NormalEof => {}
        StreamTermination::UpstreamTerminalError
        | StreamTermination::UpstreamReadError
        | StreamTermination::ProtocolError => {}
        StreamTermination::DownstreamWriteError => return,
    }
    finish_chunked_stream(stream);
}

fn parse_adapter_message(body: &[u8], stage: &str) -> Result<Value, String> {
    let message: Value = serde_json::from_slice(body).map_err(|error| {
        format!("Kimi Web Search adapter {stage} returned invalid JSON: {error}")
    })?;
    if !message.is_object() {
        return Err(format!(
            "Kimi Web Search adapter {stage} returned a non-object response"
        ));
    }
    Ok(message)
}

/// An SSE comment frame: protocol-legal, ignored by event parsers, and the
/// only honest way to move first-byte time forward on a bridged turn — no
/// fabricated `ping` events, no fake content.
fn sse_comment(text: &str) -> Vec<u8> {
    format!(": {text}\n\n").into_bytes()
}

/// Once the streaming head has been written, failures can no longer change
/// the HTTP status; they become an explicit terminal SSE error instead.
fn adapter_error(stream: &mut TcpStream, head_written: bool, status: u16, detail: &str) {
    if head_written {
        if write_chunk(stream, &stream_error_event(detail)).is_ok() {
            finish_chunked_stream(stream);
        }
    } else {
        api_error_json(stream, status, detail);
    }
}

/// Safe projection of the merged content for the adapter log line: block
/// types only, unknown types collapse to `other`.
fn merged_shape(message: &Value) -> String {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return "-".into();
    };
    if content.is_empty() {
        return "-".into();
    }
    content
        .iter()
        .map(|block| match block.get("type").and_then(Value::as_str) {
            Some(
                value @ ("thinking"
                | "redacted_thinking"
                | "text"
                | "tool_use"
                | "server_tool_use"
                | "web_search_tool_result"),
            ) => value,
            Some(_) => "other",
            None => "missing",
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The leading `_`-segment of each distinct `web_search_tool_result`
/// pairing key (e.g. `srvtoolu`), never the key itself: enough to see which
/// id family survived into the merged message when a frame later drops the
/// pair on disk.
fn pair_key_prefix(message: &Value) -> String {
    let mut prefixes: Vec<String> = Vec::new();
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if block.get("type").and_then(Value::as_str) != Some("web_search_tool_result") {
            continue;
        }
        let prefix = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(|id| id.split('_').next().unwrap_or(id))
            .map(|segment| segment.chars().take(12).collect::<String>())
            .unwrap_or_else(|| "none".into());
        if !prefixes.contains(&prefix) {
            prefixes.push(prefix);
        }
    }
    if prefixes.is_empty() {
        "-".into()
    } else {
        prefixes.join(",")
    }
}

fn handle_kimi_web_search_adapter(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    main_request: Value,
    transport: Option<&messages::AnthropicTransport>,
    downstream_stream: bool,
    prepared: kimi_web_search_adapter::PreparedRequest,
) {
    let main_body = match serde_json::to_vec(&main_request) {
        Ok(body) => body,
        Err(error) => {
            invalid_request_json(stream, &error.to_string());
            return;
        }
    };
    // A bridged turn takes several upstream calls before the first real
    // frame. Claim the streaming response immediately and mark stage
    // progress with SSE comments so the client sees bytes, not silence.
    let head_written = downstream_stream && {
        if !write_sse_head(stream) {
            return;
        }
        let _ = write_chunk(stream, &sse_comment("bridge processing"));
        true
    };
    let deadline = messages::inference_deadline(cfg);
    let main_started = Instant::now();
    let first = match messages::post_nonstream_before(cfg, main_body, transport, deadline) {
        Ok(response) => match parse_adapter_message(&response.body, "main stage") {
            Ok(message) => message,
            Err(error) => {
                adapter_error(stream, head_written, 502, &error);
                return;
            }
        },
        Err(error) => {
            crate::log_line!(
                "{}",
                messages::upstream_failure_metadata("kimi_web_search_adapter_main", &error)
            );
            adapter_error(stream, head_written, error.status, &error.detail);
            return;
        }
    };
    let main_ms = main_started.elapsed().as_millis();
    if head_written {
        let _ = write_chunk(stream, &sse_comment("main stage complete"));
    }

    enum AdapterFailure {
        Upstream(messages::UpstreamError),
        Protocol(String),
    }
    let mut nested_ms = 0;
    let mut synthesis_ms: Option<u128> = None;
    let outcome = kimi_web_search_adapter::resolve_with(
        &main_request,
        &prepared,
        &first,
        |stage, request| {
            let stage_name = match stage {
                kimi_web_search_adapter::AdapterStage::Nested => "nested stage",
                kimi_web_search_adapter::AdapterStage::Synthesis => "synthesis stage",
            };
            let body = serde_json::to_vec(&request).map_err(|error| {
                AdapterFailure::Protocol(format!(
                    "Kimi Web Search adapter {stage_name} request serialization failed: {error}"
                ))
            })?;
            let started = Instant::now();
            let response = messages::post_nonstream_before(cfg, body, transport, deadline);
            let elapsed = started.elapsed().as_millis();
            match stage {
                kimi_web_search_adapter::AdapterStage::Nested => nested_ms = elapsed,
                kimi_web_search_adapter::AdapterStage::Synthesis => synthesis_ms = Some(elapsed),
            }
            let response = response.map_err(AdapterFailure::Upstream)?;
            if head_written && stage == kimi_web_search_adapter::AdapterStage::Nested {
                let _ = write_chunk(stream, &sse_comment("nested stage complete"));
            }
            parse_adapter_message(&response.body, stage_name).map_err(AdapterFailure::Protocol)
        },
    );
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(kimi_web_search_adapter::ResolveError::Protocol(error))
        | Err(kimi_web_search_adapter::ResolveError::Upstream(
            _,
            AdapterFailure::Protocol(error),
        )) => {
            crate::log_line!(
                "relay Kimi Web Search adapter protocol failure rule={}: {error}",
                kimi_web_search_adapter::RULE_PROVIDER_KIMI_WEB_SEARCH_QUERY_TOOL_ADAPTER
            );
            adapter_error(stream, head_written, 502, &error);
            return;
        }
        Err(kimi_web_search_adapter::ResolveError::Upstream(
            stage,
            AdapterFailure::Upstream(error),
        )) => {
            let operation = match stage {
                kimi_web_search_adapter::AdapterStage::Nested => "kimi_web_search_adapter_nested",
                kimi_web_search_adapter::AdapterStage::Synthesis => {
                    "kimi_web_search_adapter_synthesis"
                }
            };
            crate::log_line!("{}", messages::upstream_failure_metadata(operation, &error));
            adapter_error(stream, head_written, error.status, &error.detail);
            return;
        }
    };
    if outcome.strip_stats.any_activity() {
        crate::log_line!(
            "relay nonstream rules={} noise={} pair={} bytes={}{}",
            stream_strip_rules(&outcome.strip_stats),
            outcome.strip_stats.noise_blocks,
            outcome.strip_stats.pair_blocks,
            outcome.strip_stats.bytes,
            strip_stats_suffix(&outcome.strip_stats)
        );
    }
    crate::log_line!(
        "relay Kimi Web Search adapter rule={} model={} bridged={} queries={} upstream_calls={} stripped_client_search_tail={} merged_shape={} pair_key_prefix={} main_ms={} nested_ms={} synthesis_ms={}",
        kimi_web_search_adapter::RULE_PROVIDER_KIMI_WEB_SEARCH_QUERY_TOOL_ADAPTER,
        main_request
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("-"),
        usize::from(outcome.queries > 0),
        outcome.queries,
        1 + usize::from(outcome.queries > 0) + usize::from(synthesis_ms.is_some()),
        outcome.stripped_client_search_tail,
        merged_shape(&outcome.message),
        pair_key_prefix(&outcome.message),
        main_ms,
        nested_ms,
        synthesis_ms.unwrap_or(0)
    );

    if downstream_stream {
        match kimi_web_search_adapter::render_stream(&outcome.message) {
            Ok(body) => {
                if write_chunk(stream, &body).is_ok() {
                    finish_chunked_stream(stream);
                }
            }
            Err(error) => adapter_error(stream, head_written, 502, &error),
        }
    } else {
        match serde_json::to_vec(&outcome.message) {
            Ok(body) => {
                write_response(stream, 200, "OK", "application/json", &body);
            }
            Err(error) => {
                adapter_error(
                    stream,
                    head_written,
                    502,
                    &format!("Kimi Web Search adapter response serialization failed: {error}"),
                );
            }
        };
    }
}

fn safe_relay_server_tool_type(tool_type: &str) -> &str {
    if ["web_search_", "web_fetch_", "code_execution_"]
        .iter()
        .any(|prefix| {
            tool_type.strip_prefix(prefix).is_some_and(|version| {
                version.len() == 8 && version.bytes().all(|byte| byte.is_ascii_digit())
            })
        })
    {
        return tool_type;
    }
    match tool_type {
        "mcp_toolset" | "mcp_servers" => tool_type,
        value if value.starts_with("tool_search_tool_") => "tool_search_tool_*",
        value if value.starts_with("advisor_") => "advisor_*",
        _ => "other",
    }
}

fn relay_server_tool_types(request: &Value) -> String {
    let mut counts = BTreeMap::new();
    for tool in request
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(tool_type) = tool
            .get("type")
            .and_then(Value::as_str)
            .filter(|tool_type| !tool_type.is_empty())
        else {
            continue;
        };
        *counts
            .entry(safe_relay_server_tool_type(tool_type))
            .or_insert(0_usize) += 1;
    }
    if counts.is_empty() {
        "-".into()
    } else {
        counts
            .into_iter()
            .map(|(tool_type, count)| format!("{tool_type}:{count}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn relay_thinking_type(request: &Value) -> &'static str {
    match request.pointer("/thinking/type").and_then(Value::as_str) {
        Some("enabled") => "enabled",
        Some("adaptive") => "adaptive",
        Some("disabled") => "disabled",
        Some("auto") => "auto",
        Some(_) => "other",
        None => "-",
    }
}

fn log_relay_metadata(
    metadata: &AnthropicMetadata,
    request: &Value,
    is_stream: bool,
    message_count: usize,
) {
    let rules = if metadata.rule_ids.is_empty() {
        "-".to_string()
    } else {
        metadata.rule_ids.join(",")
    };
    crate::log_line!(
        "POST /v1/messages relay target={} stream={} msgs={} thinking_type={} budget_tokens={:?} max_tokens={:?} server_tool_types={} dropped_server_tools={} rules={}",
        metadata.target_model,
        is_stream,
        message_count,
        relay_thinking_type(request),
        request
            .pointer("/thinking/budget_tokens")
            .and_then(Value::as_u64),
        request.get("max_tokens").and_then(Value::as_u64),
        relay_server_tool_types(request),
        metadata.dropped_server_tools,
        rules
    );
}

fn handle_messages(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    body: Vec<u8>,
    anthropic_transport: Option<&messages::AnthropicTransport>,
    _relay_models: &models::RelayModelCache,
) {
    let mut raw: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            invalid_request_json(stream, &e.to_string());
            return;
        }
    };
    if !raw.is_object() {
        invalid_request_json(stream, "request body must be a JSON object");
        return;
    }
    let is_stream = raw.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if cfg.intent == crate::config::GatewayIntent::ScratchModels {
        route_unknown_json(stream, "scratch-models does not accept inference requests");
        return;
    }
    let requested_model = match raw.get("model").and_then(Value::as_str) {
        Some(model) if !model.trim().is_empty() => model,
        _ => {
            route_unknown_json(stream, "model selector is required");
            return;
        }
    };
    let Some(resolver) = cfg.static_model_resolver.as_ref() else {
        route_unknown_json(stream, "static model catalog is unavailable");
        return;
    };
    let Some(resolved_route) = resolver.resolve(requested_model) else {
        route_unknown_json(
            stream,
            "model selector is not present in the active profile",
        );
        return;
    };
    let target_model = resolved_route.upstream_model().to_string();
    // DeepSeek 的官方 /anthropic 端点与 Kimi 同为 Anthropic Messages 中继,
    // 共用契约驱动的补偿链;两者的差异全部落在 RelayFlavor 上。
    if cfg.provider == "relay" || cfg.provider == "deepseek" {
        let provider_contract_id = cfg
            .provider_contract
            .as_ref()
            .map(|contract| contract.contract_id.as_str());
        // 渠道联网搜索开关关闭:在补偿链之前摘掉 typed Web Search 声明,整条链
        // 之后一致地按「本轮没有搜索声明」处理,兼容桥也就不触发。
        let stripped_web_search = if cfg.web_search {
            0
        } else {
            anthropic_compat::strip_typed_web_search_tools(&mut raw)
        };
        let (mut transformed, mut metadata) =
            match anthropic_compat::transform_relay_request_for_contract(
                raw,
                &target_model,
                cfg.relay_thinking.as_deref(),
                provider_contract_id,
            ) {
                Ok(result) => result,
                Err(e) => {
                    invalid_request_json(stream, &e);
                    return;
                }
            };
        // 摘除发生在补偿链之前,规则序也放在最前,日志行读起来与实际顺序一致。
        if stripped_web_search > 0 {
            metadata.rule_ids.insert(
                0,
                anthropic_compat::RULE_TOOL_RELAY_WEB_SEARCH_DISABLED_BY_CONFIG.into(),
            );
        }
        let adapter =
            match kimi_web_search_adapter::prepare_request(&mut transformed, metadata.flavor) {
                Ok(adapter) => adapter,
                Err(error) => {
                    invalid_request_json(stream, &error);
                    return;
                }
            };
        if adapter.is_some() {
            metadata.rule_ids.push(
                kimi_web_search_adapter::RULE_PROVIDER_KIMI_WEB_SEARCH_QUERY_TOOL_ADAPTER.into(),
            );
        }
        let message_count = transformed
            .get("messages")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        log_relay_metadata(&metadata, &transformed, is_stream, message_count);
        if let Some(prepared) = adapter {
            handle_kimi_web_search_adapter(
                stream,
                cfg,
                transformed,
                anthropic_transport,
                is_stream,
                prepared,
            );
            return;
        }
        // 噪声整形对全部 Kimi 流量无条件生效:上游的噪声头 / 幻影对 / 配对键
        // 漂移并不以「本轮声明了 typed search」为前提(历史轮、自发 server tool
        // 都出现过),而零命中流量在过滤器内保持字节级原样,无代价。
        // DeepSeek 与 Generic 流量不经过过滤器。
        let use_noise_filter = metadata.flavor == anthropic_compat::RelayFlavor::Kimi;
        let transformed = match serde_json::to_vec(&transformed) {
            Ok(body) => body,
            Err(e) => {
                invalid_request_json(stream, &e.to_string());
                return;
            }
        };
        let noise_filter = use_noise_filter.then(kimi_search_noise::SearchNoiseFilter::new);
        if is_stream {
            handle_stream(stream, cfg, transformed, anthropic_transport, noise_filter);
            return;
        }
        match messages::post_nonstream(cfg, transformed, anthropic_transport) {
            Ok(mut resp) => {
                if noise_filter.is_some() && resp.status == 200 {
                    if let Ok(mut body) = serde_json::from_slice::<Value>(&resp.body) {
                        let stats = kimi_search_noise::strip_nonstream_noise(&mut body);
                        if stats.any_activity() {
                            crate::log_line!(
                                "relay nonstream rules={} noise={} pair={} bytes={}{}",
                                stream_strip_rules(&stats),
                                stats.noise_blocks,
                                stats.pair_blocks,
                                stats.bytes,
                                strip_stats_suffix(&stats)
                            );
                        }
                        // 采钥即使零剥离也改写了 body,必须重序列化。
                        if stats.rewrote_body() {
                            match serde_json::to_vec(&body) {
                                Ok(stripped) => resp.body = stripped,
                                Err(error) => {
                                    crate::log_line!(
                                        "relay nonstream noise strip serialization failed: {error}"
                                    );
                                    api_error_json(
                                        stream,
                                        502,
                                        "CSSwitch response filter: stripped body serialization failed",
                                    );
                                    return;
                                }
                            }
                        }
                    }
                }
                write_response(
                    stream,
                    resp.status,
                    status_reason(resp.status),
                    &resp.content_type,
                    &resp.body,
                );
            }
            Err(e) => report_upstream_failure(stream, "post_nonstream", &e),
        }
    }
}

fn handle_post(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    target: &str,
    head: &RequestHead,
    relay_models: &models::RelayModelCache,
) {
    let path = match strip_path_secret(dequery(target), cfg.auth_secret.as_deref()) {
        AuthResult::Ok(path) => path,
        AuthResult::Forbidden => {
            forbidden_json(stream);
            return;
        }
    };
    let len = match content_length(&head.headers) {
        Ok(len) => len,
        Err(e) => {
            invalid_request_json(stream, &e);
            return;
        }
    };
    let body = if len == 0 {
        b"{}".to_vec()
    } else {
        match read_body(stream, len) {
            Ok(body) => body,
            Err(e) => {
                invalid_request_json(stream, &e);
                return;
            }
        }
    };
    if path != "/v1/messages" {
        not_found_json(stream, &path);
        return;
    }
    let anthropic_transport = match head.anthropic_transport() {
        Ok(transport) => Some(transport),
        Err(error) => {
            invalid_request_json(stream, &error);
            return;
        }
    };
    handle_messages(
        stream,
        cfg,
        body,
        anthropic_transport.as_ref(),
        relay_models,
    );
}

fn handle_one(
    cfg: GatewayConfig,
    mut stream: TcpStream,
    relay_models: Arc<models::RelayModelCache>,
) {
    let head = match read_head(&mut stream) {
        Ok(head) => head,
        Err(e) => {
            invalid_request_json(&mut stream, &e);
            return;
        }
    };
    match head.method.as_str() {
        "CONNECT" => connect::handle_connect(&head.target, stream),
        "GET" => handle_get(&mut stream, &cfg, &head.target, &relay_models),
        "POST" => {
            let target = head.target.clone();
            handle_post(&mut stream, &cfg, &target, &head, &relay_models)
        }
        _ => not_found_json(&mut stream, &head.target),
    }
}

pub fn serve(cfg: GatewayConfig) -> Result<(), String> {
    let relay_models = Arc::new(models::RelayModelCache::default());
    let listener = TcpListener::bind(("127.0.0.1", cfg.port)).map_err(|e| e.to_string())?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let cfg = cfg.clone();
                let relay_models = Arc::clone(&relay_models);
                thread::spawn(move || handle_one(cfg, stream, relay_models));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

/// 单进程服务的推理入口:按当前激活模式路由。
/// 官方模式整条透传官方上游;渠道模式按契约装配一次性 GatewayConfig 走补偿链。
pub fn handle_inference(
    stream: &mut TcpStream,
    method: &str,
    target: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    state: &Arc<crate::control::AppState>,
) {
    let timer = crate::control::Timer::start();
    let profile = state
        .profile
        .read()
        .map(|profile| profile.clone())
        .unwrap_or_default();
    let path = dequery(target);

    if profile.mode == crate::profile::Mode::Official {
        let outcome = crate::official_passthrough::forward(
            stream,
            method,
            target,
            &headers,
            body,
            crate::official_passthrough::DEFAULT_UPSTREAM,
        );
        state.record(crate::control::log_entry(
            "inference",
            json!({
                "mode": "official",
                "method": method,
                "path": path,
                "status": outcome.status,
                "sse": outcome.sse,
                "ms": timer.elapsed_ms(),
            }),
        ));
        return;
    }

    let cfg = match crate::config::GatewayConfig::for_channel(&profile) {
        Ok(cfg) => cfg,
        Err(error) => {
            state.record(crate::control::log_entry(
                "inference",
                json!({"mode": profile.mode.as_str(), "path": path, "error": error.clone()}),
            ));
            api_error_json(stream, 502, &error);
            return;
        }
    };
    let head = RequestHead {
        method: method.to_string(),
        target: target.to_string(),
        headers: headers.into_iter().collect(),
    };
    let relay_models = models::RelayModelCache::default();
    match method {
        "GET" => handle_get(stream, &cfg, target, &relay_models),
        // body 已由服务层读走,不能再走 handle_post(它会自己读 socket 并永远阻塞)。
        "POST" if path == "/v1/messages" => {
            let transport = match head.anthropic_transport() {
                Ok(transport) => Some(transport),
                Err(error) => {
                    invalid_request_json(stream, &error);
                    return;
                }
            };
            let body = if body.is_empty() {
                b"{}".to_vec()
            } else {
                body
            };
            handle_messages(stream, &cfg, body, transport.as_ref(), &relay_models);
        }
        _ => not_found_json(stream, path),
    }
    state.record(crate::control::log_entry(
        "inference",
        json!({
            "mode": profile.mode.as_str(),
            "method": method,
            "path": path,
            "ms": timer.elapsed_ms(),
        }),
    ));
}

#[cfg(test)]
mod tests {

    use std::io::{Cursor, Error, ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    use serde_json::{json, Value};

    use super::{
        forward_stream_body, handle_kimi_web_search_adapter, handle_messages, merged_shape,
        pair_key_prefix, relay_server_tool_types, relay_thinking_type, stream_error_event,
        stream_strip_rules, strip_stats_suffix, StreamTermination,
    };

    #[test]
    fn disabled_channel_web_search_never_reaches_upstream() {
        // 只测 strip 函数证明不了 `cfg.web_search` 真的被读到了 —— 本仓库刚被
        // 一个恒假的门控谓词坑过(整条主路径成死代码而单测仍绿)。这条测试从
        // handle_messages 一路打到 fake 上游,看真正发出去的报文。
        //
        // 断言落在"上游收到的 tools 只有 Bash"这一条上,它同时排除两种失败:
        // 摘除没生效(会看到 typed web_search),以及桥仍然触发(会看到桥的私有
        // 查询工具 —— 它同样叫 web_search)。
        let upstream = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let upstream_url = format!("http://{}/v1/messages", upstream.local_addr().unwrap());
        let (requests_tx, requests_rx) = mpsc::channel();
        let upstream_thread = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().unwrap();
            let request = read_http_json(&mut stream);
            write_http_json(
                &mut stream,
                &json!({
                    "id": "main", "type": "message", "role": "assistant", "model": "k3",
                    "content": [{"type": "text", "text": "我这边没有联网检索能力。"}],
                    "stop_reason": "end_turn", "stop_sequence": null,
                    "usage": {"input_tokens": 3, "output_tokens": 4}
                }),
            );
            requests_tx.send(request).unwrap();
        });

        let channel = crate::profile::Channel::from_defaults_for_test();
        let resolver = crate::static_profile::StaticProfileResolver::from_json(
            &channel.static_catalog("relay").to_string(),
        )
        .unwrap();
        let contract = crate::provider_contracts::load_runtime_contract(
            "relay",
            Some("kimi-anthropic-relay"),
            Some(&crate::provider_contracts::catalog_digest()),
        )
        .unwrap();
        let cfg = crate::config::GatewayConfig {
            provider: "relay".into(),
            port: 0,
            auth_secret: None,
            api_key: Some("fake-key".into()),
            upstream_url,
            models_url: None,
            relay_thinking: None,
            provider_contract: Some(contract),
            intent: crate::config::GatewayIntent::Formal,
            static_model_resolver: Some(resolver),
            shim_mode: "off".into(),
            launch_id: "web-search-toggle-test".into(),
            // 用户在控制台把这条渠道的联网搜索关掉了。
            web_search: false,
        };

        let body = serde_json::to_vec(&json!({
            "model": "claude-opus-5",
            "max_tokens": 1024,
            "stream": false,
            "messages": [{"role": "user", "content": "查一下今天的新闻"}],
            "tools": [
                {"type": "web_search_20250305", "name": "web_search"},
                {"name": "Bash", "input_schema": {"type": "object"}}
            ]
        }))
        .unwrap();

        let downstream_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let downstream_addr = downstream_listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(downstream_addr).unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            response
        });
        let (mut downstream, _) = downstream_listener.accept().unwrap();
        handle_messages(
            &mut downstream,
            &cfg,
            body,
            None,
            &crate::models::RelayModelCache::default(),
        );
        drop(downstream);
        let response = String::from_utf8(client.join().unwrap()).unwrap();
        upstream_thread.join().unwrap();

        let upstream_request = requests_rx.recv().unwrap();
        let tools = upstream_request["tools"].as_array().unwrap();
        assert_eq!(
            tools.len(),
            1,
            "关闭态下上游只应看到普通客户端工具,实际收到 {tools:?}"
        );
        assert_eq!(tools[0]["name"], "Bash");
        assert!(response.starts_with("HTTP/1.1 200"), "回答仍要正常送达");
    }

    #[test]
    fn adapter_log_projections_expose_shapes_and_key_prefixes_only() {
        let message = json!({
            "content": [
                {"type": "thinking", "thinking": "private"},
                {"type": "server_tool_use", "id": "srvtoolu_abc", "name": "web_search",
                 "input": {"query": "private query"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_abc",
                 "content": [{"type": "web_search_result", "url": "https://private.test"}]},
                {"type": "web_search_tool_result", "tool_use_id": "tool_def", "content": []},
                {"type": "web_search_tool_result", "content": []},
                {"type": "vendor_private_block", "secret": "must-not-leak"},
                {"type": "text", "text": "private answer"}
            ]
        });
        assert_eq!(
            merged_shape(&message),
            "thinking,server_tool_use,web_search_tool_result,web_search_tool_result,web_search_tool_result,other,text"
        );
        assert_eq!(pair_key_prefix(&message), "srvtoolu,tool,none");
        let line = format!("{} {}", merged_shape(&message), pair_key_prefix(&message));
        assert!(!line.contains("private"));
        assert!(!line.contains("srvtoolu_abc"));

        assert_eq!(merged_shape(&json!({"content": []})), "-");
        assert_eq!(pair_key_prefix(&json!({"content": []})), "-");
    }

    fn read_http_json(stream: &mut TcpStream) -> serde_json::Value {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        let expected = loop {
            let read = stream
                .read(&mut buffer)
                .expect("read fake upstream request");
            assert!(read > 0, "request ended before its body");
            request.extend_from_slice(&buffer[..read]);
            let Some(head_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&request[..head_end]);
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (head_end + 4, head_end + 4 + length);
        };
        while request.len() < expected.1 {
            let read = stream.read(&mut buffer).expect("read fake upstream body");
            assert!(read > 0, "request body ended early");
            request.extend_from_slice(&buffer[..read]);
        }
        serde_json::from_slice(&request[expected.0..expected.1]).unwrap()
    }

    fn write_http_json(stream: &mut TcpStream, value: &serde_json::Value) {
        let body = serde_json::to_vec(value).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
        stream.flush().unwrap();
    }

    fn run_kimi_query_adapter_fake(model: &'static str) {
        let upstream = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let upstream_url = format!("http://{}/v1/messages", upstream.local_addr().unwrap());
        let (requests_tx, requests_rx) = mpsc::channel();
        let upstream_thread = thread::spawn(move || {
            let mut captured = Vec::new();
            let calls = if model == "k3" { 3 } else { 2 };
            for stage in 0..calls {
                let (mut stream, _) = upstream.accept().unwrap();
                let request = read_http_json(&mut stream);
                captured.push(request.clone());
                let response = if stage == 0 {
                    let internal = request["tools"][0]["name"].as_str().unwrap();
                    json!({
                        "id": "main", "type": "message", "role": "assistant", "model": model,
                        "content": [{"type": "tool_use", "id": "internal_1", "name": internal,
                                     "input": {"query": "Rust stable release"}}],
                        "stop_reason": "tool_use", "stop_sequence": null,
                        "usage": {"input_tokens": 5, "output_tokens": 2}
                    })
                } else if stage == 1 {
                    let mut content = vec![
                        json!({"type": "text", "text": "Search results for query: Rust stable release"}),
                        json!({"type": "server_tool_use", "id": "tool_original", "name": "web_search",
                               "input": {"query": "Rust stable release"}}),
                        json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
                            {"type": "web_search_result", "url": "https://example.test/rust"}
                        ]}),
                    ];
                    if model != "k3" {
                        content.push(json!({"type": "text", "text": "Rust is current."}));
                    } else {
                        content.push(json!({
                            "type": "tool_use", "id": "hallucinated_search_tail",
                            "name": "web_search", "input": {"query": "duplicate"}
                        }));
                    }
                    json!({
                        "id": "nested", "type": "message", "role": "assistant", "model": model,
                        "content": content,
                        "stop_reason": if model == "k3" { "tool_use" } else { "end_turn" },
                        "stop_sequence": null,
                        "usage": {"input_tokens": 7, "output_tokens": 9}
                    })
                } else {
                    json!({
                        "id": "synthesis", "type": "message", "role": "assistant", "model": model,
                        "content": [{"type": "text", "text": "Synthesized answer."}],
                        "stop_reason": "end_turn", "stop_sequence": null,
                        "usage": {"input_tokens": 3, "output_tokens": 5}
                    })
                };
                write_http_json(&mut stream, &response);
            }
            requests_tx.send(captured).unwrap();
        });

        let mut main_request = json!({
            "model": model, "max_tokens": 128000, "stream": true,
            "messages": [{"role": "user", "content": "latest Rust?"}],
            "tools": [
                {"type": "web_search_20250305", "name": "web_search"},
                {"name": "Bash", "input_schema": {"type": "object"}}
            ]
        });
        let prepared = crate::kimi_web_search_adapter::prepare_request(
            &mut main_request,
            crate::anthropic_compat::RelayFlavor::Kimi,
        )
        .unwrap()
        .unwrap();
        let cfg = crate::config::GatewayConfig {
            provider: "relay".into(),
            port: 0,
            auth_secret: None,
            api_key: Some("fake-key".into()),
            upstream_url,
            models_url: None,
            relay_thinking: None,
            provider_contract: None,
            intent: crate::config::GatewayIntent::Formal,
            static_model_resolver: None,
            shim_mode: "off".into(),
            launch_id: "kimi-adapter-test".into(),
            web_search: true,
        };
        let downstream_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let downstream_addr = downstream_listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(downstream_addr).unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            response
        });
        let (mut downstream, _) = downstream_listener.accept().unwrap();
        handle_kimi_web_search_adapter(&mut downstream, &cfg, main_request, None, true, prepared);
        drop(downstream);
        let response = String::from_utf8(client.join().unwrap()).unwrap();
        upstream_thread.join().unwrap();
        let requests = requests_rx.recv().unwrap();

        assert_eq!(requests.len(), if model == "k3" { 3 } else { 2 });
        assert_eq!(requests[0]["max_tokens"], 128000);
        assert_eq!(requests[1]["max_tokens"], 4096);
        assert!(requests[0]["tools"][0].get("type").is_none());
        assert_eq!(
            requests[1]["tool_choice"],
            json!({"type": "tool", "name": "web_search"}),
            "{model}"
        );
        assert_eq!(
            requests[1]["thinking"],
            json!({"type": "disabled"}),
            "{model}"
        );
        assert_eq!(requests[1]["tools"].as_array().unwrap().len(), 1);
        if model == "k3" {
            let synthesis = &requests[2];
            assert_eq!(synthesis["max_tokens"], 8192);
            let messages = synthesis["messages"].as_array().unwrap();
            assert_eq!(
                messages[messages.len() - 2]["content"][0]["id"],
                "internal_1"
            );
            assert_eq!(
                messages[messages.len() - 1]["content"][0]["tool_use_id"],
                "internal_1"
            );
            assert_eq!(messages[messages.len() - 1]["content"][1]["type"], "text");
            assert!(synthesis["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool.get("name").and_then(Value::as_str) == Some("Bash") }));
            assert!(response.contains("Synthesized answer."));
            assert!(!response.contains("hallucinated_search_tail"));
        }
        assert_eq!(response.matches("event: message_start").count(), 1);
        assert_eq!(response.matches("event: message_stop").count(), 1);
        assert!(response.contains("srvtoolu_1"));
        assert!(!response.contains("tool_original"));
        assert!(!response.contains("internal_1"));
        assert!(!response.contains("Search results for query:"));
        // First-byte progress: the head goes out before any upstream call and
        // each completed stage leaves a comment frame ahead of message_start.
        assert!(response.contains(": bridge processing"));
        assert!(response.contains(": main stage complete"));
        assert!(response.contains(": nested stage complete"));
        assert!(
            response.find(": bridge processing").unwrap()
                < response.find("event: message_start").unwrap()
        );
    }

    #[test]
    fn kimi_query_adapter_forces_the_same_nested_policy_for_every_model() {
        for model in ["k3", "k3-256k", "kimi-for-coding"] {
            run_kimi_query_adapter_fake(model);
        }
    }

    #[test]
    fn kimi_search_adoption_rule_and_stats_are_logged_without_false_rules() {
        let adopted = crate::kimi_search_noise::StripStats {
            adopted_pairs: 2,
            ..Default::default()
        };
        assert_eq!(
            stream_strip_rules(&adopted),
            crate::kimi_search_noise::RULE_PROVIDER_KIMI_SEARCH_PAIR_ID_ADOPT
        );
        assert_eq!(strip_stats_suffix(&adopted), " adopted=2");

        let unkeyed = crate::kimi_search_noise::StripStats {
            unkeyed_pairs: 1,
            ..Default::default()
        };
        assert_eq!(stream_strip_rules(&unkeyed), "-");
        assert_eq!(strip_stats_suffix(&unkeyed), " adopted=0 unkeyed=1");
    }

    struct FailingReader;

    #[test]
    fn relay_metadata_projects_only_safe_server_tool_types_and_thinking_enum() {
        let request = json!({
            "max_tokens": 8192,
            "thinking": {
                "type": "enabled",
                "budget_tokens": 1024,
                "sidecar": "sidecar-name-must-not-leak",
            },
            "tools": [
                {
                    "type": "web_search_20250305",
                    "name": "tool-name-must-not-leak",
                    "input_schema": {"secret": "schema-must-not-leak"},
                },
                {"type": "web_search_20260803", "description": "must-not-leak"},
                {"type": "vendor_private_sidecar", "name": "must-not-leak"},
                {"name": "ordinary-client-tool", "input_schema": {}},
            ],
        });
        let types = relay_server_tool_types(&request);
        assert_eq!(types, "other:1,web_search_20250305:1,web_search_20260803:1");
        assert_eq!(relay_thinking_type(&request), "enabled");
        assert!(!types.contains("private"));
        assert_eq!(
            relay_thinking_type(&json!({"thinking": {"type": "secret-sidecar"}})),
            "other"
        );
    }

    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(Error::new(ErrorKind::ConnectionReset, "mock read failure"))
        }
    }

    struct CountingEofReader {
        reads: usize,
    }

    impl Read for CountingEofReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            Ok(0)
        }
    }

    fn kimi_complete_then_partial() -> Vec<u8> {
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"buffered\"}}"
        )
        .as_bytes()
        .to_vec()
    }

    fn complete_kimi_envelope() -> Vec<u8> {
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"plan\",\"signature\":\"opaque\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"srv_1\",\"name\":\"web_search\",\"input\":{\"query\":\"weather\"}}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"web_search_tool_result\",\"tool_use_id\":\"srv_1\",\"content\":[]}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":9}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
        .as_bytes()
        .to_vec()
    }

    #[test]
    fn native_anthropic_read_error_is_terminal_after_forwarding_first_bytes() {
        let first = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\"}"
        )
        .as_bytes();
        let mut upstream = FailingReader;
        let mut output = Vec::new();

        let termination = forward_stream_body(&mut upstream, first, None, |chunk| {
            output.extend_from_slice(chunk);
            Ok(())
        });

        assert_eq!(termination, StreamTermination::UpstreamReadError);
        assert!(String::from_utf8_lossy(&output).contains("event: message_start"));
        assert!(output.ends_with(&stream_error_event("upstream stream read failed")));
    }

    #[test]
    fn kimi_native_anthropic_sse_preserves_server_tool_lifecycle_verbatim() {
        let first = complete_kimi_envelope();
        let mut upstream = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let termination = forward_stream_body(&mut upstream, &first, None, |chunk| {
            output.extend_from_slice(chunk);
            Ok(())
        });
        let text = String::from_utf8(output).unwrap();
        assert_eq!(termination, StreamTermination::NormalEof);
        assert!(text.contains("\"thinking\":\"plan\""));
        assert!(text.contains("\"signature\":\"opaque\""));
        assert!(text.contains("\"type\":\"server_tool_use\""));
        assert!(text.contains("\"type\":\"web_search_tool_result\""));
        assert!(text.contains("\"tool_use_id\":\"srv_1\""));
        assert!(text.contains("\"index\":3"));
        assert!(text.contains("\"stop_reason\":\"end_turn\""));
        assert!(text.contains("\"output_tokens\":9"));
        assert_eq!(text.matches("event: message_stop").count(), 1);
        assert!(!text.contains("event: error"));
    }

    #[test]
    fn downstream_write_error_stops_before_more_upstream_reads() {
        let first = kimi_complete_then_partial();
        let mut upstream = CountingEofReader { reads: 0 };

        let termination = forward_stream_body(&mut upstream, &first, None, |_chunk| {
            Err(Error::new(ErrorKind::BrokenPipe, "mock client closed"))
        });

        assert_eq!(termination, StreamTermination::DownstreamWriteError);
        assert_eq!(
            upstream.reads, 0,
            "must stop reading after client write failure"
        );
    }
}
