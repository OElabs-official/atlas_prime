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
//! Operator::start().ok();              // 三回路线程：全域采样（首轮立即，2 分钟/轮）
//!                                      // + 网卡速度（1Hz）+ 进程快照（5s）
//!                                      // （幂等，二次调用 Err）
//! let n = Operator::sample_now().ok(); // 手动补一轮全域采样（返回写入行数）
//! let v = Operator::net_speeds();      // 网速缓存读（分组）(组名, rx_bps, tx_bps)，
//!                                      // 序 [wifi, cellular, 其余升序]，不碰 /proc
 //! Operator::stop();                    // 置标志 + Notify 即时唤醒，三条回路当前轮收尾即停；
 //!                                      // 随后硬上限 10s 等待线程 'settled'（关停完成
 //!                                      // 契约），超时 WARN 一行后放手，卡死不拖关停
//! ```
//!
//! # 职责
//! - [`Operator::start`]：拉起采样线程。等待阶段 = sleep 与停止唤醒（`Notify`）的
//!   select 竞争；[`Operator::stop`] 置标志并 `notify_waiters`，采样线程微秒级退出，
//!   在途轮次完整落库后收尾；随后以 10s 硬上限等待线程 'settled'（关停完成契约，
//!   见 [`Operator::stop`] 文档），超时单行 WARN 后放手；
//! - [`collect`]：全域快照——system 各黑箱（cpu/thermal/gpu/mem/swap/battery/disk）展开
//!   为存储行；**采集时刻即打戳**：`ts` = RFC3339（秒精度、UTC），同一轮全部行同一时刻；
//!   每轮经 [`spawn_collect`] 上阻塞池执行（采集链延迟不定，不占驱动线程，任务
//!   失败示警后本轮跳过）；成功轮单行 INFO：落库行数 + 全程耗时 ms；
//!   失败轮（落库 `Err` 或采集跳轮）喂 [`FailStreak`] 连败计数，连续
//!   [`SAMPLER_FAIL_STREAK`] 轮单行 stderr 示警一次（每条连败串至多一条，
//!   成功轮即重置；连败不拦采样）；
//!   全域轮覆盖 GPU 频率/使用率（kind=`gpu`，sysfs 直读→su 桥），无 root 静默缺席不告警；
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
//!   源链（直读→su 桥→sysfs→netlink）
//!   整拍全灭时**缓存冻结**（stale 照旧暴露、基线保留），连败计满
//!   [`NET_FAIL_STREAK`] 拍单行 stderr 示警一次（整串静默，一拍成功即重置）；
//! - [`run_proc_meter`]：**独立的 5s 进程快照回路**（第三条回路，同一停止唤醒、
//!   同一共享 runtime）：每轮把 [`proc::scan`] 送上 spawn_blocking（/proc 批量
//!   小文件读），产物整体刷新 [`proc`] 缓存（web `/system/proc` 纯缓存读），
//!   成功轮日志静默。**不落库**（本域只观测不存储），亦不牵动
//!   1Hz/120s 两回路；扫描任务失败 = 缓存冻结（warming 或旧拍照旧暴露）。
//!
//! 共享 runtime、连接与 WAL 并发模型见 [`crate::v1::db`] 模块文档；采样线程即跑在其上。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};

use crate::v1::db::{self, Db, DbError, Record, kind};
use crate::v1::notify;
use crate::v1::system::{battery, cpu, disk, gpu, mem, network, proc, thermal};

mod batterywatch;
mod memwatch;

/// 全域采样周期（start 使用）
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(120);

/// 网卡速度回路采样周期（1Hz；db 侧 net 按 28800 轮 ≈ 8h 修剪）
pub const NET_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// 进程快照回路采样周期（5s；纯缓存喂 web，不落库）
pub const PROC_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// 测速源链（直读→su 桥→sysfs→netlink）连续整链失败多少拍后 stderr 示警
/// 一次（每条失败连串至多一条；成功一拍即清零）。缓存自失败首拍起就冻结，
/// 阈值只管声张、不管数据面。
const NET_FAIL_STREAK: u32 = 5;

