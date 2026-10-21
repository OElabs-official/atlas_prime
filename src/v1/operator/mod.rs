//! v1/operator: 操作器黑箱。
//!
//! **操作器是程序内唯一自主驱动数据的模块**（the only module in-process that
//! autonomously drives data）。其余模块要么请求驱动（web 应答、system 读数、task
//! 受控进程），要么被动存储（db 只收发不自发产数据）。抽掉操作器，进程数据流即
//! 停摆成“拨一下转一下”的死水。
//!
//! # 公开接口用法
//! ```ignore
//! use crate::v1::operator::Operator;
//!
//! Operator::start().ok();              // 双回路线程：全域采样（首轮立即，2 分钟/轮）
//!                                      // + 网卡速度（1Hz）（幂等，二次调用 Err）
//! let n = Operator::sample_now().ok(); // 手动补一轮全域采样（返回写入行数）
//! let v = Operator::net_speeds();      // 网速缓存读（分组）(组名, rx_bps, tx_bps)，
//!                                      // 序 [wifi, cellular, 其余升序]，不碰 /proc
//! Operator::stop();                    // 置标志 + Notify 即时唤醒，两条回路当前轮收尾即停
//! ```
//!
//! # 职责
//! - [`Operator::start`]：拉起采样线程。等待阶段 = sleep 与停止唤醒（`Notify`）的
//!   select 竞争；[`Operator::stop`] 置标志并 `notify_waiters`，采样线程微秒级退出，
//!   在途轮次完整落库后收尾；
//! - [`collect`]：全域快照——system 各黑箱（cpu/thermal/mem/swap/battery/disk）展开
//!   为存储行；**采集时刻即打戳**：`ts` = RFC3339（秒精度、UTC），同一轮全部行同一时刻；
//! - 落库经 [`crate::v1::db`]（被动存储）：本模块决定何时采，db 决定怎么存/修剪；
//! - 每轮落库后跑内存告警政策：[`memwatch::policy`] 滞回边沿求值（**政策在操作器内**），
//!   事件经 [`crate::v1::notify`] 出站（被动管道，固定 id = 替换不堆叠）；
//!   “不足”警告伴 5 秒震动（[`crate::v1::notify::vibrate`]），“恢复”不震；
//!   发送失败 = 单行 stderr 告警，采样轮永不受扰；
//! - 同轮再跑充电提醒政策：[`batterywatch::policy`] 滞回边沿求值（未充电跌破 50%
//!   提醒一次并闭锁，充入电源或回到 ≥55% 静默重武装，无震动），
//!   事件同样经 [`crate::v1::notify`] 以固定 id 出站；
//! - [`run_net_meter`]：**独立的 1Hz 网卡速度回路**（与 120s 全域轮同一停止唤醒、
//!   同一共享 runtime，但各自成任务）：每拍把 `/proc/net/dev` 计数器快照喂给
//!   [`network::speed::Grouper`]（**分组计量**：wlan*→`wifi`、rmnet 数据面→
//!   `cellular`、有流量的其他网卡自组；组内先求和再差分；首轮只立基线不 emit；
//!   成员增减整组重建基线 emit 0；倒退钳 0），产出 bytes/s 后 (a) 整体刷新
//!   [`Operator::net_speeds`] 缓存（web 只读缓存，绝不自读 /proc），(b) 落库
//!   kind=`net`、key=`<组名>.rx_bps`/`<组名>.tx_bps`（如 `wifi.rx_bps`、
//!   `cellular.tx_bps`；打戳机制与全域轮相同；db 侧按 kind 分档修剪，net ≈8h）。
//!   源链（直读→adb 桥→sysfs→netlink）
//!   整拍全灭时**缓存冻结**（stale 照旧暴露、基线保留），连败计满
//!   [`NET_FAIL_STREAK`] 拍单行 stderr 示警一次（整串静默，一拍成功即重置）。
//!
//! 共享 runtime、连接与 WAL 并发模型见 [`crate::v1::db`] 模块文档；采样线程即跑在其上。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};

