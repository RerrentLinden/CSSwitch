//! 单进程服务:推理网关 + 本地控制 API + 内嵌 WebUI。
//!
//! 一个端口对外提供三类路径:
//! - `/v1/messages`、`/v1/models` —— 推理,按激活模式路由(官方直通 / 渠道中继);
//! - `/control/*` —— 本地控制面(状态、配置、切换、日志);
//! - `/`、`/ui/*` —— 内嵌 WebUI。
//!
//! 其余路径显式 404,不做静默兜底:Science 若在新版本里调用了没见过的端点,
//! 应当立刻可见,而不是被一个假的成功响应掩盖。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::profile::{Channel, Mode, Profile};

const INDEX_HTML: &str = include_str!("../ui/index.html");
const MAX_LOG_ENTRIES: usize = 300;
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// 运行时状态:配置可在运行中被 WebUI 改写,推理线程每次请求读取快照。
pub struct AppState {
    pub profile: RwLock<Profile>,
    pub log: Mutex<Vec<Value>>,
    pub port: u16,
}

impl AppState {
    fn new(profile: Profile, port: u16) -> Self {
        Self {
            profile: RwLock::new(profile),
            log: Mutex::new(Vec::new()),
            port,
        }
    }

    pub fn record(&self, entry: Value) {
        if let Ok(mut log) = self.log.lock() {
            log.push(entry);
            let len = log.len();
            if len > MAX_LOG_ENTRIES {
                log.drain(..len - MAX_LOG_ENTRIES);
            }
        }
    }

    fn snapshot(&self) -> Profile {
        self.profile
            .read()
            .map(|profile| profile.clone())
            .unwrap_or_default()
    }
}

pub fn serve(port_override: Option<u16>) -> Result<(), String> {
    let mut profile = Profile::load();
    if let Some(port) = port_override {
        profile.port = port;
    }
    let port = profile.port;
    let state = Arc::new(AppState::new(profile, port));
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("无法监听 127.0.0.1:{port}:{e}"))?;
    // 启动横幅同样落进 service.log,与其余日志行统一走带时间戳的格式。
    crate::log_line!("CSSwitch 控制台: http://127.0.0.1:{port}/");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || handle(stream, state));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }
}

fn handle(mut stream: TcpStream, state: Arc<AppState>) {
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            respond_text(&mut stream, 400, "text/plain; charset=utf-8", &error);
            return;
        }
    };
    let path = request.path().to_string();
    if path == "/" || path == "/ui" || path == "/ui/" {
        respond_text(&mut stream, 200, "text/html; charset=utf-8", INDEX_HTML);
        return;
    }
    if let Some(rest) = path.strip_prefix("/control/") {
        handle_control(&mut stream, &request, rest, &state);
        return;
    }
    if path == "/v1/messages" || path == "/v1/models" {
        crate::server::handle_inference(
            &mut stream,
            &request.method,
            &request.target,
            request.headers.clone(),
            request.body.clone(),
            &state,
        );
        return;
    }
    respond_json(
        &mut stream,
        404,
        json!({"type": "error", "error": {"type": "not_found_error", "message": path}}),
    );
}

fn handle_control(stream: &mut TcpStream, request: &Request, action: &str, state: &Arc<AppState>) {
    let result: Result<Value, String> = match (request.method.as_str(), action) {
        ("GET", "status") => Ok(status_payload(state)),
        ("GET", "logs") => Ok(json!({
            "entries": state.log.lock().map(|log| log.clone()).unwrap_or_default()
        })),
        ("POST", "config") => save_config(request, state),
        ("POST", "switch") => switch_mode(request, state),
        ("POST", "probe-models") => probe_models(request),
        ("POST", "science/start") => start_science(state),
        ("POST", "science/stop") => crate::science::stop().map(|_| json!({"ok": true})),
        ("POST", "quit") => {
            // 先把响应写回去再退,否则控制台只会看到连接被切断。
            respond_json(stream, 200, json!({"ok": true, "message": "服务正在退出"}));
            let _ = stream.flush();
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(150));
                std::process::exit(0);
            });
            return;
        }
        ("GET", "science/url") => crate::science::login_url().map(|url| json!({"url": url})),
        _ => Err(format!("未知控制操作:{} {action}", request.method)),
    };
    match result {
        Ok(value) => respond_json(stream, 200, value),
        Err(error) => respond_json(stream, 400, json!({"error": error})),
    }
}