/// 全域采样回路连续**失败轮**多少轮后 stderr 单行示警一次（[`FailStreak`]，
/// 每条连败串至多一条；成功轮一拍即清零）。失败轮 = 落库失败（`DbError`）
/// 或采集任务失败跳轮（`Ok(0)`，task-15：跳轮对采样计数是真实失败）；
/// 阈值只管声张、不管数据面——连败不拦采样，回路照常进下一轮。
const SAMPLER_FAIL_STREAK: u32 = 3;

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
    /// 网卡速度回路（每 [`NET_SAMPLE_INTERVAL`]）+ 进程快照回路（每
    /// [`PROC_SAMPLE_INTERVAL`]）并行跑在同一共享 runtime 上。
    /// 幂等：重复调用返回 [`OperatorError::Started`]。
    pub fn start() -> Result<(), OperatorError> {
        Self::start_with(Cadence::default())
    }

    /// [`Operator::start`] 的节奏/轮工作体参数化版（测试缝）：三回路改用注入
    /// 节奏、全域轮改用注入工作体，其余完全同构。start() 恒以
    /// [`Cadence::default`]（生产三常量 + 真 [`real_round`]）调用，生产语义不变。
    fn start_with(cadence: Cadence) -> Result<(), OperatorError> {
        if RUNNING.swap(true, Ordering::SeqCst) {
            return Err(OperatorError::Started);
        }
        stop_flag().store(false, Ordering::SeqCst);
        // 'settled' 通道：操作器线程 block_on 返回（= 三回路全部返回、RUNNING
        // 已由 run_sampler 清零）后发一枪；stop_with 据此硬上限等待关停完成。
        // 线程异常终止（sender 先 drop = Disconnected）同计 settled——已无可再等。
        let (tx, rx) = mpsc::channel::<()>();
        let h = std::thread::spawn(move || {
            let _ = db::rt().block_on(async {
                tokio::join!(
                    run_sampler(cadence.sample, cadence.round),
                    run_net_meter(cadence.net),
                    run_proc_meter(cadence.proc)
                )
            });
            let _ = tx.send(());
        });
        // handle 与 rx 双槽在 RUNNING=true 可见后即刻落位；极端早到的 stop()
        // 取到空槽计 Idle，回路仍会因 stop_flag 在轮次收尾处自退出。
        *handle_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
        *settle_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
        Ok(())
    }

    /// 请求停止 + 关停完成契约：置标志即时唤醒（微秒级回路退出、在途轮次完整
    /// 收尾）后，以 [`SHUTDOWN_JOIN_CAP`] 硬上限等待操作器线程 'settled'：
    /// - settle（或线程终止）= 关停完成，RUNNING 已清，返回即安全走后续关停步骤；
    /// - 超 cap = 单行 WARN 后放手（线程 detach，随 stop_flag 于轮次收尾自退出；
    ///   在途轮次被撕裂 = WAL 回滚安全）——卡死的回路不得把关停拖死（IS-3：
    ///   无外部 SIGKILL 计时兜底，这 10s 预算是关停的唯一界）。
    ///
    /// RUNNING 位语义不变；双次/并发 stop 只有一个 join 者，后来者走 Idle。
    /// settle 成功路径留单行 'operator settled' 确认（关停日志的 happy 侧凭证），
    /// Idle 静默（关停幂等，不为无谓轮次出声）。
    pub fn stop() {
        let t0 = Instant::now();
        match Self::stop_with(SHUTDOWN_JOIN_CAP, &warn_stderr) {
            Shutdown::Settled => {
                eprintln!(
                    "atlas: operator settled ({}ms)",
                    t0.elapsed().as_millis()
                );
            }
            Shutdown::TimedOut | Shutdown::Idle => {}
        }
    }

    /// [`Operator::stop`] 的 cap/warn 参数化版（测试缝：warn 闭包即 stderr 注入口）。
    fn stop_with(cap: Duration, warn: &dyn Fn(&str)) -> Shutdown {
        stop_flag().store(true, Ordering::SeqCst);
        stop_notify().notify_waiters();
        // 先取双槽再等待：JoinHandle 就此认领（settle 时线程已毕，超时则 detach，
        // 两段都不允许无界 join()，否则硬上限形同虚设）；rx 只此一个 join 者。
        let _handle = handle_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let rx = settle_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match rx {
            Some(rx) => join_settled(rx, cap, warn),
            None => Shutdown::Idle,
        }
    }

    /// 立即采一轮并落库（含 db 侧滚动修剪与内存告警策略），返回写入行数。
    /// 同步入口：驱动 db 共享 runtime（[`db::block_on`]），可在任意
    /// （非该 runtime 内部的）线程上下文调用。
    #[allow(dead_code)] // 预留（26 号决议：不加 HTTP 路由）: 手动补采入口，现仅 cfg(test)（operator 自测 / api 路由冒烟）消费
    pub(crate) fn sample_now() -> Result<usize, OperatorError> {
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

/// 全域轮工作体本体（async 结果装箱；fn 指针即注入缝）
type RoundBody = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<usize, OperatorError>> + Send>,
>;

/// 全域轮工厂（测试注轻量 stub，生产 = [`real_round`]）
type RoundFn = fn() -> RoundBody;

/// 生产轮：真 sample_round（collect → db 落库 → 双政策）
fn real_round() -> RoundBody {
    Box::pin(sample_round())
}

/// 三回路采样节奏 + 轮工作体（start 恒用 [`Cadence::default`]；测试注压缩拍
/// 与轻量轮 stub——真轮在产线库上单轮 ~6s，测试必须对 db 体量免疫且信号驱动）。
struct Cadence {
    sample: Duration,
    net: Duration,
    proc: Duration,
    round: RoundFn,
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            sample: SAMPLE_INTERVAL,
            net: NET_SAMPLE_INTERVAL,
            proc: PROC_SAMPLE_INTERVAL,
            round: real_round,
        }
    }
}

/// 关停 bounded join 的硬上限：关停唯一的界（无外部 SIGKILL 计时兜底）。
/// 置标志+唤醒后三回路于轮次收尾处退出（全域轮 ~3s 量级），10s 覆盖最坏轮次；
/// 到点放手 + WARN，卡死回路不得拖死关停。
const SHUTDOWN_JOIN_CAP: Duration = Duration::from_secs(10);

/// [`Operator::stop_with`] 的 bounded join 结果（私有观测面）
#[derive(Debug)]
enum Shutdown {
    /// 操作器线程已 settle（或已终止）：三回路全部返回，RUNNING 已清
    Settled,
    /// 硬上限到点未 settle：WARN 已发，线程已 detach（随标志于轮次收尾自退出）
    TimedOut,
    /// 无可 join 对象：未 start 过，或前次 stop 已认领槽位
    Idle,
}

/// WARN 默认出站：与网卡测速示警同系的 stderr 单行
fn warn_stderr(msg: &str) {
    eprintln!("atlas: WARN {msg}");
}

/// 操作器线程句柄槽（每次 start 重落位，stop 认领取走）
fn handle_slot() -> &'static Mutex<Option<JoinHandle<()>>> {
    static S: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// 'settled' 信号接收端槽（随 start 新建，stop 取走——双 stop 只有一个 join 者）
fn settle_slot() -> &'static Mutex<Option<mpsc::Receiver<()>>> {
    static S: OnceLock<Mutex<Option<mpsc::Receiver<()>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// 硬上限等待操作器线程 'settled'（纯通道逻辑，可脱离全局态直测）：
