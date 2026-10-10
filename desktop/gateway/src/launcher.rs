//! 安装版的启动器角色。
//!
//! 应用包里只有一个可执行文件:经 Finder / Launchpad 打开时(Info.plist 的
//! `LSEnvironment` 设了 [`APP_LAUNCH_ENV`])它先以这个角色运行,做完下面的事就退出:
//!
//! 1. 服务已经在跑 → 只打开控制台(版本不一致时先提示);
//! 2. 没在跑 → 读登录 shell 的环境,按白名单并进子进程环境,脱离式拉起
//!    `serve --port N --log-file ~/.csswitch/service.log`,等它就绪后打开控制台;
//! 3. 起不来 → 弹提示(没有终端也没有窗口,这是用户唯一看得见的出口),以非零退出。
//!
//! **启动器必须干完就退。** LaunchServices 以「应用主进程是否还活着」判断应用是否在运行;
//! 主进程若就是常驻服务,第二次双击不会再执行任何代码,控制台也就打不开。
//!
//! 登录 shell 环境是为了与 `./csswitch` 等价:从 Finder 启动的进程只有 launchd 的环境,
//! 没有用户在 shell 里配置的代理与 PATH。不导入的话,需要代理的用户官方模式直连失败、
//! 免登录沙箱找不到本机 `gh`,而界面上看不出任何区别。

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

/// 只由应用包的 Info.plist(`LSEnvironment`)设置。启动器拉起服务时会把它从子进程环境里
/// 移除,并显式带 `serve` 子命令,服务角色不会误入启动器分支。
pub const APP_LAUNCH_ENV: &str = "CSSWITCH_APP_LAUNCH";

/// 显式指定 `open` 与 `osascript` 所在目录。只为隔离测试准备(换成记录调用的假命令),
/// 生效时在日志里留痕。
const HELPER_DIR_ENV: &str = "CSSWITCH_APP_HELPER_DIR";
const DEFAULT_HELPER_DIR: &str = "/usr/bin";
const DEFAULT_SHELL: &str = "/bin/zsh";

/// 从登录 shell 导入的变量。只有这些:网络出口(代理)、可执行文件查找(PATH)、
/// Science 二进制的显式 override。模型与凭证类变量(`ANTHROPIC_*`)不在其内。
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "CLAUDE_SCIENCE_BIN",
];

/// 回环上没有监听时连接立刻被拒,这个超时只在极端情况下才会走满。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// 连上之后等状态接口回话的上限。它不是纯回环往返:服务要先执行一次
/// `claude-science status` 才回话,冷启动时可能超过一秒 —— 超时过短会把正在运行的
/// 服务判成「没在跑」,随后的拉起必然因端口被占而失败。
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const SHELL_ENV_TIMEOUT: Duration = Duration::from_secs(5);
const READY_POLL: Duration = Duration::from_millis(200);
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// 提示框无人理会时自动消失的秒数。启动器要等提示框关掉才退出;没有这个上限,
/// 一个没被注意到的提示框会让应用一直处于「运行中」,之后的双击全部无效。
const ALERT_GIVE_UP_SECS: u32 = 120;

pub fn run() -> ExitCode {
    let helpers = Helpers::from_env();
    let log_path = crate::service_log::default_path();
    let mut log = match crate::service_log::open(&log_path) {
        Ok(file) => Log { file },
        Err(error) => {
            let reason = format!("无法打开日志文件 {}:{error}", log_path.display());
            eprintln!("启动器:{reason}");
            let _ = helpers.alert(AlertKind::Critical, "CSSwitch 无法启动", &reason);
            return ExitCode::FAILURE;
        }
    };
    if helpers.overridden {
        log.line(&format!(
            "{HELPER_DIR_ENV} 生效:open 与 osascript 取自 {}",
            helpers.dir.display()
        ));
    }
    match launch(&helpers, &log_path, &mut log) {
        Ok(()) => ExitCode::SUCCESS,
        Err(LaunchError::ServiceDown(reason)) => {
            // 先取服务留下的最后一行,再写启动器自己的失败记录,否则取到的是自己那行。
            let last_line = last_log_line(&log_path);
            log.line(&format!("启动失败:{reason}"));
            let mut message = format!("{reason}\n\n日志:{}", log_path.display());
            if let Some(line) = last_line {
                message.push_str(&format!("\n最后一行:{line}"));
            }
            if let Err(error) = helpers.alert(AlertKind::Critical, "CSSwitch 无法启动", &message)
            {
                log.line(&format!("提示框没有弹出来:{error}"));
            }
            ExitCode::FAILURE
        }
        Err(LaunchError::ConsoleNotOpened(reason)) => {
            // 服务在跑,只是浏览器没开:标题不能说「无法启动」,日志最后一行也与此无关。
            log.line(&format!("控制台没有打开:{reason}"));
            if let Err(error) = helpers.alert(
                AlertKind::Warning,
                "CSSwitch 已启动,但没能打开控制台",
                &reason,
            ) {
                log.line(&format!("提示框没有弹出来:{error}"));
            }
            ExitCode::FAILURE
        }
    }
}

