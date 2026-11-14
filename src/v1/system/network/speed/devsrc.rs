//! network/speed/devsrc — `/proc/net/dev` 正文取数源链：直读 → su root 桥。
//!
//! 真机（Android 16 / Termux untrusted_app）实测：非特权进程直读
//! `/proc/net/dev` 被内核拒（EACCES/EPERM，sysfs/netlink 同拒）。旧无线调试
//! 桥已整体撤除——调试端口随网络切换/重启漂移、须人工重配对，UX 差；root
//! 模式（Magisk su）无人值守即可用。su 计划固定 `su -c "cat /proc/net/dev"`，
//! 每拍同步走、无端口文件（su 经 PATH 解析，Termux+Magisk 实测可用）。
//! 无 root（封印）设备 su 拉起 ENOENT 秒失败回 [`NetError::NoSu`]；此类设备
//! 不再每拍白付 fork+execve——连续 [`NOSU_BACKOFF_AFTER`] 拍 NoSu 终态后 su
//! 腿进入 [`NOSU_BACKOFF_SECS`] 秒回退窗（窗内直接回 NoSu 同款终态、不改回退
//! 状态，对调用侧连败计数是非事件；sysfs/netlink 兜底腿照旧每拍照跑），首个
//! 成功即清零回 eager。Magisk 首授权可能弹框拖超时一拍，属预期。
//!
//! 链序：直读 →（PermissionDenied / NotFound 时）su 腿；其余 IO 错误 su
//! 帮不上，直接 [`NetError::Denied`] 上报。su 腿失败后的 sysfs / netlink
//! 兜底不住本文件——在 [`super`]（speed）的快照编排里。
//!
//! 子进程回收纪律沿用 [`crate::v1::notify`] 的 spawn + `try_wait` 轮询 idiom
//! （notify 的 dispatch 三 stdio 皆 null、错误类型也不同，不可直接复用，
//! 此处本地等价复刻）：[`SU_BOUND`] 内未退出即 kill + wait 防僵尸——
//! Magisk 守护冷启/授权弹框的瞬间，1Hz 测速回路至多被拖 1.5s。

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// su 子进程等待上限：成功路径为百毫秒级（su 冷启动 + 一次 cat），留 1.5s 给
/// Magisk 守护冷启；越界即 kill。1Hz 回路的最坏拖长由它钳位。
const SU_BOUND: Duration = Duration::from_millis(1500);

/// NoSu 回退触发阈值：连续多少拍 NoSu 终态后 su 腿进入回退窗
const NOSU_BACKOFF_AFTER: u32 = 3;

/// NoSu 回退窗长（秒）：窗内 su 腿一律不碰进程、直接回 NoSu 终态
const NOSU_BACKOFF_SECS: i64 = 60;

/// 连续 NoSu 连击计数（仅 su 腿真实尝试且回 NoSu 才进位；任何非 NoSu 结局
/// ——成功或 SuFailed/Timeout——清零）。封印设备上封顶于秒级 u32，无溢出忧虑。
static NOSU_STREAK: AtomicU32 = AtomicU32::new(0);

/// 下一次允许 su 尝试的 Unix 秒级 epoch；0 = eager（无回退窗生效）
static NOSU_RETRY_AT: AtomicI64 = AtomicI64::new(0);


/// `try_wait` 轮询步长（notify 模块同款节奏）
const POLL_STEP: Duration = Duration::from_millis(20);

/// 源链整链失败的四分成因（无 thiserror，手写 Display 随仓库风格）
#[derive(Debug)]
pub enum NetError {
    /// 直读 `/proc/net/dev` 遭非权限类 IO 错误（su 桥对此无能，不尝试）
    Denied(std::io::Error),
    /// su 拉起即 ENOENT——设备未 root（或 su 不在 PATH），root 模式不可用，
    /// 干净终态
    NoSu,
    /// su 子进程拉起失败（非 ENOENT）或非零退出（携成因细节）——
    /// Magisk 拒绝授权即以此形态呈现，仍是干净成因、不是崩溃
    SuFailed(String),
    /// su 子进程 [`SU_BOUND`] 内未退出，已 kill 回收
    Timeout,
}

impl core::fmt::Display for NetError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NetError::Denied(e) => write!(f, "network: /proc/net/dev 直读失败: {e}"),
            NetError::NoSu => write!(f, "network: 设备无 su 二进制，root 模式不可用"),
            NetError::SuFailed(d) => write!(f, "network: su 桥读取失败: {d}"),
            NetError::Timeout => write!(f, "network: su 桥超 {SU_BOUND:?} 未退出，已 kill"),
        }
    }
}
impl std::error::Error for NetError {}

