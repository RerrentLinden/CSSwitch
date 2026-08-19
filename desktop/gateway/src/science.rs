//! 官方 Claude Science daemon 控制器。
//!
//! 用户自己安装的 Science、用户自己的默认 profile(`~/.claude-science`)、
//! 用户自己的真实登录 —— CSSwitch 只在启动时注入 `ANTHROPIC_BASE_URL`
//! 把推理指向本机网关,别的一概不碰。
//!
//! 三条硬约束(经实测确认,违反会伤到用户的真实实例):
//! - 不传 `--data-dir` / `--config`:那会造出隔离实例,对话从此分家;
//! - 不传 `--no-auto-update`:官方实例必须保留自身的自动更新;
//! - 不写、不读、不伪造任何 auth 状态。

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// 启动时清空的模型类环境变量:它们会越过网关的模型目录,
/// 让 Science 直接向上游要一个我们没路由的模型。
const MODEL_ENV_KEYS_TO_CLEAR: &[&str] = &[
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

/// 以官方默认 profile 启动 daemon,仅注入 base_url。
pub fn start(proxy_base_url: &str) -> Result<(), String> {
    let bin = find_binary()?;
    let output = run(
        &bin,
        // 不带 --data-dir / --config / --no-auto-update:见模块文档的三条硬约束。
        &["serve", "--port", "0", "--detached", "--no-browser"],
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

fn extract_url(text: &str) -> Option<String> {
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