/// 启动器的两种失败。区分它们是因为提示框要说实话:后者服务已经在跑,
/// 用户只需手动打开地址,不是「无法启动」。
enum LaunchError {
    /// 服务没起来(或根本没拉起)。提示附日志最后一行,那通常就是原因。
    ServiceDown(String),
    /// 服务在跑,`open` 没能打开浏览器。提示里带控制台地址。
    ConsoleNotOpened(String),
}

fn launch(helpers: &Helpers, log_path: &Path, log: &mut Log) -> Result<(), LaunchError> {
    use LaunchError::{ConsoleNotOpened, ServiceDown};
    let port = crate::profile::Profile::load().port;
    let console = format!("http://127.0.0.1:{port}/");
    // 回环探测不能走代理:用户环境里的 HTTP_PROXY 会把 127.0.0.1 的请求带到代理上去。
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|error| ServiceDown(format!("无法初始化本地探测:{error}")))?;

    if let Some(status) = fetch_status(&client, port, RESPONSE_TIMEOUT) {
        match compare_version(&status, crate::VERSION) {
            VersionCheck::Same => log.line("服务已在运行,只打开控制台"),
            mismatch => {
                let message = mismatch_message(&mismatch, crate::VERSION);
                log.line(&message);
                // 等用户关掉提示再开控制台:浏览器一到前台,提示框就被盖住了。
                if let Err(error) = helpers.alert(
                    AlertKind::Warning,
                    "CSSwitch 后台服务不是当前版本",
                    &message,
                ) {
                    log.line(&format!("提示框没有弹出来:{error}"));
                }
            }
        }
        return helpers.open(&console).map_err(ConsoleNotOpened);
    }

    let env = match login_shell_env() {
        Ok(env) => {
            // 只记变量名:代理地址里可能带口令。
            let names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
            log.line(&format!("已从登录 shell 导入:{}", names.join(" ")));
            env
        }
        Err(reason) => {
            log.line(&format!(
                "没有取到登录 shell 的环境,沿用应用自身的环境继续启动\
                 (代理与 PATH 可能与终端里不同):{reason}"
            ));
            Vec::new()
        }
    };

    let mut child = spawn_service(port, log_path, &log.file, &env)
        .map_err(|error| ServiceDown(format!("无法拉起后台服务:{error}")))?;
    log.line(&format!("已拉起后台服务(pid {},端口 {port})", child.id()));
    match wait_until_ready(&mut child, &client, port) {
        Startup::Ready => helpers.open(&console).map_err(ConsoleNotOpened),
        Startup::Exited(status) => Err(ServiceDown(format!(
            "后台服务刚启动就退出了({status})。常见原因是端口 {port} 被别的程序占用。"
        ))),
        Startup::TimedOut => {
            // 起了但一直不回话:留着它只会让下一次启动撞上端口,结束掉,状态才是确定的。
            let _ = child.kill();
            let _ = child.wait();
            Err(ServiceDown(format!(
                "后台服务在 {} 秒内没有就绪,已结束它。",
                READY_TIMEOUT.as_secs()
            )))
        }
    }
}

/// 启动器自己的日志行,与服务写进同一个文件;同时打到 stderr,方便在终端里直接运行时查看。
struct Log {
    file: File,
}

impl Log {
    fn line(&mut self, message: &str) {
        let line = format!("[{}] 启动器:{message}\n", crate::utc_timestamp_ms());
        let _ = self.file.write_all(line.as_bytes());
        eprint!("{line}");
    }
}

/// 日志文件末尾最后一个非空行(只看最后 4 KiB)。另开只读句柄:写日志的那个是只追加的。
fn last_log_line(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(4096))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    String::from_utf8_lossy(&tail)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

// ── 状态探测与版本比对 ─────────────────────────────────────────────────────

/// 读一次状态接口。只有「看起来是 CSSwitch 的状态」才算服务在跑:别的程序占着端口时
/// 返回 `None`,让后面的拉起去撞端口并把真实的系统报错带给用户。
fn fetch_status(client: &reqwest::blocking::Client, port: u16, timeout: Duration) -> Option<Value> {
    let response = client
        .get(format!("http://127.0.0.1:{port}/control/status"))
        .timeout(timeout)
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let status: Value = serde_json::from_slice(&response.bytes().ok()?).ok()?;
    is_service_status(&status).then_some(status)
}