/// 【公开入口】`/proc/net/dev` 全文取数：直读 → su 桥两级源链。
/// 返回正文与真文件逐字节同构——下游 [`super::parse_dev_stats`] 不感知源。
pub fn read_dev_snapshot() -> Result<String, NetError> {
    read_dev_chain(|| std::fs::read_to_string("/proc/net/dev"), su_leg)
}

/// 源链编排（两腿注入，单测换假件走全部分支）：直读成功即回；权限/缺档类
/// 错误（EACCES/EPERM → PermissionDenied、ENOENT → NotFound，std 映射齐全）
/// 直转 su 腿；其余 IO 错误回 [`NetError::Denied`]。
fn read_dev_chain(
    direct: impl FnOnce() -> Result<String, std::io::Error>,
    su: impl FnOnce() -> Result<String, NetError>,
) -> Result<String, NetError> {
    match direct() {
        Ok(text) => Ok(text),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) =>
        {
            su()
        }
        Err(e) => Err(NetError::Denied(e)),
    }
}

/// su 腿 = 回退门 + 计划 + 取回
fn su_leg() -> Result<String, NetError> {
    su_gated(epoch_secs(), || {
        let (prog, args) = su_plan();
        su_fetch(&prog, &args, SU_BOUND)
    })
}

/// NoSu 回退门（时钟注入缝，单测喂假 epoch 免真睡 60s）：回退窗内不调
/// `attempt`、直接回 [`NetError::NoSu`] 同款终态且**不改任何回退状态**——
/// 对上游（speed::snapshot_result → 操作器 net 回路连败计数）是非事件，
/// 全链耗尽的 Err 形状与未回退时逐字节一致。窗外真实尝试后按结局记账：
/// NoSu 进位、达阈值武装 [`NOSU_BACKOFF_SECS`] 回退窗；成功清零回 eager；
/// SuFailed/Timeout 说明 su 存在（拒权/冷启），与「设备无 su」成因不同，
/// 同样清零保持 eager（连击语义 = 连续 NoSu 才被中断）。
fn su_gated(now: i64, attempt: impl FnOnce() -> Result<String, NetError>) -> Result<String, NetError> {
    if !su_attempt_allowed(now) {
        return Err(NetError::NoSu);
    }
    match attempt() {
        Ok(text) => {
            NOSU_STREAK.store(0, Ordering::Relaxed);
            NOSU_RETRY_AT.store(0, Ordering::Relaxed);
            Ok(text)
        }
        Err(NetError::NoSu) => {
            let streak = NOSU_STREAK.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            if streak >= NOSU_BACKOFF_AFTER {
                NOSU_RETRY_AT.store(now.saturating_add(NOSU_BACKOFF_SECS), Ordering::Relaxed);
            }
            Err(NetError::NoSu)
        }
        Err(other) => {
            NOSU_STREAK.store(0, Ordering::Relaxed);
            NOSU_RETRY_AT.store(0, Ordering::Relaxed);
            Err(other)
        }
    }
}

/// 回退窗判据（公开测试缝）：无窗（0）或窗已到期即放行。到期不自动清零
/// ——下一次真实尝试的结局负责重武装（NoSu）或解除（成功/其他），
/// 故过期状态不会把 su 腿锁死在窗外（stale_state 对抗点）。
fn su_attempt_allowed(now: i64) -> bool {
    let retry_at = NOSU_RETRY_AT.load(Ordering::Relaxed);
    retry_at == 0 || now >= retry_at
}

/// Unix 秒级 epoch（回退窗时钟）；1970 前脏钟回 0，判据自洽（0 < 任何窗）。
fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

/// 测试专用：回退状态清零（模块级原子被同文件多测试共享，测试须先复位）
#[cfg(test)]
pub fn reset_for_test() {
    NOSU_STREAK.store(0, Ordering::Relaxed);
    NOSU_RETRY_AT.store(0, Ordering::Relaxed);
}

/// 纯计划器（单一事实源，单测钉死）：`su -c "cat /proc/net/dev"`。环境原样
/// 透传（绝不 env_clear）：su 经 PATH 解析，Termux+Magisk 实测可用。
fn su_plan() -> (String, Vec<String>) {
    (
        "su".to_string(),
        vec!["-c".to_string(), "cat /proc/net/dev".to_string()],
    )
}