/// Ok = 正常 settle；Disconnected = 线程终止未及发枪（panic unwind 路径），
/// 同计 settled；Timeout = 发 WARN（含 cap 秒数与固定措辞）后由调用方放手。
fn join_settled(rx: mpsc::Receiver<()>, cap: Duration, warn: &dyn Fn(&str)) -> Shutdown {
    match rx.recv_timeout(cap) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => Shutdown::Settled,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            warn(&format!(
                "operator did not settle within {}s - proceeding",
                cap.as_secs_f64()
            ));
            Shutdown::TimedOut
        }
    }
}

/// 采样线程主体（跑在 db 共享 runtime 上）：首轮立即，此后每 `interval` 一轮、
/// 每轮经 [`sampler_tick`] 执行 `round` 工作体（生产 = [`SAMPLE_INTERVAL`] +
/// [`real_round`]，经 [`Operator::start_with`] 注入；成功轮落库+INFO；失败轮喂
/// [`FailStreak`]，连败满 [`SAMPLER_FAIL_STREAK`] 轮单行示警一次）。
/// 等待阶段 = sleep 与停止唤醒的 select 竞争；重进 select 前先在 loop 顶复查停止
/// 标志（堵工作段落地的丢唤醒，bug F8），置位即退出——stop() 后微秒级返回睡眠，
/// 在途轮次完整收尾。
async fn run_sampler(interval: Duration, round: RoundFn) {
    let mut streak = FailStreak::new(SAMPLER_FAIL_STREAK);
    sampler_tick(&mut streak, round).await;
    loop {
        // bug F8：stop 若落在工作段（select 未注册），notify_waiters 无人可唤——
        // 重进 select 前必须先在 loop 顶复查停止标志，否则丢唤醒、睡满整拍
        if stop_flag().load(Ordering::SeqCst) {
            break;
        }
        // enable() 在进 select 前即向 Notify 注册，杜绝“睡眠前 stop()”丢唤醒竞态
        let mut notified = std::pin::pin!(stop_notify().notified());
        let _ = notified.as_mut().enable();
        tokio::select! {
            _ = notified.as_mut() => { if stop_flag().load(Ordering::SeqCst) { break; } }
            _ = tokio::time::sleep(interval) => {}
        }
        if stop_flag().load(Ordering::SeqCst) {
            break;
        }
        sampler_tick(&mut streak, round).await;
    }
    RUNNING.store(false, Ordering::SeqCst);
}

