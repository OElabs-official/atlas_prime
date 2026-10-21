//! network/speed/devsrc — `/proc/net/dev` 正文取数源链：直读 → adb 桥。
//!
//! 真机（Android 16 / Termux untrusted_app）实测：非特权进程直读
//! `/proc/net/dev` 被内核拒（EACCES/EPERM，sysfs/netlink 同拒），但 loopback
//! adb 可达——`adb -s localhost:<port> shell cat /proc/net/dev`（uid 2000 可
//! 读）实测 ~46ms 回全文。端口由外部看门狗维护在数据目录 `adb_port` 文件
//! （单行十进制）；「无线配对」任务换端口后无需重启本进程，故**每拍现读、不做缓存**
//! （一次单行小文件读，成本可忽略，换来的是一致性：永不见旧端口）。
//!
//! 链序：直读 →（PermissionDenied / NotFound 时）adb 桥；其余 IO 错误 adb
//! 帮不上，直接 [`NetError::Denied`] 上报。adb 腿失败后的 sysfs / netlink
//! 兜底不住本文件——在 [`super`]（speed）的快照编排里。
//!
//! 子进程回收纪律沿用 [`crate::v1::notify`] 的 spawn + `try_wait` 轮询 idiom
//! （notify 的 dispatch 三 stdio 皆 null、错误类型也不同，不可直接复用，
//! 此处本地等价复刻）：[`ADB_BOUND`] 内未退出即 kill + wait 防僵尸——
//! 看门狗重启 adb 的瞬间，1Hz 测速回路至多被拖 1.5s。

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use directories::ProjectDirs;

use crate::v1::PROJECT;

/// adb 子进程等待上限：成功路径实测 ~46ms，留 1.5s 给瞬断重连；越界即 kill。
/// 1Hz 回路的最坏拖长由它钳位。
const ADB_BOUND: Duration = Duration::from_millis(1500);

/// `try_wait` 轮询步长（notify 模块同款节奏）
const POLL_STEP: Duration = Duration::from_millis(20);

/// 源链整链失败的四分成因（无 thiserror，手写 Display 随仓库风格）
#[derive(Debug)]
pub enum NetError {
    /// 直读 `/proc/net/dev` 遭非权限类 IO 错误（adb 桥对此无能，不尝试）
    Denied(std::io::Error),
    /// 直读被拒且端口文件缺失/非法——adb 腿整体跳过（本机未搭桥/看门狗未起）
    NoPort,
    /// adb 子进程拉起失败或非零退出（携成因细节）
    AdbFailed(String),
    /// adb 子进程 [`ADB_BOUND`] 内未退出，已 kill 回收
    Timeout,
}

impl core::fmt::Display for NetError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NetError::Denied(e) => write!(f, "network: /proc/net/dev 直读失败: {e}"),
            NetError::NoPort => write!(
                f,
                "network: 直读被拒且 adb_port 端口文件缺失/非法，adb 桥腿跳过"
            ),
            NetError::AdbFailed(d) => write!(f, "network: adb 桥读取失败: {d}"),
            NetError::Timeout => write!(f, "network: adb 桥超 {ADB_BOUND:?} 未退出，已 kill"),
        }
    }
}
impl std::error::Error for NetError {}

/// 【公开入口】`/proc/net/dev` 全文取数：直读 → adb 桥两级源链。
/// 返回正文与真文件逐字节同构——下游 [`super::parse_dev_stats`] 不感知源。
pub fn read_dev_snapshot() -> Result<String, NetError> {
    read_dev_chain(
        || std::fs::read_to_string("/proc/net/dev"),
        adb_port,
        adb_leg,
    )
}