fn status_payload(state: &Arc<AppState>) -> Value {
    let profile = state.snapshot();
    let science = crate::science::status();
    json!({
        "mode": profile.mode.as_str(),
        "mode_label": profile.mode.display_name(),
        "port": state.port,
        "base_url": format!("http://127.0.0.1:{}", state.port),
        "science": science,
        "channels": {
            "kimi": channel_payload(&profile.kimi),
            "deepseek": channel_payload(&profile.deepseek),
        }
    })
}

/// 渠道配置对外投影:key 只报告"是否已配置",绝不回显。
fn channel_payload(channel: &Channel) -> Value {
    json!({
        "base_url": channel.base_url,
        "has_api_key": !channel.api_key.trim().is_empty(),
        "default_model": channel.default_model,
        "quality_model": channel.quality_model,
        "fast_model": channel.fast_model,
        "fable_model": channel.fable_model,
        "ready": channel.validate().is_ok(),
    })
}

fn save_config(request: &Request, state: &Arc<AppState>) -> Result<Value, String> {
    let payload: Value =
        serde_json::from_slice(&request.body).map_err(|e| format!("请求体不是合法 JSON:{e}"))?;
    let mode_name = payload
        .get("channel")
        .and_then(Value::as_str)
        .ok_or("缺少 channel")?;
    let mode = Mode::parse(mode_name).ok_or("未知 channel")?;
    if mode == Mode::Official {
        return Err("官方模式无需配置".into());
    }

    let mut profile = state.snapshot();
    {
        let channel = profile.channel_mut(&mode).ok_or("未知 channel")?;
        if let Some(base_url) = payload.get("base_url").and_then(Value::as_str) {
            channel.base_url = base_url.trim().to_string();
        }
        // API key 留空表示"保持不变",而不是"清空"——避免每次保存都要重填。
        if let Some(api_key) = payload.get("api_key").and_then(Value::as_str) {
            if !api_key.trim().is_empty() {
                channel.api_key = api_key.trim().to_string();
            }
        }
        for (field, slot) in [
            ("default_model", 0),
            ("quality_model", 1),
            ("fast_model", 2),
            ("fable_model", 3),
        ] {
            let Some(value) = payload.get(field) else {
                continue;
            };
            let parsed = serde_json::from_value(value.clone())
                .map_err(|e| format!("{field} 格式非法:{e}"))?;
            match slot {
                0 => channel.default_model = parsed,
                1 => channel.quality_model = parsed,
                2 => channel.fast_model = parsed,
                _ => channel.fable_model = parsed,
            }
        }
    }
    let channel = profile.channel(&mode).ok_or("未知 channel")?.clone();
    profile.save()?;
    if let Ok(mut guard) = state.profile.write() {
        *guard = profile;
    }
    Ok(json!({
        "ok": true,
        "channel": channel_payload(&channel),
        "note": "已保存。运行中的链路不变,下次切换或重启 Science 时生效。"
    }))
}

