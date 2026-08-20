use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use reqwest::blocking::{Client, Response};
use reqwest::header::HeaderValue;
use serde_json::Value;

use crate::config::{GatewayConfig, UPSTREAM_UA};
use crate::provider_contracts::AuthScheme;

#[derive(Debug)]
pub struct UpstreamBody {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub struct UpstreamError {
    pub status: u16,
    pub upstream_status: Option<u16>,
    pub detail: String,
}

pub(crate) fn upstream_failure_metadata(operation: &'static str, error: &UpstreamError) -> String {
    let base = format!(
        "POST /v1/messages upstream_failure operation={operation} status={} upstream_status={:?}",
        error.status, error.upstream_status
    );
    match upstream_error_diagnostic(error) {
        Some(diagnostic) => format!("{base} {diagnostic}"),
        None => base,
    }
}

/// 诊断开关 `CSSWITCH_DEBUG_UPSTREAM_ERROR=1` 下附带上游错误正文,用于定位
/// 供应商兼容缺陷。仅在上游真的回了 HTTP 状态时输出——传输层失败的 detail 里
/// 可能带 URL 与 query 凭证,那条路径保持不打印。
fn upstream_error_diagnostic(error: &UpstreamError) -> Option<String> {
    if std::env::var("CSSWITCH_DEBUG_UPSTREAM_ERROR")
        .ok()
        .as_deref()
        != Some("1")
    {
        return None;
    }
    error.upstream_status?;
    let detail: String = error
        .detail
        .chars()
        .filter(|c| !c.is_control())
        .take(400)
        .collect();
    Some(format!("upstream_detail={detail}"))
}

#[derive(Debug)]
pub struct UpstreamStream {
    pub response: Response,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InferenceTimeouts {
    connect: Duration,
    total: Duration,
    read_idle: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModelsTimeouts {
    connect: Duration,
    total: Duration,
    read_idle: Duration,
}

// Standalone/test launches retain the legacy 300-second limits. Managed
// launches replace all three values from the validated provider contract.
const INFERENCE_TIMEOUTS: InferenceTimeouts = InferenceTimeouts {
    connect: Duration::from_secs(300),
    total: Duration::from_secs(300),
    read_idle: Duration::from_secs(300),
};

const MAX_ERROR_BODY_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, Default)]
pub struct AnthropicTransport {
    anthropic_version: Option<String>,
    anthropic_beta: Option<String>,
    x_app: Option<String>,
    user_agent: Option<String>,
    beta_query: bool,
}

impl AnthropicTransport {
    pub(crate) fn from_inbound(
        target: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Self, String> {
        fn allowed_header(
            headers: &HashMap<String, String>,
            name: &str,
        ) -> Result<Option<String>, String> {
            if headers.get("connection").is_some_and(|connection| {
                connection
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case(name))
            }) {
                return Ok(None);
            }
            let Some(value) = headers.get(name) else {
                return Ok(None);
            };
            let value = HeaderValue::from_bytes(value.as_bytes())
                .map_err(|_| format!("invalid {name} header"))?;
            let value = value
                .to_str()
                .map_err(|_| format!("invalid {name} header"))?;
            Ok(Some(value.to_string()))
        }

        let beta_query = target
            .split_once('?')
            .map(|(_, query)| query.split_once('#').map_or(query, |(query, _)| query))
            .is_some_and(|query| {
                url::form_urlencoded::parse(query.as_bytes())
                    .any(|(name, value)| name == "beta" && value == "true")
            });
        Ok(Self {
            anthropic_version: allowed_header(headers, "anthropic-version")?,
            anthropic_beta: allowed_header(headers, "anthropic-beta")?,
            x_app: allowed_header(headers, "x-app")?,
            user_agent: allowed_header(headers, "user-agent")?,
            beta_query,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
enum BodyReadFailure {
    Deadline,
    Io(String),
}

/// Blocking reqwest exposes a resettable per-operation timeout but cannot
/// shorten an in-flight body read to the request's remaining cumulative
/// deadline. Move the response into a bounded worker and stop waiting exactly
/// at the remaining deadline. Cancellation is observed after the current
/// bounded read, so a timed-out request cannot leave an unbounded reader.
fn read_body_with_deadline(
    mut response: Response,
    limit: Option<u64>,
    started: Instant,
    total: Duration,
) -> Result<Vec<u8>, BodyReadFailure> {
    let remaining = total
        .checked_sub(started.elapsed())
        .ok_or(BodyReadFailure::Deadline)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut body = Vec::new();
        let mut chunk = [0_u8; 8192];
        let result = loop {
            if worker_cancelled.load(Ordering::Relaxed) {
                break Err(BodyReadFailure::Deadline);
            }
            let capacity = limit
                .map(|max| {
                    max.saturating_sub(body.len() as u64)
                        .min(chunk.len() as u64)
                })
                .unwrap_or(chunk.len() as u64) as usize;
            if capacity == 0 {
                break Ok(body);
            }
            match response.read(&mut chunk[..capacity]) {
                Ok(0) => break Ok(body),
                Ok(read) => {
                    body.extend_from_slice(&chunk[..read]);
                    if worker_cancelled.load(Ordering::Relaxed) {
                        break Err(BodyReadFailure::Deadline);
                    }
                }
                Err(error) => break Err(BodyReadFailure::Io(error.to_string())),
            }
        };
        let _ = tx.send(result);
    });
    match rx.recv_timeout(remaining) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            cancelled.store(true, Ordering::Relaxed);
            Err(BodyReadFailure::Deadline)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(BodyReadFailure::Io(
            "upstream body reader terminated unexpectedly".into(),
        )),
    }
}

fn auth_scheme(cfg: &GatewayConfig) -> AuthScheme {
    cfg.provider_contract
        .as_ref()
        .map(|contract| contract.auth_scheme)
        .unwrap_or_else(|| match cfg.provider.as_str() {
            "relay" => AuthScheme::AnthropicDual,
            _ => AuthScheme::AnthropicXApiKey,
        })
}

fn messages_post_url<'a>(
    cfg: &'a GatewayConfig,
    transport: Option<&AnthropicTransport>,
) -> Result<Cow<'a, str>, UpstreamError> {
    if transport.is_none() {
        return Ok(Cow::Borrowed(&cfg.upstream_url));
    }
    let mut url = reqwest::Url::parse(&cfg.upstream_url).map_err(|_| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: "Anthropic upstream URL is invalid".into(),
    })?;
    let mut beta_query = transport.is_some_and(|transport| transport.beta_query);
    if !beta_query && !url.query_pairs().any(|(key, _)| key == "beta") {
        return Ok(Cow::Borrowed(&cfg.upstream_url));
    }
    let existing = url
        .query_pairs()
        .filter_map(|(key, value)| {
            if key == "beta" {
                beta_query |= value == "true";
                None
            } else {
                Some((key.into_owned(), value.into_owned()))
            }
        })
        .collect::<Vec<_>>();
    url.set_query(None);
    if !existing.is_empty() || beta_query {
        let mut query = url.query_pairs_mut();
        for (key, value) in existing {
            query.append_pair(&key, &value);
        }
        if beta_query {
            query.append_pair("beta", "true");
        }
    }
    Ok(Cow::Owned(url.into()))
}