use crate::v1::db::{self, Db, DbError, Record};
use crate::v1::notify;
use crate::v1::system::{battery, cpu, disk, mem, network, thermal};

mod batterywatch;
mod memwatch;

/// 全域采样周期（start 使用）
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(120);

/// 网卡速度回路采样周期（1Hz；db 侧 net 按 28800 轮 ≈ 8h 修剪）
pub const NET_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// 测速源链（直读→adb 桥→sysfs→netlink）连续整链失败多少拍后 stderr 示警
/// 一次（每条失败连串至多一条；成功一拍即清零）。缓存自失败首拍起就冻结，
/// 阈值只管声张、不管数据面。
const NET_FAIL_STREAK: u32 = 5;

/// 操作器失败原因
#[derive(Debug)]
pub enum OperatorError {
    /// 底层存储（db 黑箱）失败
    Db(DbError),
    /// 采样线程已在运行
    Started,
}

impl core::fmt::Display for OperatorError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            OperatorError::Db(e) => write!(f, "operator: {e}"),
            OperatorError::Started => write!(f, "operator sampler already started"),
        }
    }
}
impl std::error::Error for OperatorError {}

impl From<DbError> for OperatorError {
    fn from(e: DbError) -> Self {
        OperatorError::Db(e)
    }
}

/// 唯一公开入口
pub struct Operator;

impl Operator {
    /// 启动采样线程：全域回路（首轮立即，其后每 [`SAMPLE_INTERVAL`]）+
    /// 网卡速度回路（每 [`NET_SAMPLE_INTERVAL`]）并行跑在同一共享 runtime 上。
    /// 幂等：重复调用返回 [`OperatorError::Started`]。
    pub fn start() -> Result<(), OperatorError> {
        static HANDLE: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();
        if RUNNING.swap(true, Ordering::SeqCst) {
            return Err(OperatorError::Started);
        }
        stop_flag().store(false, Ordering::SeqCst);
        let h = std::thread::spawn(|| {
            let _ = db::rt().block_on(async { tokio::join!(run_sampler(), run_net_meter()) });
        });
        HANDLE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map(|mut g| *g = Some(h))
            .ok();
        Ok(())
    }

    /// 请求停止：置标志并即时唤醒睡眠中的采样线程（微秒级退出；在途轮次完整收尾）。
    pub fn stop() {
        stop_flag().store(true, Ordering::SeqCst);
        stop_notify().notify_waiters();
    }

    /// 立即采一轮并落库（含 db 侧滚动修剪与内存告警策略），返回写入行数。
    /// 同步入口：驱动 db 共享 runtime（[`db::block_on`]），可在任意
    /// （非该 runtime 内部的）线程上下文调用。
    pub fn sample_now() -> Result<usize, OperatorError> {
        db::block_on(sample_round())
    }

    /// 网速缓存读（**分组级**）：`(组名, rx_bps, tx_bps)`，序恒为
    /// `[wifi, cellular, 其余按名升序]`。**纯缓存**——绝不自读 /proc/sysfs；
    /// 由 [`Operator::start`] 内的 1Hz 回路整体刷新，回路未启动或首个 emit
    /// 前为空。
    pub fn net_speeds() -> Vec<(String, f64, f64)> {
        net_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

// ---------------- 黑箱内部 ----------------

/// 采样线程运行标志（start 置位、线程退出清零）
static RUNNING: AtomicBool = AtomicBool::new(false);

fn stop_flag() -> &'static AtomicBool {
    static S: OnceLock<AtomicBool> = OnceLock::new();
    S.get_or_init(|| AtomicBool::new(false))
}

/// 即时停止唤醒通道：stop() 走 notify_waiters，采样循环的 select 立刻苏醒
fn stop_notify() -> &'static tokio::sync::Notify {
    static S: OnceLock<tokio::sync::Notify> = OnceLock::new();
    S.get_or_init(tokio::sync::Notify::new)
}

