//! v1/notify: 被动出站模块（`termux-notification` / `termux-vibrate` CLI 薄桥）。
//!
//! 本模块只做一件事：把出站动作（通知三元组 / 震动时长）序列化成一次
//! Termux API CLI 调用，发完即忘、**零状态**——不决定何时发、不记忆发过什么
//! （边沿/滞回等全部策略住在调用方，现即 [`crate::v1::operator`]）。抽掉本模块，
//! 进程只是失去出站动作，数据流照常——与操作器“唯一自主驱动”互为对侧：纯出站。
//!
//! # 公开接口用法
//! ```ignore
//! use crate::v1::notify::{notify, vibrate};
//! notify("atlas-mem", "ATLAS · 内存不足警告", "可用内存不足 2G：当前仅剩 1024 MB").ok();
//! vibrate(5000).ok(); // 告警伴震 5 秒
//! ```
//!
//! # 契约
//! - 程序名与参数序由纯函数 [`plan`] / [`plan_vibrate`] 生成（单一事实源，单测钉死）；
//! - 固定 `--id`：同 id 二次发送 = 替换旧通知，不堆叠（termux-notification 原生语义）；
//! - 失败三分：拉起失败（termux-api 缺失）/ 非零退出 / 各自动作超时杀进程
//!   （通知 [`SEND_BOUND`]、震动 [`VIBRATE_BOUND`]）→ [`NotifyError`] 对应变体；
//!   任何路径不 panic，进程回收（防僵尸）必达。

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 通知子进程等待上限：轮询 `try_wait` 越界即 kill（通知是尽力而为，绝不拖住采样轮）
const SEND_BOUND: Duration = Duration::from_secs(2);

/// 震动子进程等待上限。实测（本机，2026-10）：`termux-vibrate -d 5000` 阻塞
/// **2.43s**、退出码 0——CLI 在广播移交 Termux:API app 后即返回，5s 震动在
/// 移交后异步进行、不受 CLI 退出截断；但仅移交一项已远超 [`SEND_BOUND`]，
/// 且移交耗时随 app 冷启动浮动，故震动单独放宽到 8s，绝不在移交途中误杀。
const VIBRATE_BOUND: Duration = Duration::from_secs(8);

/// `try_wait` 轮询步长
const POLL_STEP: Duration = Duration::from_millis(20);

/// 发送失败原因（三分互斥；无 thiserror，手写 Display 随仓库风格）
#[derive(Debug)]
pub enum NotifyError {
    /// 子进程无法拉起或等待时 IO 出错——最常见成因：termux-api 未安装/不在 PATH
    Spawn(std::io::Error),
    /// `termux-notification` 非零退出（携退出码；`None` = 被信号终止）
    Exit(Option<i32>),
    /// 各自动作上限（通知 [`SEND_BOUND`] / 震动 [`VIBRATE_BOUND`]）内未退出，已 kill 回收
    Timeout,
}

impl core::fmt::Display for NotifyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NotifyError::Spawn(e) => {
                write!(f, "notify: 进程拉起/等待失败（termux-api 未装？）: {e}")
            }
            NotifyError::Exit(Some(code)) => {
                write!(f, "notify: termux-notification 非零退出 {code}")
            }
            NotifyError::Exit(None) => write!(f, "notify: termux-notification 被信号终止"),
            NotifyError::Timeout => write!(
                f,
                "notify: 出站 CLI 超时已杀（notification ≤ {SEND_BOUND:?} / vibrate ≤ {VIBRATE_BOUND:?}）"
            ),
        }
    }
}
impl std::error::Error for NotifyError {}

/// 纯参数规划器（单一事实源）：`termux-notification --title <t> --content <c>
/// --priority high --id <id>`。返回 (程序名, 参数序)，无 IO，可单测钉死。
pub fn plan(id: &str, title: &str, content: &str) -> (String, Vec<String>) {
    (
        "termux-notification".to_string(),
        vec![
            "--title".to_string(),
            title.to_string(),
            "--content".to_string(),
            content.to_string(),
            "--priority".to_string(),
            "high".to_string(),
            "--id".to_string(),
            id.to_string(),
        ],
    )
}

/// 发送一条 Android 通知（阻塞 ≤ [`SEND_BOUND`]，spawn+`try_wait` 轮询，超时 kill）。
/// 永不 panic；失败经 [`NotifyError`] 三分返回，由调用方决定如何声张。
pub fn notify(id: &str, title: &str, content: &str) -> Result<(), NotifyError> {
    let (prog, args) = plan(id, title, content);
    dispatch(&prog, &args, SEND_BOUND)
}

/// 纯参数规划器（单一事实源）：`termux-vibrate -d <ms>`。返回 (程序名, 参数序)，
/// 无 IO，可单测钉死。与 [`plan`] 同理：CLI 契约只在此处出现一次。
pub fn plan_vibrate(ms: u64) -> (String, Vec<String>) {
    (
        "termux-vibrate".to_string(),
        vec!["-d".to_string(), ms.to_string()],
    )
}

/// 触发一次震动（阻塞 ≤ [`VIBRATE_BOUND`]；实测移交 ~2.4s，见 [`VIBRATE_BOUND`] 注）。
/// 永不 panic；失败经 [`NotifyError`] 三分返回，由调用方决定如何声张。
pub fn vibrate(ms: u64) -> Result<(), NotifyError> {
    let (prog, args) = plan_vibrate(ms);
    dispatch(&prog, &args, VIBRATE_BOUND)
}

/// 出站子进程公共流水：spawn（三 stdio 皆 null）→ `try_wait` 轮询至 `bound`
/// 越界 kill。通知与震动共用同一回收纪律，仅超时上限不同。
fn dispatch(prog: &str, args: &[String], bound: Duration) -> Result<(), NotifyError> {
    let mut child = match Command::new(prog)
        .args(args)
        // 三stdio 皆 null：无管道 ⇒ 无缓冲区互锁，子进程退出即回收
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Err(NotifyError::Spawn(e)),
    };
    let deadline = Instant::now() + bound;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(NotifyError::Exit(status.code()))
                };
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(NotifyError::Spawn(e));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait(); // 回收防僵尸；kill 后 wait 必快速返回
            return Err(NotifyError::Timeout);
        }
        std::thread::sleep(POLL_STEP);
    }
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::{plan, plan_vibrate};

    /// 程序名与参数序为出站契约：钉死 `--title → --content → --priority high → --id`
    #[test]
    fn plan_pins_program_and_exact_arg_order() {
        let (prog, args) = plan("atlas-mem", "标题", "正文");
        assert_eq!(prog, "termux-notification");
        assert_eq!(
            args,
            vec![
                "--title",
                "标题",
                "--content",
                "正文",
                "--priority",
                "high",
                "--id",
                "atlas-mem",
            ]
        );
    }

    /// 震动出站契约：钉死 `termux-vibrate -d <ms>`（时长为十进制字符串）
    #[test]
    fn plan_vibrate_pins_program_and_duration_arg() {
        let (prog, args) = plan_vibrate(5000);
        assert_eq!(prog, "termux-vibrate");
        assert_eq!(args, vec!["-d", "5000"]);
    }
}
