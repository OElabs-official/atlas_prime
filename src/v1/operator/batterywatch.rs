//! operator 的充电提醒政策（模块内部件）：滞回边沿状态机 + 落库后的出站拼装。
//!
//! [`BatteryWatch`] 是**纯函数式**的每轮求值器：输入 = 本轮电量（%，缺读数传
//! `None`）与充电态 `plugged`，输出 = 至多一个 [`Alert`]，零 IO、零时钟、零线程
//! ——可在 `cargo test` 里直接驱动。滞回双阈值：未充电且 pct < [`LOW_PCT`] 触发
//! “充电提醒”并闭锁（持续低迷不重发）；pct ≥ [`REARM_PCT`] **或插入电源**即
//! *静默*重武装（充电态本身即“已处理”，本轮不发任何恢复通知，与 memwatch 的
//! “恢复”通知不同）；[`LOW_PCT`]..[`REARM_PCT`] 死区内状态不变。全程不震动。
//!
//! [`policy`] 是操作器每轮落库后调用的政策入口：从本轮存储行提取 battery/pct 与
//! battery/plugged 读数（collect 把 plugged 存为 0/1 标记，**非零 = 在充电**，
//! 含 AC/USB/无线模式）→ 状态机求值 → 事件以固定 [`BATT_NOTIFY_ID`] 经注入的
//! [`Sender`] 出站（生产 = `notify::notify`，测试 = 记录器）；任一读数缺失的
//! 轮次一律 no-op（不猜测）。发送失败仅单行中文 stderr——出站是政策的最末梢，
//! 采样轮永不受扰。

use std::sync::{Mutex, OnceLock};

use crate::v1::db::Record;
use crate::v1::notify::NotifyError;

/// 低电量阈值（%）：未充电且 pct 低于此值触发充电提醒
pub const LOW_PCT: f64 = 50.0;
/// 重武装阈值（%）：pct 回到此值以上解除闭锁（静默，不发恢复通知）
pub const REARM_PCT: f64 = 55.0;
/// 充电通知固定 id：同 id 覆盖旧通知，提醒不堆叠
pub const BATT_NOTIFY_ID: &str = "atlas-batt";
/// 通知标题（单义告警，标题由 [`policy`] 定，与 memwatch 同构）
pub const BATT_TITLE: &str = "ATLAS · 充电提醒";

/// [`BatteryWatch`] 的边沿输出：已填好读数的正文
#[derive(Debug)]
pub struct Alert {
    pub content: String,
}

/// 滞回边沿检测器：闭锁位即全部状态。
#[derive(Debug, Default)]
pub struct BatteryWatch {
    /// true = 已发“充电提醒”且未重武装（闭锁，低迷轮次不再触发）
    latched: bool,
}

impl BatteryWatch {
    /// 新建（未闭锁）
    pub fn new() -> Self {
        Self::default()
    }

    /// 求值一轮：`pct = None`（本轮无 battery pct 读数）→ 状态不变、无告警。
    /// 充电中或 pct ≥ [`REARM_PCT`] → 静默解锁（恒返回 `None`，无恢复通知）；
    /// 未充电且跌破 [`LOW_PCT`] 且未闭锁 → 闭锁 + [`Alert`]；
    /// 其余（死区 50..55、持续低迷、平静高位）→ `None`。
    pub fn on_round(&mut self, pct: Option<f64>, plugged: bool) -> Option<Alert> {
        let Some(pct) = pct else { return None };
        if plugged || pct >= REARM_PCT {
            self.latched = false; // 静默重武装：插电本身即“已处理”，不再打扰
            return None;
        }
        if pct < LOW_PCT {
            if self.latched {
                None
            } else {
                self.latched = true;
                Some(Alert {
                    content: format!("电量已低于 50%：当前 {pct}%，请充电"),
                })
            }
        } else {
            None // 死区 50..55（未充电）：双向不变
        }
    }
}

// ---------------- 政策入口（落库后） ----------------

/// 出站发送器函数指针（可注入接缝）：生产默认 `notify::notify`，
/// 测试注入记录器即可在不触 CLI 的前提下驱动整条政策路径。
pub type Sender = fn(id: &str, title: &str, content: &str) -> Result<(), NotifyError>;