/// 全域采样单轮执行 + 失败声张（task-15 / F11）：经注入的 `round` 工作体跑一轮
/// （生产 = [`real_round`] → [`sample_round`]）。成功轮（落库 >0 行）清零
/// [`FailStreak`]；采集任务失败跳轮（`Ok(0)`，[`spawn_collect`] 已逐轮示警）与
/// 落库失败（`Err(DbError)`）都喂连败 +1——恰满 [`SAMPLER_FAIL_STREAK`] 那一轮
/// stderr 单行示警（每条连败串一次、非每败一拍），随后照常采样，永不停摆。
/// 分类口径：`Ok(n>0)` = 成功；`Ok(0)` = 采集跳轮（真实失败，非静默非事件）；
/// `Err` = 存储失败（连败计数与两失败共喂，同 [`net_fail_tick`] 之于网卡回路）。
async fn sampler_tick(streak: &mut FailStreak, round: RoundFn) {
    let why = match round().await {
        Ok(n) if n > 0 => {
            streak.reset();
            return;
        }
        Ok(_) => "采集任务失败，本轮跳过".to_string(),
        Err(e) => e.to_string(),
    };
    if streak.note() {
        eprintln!("atlas: 全域采样连败 {SAMPLER_FAIL_STREAK} 轮: {why}");
    }
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
/// 快照上 [`tokio::task::spawn_blocking`]（su 子进程/sysfs/netlink 读链延迟
/// 不定，不占 1Hz 驱动线程，同 [`run_proc_meter`] 模式），每拍必 await 到底：
/// JoinHandle 不被 select 丢弃、无泄漏；停止落在等待阶段由 select 即时接住，
/// 落在工作段由 loop 顶复查在回到顶部那一拍接住（bug F8）。
/// 整拍失败——源链（[`network::read_dev_snapshot`] 起头，全灭才回 Err）回
/// `Err(NetError)`，或阻塞任务失败回 `Err(JoinError)`（panic/取消）——经
/// [`net_fail_tick`] 走**同一降级**：**缓存冻结**，stale 速率原样暴露、组级
/// 基线与 `prev_at` 不动（恢复后 dt 跨故障期摊薄），两类失败共喂 [`FailStreak`]
/// 计数、单行 stderr 示警——连串计数只在计满 [`NET_FAIL_STREAK`] 那一拍发声
/// （`note()` 天然 once-per-streak），成功一拍即重置。
async fn run_net_meter(tick: Duration) {
    let mut grouper = network::speed::Grouper::default();
    grouper.step(&network::speed::snapshot(), 0.0); // 首拍只立基线，产出不消费
    let mut prev_at = Instant::now();
    let mut fails = FailStreak::new(NET_FAIL_STREAK);
    loop {
        if stop_flag().load(Ordering::SeqCst) {
            break; // 见 run_sampler：loop 顶复查停止标志，堵工作段落地的丢唤醒
        }
        let mut notified = std::pin::pin!(stop_notify().notified());
        let _ = notified.as_mut().enable();
        tokio::select! {
            _ = notified.as_mut() => { if stop_flag().load(Ordering::SeqCst) { break; } }
            _ = tokio::time::sleep(tick) => {}
        }
        if stop_flag().load(Ordering::SeqCst) {
            break;
        }
        match tokio::task::spawn_blocking(network::speed::snapshot_result).await {
            Ok(Ok(cur)) => {
                fails.reset();
                let now = Instant::now();
                let dt = now.saturating_duration_since(prev_at).as_secs_f64();
                prev_at = now;
                let rates = grouper.step(&cur, dt);
                *net_cache().lock().unwrap_or_else(|e| e.into_inner()) = rates.clone();
                if !rates.is_empty() {
                    let _ = Db::insert(&net_records(&rates), DbWriteLane::acquire()).await;
                }
            }
            Ok(Err(e)) => {
                net_fail_tick(&mut fails, e);
            }
            Err(e) => {
                net_fail_tick(&mut fails, format!("测速任务异常: {e}"));
            }
        }
    }
}

/// 连败串计数器（两条采样回路各持一个实例、互不掺和）：失败拍/轮 `note()`
/// 计数 +1，恰在计满 `threshold` 那一拍返回**一次** `true`（每条连败串至多
/// 一次，超阈续败静默）；成功拍 [`reset`](Self::reset) 清零、重武装下一串。
/// 阈值只管声张、不管数据面：连败不改变任何回路的采样节奏。
#[derive(Debug)]
struct FailStreak {
    threshold: u32,
    count: u32,
}

impl FailStreak {
    const fn new(threshold: u32) -> Self {
        Self { threshold, count: 0 }
    }

    /// 记一次失败；恰在连败计满阈值那一拍返回 `true`（once-per-streak）。
    fn note(&mut self) -> bool {
        self.count += 1;
        self.count == self.threshold
    }

    /// 成功拍/轮：清零连败，重武装下一串的发声能力。
    fn reset(&mut self) {
        self.count = 0;
    }
}

/// 网卡回路单拍失败降级：喂 [`FailStreak`] 计数加一，恰在计满
/// [`NET_FAIL_STREAK`] 阈值那一拍单行 stderr 示警并回 `true`（每条连败串
/// 至多一次，成功一拍由调用侧 [`reset`](FailStreak::reset) 清零）。
/// 源链全灭 `Err(NetError)` 与 spawn_blocking 任务失败 `Err(JoinError)`
/// （panic/取消）共用本分支——**缓存冻结**语义由调用侧既定：缓存、组级基线
/// 与 `prev_at` 一概不动，stale 速率原样暴露。
fn net_fail_tick(fails: &mut FailStreak, why: impl core::fmt::Display) -> bool {
    if fails.note() {
        eprintln!("atlas: 网卡测速源链连败 {NET_FAIL_STREAK} 拍，速度示停于末次成功值: {why}");
        true
    } else {
        false
    }
}

/// 5s 进程快照回路主体（第三条回路，同一 runtime、同一停止唤醒）：每轮
/// [`proc::scan`] 上 [`tokio::task::spawn_blocking`]（/proc 批量小文件读），
/// 产物整体刷新 [`proc`] 缓存（5s 节奏，成功轮日志静默）。
/// **绝不碰 sqlite**（本域只观测不存储）；扫描任务失败（panic/cancel）
/// 缓存冻结——warming 或旧拍照旧暴露，下拍照常再试。等待阶段同为 sleep
/// 与停止 Notify 的 select 竞争（首拍 interval 即刻触发，预热窗 = 启动即扫）。
async fn run_proc_meter(tick_every: Duration) {
    let mut tick = tokio::time::interval(proc_pause(tick_every));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if stop_flag().load(Ordering::SeqCst) {
            break; // 见 run_sampler：loop 顶复查停止标志，堵工作段落地的丢唤醒
        }
        let mut notified = std::pin::pin!(stop_notify().notified());
        let _ = notified.as_mut().enable();
        tokio::select! {
            _ = notified.as_mut() => { if stop_flag().load(Ordering::SeqCst) { break; } }
            _ = tick.tick() => {}
        }
        if stop_flag().load(Ordering::SeqCst) {
            break;
        }
        match tokio::task::spawn_blocking(proc::scan).await {
            Ok(snap) => proc::update(snap),
            Err(e) => eprintln!("atlas: proc 扫描任务失败，缓存冻结: {e}"),
        }
        #[cfg(test)]
        work_hook_wait().await;
    }
}

/// 进程回路等待相位时长：生产恒为注入节拍（Cadence.proc → [`PROC_SAMPLE_INTERVAL`]）；
/// 测试缝（task-13）允许把节拍压短以在秒级墙钟内证伪丢唤醒。
fn proc_pause(default: Duration) -> Duration {
    #[cfg(test)]
    {
        let ms = TEST_PAUSE_MS.load(Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    default
}

/// 测试专用节拍压缩（task-13）：>0 = 以该毫秒数覆盖进程回路等待相位，
/// 0 = 生产节拍。
#[cfg(test)]
static TEST_PAUSE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 测试专用慢工作段（task-13）：装载后首个业务工作轮先向测试报
/// `entered`、再等 release 放行——令 stop() 恰好落在工作段中段。一次性，取走即失效。
#[cfg(test)]
static WORK_HOOK: Mutex<
    Option<(
        tokio::sync::mpsc::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
> = Mutex::new(None);

/// 工作段停驻点（task-13）：见 [`WORK_HOOK`]。未装载 = 空操作。
#[cfg(test)]
async fn work_hook_wait() {
    let hook = WORK_HOOK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some((tx, release)) = hook {
        let _ = tx.send(()).await;
        let _ = release.await;
    }
}

/// db 写道令牌（audit task-25，GAP-7/A-07）：`Db::insert` 的调用凭证，编译期把
/// 「操作器是唯一写者」钉死——令牌类型 pub(crate) 全域可见，但唯一铸造口
/// [`DbWriteLane::acquire`] 仅 operator 内可见，api/其他域既无令牌也无法自造
/// （私有字段 → 结构体字面量 E0451；直调铸造口 → E0624，item-private 家族的
/// 关联函数变体）。
/// 不用方法本体 `pub(in crate::v1::operator)`：db 与 operator 平级，precise
/// visibility 只能指向祖先模块（E0742，编译实证见 evidence task-25）。
#[derive(Clone, Copy)]
pub(crate) struct DbWriteLane {
    _sealed: (),
}

impl DbWriteLane {
    /// 唯一铸造口：operator 车道专属。
    pub(in crate::v1::operator) const fn acquire() -> Self {
        Self { _sealed: () }
    }
}

/// 测试缝（audit task-25）：`Db::insert` 的调用凭证收归 operator 后，operator 之外
/// 的测试（db 的常驻连接探针）经本函数转发调用唯一写入口——编译契约不因测试而
/// 放宽，生产构建下本缝不存在。
#[cfg(test)]
pub(crate) async fn db_insert_for_tests(records: &[Record]) -> Result<usize, DbError> {
    Db::insert(records, DbWriteLane::acquire()).await
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
                    kind: kind::NET.into(),
                    key: format!("{group}.rx_bps"),
                    value: *rx,
                },
                Record {
                    ts: ts.clone(),
                    ts_secs,
                    kind: kind::NET.into(),
                    key: format!("{group}.tx_bps"),
                    value: *tx,
                },
            ]
        })
        .collect()
}

