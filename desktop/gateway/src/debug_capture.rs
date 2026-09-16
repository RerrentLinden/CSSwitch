//! 本机排障抓包。仅当环境变量 `CSSWITCH_DEBUG_CAPTURE_DIR` 指向一个目录时生效;
//! 未设置时每个入口只做一次 `OnceLock` 读取就返回。
//!
//! 每条 relay 推理请求一组文件,前缀 `<序号>-<UTC 时间>-<上游模型>`:
//! - `1-science-request.json`:Science 发给网关的原始请求体
//! - `2-upstream-request.json`:网关整形后发往上游的请求体
//! - `3-upstream-response.{sse,json}`:上游原始响应字节
//! - `4-science-response.{sse,json}`:网关整形后回给 Science 的响应字节
//! - `error.txt`:上游失败摘要
//!
//! 用途是还原真实会话形态——隔离探针复现不出来的问题(Science 的系统提示词、
//! 工具列表、历史瘦身)只能这样看到。**文件包含完整对话正文**,只应在本机临时开启,
//! 排障后删除;文件权限 0600。请求头不落盘,凭证也就不会进来。

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use serde_json::Value;

pub const CAPTURE_DIR_ENV: &str = "CSSWITCH_DEBUG_CAPTURE_DIR";

static SEQ: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// 当前线程正在抓的那一组的文件名前缀。服务按连接起线程,一个线程只处理一条请求。
    static CURRENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn capture_root() -> Option<&'static PathBuf> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let dir = PathBuf::from(std::env::var_os(CAPTURE_DIR_ENV)?);
        fs::create_dir_all(&dir).ok()?;
        crate::log_line!("排障抓包已开启:{}(文件含完整对话正文)", dir.display());
        Some(dir)
    })
    .as_ref()
}

/// 文件名里只留 ASCII 字母数字、`-` 与 `.`,其余一律换成 `_`。
fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 开始一条请求的抓包;同一线程上后续写入都归入这一组。
pub fn begin(model: &str) {
    if capture_root().is_none() {
        return;
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let prefix = format!(
        "{seq:04}-{}-{}",
        sanitize(&crate::utc_timestamp_ms()),
        sanitize(model)
    );
    CURRENT.with(|current| *current.borrow_mut() = Some(prefix));
}

fn path_for(name: &str) -> Option<PathBuf> {
    let root = capture_root()?;
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map(|prefix| root.join(format!("{prefix}-{name}")))
    })
}

fn open(path: PathBuf, append: bool) -> Option<File> {
    let mut options = OpenOptions::new();
    options.create(true);
    if append {
        options.append(true);
    } else {
        options.write(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).ok()
}

pub fn json(name: &str, value: &Value) {
    let Some(path) = path_for(name) else { return };
    if let (Ok(text), Some(mut file)) = (serde_json::to_vec_pretty(value), open(path, false)) {
        let _ = file.write_all(&text);
    }
}

pub fn write(name: &str, data: &[u8]) {
    let Some(path) = path_for(name) else { return };
    if let Some(mut file) = open(path, false) {
        let _ = file.write_all(data);
    }
}

pub fn append(name: &str, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let Some(path) = path_for(name) else { return };
    if let Some(mut file) = open(path, true) {
        let _ = file.write_all(data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_parts_cannot_escape_the_capture_dir() {
        assert_eq!(sanitize("2026-09-16T08:26:50.073Z"), "2026-09-16T08_26_50.073Z");
        assert_eq!(sanitize("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize("kimi-for-coding"), "kimi-for-coding");
    }

    #[test]
    fn writes_without_begin_are_dropped() {
        // 本线程从未 begin:即使抓包目录已配置,也不得凭空落盘。
        CURRENT.with(|current| *current.borrow_mut() = None);
        assert!(CURRENT.with(|current| current.borrow().is_none()));
        append("x.sse", b"data");
    }
}