fn switch_mode(request: &Request, state: &Arc<AppState>) -> Result<Value, String> {
    let payload: Value =
        serde_json::from_slice(&request.body).map_err(|e| format!("请求体不是合法 JSON:{e}"))?;
    let mode_name = payload
        .get("mode")
        .and_then(Value::as_str)
        .ok_or("缺少 mode")?;
    let mode = Mode::parse(mode_name).ok_or("未知 mode")?;

    let mut profile = state.snapshot();
    // 切到渠道模式前先校验配置完整,避免 Science 重启后才发现连不上。
    if let Some(channel) = profile.channel(&mode) {
        channel.validate()?;
    }
    profile.mode = mode.clone();
    profile.save()?;
    if let Ok(mut guard) = state.profile.write() {
        *guard = profile;
    }

    // Science 只在启动时读 base_url,所以切换必须重启 daemon。
    // 重启会打断进行中的会话(Science 侧标记为 "interrupted by a restart"),
    // 因此有活跃会话时必须显式确认,不能默默切。
    let science = crate::science::status();
    let running = science
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let active = science
        .pointer("/detail/daemon/active_conversations")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let forced = payload
        .get("force")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if running && active > 0 && !forced {
        return Err(format!(
            "有 {active} 个会话正在进行,切换会重启 Science 并打断它们。确认后重试。"
        ));
    }
    let restarted = if running {
        crate::science::stop()?;
        std::thread::sleep(Duration::from_millis(600));
        crate::science::start(&format!("http://127.0.0.1:{}", state.port))?;
        true
    } else {
        false
    };
    let url = crate::science::login_url().ok();
    Ok(json!({
        "ok": true,
        "mode": mode.as_str(),
        "mode_label": mode.display_name(),
        "science_restarted": restarted,
        "url": url,
    }))
}

fn start_science(state: &Arc<AppState>) -> Result<Value, String> {
    crate::science::start(&format!("http://127.0.0.1:{}", state.port))?;
    let url = crate::science::login_url().ok();
    Ok(json!({"ok": true, "url": url}))
}

/// 向渠道拉取可用模型清单(WebUI 的"获取可用模型")。
fn probe_models(request: &Request) -> Result<Value, String> {
    let payload: Value =
        serde_json::from_slice(&request.body).map_err(|e| format!("请求体不是合法 JSON:{e}"))?;
    let base_url = payload
        .get("base_url")
        .and_then(Value::as_str)
        .ok_or("缺少 base_url")?
        .trim()
        .trim_end_matches('/')
        .to_string();
    let api_key = payload
        .get("api_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let stored_channel = payload.get("channel").and_then(Value::as_str);
    let api_key = match api_key {
        Some(key) => key,
        None => {
            // WebUI 不回显已保存的 key,探测时从磁盘取当前值。
            let mode = stored_channel
                .and_then(Mode::parse)
                .ok_or("缺少 api_key 且未指定已保存的 channel")?;
            let profile = Profile::load();
            profile
                .channel(&mode)
                .map(|channel| channel.api_key.clone())
                .filter(|key| !key.trim().is_empty())
                .ok_or("该渠道尚未保存 API Key")?
        }
    };

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;

    let mut last_error = String::new();
    let mut body = Value::Null;
    for url in model_list_candidates(&base_url) {
        let response = match client
            .get(&url)
            .header("authorization", format!("Bearer {api_key}"))
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
        {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("请求 {url} 失败:{error}");
                continue;
            }
        };
        let status = response.status();
        let text = response.text().unwrap_or_default();
        if !status.is_success() {
            // 保留上游原文,不伪造一个空清单。
            let detail: String = text.chars().take(200).collect();
            last_error = format!("{url} → 上游 {}:{detail}", status.as_u16());
            continue;
        }
        match serde_json::from_str::<Value>(&text) {
            Ok(parsed) => {
                body = parsed;
                break;
            }
            Err(error) => last_error = format!("{url} → 上游返回不是 JSON:{error}"),
        }
    }
    if body.is_null() {
        return Err(last_error);
    }
    let models: Vec<Value> = body
        .get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = item.get("id").and_then(Value::as_str)?;
                    Some(json!({
                        "id": id,
                        "display_name": item.get("display_name").and_then(Value::as_str).unwrap_or(id)
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({"models": models}))
}

/// 模型清单的候选地址。Anthropic 兼容端点未必在自己的路径下提供 models
/// (DeepSeek 的 `/anthropic/v1/models` 就是 404),需要回退到服务根域。
fn model_list_candidates(base_url: &str) -> Vec<String> {
    let base = base_url.trim_end_matches('/');
    let mut candidates = Vec::new();
    if base.ends_with("/v1") {
        candidates.push(format!("{base}/models"));
    } else {
        candidates.push(format!("{base}/v1/models"));
    }
    if let Ok(parsed) = reqwest::Url::parse(base) {
        if !parsed.path().trim_matches('/').is_empty() {
            if let Some(host) = parsed.host_str() {
                let port = parsed
                    .port()
                    .map(|port| format!(":{port}"))
                    .unwrap_or_default();
                let root = format!("{}://{host}{port}/v1/models", parsed.scheme());
                if !candidates.contains(&root) {
                    candidates.push(root);
                }
            }
        }
    }
    candidates
}

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let mut buffer = Vec::with_capacity(4096);
    let mut byte = [0_u8; 1];
    while !buffer.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut byte).map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("请求头被截断".into());
        }
        buffer.push(byte[0]);
        if buffer.len() > 64 * 1024 {
            return Err("请求头超过上限".into());
        }
    }
    let text = std::str::from_utf8(&buffer).map_err(|_| "请求头不是合法 UTF-8")?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or("缺少请求行")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("缺少方法")?.to_string();
    let target = parts.next().ok_or("缺少请求目标")?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map(|(_, value)| value.parse().unwrap_or(0))
        .unwrap_or(0);
    if length > MAX_BODY_BYTES {
        return Err("请求体超过上限".into());
    }
    let mut body = vec![0_u8; length];
    if length > 0 {
        stream.read_exact(&mut body).map_err(|e| e.to_string())?;
    }
    Ok(Request {
        method,
        target,
        headers,
        body,
    })
}