/// 采样线程主体（跑在 db 共享 runtime 上）：首轮立即，此后每 [`SAMPLE_INTERVAL`] 一轮。
/// 等待阶段 = sleep 与停止唤醒的 select 竞争；唤醒/轮次结束后复查停止标志，
/// 置位即退出——stop() 后微秒级返回睡眠，在途轮次完整收尾。
async fn run_sampler() {
    let _ = sample_round().await;
    loop {
        // enable() 在进 select 前即向 Notify 注册，杜绝“睡眠前 stop()”丢唤醒竞态
        let mut notified = std::pin::pin!(stop_notify().notified());
        let _ = notified.as_mut().enable();
        tokio::select! {
            _ = notified.as_mut() => { if stop_flag().load(Ordering::SeqCst) { break; } }
            _ = tokio::time::sleep(SAMPLE_INTERVAL) => {}
        }
        if stop_flag().load(Ordering::SeqCst) {
            break;
        }
        let _ = sample_round().await;
    }
    RUNNING.store(false, Ordering::SeqCst);
}

/// 网速缓存快照（分组级）`(组名, rx_bps, tx_bps)`，序 [wifi, cellular, 其余升序]：
/// 1Hz 回路整体写入，[`Operator::net_speeds`]（web 读路径）就地克隆读出。
fn net_cache() -> &'static Mutex<Vec<(String, f64, f64)>> {
    static S: OnceLock<Mutex<Vec<(String, f64, f64)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

/// 1Hz 网卡速度回路主体（与 [`run_sampler`] 同一 runtime、同一停止唤醒，互不掺和）：
/// 首拍只喂 [`network::speed::Grouper`] 立组级基线不 emit；此后每拍 快照 →
/// 组和差分（成员增减整组重建基线 emit 0、倒退钳 0，dt 实测）→ 整体刷新缓存
/// → 落库 kind=net。等待阶段同样是 sleep 与停止 Notify 的 select 竞争。
/// 源链（[`network::read_dev_snapshot`] 起头，全灭才回 Err）整拍失败时
/// **缓存冻结**：stale 速率原样暴露、组级基线与 `prev_at` 不动（恢复后 dt 跨
/// 故障期摊薄），连续 [`NET_FAIL_STREAK`] 拍单行 stderr 示警——连串计数只在
/// 越过阈值那一拍发声（`==` 语义天然 once-per-streak），成功一拍即重置。
async fn run_net_meter() {
    let mut grouper = network::speed::Grouper::default();
    grouper.step(&network::speed::snapshot(), 0.0); // 首拍只立基线，产出不消费
    let mut prev_at = Instant::now();
    let mut fails: u32 = 0;
    loop {
        let mut notified = std::pin::pin!(stop_notify().notified());
        let _ = notified.as_mut().enable();
        tokio::select! {
            _ = notified.as_mut() => { if stop_flag().load(Ordering::SeqCst) { break; } }
            _ = tokio::time::sleep(NET_SAMPLE_INTERVAL) => {}
        }
        if stop_flag().load(Ordering::SeqCst) {
            break;
        }
        match network::speed::snapshot_result() {
            Ok(cur) => {
                fails = 0;
                let now = Instant::now();
                let dt = now.saturating_duration_since(prev_at).as_secs_f64();
                prev_at = now;
                let rates = grouper.step(&cur, dt);
                *net_cache().lock().unwrap_or_else(|e| e.into_inner()) = rates.clone();
                if !rates.is_empty() {
                    let _ = Db::insert(&net_records(&rates)).await;
                }
            }
            Err(e) => {
                fails += 1;
                if fails == NET_FAIL_STREAK {
                    eprintln!(
                        "atlas: 网卡测速源链连败 {NET_FAIL_STREAK} 拍，速度示停于末次成功值: {e}"
                    );
                }
            }
        }
    }
}