/// 状态接口从第一个版本起就有字符串 `mode`;`version` 是后来才加的,不能拿它当判据。
fn is_service_status(status: &Value) -> bool {
    status.get("mode").and_then(Value::as_str).is_some()
}

#[derive(Debug, Eq, PartialEq)]
enum VersionCheck {
    Same,
    Different(String),
    /// 更早的服务不报告版本号。
    Missing,
}

fn compare_version(status: &Value, own: &str) -> VersionCheck {
    match status.get("version").and_then(Value::as_str) {
        Some(version) if version == own => VersionCheck::Same,
        Some(version) => VersionCheck::Different(version.to_string()),
        None => VersionCheck::Missing,
    }
}

/// 不替用户重启:退出服务会停掉 Science 与沙箱、打断进行中的会话,必须由用户决定。
fn mismatch_message(check: &VersionCheck, own: &str) -> String {
    let running = match check {
        VersionCheck::Different(version) => format!("{version} 版"),
        _ => "更早的版本(不报告版本号)".to_string(),
    };
    format!(
        "后台正在运行的服务是{running},不是当前应用的 {own} 版,所以现在用的仍是那个版本。\
         请在控制台点「退出服务」,再重新打开 CSSwitch。"
    )
}

// ── 登录 shell 环境 ────────────────────────────────────────────────────────

/// 登录 shell 里白名单内的变量。任何一步失败都返回原因,由调用方记日志并继续启动。
fn login_shell_env() -> Result<Vec<(String, OsString)>, String> {
    let shell = std::env::var_os("SHELL")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_SHELL.into());
    let shell_name = PathBuf::from(&shell).display().to_string();
    let marker = format!(
        "__CSSWITCH_ENV_{}_{}__",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    );
    let mut command = Command::new(&shell);
    command
        .args(["-l", "-i", "-c"])
        .arg(format!("printf \"%s\\0\" {marker}; /usr/bin/env -0"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // 独立会话:交互式 shell 没有控制终端可抢;超时时也能按进程组连它启动的东西一起结束。
    detach_session(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法执行 {shell_name}:{error}"))?;

    let deadline = Instant::now() + SHELL_ENV_TIMEOUT;
    let mut stdout = child.stdout.take().ok_or("拿不到登录 shell 的输出管道")?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.read_to_end(&mut output);
        let _ = sender.send(output);
    });
    let timed_out = || {
        format!(
            "{shell_name} 在 {} 秒内没有返回",
            SHELL_ENV_TIMEOUT.as_secs()
        )
    };
    let Ok(output) = receiver.recv_timeout(SHELL_ENV_TIMEOUT) else {
        kill_session(&mut child);
        return Err(timed_out());
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                kill_session(&mut child);
                return Err(timed_out());
            }
            Err(error) => return Err(format!("等待 {shell_name} 退出失败:{error}")),
        }
    };
    if !status.success() {
        return Err(format!("{shell_name} 以 {status} 退出"));
    }
    parse_shell_env(&output, &marker)
        .ok_or_else(|| format!("{shell_name} 的输出里没有找到环境标记"))
}

/// 从 `<rc 文件的输出><标记>\0KEY=VALUE\0KEY=VALUE\0…` 里取白名单内的变量。
///
/// 只认标记之后的内容:rc 文件可能打印横幅,其中任何一行都不能被当成变量。
/// 标记后面必须紧跟 NUL(`printf "%s\0"` 的输出),单独出现的标记文本不算。
/// 空值照样返回:shell 里显式置空与没有设置不是一回事,调用方按「有就覆盖」处理。
fn parse_shell_env(output: &[u8], marker: &str) -> Option<Vec<(String, OsString)>> {
    let mut needle = marker.as_bytes().to_vec();
    needle.push(0);
    let start = output
        .windows(needle.len())
        .position(|window| window == needle)?
        + needle.len();
    Some(
        output[start..]
            .split(|byte| *byte == 0)
            .filter_map(|entry| {
                let equals = entry.iter().position(|byte| *byte == b'=')?;
                let name = std::str::from_utf8(&entry[..equals]).ok()?;
                ENV_ALLOWLIST.contains(&name).then(|| {
                    (
                        name.to_string(),
                        OsString::from_vec(entry[equals + 1..].to_vec()),
                    )
                })
            })
            .collect(),
    )
}

// ── 进程 ───────────────────────────────────────────────────────────────────