/// 进程唯一的滞回边沿状态机（充电政策的全部记忆所在；采样线程单写者，
/// Mutex 只为合规，毒化后直取内值——求值器不可能 panic）。
fn batt_watch() -> &'static Mutex<BatteryWatch> {
    static S: OnceLock<Mutex<BatteryWatch>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(BatteryWatch::new()))
}

/// 落库后的政策入口：从本轮存储行取 battery/pct 与 battery/plugged 读数
/// （collect 存法：plugged 非零 = 在充电；缺任一读数 = 不猜测、整轮 no-op）→
/// [`BatteryWatch`] 边沿求值 → 事件经 `send` 以 [`BATT_NOTIFY_ID`] 出站，不震动。
/// 出站失败仅单行中文 stderr，采样轮永不受扰。
pub fn policy(records: &[Record], send: Sender) {
    let mut pct = None;
    let mut plugged = None;
    for r in records {
        if r.kind == "battery" {
            match r.key.as_str() {
                "pct" => pct = Some(r.value),
                "plugged" => plugged = Some(r.value != 0.0),
                _ => {}
            }
        }
    }
    let Some(plugged) = plugged else { return };
    let Some(alert) = batt_watch()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .on_round(pct, plugged)
    else {
        return;
    };
    if let Err(e) = send(BATT_NOTIFY_ID, BATT_TITLE, &alert.content) {
        eprintln!("操作器: 充电提醒发送失败（采样轮不受影响）: {e}");
    }
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: &str, key: &str, value: f64) -> Record {
        Record {
            ts: "2026-01-01T00:00:00Z".into(),
            ts_secs: 0,
            kind: kind.into(),
            key: key.into(),
            value,
        }
    }

    /// 每下降沿恰发一次：首跌破触发，此后低迷轮次一律静默；
    /// ≥55 静默重武装后再度跌破 = 新下降沿，再触发
    #[test]
    fn low_fires_once_per_falling_edge() {
        let mut w = BatteryWatch::new();
        let a = w.on_round(Some(44.0), false).expect("首跌破应告警");
        assert!(a.content.contains("44"), "正文应含读数：{}", a.content);
        assert!(w.on_round(Some(30.0), false).is_none(), "持续低迷不得重发");
        assert!(w.on_round(Some(49.9), false).is_none());
        assert!(
            w.on_round(Some(55.0), false).is_none(),
            "重武装应静默，不发恢复通知"
        );
        assert!(
            w.on_round(Some(48.0), false).is_some(),
            "重武装后再度跌破应再触发"
        );
    }

    /// 充电中的低电量轮次拦截触发（拔电后跌破照常触发）
    #[test]
    fn plugged_round_blocks_fire() {
        let mut w = BatteryWatch::new();
        assert!(w.on_round(Some(30.0), true).is_none(), "充电中不得触发");
        assert!(w.on_round(Some(28.0), true).is_none());
        assert!(
            w.on_round(Some(28.0), false).is_some(),
            "拔电后跌破应触发"
        );
    }

    /// 插入电源 = 静默重武装：零通知，且下一次拔电跌破可再触发
    #[test]
    fn plug_in_rearms_silently_and_allows_refire() {
        let mut w = BatteryWatch::new();
        assert!(w.on_round(Some(44.0), false).is_some(), "前提：先触发");
        assert!(
            w.on_round(Some(44.0), true).is_none(),
            "插电重武装必须静默（本轮低电量也不重发）"
        );
        assert!(
            w.on_round(Some(44.0), false).is_some(),
            "插电重武装后，拔电跌破应再触发"
        );
    }

    /// 死区 50..55（未充电）既不解锁也不重发；≥55 才解锁
    #[test]
    fn dead_zone_holds_latch() {
        let mut w = BatteryWatch::new();
        assert!(w.on_round(Some(44.0), false).is_some());
        assert!(w.on_round(Some(52.0), false).is_none());
        assert!(w.on_round(Some(49.0), false).is_none(), "死区不得提前解锁重发");
        assert!(w.on_round(Some(60.0), false).is_none(), "静默解锁零通知");
        assert!(
            w.on_round(Some(44.0), false).is_some(),
            "≥55 后应已重武装"
        );
    }

    /// None 读数完全 no-op：两态下均不出告警、不动闭锁
    #[test]
    fn none_pct_is_noop_in_both_states() {
        let mut w = BatteryWatch::new();
        assert!(w.on_round(None, false).is_none(), "平静态 None 应 no-op");
        assert!(w.on_round(Some(44.0), false).is_some());
        assert!(w.on_round(None, false).is_none(), "闭锁态 None 应 no-op");
        assert!(w.on_round(None, true).is_none(), "None+插电也零输出");
        assert!(
            w.on_round(Some(44.0), false).is_none(),
            "None 轮不得悄悄解锁"
        );
    }

    /// 临界抖动 49.5..54.5（未充电）：全程只有一个下降沿，恰触发一次
    #[test]
    fn flap_across_low_edge_no_double_fire() {
        let mut w = BatteryWatch::new();
        let mut fired = 0;
        for pct in [49.5, 54.5, 49.5, 52.0, 54.5, 49.9] {
            fired += usize::from(w.on_round(Some(pct), false).is_some());
        }
        assert_eq!(fired, 1, "49.5..54.5 抖动只应触发一次，实际 {fired}");
    }

    /// 端到端（注入发送器，不触真实 CLI）：假 battery 存储行驱动落库后的完整政策
    /// 路径——跌破发一条（固定 id + 固定标题 + 含读数正文）；低迷、缺 plugged 键、
    /// 缺 pct 键一律静默；插电静默重武装后可再触发；坏发送器只损失一行 stderr，
    /// 状态机不被打坏、轮次照常。本测试是 [`batt_watch`] 静态态的唯一驱动者，
    /// 序列自洽与调度无关。
    #[test]
    fn policy_sends_edges_via_injected_sender() {
        static SENT: OnceLock<Mutex<Vec<(String, String, String)>>> = OnceLock::new();
        fn sent_log() -> Vec<(String, String, String)> {
            SENT.get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
        fn rec3(id: &str, title: &str, content: &str) -> Result<(), NotifyError> {
            SENT.get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((id.to_string(), title.to_string(), content.to_string()));
            Ok(())
        }
        fn boom(_: &str, _: &str, _: &str) -> Result<(), NotifyError> {
            Err(NotifyError::Exit(Some(1)))
        }
        let round = |pct: f64, plugged: f64| {
            vec![
                rec("battery", "pct", pct),
                rec("battery", "plugged", plugged),
            ]
        };

        policy(&round(44.0, 0.0), rec3); // 下降沿 → 恰一条通知
        assert_eq!(sent_log().len(), 1);

        policy(&round(40.0, 0.0), rec3); // 持续低迷 → 闭锁静默
        policy(&[rec("battery", "pct", 40.0)], rec3); // 缺 plugged 键 → no-op
        policy(&[rec("battery", "plugged", 0.0)], rec3); // 缺 pct 键 → no-op
        policy(&[], rec3); // 空轮 → no-op
        assert_eq!(sent_log().len(), 1, "低迷/缺读数不得重发");

        policy(&round(40.0, 1.0), rec3); // 插电 → 静默重武装，零通知
        assert_eq!(sent_log().len(), 1, "重武装不得发通知");

        policy(&round(39.0, 0.0), boom); // 拔电再跌破撞坏发送器 → 政策不炸、状态机闭锁
        policy(&round(38.0, 0.0), rec3); // 坏发送器轮后仍闭锁 → 不重发
        assert_eq!(sent_log().len(), 1, "坏发送器未污染闭锁态");

        policy(&round(60.0, 0.0), rec3); // ≥55 静默重武装
        policy(&round(40.0, 0.0), rec3); // 新下降沿 → 第二条通知
        let sent = sent_log();
        assert_eq!(sent.len(), 2, "应恰发两条提醒，实际 {sent:?}");
        assert_eq!(sent[0].0, BATT_NOTIFY_ID);
        assert_eq!(sent[0].1, "ATLAS · 充电提醒");
        assert!(sent[0].2.contains("44"), "正文应含读数：{}", sent[0].2);
        assert!(sent[1].2.contains("40"), "正文应含读数：{}", sent[1].2);
    }
}