fn merged_anthropic_beta(transport: Option<&AnthropicTransport>) -> Option<String> {
    transport
        .and_then(|transport| transport.anthropic_beta.as_deref())
        .map(str::to_string)
}

fn models_timeout_secs(_provider: &str) -> u64 {
    120
}

fn models_timeouts(cfg: &GatewayConfig) -> ModelsTimeouts {
    cfg.provider_contract
        .as_ref()
        .map(|contract| ModelsTimeouts {
            connect: contract.connect_timeout,
            total: contract.request_timeout,
            read_idle: contract.read_idle_timeout,
        })
        .unwrap_or_else(|| {
            let legacy = Duration::from_secs(models_timeout_secs(&cfg.provider));
            ModelsTimeouts {
                connect: legacy,
                total: legacy,
                read_idle: legacy,
            }
        })
}

fn inference_client(
    timeouts: InferenceTimeouts,
    enforce_total: bool,
) -> Result<Client, UpstreamError> {
    // Blocking reqwest reapplies `timeout` to request send and every Response::read.
    // Finite requests therefore use the tighter per-operation bound and also enforce
    // the cumulative total deadline while consuming the body. Active SSE streams use
    // only the resettable read-idle bound and intentionally have no cumulative limit.
    let operation_timeout = if enforce_total {
        timeouts.total.min(timeouts.read_idle)
    } else {
        timeouts.read_idle
    };
    let builder = Client::builder()
        .connect_timeout(timeouts.connect)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(operation_timeout);
    builder.build().map_err(|e| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: e.to_string(),
    })
}

fn models_client(timeouts: ModelsTimeouts) -> Result<Client, UpstreamError> {
    Client::builder()
        .connect_timeout(timeouts.connect)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeouts.total.min(timeouts.read_idle))
        .build()
        .map_err(|e| UpstreamError {
            status: 502,
            upstream_status: None,
            detail: e.to_string(),
        })
}

fn post_with_timeouts(
    cfg: &GatewayConfig,
    body: Vec<u8>,
    timeouts: InferenceTimeouts,
    enforce_total: bool,
    transport: Option<&AnthropicTransport>,
) -> Result<Response, UpstreamError> {
    let api_key = cfg.api_key.as_deref().unwrap_or("");
    let upstream_url = messages_post_url(cfg, transport)?;
    let user_agent = transport
        .and_then(|transport| transport.user_agent.as_deref())
        .unwrap_or(UPSTREAM_UA);
    let mut request = inference_client(timeouts, enforce_total)?
        .post(upstream_url.as_ref())
        .header("content-type", "application/json")
        .header("user-agent", user_agent);
    if let Some(version) = transport.and_then(|transport| transport.anthropic_version.as_deref()) {
        request = request.header("anthropic-version", version);
    } else if transport.is_some()
        || matches!(
            auth_scheme(cfg),
            AuthScheme::AnthropicDual | AuthScheme::AnthropicXApiKey
        )
    {
        request = request.header("anthropic-version", "2023-06-01");
    }
    if let Some(beta) = merged_anthropic_beta(transport) {
        request = request.header("anthropic-beta", beta);
    }
    if let Some(x_app) = transport.and_then(|transport| transport.x_app.as_deref()) {
        request = request.header("x-app", x_app);
    }
    request = match auth_scheme(cfg) {
        AuthScheme::Bearer => request.header("authorization", format!("Bearer {api_key}")),
        AuthScheme::AnthropicDual => request
            .header("x-api-key", api_key)
            .header("authorization", format!("Bearer {api_key}")),
        AuthScheme::AnthropicXApiKey => request.header("x-api-key", api_key),
        AuthScheme::CsswitchOauth => request,
    };
    request.body(body).send().map_err(|e| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: e.to_string(),
    })
}