/// 让子进程自成一个会话(也就自成一个进程组),不随启动器的终端或进程组走。
fn detach_session(command: &mut Command) {
    // SAFETY: fork 之后、exec 之前只调用 setsid,它是 async-signal-safe 的。
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// 结束 [`detach_session`] 起的子进程连同它那个进程组。
fn kill_session(child: &mut Child) {
    // SAFETY: 目标是我们自己刚起的子进程所在的进程组(pgid == 子进程 pid)。
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.wait();
}

/// 拉起服务角色:同一个可执行文件、显式 `serve`、去掉 [`APP_LAUNCH_ENV`]。
/// stdout / stderr 在 exec 之前就接到日志文件,参数解析失败、panic 这类最早期的输出也不丢。
fn spawn_service(
    port: u16,
    log_path: &Path,
    log: &File,
    env: &[(String, OsString)],
) -> io::Result<Child> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["serve", "--port", &port.to_string(), "--log-file"])
        .arg(log_path)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .env_remove(APP_LAUNCH_ENV)
        .envs(env.iter().map(|(name, value)| (name, value)));
    detach_session(&mut command);
    command.spawn()
}

enum Startup {
    Ready,
    Exited(std::process::ExitStatus),
    TimedOut,
}

fn wait_until_ready(child: &mut Child, client: &reqwest::blocking::Client, port: u16) -> Startup {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        // 先睡再查:端口被占时子进程几十毫秒内就退出,先看到它退出,
        // 就不必对着那个不回话的占用者白等一次响应超时。
        std::thread::sleep(READY_POLL);
        if let Ok(Some(status)) = child.try_wait() {
            return Startup::Exited(status);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Startup::TimedOut;
        }
        if fetch_status(client, port, remaining.min(RESPONSE_TIMEOUT)).is_some() {
            return Startup::Ready;
        }
    }
}

// ── 打开控制台与提示 ───────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum AlertKind {
    Warning,
    Critical,
}

struct Helpers {
    dir: PathBuf,
    overridden: bool,
}

impl Helpers {
    fn from_env() -> Self {
        match std::env::var_os(HELPER_DIR_ENV).filter(|value| !value.is_empty()) {
            Some(dir) => Self {
                dir: dir.into(),
                overridden: true,
            },
            None => Self {
                dir: DEFAULT_HELPER_DIR.into(),
                overridden: false,
            },
        }
    }

    fn open(&self, url: &str) -> Result<(), String> {
        let describe =
            |detail: String| format!("没能在浏览器里打开控制台({detail})。请手动访问 {url}");
        let status = Command::new(self.dir.join("open"))
            .arg(url)
            .stdin(Stdio::null())
            .status()
            .map_err(|error| describe(error.to_string()))?;
        if status.success() {
            Ok(())
        } else {
            Err(describe(format!("open 以 {status} 退出")))
        }
    }

