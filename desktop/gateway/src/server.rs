use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use serde_json::{json, Value};

use crate::auth::{strip_path_secret, AuthResult};
use crate::config::GatewayConfig;
use crate::{
    anthropic_compat::{self, AnthropicMetadata},
    connect, messages, models,
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
    eprintln!("{}", messages::upstream_failure_metadata(operation, error));
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

fn forward_stream_body<R, F>(upstream: &mut R, first: &[u8], emit: F) -> StreamTermination
where
    R: Read,
    F: FnMut(&[u8]) -> std::io::Result<()>,
{
    let mut success_rollback = None;
    forward_stream_body_with_capture(upstream, first, None, &mut success_rollback, emit, |_| {
        Ok(None)
    })
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

    if let Some(termination) = process(first, &mut validator, &mut collector, &mut emit) {
        return termination;
    }

    let mut buf = [0_u8; 8192];
    loop {
        match upstream.read(&mut buf) {
            Ok(0) => {
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
                                            eprintln!("thinking continuity rollback failed");
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
                if let Some(termination) =
                    process(&buf[..n], &mut validator, &mut collector, &mut emit)
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

fn handle_stream(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    body: Vec<u8>,
    transport: Option<&messages::AnthropicTransport>,
) {
    let mut upstream = match messages::open_stream(cfg, body, transport) {
        Ok(upstream) => upstream,
        Err(error) => {
            report_upstream_failure(stream, "open_stream", &error);
            return;
        }
    };
    if write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
    )
    .and_then(|_| stream.flush())
    .is_err()
    {
        return;
    }
    let termination = forward_stream_body(&mut upstream.response, &[], |chunk| {
        write_chunk(stream, chunk)
    });
    match termination {
        StreamTermination::NormalEof => {}
        StreamTermination::UpstreamTerminalError
        | StreamTermination::UpstreamReadError
        | StreamTermination::ProtocolError => {}
        StreamTermination::DownstreamWriteError => return,
    }
    let _ = stream.write_all(b"0\r\n\r\n").and_then(|_| stream.flush());
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
    eprintln!(
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
    let raw: Value = match serde_json::from_slice(&body) {
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
        let (transformed, metadata) = match anthropic_compat::transform_relay_request_for_contract(
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
        let message_count = transformed
            .get("messages")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        log_relay_metadata(&metadata, &transformed, is_stream, message_count);
        let transformed = match serde_json::to_vec(&transformed) {
            Ok(body) => body,
            Err(e) => {
                invalid_request_json(stream, &e.to_string());
                return;
            }
        };
        if is_stream {
            handle_stream(stream, cfg, transformed, anthropic_transport);
            return;
        }
        match messages::post_nonstream(cfg, transformed, anthropic_transport) {
            Ok(resp) => {
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

    use std::io::{Cursor, Error, ErrorKind, Read};

    use serde_json::json;

    use super::{
        forward_stream_body, relay_server_tool_types, relay_thinking_type, stream_error_event,
        StreamTermination,
    };

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

        let termination = forward_stream_body(&mut upstream, first, |chunk| {
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
        let termination = forward_stream_body(&mut upstream, &first, |chunk| {
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

        let termination = forward_stream_body(&mut upstream, &first, |_chunk| {
            Err(Error::new(ErrorKind::BrokenPipe, "mock client closed"))
        });

        assert_eq!(termination, StreamTermination::DownstreamWriteError);
        assert_eq!(
            upstream.reads, 0,
            "must stop reading after client write failure"
        );
    }
}
