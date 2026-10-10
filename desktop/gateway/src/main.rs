use std::path::PathBuf;
use std::process::ExitCode;

/// 这个二进制的几种角色。应用包里只有它一个可执行文件,由入口区分。
#[derive(Debug, Eq, PartialEq)]
enum Entry {
    /// 安装版的启动器:拉起后台服务、打开控制台,然后退出。
    Launcher,
    Version,
    /// 默认形态:单进程服务(网关 + 控制 API + WebUI),前台运行。
    Serve,
    OfficialPassthrough,
    /// 其余参数:独立网关(配置来自环境变量与参数)。
    Gateway,
}

/// 入口判定。`app_launch` 为真表示环境里有 `CSSWITCH_APP_LAUNCH=1`,它只由应用包的
/// Info.plist 设置 —— 不靠「可执行文件是否位于 .app 里」这种路径嗅探。
///
/// 这个变量只在**没有参数**时起作用:显式子命令永远优先,所以启动器拉起的
/// `serve` 即使继承到它也不会再变回启动器。
fn entry(args: &[String], app_launch: bool) -> Entry {
    match args.get(1).map(String::as_str) {
        None if app_launch => Entry::Launcher,
        None | Some("serve") => Entry::Serve,
        Some("launch") => Entry::Launcher,
        Some("--version") => Entry::Version,
        Some("official-passthrough") => Entry::OfficialPassthrough,
        Some(_) => Entry::Gateway,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let app_launch = std::env::var_os(csswitch_gateway::launcher::APP_LAUNCH_ENV)
        .is_some_and(|value| value == "1");
    match entry(&args, app_launch) {
        Entry::Launcher => csswitch_gateway::launcher::run(),
        Entry::Version => {
            println!("{}", csswitch_gateway::VERSION);
            ExitCode::SUCCESS
        }
        Entry::Serve => {
            match log_file_arg(&args) {
                Ok(Some(path)) => {
                    if let Err(e) = csswitch_gateway::service_log::attach(&path) {
                        csswitch_gateway::log_line!("csswitch: {e}");
                        return ExitCode::from(1);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    csswitch_gateway::log_line!("csswitch: {e}");
                    return ExitCode::from(2);
                }
            }
            if let Err(e) = csswitch_gateway::control::serve(port_arg(&args)) {
                csswitch_gateway::log_line!("csswitch: {e}");
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Entry::OfficialPassthrough => {
            if let Err(e) = csswitch_gateway::official_passthrough::run_cli(&args[2..]) {
                eprintln!("csswitch-gateway official-passthrough: {e}");
                return ExitCode::from(2);
            }
            ExitCode::SUCCESS
        }
        Entry::Gateway => match csswitch_gateway::config::GatewayConfig::from_env_args(args) {
            Ok(cfg) => {
                if let Err(e) = csswitch_gateway::server::serve(cfg) {
                    csswitch_gateway::log_line!("csswitch-gateway: {e}");
                    return ExitCode::from(1);
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                csswitch_gateway::log_line!("csswitch-gateway: {e}");
                ExitCode::from(2)
            }
        },
    }
}

fn port_arg(args: &[String]) -> Option<u16> {
    args.iter()
        .position(|arg| arg == "--port")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<u16>().ok())
}

/// `serve --log-file PATH`。给了开关却没给路径是调用方的错,报出来而不是当成没给:
/// 安装版的服务没有终端,悄悄退回「日志打 stderr」等于日志全丢。
fn log_file_arg(args: &[String]) -> Result<Option<PathBuf>, String> {
    let Some(index) = args.iter().position(|arg| arg == "--log-file") else {
        return Ok(None);
    };
    match args.get(index + 1) {
        Some(path) if !path.is_empty() && !path.starts_with("--") => Ok(Some(PathBuf::from(path))),
        _ => Err("--log-file 后面缺少日志文件路径".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(rest: &[&str]) -> Vec<String> {
        std::iter::once("csswitch-gateway")
            .chain(rest.iter().copied())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn bare_invocation_stays_a_foreground_service_unless_the_app_bundle_says_otherwise() {
        // `./csswitch` 与 loopback 都靠「不带参数 = 前台 serve」。
        assert_eq!(entry(&args(&[]), false), Entry::Serve);
        assert_eq!(entry(&args(&["serve", "--port", "1"]), false), Entry::Serve);
        // 只有应用包(LSEnvironment)能把无参数启动变成启动器。
        assert_eq!(entry(&args(&[]), true), Entry::Launcher);
        assert_eq!(entry(&args(&["launch"]), false), Entry::Launcher);
    }

    #[test]
    fn an_explicit_subcommand_always_wins_over_the_app_launch_variable() {
        // 启动器拉起的服务若继承到这个变量又被判成启动器,就会一层层无限拉起自己。
        assert_eq!(entry(&args(&["serve", "--port", "1"]), true), Entry::Serve);
        assert_eq!(entry(&args(&["--version"]), true), Entry::Version);
        assert_eq!(
            entry(&args(&["official-passthrough", "--port", "1"]), true),
            Entry::OfficialPassthrough
        );
        // 其余参数仍然交给独立网关,不被新分支吞掉。
        assert_eq!(
            entry(&args(&["--listen", "127.0.0.1:1"]), true),
            Entry::Gateway
        );
        assert_eq!(
            entry(&args(&["--listen", "127.0.0.1:1"]), false),
            Entry::Gateway
        );
    }

    #[test]
    fn log_file_flag_without_a_path_is_an_error_not_a_silent_default() {
        assert_eq!(log_file_arg(&args(&["serve", "--port", "1"])), Ok(None));
        assert_eq!(
            log_file_arg(&args(&["serve", "--log-file", "/tmp/s.log", "--port", "1"])),
            Ok(Some(PathBuf::from("/tmp/s.log")))
        );
        assert!(log_file_arg(&args(&["serve", "--log-file"])).is_err());
        assert!(log_file_arg(&args(&["serve", "--log-file", "--port", "1"])).is_err());
    }
}
