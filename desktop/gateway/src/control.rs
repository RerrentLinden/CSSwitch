//! 单进程服务:推理网关 + 本地控制 API + 内嵌 WebUI。
//!
//! 一个端口对外提供三类路径:
//! - `/v1/messages`、`/v1/models` —— 正式实例的推理,按正式实例的模式路由
//!   (官方直通 / 渠道中继);
//!   `/sandbox/v1/messages`、`/sandbox/v1/models` —— 免登录沙箱的推理,按沙箱自己选的
//!   渠道路由(只有渠道中继)。两个实例各有各的模式,靠这段路径前缀区分;
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

use crate::profile::{Channel, Instance, Mode, Profile};
use crate::science_config::LaunchOverrides;

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

    /// 普通请求条目,受「记录请求日志」开关控制。错误条目走 [`Self::record`],始终保留。
    pub fn record_request(&self, entry: Value) {
        if crate::request_log_enabled() {
            self.record(entry);
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
    crate::set_request_log_enabled(profile.request_log);
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
    if let Some((instance, target)) = route_inference(&request.target) {
        crate::server::handle_inference(
            &mut stream,
            &request.method,
            target,
            request.headers.clone(),
            request.body.clone(),
            instance,
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

/// 推理路由:请求目标 →(来自哪个实例, 交给推理层的 target)。不是推理路径时返回 `None`,
/// 由调用方显式 404。
///
/// 正式实例的 base_url 是不带路径的网关地址,请求落在 `/v1/*`;免登录沙箱的 base_url 多带
/// `sandbox::INFERENCE_PREFIX`,请求落在 `/sandbox/v1/*`。前缀只在这一处剥离(query 保留),
/// 推理层看到的 target 与实例无关。路径必须整条相等,不做前缀匹配:`/sandbox`、
/// `/sandbox/v1/别的`、`/sandboxx/v1/messages` 都不是推理路径。
fn route_inference(target: &str) -> Option<(Instance, &str)> {
    fn is_inference(target: &str) -> bool {
        let path = target.split('?').next().unwrap_or(target);
        path == "/v1/messages" || path == "/v1/models"
    }
    if is_inference(target) {
        return Some((Instance::Official, target));
    }
    let rest = target.strip_prefix(crate::sandbox::INFERENCE_PREFIX)?;
    is_inference(rest).then_some((Instance::Sandbox, rest))
}

fn handle_control(stream: &mut TcpStream, request: &Request, action: &str, state: &Arc<AppState>) {
    let result: Result<Value, String> = match (request.method.as_str(), action) {
        ("GET", "status") => Ok(status_payload(state)),
        ("GET", "logs") => Ok(json!({
            "entries": state.log.lock().map(|log| log.clone()).unwrap_or_default()
        })),
        ("POST", "config") => save_config(request, state),
        ("POST", "logging") => save_logging(request, state),
        ("POST", "web-search") => set_web_search(request, state),
        ("POST", "switch") => switch_mode(request, state),
        ("POST", "probe-models") => probe_models(request),
        ("POST", "science/start") => start_science(state),
        ("POST", "science/stop") => crate::science::stop().map(|_| json!({"ok": true})),
        ("GET", "sandbox/status") => Ok(crate::sandbox::status()),
        ("POST", "sandbox/start") => start_sandbox(state),
        ("POST", "sandbox/switch") => switch_sandbox_mode(request, state),
        ("POST", "sandbox/stop") => crate::sandbox::stop().map(|_| json!({"ok": true})),
        ("GET", "sandbox/url") => crate::sandbox::entry_url().map(|url| json!({"url": url})),
        ("POST", "quit") => {
            // 网关一退,经它启动的实例只剩断掉的推理链路,所以先停实例再退。
            // 停止失败不拦截退出(退出是用户的明确意图),失败原文随响应交给控制台。
            let (stopped, errors) = stop_instances();
            if !stopped.is_empty() {
                crate::log_line!("退出服务:已停止 {}", stopped.join("、"));
            } else if errors.is_empty() {
                crate::log_line!("退出服务:没有需要停止的实例");
            }
            for error in &errors {
                crate::log_line!("退出服务:{error}");
            }
            // 先把响应写回去再退,否则控制台只会看到连接被切断。
            respond_json(
                stream,
                200,
                json!({
                    "ok": true,
                    "message": "服务正在退出",
                    "stopped": stopped,
                    "errors": errors,
                }),
            );
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
        // 正在运行的这个服务进程的版本。安装版的启动器拿它与自己比对:
        // 换了新版应用而后台还是旧服务时,要提示而不是让人以为已经升级。
        "version": crate::VERSION,
        // 正式实例的模式。
        "mode": profile.mode.as_str(),
        "mode_label": profile.mode.display_name(),
        "port": state.port,
        "base_url": format!("http://127.0.0.1:{}", state.port),
        "science": science,
        "sandbox": crate::sandbox::status(),
        // 免登录沙箱的渠道。放在顶层、且不叫 `mode`:序列化保留写入顺序,取顶层模式的
        // 脚本用的是贪婪匹配,任何更靠后的 `"mode":"…"` 都会被它当成正式实例的模式。
        "sandbox_mode": profile.sandbox_mode.as_str(),
        "sandbox_mode_label": profile.sandbox_mode.display_name(),
        "request_log": profile.request_log,
        "web_search": profile.web_search,
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
    // 联网搜索已是全局开关,不再随渠道保存。旧客户端带着这个字段来时显式报错:
    // 静默忽略会让调用方以为自己把搜索关掉了。
    if payload.get("web_search").is_some() {
        return Err(
            "web_search 不再是渠道配置:联网搜索已改为全局开关,请改用 POST /control/web-search"
                .into(),
        );
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
        // 保存不重启 Science(会打断会话)。地址、模型槽与默认模型都要等它下次启动才完全生效。
        "note": "已保存,重启 Science 后生效。",
    }))
}

/// 这个模式的模型目录。官方模式没有渠道,返回 `None`;渠道的默认槽为空(配置不完整,
/// 目录生成不出来)时同样是 `None`。
fn channel_resolver(
    profile: &Profile,
    mode: &Mode,
) -> Option<crate::static_profile::StaticProfileResolver> {
    let (adapter, _) = mode.contract()?;
    let catalog = profile.channel(mode)?.static_catalog(adapter);
    crate::static_profile::StaticProfileResolver::from_json(&catalog.to_string()).ok()
}

/// 这个实例本次启动要带的覆盖项:联网搜索总开关的当前值,以及新会话默认用哪个模型。
///
/// 默认模型只在这一处决定,所有启动点(切换模式、启动 Science、切换联网搜索总开关、
/// 启动沙箱、切换沙箱渠道)都从这里取:
/// - 正式实例 + 官方模式 → 不指定,沿用 Science 自己的默认;
/// - 走渠道的实例 → 该渠道清单里承载默认槽的那一条(Science 出厂的 `claude-opus-5` 不在清单里,
///   不指定的话它会显示成不可用)。这一条恒为 `claude-sonnet-19`,见
///   `static_profile::MENU_POSITIONS`。
///
/// 渠道的默认槽为空时目录生成不出来,取不到 id,这里不因此拦截:启动前的渠道完整性检查
/// 另有其处。
fn launch_overrides(profile: &Profile, instance: Instance) -> LaunchOverrides {
    LaunchOverrides {
        web_search: profile.web_search,
        default_model: channel_resolver(profile, profile.mode_for(instance))
            .map(|resolver| resolver.default_menu_id().to_string()),
    }
}

/// 「记录请求日志」开关。先存盘再改运行时:存盘失败时进程行为保持不变,
/// 界面收到错误后回滚勾选状态,两边不会出现不一致。
fn save_logging(request: &Request, state: &Arc<AppState>) -> Result<Value, String> {
    let payload: Value =
        serde_json::from_slice(&request.body).map_err(|e| format!("请求体不是合法 JSON:{e}"))?;
    let enabled = parse_request_log(&payload)?;
    let mut profile = state.snapshot();
    profile.request_log = enabled;
    profile.save()?;
    crate::set_request_log_enabled(enabled);
    if let Ok(mut guard) = state.profile.write() {
        *guard = profile;
    }
    Ok(json!({"ok": true, "request_log": enabled}))
}

fn parse_request_log(payload: &Value) -> Result<bool, String> {
    match payload.get("request_log") {
        Some(Value::Bool(enabled)) => Ok(*enabled),
        Some(_) => Err("request_log 必须是布尔值".into()),
        None => Err("缺少 request_log".into()),
    }
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
    let overrides = launch_overrides(&profile, Instance::Official);
    profile.save()?;
    if let Ok(mut guard) = state.profile.write() {
        *guard = profile;
    }

    // Science 只在启动时读 base_url,所以切换必须重启 daemon。
    // 重启会打断进行中的会话(Science 侧标记为 "interrupted by a restart"),
    // 因此有活跃会话时必须显式确认,不能默默切。
    let (running, active) = science_activity(&crate::science::status());
    let forced = payload
        .get("force")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if running && active > 0 && !forced {
        return Err(active_sessions_refusal(active));
    }
    let restarted = if running {
        crate::science::stop()?;
        std::thread::sleep(Duration::from_millis(600));
        crate::science::start(&format!("http://127.0.0.1:{}", state.port), &overrides)?;
        true
    } else {
        // 没在运行就走不到 start。新模式不需要镜像配置(官方模式 + 联网搜索开启)时,
        // 顺手删掉上次留下的那一份,否则它会一直留到下次启动;需要镜像时什么都不做,
        // 下次启动会重新生成。
        if !overrides.needs_mirror() {
            let _ = crate::science_config::official_launch_config(&overrides);
        }
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

/// 官方实例是否在运行、有几个会话正在进行。输入是 [`crate::science::status`] 的结果:
/// `status` 子命令的原始输出在 `detail` 下,会话数在 `daemon.active_conversations`。
fn science_activity(science: &Value) -> (bool, u64) {
    let running = science
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let active = science
        .pointer("/detail/daemon/active_conversations")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (running, active)
}

/// 有会话正在进行时拒绝重启 Science 的文案。控制台按「会话正在进行」识别它并转成确认框,
/// 切换模式与切换联网搜索共用同一句。
fn active_sessions_refusal(active: u64) -> String {
    format!("有 {active} 个会话正在进行,切换会重启 Science 并打断它们。确认后重试。")
}

fn start_science(state: &Arc<AppState>) -> Result<Value, String> {
    let overrides = launch_overrides(&state.snapshot(), Instance::Official);
    crate::science::start(&format!("http://127.0.0.1:{}", state.port), &overrides)?;
    let url = crate::science::login_url().ok();
    Ok(json!({"ok": true, "url": url}))
}

/// 切换联网搜索总开关时要做什么。
#[derive(Debug, Eq, PartialEq)]
enum WebSearchSwitch {
    /// 目标值与当前值相同:什么都不做。
    Unchanged,
    /// 官方实例里有会话正在进行而调用方没有确认:拒绝,任何状态不变。
    NeedsConfirmation { active: u64 },
    /// 官方实例没在运行:只存盘,下次启动时带上新设置。
    SaveOnly,
    /// 官方实例在运行:存盘后重启它,新设置才到 Science 一侧。
    SaveAndRestart,
}

/// 纯判定,不碰任何状态。Science 只在启动时读配置,所以运行中的官方实例必须重启;
/// 重启会打断进行中的会话,与切换模式同一套规矩:有会话且未确认就拒绝。
fn plan_web_search_switch(
    current: bool,
    target: bool,
    running: bool,
    active: u64,
    force: bool,
) -> WebSearchSwitch {
    if current == target {
        WebSearchSwitch::Unchanged
    } else if !running {
        WebSearchSwitch::SaveOnly
    } else if active > 0 && !force {
        WebSearchSwitch::NeedsConfirmation { active }
    } else {
        WebSearchSwitch::SaveAndRestart
    }
}

/// 请求体 `{"enabled": bool, "force"?: bool}`。两个字段都不做类型降级:
/// 字符串 "false" 在 JS 里是真值,静默接受会把意图反过来。
fn parse_web_search_request(payload: &Value) -> Result<(bool, bool), String> {
    let enabled = match payload.get("enabled") {
        Some(Value::Bool(enabled)) => *enabled,
        Some(_) => return Err("enabled 必须是布尔值".into()),
        None => return Err("缺少 enabled".into()),
    };
    Ok((enabled, parse_force(payload)?))
}

/// 请求体里可选的 `force`:「我确认打断正在进行的事」。缺省为否;出现了就必须是布尔值,
/// 不从别的类型猜。联网搜索总开关与沙箱渠道切换共用。
fn parse_force(payload: &Value) -> Result<bool, String> {
    match payload.get("force") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(force)) => Ok(*force),
        Some(_) => Err("force 必须是布尔值".into()),
    }
}

/// 联网搜索总开关。一个值驱动两层:
/// - 网关一侧每请求读快照,存盘并更新快照后下一次请求即生效;
/// - Science 一侧只在启动时读配置,官方实例在运行就重启它。免登录沙箱不自动重启
///   (它没有可供确认的会话数),只在响应里标出需要重启。
///
/// 拒绝与镜像预检都在存盘之前:被拒绝、或镜像生成不出来(例如用户的 `config.toml`
/// 无法解析)时,开关不会停在「已保存但没生效」的状态。重启失败不回滚:那时的真实状态
/// 就是「开关已改、Science 没按新设置跑起来」,原文报给用户;官方模式下把开关拨回开启
/// 再启动,就是不带 `--config` 的原始启动方式。
fn set_web_search(request: &Request, state: &Arc<AppState>) -> Result<Value, String> {
    let payload: Value =
        serde_json::from_slice(&request.body).map_err(|e| format!("请求体不是合法 JSON:{e}"))?;
    let (enabled, force) = parse_web_search_request(&payload)?;
    let mut profile = state.snapshot();
    let (running, active) = science_activity(&crate::science::status());
    let restart = match plan_web_search_switch(profile.web_search, enabled, running, active, force)
    {
        WebSearchSwitch::Unchanged => {
            return Ok(json!({
                "ok": true,
                "web_search": enabled,
                "changed": false,
                "science_restarted": false,
                "url": Value::Null,
                "sandbox_restart_required": false,
            }))
        }
        WebSearchSwitch::NeedsConfirmation { active } => {
            return Err(active_sessions_refusal(active))
        }
        WebSearchSwitch::SaveOnly => false,
        WebSearchSwitch::SaveAndRestart => true,
    };
    // 预检:按新值算出这次启动的覆盖项。要带镜像配置(关闭联网搜索,或正式实例在第三方
    // 模式)而现在生成不出来,就不要存;不需要镜像时顺手删掉残留的那一份 —— 官方实例没在
    // 运行时不会走到 start,残留会一直留到下次启动。
    profile.web_search = enabled;
    let overrides = launch_overrides(&profile, Instance::Official);
    crate::science_config::official_launch_config(&overrides)?;

    profile.save()?;
    if let Ok(mut guard) = state.profile.write() {
        *guard = profile;
    }

    if restart {
        let saved = if enabled { "开启" } else { "关闭" };
        crate::science::stop().map_err(|error| {
            format!("联网搜索已保存为{saved},网关一侧已生效;但停止 Science 失败,它没有按新设置重启:{error}")
        })?;
        std::thread::sleep(Duration::from_millis(600));
        crate::science::start(&format!("http://127.0.0.1:{}", state.port), &overrides).map_err(
            |error| {
                format!("联网搜索已保存为{saved},网关一侧已生效;但 Science 已停止,按新设置启动失败:{error}")
            },
        )?;
    }
    let sandbox_running = reports_running(&crate::sandbox::status());
    Ok(json!({
        "ok": true,
        "web_search": enabled,
        "changed": true,
        "science_restarted": restart,
        "url": if restart { crate::science::login_url().ok() } else { None },
        "sandbox_restart_required": sandbox_running,
    }))
}

/// 退出服务前停掉官方实例与免登录沙箱,返回(已停止的实例, 失败原文)。
/// 只停正在运行的:未安装 / 未运行不算失败,沙箱从未初始化时也不去碰它的目录。
/// 两者互不依赖,并行停止以缩短等待。
fn stop_instances() -> (Vec<&'static str>, Vec<String>) {
    let (science, sandbox) = thread::scope(|scope| {
        let sandbox =
            scope.spawn(|| stop_if_running(&crate::sandbox::status(), crate::sandbox::stop));
        let science = stop_if_running(&crate::science::status(), crate::science::stop);
        let sandbox = sandbox
            .join()
            .unwrap_or_else(|_| Some(Err("停止免登录沙箱的线程意外终止".into())));
        (science, sandbox)
    });
    let mut stopped = Vec::new();
    let mut errors = Vec::new();
    for (label, outcome) in [("Science", science), ("免登录沙箱", sandbox)] {
        match outcome {
            Some(Ok(())) => stopped.push(label),
            Some(Err(error)) => errors.push(error),
            None => {}
        }
    }
    (stopped, errors)
}

/// `None` 表示实例本就没在运行,无需停止。
fn stop_if_running(status: &Value, stop: fn() -> Result<(), String>) -> Option<Result<(), String>> {
    reports_running(status).then(stop)
}

/// 实例状态(`science::status` / `sandbox::status`)是否报告在运行。
fn reports_running(status: &Value) -> bool {
    status
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 沙箱为什么不能用官方 Claude。切换入口拿它拒绝请求,控制台原文展示。
fn sandbox_official_refusal() -> String {
    "免登录沙箱不能使用官方 Claude:沙箱用的是 CSSwitch 生成的虚拟登录,没有真实的 Claude 凭证,\
     官方接口会拒绝它。请选择 Kimi 或 DeepSeek。"
        .into()
}

/// 免登录沙箱能不能用这个渠道:必须是第三方渠道,且配置完整;报错点名渠道与缺项。
/// 启动沙箱(看沙箱当前选的渠道)与切换沙箱渠道(看目标渠道)共用。
fn sandbox_channel_ready(profile: &Profile, mode: &Mode) -> Result<(), String> {
    let channel = profile.channel(mode).ok_or_else(sandbox_official_refusal)?;
    channel.validate().map_err(|error| {
        format!(
            "免登录沙箱要用的 {} 渠道配置不完整:{error}。请先在「渠道配置」里补全。",
            mode.display_name()
        )
    })
}

/// 启动沙箱的前置检查:只看沙箱自己选的渠道(`sandbox_mode`)。正式实例处于什么模式与它
/// 无关 —— 沙箱的请求按 `sandbox_mode` 路由,正式实例是官方模式时沙箱照样能用。
/// 启动前就拒绝,不要等沙箱起来后推理才报错。
fn sandbox_start_precheck(profile: &Profile) -> Result<(), String> {
    sandbox_channel_ready(profile, &profile.sandbox_mode)
}

fn start_sandbox(state: &Arc<AppState>) -> Result<Value, String> {
    let profile = state.snapshot();
    sandbox_start_precheck(&profile)?;
    crate::sandbox::start(
        &format!("http://127.0.0.1:{}", state.port),
        &launch_overrides(&profile, Instance::Sandbox),
    )
}

/// 切换免登录沙箱的渠道时要做什么。
#[derive(Debug, Eq, PartialEq)]
enum SandboxSwitch {
    /// 目标渠道就是当前渠道:什么都不做。
    Unchanged,
    /// 沙箱没在运行:只存盘,下次启动沙箱时生效。
    SaveOnly,
    /// 沙箱在运行而调用方没有确认:拒绝,任何状态不变。
    NeedsConfirmation,
    /// 沙箱在运行且已确认:存盘后重启它。
    SaveAndRestart,
}

/// 纯判定,不碰任何状态。沙箱的请求每次都按 `sandbox_mode` 路由,所以运行中的沙箱必须
/// 跟着重启:只存不重启,它会立刻被改道到新渠道,而它手里的模型目录还是旧渠道的。
/// 沙箱没有可靠的会话数可查(官方实例有 `active_conversations`),因此不区分有没有会话:
/// 只要在运行就先要确认。
fn plan_sandbox_switch(current: &Mode, target: &Mode, running: bool, force: bool) -> SandboxSwitch {
    if current == target {
        SandboxSwitch::Unchanged
    } else if !running {
        SandboxSwitch::SaveOnly
    } else if !force {
        SandboxSwitch::NeedsConfirmation
    } else {
        SandboxSwitch::SaveAndRestart
    }
}

/// 沙箱在运行而未确认时拒绝切换渠道的文案。控制台按「沙箱正在运行」识别它并转成确认框。
fn sandbox_running_refusal() -> String {
    "免登录沙箱正在运行,切换渠道会重启沙箱并打断其中进行中的会话。确认后重试。".into()
}

/// 请求体 `{"mode": "kimi" | "deepseek", "force"?: bool}`。`official` 在入口就拒绝:
/// 沙箱永不为官方。
fn parse_sandbox_switch_request(payload: &Value) -> Result<(Mode, bool), String> {
    let mode_name = payload
        .get("mode")
        .and_then(Value::as_str)
        .ok_or("缺少 mode")?;
    let mode = Mode::parse(mode_name).ok_or("未知 mode")?;
    if mode == Mode::Official {
        return Err(sandbox_official_refusal());
    }
    Ok((mode, parse_force(payload)?))
}

/// 切换免登录沙箱的渠道。只动沙箱:不改正式实例的模式,也不重启正式实例。
///
/// 顺序与联网搜索总开关一致:解析、渠道校验、判定都在存盘之前,被拒绝(或用户在确认框里
/// 取消)时任何状态都不变。存盘并更新快照之后,沙箱的请求从下一条起就按新渠道路由,
/// 运行中的沙箱随即重启。重启失败不回滚(回滚再重启可能循环):那时的真实状态就是
/// 「渠道已保存、沙箱没有按新渠道跑起来」,原文报给用户。
fn switch_sandbox_mode(request: &Request, state: &Arc<AppState>) -> Result<Value, String> {
    let payload: Value =
        serde_json::from_slice(&request.body).map_err(|e| format!("请求体不是合法 JSON:{e}"))?;
    let (mode, force) = parse_sandbox_switch_request(&payload)?;
    let mut profile = state.snapshot();
    sandbox_channel_ready(&profile, &mode)?;
    let running = reports_running(&crate::sandbox::status());
    let restart = match plan_sandbox_switch(&profile.sandbox_mode, &mode, running, force) {
        SandboxSwitch::Unchanged => {
            return Ok(json!({
                "ok": true,
                "mode": mode.as_str(),
                "mode_label": mode.display_name(),
                "changed": false,
                "sandbox_restarted": false,
            }))
        }
        SandboxSwitch::NeedsConfirmation => return Err(sandbox_running_refusal()),
        SandboxSwitch::SaveOnly => false,
        SandboxSwitch::SaveAndRestart => true,
    };

    profile.sandbox_mode = mode.clone();
    let overrides = launch_overrides(&profile, Instance::Sandbox);
    profile.save()?;
    if let Ok(mut guard) = state.profile.write() {
        *guard = profile;
    }

    if restart {
        let saved = mode.display_name();
        crate::sandbox::stop().map_err(|error| {
            format!(
                "沙箱渠道已保存为 {saved},沙箱的请求已按它路由;但停止沙箱失败,它没有重启:{error}"
            )
        })?;
        std::thread::sleep(Duration::from_millis(600));
        crate::sandbox::start(&format!("http://127.0.0.1:{}", state.port), &overrides).map_err(
            |error| format!("沙箱渠道已保存为 {saved};但沙箱已停止,按新渠道启动失败:{error}"),
        )?;
    }
    Ok(json!({
        "ok": true,
        "mode": mode.as_str(),
        "mode_label": mode.display_name(),
        "changed": true,
        "sandbox_restarted": restart,
    }))
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

    /// 两个渠道都是出厂默认的配置。Kimi:默认 k3-256k / 高质量 k3 / 快速 kimi-for-coding;
    /// DeepSeek:默认 deepseek-v4-pro / 高质量留空 / 快速 deepseek-v4-flash。
    fn profile_with(mode: Mode, sandbox_mode: Mode) -> Profile {
        Profile {
            mode,
            sandbox_mode,
            ..Profile::default()
        }
    }

    #[test]
    fn launch_overrides_point_each_instance_at_its_own_channels_default_entry() {
        let menu = |profile: &Profile, instance| launch_overrides(profile, instance).default_model;
        // 正式实例是官方模式:不指定默认模型,Science 用它自己的。沙箱走它自己选的渠道。
        // 把沙箱的写成取 `profile.mode`,这里沙箱会跟着正式实例变成 None;把正式实例的写成取
        // `profile.sandbox_mode`,正式实例会跟着沙箱变成有值。
        let profile = profile_with(Mode::Official, Mode::Kimi);
        assert_eq!(menu(&profile, Instance::Official), None);
        assert_eq!(
            menu(&profile, Instance::Sandbox).as_deref(),
            Some("claude-sonnet-19")
        );
        // 默认模型恒在 sonnet 位,与渠道里别的槽怎么填无关(出厂的 Kimi 另配了高质量槽,
        // DeepSeek 没有)。
        let profile = profile_with(Mode::Deepseek, Mode::Kimi);
        for instance in [Instance::Official, Instance::Sandbox] {
            assert_eq!(
                menu(&profile, instance).as_deref(),
                Some("claude-sonnet-19")
            );
        }

        // 联网搜索总开关的值原样带上。
        let mut profile = profile_with(Mode::Kimi, Mode::Deepseek);
        assert!(launch_overrides(&profile, Instance::Official).web_search);
        profile.web_search = false;
        for instance in [Instance::Official, Instance::Sandbox] {
            assert!(!launch_overrides(&profile, instance).web_search);
        }

        // 渠道还没填默认模型:取不到 id,但不因此出错 —— 启动前的完整性检查另有其处。
        // 走另一个渠道的实例不受牵连。
        profile.kimi.default_model = Default::default();
        assert_eq!(menu(&profile, Instance::Official), None);
        assert_eq!(
            menu(&profile, Instance::Sandbox).as_deref(),
            Some("claude-sonnet-19")
        );
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
    fn request_log_payload_must_be_an_explicit_bool() {
        assert_eq!(parse_request_log(&json!({"request_log": false})), Ok(false));
        assert_eq!(parse_request_log(&json!({"request_log": true})), Ok(true));
        // 不做类型降级:字符串 "false" 在 JS 里是真值,静默接受会把意图反过来。
        assert!(parse_request_log(&json!({"request_log": "false"})).is_err());
        assert!(parse_request_log(&json!({})).is_err());
    }

    #[test]
    fn web_search_switch_never_interrupts_sessions_without_consent() {
        use WebSearchSwitch::*;
        // 值没变:不存盘、不重启 —— 哪怕 Science 正在跑、带着 force。
        assert_eq!(plan_web_search_switch(true, true, true, 3, true), Unchanged);
        assert_eq!(
            plan_web_search_switch(false, false, true, 0, false),
            Unchanged
        );
        // 有会话正在进行且未确认:拒绝。开→关、关→开都要重启,两个方向都受保护。
        assert_eq!(
            plan_web_search_switch(true, false, true, 2, false),
            NeedsConfirmation { active: 2 }
        );
        assert_eq!(
            plan_web_search_switch(false, true, true, 1, false),
            NeedsConfirmation { active: 1 }
        );
        // 没在运行:没有会话可打断,只存盘。状态里残留的会话数不构成拒绝理由。
        assert_eq!(
            plan_web_search_switch(true, false, false, 0, false),
            SaveOnly
        );
        assert_eq!(
            plan_web_search_switch(true, false, false, 5, false),
            SaveOnly
        );
        // 在运行:没有会话,或用户已确认 → 存盘并重启。
        assert_eq!(
            plan_web_search_switch(true, false, true, 0, false),
            SaveAndRestart
        );
        assert_eq!(
            plan_web_search_switch(true, false, true, 2, true),
            SaveAndRestart
        );
    }

    #[test]
    fn science_activity_reads_the_real_status_shape() {
        // `claude-science status` 的真实输出形状,包在 science::status() 的 `detail` 下。
        // 读错路径时会话数恒为 0,「有会话先确认」就成了摆设。
        let status = json!({
            "installed": true,
            "running": true,
            "detail": {"running": true, "daemon": {"active_conversations": 2}},
        });
        assert_eq!(science_activity(&status), (true, 2));
        assert_eq!(
            science_activity(&json!({"installed": true, "running": false})),
            (false, 0)
        );
        assert!(
            active_sessions_refusal(2).contains("会话正在进行"),
            "控制台靠这几个字把拒绝转成确认框"
        );
    }

    #[test]
    fn web_search_request_must_carry_explicit_bools() {
        assert_eq!(
            parse_web_search_request(&json!({"enabled": false})),
            Ok((false, false))
        );
        assert_eq!(
            parse_web_search_request(&json!({"enabled": true, "force": true})),
            Ok((true, true))
        );
        // 不做类型降级:字符串 "false" 在 JS 里是真值。
        assert!(parse_web_search_request(&json!({"enabled": "false"})).is_err());
        assert!(parse_web_search_request(&json!({"enabled": 0})).is_err());
        assert!(parse_web_search_request(&json!({})).is_err());
        // force 是「我确认打断会话」,同样不能从别的类型猜。
        assert!(parse_web_search_request(&json!({"enabled": false, "force": "true"})).is_err());
    }

    #[test]
    fn inference_routing_tells_the_two_instances_apart() {
        // 正式实例:target 原样交给推理层。
        assert_eq!(
            route_inference("/v1/messages?beta=true"),
            Some((Instance::Official, "/v1/messages?beta=true"))
        );
        assert_eq!(
            route_inference("/v1/models"),
            Some((Instance::Official, "/v1/models"))
        );
        // 沙箱:前缀剥掉、query 保留。前缀没剥干净,推理层只会回 404。
        assert_eq!(
            route_inference("/sandbox/v1/messages?beta=true"),
            Some((Instance::Sandbox, "/v1/messages?beta=true"))
        );
        assert_eq!(
            route_inference("/sandbox/v1/models"),
            Some((Instance::Sandbox, "/v1/models"))
        );
        // 近似路径一律不是推理路径,留给调用方显式 404。放宽成前缀匹配的话 `/sandboxx`
        // 会被当成沙箱;循环剥前缀的话叠两层也会被接受。
        for target in [
            "/sandbox",
            "/sandbox/",
            "/sandbox/v1/unknown",
            "/sandboxx/v1/messages",
            "/sandbox/sandbox/v1/messages",
            "//sandbox/v1/messages",
        ] {
            assert_eq!(route_inference(target), None, "{target}");
        }
    }

    #[test]
    fn sandbox_switch_never_restarts_a_running_sandbox_without_consent() {
        use SandboxSwitch::*;
        let (kimi, deepseek) = (Mode::Kimi, Mode::Deepseek);
        // 目标就是当前渠道:不存盘、不重启 —— 哪怕沙箱正在跑、带着 force。
        assert_eq!(plan_sandbox_switch(&kimi, &kimi, true, true), Unchanged);
        assert_eq!(
            plan_sandbox_switch(&deepseek, &deepseek, false, false),
            Unchanged
        );
        // 没在运行:只存盘,下次启动沙箱时生效。带着 force 也不会顺手把它启动起来。
        assert_eq!(
            plan_sandbox_switch(&kimi, &deepseek, false, false),
            SaveOnly
        );
        assert_eq!(plan_sandbox_switch(&kimi, &deepseek, false, true), SaveOnly);
        // 在运行且未确认:拒绝。沙箱没有会话数可查,所以只要在运行就要确认;两个方向都受保护。
        assert_eq!(
            plan_sandbox_switch(&kimi, &deepseek, true, false),
            NeedsConfirmation
        );
        assert_eq!(
            plan_sandbox_switch(&deepseek, &kimi, true, false),
            NeedsConfirmation
        );
        // 在运行且已确认:存盘并重启。
        assert_eq!(
            plan_sandbox_switch(&kimi, &deepseek, true, true),
            SaveAndRestart
        );
        assert!(
            sandbox_running_refusal().contains("沙箱正在运行"),
            "控制台靠这几个字把拒绝转成确认框"
        );
    }

    #[test]
    fn sandbox_switch_request_refuses_official_and_guessed_types() {
        assert_eq!(
            parse_sandbox_switch_request(&json!({"mode": "deepseek"})),
            Ok((Mode::Deepseek, false))
        );
        assert_eq!(
            parse_sandbox_switch_request(&json!({"mode": "kimi", "force": true})),
            Ok((Mode::Kimi, true))
        );
        // 沙箱永不为官方:在入口就拒绝并说明原因,带着 force 也一样。
        let official =
            parse_sandbox_switch_request(&json!({"mode": "official", "force": true})).unwrap_err();
        assert!(official.contains("虚拟登录"), "要说明原因:{official}");
        assert!(parse_sandbox_switch_request(&json!({"mode": "claude"})).is_err());
        assert!(parse_sandbox_switch_request(&json!({})).is_err());
        // force 是「我确认重启沙箱」:字符串 "false" 在 JS 里是真值,不从别的类型猜。
        assert!(parse_sandbox_switch_request(&json!({"mode": "kimi", "force": "false"})).is_err());
    }

    #[test]
    fn sandbox_start_looks_only_at_the_sandbox_channel() {
        // 正式实例是官方模式:以前这时沙箱直接被拒(要求先把全局模式切到第三方)。
        let mut profile = Profile {
            mode: Mode::Official,
            sandbox_mode: Mode::Deepseek,
            ..Profile::default()
        };
        profile.deepseek.api_key = "k".into();
        assert_eq!(sandbox_start_precheck(&profile), Ok(()));

        // 反过来:正式实例的渠道配得再完整也不顶用,看的是沙箱自己选的那个渠道。
        profile.mode = Mode::Kimi;
        profile.kimi.api_key = "k".into();
        profile.deepseek.api_key.clear();
        let error = sandbox_start_precheck(&profile).unwrap_err();
        assert!(
            error.contains("DeepSeek") && error.contains("API Key"),
            "要点名渠道与缺项:{error}"
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