/// 一轮网速 → 存储行：kind=`net`，key=`<组名>.rx_bps` / `<组名>.tx_bps`
/// （组名 ∈ wifi / cellular / 有流量的其他网卡名）。
/// 打戳与 [`collect`] 同一机制：采集时刻单点、秒精度 UTC RFC3339，同拍同行同 ts。
fn net_records(rates: &[(String, f64, f64)]) -> Vec<Record> {
    let now = Utc::now();
    let ts = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let ts_secs = now.timestamp();
    rates
        .iter()
        .flat_map(|(group, rx, tx)| {
            [
                Record {
                    ts: ts.clone(),
                    ts_secs,
                    kind: "net".into(),
                    key: format!("{group}.rx_bps"),
                    value: *rx,
                },
                Record {
                    ts: ts.clone(),
                    ts_secs,
                    kind: "net".into(),
                    key: format!("{group}.tx_bps"),
                    value: *tx,
                },
            ]
        })
        .collect()
}

/// 一轮采样流水：collect → db 落库（被动存储）→ 内存告警政策（政策本体在
/// [`memwatch`]，出站走 [`notify::notify`] / [`notify::vibrate`] 默认通道）→
/// 充电提醒政策（政策本体在 [`batterywatch`]，出站走 [`notify::notify`]，不震动）。
/// 落库失败也早退——告警只对已成功落库的轮次负责。
async fn sample_round() -> Result<usize, OperatorError> {
    let records = collect();
    let n = Db::insert(&records).await?;
    memwatch::policy(&records, notify::notify, notify::vibrate);
    batterywatch::policy(&records, notify::notify);
    Ok(n)
}

