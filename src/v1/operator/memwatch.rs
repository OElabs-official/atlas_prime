//! operator 的内存告警政策（模块内部件）：滞回边沿状态机 + 落库后的出站拼装。
//!
//! [`MemWatch`] 是**纯函数式**的每轮求值器：输入 = 本轮 mem avail（MB，缺读数
//! 传 `None`），输出 = 至多一个 [`Alert`]，零 IO、零时钟、零线程——可在 `cargo test`
//! 里直接驱动。滞回双阈值：avail < [`LOW_MB`] 触发“不足”并闭锁（持续低迷不重发），
//! 必须回到 ≥ [`REARM_MB`] 才发一次“恢复”并重武装；2048..2560 死区内状态不变。
//!
//! [`policy`] 是操作器每轮落库后调用的政策入口：从本轮存储行提取 mem/avail 读数
//! → 状态机求值 → 事件以固定 [`MEM_NOTIFY_ID`] 经注入的 [`Sender`] 出站
//! （生产 = `notify::notify`，测试 = 记录器）；**仅“不足”警告**伴一次
//! [`VIBRATE_MS`] 毫秒震动，经注入的 [`Vibrator`] 出站（生产 = `notify::vibrate`），
//! “恢复”不震动。发送失败仅单行中文 stderr——出站是政策的最末梢，
//! 采样轮永不受扰。

use std::sync::{Mutex, OnceLock};

use crate::v1::db::Record;
use crate::v1::notify::NotifyError;

/// 低水位阈值（MB）：avail 低于此值触发告警
pub const LOW_MB: f64 = 2048.0;
/// 重武装阈值（MB）：avail 回到此值以上才解除闭锁并发恢复通知
pub const REARM_MB: f64 = 2560.0;
/// 内存通知固定 id：同 id 覆盖旧通知，告警不堆叠
pub const MEM_NOTIFY_ID: &str = "atlas-mem";
/// “不足”警告伴震时长（毫秒）：仅下降沿响一次，恢复不震动
pub const VIBRATE_MS: u64 = 5000;

/// 告警类别（决定通知标题；文案与 id 由操作器定）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    /// 可用内存跌破低水位（下降沿）
    Low,
    /// 可用内存回到重武装线（上升沿，闭锁解除）
    Recovered,
}

/// [`MemWatch`] 的边沿输出：类别 + 已填好读数的正文
#[derive(Debug)]
pub struct Alert {
    pub kind: AlertKind,
    pub content: String,
}

/// 滞回边沿检测器：闭锁位即全部状态。
#[derive(Debug, Default)]
pub struct MemWatch {
    /// true = 已发“不足”且未恢复（闭锁，低迷轮次不再触发）
    latched: bool,
}

impl MemWatch {
    /// 新建（未闭锁）
    pub fn new() -> Self {
        Self::default()
    }

    /// 求值一轮：`avail_mb = None`（本轮无 mem avail 读数）→ 状态不变、无告警。
    /// 下降沿（跌破 [`LOW_MB`] 且未闭锁）→ 闭锁 + [`AlertKind::Low`]；
    /// 上升沿（≥ [`REARM_MB`] 且已闭锁）→ 解锁 + [`AlertKind::Recovered`]；
    /// 其余（死区、持续低迷、平静高位）→ `None`。
    pub fn on_round(&mut self, avail_mb: Option<f64>) -> Option<Alert> {
        match avail_mb {
            None => None,
            Some(avail) if avail < LOW_MB => {
                if self.latched {
                    None
                } else {
                    self.latched = true;
                    Some(Alert {
                        kind: AlertKind::Low,
                        content: format!("可用内存不足 2G：当前仅剩 {avail} MB"),
                    })
                }
            }
            Some(avail) if avail >= REARM_MB => {
                if self.latched {
                    self.latched = false;
                    Some(Alert {
                        kind: AlertKind::Recovered,
                        content: format!("可用内存已恢复至 {avail} MB"),
                    })
                } else {
                    None
                }
            }
            Some(_) => None, // 死区 2048..2560：双向不变
        }
    }
}

// ---------------- 政策入口（落库后） ----------------