/// 一轮采样流水：collect（经 [`spawn_collect`] 上阻塞池，不占驱动线程）→
/// db 落库（被动存储）→ 内存告警政策（政策本体在 [`memwatch`]，出站走
/// [`notify::notify`] / [`notify::vibrate`] 默认通道）→ 充电提醒政策（政策本体在
/// [`batterywatch`]，出站走 [`notify::notify`]，不震动）→ 单行 INFO（落库行数 +
/// 全程耗时 ms）。落库失败也早退——告警只对已成功落库的轮次负责。
/// 采集任务失败（panic/取消 → JoinError）示警后本轮跳过（Ok(0)）——与旧
/// `let _ =` 容忍同格，回路照常进入下一轮。停止唤醒落在途轮次时不掐阻塞任务：
/// 每轮必 await 到底（同 [`run_net_meter`]，在途轮次完整收尾）。
async fn sample_round() -> Result<usize, OperatorError> {
    let t0 = Instant::now();
    let Some(records) = spawn_collect(collect).await else {
        return Ok(0); // 采集任务失败已示警，本轮跳过
    };
    let n = Db::insert(&records, DbWriteLane::acquire()).await?;
    memwatch::policy(&records, notify::notify, notify::vibrate);
    batterywatch::policy(&records, notify::notify);
    eprintln!(
        "atlas: 全域采样轮落库 {n} 行，耗时 {}ms",
        t0.elapsed().as_millis()
    );
    Ok(n)
}

/// 全域采集上 [`tokio::task::spawn_blocking`]：sync 采集链（sysfs/su 子进程/
/// proc 读，延迟不定）不占 120s 驱动线程，await 期间 1Hz 网卡回路照常出拍。
/// 任务失败（panic/取消 → JoinError）单行 stderr 示警并回 `None`（本轮跳过），
/// 驱动回路不受污染、下一轮照常。
async fn spawn_collect(
    job: impl FnOnce() -> Vec<Record> + Send + 'static,
) -> Option<Vec<Record>> {
    match tokio::task::spawn_blocking(job).await {
        Ok(records) => Some(records),
        Err(e) => {
            eprintln!("atlas: 全域采集任务失败，本轮跳过: {e}");
            None
        }
    }
}