/// 一轮全量采集：system 各黑箱 → 窄长存储行。
/// 采集时刻单点打戳：全行共享同一 RFC3339 `ts`（秒精度、UTC）与同源 `ts_secs`。
fn collect() -> Vec<Record> {
    let now = Utc::now();
    let ts = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let ts_secs = now.timestamp();
    let mut out = Vec::new();
    {
        let mut push = |kind: &str, key: String, value: f64| {
            out.push(Record {
                ts: ts.clone(),
                ts_secs,
                kind: kind.to_string(),
                key,
                value,
            })
        };

        for (i, ghz) in cpu::freq_ghz().iter().enumerate() {
            push("cpu_freq", format!("cpu{i}"), *ghz as f64);
        }

        let t = thermal::Temp::get();
        for (cat, group) in [
            ("cpu_core", &t.cpu_core),
            ("gpu", &t.gpu),
            ("camera", &t.camera),
            ("pmic", &t.pmic),
            ("battery", &t.battery),
        ] {
            for (i, (v, _name)) in group.iter().enumerate() {
                push("temp", format!("{cat}.{i}"), *v as f64);
            }
        }

        let m = mem::Mem::get();
        push("mem", "total".into(), m.total_mem_mb as f64);
        push("mem", "used".into(), m.used_mem_mb as f64);
        push("mem", "avail".into(), m.avail_mem_mb as f64);
        push("swap", "total".into(), m.total_swap_mb as f64);
        push("swap", "used".into(), m.used_swap_mb as f64);

        let b = battery::Battery::get();
        push("battery", "pct".into(), b.percentage as f64);
        push("battery", "volt_mv".into(), b.voltage as f64);
        push("battery", "current_ma".into(), b.current as f64);
        // 沿用 CLI 温度源；迁移 thermal::Temp.battery 属 system 侧改造，不在操作器内进行
        #[expect(
            deprecated,
            reason = "Battery::temperature 已弃用但迁移属 system 侧范围"
        )]
        push("battery", "temp_c".into(), b.temperature / 10.0); // CLI 值为 0.1℃ 单位
        push("battery", "cycle".into(), b.cycle as f64);
        push(
            "battery",
            "plugged".into(),
            (!b.plugged.is_empty() && b.plugged != "None") as u8 as f64,
        );
        push("battery", "present".into(), b.present as u8 as f64);

        for d in disk::Disk::get() {
            push("disk", format!("{}|used", d.mount_point), d.used_mb as f64);
            push(
                "disk",
                format!("{}|total", d.mount_point),
                d.total_mb as f64,
            );
        }
    }

    out
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    #[test]
    fn collect_covers_every_domain() {
        let v = collect();
        for kind in ["cpu_freq", "temp", "mem", "swap", "battery", "disk"] {
            assert!(v.iter().any(|r| r.kind == kind), "缺 {kind} 采样");
        }
        // 全轮同一采集时刻：ts 必须是可解析 RFC3339 且与 ts_secs 投影自洽
        assert!(v.iter().all(|r| r.ts_secs > 0));
        assert!(v.iter().all(|r| {
            DateTime::parse_from_rfc3339(&r.ts)
                .map(|d| d.timestamp() == r.ts_secs)
                .unwrap_or(false)
        }));
    }

    /// 缓存契约：1Hz 回路未启动 → 读到空（web 冷启动回 "{}" 的依据）
    #[test]
    fn net_speeds_empty_before_first_emit() {
        assert!(
            Operator::net_speeds().is_empty(),
            "首个 emit 前必须是空缓存，实际 {:?}",
            Operator::net_speeds()
        );
    }

    /// 净行数与打戳形状：每组 rx/tx 两行、同拍同 ts、key=`<组名>.<方向>`
    /// （分组级键：wifi/cellular/其他网卡名，与 net_speeds 缓存同形）
    #[test]
    fn net_records_shape_and_stamp() {
        let v = net_records(&[
            ("wifi".to_string(), 1.5, 2.5),
            ("cellular".to_string(), 3.5, 4.5),
        ]);
        assert_eq!(v.len(), 4);
        assert!(
            v.iter()
                .all(|r| r.kind == "net" && r.ts == v[0].ts && r.ts_secs > 0)
        );
        assert_eq!(v[0].key, "wifi.rx_bps");
        assert_eq!(v[1].key, "wifi.tx_bps");
        assert_eq!((v[0].value, v[1].value), (1.5, 2.5));
        assert_eq!(v[2].key, "cellular.rx_bps");
        assert_eq!(v[3].key, "cellular.tx_bps");
        assert!(DateTime::parse_from_rfc3339(&v[0].ts).is_ok());
    }

    /// 真机闭环：sample_now → rows → history（ts 已为 RFC3339）。
    /// `cargo test v1::operator -- --ignored --test-threads=1`
    #[test]
    #[ignore]
    fn live_roundtrip() {
        let n = Operator::sample_now().unwrap();
        assert!(n > 10, "单轮应写 >10 行，实际 {n}");
        let total = Db::rows().unwrap();
        assert!(total >= n as i64);
        let hist = Db::history("mem", Some("used"), 5);
        assert!(!hist.is_empty() && hist[0].value > 0.0);
        assert!(DateTime::parse_from_rfc3339(&hist[0].ts).is_ok());
    }

    /// 真机即时停止：采样线程运行中 stop() 应毫秒级退出（而非等满 2 分钟）。
    /// `cargo test v1::operator live_instant_stop -- --ignored --test-threads=1`
    #[test]
    #[ignore]
    fn live_instant_stop() {
        Operator::start().expect("首次 start 应成功");
        std::thread::sleep(Duration::from_secs(2)); // 越过首轮采样
        let t0 = std::time::Instant::now();
        Operator::stop();
        let t_stop = t0.elapsed();
        let exited = std::thread::spawn(|| {
            for _ in 0..500 {
                if !RUNNING.load(Ordering::SeqCst) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        })
        .join()
        .unwrap_or(false);
        assert!(exited, "stop() 后采样线程应在 5s 内退出");
        assert!(
            t_stop < Duration::from_millis(500),
            "stop() 不应阻塞，实际 {t_stop:?}"
        );
    }
}
