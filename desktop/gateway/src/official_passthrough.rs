//! 官方实例直通模式(任务 08-19-official-instance-smoke)。
//!
//! 官方 Claude Science(默认 profile、真实登录)把 `ANTHROPIC_BASE_URL` 指到
//! 本模式后,全部请求被原样转发到官方上游。用途:验证"官方实例 + 本地网关"
//! 形态可行,并实测枚举 Science 经 base_url 调用的端点。
//!
//! 合同(与任务 PRD 一致):
//! - 请求/响应字节零改写;仅剥离 hop-by-hop 头,鉴权头原样透传;
//! - 任何 header 与 body 不落盘、不打印;JSONL 日志只含时间、方法、
//!   路径+query(query 值按敏感键名脱敏)、状态、耗时、SSE 标记、字节数;
//! - 上游失败向客户端显式返回 502 文本并记日志,不静默。

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";

/// 请求侧剥离:hop-by-hop 语义头,以及由转发客户端按实际 body 重建的头。
const STRIP_REQUEST_HEADERS: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "expect",
];

/// 响应侧剥离:连接语义由本端重建(保留 content-length,整体以
/// `connection: close` + 读到 EOF 定界,SSE 不缓冲)。
const STRIP_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "transfer-encoding",
    "trailer",
    "upgrade",
];

const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

struct Options {
    port: u16,
    log_path: String,
    upstream: String,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut port: Option<u16> = None;
    let mut log_path: Option<String> = None;
    let mut upstream = DEFAULT_UPSTREAM.to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                let raw = args.get(i).ok_or("--port 缺少值")?;
                port = Some(raw.parse().map_err(|_| format!("非法端口:{raw}"))?);
            }
            "--log" => {
                i += 1;
                log_path = Some(args.get(i).ok_or("--log 缺少值")?.clone());
            }
            "--upstream" => {
                i += 1;
                upstream = args.get(i).ok_or("--upstream 缺少值")?.clone();
            }
            other => return Err(format!("未知参数:{other}")),
        }
        i += 1;
    }
    Ok(Options {
        port: port.ok_or("必须提供 --port")?,
        log_path: log_path.ok_or("必须提供 --log(脱敏端点日志文件)")?,
        upstream: upstream.trim_end_matches('/').to_string(),
    })
}

pub fn run_cli(args: &[String]) -> Result<(), String> {
    let opts = parse_args(args)?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // SSE 与长响应:总超时必须关闭,由连接生命周期定界。
        .timeout(None)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let log = Arc::new(EndpointLog::open(&opts.log_path)?);
    let upstream = Arc::new(opts.upstream.clone());
    let listener = TcpListener::bind(("127.0.0.1", opts.port)).map_err(|e| e.to_string())?;
    eprintln!(
        "official-passthrough listening on 127.0.0.1:{} -> {} (log: {})",
        opts.port, opts.upstream, opts.log_path
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let client = client.clone();
                let log = Arc::clone(&log);
                let upstream = Arc::clone(&upstream);
                thread::spawn(move || handle_connection(stream, &client, &log, &upstream));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

fn handle_connection(
    mut stream: TcpStream,
    client: &reqwest::blocking::Client,
    log: &EndpointLog,
    upstream: &str,
) {
    let started = Instant::now();
    let head = match read_request_head(&mut stream) {
        Ok(head) => head,
        Err(e) => {
            write_plain(
                &mut stream,
                400,
                "Bad Request",
                &format!("passthrough: {e}"),
            );
            return;
        }
    };
    // 客户端声明 Expect: 100-continue 时必须先应答,否则对端会等待超时。
    if header_value(&head.headers, "expect")
        .map(|v| v.eq_ignore_ascii_case("100-continue"))
        .unwrap_or(false)
    {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        let _ = stream.flush();
    }
    let body = match read_request_body(&mut stream, &head.headers) {
        Ok(body) => body,
        Err(e) => {
            write_plain(
                &mut stream,
                400,
                "Bad Request",
                &format!("passthrough: {e}"),
            );
            log.write(
                &head.method,
                &head.target,
                400,
                started.elapsed(),
                false,
                0,
                0,
                Some("bad_request_body"),
            );
            return;
        }
    };
    if !head.target.starts_with('/') {
        write_plain(
            &mut stream,
            400,
            "Bad Request",
            "passthrough: 仅支持 origin-form 请求目标",
        );
        return;
    }
    let method = match reqwest::Method::from_bytes(head.method.as_bytes()) {
        Ok(method) => method,
        Err(_) => {
            write_plain(
                &mut stream,
                400,
                "Bad Request",
                "passthrough: 非法 HTTP 方法",
            );
            return;
        }
    };

    let req_bytes = body.len();
    let url = format!("{upstream}{}", head.target);
    let mut request = client.request(method, &url);
    for (name, value) in &head.headers {
        if should_strip_request_header(name) {
            continue;
        }
        request = request.header(name, value);
    }
    match request.body(body).send() {
        Err(e) => {
            write_plain(
                &mut stream,
                502,
                "Bad Gateway",
                &format!("passthrough upstream error: {e}"),
            );
            log.write(
                &head.method,
                &head.target,
                502,
                started.elapsed(),
                false,
                req_bytes,
                0,
                Some("upstream_error"),
            );
        }
        Ok(mut resp) => {
            let status = resp.status();
            let sse = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.contains("text/event-stream"))
                .unwrap_or(false);
            let mut head_out: Vec<u8> = Vec::with_capacity(1024);
            head_out.extend_from_slice(
                format!(
                    "HTTP/1.1 {} {}\r\n",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("")
                )
                .as_bytes(),
            );
            for (name, value) in resp.headers() {
                if should_strip_response_header(name.as_str()) {
                    continue;
                }
                head_out.extend_from_slice(name.as_str().as_bytes());
                head_out.extend_from_slice(b": ");
                head_out.extend_from_slice(value.as_bytes());
                head_out.extend_from_slice(b"\r\n");
            }
            head_out.extend_from_slice(b"connection: close\r\n\r\n");
            if stream.write_all(&head_out).is_err() {
                return;
            }
            let _ = stream.flush();

            let mut resp_bytes: u64 = 0;
            let mut buf = [0_u8; 16 * 1024];
            loop {
                match resp.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        resp_bytes += n as u64;
                        if stream.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        // SSE 逐帧到达依赖及时冲刷,不能等缓冲区满。
                        let _ = stream.flush();
                    }
                    // 上游中断:连接随之关闭,以 close 定界向客户端表达截断。
                    Err(_) => break,
                }
            }
            log.write(
                &head.method,
                &head.target,
                status.as_u16(),
                started.elapsed(),
                sse,
                req_bytes,
                resp_bytes,
                None,
            );
        }
    }
}