/// 一轮全量采集：system 各黑箱 → 窄长存储行。
/// 采集时刻单点打戳：全行共享同一 RFC3339 `ts`（秒精度、UTC）与同源 `ts_secs`。
/// gpu 行（load_pct/freq_mhz）仅源链可用时写入、每个 Some 值一行（freq_mhz 是锚，
/// gpubusy 差分首拍 load_pct 缺席）；无 root 设备整域静默缺席，不计失败不告警。
fn collect() -> Vec<Record> {
    let now = Utc::now();
    let ts = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let ts_secs = now.timestamp();
    let mut out = Vec::new();
    {
        // 参数名 `k` 而非 `kind`：让出名字空间给 kind 登记表（audit task-23），
        // 调用点 `kind::XXX` 恒为模块路径，无歧义。
        let mut push = |k: &str, key: String, value: f64| {
            out.push(Record {
                ts: ts.clone(),
                ts_secs,
                kind: k.to_string(),
                key,
                value,
            })
        };

        for (i, ghz) in cpu::freq_ghz().iter().enumerate() {
            push(kind::CPU_FREQ, format!("cpu{i}"), *ghz as f64);
        }

        let t = thermal::Temp::get();
        // temp 行类别名与域名同词（temp key = `<类别>.{i}`，前端契约）：同名两类
        // 复用 kind 常量防漂移；cpu_core/camera/pmic 非 kind 词，保持字面量。
        for (cat, group) in [
            ("cpu_core", &t.cpu_core),
            (kind::GPU, &t.gpu),
            ("camera", &t.camera),
            ("pmic", &t.pmic),
            (kind::BATTERY, &t.battery),
        ] {
            for (i, (v, _name)) in group.iter().enumerate() {
                push(kind::TEMP, format!("{cat}.{i}"), *v as f64);
            }
        }

        // GPU 频率/使用率（kind=gpu）：源链可用才写行、每个 Some 一行
        // （gpubusy 差分首拍 load_pct 可为 None，freq_mhz 是锚）；无 root 静默缺席不告警
        let g = gpu::Gpu::get();
        if g.available {
            if let Some(p) = g.load_pct {
                push(kind::GPU, "load_pct".into(), p);
            }
            if let Some(f) = g.freq_mhz {
                push(kind::GPU, "freq_mhz".into(), f);
            }
        }

        let m = mem::Mem::get();
        push(kind::MEM, "total".into(), m.total_mem_mb as f64);
        push(kind::MEM, "used".into(), m.used_mem_mb as f64);
        push(kind::MEM, "avail".into(), m.avail_mem_mb as f64);
        push(kind::SWAP, "total".into(), m.total_swap_mb as f64);
        push(kind::SWAP, "used".into(), m.used_swap_mb as f64);

        let b = battery::Battery::get();
        push(kind::BATTERY, "pct".into(), b.percentage as f64);
        push(kind::BATTERY, "volt_mv".into(), b.voltage as f64);
        push(kind::BATTERY, "current_ua".into(), b.current as f64); // CLI 值为 µA（负=放电）
        // 沿用 CLI 温度源；迁移 thermal::Temp.battery 属 system 侧改造，不在操作器内进行
        #[expect(
            deprecated,
            reason = "Battery::temperature 已弃用但迁移属 system 侧范围"
        )]
        push(kind::BATTERY, "temp_c".into(), b.temperature); // CLI 值已是 °C，原样入库
        push(kind::BATTERY, "cycle".into(), b.cycle as f64);
        push(
            kind::BATTERY,
            "plugged".into(),
            (!b.plugged.is_empty() && b.plugged != "None") as u8 as f64,
        );
        push(kind::BATTERY, "present".into(), b.present as u8 as f64);

        for d in disk::Disk::get() {
            push(kind::DISK, format!("{}|used", d.mount_point), d.used_mb as f64);
            push(
                kind::DISK,
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
        for k in [
            kind::CPU_FREQ,
            kind::TEMP,
            kind::MEM,
            kind::SWAP,
            kind::BATTERY,
            kind::DISK,
        ] {
            assert!(v.iter().any(|r| r.kind == k), "缺 {k} 采样");
        }
        // 登记表穷尽（audit task-23 结构锁）：collect 产出的每个 kind 必须是
        // kind 登记表在册值（gpu 允许缺席——无 root 静默；但不允许在册之外的 kind）。
        const REGISTERED: [&str; 8] = [
            kind::CPU_FREQ,
            kind::TEMP,
            kind::GPU,
            kind::MEM,
            kind::SWAP,
            kind::BATTERY,
            kind::DISK,
            kind::NET,
        ];
        for r in &v {
            assert!(
                REGISTERED.contains(&r.kind.as_str()),
                "collect 产出了未注册 kind: {:?}",
                r.kind
            );
        }
        // 全轮同一采集时刻：ts 必须是可解析 RFC3339 且与 ts_secs 投影自洽
        assert!(v.iter().all(|r| r.ts_secs > 0));
        assert!(v.iter().all(|r| {
            DateTime::parse_from_rfc3339(&r.ts)
                .map(|d| d.timestamp() == r.ts_secs)
                .unwrap_or(false)
        }));
    }

    /// 触碰操作器全局槽位/缓存的测试共锁：真实回路的 start/stop 测试会喂
    /// net_cache，必须与本文件的缓存断言串行（否则并行窗口误报"非空"）
    fn operator_test_lock() -> &'static std::sync::Mutex<()> {
        static S: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        S.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// 缓存契约：1Hz 回路未启动 → 读到空（web 冷启动回 "{}" 的依据）
    #[test]
    fn net_speeds_empty_before_first_emit() {
        let _g = operator_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
                .all(|r| r.kind == kind::NET && r.ts == v[0].ts && r.ts_secs > 0)
        );
        assert_eq!(v[0].key, "wifi.rx_bps");
        assert_eq!(v[1].key, "wifi.tx_bps");
        assert_eq!((v[0].value, v[1].value), (1.5, 2.5));
        assert_eq!(v[2].key, "cellular.rx_bps");
        assert_eq!(v[3].key, "cellular.tx_bps");
        assert!(DateTime::parse_from_rfc3339(&v[0].ts).is_ok());
    }

    /// 两类失败路共用同一计数器（misleading-success 反证）：源链 `NetError`
    /// 拍与真实 `JoinError` 拍（spawn_blocking panic 子任务）混排仍逐拍 +1，
    /// 连续到 [`NET_FAIL_STREAK`] 照示警——JoinError 不会把失败吞成静默。
    /// task-11（快照上 spawn_blocking）的降级等价性锁。task-15 起连败计数由
    /// [`FailStreak`] 持有，发声拍由返回值唯一定位（发声计数 = 恰好一次）。
    #[tokio::test]
    async fn net_fail_tick_counts_join_error_like_net_error() {
        let mut fails = FailStreak::new(NET_FAIL_STREAK);
        assert!(!net_fail_tick(&mut fails, network::speed::NetError::NoSu));
        assert!(!net_fail_tick(&mut fails, network::speed::NetError::Timeout));
        assert_eq!(fails.count, 2, "源链失败拍逐拍计数");
        let join = tokio::task::spawn_blocking(|| panic!("强制慢源炸裂（task-11 故障注入）")).await;
        let e = join.expect_err("panic 的阻塞任务必须回 Err(JoinError)");
        assert!(!net_fail_tick(&mut fails, format!("测速任务异常: {e}")));
        assert_eq!(fails.count, 3, "JoinError 拍与 NetError 拍喂同一计数器");
        assert!(!net_fail_tick(&mut fails, "注入拍 4"));
        assert!(net_fail_tick(&mut fails, "注入拍 5"), "恰在阈值拍发声一次");
        assert_eq!(fails.count, NET_FAIL_STREAK, "混排连败应同样抵达示警阈值");
    }

    /// FailStreak 声张语义（task-15 / F11）：阈值 3——前两连败静默，第 3 连败
    /// 恰发声**一次**，第 4~6 拍（持续性失败）继续静默：每条连败串一条示警、
    /// 非每失败一拍一条；计数照常递增（发声与否不改计数）。
    #[test]
    fn failstreak_announces_once_per_streak() {
        let mut s = FailStreak::new(3);
        assert!(!s.note(), "连败第 1 拍静默");
        assert!(!s.note(), "连败第 2 拍静默");
        assert!(s.note(), "第 3 连败恰发声一次");
        assert!(!s.note());
        assert!(!s.note());
        assert!(!s.note());
        assert_eq!(s.count, 6, "6 连败仍只发声一次");
    }

    /// stale_state 锁：成功轮 `reset()` 清零重武装——fail2 + 成功 + fail2 全程
    /// 静默（计数不跨成功串味）；新串第 3 败再次发声（发声能力随 reset 恢复，
    /// 非一次性）。
    #[test]
    fn failstreak_reset_rearms_next_streak() {
        let mut s = FailStreak::new(3);
        assert!(!s.note());
        assert!(!s.note());
        s.reset();
        assert_eq!(s.count, 0, "成功轮后计数必须清零");
        assert!(!s.note());
        assert!(!s.note(), "fail2+成功+fail2 全程静默");
        assert!(s.note(), "重置后的新一轮第 3 败须再次发声（重武装）");
    }

    /// JoinError 臂反证（misleading-success 锁）：真 panic 的阻塞任务经
    /// [`spawn_collect`]（[`sample_round`] 的采集步）回 `None` = 本轮跳过；
    /// 紧接着的正常轮照常产出 `Some`——示警不污染回路，下一轮不受牵连。
    /// task-12（采集上 spawn_blocking）的降级等价性锁。
    #[tokio::test]
    async fn spawn_collect_skips_round_on_join_error_and_recovers() {
        let skipped = spawn_collect(|| panic!("强制采集炸裂（task-12 故障注入）")).await;
        assert!(
            skipped.is_none(),
            "panic 的采集任务必须回 None（示警+本轮跳过）"
        );
        let rec = Record {
            ts: "2026-01-01T00:00:00+00:00".into(),
            ts_secs: 1,
            kind: kind::MEM.into(),
            key: "used".into(),
            value: 1.0,
        };
        let next = spawn_collect(|| vec![rec]).await;
        assert!(
            matches!(&next, Some(v) if v.len() == 1 && v[0].kind == kind::MEM),
            "故障轮之后的正常轮必须照常产出"
        );
    }

    /// 丢唤醒锁（bug F8）：stop 落在**工作段中段**时，唤醒通道尚无已注册
    /// 等待者、`notify_waiters` 空转——回路必须靠 loop 顶部的停止复查在回到
    /// 顶部那一轮立即退出，而非睡满整拍节拍。测试跑真 [`run_proc_meter`]：
    /// 节拍经 [`TEST_PAUSE_MS`] 压到 5s（丢唤醒的代价即整拍），工作段经
    /// [`WORK_HOOK`] 设一次性停驻；报入 → stop → 放行，回路须在 1s（远短于
    /// 节拍）内收尾。全程信号同步，无墙钟猜测睡眠。
    #[tokio::test]
    async fn stops_mid_work_segment() {
        use tokio::sync::{mpsc, oneshot};
        // w3-gate [18]：本用例复位全局 stop 标志并调用 Operator::stop()（W3 起
        // 认领 settle 槽 + bounded join），与持锁的 stop_* 系列并行即互相串扰
        // （标志被对方清零 → wedge 用例的 detached 线程永不收尾）。共锁串行化；
        // guard 跨 await 安全：本 runtime 内只等待自发任务的通道/句柄，持锁方
        // 均不同步等待本 runtime（std::sync::Mutex 仅约束跨线程串行）。
        let _g = operator_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        stop_flag().store(false, Ordering::SeqCst);
        TEST_PAUSE_MS.store(5_000, Ordering::SeqCst);
        let (entered_tx, mut entered_rx) = mpsc::channel::<()>(1);
        let (release_tx, release_rx) = oneshot::channel::<()>();
        *WORK_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some((entered_tx, release_rx));
        let h = tokio::spawn(run_proc_meter(PROC_SAMPLE_INTERVAL));
        // When: drive the loop into its first work segment, stop mid-work
        entered_rx
            .recv()
            .await
            .expect("proc 回路首个工作轮必须执行并报入");
        let t0 = Instant::now();
        Operator::stop(); // 此刻 select 未注册（工作段中），notify_waiters 无人可唤
        let _ = release_tx.send(()); // 放行工作段，回路回到 loop 顶部
        let settled = tokio::time::timeout(Duration::from_millis(1_000), h).await;
        let elapsed = t0.elapsed();
        // 拆除测试缝，勿把全局态漏给并发用例
        TEST_PAUSE_MS.store(0, Ordering::SeqCst);
        stop_flag().store(false, Ordering::SeqCst);
        // Then: 未等满 5s 节拍即退出
        assert!(
            matches!(settled, Ok(Ok(()))),
            "stop 落工作段后回路须在 1s 内退出而非等满 5s 节拍（未退出或任务异常）"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "退出墙钟应远短于节拍，实际 {elapsed:?}"
        );
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
        let hist = Db::history(kind::MEM, Some("used"), 5);
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

    // ---- task 18: stop() 完成契约（bounded join，IS-3 硬预算）----

    /// 轻量轮 stub：80ms 让出即"落库"（生产真轮在产线库上 ~6s，测试必须对
    /// db 体量免疫——完成契约测的是关停协议，不是采集/存储性能）
    fn stub_round_fast() -> RoundBody {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok(0)
        })
    }

    /// 卡死轮 stub（超时路径注入件）：1.2s 工作段模拟"回环内不可打断的轮次"，
    /// 远大于注入 cap——验证到点放手后，detached 线程仍会随标志自行收尾。
    fn stub_round_wedge() -> RoundBody {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            Ok(0)
        })
    }

    /// 完成契约（GREEN 核心）：压缩节奏 + 轻量轮 start → 首轮全域轮在途时 stop →
    /// stop 直到操作器线程 'settled' 才返回；返回点 RUNNING 已清、WARN 静默。
    /// 信号驱动无真 10s：settle 预算 = 轻量轮 + 一拍压缩节奏（本分支含 W2 环顶
    /// 复查，丢唤醒兜底即节奏 300ms 上限，settle 只会更早），测试墙钟 < 3s。
    #[test]
    fn stop_joins_within_round() {
        use std::sync::Arc;
        let _g = operator_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let warns: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let w = Arc::clone(&warns);
        let warn = |m: &str| w.lock().unwrap_or_else(|e| e.into_inner()).push(m.to_string());
        let t0 = Instant::now();
        Operator::start_with(Cadence {
            sample: Duration::from_millis(300),
            net: Duration::from_millis(300),
            proc: Duration::from_millis(300),
            round: stub_round_fast,
        })
        .expect("压缩节奏 start 应成功");
        // stop 进入点：start 即跑的首轮全域轮正在途（work 段中 stop）
        match Operator::stop_with(Duration::from_secs(5), &warn) {
            Shutdown::Settled => {}
            other => panic!("应在 cap 内 settle，实际 {other:?}"),
        }
        let el = t0.elapsed();
        assert!(
            warns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "settle 路径 WARN 缝必须静默"
        );
        assert!(
            !RUNNING.load(Ordering::SeqCst),
            "'settled' = 线程已退出，RUNNING 必须已清"
        );
        assert!(
            el < Duration::from_secs(3),
            "join 应在轮次收尾处完成，实际 {el:?}"
        );
        assert!(
            Operator::net_speeds().is_empty(),
            "压缩节奏窗口太短，net 回路不得完成首个计量轮（否则缓存断言会互污）"
        );
    }

    /// 机件级超时路径（QA failure 场景）：注入 1.2s 卡死轮 + 250ms cap →
    /// stop 到点返回 TimedOut、WARN 恰一行；随后 detached 线程仍随 stop_flag
    /// 于轮次收尾自退出（RUNNING 于 3s 内清零）——卡死轮不拖死关停（IS-3）。
    #[test]
    fn stop_wedged_round_times_out_then_self_settles() {
        use std::sync::Arc;
        let _g = operator_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let warns: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let w = Arc::clone(&warns);
        let warn = |m: &str| w.lock().unwrap_or_else(|e| e.into_inner()).push(m.to_string());
        Operator::start_with(Cadence {
            sample: Duration::from_millis(300),
            net: Duration::from_millis(300),
            proc: Duration::from_millis(300),
            round: stub_round_wedge,
        })
        .expect("压缩节奏 start 应成功");
        match Operator::stop_with(Duration::from_millis(250), &warn) {
            Shutdown::TimedOut => {}
            other => panic!("卡死轮 + 250ms cap 必须 TimedOut，实际 {other:?}"),
        }
        let g = warns.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(g.len(), 1, "超时 WARN 恰一行，实际 {g:?}");
        assert!(
            g[0].contains("operator did not settle") && g[0].contains(" - proceeding"),
            "WARN 措辞不符合同: {}",
            g[0]
        );
        drop(g);
        // 放手不等于遗忘：detached 线程必须自行收尾（在途轮撕开后随标志退出）
        let t0 = Instant::now();
        while RUNNING.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !RUNNING.load(Ordering::SeqCst),
            "detached 操作器线程必须于轮次收尾处自退出（3s 内）"
        );
    }

    /// 超时路径（卡死轮次模拟：sender 永不发）：cap 到点返回 TimedOut，
    /// WARN 恰一行、含合同措辞；墙钟 ≈ cap（信号驱动，无真 10s）。
    #[test]
    fn stop_timeout_warns_once_and_returns() {
        use std::sync::{Arc, mpsc};
        let (tx, rx) = mpsc::channel::<()>();
        let warns: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let w = Arc::clone(&warns);
        let warn = |m: &str| w.lock().unwrap_or_else(|e| e.into_inner()).push(m.to_string());
        let t0 = Instant::now();
        match join_settled(rx, Duration::from_millis(200), &warn) {
            Shutdown::TimedOut => {}
            other => panic!("无 'settled' 信号必须报 TimedOut，实际 {other:?}"),
        }
        let el = t0.elapsed();
        assert!(
            el < Duration::from_secs(1),
            "超时 cap 必须生效，实际 {el:?}"
        );
        let g = warns.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(g.len(), 1, "WARN 恰一行，实际 {g:?}");
        assert!(
            g[0].contains("operator did not settle") && g[0].contains(" - proceeding"),
            "WARN 措辞不符合同: {}",
            g[0]
        );
        drop(tx); // 模拟卡死线程随测试退场
    }

    /// 无可 join 对象：从未 start（或前次 stop 已取走槽位）→ Idle、静默、即时返回；
    /// 事后复位 stop 标志，不污染其余测试。
    #[test]
    fn stop_without_join_target_is_idle() {
        use std::sync::Arc;
        let _g = operator_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let warns: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let w = Arc::clone(&warns);
        let warn = |m: &str| w.lock().unwrap_or_else(|e| e.into_inner()).push(m.to_string());
        let t0 = Instant::now();
        match Operator::stop_with(Duration::from_millis(200), &warn) {
            Shutdown::Idle => {}
            other => panic!("无可 join 对象必须 Idle，实际 {other:?}"),
        }
        assert!(
            t0.elapsed() < Duration::from_millis(100),
            "Idle 必须即时返回"
        );
        assert!(
            warns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "Idle 路径不得 WARN"
        );
        stop_flag().store(false, Ordering::SeqCst);
    }
}
