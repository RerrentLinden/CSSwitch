//! 免登录沙箱编排:在 CSSwitch 自有隔离目录(`~/.csswitch/science-sandbox/`)内铸造
//! 虚拟登录并拉起隔离 Science daemon,让第三方渠道在无 Claude 订阅时仍可驱动 Science
//! 工作台。真实实例路径(`science.rs`)与本模块完全独立,两实例可同时运行。
//!
//! 硬约束(违反会伤到用户的真实实例或把密钥泄出隔离边界):
//! - 绝不读写真实 `~/.claude-science`:forge 三道护栏在写任何文件之前拒绝;
//! - 固定端口 8790/8791(邻接网关 8788 端口族,避开真实实例 8765/8767),占用即报
//!   明确错误,永不自动改绑;全部监听回环;
//! - 入口链接的主机名固定 `127.0.0.1`,真实实例留给 `localhost`:浏览器 cookie 只按主机名
//!   划作用域、不看端口,两实例同主机时会互相顶掉登录(见 `pin_cookie_host`);
//! - 推理只经 `ANTHROPIC_BASE_URL` 指向本网关,模型类/凭证类环境变量一律清除
//!   (与真实实例共享 `science::MODEL_ENV_KEYS_TO_CLEAR` 同一份清单);
//! - 沙箱钥匙串必须真正生效并经启动后正向校验:0.1.48 起 daemon 会把 encryption keys
//!   迁入 macOS Keychain(research/science-0148-smoke.md),校验不过即停 daemon 报错,
//!   不带病运行——否则沙箱密钥会落进用户真实 login keychain;forge 修复/铸新后还要先
//!   清空沙箱钥匙串旧条目——0.1.48 以 Keychain 为权威,不一致时把 file 回写成旧值
//!   (research/science-0148-login-gate.md §2.2),不清则修复被回写吃掉形成死循环;
//! - 兜底停止只杀「pid 来自沙箱 operon.lock 且命令行含精确 `--data-dir`」的进程;
//! - 刻意跨出隔离边界的动作只有两个桥接,都不把真实登录钥匙串挂进沙箱搜索表(真实实例的
//!   Science 加密密钥也在里面):
//!   - SSH 桥接:把真实 `~/.ssh` 的 `config`/`known_hosts` 两个非秘密文件 symlink 进沙箱
//!     HOME(私钥不链接,认证走继承的 SSH_AUTH_SOCK);只读、不修改源,目标已存在绝不覆盖;
//!   - gh 桥接:沙箱 PATH 最前面放一个 gh 包装脚本,只让 gh 进程以真实 HOME 运行,从而读到
//!     本机 gh 登录(令牌在真实登录钥匙串里);git 访问 GitHub 时经环境变量改用 gh 取凭据。

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::sandbox_forge::LoginAction;

/// 沙箱 daemon 与预览端口。
const DAEMON_PORT: u16 = 8790;
const PREVIEW_PORT: u16 = 8791;
/// 沙箱入口链接对外使用的主机名。真实实例走 `localhost`,两边分开 cookie 作用域
/// (理由见 `pin_cookie_host`)。监听地址仍是 `127.0.0.1`,这里只换链接里的写法。
const COOKIE_HOST: &str = "127.0.0.1";
/// 假账号:`.invalid` 保留顶级域(RFC 2606),与真实账号零碰撞。
const VIRTUAL_EMAIL: &str = "virtual@localhost.invalid";
/// daemon 外联 Anthropic 的兜底:经网关快速失败,回环与官方 MCP 除外(沿用旧脚本清单)。
/// conda/pip 通道也直连:沙箱首次启动要下载内核/MCP 环境,经快速失败代理只会 404
/// (E2E 实测 `Skeleton environment provisioning: 2/2 failed` 与 pypi.org 被拒)。
const NO_PROXY_LIST: &str = "127.0.0.1,localhost,::1,pubmed.mcp.claude.com,hcls.mcp.claude.com,conda.anaconda.org,repo.anaconda.com,pypi.org,files.pythonhosted.org";
const LOCK_FILE: &str = "operon.lock";
const HEALTH_TIMEOUT: Duration = Duration::from_secs(15);
/// 0.1.48 把 encryption keys 迁入 macOS Keychain 时用的 service 名(二进制内硬编码,
/// account 按实例派生;research/science-0148-login-gate.md §2.2)。`security` CLI 走
/// HOME 定位钥匙串,所以 HOME 隔离 + 按路径限定的 find-generic-password 是有效探测。
const KEYCHAIN_SERVICE: &str = "com.anthropic.operon-cli";

/// `~/.csswitch/science-sandbox/` —— 隔离根,CSSwitch 独占管理。
pub fn sandbox_root() -> PathBuf {
    crate::profile::config_dir().join("science-sandbox")
}

/// 隔离 HOME。Keychain 按 HOME 定位,所以只换 `--data-dir` 不够,HOME 必须一起换。
pub fn sandbox_home() -> PathBuf {
    sandbox_root().join("home")
}

/// 隔离 data-dir(显式传给 `--data-dir`),同时也是 forge 的 auth_dir。
pub fn sandbox_data_dir() -> PathBuf {
    sandbox_home().join(".claude-science")
}

fn keychain_path() -> PathBuf {
    sandbox_home().join("Library/Keychains/login.keychain-db")
}