struct RequestHead {
    method: String,
    target: String,
    /// 保序、允许重名;名字统一小写。
    headers: Vec<(String, String)>,
}

fn read_request_head<R: Read>(stream: &mut R) -> Result<RequestHead, String> {
    let mut buf = Vec::with_capacity(4096);
    let mut byte = [0_u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("请求头为空或被截断".to_string());
        }
        buf.push(byte[0]);
        if buf.len() > MAX_HEAD_BYTES {
            return Err("请求头超过上限".to_string());
        }
    }
    let text = std::str::from_utf8(&buf).map_err(|_| "请求头不是合法 UTF-8".to_string())?;
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
        // 错误信息只报长度,不回显内容(头里可能有凭证)。
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("非法头行({} 字节)", line.len()))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok(RequestHead {
        method,
        target,
        headers,
    })
}

fn read_request_body<R: Read>(
    stream: &mut R,
    headers: &[(String, String)],
) -> Result<Vec<u8>, String> {
    if header_value(headers, "transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        return read_chunked_body(stream);
    }
    let len = match header_value(headers, "content-length") {
        None => return Ok(Vec::new()),
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| "非法 Content-Length".to_string())?,
    };
    if len > MAX_BODY_BYTES {
        return Err(format!("请求体超过上限:{len} 字节"));
    }
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok(body)
}

fn read_chunked_body<R: Read>(stream: &mut R) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line = read_crlf_line(stream)?;
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size =
            usize::from_str_radix(size_str, 16).map_err(|_| "非法 chunk 长度".to_string())?;
        if size == 0 {
            // trailer 区读到空行为止。
            loop {
                let trailer = read_crlf_line(stream)?;
                if trailer.is_empty() {
                    break;
                }
            }
            return Ok(body);
        }
        if body.len() + size > MAX_BODY_BYTES {
            return Err("chunked 请求体超过上限".to_string());
        }
        let mut chunk = vec![0_u8; size];
        stream.read_exact(&mut chunk).map_err(|e| e.to_string())?;
        body.extend_from_slice(&chunk);
        let sep = read_crlf_line(stream)?;
        if !sep.is_empty() {
            return Err("chunk 终止符缺失".to_string());
        }
    }
}

fn read_crlf_line<R: Read>(stream: &mut R) -> Result<String, String> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let n = stream.read(&mut byte).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("chunked 编码被截断".to_string());
        }
        if byte[0] == b'\n' {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return String::from_utf8(line).map_err(|_| "chunk 元数据不是合法 UTF-8".to_string());
        }
        line.push(byte[0]);
        if line.len() > 8192 {
            return Err("chunk 元数据行过长".to_string());
        }
    }
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

fn should_strip_request_header(name: &str) -> bool {
    STRIP_REQUEST_HEADERS.contains(&name)
}

fn should_strip_response_header(name: &str) -> bool {
    STRIP_RESPONSE_HEADERS.contains(&name)
}

fn write_plain(stream: &mut TcpStream, status: u16, reason: &str, message: &str) {
    let body = message.as_bytes();
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .and_then(|_| stream.write_all(body))
    .and_then(|_| stream.flush());
}