/// 出站发送器函数指针（可注入接缝）：生产默认 `notify::notify`，
/// 测试注入记录器即可在不触 CLI 的前提下驱动整条政策路径。
pub type Sender = fn(id: &str, title: &str, content: &str) -> Result<(), NotifyError>;

/// 出站震动器函数指针（可注入接缝，与 [`Sender`] 并列）：生产默认
/// `notify::vibrate`，测试注入记录器以钉死“仅警告震动、恢复不震动”。
pub type Vibrator = fn(ms: u64) -> Result<(), NotifyError>;

/// 进程唯一的滞回边沿状态机（低内存政策的全部记忆所在；采样线程单写者，
/// Mutex 只为合规，毒化后直取内值——求值器不可能 panic）。
fn mem_watch() -> &'static Mutex<MemWatch> {
    static S: OnceLock<Mutex<MemWatch>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(MemWatch::new()))
}

/// 落库后的政策入口：从本轮存储行取 mem/avail 读数（缺读数 = no-op）→
/// [`MemWatch`] 边沿求值 → 事件经 `send` 以 [`MEM_NOTIFY_ID`] 出站；
/// “不足”警告额外经 `vibrate` 伴震 [`VIBRATE_MS`] 毫秒（“恢复”不震动）。
/// 出站失败仅单行中文 stderr，采样轮永不受扰。
pub fn policy(records: &[Record], send: Sender, vibrate: Vibrator) {
    let avail = records
        .iter()
        .find(|r| r.kind == "mem" && r.key == "avail")
        .map(|r| r.value);
    let Some(alert) = mem_watch()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .on_round(avail)
    else {
        return;
    };
    // 标题与伴震策略在同一个穷举 match 里定：新告警类别必须当场表态震不震
    let (title, buzz) = match alert.kind {
        AlertKind::Low => ("ATLAS · 内存不足警告", true),
        AlertKind::Recovered => ("ATLAS · 内存恢复", false),
    };
    if let Err(e) = send(MEM_NOTIFY_ID, title, &alert.content) {
        eprintln!("操作器: 内存通知发送失败（采样轮不受影响）: {e}");
    }
    if buzz {
        if let Err(e) = vibrate(VIBRATE_MS) {
            eprintln!("操作器: 告警震动发送失败（采样轮不受影响）: {e}");
        }
    }
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 每下降沿恰发一次：首跌破触发，此后低迷轮次一律静默
    #[test]
    fn low_fires_once_per_falling_edge() {
        let mut w = MemWatch::new();
        let a = w.on_round(Some(2000.0)).expect("首跌破应告警");
        assert_eq!(a.kind, AlertKind::Low);
        assert!(a.content.contains("2000"));
        assert!(w.on_round(Some(1500.0)).is_none(), "持续低迷不得重发");
        assert!(w.on_round(Some(100.0)).is_none());
        // 恢复后再度跌破 = 新下降沿，再触发
        w.on_round(Some(3000.0));
        assert!(
            w.on_round(Some(1800.0))
                .is_some_and(|a| a.kind == AlertKind::Low)
        );
    }

    /// 死区 2048..2560 抖动（已闭锁）：既不重发“不足”，也不误发“恢复”
    #[test]
    fn flap_within_dead_zone_is_silent() {
        let mut w = MemWatch::new();
        assert!(w.on_round(Some(1900.0)).is_some());
        for v in [2100.0, 2559.9, 2300.0, 2048.0] {
            assert!(w.on_round(Some(v)).is_none(), "死区 {v} 不得出告警");
        }
    }

    /// 上升沿只发一次“恢复”且重武装：高位第二轮静默，再跌破可再触发
    #[test]
    fn recovery_emits_once_and_rearms() {
        let mut w = MemWatch::new();
        w.on_round(Some(1900.0));
        let r = w.on_round(Some(2560.0)).expect("到线应发恢复");
        assert_eq!(r.kind, AlertKind::Recovered);
        assert!(r.content.contains("2560"));
        assert!(w.on_round(Some(4000.0)).is_none(), "恢复只发一次");
        assert!(
            w.on_round(Some(1000.0))
                .is_some_and(|a| a.kind == AlertKind::Low),
            "应已重武装"
        );
    }

    /// 未闭锁时到线 = 无事发生（无“未报警先恢复”）
    #[test]
    fn recovery_without_prior_low_is_silent() {
        let mut w = MemWatch::new();
        assert!(w.on_round(Some(5000.0)).is_none());
    }

    /// None 输入完全 no-op：两态下均不出告警、不动闭锁
    #[test]
    fn none_input_is_noop_in_both_states() {
        let mut w = MemWatch::new();
        assert!(w.on_round(None).is_none(), "平静态 None 应 no-op");
        assert!(w.on_round(Some(1900.0)).is_some());
        assert!(w.on_round(None).is_none(), "闭锁态 None 应 no-op");
        assert!(w.on_round(Some(1900.0)).is_none(), "None 不得悄悄解锁");
    }

    /// 端到端（注入发送器+震动器，不触真实 CLI）：假存储行驱动落库后的完整政策
    /// 路径——跌破只发一次警告（固定 id + 警告标题 + 含读数正文）且**伴震一次**
    /// [`VIBRATE_MS`]；低迷/无读数静默零震动；恢复发一次且**零震动**；坏发送器
    /// 只损失一行 stderr，伴震照常、状态机不被打坏、轮次照常。
    /// 本测试是 [`mem_watch`] 静态态的唯一驱动者，序列自洽与调度无关。
    #[test]
    fn policy_sends_edges_via_injected_sender() {
        static SENT: OnceLock<Mutex<Vec<(String, String, String)>>> = OnceLock::new();
        static VIB: OnceLock<Mutex<Vec<u64>>> = OnceLock::new();
        fn sent_log() -> Vec<(String, String, String)> {
            SENT.get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
        fn vib_log() -> Vec<u64> {
            VIB.get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
        fn rec(id: &str, title: &str, content: &str) -> Result<(), NotifyError> {
            SENT.get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((id.to_string(), title.to_string(), content.to_string()));
            Ok(())
        }
        fn rec_vib(ms: u64) -> Result<(), NotifyError> {
            VIB.get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(ms);
            Ok(())
        }
        fn boom(_: &str, _: &str, _: &str) -> Result<(), NotifyError> {
            Err(NotifyError::Exit(Some(1)))
        }
        let round = |avail: f64| {
            vec![Record {
                ts: "2026-01-01T00:00:00Z".into(),
                ts_secs: 0,
                kind: "mem".into(),
                key: "avail".into(),
                value: avail,
            }]
        };

        policy(&round(1900.0), rec, rec_vib); // 下降沿 → 警告：恰一条通知 + 恰一次伴震
        assert_eq!(sent_log().len(), 1);
        assert_eq!(vib_log(), vec![VIBRATE_MS], "警告应恰伴震一次");

        policy(&round(1800.0), rec, rec_vib); // 持续低迷 → 闭锁静默
        policy(&[], rec, rec_vib); // 本轮无 mem/avail 读数 → no-op
        assert_eq!(
            (sent_log().len(), vib_log().len()),
            (1, 1),
            "持续低迷/无读数不得重发或重震"
        );

        policy(&round(3000.0), rec, rec_vib); // 上升沿 → 恢复：只发通知，零震动
        assert_eq!(
            (sent_log().len(), vib_log().len()),
            (2, 1),
            "恢复不得伴震"
        );

        policy(&round(1000.0), boom, rec_vib); // 再跌破撞坏发送器 → 政策不炸、伴震照常
        assert_eq!(vib_log(), vec![VIBRATE_MS; 2], "通知失败不得吞掉伴震");

        policy(&round(3000.0), rec, rec_vib); // 失败未污染状态机 → 恢复照常

        let sent = sent_log();
        assert_eq!(sent.len(), 3, "应恰发 警告/恢复/恢复 三条，实际 {sent:?}");
        assert_eq!(sent[0].0, MEM_NOTIFY_ID);
        assert_eq!(sent[0].1, "ATLAS · 内存不足警告");
        assert!(sent[0].2.contains("1900"), "正文应含读数：{}", sent[0].2);
        assert_eq!(sent[1].1, "ATLAS · 内存恢复");
        assert!(sent[1].2.contains("3000"));
        assert_eq!(sent[2].1, "ATLAS · 内存恢复");
        assert_eq!(
            vib_log(),
            vec![VIBRATE_MS; 2],
            "两个下降沿各伴震一次，恢复/低迷零震动"
        );
    }
}