/// 源链编排（三腿注入，单测换假件走全部分支）：直读成功即回；权限/缺档类
/// 错误（EACCES/EPERM → PermissionDenied、ENOENT → NotFound，std 映射齐全）
/// 转 adb 腿，端口不可得回 [`NetError::NoPort`]；其余 IO 错误回 [`NetError::Denied`]。
fn read_dev_chain(
    direct: impl FnOnce() -> Result<String, std::io::Error>,
    port: impl FnOnce() -> Option<u16>,
    adb: impl FnOnce(u16) -> Result<String, NetError>,
) -> Result<String, NetError> {
    match direct() {
        Ok(text) => Ok(text),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) =>
        {
            match port() {
                Some(p) => adb(p),
                None => Err(NetError::NoPort),
            }
        }
        Err(e) => Err(NetError::Denied(e)),
    }
}

/// 端口文件（每拍现读，「无线配对」任务换端口即时生效）：数据目录 `adb_port`，
/// 与 db / 任务配置同源（directories + [`PROJECT`] 三元组）。
fn adb_port() -> Option<u16> {
    let path = ProjectDirs::from(PROJECT[0], PROJECT[1], PROJECT[2])
        .map(|p| p.data_dir().join("adb_port"))?;
    port_from_file(&path)
}

/// 端口文件解析（路径注入供单测）：trim 后整行须为合法非零端口号，
/// 缺失 / 非数 / 0 一律视为「无端口」。
fn port_from_file(path: &Path) -> Option<u16> {
    let p: u16 = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
    (p > 0).then_some(p)
}

/// adb 腿 = 计划 + 取回
fn adb_leg(port: u16) -> Result<String, NetError> {
    let (prog, args) = adb_plan(port);
    adb_fetch(&prog, &args, ADB_BOUND)
}

/// 纯计划器（单一事实源，单测钉死）：
/// `adb -s localhost:<port> shell cat /proc/net/dev`。环境原样透传（绝不
/// env_clear）：adb 在普通 Termux env 下即可工作。
fn adb_plan(port: u16) -> (String, Vec<String>) {
    (
        "adb".to_string(),
        vec![
            "-s".to_string(),
            format!("localhost:{port}"),
            "shell".to_string(),
            "cat".to_string(),
            "/proc/net/dev".to_string(),
        ],
    )
}