/// su 子进程取正文：spawn（stdout 管道，另两侧 null）→ `try_wait` 轮询至
/// `bound` 越界 kill + wait。正文只有几 KB（远小于管道缓冲），故进程退出后
/// 一次性读到 EOF 即可，无需并发泵送。
fn su_fetch(prog: &str, args: &[String], bound: Duration) -> Result<String, NetError> {
    let mut child = match Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        // ENOENT = 设备未装 su（未 root），干净终态；其余拉起失败为运行期故障
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(NetError::NoSu),
        Err(e) => return Err(NetError::SuFailed(format!("进程拉起失败: {e}"))),
    };
    let mut pipe = child.stdout.take(); // piped() ⇒ Some
    let deadline = Instant::now() + bound;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    // Magisk 拒绝授权即以此形态呈现（非零退出）
                    return Err(NetError::SuFailed(format!("非零退出 {:?}", status.code())));
                }
                let Some(pipe) = pipe.as_mut() else {
                    return Err(NetError::SuFailed("stdout 管道句柄缺失（内部）".to_string()));
                };
                let mut buf = Vec::new();
                pipe.read_to_end(&mut buf)
                    .map_err(|e| NetError::SuFailed(format!("管道读取失败: {e}")))?;
                // /proc 正文本质 ASCII；极端脏字节走 lossy，解析器按行列自会消化
                return Ok(String::from_utf8_lossy(&buf).into_owned());
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(NetError::SuFailed(format!("try_wait 失败: {e}")));
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
    use super::{
        NetError, NOSU_BACKOFF_SECS, read_dev_chain, reset_for_test, su_attempt_allowed,
        su_fetch, su_gated, su_plan, SU_BOUND,
    };
    use std::cell::Cell;
    use std::io::{Error, ErrorKind};
    use std::time::{Duration, Instant};

    /// 真机形状 fixture（正文子集，链路透传验证用）
    const TEXT: &str = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n wlan0: 987654321  456789    0    0    0     0          0         0 123456789   345678    0    0    0     0       0          0\n";

    #[test]
    fn chain_returns_direct_text_without_touching_su_leg() {
        let su_hit = Cell::new(false);
        let got = read_dev_chain(
            || Ok(TEXT.to_string()),
            || {
                su_hit.set(true);
                Err(NetError::Timeout)
            },
        );
        assert_eq!(got.ok().as_deref(), Some(TEXT));
        assert!(!su_hit.get(), "直读成功即出口，su 腿不可被触碰");
    }

    #[test]
    fn chain_falls_to_su_leg_on_permission_denied_and_returns_bridge_text() {
        let su_hit = Cell::new(false);
        let got = read_dev_chain(
            || Err(Error::from_raw_os_error(libc::EACCES)), // EPERM 亦归 PermissionDenied
            || {
                su_hit.set(true);
                Ok(TEXT.to_string())
            },
        );
        assert_eq!(got.ok().as_deref(), Some(TEXT), "su 腿正文须原样透传");
        assert!(su_hit.get(), "直读被拒须转 su 腿");
    }

    #[test]
    fn chain_su_leg_no_su_propagates_cleanly() {
        let got = read_dev_chain(
            || Err(Error::from_raw_os_error(libc::ENOENT)),
            || Err(NetError::NoSu),
        );
        assert!(
            matches!(got, Err(NetError::NoSu)),
            "无 root 设备（su 拉起 ENOENT）须干净报 NoSu，实为 {got:?}"
        );
    }

    #[test]
    fn chain_non_permission_io_error_reports_denied() {
        let got = read_dev_chain(
            || Err(Error::new(ErrorKind::InvalidData, "脏读")),
            || Ok(TEXT.to_string()),
        );
        assert!(matches!(got, Err(NetError::Denied(_))), "非权限错直报 Denied，实为 {got:?}");
    }

    #[test]
    fn su_plan_pins_program_and_exact_arg_order() {
        let (prog, args) = su_plan();
        assert_eq!(prog, "su");
        assert_eq!(args, vec!["-c", "cat /proc/net/dev"]);
    }

    #[test]
    fn su_fetch_missing_binary_reports_no_su() {
        let got = su_fetch("atlas-no-such-su-zzz", &[], SU_BOUND);
        assert!(
            matches!(got, Err(NetError::NoSu)),
            "spawn ENOENT 须归 NoSu（未 root 终态），实为 {got:?}"
        );
    }

    #[test]
    fn su_fetch_passes_through_child_stdout() {
        let got = su_fetch(
            "sh",
            &["-c".to_string(), "printf hello".to_string()],
            SU_BOUND,
        )
        .expect("子进程 stdout 应原样取回");
        assert_eq!(got, "hello");
    }

    /// 慢假子进程（sleep 5 ≫ 300ms 上限）：验证超时钳 + kill 回收路径
    #[test]
    fn su_fetch_kills_overrun_child_and_reports_timeout() {
        let t0 = Instant::now();
        let got = su_fetch(
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

    // ---------------- NoSu 回退门（task 14） ----------------
    //
    // 时钟缝：su_gated 吃注入的 epoch 秒，测试全程零 sleep；尝试缝：假 attempt
    // 计数器 = execve 代理（被跳过即不 +1）。模块级原子被并发测试共享，
    // 全部回退测试经 BACKOFF_LOCK 串行。

    static BACKOFF_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 假 Unix 秒基准（远离 0，避开「无窗 = 0」哨兵语义）
    const T0: i64 = 1_700_000_000;

    fn no_su_attempt(hits: &Cell<u32>) -> impl FnOnce() -> Result<String, NetError> + '_ {
        move || {
            hits.set(hits.get() + 1);
            Err(NetError::NoSu)
        }
    }

    /// 连续 3 拍 NoSu 武装回退后，60s 窗内后续拍（第 4/5/6 拍）不再触碰 su
    /// 进程、原样回 NoSu 终态（封装机每分钟 execve 归零的根据）
    #[test]
    fn no_su_backoff_suppresses_su_leg_within_window_after_three_streak() {
        let _g = BACKOFF_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        let hits = Cell::new(0u32);
        for _ in 0..3 {
            assert!(matches!(su_gated(T0, no_su_attempt(&hits)), Err(NetError::NoSu)));
        }
        assert_eq!(hits.get(), 3, "武装修士前每拍都须真实尝试 su");
        for k in 1..=3 {
            assert!(
                matches!(su_gated(T0 + k, no_su_attempt(&hits)), Err(NetError::NoSu)),
                "回退拍 {k} 须原样回 NoSu 终态（调用侧全链耗尽形状不变）"
            );
        }
        assert_eq!(hits.get(), 3, "回退窗内 su 进程不得被拉起（execve 计数须冻结）");
    }

    /// stale_state 对抗：回退窗到期即重新放行尝试（无新成功也不许把 su 腿
    /// 锁死）；重探再 NoSu 即从新时刻重新武装 60s
    #[test]
    fn no_su_backoff_expires_at_window_end_and_rearms_on_next_no_su() {
        let _g = BACKOFF_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        let hits = Cell::new(0u32);
        for _ in 0..3 {
            let _ = su_gated(T0, no_su_attempt(&hits));
        }
        assert!(!su_attempt_allowed(T0 + NOSU_BACKOFF_SECS - 1), "窗内最后一秒仍须回退");
        let _ = su_gated(T0 + NOSU_BACKOFF_SECS, no_su_attempt(&hits));
        assert_eq!(hits.get(), 4, "窗到期那一拍须重新真实尝试");
        let _ = su_gated(T0 + 2 * NOSU_BACKOFF_SECS - 1, no_su_attempt(&hits));
        assert_eq!(hits.get(), 4, "重探 NoSu 须从 T0+60 再武装 60s");
        let _ = su_gated(T0 + 2 * NOSU_BACKOFF_SECS, no_su_attempt(&hits));
        assert_eq!(hits.get(), 5, "第二个窗到期亦须放行（回退不许过期锁死）");
    }

    /// 首个成功清零：回退窗边界重探成功 → 状态归零，原窗内时刻恢复 eager
    #[test]
    fn no_su_backoff_first_success_resets_to_eager() {
        let _g = BACKOFF_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        let hits = Cell::new(0u32);
        for _ in 0..3 {
            let _ = su_gated(T0, no_su_attempt(&hits));
        }
        assert!(!su_attempt_allowed(T0 + 1), "前置：回退窗须已武装");
        let got = su_gated(T0 + NOSU_BACKOFF_SECS, || Ok(TEXT.to_string()));
        assert_eq!(got.ok().as_deref(), Some(TEXT), "窗边界重探成功须透传正文");
        assert!(su_attempt_allowed(T0 + 1), "成功须解除回退窗回 eager（即使时刻在旧窗内）");
        let _ = su_gated(T0 + NOSU_BACKOFF_SECS + 1, no_su_attempt(&hits));
        assert_eq!(hits.get(), 4, "成功后的下一拍须真实尝试");
    }

    /// 连击语义：SuFailed 中断 NoSu 连击（清零）；成功同时清零计数与窗
    #[test]
    fn non_no_su_terminal_breaks_streak_and_clears_backoff() {
        let _g = BACKOFF_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        let hits = Cell::new(0u32);
        let _ = su_gated(T0, no_su_attempt(&hits));
        let _ = su_gated(T0, no_su_attempt(&hits));
        let _ = su_gated(T0, || Err(NetError::SuFailed("拒权".to_string())));
        let _ = su_gated(T0, no_su_attempt(&hits));
        let _ = su_gated(T0, no_su_attempt(&hits));
        assert!(su_attempt_allowed(T0), "SuFailed 已断连击，5 拍后仅 2 连击不得武装");
        // 第 3 连击武装后，窗边界成功 → 双清零
        let _ = su_gated(T0, no_su_attempt(&hits));
        assert!(!su_attempt_allowed(T0 + 1), "第 3 连击须武装回退窗");
        let got = su_gated(T0 + NOSU_BACKOFF_SECS, || Ok(TEXT.to_string()));
        assert!(got.is_ok());
        assert!(su_attempt_allowed(T0), "成功须同时清零连击与窗");
    }
}
