//! 服务日志落盘(`serve --log-file PATH`)。
//!
//! 脚本形态下日志打在终端。安装版没有终端:启动器把服务的 stdout / stderr 接到
//! `~/.csswitch/service.log`,再带着 `--log-file` 拉起服务。服务拿到这个参数后做两件事:
//!
//! - 自己再把 fd 1 / 2 接到同一个文件(追加、0600)。启动器已经接过一次,那一次是为了
//!   参数解析失败、panic 这类最早期的输出不丢;这一次让 `--log-file` 单独使用时也成立。
//! - 起一个后台线程按大小轮转。轮转必须在服务进程内做:fd 重定向之后,外部改名不会
//!   改变进程正在写的文件,只在启动时轮转挡不住长期运行。
//!
//! 不带 `--log-file` 时这个模块一行都不执行,`log_line!` 仍是原样的 `eprintln!`。

use std::fs::{File, OpenOptions, Permissions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 单个日志文件的大小上限。超过后改名为 `<文件名>.1`(覆盖上一份)并另起新文件,
/// 所以磁盘占用的上限是「上限 + 一个检查周期的写入量」再加一份历史。
const MAX_BYTES: u64 = 5 * 1024 * 1024;
const CHECK_INTERVAL: Duration = Duration::from_secs(60);
const STDIO: [RawFd; 2] = [libc::STDOUT_FILENO, libc::STDERR_FILENO];

/// 安装版的日志位置。
pub fn default_path() -> PathBuf {
    crate::profile::config_dir().join("service.log")
}

/// 以追加方式打开日志文件(不存在则连同目录一起创建),权限 0600。
/// 路径本身是符号链接时拒绝:不顺着链接去写、去改别处文件的权限。
pub fn open(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    // `mode` 只在新建时生效;已存在的文件可能是别的权限。
    file.set_permissions(Permissions::from_mode(0o600))?;
    Ok(file)
}

/// 把本进程的 stdout / stderr 接到 `path`,并开始按大小轮转。
pub fn attach(path: &Path) -> Result<(), String> {
    let describe = |error: io::Error| format!("无法使用日志文件 {}:{error}", path.display());
    let file = open(path).map_err(describe)?;
    redirect(&file, &STDIO).map_err(describe)?;
    let path = path.to_path_buf();
    std::thread::Builder::new()
        .name("log-rotate".into())
        .spawn(move || {
            let mut current = file;
            // 先查再睡:上一次运行留下的超限文件在启动时就轮转掉。
            loop {
                if let Err(error) = rotate_if_oversized(&mut current, &path, MAX_BYTES, &STDIO) {
                    crate::log_line!("日志轮转失败({}):{error}", path.display());
                }
                std::thread::sleep(CHECK_INTERVAL);
            }
        })
        .map_err(|error| format!("无法启动日志轮转线程:{error}"))?;
    Ok(())
}

fn redirect(file: &File, targets: &[RawFd]) -> io::Result<()> {
    for &target in targets {
        // SAFETY: dup2 只改本进程的文件描述符表;源 fd 由 `file` 持有,调用期间有效。
        if unsafe { libc::dup2(file.as_raw_fd(), target) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

/// 正在写的文件超过 `limit` 就轮转:当前文件改名为 `<path>.1` → 0600 新建 `path` →
/// 把 `targets` 接到新文件。返回是否发生了轮转。
///
/// 大小取自 `current` 这个句柄,不是路径:日志文件被外部删掉或挪走后,进程仍在往原来的
/// 文件里写,按路径看永远「没超限」,实际占用却一直涨。改名时路径已不存在不算错,
/// 照样另起新文件。
fn rotate_if_oversized(
    current: &mut File,
    path: &Path,
    limit: u64,
    targets: &[RawFd],
) -> io::Result<bool> {
    if current.metadata()?.len() <= limit {
        return Ok(false);
    }
    match std::fs::rename(path, rotated_path(path)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let fresh = open(path)?;
    redirect(&fresh, targets)?;
    *current = fresh;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::{FromRawFd, OwnedFd};

    fn test_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "csswitch-service-log-{tag}-{}-{}",
            std::process::id(),
            crate::utc_timestamp_ms().replace(':', "")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 代替 fd 1 / 2 的一个普通描述符:测试进程自己的 stdout / stderr 不能被接走。
    fn stand_in_for_stdio(file: &File) -> OwnedFd {
        // SAFETY: dup 返回一个新的、由本测试独占的描述符。
        let fd = unsafe { libc::dup(file.as_raw_fd()) };
        assert!(fd >= 0);
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    /// 按描述符号写,和进程里的 `eprintln!` 写 fd 2 是同一回事:
    /// 轮转接走的是这个号,不是某个 `File` 对象。
    fn write_fd(fd: &OwnedFd, text: &str) {
        // SAFETY: fd 在调用期间有效,缓冲区长度与指针来自同一个切片。
        let written = unsafe { libc::write(fd.as_raw_fd(), text.as_ptr().cast(), text.len()) };
        assert_eq!(written, text.len() as isize);
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn oversized_log_moves_aside_and_later_writes_land_in_a_fresh_0600_file() {
        let dir = test_dir("rotate");
        let path = dir.join("service.log");
        let rotated = rotated_path(&path);
        // 上一轮留下的历史:必须被覆盖,不能越攒越多。
        std::fs::write(&rotated, "older history").unwrap();
        let mut current = open(&path).unwrap();
        let stdio = stand_in_for_stdio(&current);
        write_fd(&stdio, "before rotation\n");

        // 没超限:什么都不动。
        assert!(!rotate_if_oversized(&mut current, &path, 1024, &[stdio.as_raw_fd()]).unwrap());
        assert_eq!(std::fs::read_to_string(&rotated).unwrap(), "older history");

        assert!(rotate_if_oversized(&mut current, &path, 4, &[stdio.as_raw_fd()]).unwrap());
        // 轮转之后沿用同一个描述符号继续写:内容必须落在新文件里,而不是被改了名的旧文件。
        write_fd(&stdio, "after rotation\n");

        assert_eq!(
            std::fs::read_to_string(&rotated).unwrap(),
            "before rotation\n",
            "旧内容整份挪到 .1,并覆盖更早的历史"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after rotation\n");
        assert_eq!(mode(&path), 0o600, "新建的日志文件同样是 0600");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_log_deleted_from_outside_is_still_bounded() {
        // 用户删掉了 service.log,进程还握着旧文件在写。按路径量大小永远是 0;
        // 必须按正在写的那个文件量,并在原路径另起新文件。
        let dir = test_dir("deleted");
        let path = dir.join("service.log");
        let mut current = open(&path).unwrap();
        let stdio = stand_in_for_stdio(&current);
        write_fd(&stdio, "written before the file vanished\n");
        std::fs::remove_file(&path).unwrap();

        assert!(rotate_if_oversized(&mut current, &path, 4, &[stdio.as_raw_fd()]).unwrap());
        write_fd(&stdio, "visible again\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "visible again\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn opening_tightens_a_loose_file_and_refuses_a_symlink() {
        let dir = test_dir("open");
        let path = dir.join("service.log");
        std::fs::write(&path, "existing\n").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        let mut file = open(&path).unwrap();
        file.write_all(b"appended\n").unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "existing\nappended\n",
            "已有内容不得被截断"
        );

        // 日志路径被换成指向别处的符号链接:不写过去,也不改那个文件的权限。
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "not a log\n").unwrap();
        std::fs::set_permissions(&outside, Permissions::from_mode(0o644)).unwrap();
        let link = dir.join("linked.log");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(open(&link).is_err());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "not a log\n");
        assert_eq!(mode(&outside), 0o644);
        let _ = std::fs::remove_dir_all(dir);
    }
}