fn respond_text(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let bytes = body.as_bytes();
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reason(status),
        bytes.len()
    )
    .and_then(|_| stream.write_all(bytes))
    .and_then(|_| stream.flush());
}

fn respond_json(stream: &mut TcpStream, status: u16, value: Value) {
    respond_text(
        stream,
        status,
        "application/json",
        &serde_json::to_string(&value).unwrap_or_else(|_| "{}".into()),
    );
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Error",
    }
}

/// 供推理侧记录一条脱敏日志。
pub fn log_entry(action: &str, detail: Value) -> Value {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    json!({"ts_ms": ts, "action": action, "detail": detail})
}

/// 推理请求的计时器,便于日志记录耗时。
pub struct Timer(Instant);

impl Timer {
    pub fn start() -> Self {
        Self(Instant::now())
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.0.elapsed().as_millis() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_payload_never_exposes_the_api_key() {
        let mut channel = Channel::from_defaults_for_test();
        channel.api_key = "sk-super-secret-value".into();
        let payload = channel_payload(&channel);
        let text = payload.to_string();
        assert!(!text.contains("sk-super-secret-value"), "key 不得回显");
        assert_eq!(payload["has_api_key"], json!(true));
    }

    #[test]
    fn model_list_falls_back_to_the_service_root() {
        // 回归:DeepSeek 的 /anthropic/v1/models 是 404,清单在根域的 /v1/models。
        assert_eq!(
            model_list_candidates("https://api.deepseek.com/anthropic"),
            vec![
                "https://api.deepseek.com/anthropic/v1/models".to_string(),
                "https://api.deepseek.com/v1/models".to_string(),
            ]
        );
        assert_eq!(
            model_list_candidates("https://api.kimi.com"),
            vec!["https://api.kimi.com/v1/models".to_string()]
        );
    }

    #[test]
    fn log_ring_buffer_drops_the_oldest_entries() {
        let state = AppState::new(Profile::default(), 1);
        for index in 0..(MAX_LOG_ENTRIES + 25) {
            state.record(json!({"n": index}));
        }
        let log = state.log.lock().unwrap();
        assert_eq!(log.len(), MAX_LOG_ENTRIES);
        assert_eq!(log[0]["n"], json!(25), "最旧的条目应当被丢弃");
    }
}