struct EndpointLog {
    file: Mutex<std::fs::File>,
}

impl EndpointLog {
    fn open(path: &str) -> Result<Self, String> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("无法打开端点日志 {path}:{e}"))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        method: &str,
        target: &str,
        status: u16,
        elapsed: Duration,
        sse: bool,
        req_bytes: usize,
        resp_bytes: u64,
        note: Option<&str>,
    ) {
        let line = build_log_line(
            method, target, status, elapsed, sse, req_bytes, resp_bytes, note,
        );
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
        eprintln!("{line}");
    }
}

#[allow(clippy::too_many_arguments)]
fn build_log_line(
    method: &str,
    target: &str,
    status: u16,
    elapsed: Duration,
    sse: bool,
    req_bytes: usize,
    resp_bytes: u64,
    note: Option<&str>,
) -> String {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    serde_json::json!({
        "ts_ms": ts_ms,
        "method": method,
        "path": redact_target(target),
        "status": status,
        "ms": elapsed.as_millis() as u64,
        "sse": sse,
        "req_bytes": req_bytes,
        "resp_bytes": resp_bytes,
        "note": note,
    })
    .to_string()
}

/// query 值按敏感键名脱敏;Anthropic 端点不在 query 放凭证,此处为防御性约束。
fn redact_target(target: &str) -> String {
    const SENSITIVE_KEY_PARTS: &[&str] = &["key", "token", "secret", "auth", "signature", "sig"];
    match target.split_once('?') {
        None => target.to_string(),
        Some((path, query)) => {
            let redacted: Vec<String> = query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    None => pair.to_string(),
                    Some((k, v)) => {
                        let kl = k.to_ascii_lowercase();
                        if SENSITIVE_KEY_PARTS.iter().any(|s| kl.contains(s)) {
                            format!("{k}=<redacted>")
                        } else {
                            format!("{k}={v}")
                        }
                    }
                })
                .collect();
            format!("{path}?{}", redacted.join("&"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::time::Duration;

    #[test]
    fn head_parser_preserves_order_and_duplicates() {
        let raw = b"POST /v1/messages?beta=true HTTP/1.1\r\nHost: x\r\nX-A: 1\r\nAuthorization: Bearer t\r\nX-A: 2\r\n\r\n";
        let head = read_request_head(&mut Cursor::new(&raw[..])).unwrap();
        assert_eq!(head.method, "POST");
        assert_eq!(head.target, "/v1/messages?beta=true");
        let xa: Vec<&str> = head
            .headers
            .iter()
            .filter(|(n, _)| n == "x-a")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(xa, vec!["1", "2"]);
    }

    #[test]
    fn request_strip_keeps_auth_but_drops_hop_by_hop() {
        assert!(!should_strip_request_header("authorization"));
        assert!(!should_strip_request_header("x-api-key"));
        assert!(!should_strip_request_header("anthropic-beta"));
        assert!(should_strip_request_header("host"));
        assert!(should_strip_request_header("connection"));
        assert!(should_strip_request_header("content-length"));
        assert!(should_strip_request_header("transfer-encoding"));
    }

    #[test]
    fn response_strip_keeps_content_headers() {
        assert!(!should_strip_response_header("content-length"));
        assert!(!should_strip_response_header("content-encoding"));
        assert!(!should_strip_response_header("content-type"));
        assert!(should_strip_response_header("transfer-encoding"));
        assert!(should_strip_response_header("connection"));
    }

    #[test]
    fn chunked_body_decodes_with_trailer() {
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\nx-trailer: v\r\n\r\n";
        let body = read_chunked_body(&mut Cursor::new(&raw[..])).unwrap();
        assert_eq!(body, b"Wikipedia");
    }

    #[test]
    fn content_length_body_reads_exact() {
        let headers = vec![("content-length".to_string(), "5".to_string())];
        let body = read_request_body(&mut Cursor::new(&b"hello"[..]), &headers).unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn log_line_redacts_sensitive_query_and_carries_no_headers() {
        let line = build_log_line(
            "GET",
            "/v1/models?key=abc123&beta=true",
            401,
            Duration::from_millis(42),
            false,
            0,
            17,
            None,
        );
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["path"], "/v1/models?key=<redacted>&beta=true");
        assert_eq!(parsed["status"], 401);
        assert!(!line.contains("abc123"));
        assert!(!line.to_ascii_lowercase().contains("authorization"));
    }

    #[test]
    fn parse_args_requires_port_and_log() {
        assert!(parse_args(&[]).is_err());
        let opts = parse_args(&[
            "--port".into(),
            "8791".into(),
            "--log".into(),
            "/tmp/x.jsonl".into(),
        ])
        .unwrap();
        assert_eq!(opts.port, 8791);
        assert_eq!(opts.upstream, DEFAULT_UPSTREAM);
    }
}