fn inference_timeouts(cfg: &GatewayConfig) -> InferenceTimeouts {
    cfg.provider_contract
        .as_ref()
        .map(|contract| InferenceTimeouts {
            connect: contract.connect_timeout,
            total: contract.request_timeout,
            read_idle: contract.read_idle_timeout,
        })
        .unwrap_or(INFERENCE_TIMEOUTS)
}

fn get_once(cfg: &GatewayConfig, url: &str) -> Result<UpstreamBody, UpstreamError> {
    let timeouts = models_timeouts(cfg);
    let started = Instant::now();
    let api_key = cfg.api_key.as_deref().unwrap_or("");
    let request = models_client(timeouts)?
        .get(url)
        .header("user-agent", UPSTREAM_UA);
    let request = match auth_scheme(cfg) {
        AuthScheme::Bearer => request.header("authorization", format!("Bearer {api_key}")),
        AuthScheme::AnthropicDual => request
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", api_key)
            .header("authorization", format!("Bearer {api_key}")),
        AuthScheme::AnthropicXApiKey => request
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", api_key),
        AuthScheme::CsswitchOauth => request,
    };
    let resp = request.send().map_err(|e| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: e.to_string(),
    })?;
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    if !(200..300).contains(&status) {
        let body = bounded_redacted_error_body(resp, api_key, started, timeouts.total);
        let detail = if body.is_empty() {
            format!("upstream {status}")
        } else {
            format!("upstream {status}: {body}")
        };
        return Err(UpstreamError {
            status: if (400..=599).contains(&status) {
                status
            } else {
                502
            },
            upstream_status: Some(status),
            detail,
        });
    }
    let body = read_body_with_deadline(resp, None, started, timeouts.total).map_err(|error| {
        let detail = match error {
            BodyReadFailure::Deadline => {
                "upstream models response exceeded the total timeout".into()
            }
            BodyReadFailure::Io(error) => {
                format!("upstream response body read failed: {error}")
            }
        };
        UpstreamError {
            status: 502,
            upstream_status: None,
            detail,
        }
    })?;
    Ok(UpstreamBody {
        status,
        content_type,
        body,
    })
}

fn retry_delay(attempt: usize) {
    std::thread::sleep(Duration::from_millis(800 * attempt as u64));
}

pub fn get(cfg: &GatewayConfig, url: &str) -> Result<UpstreamBody, UpstreamError> {
    let mut last_error = None;
    for attempt in 1..=3 {
        match get_once(cfg, url) {
            Ok(resp) => return Ok(resp),
            Err(e) if e.upstream_status.is_some() => return Err(e),
            Err(e) => {
                last_error = Some(e);
                if attempt < 3 {
                    retry_delay(attempt);
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: "upstream models request failed".to_string(),
    }))
}

fn sensitive_error_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "authorization",
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "credential",
        "cookie",
        "secret",
        "path_secret",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn redact_json(value: &mut Value, api_key: &str) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if sensitive_error_key(key) {
                    *value = Value::String("[REDACTED]".into());
                } else {
                    redact_json(value, api_key);
                }
            }
        }
        Value::Array(array) => {
            for value in array {
                redact_json(value, api_key);
            }
        }
        Value::String(text) => {
            let exact_redacted = if api_key.is_empty() {
                std::mem::take(text)
            } else {
                text.replace(api_key, "[REDACTED]")
            };
            *text = redact_ascii_token(redact_ascii_token(exact_redacted, "bearer "), "sk-");
        }
        _ => {}
    }
}

fn redact_ascii_token(mut text: String, marker: &str) -> String {
    let mut offset = 0;
    loop {
        let lower = text.to_ascii_lowercase();
        let Some(relative_start) = lower[offset..].find(marker) else {
            return text;
        };
        let start = offset + relative_start;
        let token_start = start + marker.len();
        let token_len = text[token_start..]
            .bytes()
            .take_while(|byte| {
                !byte.is_ascii_whitespace()
                    && !matches!(*byte, b'"' | b'\'' | b',' | b'}' | b']' | b';')
            })
            .count();
        if token_len == 0 {
            offset = token_start;
            continue;
        }
        text.replace_range(token_start..token_start + token_len, "[REDACTED]");
        offset = token_start + "[REDACTED]".len();
    }
}

fn redact_error_body(mut bytes: Vec<u8>, api_key: &str) -> String {
    let truncated = bytes.len() as u64 > MAX_ERROR_BODY_BYTES;
    bytes.truncate(MAX_ERROR_BODY_BYTES as usize);
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let mut safe = match serde_json::from_str::<Value>(&raw) {
        Ok(mut value) => {
            redact_json(&mut value, api_key);
            serde_json::to_string(&value).unwrap_or_else(|_| "upstream error".into())
        }
        Err(_) => {
            let text = if api_key.is_empty() {
                raw
            } else {
                raw.replace(api_key, "[REDACTED]")
            };
            redact_ascii_token(redact_ascii_token(text, "bearer "), "sk-")
        }
    };
    if truncated {
        safe.push_str("…[truncated]");
    }
    safe
}