    /// 弹一个提示框并等它关掉。标题与正文经 argv 传给脚本,不拼进 AppleScript 源码:
    /// 正文里带着日志的最后一行,内容不受我们控制。
    fn alert(&self, kind: AlertKind, title: &str, message: &str) -> Result<(), String> {
        let kind = match kind {
            AlertKind::Warning => "warning",
            AlertKind::Critical => "critical",
        };
        let status = Command::new(self.dir.join("osascript"))
            .args([
                "-e",
                "on run argv",
                "-e",
                "try",
                "-e",
                "activate",
                "-e",
                "end try",
            ])
            .arg("-e")
            .arg(format!(
                "display alert (item 1 of argv) message (item 2 of argv) as {kind} \
                 giving up after {ALERT_GIVE_UP_SECS}"
            ))
            .args(["-e", "end run", title, message])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .map_err(|error| format!("无法执行 osascript:{error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("osascript 以 {status} 退出"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MARKER: &str = "__CSSWITCH_ENV_TEST__";

    fn shell_output(before_marker: &str, entries: &[&str]) -> Vec<u8> {
        let mut output = before_marker.as_bytes().to_vec();
        output.extend_from_slice(MARKER.as_bytes());
        output.push(0);
        for entry in entries {
            output.extend_from_slice(entry.as_bytes());
            output.push(0);
        }
        output
    }

    fn value_of<'a>(env: &'a [(String, OsString)], name: &str) -> Option<&'a str> {
        env.iter()
            .find(|(key, _)| key == name)
            .and_then(|(_, value)| value.to_str())
    }

    #[test]
    fn only_allowlisted_variables_after_the_marker_are_imported() {
        // rc 文件的横幅里恰好有一行长得像变量:它在标记之前,不能被导入。
        let output = shell_output(
            "Welcome back!\nPATH=/from/the/banner\n",
            &[
                "PATH=/opt/homebrew/bin:/usr/bin",
                "https_proxy=http://user:pass@proxy.example:7890",
                "NO_PROXY=localhost,127.0.0.1",
                "CLAUDE_SCIENCE_BIN=/Users/u/bin/claude-science",
                "ANTHROPIC_API_KEY=sk-must-not-travel",
                "ANTHROPIC_BASE_URL=https://elsewhere.example",
                "HOME=/Users/someone-else",
                "CSSWITCH_APP_LAUNCH=1",
            ],
        );
        let env = parse_shell_env(&output, MARKER).expect("标记存在");
        let mut names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["CLAUDE_SCIENCE_BIN", "NO_PROXY", "PATH", "https_proxy"],
            "凭证、模型、HOME 与启动器自己的标记变量都不得进入服务环境"
        );
        assert_eq!(value_of(&env, "PATH"), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(
            value_of(&env, "https_proxy"),
            Some("http://user:pass@proxy.example:7890"),
            "值里的冒号、@ 原样保留"
        );
    }

    #[test]
    fn values_keep_their_equals_signs_newlines_and_emptiness() {
        let output = shell_output(
            "",
            &[
                "NO_PROXY=a=b\n.internal",
                "HTTP_PROXY=",
                "MALFORMED_NO_EQUALS",
            ],
        );
        let env = parse_shell_env(&output, MARKER).expect("标记存在");
        assert_eq!(
            value_of(&env, "NO_PROXY"),
            Some("a=b\n.internal"),
            "只在第一个等号处切分;条目以 NUL 分隔,值里的换行不是分隔符"
        );
        // 显式置空必须作为「有这个变量、值为空」传下去,这样才盖得住继承来的旧值;
        // 被当成「没有」丢掉的话,服务会继续用 launchd 环境里的那个代理。
        assert_eq!(value_of(&env, "HTTP_PROXY"), Some(""));
        assert_eq!(env.len(), 2);
    }

    #[test]
    fn output_without_a_proper_marker_is_a_failure_not_an_empty_environment() {
        // 没有标记:shell 没跑到我们的命令。必须报失败(留日志),不能当成「shell 里什么都没设」。
        assert!(parse_shell_env(b"PATH=/usr/bin\0HTTP_PROXY=http://p\0", MARKER).is_none());
        // 标记文本出现了但后面不是 NUL(例如 shell 把命令行回显了出来):同样不算。
        let echoed = format!("+ printf %s {MARKER}; /usr/bin/env -0\nPATH=/echoed\0");
        assert!(parse_shell_env(echoed.as_bytes(), MARKER).is_none());
    }

    #[test]
    fn a_service_from_another_version_is_never_mistaken_for_this_one() {
        let own = "0.9.0";
        assert_eq!(
            compare_version(&json!({"mode": "kimi", "version": "0.9.0"}), own),
            VersionCheck::Same
        );
        assert_eq!(
            compare_version(&json!({"mode": "kimi", "version": "0.9.1"}), own),
            VersionCheck::Different("0.9.1".into())
        );
        // 引入版本号之前的服务没有这个字段:它一定不是当前版本。
        assert_eq!(
            compare_version(&json!({"mode": "official", "port": 8788}), own),
            VersionCheck::Missing
        );
        // 状态里另有一个嵌套的 version(沙箱里 Science 的版本),不能拿它充数。
        assert_eq!(
            compare_version(
                &json!({"mode": "kimi", "sandbox": {"version": "0.9.0"}}),
                own
            ),
            VersionCheck::Missing
        );
        // 两种不一致的提示都要说清是哪个版本在跑、该怎么办。
        for check in [
            VersionCheck::Different("0.8.4".into()),
            VersionCheck::Missing,
        ] {
            let message = mismatch_message(&check, own);
            assert!(
                message.contains("0.9.0") && message.contains("退出服务"),
                "{message}"
            );
        }
        assert!(mismatch_message(&VersionCheck::Different("0.8.4".into()), own).contains("0.8.4"));
    }

    #[test]
    fn a_foreign_listener_on_the_port_is_not_a_running_service() {
        // 更早的 CSSwitch 服务:没有 version,但有 mode —— 算在跑(随后走版本提示)。
        assert!(is_service_status(
            &json!({"mode": "official", "port": 8788})
        ));
        // 别的程序恰好对这个路径回了 200 + JSON:不能据此只开控制台,那会打开别人的页面。
        assert!(!is_service_status(&json!({"status": "ok"})));
        assert!(!is_service_status(&json!({"mode": 3})));
        assert!(!is_service_status(&json!([])));
    }
}
