//! 官方 Claude Science daemon 控制器。
//!
//! 用户自己安装的 Science、用户自己的默认 profile(`~/.claude-science`)、
//! 用户自己的真实登录 —— CSSwitch 只在启动时注入:`ANTHROPIC_BASE_URL` 把推理指向
//! 本机网关;有启动覆盖项时再多带一份镜像配置(见下)。别的一概不碰,
//! 对 `~/.claude-science/` 不写任何文件。
//!
//! 硬约束(经实测确认,违反会伤到用户的真实实例或改变它的行为):
//! - 不传 `--data-dir`:那会造出隔离实例,对话从此分家;
//! - `--config` 只指向 CSSwitch 生成的镜像(`science_config.rs`:用户原配置 + 启动覆盖项),
//!   出现与否由覆盖项唯一决定:
//!
//!   ```text
//!   官方模式 · 联网搜索开   serve --port 8686 --sandbox-port 8687 --detached --no-browser
//!   官方模式 · 联网搜索关   serve --config <镜像> …      镜像写 enable_web_search = false
//!   第三方模式(Kimi / DeepSeek) serve --config <镜像> …  总是带:镜像写 default_model;
//!                                                       联网搜索关闭时再加 enable_web_search
//!   ```
//!
//!   第一行与 CSSwitch 没有这些覆盖项时逐项相同。`--config` 只换配置文件、不换数据目录
//!   (2026-10-10 用 0.1.60 隔离实测);
//! - 不传 `--no-auto-update`:官方实例必须保留自身的自动更新;
//! - 端口固定为 [`OFFICIAL_PORT`] / [`OFFICIAL_SANDBOX_PORT`],不用 `0` 随机端口:
//!   控制台地址必须稳定可收藏。不沿用 Science 默认的 8000/8001,那两个端口常被
//!   本机其它开发服务占用;代价是手动 `claude-science serve` 仍落在 8000,
//!   与经 CSSwitch 启动的地址不同;
//! - 不写、不读、不伪造任何 auth 状态。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::science_config::LaunchOverrides;

/// 启动时清空的模型类环境变量:它们会越过网关的模型目录,
/// 让 Science 直接向上游要一个我们没路由的模型。
/// 与沙箱编排(`sandbox.rs`)共享同一份清单,不复制。
pub(crate) const MODEL_ENV_KEYS_TO_CLEAR: &[&str] = &[
    "ANTHROPIC_MODEL",
    "ANTHROPIC_REASONING_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
];

/// 经 CSSwitch 启动官方实例时的监听端口。
pub const OFFICIAL_PORT: u16 = 8686;
/// 官方实例的 HTML 预览端口。显式给出而不依赖 Science 的「port+1」默认推导,
/// 这样两个端口都能在代码里一眼看到,推导规则变了也不会悄悄漂移。
pub const OFFICIAL_SANDBOX_PORT: u16 = 8687;

/// 跳过审批总开关开着时追加到 `serve` 末尾的参数(Science 官方开关:权限卡一律自动批准,
/// 直到不带它重启)。正式实例与免登录沙箱共用这一处定义。
pub(crate) const SKIP_APPROVALS_FLAG: &str = "--dangerously-skip-approvals";

const BIN_ENV: &str = "CLAUDE_SCIENCE_BIN";
const BINARY_NAME: &str = "claude-science";

pub fn find_binary() -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os(BIN_ENV).map(PathBuf::from) {
        return if explicit.is_file() {
            Ok(explicit)
        } else {
            // 显式 override 无效时 fail closed,不静默回落到别的候选。
            Err(format!("{BIN_ENV} 指向的文件不存在:{}", explicit.display()))
        };
    }
    let home = crate::profile::home();
    let mut candidates = Vec::new();
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            candidates.push(dir.join(BINARY_NAME));
        }
    }
    candidates.push(home.join(".claude-science/bin").join(BINARY_NAME));
    candidates.push(home.join(".local/bin").join(BINARY_NAME));
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            format!(
                "找不到 {BINARY_NAME}(已查找 PATH、~/.claude-science/bin、~/.local/bin);\
                 可用 {BIN_ENV} 指定绝对路径"
            )
        })
}