fn bounded_redacted_error_body(
    resp: Response,
    api_key: &str,
    started: Instant,
    total: Duration,
) -> String {
    match read_body_with_deadline(resp, Some(MAX_ERROR_BODY_BYTES + 1), started, total) {
        Ok(bytes) => redact_error_body(bytes, api_key),
        Err(BodyReadFailure::Deadline) => "upstream error body exceeded the total timeout".into(),
        Err(BodyReadFailure::Io(_)) => "upstream error body could not be read".into(),
    }
}

fn map_http_error(
    resp: Response,
    api_key: &str,
    started: Instant,
    total: Duration,
) -> UpstreamError {
    let status = resp.status().as_u16();
    let body = bounded_redacted_error_body(resp, api_key, started, total);
    let mapped = if (400..=599).contains(&status) {
        status
    } else {
        502
    };
    let detail = if body.is_empty() {
        format!("upstream {status}")
    } else {
        format!("upstream {status}: {body}")
    };
    UpstreamError {
        status: mapped,
        upstream_status: Some(status),
        detail,
    }
}

fn post_nonstream_with_timeouts(
    cfg: &GatewayConfig,
    body: Vec<u8>,
    transport: Option<&AnthropicTransport>,
    timeouts: InferenceTimeouts,
) -> Result<UpstreamBody, UpstreamError> {
    let started = Instant::now();
    let resp = post_with_timeouts(cfg, body, timeouts, true, transport)?;
    if !resp.status().is_success() {
        return Err(map_http_error(
            resp,
            cfg.api_key.as_deref().unwrap_or_default(),
            started,
            timeouts.total,
        ));
    }
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let response_body =
        read_body_with_deadline(resp, None, started, timeouts.total).map_err(|error| {
            let detail = match error {
                BodyReadFailure::Deadline => "upstream response exceeded the total timeout".into(),
                BodyReadFailure::Io(error) => {
                    format!("upstream response body read failed: {error}")
                }
            };
            UpstreamError {
                status: 502,
                upstream_status: Some(status),
                detail,
            }
        })?;
    Ok(UpstreamBody {
        status,
        content_type,
        body: response_body,
    })
}

pub fn post_nonstream(
    cfg: &GatewayConfig,
    body: Vec<u8>,
    transport: Option<&AnthropicTransport>,
) -> Result<UpstreamBody, UpstreamError> {
    post_nonstream_with_timeouts(cfg, body, transport, inference_timeouts(cfg))
}

/// One adapter turn can issue multiple finite upstream requests. They share
/// the contract's cumulative deadline instead of each resetting the full
/// timeout, while HTTP/auth/header construction stays owned by this module.
pub(crate) fn inference_deadline(cfg: &GatewayConfig) -> Instant {
    Instant::now()
        .checked_add(inference_timeouts(cfg).total)
        .expect("validated inference timeout must fit Instant")
}

pub(crate) fn post_nonstream_before(
    cfg: &GatewayConfig,
    body: Vec<u8>,
    transport: Option<&AnthropicTransport>,
    deadline: Instant,
) -> Result<UpstreamBody, UpstreamError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| UpstreamError {
            status: 504,
            upstream_status: None,
            detail: "upstream request exceeded the cumulative total timeout".into(),
        })?;
    let mut timeouts = inference_timeouts(cfg);
    timeouts.total = remaining;
    timeouts.connect = timeouts.connect.min(remaining);
    post_nonstream_with_timeouts(cfg, body, transport, timeouts)
}

#[cfg(test)]
fn read_first_line(resp: &mut Response) -> Result<Vec<u8>, UpstreamError> {
    let mut first = Vec::new();
    let mut byte = [0_u8; 1];
    while first.len() < 65_536 {
        match resp.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                first.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(e) => {
                return Err(UpstreamError {
                    status: 502,
                    upstream_status: None,
                    detail: e.to_string(),
                });
            }
        }
    }
    if first.is_empty() {
        return Err(UpstreamError {
            status: 502,
            upstream_status: Some(200),
            detail: "upstream 200 but empty body".to_string(),
        });
    }
    Ok(first)
}