/// adb 子进程取正文：spawn（stdout 管道，另两侧 null）→ `try_wait` 轮询至
/// `bound` 越界 kill + wait。正文只有几 KB（远小于管道缓冲），故进程退出后
/// 一次性读到 EOF 即可，无需并发泵送。
fn adb_fetch(prog: &str, args: &[String], bound: Duration) -> Result<String, NetError> {
    let mut child = match Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Err(NetError::AdbFailed(format!(
                "进程拉起失败（adb 不在 PATH？）: {e}"
            )));
        }
    };
    let mut pipe = child.stdout.take(); // piped() ⇒ Some
    let deadline = Instant::now() + bound;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Err(NetError::AdbFailed(format!("非零退出 {:?}", status.code())));
                }
                let Some(pipe) = pipe.as_mut() else {
                    return Err(NetError::AdbFailed("stdout 管道句柄缺失（内部）".to_string()));
                };
                let mut buf = Vec::new();
                pipe.read_to_end(&mut buf)
                    .map_err(|e| NetError::AdbFailed(format!("管道读取失败: {e}")))?;
                // /proc 正文本质 ASCII；极端脏字节走 lossy，解析器按行列自会消化
                return Ok(String::from_utf8_lossy(&buf).into_owned());
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(NetError::AdbFailed(format!("try_wait 失败: {e}")));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait(); // 回收防僵尸；kill 后 wait 必快速返回
            return Err(NetError::Timeout);
        }
        std::thread::sleep(POLL_STEP);
    }
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::{NetError, adb_fetch, adb_plan, port_from_file, read_dev_chain, ADB_BOUND};
    use std::cell::Cell;
    use std::io::{Error, ErrorKind};
    use std::time::{Duration, Instant};

    /// 真机形状 fixture（正文子集，链路透传验证用）
    const TEXT: &str = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n wlan0: 987654321  456789    0    0    0     0          0         0 123456789   345678    0    0    0     0       0          0\n";

    #[test]
    fn chain_returns_direct_text_without_touching_adb_legs() {
        let port_hit = Cell::new(false);
        let adb_hit = Cell::new(false);
        let got = read_dev_chain(
            || Ok(TEXT.to_string()),
            || {
                port_hit.set(true);
                Some(33741)
            },
            |_p| {
                adb_hit.set(true);
                Err(NetError::Timeout)
            },
        );
        assert_eq!(got.ok().as_deref(), Some(TEXT));
        assert!(
            !port_hit.get() && !adb_hit.get(),
            "直读成功即出口，端口/adb 两腿不可被触碰"
        );
    }

    #[test]
    fn chain_falls_to_adb_with_port_and_returns_bridge_text() {
        let handed = Cell::new(0u16);
        let got = read_dev_chain(
            || Err(Error::from_raw_os_error(libc::EACCES)),
            || Some(33741),
            |p| {
                handed.set(p);
                Ok(TEXT.to_string())
            },
        );
        assert_eq!(got.ok().as_deref(), Some(TEXT), "adb 腿正文须原样透传");
        assert_eq!(handed.get(), 33741, "端口须递到 adb 腿");
    }

    #[test]
    fn chain_no_port_file_skips_adb_and_reports_cleanly() {
        let adb_hit = Cell::new(false);
        let got = read_dev_chain(
            || Err(Error::from_raw_os_error(libc::EPERM)), // std 亦归 PermissionDenied
            || None,
            |_p| {
                adb_hit.set(true);
                Ok(TEXT.to_string())
            },
        );
        assert!(
            matches!(got, Err(NetError::NoPort)),
            "缺端口文件须干净报 NoPort 且不拉 adb，实为 {got:?}"
        );
        assert!(!adb_hit.get(), "无端口 = adb 腿整体跳过");
    }

    #[test]
    fn chain_non_permission_io_error_reports_denied() {
        let got = read_dev_chain(
            || Err(Error::new(ErrorKind::InvalidData, "脏读")),
            || Some(33741),
            |_p| Ok(TEXT.to_string()),
        );
        assert!(matches!(got, Err(NetError::Denied(_))), "非权限错直报 Denied，实为 {got:?}");
    }

    #[test]
    fn adb_plan_pins_program_and_exact_arg_order() {
        let (prog, args) = adb_plan(33741);
        assert_eq!(prog, "adb");
        assert_eq!(
            args,
            vec!["-s", "localhost:33741", "shell", "cat", "/proc/net/dev"]
        );
    }

    #[test]
    fn adb_fetch_passes_through_child_stdout() {
        let got = adb_fetch(
            "sh",
            &["-c".to_string(), "printf hello".to_string()],
            ADB_BOUND,
        )
        .expect("子进程 stdout 应原样取回");
        assert_eq!(got, "hello");
    }

    /// 慢假子进程（sleep 5 ≫ 300ms 上限）：验证超时钳 + kill 回收路径
    #[test]
    fn adb_fetch_kills_overrun_child_and_reports_timeout() {
        let t0 = Instant::now();
        let got = adb_fetch(
            "sh",
            &["-c".to_string(), "sleep 5".to_string()],
            Duration::from_millis(300),
        );
        assert!(
            matches!(got, Err(NetError::Timeout)),
            "越界子进程须钳 Timeout，实为 {got:?}"
        );
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "kill 未生效/等待失控: {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn port_file_parses_single_line_and_rejects_junk() {
        // 跨进程隔离的唯一临时目录（并发 checkout 跑测试互不踩踏）
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "atlas_devsrc_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("临时目录");
        let f = dir.join("adb_port");
        std::fs::write(&f, "33741\n").expect("写入端口");
        assert_eq!(port_from_file(&f), Some(33741), "单行 + 尾换行 = 合法");
        std::fs::write(&f, "not-a-port").expect("写入垃圾");
        assert_eq!(port_from_file(&f), None, "非数须拒");
        std::fs::write(&f, "0").expect("写入零端口");
        assert_eq!(port_from_file(&f), None, "端口 0 须拒");
        std::fs::remove_file(&f).expect("删文件");
        assert_eq!(port_from_file(&f), None, "缺失文件 = 无端口（不 panic）");
        let _ = std::fs::remove_dir(&dir);
    }
}