/// 沙箱钥匙串的随机密码落盘处(CSSwitch 自有目录,0600)。空密码在当前 macOS 上
/// 解锁失败(见冒烟记录),必须随机密码并持久化,否则下次启动无法重新解锁。
fn keychain_password_path() -> PathBuf {
    sandbox_root().join("keychain.v1.pass")
}

fn lock_path() -> PathBuf {
    sandbox_data_dir().join(LOCK_FILE)
}

/// 沙箱 daemon 状态。探测靠沙箱 `operon.lock` + 进程存活 + 命令行三重核对,
/// 服务进程重启后仍能识别残留的沙箱 daemon;找不到二进制不影响状态判定。
pub fn status() -> Value {
    let pid = sandbox_process_pid();
    let version = read_lock()
        .and_then(|lock| lock.get("version").and_then(Value::as_str).map(str::to_string));
    json!({
        "initialized": sandbox_data_dir().is_dir(),
        "running": pid.is_some(),
        "port": DAEMON_PORT,
        "pid": pid,
        "version": version,
    })
}

/// 一键启动:forge(幂等)→ SSH / gh 桥接 → 沙箱钥匙串就位 → 端口检查 → 拉起 daemon →
/// 健康检查 → 钥匙串正向校验。桥接与钥匙串配置步骤失败仅警告(桥接缺失只影响 Compute 找
/// 主机别名、沙箱读不到本机 gh 登录),其余任一步失败都显式报错;健康/校验不过会先停
/// daemon 再报错。
pub fn start(proxy_base_url: &str) -> Result<Value, String> {
    if sandbox_process_pid().is_some() {
        return Ok(json!({
            "ok": true,
            "action": "already-running",
            "url": entry_url().ok(),
        }));
    }
    let bin = crate::science::find_binary()?;
    let home = sandbox_home();
    let data_dir = sandbox_data_dir();
    // 1. 铸造/修复虚拟登录(三道护栏在写任何文件之前)。
    let (_forge, action) = crate::sandbox_forge::ensure_virtual_login(&data_dir, VIRTUAL_EMAIL, &home)?;
    chmod_best_effort(&sandbox_root(), 0o700);
    chmod_best_effort(&home, 0o700);
    // 1.5 SSH 桥接:让沙箱 Science 的 Compute 功能按 HOME 相对路径找得到主机别名/指纹。
    ensure_ssh_bridge();
    // 1.6 gh 桥接:让沙箱 Science 的 Credentials 读到本机 gh 登录。
    let gh_wrapper = ensure_gh_bridge();
    // 2. 沙箱钥匙串必须先就位:daemon 启动时会把 encryption keys 迁入 Keychain。
    setup_sandbox_keychain()?;
    // 2b. 修复/铸新后清掉沙箱钥匙串里的旧密钥条目:0.1.48 以 Keychain 为权威,与 file
    //     不一致时会把 file 回写成旧值——不清则本次修复被回写吃掉,新 .enc 解不开,
    //     下次 ensure 又修复,死循环。Reused = file 与 keychain 自洽,一项都不动。
    if should_purge_keychain(action) {
        purge_sandbox_keychain_entries();
    }
    // 3. 固定端口,占用即报错,不自动改绑。
    ensure_port_free(DAEMON_PORT)?;
    ensure_port_free(PREVIEW_PORT)?;
    // 4. 拉起隔离 daemon。沙箱与真实实例共享同一个二进制,自更新由真实实例负责,
    //    沙箱侧钉 `--no-auto-update` 避免它自己去碰更新通道。
    let mut env = daemon_env(&home, proxy_base_url);
    if let Some(wrapper) = gh_wrapper.as_deref() {
        env.extend(gh_bridge_env(
            wrapper,
            std::env::var("PATH").ok().as_deref(),
            std::env::var_os("GIT_CONFIG_COUNT").is_some(),
        ));
    }
    let output = run_cli(&bin, &serve_args(&data_dir), &env)?;
    if !output.status.success() {
        return Err(format!(
            "启动沙箱 daemon 失败:{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // 5. 健康检查,超时即停 daemon 报错。
    if let Err(error) = wait_for_health() {
        let _ = stop();
        return Err(error);
    }
    // 6. 钥匙串正向校验,不过即停 daemon 报错。
    if let Err(error) = verify_keychain_isolation() {
        let _ = stop();
        return Err(error);
    }
    // 记录版本号便于事后归因(共享二进制会随真实实例自更新漂移)。
    let version = read_lock()
        .and_then(|lock| lock.get("version").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "unknown".into());
    crate::log_line!(
        "沙箱 daemon 已启动:port={DAEMON_PORT} version={version} login_action={} gh_bridge={}",
        action.as_str(),
        if gh_wrapper.is_some() { "on" } else { "off" }
    );
    Ok(json!({
        "ok": true,
        "action": action.as_str(),
        "url": entry_url().ok(),
    }))
}

/// 停止:先官方 CLI;兜底只杀「pid 来自沙箱 lock 且命令行含精确 --data-dir」的进程。
pub fn stop() -> Result<(), String> {
    let mut cli_result: Result<(), String> = Ok(());
    if let Ok(bin) = crate::science::find_binary() {
        let args = vec![
            "stop".to_string(),
            "--data-dir".to_string(),
            sandbox_data_dir().display().to_string(),
        ];
        match run_cli(&bin, &args, &sandbox_home_env()) {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
                // 未在运行时 stop 也会非零退出,这不算失败 —— 但不提前返回:CLI 判
                // 「不在跑」而 lock pid 其实活着(sock 失联)时,下面的兜底仍要收口。
                if !stderr.contains("not running") && !stderr.contains("no daemon") {
                    cli_result = Err(format!("停止沙箱 daemon 失败:{}", stderr.trim()));
                }
            }
            Err(error) => cli_result = Err(error),
        }
    }
    let Some(pid) = sandbox_process_pid() else {
        // 没有活着的沙箱 daemon:CLI 已成功,或目标本就不在 —— 目标状态已达成。
        if let Err(error) = cli_result {
            crate::log_line!("沙箱停止:CLI 报错({error}),但未发现存活的沙箱 daemon,视为已停止");
        }
        return Ok(());
    };
    // SAFETY: kill(2) 只发信号;pid 已过 lock 来源 + 存活 + 精确命令行三重校验。
    unsafe { libc::kill(pid, libc::SIGTERM) };
    for _ in 0..30 {
        if !process_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "沙箱 daemon(pid {pid})在 SIGTERM 后 3 秒内未退出,请人工检查"
    ))
}

/// 一次性入口链接(nonce 由同版本官方 CLI 内部消化,协议漂移免疫)。
/// 出口前把主机名钉到 `127.0.0.1`,与真实实例的 `localhost` 分开 cookie 作用域。
pub fn entry_url() -> Result<String, String> {
    let bin = crate::science::find_binary()?;
    let args = vec![
        "url".to_string(),
        "--data-dir".to_string(),
        sandbox_data_dir().display().to_string(),
    ];
    let output = run_cli(&bin, &args, &sandbox_home_env())?;
    if !output.status.success() {
        return Err(format!(
            "获取沙箱入口链接失败:{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    crate::science::extract_url(&stdout)
        .map(|url| pin_cookie_host(&url))
        .ok_or_else(|| "沙箱 daemon 未返回入口链接".to_string())
}

/// 把入口链接的主机名换成 `COOKIE_HOST`,端口/路径/nonce 一概不动。
///
/// 浏览器 cookie 只按主机名划分作用域、不看端口(RFC 6265 §5.1.3/§5.3),而 Science 的
/// `operon_auth` / `operon_csrf` 都是 host-only(签发时不带 Domain 属性)。daemon 的
/// `url` 子命令在 macOS 上固定打印 `localhost`,于是沙箱与真实实例虽然端口不同,却共用
/// 同一份 cookie —— 哪边后登录,哪边就把对方顶掉。
///
/// 换主机名不需要给 daemon 加任何参数:0.1.48 的 Host 闸门是
/// `^(?:127\.0\.0\.1|localhost)(?::\d+)?$`,写操作的 Origin 闸门是
/// `^https?://(?:127\.0\.0\.1|localhost)(?::\d+)?$`,两者都原样放行 `127.0.0.1`;
/// HTML 预览口(8791)的 origin 由 daemon 自己按 `http://127.0.0.1:<port>` 组装,不受影响。
///
/// 只改沙箱一侧:真实实例的 claude.ai OAuth 回调地址固定归一到 `localhost`,且它是用户
/// 可收藏的稳定入口,动它会伤到真实登录。解析失败时原样返回 —— 宁可退回旧行为(会话互挤),
/// 也不吐出一个拼坏的链接。
fn pin_cookie_host(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    if parsed.set_host(Some(COOKIE_HOST)).is_err() {
        return url.to_string();
    }
    parsed.to_string()
}

// ---------- 启动组装(纯函数,供单测钉死) ----------
fn serve_args(data_dir: &Path) -> Vec<String> {
    vec![
        "serve".to_string(),
        "--data-dir".to_string(),
        data_dir.display().to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        DAEMON_PORT.to_string(),
        "--sandbox-port".to_string(),
        PREVIEW_PORT.to_string(),
        "--no-browser".to_string(),
        "--no-auto-update".to_string(),
        "--detached".to_string(),
    ]
}

fn daemon_env(sandbox_home: &Path, proxy_base_url: &str) -> Vec<(String, String)> {
    // Anthropic HTTPS 全部指向网关快速失败(网关对非推理路径显式 404),而不是挂住。
    let fastfail = proxy_base_url.to_string();
    vec![
        ("HOME".to_string(), sandbox_home.display().to_string()),
        ("ANTHROPIC_BASE_URL".to_string(), proxy_base_url.to_string()),
        ("https_proxy".to_string(), fastfail.clone()),
        ("HTTPS_PROXY".to_string(), fastfail),
        ("no_proxy".to_string(), NO_PROXY_LIST.to_string()),
        ("NO_PROXY".to_string(), NO_PROXY_LIST.to_string()),
    ]
}

fn sandbox_home_env() -> Vec<(String, String)> {
    vec![("HOME".to_string(), sandbox_home().display().to_string())]
}

fn run_cli(
    bin: &Path,
    args: &[String],
    envs: &[(String, String)],
) -> Result<std::process::Output, String> {
    let mut command = Command::new(bin);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in crate::science::MODEL_ENV_KEYS_TO_CLEAR {
        command.env_remove(key);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    command
        .output()
        .map_err(|e| format!("执行 claude-science 失败:{e}"))
}

fn ensure_port_free(port: u16) -> Result<(), String> {
    match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(e) => Err(format!(
            "沙箱端口 {port} 已被占用:{e}。免登录沙箱使用固定端口,请释放后重试。"
        )),
    }
}

fn wait_for_health() -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        // 只打回环,绝不走系统代理。
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("http://127.0.0.1:{DAEMON_PORT}/health");
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    loop {
        if let Ok(response) = client.get(&url).send() {
            if response.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "沙箱 daemon 健康检查超时:{HEALTH_TIMEOUT:?} 内 127.0.0.1:{DAEMON_PORT}/health 未就绪"
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

// ---------- SSH 桥接 ----------
/// 把真实 `~/.ssh` 的 `config` 与 `known_hosts` 链接进沙箱 HOME:Science 的 Compute/SSH
/// 按 `$HOME/.ssh/config` 找主机别名,沙箱 HOME 下原本什么都没有(E2E 用户实测报障)。
/// 私钥一律不链接——认证走继承的 `SSH_AUTH_SOCK`(gateway 进程环境默认继承)或 config 里
/// 的绝对 `IdentityFile` 路径。注意:沙箱 daemon 与真实实例同为当前 OS 用户,无文件系统级
/// 隔离;HOME 换向管的是配置命名空间,不是访问控制——这里只是把 ssh 按 HOME 相对查找的
/// 两个只读文件补回去。
fn ensure_ssh_bridge() {
    link_ssh_entries(
        &crate::profile::home().join(".ssh"),
        &sandbox_home().join(".ssh"),
    );
}

fn link_ssh_entries(real_ssh: &Path, sandbox_ssh: &Path) {
    if std::fs::create_dir_all(sandbox_ssh).is_err() {
        crate::log_line!("警告:沙箱 .ssh 目录创建失败(SSH 桥接跳过,不影响其他功能)");
        return;
    }
    chmod_best_effort(sandbox_ssh, 0o700);
    for name in ["config", "known_hosts"] {
        let src = real_ssh.join(name);
        let dst = sandbox_ssh.join(name);
        // 源不存在跳过;目标已存在(文件或链接)绝不覆盖——用户可能故意换成自己的副本。
        if !src.exists() || dst.symlink_metadata().is_ok() {
            continue;
        }
        if std::os::unix::fs::symlink(&src, &dst).is_err() {
            crate::log_line!("警告:沙箱 SSH 桥接 {name} 链接失败(不影响其他功能)");
        }
    }
}

// ---------- gh 桥接 ----------
/// 沙箱专用可执行目录,排在沙箱 daemon PATH 的最前面。
fn sandbox_bin_dir() -> PathBuf {
    sandbox_root().join("bin")
}

/// 让沙箱 Science 读到本机 gh 登录(它的 Credentials 页按 `gh auth token` 与
/// `git credential fill` 探测)。gh 的令牌默认存在 macOS 登录钥匙串,沙箱 HOME 换向后钥匙串
/// 搜索表只剩沙箱钥匙串,所以只链接 `~/.config/gh` 拿不到令牌;把真实登录钥匙串挂进沙箱
/// 搜索表又会让沙箱 daemon 读到真实实例的加密密钥。折中是在沙箱 PATH 最前面放一个 gh
/// 包装脚本:只有 gh 进程以真实 HOME 运行。脚本每次启动重写(本机 gh 路径可能变);
/// 本机没装 gh 时删掉旧脚本,返回 `None`。
fn ensure_gh_bridge() -> Option<PathBuf> {
    let bin_dir = sandbox_bin_dir();
    let wrapper = bin_dir.join("gh");
    let real_gh = std::env::var_os("PATH")
        .and_then(|path| find_executable("gh", &path, &bin_dir));
    let Some(real_gh) = real_gh else {
        let _ = std::fs::remove_file(&wrapper);
        return None;
    };
    match write_gh_wrapper(&wrapper, &crate::profile::home(), &real_gh) {
        Ok(()) => Some(wrapper),
        Err(error) => {
            crate::log_line!("警告:沙箱 gh 桥接写入失败({error}),沙箱内读不到本机 gh 登录");
            None
        }
    }
}

/// 按 PATH 顺序找可执行文件,跳过沙箱自己的 bin 目录(否则会找到包装脚本自身)。
fn find_executable(name: &str, path: &std::ffi::OsStr, skip_dir: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .filter(|dir| dir != skip_dir)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// POSIX sh 单引号转义。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn gh_wrapper_script(real_home: &str, real_gh: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # 由 CSSwitch 生成,每次启动沙箱时重写。以真实 HOME 运行本机 gh,\n\
         # 让沙箱 Science 读到本机 gh 登录(配置文件与登录钥匙串)。\n\
         HOME={} exec {} \"$@\"\n",
        shell_quote(real_home),
        shell_quote(real_gh)
    )
}

/// 先写临时文件再改名,避免 daemon 恰好执行到半截脚本。
fn write_gh_wrapper(wrapper: &Path, real_home: &Path, real_gh: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let (Some(home), Some(gh)) = (real_home.to_str(), real_gh.to_str()) else {
        return Err("路径不是 UTF-8".into());
    };
    let dir = wrapper.parent().ok_or("包装脚本路径没有父目录")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = wrapper.with_extension("tmp");
    std::fs::write(&tmp, gh_wrapper_script(home, gh)).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, wrapper).map_err(|e| e.to_string())
}

/// gh 桥接需要的 daemon 环境。
///
/// - `PATH`:包装脚本目录排最前。继承的 PATH 不可用时不覆盖(只剩一个目录会让 daemon
///   找不到任何系统命令),此时 `gh auth token` 探测不通,git 部分仍然有效。
/// - git:对 github.com / gist.github.com 先写空值清掉已有凭据助手(系统级 osxkeychain 在
///   沙箱 HOME 下只会查沙箱钥匙串),再改用 `gh auth git-credential`——与
///   `gh auth setup-git` 写入的配置等价,但只经 `GIT_CONFIG_*` 环境注入,不写沙箱
///   `~/.gitconfig`。继承环境里已有 `GIT_CONFIG_COUNT` 时不覆盖,只留 PATH 部分。
fn gh_bridge_env(
    wrapper: &Path,
    inherited_path: Option<&str>,
    git_config_env_taken: bool,
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let bin_dir = wrapper.parent().map(|dir| dir.display().to_string()).unwrap_or_default();
    match inherited_path.filter(|path| !path.is_empty()) {
        Some(path) => env.push(("PATH".to_string(), format!("{bin_dir}:{path}"))),
        None => crate::log_line!("警告:继承的 PATH 不可用,沙箱 gh 桥接只对 git 生效"),
    }
    if git_config_env_taken {
        crate::log_line!("警告:环境里已有 GIT_CONFIG_COUNT,沙箱 git 不改用 gh 凭据");
        return env;
    }
    let helper = format!("!{} auth git-credential", shell_quote(&wrapper.display().to_string()));
    let mut index = 0;
    for host in ["https://github.com", "https://gist.github.com"] {
        for value in ["", helper.as_str()] {
            env.push((format!("GIT_CONFIG_KEY_{index}"), format!("credential.{host}.helper")));
            env.push((format!("GIT_CONFIG_VALUE_{index}"), value.to_string()));
            index += 1;
        }
    }
    env.push(("GIT_CONFIG_COUNT".to_string(), index.to_string()));
    env
}

// ---------- 沙箱钥匙串 ----------
/// 创建(随机密码)→ 挂入搜索表 → 设为默认 → 解锁 → 关自动锁,全部只作用于沙箱 HOME。
/// `security` 步骤失败仅警告(原始输出可能含路径,不记录),最终由启动后的正向校验收口;
/// 密码文件失败则直接报错——没有它下次启动必无法解锁,带病继续没有意义。
fn setup_sandbox_keychain() -> Result<(), String> {
    let home = sandbox_home();
    let keychain = keychain_path();
    if let Some(parent) = keychain.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建沙箱钥匙串目录失败:{e}"))?;
    }
    let password = keychain_password()?;
    let path = keychain.display().to_string();
    let security = |args: &[&str]| -> bool {
        Command::new("security")
            .args(args)
            .env("HOME", &home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };
    if !keychain.is_file() && !security(&["create-keychain", "-p", password.as_str(), path.as_str()])
    {
        crate::log_line!("警告:沙箱钥匙串创建失败(原始输出可能含路径,未记录)");
    }
    for args in [
        vec!["list-keychains", "-d", "user", "-s", path.as_str()],
        vec!["default-keychain", "-d", "user", "-s", path.as_str()],
        vec!["unlock-keychain", "-p", password.as_str(), path.as_str()],
        vec!["set-keychain-settings", path.as_str()],
    ] {
        if !security(&args) {
            crate::log_line!("警告:沙箱钥匙串配置步骤 {} 未成功(输出未记录)", args[0]);
        }
    }
    Ok(())
}

fn keychain_password() -> Result<String, String> {
    let path = keychain_password_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let password = crate::sandbox_forge::rand_hex(24)?;
    // 复用 forge 的安全写:拒符号链接 + 临时文件 + rename,0600 从创建即生效,
    // 密码文件不以 umask 默认权限存在过任何瞬间。
    crate::sandbox_forge::safe_write(&path, format!("{password}\n").as_bytes(), 0o600)
        .map_err(|e| format!("写入沙箱钥匙串密码失败:{e}"))?;
    Ok(password)
}

/// 是否需要清沙箱钥匙串旧条目:仅 forge 修复/铸新时。Reused = 现有 file 与 keychain
/// 自洽(0.1.48 启动时比对过),一项都不动。
fn should_purge_keychain(action: LoginAction) -> bool {
    action != LoginAction::Reused
}

/// 清空沙箱钥匙串里 `com.anthropic.operon-cli` 的全部条目(best-effort)。
/// 调用前提:setup_sandbox_keychain() 已跑过(钥匙串已解锁)。每次调用删一项
/// (account 按实例派生,正常只有一项),删到「找不到」为止,上限 8 次防死循环;
/// 清不干净也不报错——启动后的正向校验是最终闸门。
fn purge_sandbox_keychain_entries() {
    let home = sandbox_home();
    let path = keychain_path().display().to_string();
    let mut purged = 0_u32;
    for _ in 0..8 {
        let deleted = Command::new("security")
            .args(["delete-generic-password", "-s", KEYCHAIN_SERVICE, path.as_str()])
            .env("HOME", &home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !deleted {
            break;
        }
        purged += 1;
    }
    crate::log_line!("沙箱登录已修复/铸新:清理沙箱钥匙串旧密钥条目 {purged} 项(daemon 将从修复后的 file 重新迁入)");
}

/// 启动后正向校验:daemon 迁入的加密密钥必须能在沙箱钥匙串内找到(按 0.1.48 硬编码
/// service 名 + 沙箱钥匙串路径限定搜索,不读真实 keychain)。校验失败 = 密钥可能落进了
/// 用户真实 login keychain —— 调用方停 daemon 报错,不带病运行。
fn verify_keychain_isolation() -> Result<(), String> {
    let home = sandbox_home();
    let path = keychain_path().display().to_string();
    // 迁入发生在 daemon 启动早期,健康就绪后短暂重试以吸收时序差。
    for _ in 0..10 {
        let found = Command::new("security")
            .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, path.as_str()])
            .env("HOME", &home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if found {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Err("沙箱钥匙串正向校验失败:未在沙箱钥匙串中找到 daemon 迁入的加密密钥,\
         已停止沙箱 daemon(防止密钥落入真实钥匙串)。\
         请把 daemon 版本与启动日志一并反馈。"
        .into())
}

// ---------- 进程探测 ----------
fn read_lock() -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(lock_path()).ok()?).ok()
}

fn lock_pid(lock: &Value) -> Option<i32> {
    let pid = lock.get("pid")?.as_i64()?;
    let pid = i32::try_from(pid).ok()?;
    (pid > 0).then_some(pid)
}

/// 存活的沙箱 daemon pid:lock 里有 pid + 进程活着 + 命令行含精确 `--data-dir`,
/// 三重都过才算数(pid 复用不得误判)。
fn sandbox_process_pid() -> Option<i32> {
    let pid = lock_pid(&read_lock()?)?;
    if !process_alive(pid) {
        return None;
    }
    let cmdline = process_command_line(pid)?;
    command_line_contains_data_dir(&cmdline, &sandbox_data_dir()).then_some(pid)
}

fn process_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) 不发信号,只做存在性/权限探测。
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn process_command_line(pid: i32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "command=", "-p", pid.to_string().as_str()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn command_line_contains_data_dir(cmdline: &str, data_dir: &Path) -> bool {
    // argv[0] 必须是 claude-science 本体(不能整体 contains: data-dir 路径里本来
    // 就带 ".claude-science" 字样,会把任何进程的命令行都误判成沙箱 daemon)。
    let tokens: Vec<&str> = cmdline.split_whitespace().collect();
    let program_is_science = tokens
        .first()
        .map(|arg0| arg0.ends_with("claude-science"))
        .unwrap_or(false);
    if !program_is_science {
        return false;
    }
    // 必须是 serve 形态的 daemon(对齐 0.1.48 自带 pidIsOperonDaemon 的判定),
    // 排除 `url`/`stop` 这类带相同 --data-dir 的瞬时 CLI 调用。
    if !tokens.contains(&"serve") {
        return false;
    }
    // 逐 token 精确匹配 `--data-dir <path>`:子串匹配会把 `...claude-science-other`
    // 这类前缀相似的目录误判成命中。
    let expected = data_dir.display().to_string();
    tokens
        .windows(2)
        .any(|pair| pair[0] == "--data-dir" && pair[1] == expected)
}

fn chmod_best_effort(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_args_pin_the_sandbox_layout() {
        let data_dir = Path::new("/tmp/sandbox/home/.claude-science");
        let args = serve_args(data_dir);
        let joined = args.join(" ");
        assert!(joined.contains("--data-dir /tmp/sandbox/home/.claude-science"));
        assert!(joined.contains("--host 127.0.0.1"));
        assert!(joined.contains("--port 8790"));
        assert!(joined.contains("--sandbox-port 8791"));
        assert!(joined.contains("--no-browser"));
        assert!(joined.contains("--no-auto-update"));
        assert!(joined.contains("--detached"));
        assert!(!joined.contains("8765"), "绝不碰真实实例保留端口");
    }

    #[test]
    fn entry_url_host_is_pinned_away_from_the_real_instance() {
        // 回归:daemon 的 `url` 子命令在 macOS 上固定打印 localhost,不换主机名两实例
        // 就共用同一份 host-only cookie,后登录的一方会把另一方顶下线。
        assert_eq!(
            pin_cookie_host("http://localhost:8790/?nonce=abc123"),
            "http://127.0.0.1:8790/?nonce=abc123"
        );
        // 端口、路径、nonce 一概不动。
        assert_eq!(
            pin_cookie_host("http://localhost:8790/bio/?nonce=abc123"),
            "http://127.0.0.1:8790/bio/?nonce=abc123"
        );
        // 幂等:daemon 若已经给出 127.0.0.1(url_host 被它自己覆盖时)不重复改写。
        assert_eq!(
            pin_cookie_host("http://127.0.0.1:8790/?nonce=abc123"),
            "http://127.0.0.1:8790/?nonce=abc123"
        );
        // 解析不了就原样返回,绝不吐出拼坏的链接。
        assert_eq!(pin_cookie_host("not a url"), "not a url");
        assert_ne!(
            COOKIE_HOST, "localhost",
            "沙箱主机名必须与真实实例的 localhost 不同,否则 cookie 作用域又合流"
        );
    }

    #[test]
    fn daemon_env_injects_home_base_url_and_fastfail_proxy() {
        let env = daemon_env(Path::new("/tmp/sandbox/home"), "http://127.0.0.1:8788");
        let get = |key: &str| {
            env.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("HOME"), Some("/tmp/sandbox/home"));
        assert_eq!(get("ANTHROPIC_BASE_URL"), Some("http://127.0.0.1:8788"));
        // 外联快速失败代理 = 网关自身(非推理路径显式 404)。
        assert_eq!(get("https_proxy"), Some("http://127.0.0.1:8788"));
        assert_eq!(get("HTTPS_PROXY"), Some("http://127.0.0.1:8788"));
        assert_eq!(get("no_proxy"), Some(NO_PROXY_LIST));
        assert!(NO_PROXY_LIST.contains("127.0.0.1"), "回环必须走直连");
        // 模型/凭证类变量不在注入清单里(它们走 env_remove 清除,清单与 science.rs 共享)。
        assert!(get("ANTHROPIC_MODEL").is_none());
        assert!(get("ANTHROPIC_API_KEY").is_none());
    }

    #[test]
    fn sandbox_ports_avoid_the_real_instance() {
        assert_ne!(DAEMON_PORT, 8765);
        assert_ne!(PREVIEW_PORT, 8765);
        assert_ne!(DAEMON_PORT, 8767);
        assert_ne!(PREVIEW_PORT, 8767);
        assert_ne!(DAEMON_PORT, 8788, "不得与网关自身冲突");
        assert_ne!(PREVIEW_PORT, 8788);
        assert_eq!(PREVIEW_PORT, DAEMON_PORT + 1);
    }

    #[test]
    fn occupied_port_is_a_clear_error() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let error = ensure_port_free(port).unwrap_err();
        assert!(error.contains("已被占用"), "占用必须报明确错误:{error}");
        drop(listener);
        assert!(ensure_port_free(port).is_ok(), "释放后应可通过");
    }

    #[test]
    fn lock_pid_parsing_is_strict() {
        assert_eq!(lock_pid(&json!({"pid": 123})), Some(123));
        assert_eq!(lock_pid(&json!({"pid": 0})), None);
        assert_eq!(lock_pid(&json!({"pid": -1})), None);
        assert_eq!(lock_pid(&json!({"pid": "123"})), None);
        assert_eq!(lock_pid(&json!({})), None);
    }

    #[test]
    fn keychain_purge_only_when_login_was_rewritten() {
        // 钉死「仅非 Reused 才清」:Reused 时 file 与 keychain 自洽,一项都不能动;
        // Repaired/Created 时 file 已被 forge 重写,旧 keychain 条目必须失效,
        // 否则 0.1.48 的 Keychain 权威回写会吃掉修复(login-gate §2.2)。
        assert!(!should_purge_keychain(LoginAction::Reused));
        assert!(should_purge_keychain(LoginAction::Repaired));
        assert!(should_purge_keychain(LoginAction::Created));
    }

    #[test]
    fn fallback_kill_requires_the_exact_data_dir() {
        let data_dir = Path::new("/Users/x/.csswitch/science-sandbox/home/.claude-science");
        // 正例:沙箱 daemon 本人的命令行。
        assert!(command_line_contains_data_dir(
            "/Users/x/.claude-science/bin/claude-science serve --data-dir \
             /Users/x/.csswitch/science-sandbox/home/.claude-science --host 127.0.0.1 --port 8790",
            data_dir
        ));
        // pid 复用到别的进程:不含 claude-science → 拒。
        assert!(!command_line_contains_data_dir(
            "/usr/bin/python3 -m http.server --data-dir /Users/x/.csswitch/science-sandbox/home/.claude-science",
            data_dir
        ));
        // 真实实例:含 claude-science 但 data-dir 不同 → 拒。
        assert!(!command_line_contains_data_dir(
            "/Users/x/.claude-science/bin/claude-science serve --detached --no-browser",
            data_dir
        ));
        // 前缀相似的别的目录(……/home/.claude-science-other)→ 拒。
        assert!(!command_line_contains_data_dir(
            "/Users/x/.claude-science/bin/claude-science serve --data-dir \
             /Users/x/.csswitch/science-sandbox/home/.claude-science-other --port 8999",
            data_dir
        ));
        // 同一 data-dir 的瞬时 CLI 调用(url/stop,无 serve)→ 拒(对齐 daemon 自身的
        // pidIsOperonDaemon 判定)。
        assert!(!command_line_contains_data_dir(
            "/Users/x/.claude-science/bin/claude-science url --data-dir \
             /Users/x/.csswitch/science-sandbox/home/.claude-science",
            data_dir
        ));
    }

    // ---------- gh 桥接 ----------
    fn gh_test_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "csswitch-gh-test-{tag}-{}-{}",
            std::process::id(),
            crate::sandbox_forge::rand_hex(4).unwrap()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn write_exec(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn shell_quote_survives_single_quotes_and_spaces() {
        assert_eq!(shell_quote("/Users/a b/c"), "'/Users/a b/c'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn gh_wrapper_forces_the_real_home_and_forwards_arguments() {
        // 行为级校验:用一个打印 HOME 与参数的假 gh,在「沙箱 HOME」下执行包装脚本。
        let base = gh_test_dir("wrap");
        let real_home = base.join("real home");
        let fake_gh = base.join("tools/gh");
        write_exec(&fake_gh, "#!/bin/sh\necho \"$HOME|$*\"\n");
        let wrapper = base.join("sandbox/bin/gh");
        write_gh_wrapper(&wrapper, &real_home, &fake_gh).unwrap();
        let output = std::process::Command::new(&wrapper)
            .args(["auth", "token"])
            .env("HOME", base.join("sandbox-home"))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            format!("{}|auth token", real_home.display())
        );
        assert!(!wrapper.with_extension("tmp").exists(), "临时文件必须被改名掉");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn find_executable_skips_the_sandbox_bin_and_non_executables() {
        let base = gh_test_dir("find");
        let sandbox_bin = base.join("sandbox-bin");
        let plain = base.join("plain");
        let tools = base.join("tools");
        write_exec(&sandbox_bin.join("gh"), "#!/bin/sh\n");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("gh"), "not executable").unwrap();
        write_exec(&tools.join("gh"), "#!/bin/sh\n");
        let path = std::env::join_paths([&sandbox_bin, &plain, &tools]).unwrap();
        assert_eq!(find_executable("gh", &path, &sandbox_bin), Some(tools.join("gh")));
        let only_sandbox = std::env::join_paths([&sandbox_bin]).unwrap();
        assert_eq!(find_executable("gh", &only_sandbox, &sandbox_bin), None);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn gh_bridge_env_prepends_path_and_routes_github_git_credentials_to_gh() {
        let env = gh_bridge_env(Path::new("/sb/bin/gh"), Some("/usr/bin:/bin"), false);
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("PATH"), Some("/sb/bin:/usr/bin:/bin"));
        assert_eq!(get("GIT_CONFIG_COUNT"), Some("4"));
        for (index, host) in [(0, "github.com"), (2, "gist.github.com")] {
            let key = format!("credential.https://{host}.helper");
            // 先清空(去掉沙箱 HOME 下无效的 osxkeychain),再指向 gh。
            assert_eq!(get(&format!("GIT_CONFIG_KEY_{index}")), Some(key.as_str()));
            assert_eq!(get(&format!("GIT_CONFIG_VALUE_{index}")), Some(""));
            assert_eq!(get(&format!("GIT_CONFIG_KEY_{}", index + 1)), Some(key.as_str()));
            assert_eq!(
                get(&format!("GIT_CONFIG_VALUE_{}", index + 1)),
                Some("!'/sb/bin/gh' auth git-credential")
            );
        }
    }

    #[test]
    fn gh_bridge_env_never_clobbers_existing_git_config_env_or_a_missing_path() {
        let env = gh_bridge_env(Path::new("/sb/bin/gh"), Some("/usr/bin"), true);
        assert_eq!(env, vec![("PATH".to_string(), "/sb/bin:/usr/bin".to_string())]);
        // PATH 不可用时不能只留包装目录,否则 daemon 找不到任何系统命令。
        let env = gh_bridge_env(Path::new("/sb/bin/gh"), None, false);
        assert!(env.iter().all(|(k, _)| k != "PATH"));
        assert!(env.iter().any(|(k, _)| k == "GIT_CONFIG_COUNT"));
    }

    // ---------- SSH 桥接 ----------
    fn ssh_test_dirs() -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "csswitch-ssh-test-{}-{}",
            std::process::id(),
            crate::sandbox_forge::rand_hex(4).unwrap()
        ));
        let real = base.join("real");
        let sand = base.join("sand");
        std::fs::create_dir_all(&real).unwrap();
        (base, real, sand)
    }

    #[test]
    fn ssh_bridge_links_both_files_and_resolves_to_real_paths() {
        let (base, real, sand) = ssh_test_dirs();
        std::fs::write(real.join("config"), "Host x").unwrap();
        std::fs::write(real.join("known_hosts"), "host key").unwrap();
        link_ssh_entries(&real, &sand);
        for name in ["config", "known_hosts"] {
            assert_eq!(
                std::fs::read_link(sand.join(name)).unwrap(),
                real.join(name),
                "{name} 必须是指向真实 .ssh 的符号链接"
            );
        }
        assert_eq!(
            std::fs::read_dir(&sand).unwrap().count(),
            2,
            "白名单外不多建任何条目"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn ssh_bridge_skips_missing_sources() {
        let (base, real, sand) = ssh_test_dirs();
        std::fs::write(real.join("config"), "Host x").unwrap();
        // known_hosts 源缺失 → 不建链接,也不报错。
        link_ssh_entries(&real, &sand);
        assert!(sand.join("config").symlink_metadata().is_ok());
        assert!(sand.join("known_hosts").symlink_metadata().is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn ssh_bridge_never_overwrites_existing_destination() {
        let (base, real, sand) = ssh_test_dirs();
        std::fs::write(real.join("config"), "Host real").unwrap();
        std::fs::create_dir_all(&sand).unwrap();
        std::fs::write(sand.join("config"), "Host mine").unwrap();
        link_ssh_entries(&real, &sand);
        // 用户自己放的副本原样保留,不被替换成符号链接。
        assert_eq!(
            std::fs::read_to_string(sand.join("config")).unwrap(),
            "Host mine"
        );
        assert!(sand.join("config").symlink_metadata().unwrap().file_type().is_file());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn ssh_bridge_never_links_private_keys() {
        let (base, real, sand) = ssh_test_dirs();
        std::fs::write(real.join("config"), "Host x").unwrap();
        std::fs::write(real.join("id_ed25519"), "PRIVATE KEY MATERIAL").unwrap();
        link_ssh_entries(&real, &sand);
        assert!(
            sand.join("id_ed25519").symlink_metadata().is_err(),
            "私钥绝不链接进沙箱(认证走 SSH_AUTH_SOCK / 绝对 IdentityFile)"
        );
        let _ = std::fs::remove_dir_all(base);
    }
}