pub fn open_stream(
    cfg: &GatewayConfig,
    body: Vec<u8>,
    transport: Option<&AnthropicTransport>,
) -> Result<UpstreamStream, UpstreamError> {
    let timeouts = inference_timeouts(cfg);
    let started = Instant::now();
    let resp = post_with_timeouts(cfg, body, timeouts, false, transport)?;
    if !resp.status().is_success() {
        return Err(map_http_error(
            resp,
            cfg.api_key.as_deref().unwrap_or_default(),
            started,
            timeouts.total,
        ));
    }
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
    {
        return Err(UpstreamError {
            status: 502,
            upstream_status: Some(status),
            detail: "upstream 200 returned a non-SSE Content-Type".into(),
        });
    }
    Ok(UpstreamStream { response: resp })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        messages_post_url, models_timeout_secs, models_timeouts, post_nonstream_before,
        post_with_timeouts, read_body_with_deadline, read_first_line, redact_error_body,
        upstream_failure_metadata, AnthropicTransport, BodyReadFailure, InferenceTimeouts,
        ModelsTimeouts, UpstreamError, INFERENCE_TIMEOUTS,
    };
    use crate::config::GatewayConfig;
    use crate::provider_contracts::AuthScheme;

    fn bind_loopback() -> TcpListener {
        loop {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock upstream");
            if listener.local_addr().expect("mock address").port() != 8765 {
                return listener;
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set mock read timeout");
        let mut request = Vec::new();
        let mut buf = [0_u8; 1024];
        let mut expected_len = None;
        loop {
            let read = stream.read(&mut buf).expect("read mock request");
            assert!(read > 0, "gateway closed before request body completed");
            request.extend_from_slice(&buf[..read]);
            if expected_len.is_none() {
                if let Some(head_end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..head_end]);
                    let body_len = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().expect("content length"))
                        })
                        .unwrap_or(0);
                    expected_len = Some(head_end + 4 + body_len);
                }
            }
            if expected_len.is_some_and(|len| request.len() >= len) {
                return request;
            }
        }
    }

    fn spawn_stream(chunks: Vec<(Duration, &'static [u8])>) -> (String, thread::JoinHandle<()>) {
        let listener = bind_loopback();
        let address = listener.local_addr().expect("mock address");
        let total_len = chunks.iter().map(|(_, chunk)| chunk.len()).sum::<usize>();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept gateway request");
            let _ = read_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {total_len}\r\n\r\n"
            )
            .expect("write mock response head");
            stream.flush().expect("flush mock response head");
            for (delay, chunk) in chunks {
                thread::sleep(delay);
                if stream.write_all(chunk).is_err() {
                    return;
                }
                if stream.flush().is_err() {
                    return;
                }
            }
        });
        (format!("http://{address}/v1/messages"), handle)
    }

    fn spawn_counted_response(
        response: Vec<u8>,
    ) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = bind_loopback();
        let address = listener.local_addr().expect("mock address");
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_thread = Arc::clone(&count);
        let handle = thread::spawn(move || {
            let serve = |mut stream: TcpStream| {
                count_for_thread.fetch_add(1, Ordering::SeqCst);
                let _ = read_request(&mut stream);
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            };
            let (stream, _) = listener.accept().expect("accept first request");
            serve(stream);
            listener.set_nonblocking(true).expect("set nonblocking");
            let deadline = Instant::now() + Duration::from_millis(1_500);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        (format!("http://{address}/v1/messages"), count, handle)
    }

    fn spawn_captured_response() -> (String, mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
        let listener = bind_loopback();
        let address = listener.local_addr().expect("mock address");
        let (request_tx, request_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept gateway request");
            request_tx
                .send(read_request(&mut stream))
                .expect("capture gateway request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .expect("write mock response");
            stream.flush().expect("flush mock response");
        });
        (format!("http://{address}/v1/messages"), request_rx, handle)
    }

    fn spawn_redirect_response() -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = bind_loopback();
        let address = listener.local_addr().expect("mock address");
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_thread = Arc::clone(&count);
        let handle = thread::spawn(move || {
            let serve = |mut stream: TcpStream, redirect: bool| {
                count_for_thread.fetch_add(1, Ordering::SeqCst);
                let _ = read_request(&mut stream);
                let response = if redirect {
                    format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{address}/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .into()
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            };
            let (stream, _) = listener.accept().expect("accept redirect request");
            serve(stream, true);
            listener.set_nonblocking(true).expect("set nonblocking");
            let deadline = Instant::now() + Duration::from_millis(1_500);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, false),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        (format!("http://{address}/v1/messages"), count, handle)
    }

    struct ServerRelease(Option<mpsc::Sender<()>>);

    impl ServerRelease {
        fn release(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    impl Drop for ServerRelease {
        fn drop(&mut self) {
            self.release();
        }
    }

    fn spawn_blocked_stream(
        first: Option<&'static [u8]>,
    ) -> (
        String,
        ServerRelease,
        mpsc::Receiver<()>,
        thread::JoinHandle<()>,
    ) {
        let listener = bind_loopback();
        let address = listener.local_addr().expect("mock address");
        let (release_tx, release_rx) = mpsc::channel();
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept gateway request");
            let _ = read_request(&mut stream);
            let declared_len = first.map_or(1, |chunk| chunk.len() + 1);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared_len}\r\n\r\n"
            )
            .expect("write blocked response head");
            if let Some(chunk) = first {
                stream.write_all(chunk).expect("write first stream line");
            }
            stream.flush().expect("flush blocked response");
            blocked_tx.send(()).expect("signal blocked response");
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
        });
        (
            format!("http://{address}/v1/messages"),
            ServerRelease(Some(release_tx)),
            blocked_rx,
            handle,
        )
    }

    fn test_config(upstream_url: String) -> GatewayConfig {
        GatewayConfig {
            provider: "deepseek".to_string(),
            port: 0,
            auth_secret: None,
            api_key: Some("fake-key".to_string()),
            upstream_url,
            models_url: None,
            relay_thinking: None,
            provider_contract: None,
            intent: crate::config::GatewayIntent::Formal,
            static_model_resolver: None,
            shim_mode: "off".to_string(),
            launch_id: "timeout-test".to_string(),
        }
    }

    #[test]
    fn expired_cumulative_inference_deadline_fails_before_network_io() {
        let cfg = test_config("http://127.0.0.1:9/v1/messages".into());
        let deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        let error = post_nonstream_before(&cfg, b"{}".to_vec(), None, deadline).unwrap_err();
        assert_eq!(error.status, 504);
        assert_eq!(error.upstream_status, None);
        assert_eq!(
            error.detail,
            "upstream request exceeded the cumulative total timeout"
        );
    }

    #[test]
    fn inference_and_models_timeout_contracts_are_separate() {
        assert_eq!(INFERENCE_TIMEOUTS.connect, Duration::from_secs(300));
        assert_eq!(INFERENCE_TIMEOUTS.total, Duration::from_secs(300));
        assert_eq!(INFERENCE_TIMEOUTS.read_idle, Duration::from_secs(300));

        // 独立启动的 models 超时对所有 provider 一致;托管启动由契约覆盖(见下)。
        assert_eq!(models_timeout_secs("deepseek"), 120);
        assert_eq!(models_timeout_secs("relay"), 120);

        let mut managed = test_config("http://127.0.0.1:9/v1/messages".into());
        managed.provider_contract =
            Some(crate::provider_contracts::load_runtime_contract("deepseek", None, None).unwrap());
        assert_eq!(
            models_timeouts(&managed),
            ModelsTimeouts {
                connect: Duration::from_secs(10),
                total: Duration::from_secs(30),
                read_idle: Duration::from_secs(300),
            }
        );
    }

    #[test]
    fn anthropic_messages_url_keeps_only_one_safe_beta_query() {
        let original = "http://127.0.0.1:9/v1/messages?opaque=%2Fvalue&beta=false&beta=true";
        let transport = AnthropicTransport::from_inbound(
            "/local-secret/v1/messages?beta=true&nonce=local-nonce",
            &HashMap::new(),
        )
        .unwrap();
        let mut kimi = test_config(original.into());
        let mut contract =
            crate::provider_contracts::load_runtime_contract("deepseek", None, None).unwrap();
        contract.contract_id = "kimi-anthropic-relay".into();
        kimi.provider_contract = Some(contract);
        assert_eq!(
            messages_post_url(&kimi, Some(&transport)).unwrap(),
            "http://127.0.0.1:9/v1/messages?opaque=%2Fvalue&beta=true"
        );

        let custom = test_config("http://127.0.0.1:9/v1/messages?beta=false".into());
        let no_beta = AnthropicTransport::from_inbound(
            "/local-secret/v1/messages?beta=false&nonce=local-nonce",
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(
            messages_post_url(&custom, Some(&no_beta)).unwrap(),
            "http://127.0.0.1:9/v1/messages"
        );
    }

    #[test]
    fn generic_anthropic_loopback_forwards_allowlist_and_rebuilds_sensitive_transport() {
        let (url, request_rx, upstream) = spawn_captured_response();
        let mut headers = HashMap::from([
            ("anthropic-version".into(), "2024-01-01".into()),
            ("anthropic-beta".into(), "incoming-a,incoming-b".into()),
            ("x-app".into(), "science".into()),
            ("user-agent".into(), "Claude-Science/1.2".into()),
            ("authorization".into(), "Bearer client-auth-secret".into()),
            ("x-api-key".into(), "client-api-secret".into()),
            ("cookie".into(), "client-cookie-secret".into()),
            ("host".into(), "local-host-secret".into()),
            ("content-length".into(), "999".into()),
            ("connection".into(), "x-hop-secret".into()),
            ("x-hop-secret".into(), "client-hop-secret".into()),
            ("keep-alive".into(), "local-keepalive-secret".into()),
            (
                "proxy-authorization".into(),
                "local-proxy-auth-secret".into(),
            ),
            ("transfer-encoding".into(), "chunked".into()),
            ("x-local-nonce".into(), "local-header-nonce".into()),
            ("x-path-secret".into(), "local-header-path-secret".into()),
        ]);
        let transport = AnthropicTransport::from_inbound(
            "/local-path-secret/v1/messages?beta=true&beta=false&nonce=local-query-nonce",
            &headers,
        )
        .unwrap();
        headers.clear();
        let mut cfg = test_config(format!("{url}?trusted=%2Fvalue&beta=false"));
        cfg.provider = "deepseek".into();
        super::post_nonstream(&cfg, b"{}".to_vec(), Some(&transport)).unwrap();

        let request = String::from_utf8(request_rx.recv().unwrap())
            .unwrap()
            .to_ascii_lowercase();
        assert!(request.starts_with("post /v1/messages?trusted=%2fvalue&beta=true http/1.1\r\n"));
        assert_eq!(request.matches("beta=true").count(), 1);
        assert!(request.contains("anthropic-version: 2024-01-01\r\n"));
        assert!(request.contains("anthropic-beta: incoming-a,incoming-b\r\n"));
        assert!(request.contains("x-app: science\r\n"));
        assert!(request.contains("user-agent: claude-science/1.2\r\n"));
        assert!(request.contains("x-api-key: fake-key\r\n"));
        assert!(request.contains("content-length: 2\r\n"));
        for forbidden in [
            "client-auth-secret",
            "client-api-secret",
            "client-cookie-secret",
            "local-host-secret",
            "client-hop-secret",
            "local-keepalive-secret",
            "local-proxy-auth-secret",
            "local-header-nonce",
            "local-header-path-secret",
            "local-path-secret",
            "local-query-nonce",
        ] {
            assert!(!request.contains(forbidden), "leaked {forbidden}");
        }
        upstream.join().unwrap();
    }

    #[test]
    fn connection_nominated_anthropic_header_is_not_forwardable() {
        let transport = AnthropicTransport::from_inbound(
            "/v1/messages",
            &HashMap::from([
                ("connection".into(), "keep-alive, anthropic-beta".into()),
                ("anthropic-beta".into(), "must-not-forward".into()),
            ]),
        )
        .unwrap();
        assert!(transport.anthropic_beta.is_none());
    }

    #[test]
    fn kimi_anthropic_loopback_forwards_science_identity_without_injecting_anything() {
        let (url, request_rx, upstream) = spawn_captured_response();
        let transport = AnthropicTransport::from_inbound(
            "/v1/messages",
            &HashMap::from([
                ("anthropic-beta".into(), "incoming-a,incoming-b".into()),
                ("x-app".into(), "science".into()),
                ("user-agent".into(), "Claude-Science/1.2".into()),
            ]),
        )
        .unwrap();
        let mut cfg = test_config(url);
        cfg.provider = "relay".into();
        let mut contract =
            crate::provider_contracts::load_runtime_contract("deepseek", None, None).unwrap();
        contract.contract_id = "kimi-anthropic-relay".into();
        contract.auth_scheme = AuthScheme::Bearer;
        cfg.provider_contract = Some(contract);
        super::post_nonstream(&cfg, b"{}".to_vec(), Some(&transport)).unwrap();

        let request = String::from_utf8(request_rx.recv().unwrap())
            .unwrap()
            .to_ascii_lowercase();
        // The gateway no longer forces a beta query, a Claude Code beta token or
        // a claude-cli identity onto this upstream: Science's own headers pass
        // through untouched.
        assert!(request.starts_with("post /v1/messages http/1.1\r\n"));
        assert!(!request.contains("beta=true"));
        assert!(request.contains("anthropic-beta: incoming-a,incoming-b\r\n"));
        assert!(!request.contains("claude-code-20250219"));
        assert!(request.contains("user-agent: claude-science/1.2\r\n"));
        assert!(!request.contains("claude-cli/"));
        assert!(request.contains("x-app: science\r\n"));
        assert!(request.contains("authorization: bearer fake-key\r\n"));
        assert!(!request.contains("x-api-key:"));
        upstream.join().unwrap();
    }

    #[test]
    fn nonstream_body_failure_and_empty_stream_handshake_post_exactly_once() {
        let incomplete_json = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{}".to_vec();
        let (url, count, upstream) = spawn_counted_response(incomplete_json);
        let error = super::post_nonstream(&test_config(url), b"{}".to_vec(), None).unwrap_err();
        assert_eq!(error.status, 502);
        upstream.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);

        let empty_sse = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (url, count, upstream) = spawn_counted_response(empty_sse);
        let opened = super::open_stream(&test_config(url), b"{}".to_vec(), None).unwrap();
        drop(opened);
        upstream.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn inference_redirect_is_not_followed_or_reposted() {
        let (url, count, upstream) = spawn_redirect_response();
        let error = super::post_nonstream(&test_config(url), b"{}".to_vec(), None).unwrap_err();
        assert_eq!(error.status, 502);
        assert_eq!(error.upstream_status, Some(307));
        upstream.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn finite_body_read_obeys_the_remaining_cumulative_deadline() {
        let (url, mut release, blocked, upstream) = spawn_blocked_stream(None);
        let timeouts = InferenceTimeouts {
            connect: Duration::from_secs(1),
            total: Duration::from_millis(120),
            read_idle: Duration::from_millis(500),
        };
        let started = Instant::now();
        let response = post_with_timeouts(&test_config(url), b"{}".to_vec(), timeouts, true, None)
            .expect("response headers");
        blocked
            .recv_timeout(Duration::from_secs(1))
            .expect("upstream blocked after headers");
        thread::sleep(Duration::from_millis(70));
        let read_started = Instant::now();
        let error = read_body_with_deadline(response, None, started, timeouts.total).unwrap_err();
        assert_eq!(error, BodyReadFailure::Deadline);
        assert!(read_started.elapsed() < Duration::from_millis(100));
        release.release();
        upstream.join().unwrap();
    }

    #[test]
    fn json_error_strings_redact_generic_tokens_without_truncation() {
        let safe = redact_error_body(
            br#"{"error":{"message":"Bearer other-bearer and sk-other-key and configured-key","detail":"ok"}}"#
                .to_vec(),
            "configured-key",
        );
        assert!(!safe.contains("other-bearer"));
        assert!(!safe.contains("other-key"));
        assert!(!safe.contains("configured-key"));
        assert!(safe.contains("Bearer [REDACTED]"));
        assert!(safe.contains("sk-[REDACTED]"));
    }

    #[test]
    fn stream_requires_sse_content_type_before_success() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 3\r\nConnection: close\r\n\r\n{}\n".to_vec();
        let (url, count, upstream) = spawn_counted_response(response);
        let error = super::open_stream(&test_config(url), b"{}".to_vec(), None).unwrap_err();
        assert_eq!(error.status, 502);
        assert_eq!(error.upstream_status, Some(200));
        assert!(error.detail.contains("non-SSE"));
        upstream.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn upstream_http_status_is_preserved_and_error_body_is_bounded_and_redacted() {
        let key = "fake-key";
        let body = format!(
            "{{\"error\":{{\"message\":\"Bearer other-secret and sk-other-secret and Bearer {key}\",\"api_key\":\"{key}\",\"padding\":\"{}\"}}}}",
            "x".repeat(20_000)
        );
        let response = format!(
            "HTTP/1.1 405 Method Not Allowed\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let (url, count, upstream) = spawn_counted_response(response);
        let error = super::post_nonstream(&test_config(url), b"{}".to_vec(), None).unwrap_err();
        assert_eq!(error.status, 405);
        assert_eq!(error.upstream_status, Some(405));
        assert!(!error.detail.contains(key));
        assert!(!error.detail.contains("other-secret"));
        assert!(error.detail.len() < 17_000);
        assert!(error.detail.contains("truncated"));
        let metadata = upstream_failure_metadata("post_nonstream", &error);
        assert!(!metadata.contains(key));
        assert!(!metadata.contains("other-secret"));
        assert!(!metadata.contains("padding"));
        assert!(!metadata.contains("detail"));
        assert!(metadata.contains("upstream_status=Some(405)"));
        upstream.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn upstream_failure_metadata_omits_transport_detail_with_url() {
        let metadata = upstream_failure_metadata(
            "open_stream",
            &UpstreamError {
                status: 502,
                upstream_status: None,
                detail: "request failed for https://private.invalid/path?token=must-not-leak"
                    .into(),
            },
        );
        assert!(!metadata.contains("private.invalid"));
        assert!(!metadata.contains("must-not-leak"));
        assert!(!metadata.contains("detail"));
        assert!(metadata.contains("upstream_status=None"));
    }

    #[test]
    fn active_stream_can_outlive_read_idle_timeout() {
        let read_idle = Duration::from_millis(500);
        let (url, upstream) = spawn_stream(vec![
            (Duration::ZERO, b"event: message_start\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
        ]);
        let cfg = test_config(url);
        let mut response = post_with_timeouts(
            &cfg,
            b"{}".to_vec(),
            InferenceTimeouts {
                connect: Duration::from_secs(1),
                total: Duration::from_secs(1),
                read_idle,
            },
            false,
            None,
        )
        .expect("open active stream");
        let first = read_first_line(&mut response).expect("read first stream line");
        let started = Instant::now();
        let mut remaining = Vec::new();
        response
            .read_to_end(&mut remaining)
            .expect("active stream must not hit a total deadline");
        let elapsed = started.elapsed();

        assert_eq!(first, b"event: message_start\n");
        assert_eq!(
            remaining,
            b"data: tick\ndata: tick\ndata: tick\ndata: tick\ndata: tick\ndata: tick\ndata: tick\ndata: tick\n"
        );
        assert!(
            elapsed > read_idle,
            "stream should run longer than one idle window: {elapsed:?}"
        );
        upstream.join().expect("join active mock upstream");
    }

    #[test]
    fn stalled_stream_exceeding_read_idle_timeout_fails() {
        let read_idle = Duration::from_millis(250);
        let (url, mut release, blocked, upstream) =
            spawn_blocked_stream(Some(b"event: message_start\n"));
        let cfg = test_config(url);
        let mut response = post_with_timeouts(
            &cfg,
            b"{}".to_vec(),
            InferenceTimeouts {
                connect: Duration::from_secs(1),
                total: Duration::from_secs(1),
                read_idle,
            },
            false,
            None,
        )
        .expect("open stalled stream");
        blocked
            .recv_timeout(Duration::from_secs(1))
            .expect("mock upstream must enter the controlled stall");
        assert_eq!(
            read_first_line(&mut response).expect("read first stream line"),
            b"event: message_start\n"
        );

        let started = Instant::now();
        let error = response
            .read_to_end(&mut Vec::new())
            .expect_err("stalled stream must hit the read-idle timeout");
        let elapsed = started.elapsed();
        assert!(
            !error.to_string().is_empty(),
            "stalled-stream error detail must not be empty"
        );
        assert!(elapsed >= read_idle, "timeout fired too early: {elapsed:?}");
        release.release();
        upstream.join().expect("join stalled mock upstream");
    }

    #[test]
    fn first_byte_idle_timeout_keeps_upstream_error_contract() {
        let read_idle = Duration::from_millis(250);
        let (url, mut release, blocked, upstream) = spawn_blocked_stream(None);
        let cfg = test_config(url);
        let mut response = post_with_timeouts(
            &cfg,
            b"{}".to_vec(),
            InferenceTimeouts {
                connect: Duration::from_secs(1),
                total: Duration::from_secs(1),
                read_idle,
            },
            false,
            None,
        )
        .expect("receive mock response headers");
        blocked
            .recv_timeout(Duration::from_secs(1))
            .expect("mock upstream must enter the controlled first-byte stall");

        let started = Instant::now();
        let error = read_first_line(&mut response)
            .expect_err("missing first byte must hit the read-idle timeout");
        let elapsed = started.elapsed();
        assert_eq!(error.status, 502);
        assert_eq!(error.upstream_status, None);
        assert!(!error.detail.is_empty());
        assert!(elapsed >= read_idle, "timeout fired too early: {elapsed:?}");
        release.release();
        upstream.join().expect("join first-byte mock upstream");
    }
}