fn run(
    bin: &PathBuf,
    args: &[&str],
    envs: &[(&str, String)],
) -> Result<std::process::Output, String> {
    let mut command = Command::new(bin);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in MODEL_ENV_KEYS_TO_CLEAR {
        command.env_remove(key);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    command
        .output()
        .map_err(|e| format!("执行 {BINARY_NAME} 失败:{e}"))
}

/// daemon 当前状态。`status` 子命令总是退出 0,靠解析输出判断。
pub fn status() -> Value {
    let Ok(bin) = find_binary() else {
        return json!({"installed": false, "running": false});
    };
    let Ok(output) = run(&bin, &["status"], &[]) else {
        return json!({"installed": true, "running": false});
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Option<Value> = serde_json::from_str(stdout.trim()).ok();
    let running = parsed
        .as_ref()
        .and_then(|value| value.get("running").and_then(Value::as_bool))
        .unwrap_or_else(|| stdout.contains("running"));
    json!({
        "installed": true,
        "running": running,
        "detail": parsed.unwrap_or(Value::Null),
        "binary": bin.display().to_string(),
    })
}

pub fn stop() -> Result<(), String> {
    let bin = find_binary()?;
    let output = run(&bin, &["stop"], &[])?;
    if output.status.success() {
        Ok(())
    } else {
        // 未在运行时 stop 也会非零退出,这不算失败。
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        if stderr.contains("not running") || stderr.contains("no daemon") {
            Ok(())
        } else {
            Err(format!(
                "停止 Science 失败:{}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

/// 官方实例的 `serve` 参数。永不带 --data-dir / --no-auto-update;`--config` 只在
/// `mirror_config` 有值(有启动覆盖项:第三方模式,或联网搜索关闭)时出现,见模块文档的
/// 硬约束。端口固定,理由同上。跳过审批开着时末尾追加 [`SKIP_APPROVALS_FLAG`],
/// 关着时参数与没有这个开关时逐项相同。
fn serve_args(mirror_config: Option<&Path>, skip_approvals: bool) -> Vec<String> {
    let mut args = vec!["serve".to_string()];
    if let Some(config) = mirror_config {
        args.push("--config".into());
        args.push(config.display().to_string());
    }
    args.extend([
        "--port".to_string(),
        OFFICIAL_PORT.to_string(),
        "--sandbox-port".to_string(),
        OFFICIAL_SANDBOX_PORT.to_string(),
        "--detached".to_string(),
        "--no-browser".to_string(),
    ]);
    if skip_approvals {
        args.push(SKIP_APPROVALS_FLAG.to_string());
    }
    args
}

/// 以官方默认 profile 启动 daemon:注入 base_url;有启动覆盖项(第三方模式要指定默认模型;
/// 联网搜索总开关关闭)时再带上镜像配置。覆盖项由调用方给出,这里不读配置文件。
/// 镜像生成失败(例如用户的 `config.toml` 无法解析)直接报错,不启动 —— 不能让设置看起来
/// 生效而 Science 实际照旧。
pub fn start(proxy_base_url: &str, overrides: &LaunchOverrides) -> Result<(), String> {
    let bin = find_binary()?;
    let mirror_config = crate::science_config::official_launch_config(overrides)?;
    let args = serve_args(mirror_config.as_deref(), overrides.skip_approvals);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = run(
        &bin,
        &args,
        &[("ANTHROPIC_BASE_URL", proxy_base_url.to_string())],
    )?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "启动 Science 失败:{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// 取一次性登录链接。
pub fn login_url() -> Result<String, String> {
    let bin = find_binary()?;
    let output = run(&bin, &["url"], &[])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    extract_url(&stdout).ok_or_else(|| "Science 未返回登录链接".to_string())
}

/// 从 CLI 输出中截取第一个 URL。沙箱的 `url --data-dir` 入口链接复用同一实现。
pub(crate) fn extract_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|token| token.starts_with("http://") || token.starts_with("https://"))
        .map(|token| token.trim_end_matches(['.', ',']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_url_is_extracted_from_cli_noise() {
        let text = "  login link: http://localhost:53787/?nonce=abc123 \n other text";
        assert_eq!(
            extract_url(text).as_deref(),
            Some("http://localhost:53787/?nonce=abc123")
        );
        assert!(extract_url("no url here").is_none());
    }

    #[test]
    fn official_serve_args_pin_ports_and_never_isolate() {
        // 开启态:与引入联网搜索总开关之前逐项相同。这份字面量就是那时的参数向量,
        // 多一项少一项都意味着用户的官方实例被换了一种方式启动。
        let enabled = serve_args(None, false);
        assert_eq!(
            enabled,
            [
                "serve",
                "--port",
                "8686",
                "--sandbox-port",
                "8687",
                "--detached",
                "--no-browser"
            ]
        );

        // 关闭态:只多出一对 `--config <镜像>`,其余原样。
        let mirror = Path::new("/home/u/.csswitch/science-config/official.toml");
        let disabled = serve_args(Some(mirror), false);
        assert_eq!(disabled[0], "serve", "子命令必须在最前");
        let flags: Vec<usize> = disabled
            .iter()
            .enumerate()
            .filter_map(|(index, arg)| (arg == "--config").then_some(index))
            .collect();
        assert_eq!(flags.len(), 1, "--config 恰好出现一次:{disabled:?}");
        assert_eq!(
            disabled[flags[0] + 1],
            mirror.display().to_string(),
            "--config 只指向 CSSwitch 生成的镜像"
        );
        let mut rest = disabled.clone();
        rest.drain(flags[0]..=flags[0] + 1);
        assert_eq!(rest, enabled, "去掉这一对之后与开启态逐项相同");

        // 回归:这两个参数任何一个出现,都会伤到用户的真实实例。
        for args in [&enabled, &disabled] {
            for forbidden in ["--data-dir", "--no-auto-update"] {
                assert!(
                    !args.iter().any(|arg| arg == forbidden),
                    "{forbidden} 不得出现"
                );
            }
        }
    }

    #[test]
    fn launch_overrides_alone_decide_whether_the_config_flag_appears() {
        // `start` 做的就是这两步:覆盖项 → 镜像(有或没有)→ serve 参数。
        let base = std::env::temp_dir().join(format!(
            "csswitch-science-launch-test-{}-{}",
            std::process::id(),
            crate::sandbox_forge::rand_hex(4).unwrap()
        ));
        let source = base.join("science/config.toml");
        let mirror = base.join("csswitch/science-config/official.toml");
        let args_for = |web_search: bool, default_model: Option<&str>| {
            let overrides = LaunchOverrides {
                web_search,
                default_model: default_model.map(str::to_string),
                skip_approvals: false,
            };
            let config =
                crate::science_config::launch_config(&overrides, &source, &mirror).unwrap();
            serve_args(config.as_deref(), false)
        };
        let config_flags = |args: &[String]| args.iter().filter(|arg| *arg == "--config").count();

        // 第三方模式总是带镜像 —— 联网搜索开着也一样,否则 Science 的默认模型会停在
        // 清单里已经没有的官方 id 上。
        for web_search in [true, false] {
            let args = args_for(web_search, Some("claude-sonnet-19"));
            assert_eq!(config_flags(&args), 1, "--config 恰好出现一次:{args:?}");
            let flag = args.iter().position(|arg| arg == "--config").unwrap();
            assert_eq!(args[flag + 1], mirror.display().to_string());
        }
        // 官方模式 + 联网搜索开启:回到不带任何覆盖项的原始参数(先在第三方模式下启动过也一样)。
        assert_eq!(
            args_for(true, None),
            [
                "serve",
                "--port",
                "8686",
                "--sandbox-port",
                "8687",
                "--detached",
                "--no-browser"
            ]
        );
        assert!(!mirror.exists(), "切回官方模式后不留镜像");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn model_env_clear_list_covers_auth_and_role_overrides() {
        // 回归:漏掉任何一个,Science 都会绕过网关的模型目录或带着旧凭证启动。
        for key in [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
        ] {
            assert!(MODEL_ENV_KEYS_TO_CLEAR.contains(&key), "{key} 必须被清除");
        }
        assert!(
            !MODEL_ENV_KEYS_TO_CLEAR.contains(&"ANTHROPIC_BASE_URL"),
            "base_url 是我们要注入的,不能出现在清除列表里"
        );
    }
}
