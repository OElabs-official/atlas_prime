//! v1/db: sqlite 被动存储黑箱（sqlx / runtime-tokio；自带单一共享 runtime，不依赖外层）。
//!
//! 本模块**只负责 open/schema（含旧库迁移）/insert/history/滚动修剪**，从不自主
//! 产生数据——周期产数据的唯一写入方是 [`crate::v1::operator`]（操作器，程序内唯一
//! 自主驱动数据的模块），其余皆为请求驱动的读路径。
//!
//! # 公开接口用法
//! ```ignore
//! use crate::v1::db::Db;
//!
//! let hist = Db::history("temp", Some("battery.0"), 200);   // 最近 200 条电池温度（新→旧）
//! let n = Db::rows().ok();                                   // 存量行数（观察 MAX_ROUNDS 上限用）
//! ```
//!
//! # 存储模型
//! 单表窄长项 `records(ts TEXT, kind TEXT, key TEXT, value REAL)`（rowid 隐式 = 插入序；
//! 旧 `samples` 表的 `id` 列在新 schema 取消，history 排序以 rowid 落地，新→旧语义不变）：
//! - `ts` = RFC3339（秒精度、UTC，如 `2023-11-14T22:13:20Z`），由操作器采集时打戳；
//! - `cpu_freq` key=`cpu{i}`（GHz）；`temp` key=`{类别}.{i}`（℃，类别=cpu_core/gpu/camera/pmic/battery）；
//! - `mem` key=total/used/avail、`swap` key=total/used（MB）；
//! - `battery` key=pct/volt_mv/current_ua/temp_c/cycle/plugged/present；
//! - `disk` key=`{挂载点}|used` / `{挂载点}|total`（MB）。
//!
//! 滚动上限按 **kind 分档**（轮粒度：一轮 = 同一 ts 的全部样本行；RFC3339 统一
//! 格式 ⇒ 字典序即时间序，修剪恒整轮丢弃）：
//! - `net`（网卡速度，操作器 1Hz 回路写入）：[`NET_MAX_ROUNDS`] = 28800 轮 ≈ 8h 回溯；
//!   key 为**分组级** `<组>.rx_bps` / `<组>.tx_bps`（组 = `wifi` / `cellular` /
//!   有流量的其他网卡名，见 [`crate::v1::system::network::speed`]）；2026-10-20.28
//!   起的分组化改造令旧 per-iface 键（`rmnet_data3.rx_bps` 等）退役——不迁移、
//!   不改写，随 ≈8h 滚动窗口自然出清；
//! - 其余 kind（2 分钟/轮）：[`MAX_ROUNDS`] = 20000 轮 ≈ 27.7 天回溯。
//!
//! net 与非 net 各自只在本范围内数轮、互不混算（否则 1Hz 的海量 ts 会把 120s 类
//! 的窗口压没）。修剪 cutoff **不再**走每轮 `DISTINCT ts … LIMIT 1 OFFSET N` 子查询
//! 扫描（audit GAP-2 task-17：SCOPE_OTHER 一腿每次全索引 walk 0.65–0.91s，纯扫描税）：
//! 每个范围在**常驻锁内**维护一条轮首 ts 环形队列（[`ScopeTrim`]，与常驻连接同一把
//! 锁，无第二把锁）。该范围首轮落库时以**全量列表查询**引导（`SELECT DISTINCT
//! CAST(strftime('%s',ts) AS INTEGER) … ORDER BY ts DESC LIMIT window+1`，进程内
//! 一次性；`window+1` 即激活位——超限库重启后首轮即收敛）；此后每轮推入本轮 ts
//! （相邻同秒去重），队列长度 > window 时取 `queue[window-1]`（第 window 新轮）为
//! cutoff，发 `DELETE … AND ts < strftime('%Y-%m-%dT%H:%M:%SZ', ?cutoff_secs,
//! 'unixepoch')`——cutoff 秒数**必须经右侧 strftime 生成 RFC3339 文本比对**：ts 为
//! TEXT 列，裸 i64 直接比对触发 affinity 转换（'1731…' vs '2026…'），DELETE 静默
//! 永不命中、表无界增长；右侧表达式不包裹列 ⇒ 索引范围扫描（`idx_records_kind_ts`
//! /`idx_records_ts`）保留。删成后弹出队尾所有 < cutoff 的项。net 范围每
//! [`NET_TRIM_TICKS`] 个 net 插入轮才删一次（可观测窗口 8h±60s，计划已锁；入队仍
//! 每轮），其余范围每轮一次。冷空库/未满窗不是失败：trim 仅在队列长度 > window 后
//! 激活；引导 SQL 真错则该范围停用修剪并记 WARN（行数临时超窗，宁多删勿误删）。
//! 硬守卫：**绝不在空/未引导队列上算 cutoff**。
//!
//! # schema 迁移（幂等；生产路径仅常驻连接 init 时执行一次，测试缝 `open_at` 逐次
//! 执行、二次 open = no-op）
//! `pragma_table_info` 查 `records.ts` 声明类型：TEXT → 新 schema，什么都不做；
//! `records` 缺失而旧 `samples`（ts=unix 秒 INTEGER）在 → 单事务内
//! `records_new`(TEXT) ← `strftime('%Y-%m-%dT%H:%M:%SZ', ts, 'unixepoch')` 转写复制
//! （ORDER BY rowid 保历史插入序，rowid 单调延续）→ DROP 旧表 → RENAME；
//! 事务失败整体回滚 = 未迁移，旧行必不丢。两表皆缺 → 全新库就地建新。
//! 旧 `idx_samples_*` 随 DROP 消失，等价的 `idx_records_*` 随后重建。
//!
//! # 一次性电池行迁移（`PRAGMA user_version` 守卫，幂等）
//! `user_version < 2` 时**单事务**内：`battery/current_ma`（值本就是 µA）改名
//! `current_ua`；`battery/temp_c`（旧写入侧 ÷10 存储）×10 还原真实 ℃；随后置
//! `user_version = 2`（版本只升不降）。标记在库头部页而非数据行——滚动修剪吃不掉、
//! 重启不重放。仅当 `records` 表确保存在后执行：新 schema 直接执行；遗留路径在
//! 重建/改名完成后执行；全新库直接盖章（建表交给 `open_at`）——冷启动空库绝不
//! 触发 `no such table`。三条语句与版本盖章同一提交/回滚单元（PRAGMA 写头部页随
//! 事务回滚），崩溃后重开重跑恒安全。
//!
//! # 读取契约
//! `/db/history` 只经 [`Db::history`] 暴露 unix 秒投影：SELECT 内
//! `CAST(strftime('%s', ts) AS INTEGER)`（迁移后全行均为 RFC3339 TEXT，strftime 统一可析）。
//!
//! # 写入与并发模型
//! 全库共用**一个**常驻 current-thread runtime（同步入口与操作器采样线程同驱动，
//! 进程内只建一次）+ **一个**常驻连接（`static CONN`，audit GAP-2：消灭每操作
//! open/close 的 openat/系统调用风暴）：首次读写经 `open_at` 建立，连上即执行
//! `PRAGMA journal_mode=WAL; synchronous=NORMAL; busy_timeout=5000`
//! ——Android F2FS fsync 昂贵，WAL+NORMAL 显著压降写盘抖动，busy_timeout 抗并发锁竞争。
//! PRAGMA/迁移/建表/建索引**只在 init 执行这一次**（journal_mode/user_version 持久于
//! 库文件头，重启天然免疫）。此后所有读写经 `tokio::sync::Mutex`（**必须** tokio 版：
//! 单一驱动 runtime 上 std guard 跨 await 会阻塞同线程 sqlite driver 任务 = 自死锁）
//! 独占复用。每批落库后至多每 [`CHECKPOINT_INTERVAL`]=120s 发一条
//! `PRAGMA wal_checkpoint(TRUNCATE)`（db 模块内 Instant 守卫）：WAL 在两次截断之间
//! 增长、截断处归零（对照旧基线的每操作 checkpoint 锯齿）；被外部读事务卡住返回
//! BUSY 时**记 WARN 跳过、绝不令本轮失败**，下一轮重试。
//! 一轮 = 单事务内若干条 **多值 INSERT**（每片 ≤200 行 = ≤800 绑定，低于
//! SQLITE_MAX_VARIABLE_NUMBER=999），提交后经 [`post_insert`] 按范围入队并按节奏修剪
//! （[`ScopeTrim`] 两条环形队列 + net 60 轮计数）。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqliteRow};
use sqlx::{Connection, Row};

use crate::v1::prelude;

/// 非 net kind 的滚动上限（采样轮数；一轮 ≈ 百行，2 分钟/轮 ≈ 27.7 天）
pub const MAX_ROUNDS: i64 = 20_000;

/// `net`（网卡速度，1Hz）的滚动上限（轮数 = 秒数；28800 ≈ 8h 回溯）
pub const NET_MAX_ROUNDS: i64 = 28_800;

/// 修剪范围 SQL 片段：net 专属 / 其余全部（两条固定 prepared 语句路径）
const SCOPE_NET: &str = "kind = 'net'";
const SCOPE_OTHER: &str = "kind <> 'net'";

/// 单条多值 INSERT 的最大行数（4 列 × 200 = 800 绑定 < SQLITE_MAX_VARIABLE_NUMBER=999）
const INSERT_CHUNK: usize = 200;

/// 手动 WAL 截断检查点的最小间隔（audit GAP-2）：常驻连接下 wal 在两截断之间增长，
/// 每批落库后经 [`checkpoint_paced`] 至多每此间隔发一条 TRUNCATE；BUSY 记 WARN 跳过、
/// 守卫不推进，下一轮重试。取代旧「每操作 open/close 各触发一次自动 checkpoint」的锯齿。
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(120);

/// net 范围修剪节奏（task-17）：每 N 个**含 net 行的插入轮**才发一条 SCOPE_NET
/// DELETE（入队每轮照做）。1Hz 下 ≈ 每 60s 一删，可观测窗口 8h±60s（计划锁定）；
/// 120s 大轮内随行写入的 net 行也计一个 tick。
const NET_TRIM_TICKS: u32 = 60;

/// records 列定义（全新建表与迁移重建 records_new 共用同一事实源）
const RECORDS_COLUMNS: &str =
    "ts TEXT NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL, value REAL NOT NULL";

/// 一条历史样本（存储行类型：操作器写入，[`Db::history`] 读回）
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    /// RFC3339（秒精度、UTC）。写入侧：入库 TEXT 列原值；读取侧：列原值回读。
    pub ts: String,
    /// Unix 秒。写入侧：与 `ts` 同一时刻；读取侧：SQL 投影
    /// `CAST(strftime('%s', ts) AS INTEGER)`（web 层 `/db/history` 的 `t` 即此字段）。
    pub ts_secs: i64,
    pub kind: String,
    pub key: String,
    pub value: f64,
}

/// 失败原因（Open=库文件/schema/迁移环境，Sql=SQL 执行）
#[derive(Debug, Clone)]
pub enum DbError {
    Open(String),
    Sql(String),
}

impl core::fmt::Display for DbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DbError::Open(m) => write!(f, "db open/schema failed: {m}"),
            DbError::Sql(m) => write!(f, "db sql failed: {m}"),
        }
    }
}
impl std::error::Error for DbError {}

/// 唯一公开入口（被动存储：读写皆由调用方发起）
pub struct Db;

impl Db {
    /// 写入一轮记录（常驻连接：单事务多值插入 → 入队+按节奏滚动修剪 → 节流 WAL
    /// 截断）。唯一调用方 = 操作器。
    pub(crate) async fn insert(records: &[Record]) -> Result<usize, DbError> {
        let cell = shared_conn().await?;
        let mut core = cell.lock().await;
        let n = run_insert(&mut core.conn, records).await?;
        post_insert(&mut core, records).await?;
        // best-effort（锁定决策 iii）：checkpoint 失败只 WARN，绝不令本轮失败
        checkpoint_paced(&mut core.conn).await;
        Ok(n)
    }

    /// 最近历史（新→旧）。key=None 时聚合该 kind 全部键。
    /// `ts_secs` = unix 秒 SQL 投影；调用方自行升序排序。
    pub fn history(kind: &str, key: Option<&str>, limit: u32) -> Vec<Record> {
        let kind = kind.to_string();
        let key = key.map(|k| k.to_string());
        block_on(async move {
            match shared_conn().await {
                Ok(cell) => {
                    let mut core = cell.lock().await;
                    history_on(&mut core.conn, &kind, key.as_deref(), limit).await
                }
                Err(e) => {
                    report_history_err("conn", &e);
                    Vec::new()
                }
            }
        })
    }

    /// 当前存量行数（观察分 kind 滚动上限用）
    pub fn rows() -> Result<i64, DbError> {
        block_on(async {
            let cell = shared_conn().await?;
            let mut core = cell.lock().await;
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM records")
                .fetch_one(&mut core.conn)
                .await
                .map_err(|e| DbError::Sql(e.to_string()))
        })
    }
}

// ---------------- 黑箱内部 ----------------

/// 进程唯一常驻连接（audit GAP-2）：首次读写经 [`init_conn`] 用 `open_at` 建立
/// （三 PRAGMA + 迁移 + 建表/建索引只随该次 init 执行），此后全进程经独占锁复用。
/// `tokio::sync::Mutex` 而非 std（锁定决策 ii）：guard 跨 await，std 版会在单一驱动
/// runtime 上阻塞驱动线程、饿死同线程的 sqlite driver 任务 = 自死锁。
/// 锁内负载是 [`DbCore`]：连接 + 两条修剪环形队列（task-17）同锁互斥——入队/修剪
/// 与读写天然串行，无第二把锁、无跨锁竞态。
static CONN: OnceLock<tokio::sync::Mutex<DbCore>> = OnceLock::new();

/// 常驻锁内负载：唯一连接 + 两范围修剪状态 + net 修剪计数（task-17）。
/// 测试经 [`DbCore::new`] 在自有连接上实例化（队列态 = 每缝实例自带），
/// 不依赖全局 CONN；生产只有 [`init_conn`] 构造一次。
struct DbCore {
    conn: SqliteConnection,
    /// SCOPE_NET（1Hz 写入）修剪队列；每 [`NET_TRIM_TICKS`] 个 net 插入轮删一次
    net: ScopeTrim,
    /// SCOPE_OTHER（120s 写入）修剪队列；每轮删一次
    other: ScopeTrim,
    /// 距上次 net 修剪的 net 插入轮计数
    net_ticks: u32,
}

impl DbCore {
    fn new(conn: SqliteConnection) -> Self {
        DbCore {
            conn,
            net: ScopeTrim::with_window(NET_MAX_ROUNDS),
            other: ScopeTrim::with_window(MAX_ROUNDS),
            net_ticks: 0,
        }
    }
}

/// 修剪队列生命周期：Pending = 未引导（该范围尚未有首轮落库）；Active = 队列可用
/// （长度可 < window = 窗口未满，修剪待激）；Disabled = 引导 SQL 真错，本范围修剪
/// 永久停用（宁可不删=行数临时超窗，绝不误删）。
enum TrimPhase {
    Pending,
    Active,
    Disabled,
}

/// 单范围修剪状态（task-17）：轮首 ts_secs 环形队列替代
/// `DISTINCT ts … LIMIT 1 OFFSET N` 每轮扫描。队列新→旧（front = 最新轮），
/// 每轮一项（相邻同秒去重）；长度 > window 时 `queue[window-1]`（第 window 新轮）
/// 即 cutoff，删严格老于它的整轮 ⇒ 删后库中该范围恰剩 window 轮。
struct ScopeTrim {
    phase: TrimPhase,
    ring: VecDeque<i64>,
    /// 本范围滚动上限（生产 = 28800 / 20000；测试缝用小窗口复刻）
    window: usize,
}

impl ScopeTrim {
    fn with_window(window: i64) -> Self {
        ScopeTrim {
            phase: TrimPhase::Pending,
            ring: VecDeque::new(),
            window: window.max(1) as usize,
        }
    }

    /// 入队本轮轮首（仅 Active；与队首同秒 = 同轮，去重）。时间必须单调：
    /// 时钟回跳令乱序项滞留在队列中，最多致一轮少删/多删，重启引导即纠回。
    fn push(&mut self, ts_secs: i64) {
        if matches!(self.phase, TrimPhase::Active) && self.ring.front() != Some(&ts_secs) {
            self.ring.push_front(ts_secs);
        }
    }

    /// 引导（该范围至多执行一次）：**全量列表**查询灌入队列（旧
    /// `LIMIT 1 OFFSET N` 边界子查询只返回边界一个 ts，不是队列——冷启动超限库
    /// 拿它引导会当窗口未满、永不追删）。`LIMIT window+1`：多取的一项即激活位，
    /// 超限库首轮 trim_now 即收敛。库未满（含冷空库 = 0 行）只是待激、不是失败；
    /// SQL 真错才 Disabled（WARN 一次，本范围此后不修剪）。
    async fn ensure_bootstrap(&mut self, conn: &mut SqliteConnection, scope: &str) {
        if !matches!(self.phase, TrimPhase::Pending) {
            return;
        }
        let sql = format!(
            "SELECT DISTINCT CAST(strftime('%s', ts) AS INTEGER) FROM records \
             WHERE {scope} ORDER BY ts DESC LIMIT ?1"
        );
        match sqlx::query_scalar::<_, i64>(&sql)
            .bind((self.window + 1) as i64)
            .fetch_all(conn)
            .await
        {
            Ok(rows) => {
                self.ring = rows.into_iter().collect();
                self.phase = TrimPhase::Active;
            }
            Err(e) => {
                eprintln!(
                    "WARN db trim bootstrap scope [{scope}] failed, trim disabled for scope \
                     (rows may exceed window): {e}"
                );
                self.ring.clear();
                self.phase = TrimPhase::Disabled;
            }
        }
    }

    /// 修剪一次（不含节奏门）：未满窗（len ≤ window）或 Disabled 直接 0 行；
    /// 否则以 queue[window-1] 为 cutoff 整轮删除（守卫 len > window ⇒ 下标必在界）。
    /// DELETE 谓词右侧 strftime 把 cutoff 秒数**生成 RFC3339 TEXT 再比对**：ts 列是
    /// TEXT，裸 i64 绑定触发 SQLite affinity 转换（'1731…' 数字 vs '2026…' 文本），
    /// 谓词静默永不真值、DELETE 空转、表无界增长。右值表达式不包裹列 ⇒ 保留
    /// `idx_records_kind_ts`（net 等值腿）/ `idx_records_ts`（other 范围腿）range 扫描。
    /// DELETE 失败向上传播且**不弹队**——下轮重算同一 cutoff，天然重试。
    async fn trim_now(
        &mut self,
        conn: &mut SqliteConnection,
        scope: &str,
    ) -> Result<u64, DbError> {
        if !matches!(self.phase, TrimPhase::Active) || self.ring.len() <= self.window {
            return Ok(0);
        }
        let cutoff = self.ring[self.window - 1];
        let sql = format!(
            "DELETE FROM records WHERE {scope} AND ts < \
             strftime('%Y-%m-%dT%H:%M:%SZ', ?1, 'unixepoch')"
        );
        let deleted = sqlx::query(&sql)
            .bind(cutoff)
            .execute(conn)
            .await
            .map_err(|e| DbError::Sql(e.to_string()))?
            .rows_affected();
        while self.ring.back().is_some_and(|t| *t < cutoff) {
            self.ring.pop_back();
        }
        Ok(deleted)
    }
}

/// init 路径执行计数（红先测试的观测面：常驻连接下多次读写必须恒 ==1）。
#[cfg(test)]
static INIT_RUNS: AtomicUsize = AtomicUsize::new(0);

/// 测试专用 CONN 复位（锁定决策 iv）：std `OnceLock` 无 `&self` 反初始化
/// （`take` 要 `&mut`；对 static 手写 `addr_of_mut` 写入会与并发 `CONN.get()`——
/// 操作器测试线程可 1Hz 驱动 `Db::insert`——竞争成 UB），故复位语义为
/// 「异步锁排空在途读写 + TRUNCATE 截断收尾」：常驻连接留槽至进程退出，
/// init-once 断言全部用相对增量、不依赖反初始化。仅在共享测试锁持有时调用。
#[cfg(test)]
fn reset_conn_for_test() {
    if let Some(cell) = CONN.get() {
        block_on(async {
            let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE);")
                .execute(&mut cell.lock().await.conn)
                .await;
        });
    }
}

/// 取常驻连接（懒 init 快路径）。始终在 db 共享 runtime 上被调用（Db::* 的
/// block_on 之内），故 init 的 `open_at` 必在 current-thread runtime 内建立连接。
async fn shared_conn() -> Result<&'static tokio::sync::Mutex<DbCore>, DbError> {
    if let Some(cell) = CONN.get() {
        return Ok(cell);
    }
    init_conn().await
}

/// 连接初始化（进程内至多成功一次）。并发竞态的处理：OnceLock::get_or_init 闭包
/// 不可 await，故用 tokio Mutex 门 + 双检——并发 init 中只有执锁者真正跑 `open_at`，
/// 晚到者拿锁后见 CONN 已置、直接走快路径。不用「双 open + set + 败者丢弃」：
/// 那会让两个连接并发跑迁移，WAL 写锁竞争下一个 open 整体失败并无谓放大启动风险；
/// 门将窗口清零且零额外成本（init 每进程至多一次）。`CONN.set` 仅在测试期
/// [`reset_conn_for_test`] 竞态下可能失败：败者连接随 set 的 Err 返回值一并丢弃。
async fn init_conn() -> Result<&'static tokio::sync::Mutex<DbCore>, DbError> {
    static GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _gate = GATE.lock().await;
    if let Some(cell) = CONN.get() {
        return Ok(cell);
    }
    let conn = open_at(&db_path()?).await?;
    #[cfg(test)]
    INIT_RUNS.fetch_add(1, Ordering::SeqCst);
    let _ = CONN.set(tokio::sync::Mutex::new(DbCore::new(conn)));
    CONN.get().ok_or_else(|| DbError::Open("conn slot lost during init".into()))
}

/// WAL 截断检查点的 120s 节流守卫（`Some` = 上次成功时刻）。成功才推进守卫：
/// 被占住时记 WARN 不推进，下一轮读写即重试——best-effort，永不向调用方传播
/// 错误、绝不令本轮失败（锁定决策 iii）。
static CKPT_GATE: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);

async fn checkpoint_paced(conn: &mut SqliteConnection) {
    let mut last_ok = CKPT_GATE.lock().await;
    if let Some(t) = *last_ok {
        if t.elapsed() < CHECKPOINT_INTERVAL {
            return;
        }
    }
    #[cfg(test)]
    CKPT_RUNS.fetch_add(1, Ordering::SeqCst);
    // `PRAGMA wal_checkpoint(TRUNCATE)` 被读事务占住时**不报错**：返回
    // (busy=1, log, ckpt) 行、SQLITE_OK（SQLITE_BUSY 仅语句级硬错误路径）。
    // 两种「未完成」都必须落进不推进守卫的跳过分支，否则空转截断被误记为成功。
    match sqlx::query_as::<_, (i64, i64, i64)>("PRAGMA wal_checkpoint(TRUNCATE);")
        .fetch_optional(&mut *conn)
        .await
    {
        Ok(Some((0, _, _))) => *last_ok = Some(Instant::now()),
        Ok(Some((busy, log, ckpt))) => eprintln!(
            "WARN db wal_checkpoint(TRUNCATE) busy (best-effort, retry next round): busy={busy} log={log} ckpt={ckpt}"
        ),
        Ok(None) => eprintln!("WARN db wal_checkpoint(TRUNCATE) returned no row (skipped)"),
        Err(e) => eprintln!("WARN db wal_checkpoint(TRUNCATE) skipped (best-effort): {e}"),
    }
}

/// checkpoint 尝试计数（测试观测面：节流跳过与 BUSY 重试路径断言用）。
#[cfg(test)]
static CKPT_RUNS: AtomicUsize = AtomicUsize::new(0);

/// HISTORY_ERRS 读取失败累计计数（audit bug F12：静默空 vec 的唯一新增可观测面）。
/// 读失败仍返回空 vec（响应形状不变），信号只有本计数 + 一条 WARN 行；
/// 测试经计数断言，无需捕获 stderr。
pub(crate) static HISTORY_ERRS: AtomicUsize = AtomicUsize::new(0);

/// history 读取失败的唯一上报出口：计数 +1 并打一行 WARN（含错误文本）。
/// 调用方维持"空 vec / 默认字段"返回，响应形状不变。
pub(crate) fn report_history_err(what: &str, err: &dyn core::fmt::Display) {
    HISTORY_ERRS.fetch_add(1, Ordering::Relaxed);
    eprintln!("WARN db history {what} failed: {err}");
}

/// 行级字段读取：失败经 [`report_history_err`] 上报后取 `T::default()`
///（与原 `try_get().unwrap_or(0)` / `unwrap_or_default()` 行为等价）。
fn row_field<O>(row: &SqliteRow, idx: usize) -> O
where
    O: for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite> + Default,
{
    match row.try_get::<O, _>(idx) {
        Ok(v) => v,
        Err(e) => {
            report_history_err("row", &e);
            O::default()
        }
    }
}

/// 读取最近历史（测试缝：自带 open，迁移语义逐次可测；生产路径走常驻连接 + [`history_on`]）。
/// 所有失败路径（open / 查询 / 行解码）经 [`report_history_err`] 上报（计数 + WARN）后
/// 仍返回空 vec / 默认字段——响应形状不变，不再静默。
async fn history_at(path: &Path, kind: &str, key: Option<&str>, limit: u32) -> Vec<Record> {
    let mut conn = match open_at(path).await {
        Ok(conn) => conn,
        Err(e) => {
            report_history_err(&format!("open {}", path.display()), &e);
            return Vec::new();
        }
    };
    history_on(&mut conn, kind, key, limit).await
}

/// 历史查询主体（在调用方提供的连接上执行；生产 = 常驻连接独占锁内）。
/// 查询/行解码失败经 [`report_history_err`] 上报后返回空 vec / 默认字段。
async fn history_on(
    conn: &mut SqliteConnection,
    kind: &str,
    key: Option<&str>,
    limit: u32,
) -> Vec<Record> {
    let sql = match key {
        Some(_) => {
            "SELECT CAST(strftime('%s', ts) AS INTEGER),ts,kind,key,value FROM records WHERE kind=?1 AND key=?2 ORDER BY rowid DESC LIMIT ?3"
        }
        None => {
            "SELECT CAST(strftime('%s', ts) AS INTEGER),ts,kind,key,value FROM records WHERE kind=?1 ORDER BY rowid DESC LIMIT ?2"
        }
    };
    let mut q = sqlx::query::<sqlx::Sqlite>(sql).bind(kind);
    if let Some(k) = key {
        q = q.bind(k);
    }
    q = q.bind(i64::from(limit));
    let rows = match q.fetch_all(conn).await {
        Ok(rows) => rows,
        Err(e) => {
            report_history_err("query", &e);
            return Vec::new();
        }
    };
    rows.iter()
        .map(|r| Record {
            ts_secs: row_field::<i64>(r, 0),
            ts: row_field::<String>(r, 1),
            kind: row_field::<String>(r, 2),
            key: row_field::<String>(r, 3),
            value: row_field::<f64>(r, 4),
        })
        .collect()
}

/// 写入一轮（测试缝：自带 open + 每缝实例修剪队列，完整轮语义——插入 + 入队 +
/// 按节奏修剪；生产路径走常驻连接的同名编排）。
async fn insert_at(path: &Path, records: &[Record]) -> Result<usize, DbError> {
    let mut core = DbCore::new(open_at(path).await?);
    let n = run_insert(&mut core.conn, records).await?;
    post_insert(&mut core, records).await?;
    Ok(n)
}

/// 插入主体（在调用方提供的连接上执行；生产 = 常驻连接独占锁内）：
/// 单事务多值插入，不含修剪——修剪属 [`post_insert`]（提交后账目）。
async fn run_insert(conn: &mut SqliteConnection, records: &[Record]) -> Result<usize, DbError> {
    let mut tx = conn
        .begin()
        .await
        .map_err(|e| DbError::Sql(e.to_string()))?;
    // 每片一条多值 INSERT（行主序绑定，占位符连续编号）；空轮不进入循环、不发空语句
    for chunk in records.chunks(INSERT_CHUNK) {
        let mut sql = String::from("INSERT INTO records(ts,kind,key,value) VALUES ");
        for i in 0..chunk.len() {
            if i > 0 {
                sql.push(',');
            }
            let b = i * 4;
            sql.push_str(&format!("(?{},?{},?{},?{})", b + 1, b + 2, b + 3, b + 4));
        }
        let mut q = sqlx::query(&sql);
        for r in chunk {
            q = q.bind(&r.ts).bind(&r.kind).bind(&r.key).bind(r.value);
        }
        q.execute(&mut *tx)
            .await
            .map_err(|e| DbError::Sql(e.to_string()))?;
    }
    tx.commit().await.map_err(|e| DbError::Sql(e.to_string()))?;
    Ok(records.len())
}

/// 提交后的修剪账目（task-17；生产 = 常驻锁内，测试 = 自持 DbCore）：按批内所含
/// 范围各自入队轮首，再按节奏修剪——net 每 [`NET_TRIM_TICKS`] 个 net 插入轮一次
/// （计数只统计含 net 行的轮；先引导后入队），其余每轮一次。引导错误已在
/// [`ScopeTrim::ensure_bootstrap`] 内消化（Disabled + WARN，本轮不失败）；DELETE 失败
/// 向上传播且不弹队，下轮重试——队列与库始终同锁同序，无脱钩窗口。
async fn post_insert(core: &mut DbCore, records: &[Record]) -> Result<(), DbError> {
    let (net_head, other_head) = round_heads(records);
    if let Some(ts) = net_head {
        core.net.ensure_bootstrap(&mut core.conn, SCOPE_NET).await;
        core.net.push(ts);
        core.net_ticks += 1;
        if core.net_ticks >= NET_TRIM_TICKS {
            core.net_ticks = 0;
            core.net.trim_now(&mut core.conn, SCOPE_NET).await?;
        }
    }
    if let Some(ts) = other_head {
        core.other.ensure_bootstrap(&mut core.conn, SCOPE_OTHER).await;
        core.other.push(ts);
        core.other.trim_now(&mut core.conn, SCOPE_OTHER).await?;
    }
    Ok(())
}

/// 批内两范围各自的轮首（该范围行集合的最大 ts_secs；None = 本批无该范围的行）。
/// 一轮可混装（120s 大轮同时带 system 行与当秒 net 行）→ 两范围各自成轮。
fn round_heads(records: &[Record]) -> (Option<i64>, Option<i64>) {
    let (mut net, mut other) = (None, None);
    for r in records {
        let slot = if r.kind == "net" { &mut net } else { &mut other };
        *slot = Some(slot.map_or(r.ts_secs, |m: i64| m.max(r.ts_secs)));
    }
    (net, other)
}

fn db_path() -> Result<PathBuf, DbError> {
    static P: OnceLock<Option<PathBuf>> = OnceLock::new();
    P.get_or_init(|| prelude::data_dir().map(|d| d.join("atlas_history.db")))
        .clone()
        .ok_or_else(|| DbError::Open("no project data dir".into()))
}

/// 连接 + WAL 三 pragma + 幂等迁移 + 建表/建索引。生产路径仅由 [`init_conn`] 调用
/// 一次（常驻连接，GAP-2）；测试缝（`*_at`）仍逐次调用——迁移/建表幂等性逐次可测。
async fn open_at(path: &Path) -> Result<SqliteConnection, DbError> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let mut conn = SqliteConnection::connect_with(&opts)
        .await
        .map_err(|e| DbError::Open(e.to_string()))?;
    // F2FS fsync 昂贵：WAL + NORMAL 降写盘抖动；busy_timeout 抗并发锁竞争
    for pragma in [
        "PRAGMA journal_mode=WAL;",
        "PRAGMA synchronous=NORMAL;",
        "PRAGMA busy_timeout=5000;",
    ] {
        sqlx::query(pragma)
            .execute(&mut conn)
            .await
            .map_err(|e| DbError::Open(e.to_string()))?;
    }
    migrate(&mut conn).await?;
    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS records({RECORDS_COLUMNS})"
    ))
    .execute(&mut conn)
    .await
    .map_err(|e| DbError::Open(e.to_string()))?;
    for ddl in [
        "CREATE INDEX IF NOT EXISTS idx_records_kind_ts ON records(kind, ts)",
        "CREATE INDEX IF NOT EXISTS idx_records_ts ON records(ts)",
    ] {
        sqlx::query(ddl)
            .execute(&mut conn)
            .await
            .map_err(|e| DbError::Open(e.to_string()))?;
    }
    Ok(conn)
}

/// 就地 schema 迁移（幂等；判定树见模块文档「schema 迁移」）。
/// 电池行一次性迁移（`migrate_battery_units`）挂在 records 确保存在的各分支尾后；
/// 全新库走 `stamp_battery_migration` 直接盖章（建表在 open_at，UPDATE 不得先行）。
async fn migrate(conn: &mut SqliteConnection) -> Result<(), DbError> {
    match ts_decl_type(conn, "records").await? {
        // 新 schema（含全新库首次建表后的每一次 open）：仅走电池行一次性迁移
        Some(t) if t.eq_ignore_ascii_case("TEXT") => migrate_battery_units(conn).await,
        // records 在而 ts 非 TEXT（异常遗留）：就地重建，完成后迁移电池行
        Some(_) => {
            rebuild(conn, "records").await?;
            migrate_battery_units(conn).await
        }
        None => match ts_decl_type(conn, "samples").await? {
            // 两表皆缺：全新库，无旧行可迁移——直接盖章（建表交给 open_at）
            None => stamp_battery_migration(conn).await,
            // 旧表但 ts 已是 TEXT（历史手工迁移的理论遗留）：纯改名后迁移电池行
            Some(t) if t.eq_ignore_ascii_case("TEXT") => {
                sqlx::query("ALTER TABLE samples RENAME TO records")
                    .execute(&mut *conn)
                    .await
                    .map(|_| ())
                    .map_err(|e| DbError::Open(e.to_string()))?;
                migrate_battery_units(conn).await
            }
            // 主迁移路径：unix 秒 INTEGER → RFC3339 TEXT，重建完成后迁移电池行
            Some(_) => {
                rebuild(conn, "samples").await?;
                migrate_battery_units(conn).await
            }
        },
    }
}

/// 库级 schema 版本（`PRAGMA user_version`，存库头部页，随事务提交/回滚）
async fn user_version(conn: &mut SqliteConnection) -> Result<i64, DbError> {
    sqlx::query_scalar::<_, i64>("PRAGMA user_version")
        .fetch_one(conn)
        .await
        .map_err(|e| DbError::Open(e.to_string()))
}

/// 全新库快速路径：无旧行可迁移，仅盖章 user_version=2（版本只升不降）。
/// 单条 PRAGMA 自提交，无原子性面。
async fn stamp_battery_migration(conn: &mut SqliteConnection) -> Result<(), DbError> {
    if user_version(conn).await? < 2 {
        sqlx::query("PRAGMA user_version=2;")
            .execute(conn)
            .await
            .map_err(|e| DbError::Open(e.to_string()))?;
    }
    Ok(())
}

/// 一次性电池历史行迁移（GAP-1 存量清洗，配套写入侧 current_ua/temp_c 契约）：
/// `user_version < 2` 时单事务内改名 `battery/current_ma` → `current_ua`（值本就
/// 是 µA）、`battery/temp_c` ×10（旧写入侧 ÷10 存储还原真实 ℃）、置版本 2。
/// 三条语句同一提交/回滚单元：崩溃 = 整体未迁移（版本未盖），重开重跑恒安全，
/// 绝不半迁移或重复 ×10。版本 ≥2 时本事务只剩一次版本读（no-op）。
/// 前置条件：records 表确保存在（migrate 判定树各分支保证）。
async fn migrate_battery_units(conn: &mut SqliteConnection) -> Result<(), DbError> {
    let mut tx = conn
        .begin()
        .await
        .map_err(|e| DbError::Open(e.to_string()))?;
    if user_version(&mut tx).await? < 2 {
        for stmt in [
            "UPDATE records SET key='current_ua' WHERE kind='battery' AND key='current_ma'",
            "UPDATE records SET value=value*10 WHERE kind='battery' AND key='temp_c'",
            "PRAGMA user_version=2;",
        ] {
            sqlx::query(stmt)
                .execute(&mut *tx)
                .await
                .map_err(|e| DbError::Open(e.to_string()))?;
        }
    }
    tx.commit().await.map_err(|e| DbError::Open(e.to_string()))
}

/// 转写重建：单事务，任何失败整体回滚 = 未迁移，旧表旧行必不丢。
/// 复制按 rowid 排序，新表 rowid 单调延续旧插入序（history 新→旧语义不变）。
async fn rebuild(conn: &mut SqliteConnection, src: &str) -> Result<(), DbError> {
    let mut tx = conn
        .begin()
        .await
        .map_err(|e| DbError::Open(e.to_string()))?;
    for stmt in [
        format!("CREATE TABLE records_new({RECORDS_COLUMNS})"),
        format!(
            "INSERT INTO records_new(ts,kind,key,value) \
             SELECT strftime('%Y-%m-%dT%H:%M:%SZ', ts, 'unixepoch'), kind, key, value \
             FROM {src} ORDER BY rowid"
        ),
        format!("DROP TABLE {src}"),
        "ALTER TABLE records_new RENAME TO records".to_string(),
    ] {
        sqlx::query(&stmt)
            .execute(&mut *tx)
            .await
            .map_err(|e| DbError::Open(e.to_string()))?;
    }
    tx.commit().await.map_err(|e| DbError::Open(e.to_string()))
}

/// `table` 表 `ts` 列的声明类型（大写化）；表或列缺失 = None
async fn ts_decl_type(conn: &mut SqliteConnection, table: &str) -> Result<Option<String>, DbError> {
    sqlx::query_scalar::<_, String>(&format!(
        "SELECT upper(type) FROM pragma_table_info('{table}') WHERE name='ts'"
    ))
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| DbError::Open(e.to_string()))
}

/// 全库唯一常驻 runtime：同步入口（[`block_on`]）与操作器采样线程共用，进程内只建一次
pub(crate) fn rt() -> &'static tokio::runtime::Runtime {
    static S: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    S.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("db shared runtime")
    })
}

pub(crate) fn block_on<F: std::future::Future>(f: F) -> F::Output {
    rt().block_on(f)
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, SecondsFormat, Utc};

    /// 数据目录下按 tag+pid 隔离的临时库文件（先行清理，含 WAL 伴生文件）
    fn temp_db(tag: &str) -> PathBuf {
        let file = db_path()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("dbtest_{tag}_{}.db", std::process::id()));
        cleanup(&file);
        file
    }

    fn cleanup(file: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", file.display()));
        }
    }

    // ---- todo 17：环形队列修剪（引导 / 节奏 / affinity / 重启 / 重试）----

    /// 测试时刻基准（1700000000 = 2023-11-14T22:13:20Z）；off 秒偏移 → RFC3339 文本
    const T0: i64 = 1_700_000_000;

    fn rfc(off: i64) -> String {
        chrono::DateTime::from_timestamp(T0 + off, 0)
            .unwrap()
            .to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    /// 单行样本（每轮一行制测试用；ts 与 ts_secs 同一时刻）
    fn round_at(kind: &str, off: i64, key: &str) -> Record {
        Record {
            ts: rfc(off),
            ts_secs: T0 + off,
            kind: kind.into(),
            key: key.into(),
            value: 1.0,
        }
    }

    /// n 轮 × 每轮一行（off = 0..n，秒步进）——>window 种子库用
    fn seed_rounds(kind: &str, n: i64) -> Vec<Record> {
        (0..n).map(|i| round_at(kind, i, "k")).collect()
    }

    async fn distinct_rounds(conn: &mut SqliteConnection, kind_clause: &str) -> i64 {
        sqlx::query_scalar(&format!(
            "SELECT COUNT(DISTINCT ts) FROM records {kind_clause}"
        ))
        .fetch_one(conn)
        .await
        .unwrap()
    }

    #[test]
    fn trim_keeps_newest_rounds_whole() {
        let file = temp_db("trim");
        block_on(async {
            let mut core = DbCore::new(open_at(&file).await.unwrap());
            core.other = ScopeTrim::with_window(10);
            for i in 0..12i64 {
                // 每轮两行，验证按轮整删；ts=RFC3339，字典序即时间序
                let ts = rfc(i);
                for k in ["a", "b"] {
                    sqlx::query("INSERT INTO records(ts,kind,key,value) VALUES(?1,'k',?2,1)")
                        .bind(&ts)
                        .bind(k)
                        .execute(&mut core.conn)
                        .await
                        .unwrap();
                }
            }
            // 新轮（off=12）经完整提交后账目入队并触发修剪：引导快照 11 项
            //（window+1）+ 本轮 → len 12 > 10 → cutoff = ring[9] = 第 4 新…第 3 偏移轮
            // → 最老 3 轮整轮删除
            let round = vec![round_at("k", 12, "a"), round_at("k", 12, "b")];
            run_insert(&mut core.conn, &round).await.unwrap();
            post_insert(&mut core, &round).await.unwrap();
            let rounds = distinct_rounds(&mut core.conn, "").await;
            assert_eq!(rounds, 10, "应剩恰 window=10 轮");
            let oldest: String = sqlx::query_scalar("SELECT MIN(ts) FROM records")
                .fetch_one(&mut core.conn)
                .await
                .unwrap();
            assert_eq!(oldest, rfc(3), "剩余最老轮应为第 4 轮（off=3）");
            let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM records")
                .fetch_one(&mut core.conn)
                .await
                .unwrap();
            assert_eq!(rows, 20, "整轮保留：每轮 2 行 × 10 轮");
        });
        cleanup(&file);
    }

    /// net 与非 net 各自独立数轮：对 SCOPE_OTHER 修剪不动 net 行，反之亦然；
    /// 两档上限与 net 60 轮节奏用小窗口/逐 tick 复刻（生产 20000 / 28800 / 60）。
    #[test]
    fn trim_per_kind_scopes_are_independent() {
        let file = temp_db("trim_scope");
        block_on(async {
            let mut core = DbCore::new(open_at(&file).await.unwrap());
            core.net = ScopeTrim::with_window(5);
            core.other = ScopeTrim::with_window(10);
            let seed: Vec<Record> = (0..12i64)
                .flat_map(|i| [round_at("net", i, "k"), round_at("mem", i, "k")])
                .collect();
            run_insert(&mut core.conn, &seed).await.unwrap();
            // net-only 批连打 61 tick（off=12..72）：第 60 个 tick（off=71）修剪一次，
            // 其余 tick 只入队；mem 行从未出现在批内 → other 连引导都不发生
            for i in 12..73i64 {
                let batch = [round_at("net", i, "k")];
                run_insert(&mut core.conn, &batch).await.unwrap();
                post_insert(&mut core, &batch).await.unwrap();
                let mem_rounds =
                    distinct_rounds(&mut core.conn, "WHERE kind='mem'").await;
                assert_eq!(mem_rounds, 12, "net tick off={i} 不得动 mem 行");
                let want_net = match i {
                    71 => 5,                            // 第 60 tick：修剪落地
                    72 => 6,                            // 第 61 tick：修剪后重新积累
                    _ => i + 1,                         // 未到期：只增不删
                };
                let net_rounds =
                    distinct_rounds(&mut core.conn, "WHERE kind='net'").await;
                assert_eq!(net_rounds, want_net, "tick off={i}：修剪只该在第 60 轮发生");
            }
            // mem-only 批：other 此刻才引导（快照 11）+ 入队 → len 12 > 10 → 删 3 轮
            let batch = [round_at("mem", 73, "k")];
            run_insert(&mut core.conn, &batch).await.unwrap();
            post_insert(&mut core, &batch).await.unwrap();
            // net = 第 60 tick 修剪后的 window=5 + 第 61 tick（off=72）重新积累的一轮
            for (kind, want) in [("mem", 10), ("net", 6)] {
                let left =
                    distinct_rounds(&mut core.conn, &format!("WHERE kind='{kind}'")).await;
                assert_eq!(left, want, "{kind} 应剩 {want} 轮（两范围互不越界）");
            }
            let mem_min: String =
                sqlx::query_scalar("SELECT MIN(ts) FROM records WHERE kind='mem'")
                    .fetch_one(&mut core.conn)
                    .await
                    .unwrap();
            assert_eq!(mem_min, rfc(3), "mem 修剪只删自己最老 3 轮");
        });
        cleanup(&file);
    }

    /// 红先（真窗验收）：tempdir 库种子 MAX_ROUNDS+2 轮（每轮一行）→ 新一轮走完整
    /// 生产缝（run_insert + post_insert）：全量列表引导（window+1 项含本轮）→ 首轮
    /// 即激活 → cutoff 整轮删，恰剩 window 轮（观察界 = window，整轮保留）。
    #[test]
    fn bootstrap_then_ring_trim() {
        let file = temp_db("ring_boot");
        block_on(async {
            let mut core = DbCore::new(open_at(&file).await.unwrap());
            let w = MAX_ROUNDS; // 真实窗 20000（非缩小复刻）
            let seed = seed_rounds("mem", w + 2);
            assert_eq!(run_insert(&mut core.conn, &seed).await.unwrap(), (w + 2) as usize);
            let before = distinct_rounds(&mut core.conn, "WHERE kind='mem'").await;
            assert_eq!(before, w + 2, "前置：种子超限");

            let batch = [round_at("mem", w + 2, "k")];
            run_insert(&mut core.conn, &batch).await.unwrap();
            post_insert(&mut core, &batch).await.unwrap();

            let after = distinct_rounds(&mut core.conn, "WHERE kind='mem'").await;
            assert_eq!(after, w, "修剪后恰剩 window 轮（整轮）");
            let min: String = sqlx::query_scalar("SELECT MIN(ts) FROM records")
                .fetch_one(&mut core.conn)
                .await
                .unwrap();
            assert_eq!(min, rfc(3), "最老 3 轮（off 0..2）必须整轮删除");
            // 队列状态同步：修剪后 ring 恰 window 项
            assert_eq!(core.other.ring.len(), w as usize);
        });
        cleanup(&file);
    }

    /// 红先（节奏验收）：真窗 NET_MAX_ROUNDS 种子 → 逐 tick 打 net 轮：第 1..59 个
    /// tick 只增不删（观察界 window+59 ≤ 计划锁定的 window+60），第 60 个 tick 一次
    /// 性整轮修剪回 window（观察界 window+60 → window）。
    #[test]
    fn net_trim_every_60() {
        let file = temp_db("net60");
        block_on(async {
            let mut core = DbCore::new(open_at(&file).await.unwrap());
            let w = NET_MAX_ROUNDS; // 真实窗 28800
            let seed = seed_rounds("net", w);
            assert_eq!(run_insert(&mut core.conn, &seed).await.unwrap(), w as usize);

            for k in 1..=60i64 {
                let batch = [round_at("net", w + k, "k")];
                run_insert(&mut core.conn, &batch).await.unwrap();
                post_insert(&mut core, &batch).await.unwrap();
                let rounds = distinct_rounds(&mut core.conn, "WHERE kind='net'").await;
                if k < 60 {
                    assert_eq!(rounds, w + k, "第 {k} 个 net tick（<60）不得修剪");
                } else {
                    assert_eq!(rounds, w, "第 60 个 net tick 必须修剪回 window");
                }
            }
            let min: String = sqlx::query_scalar("SELECT MIN(ts) FROM records")
                .fetch_one(&mut core.conn)
                .await
                .unwrap();
            assert_eq!(min, rfc(60), "修剪删最老 60 轮（种子 off 0..59 出清）");
        });
        cleanup(&file);
    }

    /// 红先（affinity 陷阱）：修剪必须**真实删行**——裸 i64 绑定对 TEXT ts 列做 <
    /// 比较会命中 SQLite affinity 转换（'1731…' 数字转文本 vs '2026…'），谓词静默
    /// 恒假、DELETE 空转、表无界增长。断言行数增量 + MIN(ts) 推进 + rows_affected，
    /// 任何「只回 Ok 不删行」的实现（含 i64 绑定回归）必红。
    #[test]
    fn trim_actually_deletes_rows_not_just_ok() {
        let file = temp_db("affinity");
        block_on(async {
            let mut st = ScopeTrim::with_window(10);
            let mut conn = open_at(&file).await.unwrap();
            let seed = seed_rounds("mem", 12);
            run_insert(&mut conn, &seed).await.unwrap();
            let batch = [round_at("mem", 12, "k")];
            run_insert(&mut conn, &batch).await.unwrap();
            let pre: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM records")
                .fetch_one(&mut conn)
                .await
                .unwrap();

            st.ensure_bootstrap(&mut conn, SCOPE_OTHER).await;
            st.push(T0 + 12);
            let deleted = st.trim_now(&mut conn, SCOPE_OTHER).await.unwrap();

            assert_eq!(deleted, 3, "rows_affected 必须是真实删除数（3 轮 3 行）");
            let post: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM records")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(post, pre - 3, "行数必须等量减少");
            let min: String = sqlx::query_scalar("SELECT MIN(ts) FROM records")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(min, rfc(3), "MIN(ts) 必须推进到 cutoff 轮");
        });
        cleanup(&file);
    }

    /// 红先（引导 = 全量列表，非旧边界子查询）：超限库（种子 > window）引导必须
    /// 拿到 window+1 条 ts（旧 `LIMIT 1 OFFSET N` 只回 1 条边界值 = 库被误判未满、
    /// 永不追删）；队首 = 最新轮、队尾 = 第 window+1 新轮。
    #[test]
    fn bootstrap_seeds_full_list_not_boundary() {
        let file = temp_db("boot_full");
        block_on(async {
            let mut st = ScopeTrim::with_window(10);
            let mut conn = open_at(&file).await.unwrap();
            run_insert(&mut conn, &seed_rounds("net", 25)).await.unwrap();

            st.ensure_bootstrap(&mut conn, SCOPE_NET).await;
            assert!(matches!(st.phase, TrimPhase::Active));
            assert_eq!(st.ring.len(), 11, "全量引导 = window+1 条，不是 1 条边界");
            assert_eq!(st.ring.front(), Some(&(T0 + 24)), "队首 = 最新轮");
            assert_eq!(st.ring.back(), Some(&(T0 + 14)), "队尾 = 第 window+1 新轮");
            assert!(
                st.ring.iter().zip(st.ring.iter().skip(1)).all(|(a, b)| a > b),
                "队列必须严格新→旧"
            );
        });
        cleanup(&file);
    }

    /// 红先（冷空库 = 待激不是失败）：空库引导 → Active + 空队列；未满窗每轮
    /// 只入队不删；窗口越满的那一轮激活、恰剩 window（无永久停用）。
    #[test]
    fn cold_empty_ring_activates_on_fill() {
        let file = temp_db("cold_ring");
        block_on(async {
            let mut core = DbCore::new(open_at(&file).await.unwrap());
            core.other = ScopeTrim::with_window(10);
            for i in 0..10i64 {
                let batch = [round_at("mem", i, "k")];
                run_insert(&mut core.conn, &batch).await.unwrap();
                post_insert(&mut core, &batch).await.unwrap();
                assert!(matches!(core.other.phase, TrimPhase::Active), "引导必须成功");
                let rounds = distinct_rounds(&mut core.conn, "WHERE kind='mem'").await;
                assert_eq!(rounds, i + 1, "未满窗只增不删（轮 {i}）");
            }
            let batch = [round_at("mem", 10, "k")];
            run_insert(&mut core.conn, &batch).await.unwrap();
            post_insert(&mut core, &batch).await.unwrap();
            let rounds = distinct_rounds(&mut core.conn, "WHERE kind='mem'").await;
            assert_eq!(rounds, 10, "越满窗那轮激活，恰剩 window");
            let min: String =
                sqlx::query_scalar("SELECT MIN(ts) FROM records WHERE kind='mem'")
                    .fetch_one(&mut core.conn)
                    .await
                    .unwrap();
            assert_eq!(min, rfc(1), "只出清最老一轮");
        });
        cleanup(&file);
    }

    /// 引导 SQL 真错（records 缺失）→ 该范围 Disabled + trim_now 恒 0：绝不从空队列
    /// 算 cutoff（误删全库是比超窗更可怕的失败模式），也绝不让 insert 轮失败。
    #[test]
    fn bootstrap_sql_error_disables_scope_not_insert() {
        let file = temp_db("boot_err");
        block_on(async {
            let mut core = DbCore::new(open_at(&file).await.unwrap());
            core.other = ScopeTrim::with_window(10);
            sqlx::query("DROP TABLE records")
                .execute(&mut core.conn)
                .await
                .unwrap();

            core.other.ensure_bootstrap(&mut core.conn, SCOPE_OTHER).await;
            assert!(matches!(core.other.phase, TrimPhase::Disabled), "引导真错必须停用");
            assert_eq!(
                core.other.trim_now(&mut core.conn, SCOPE_OTHER).await.unwrap(),
                0,
                "停用范围不得发 DELETE"
            );
            core.other.ensure_bootstrap(&mut core.conn, SCOPE_OTHER).await;
            assert!(matches!(core.other.phase, TrimPhase::Disabled), "停用后不反复重试引导");
        });
        cleanup(&file);
    }

    /// stale-state 对抗（重启语义）：修剪落地后丢弃整个 DbCore（模拟进程重启）再
    /// 从同一库文件重建 → 引导必须从库里重新灌满队列，新一轮修剪照常、
    /// **零过删**（行数 ≥ window，恰剩 window，不是一切归零）。
    #[test]
    fn restart_rebootstraps_from_db_without_over_trim() {
        let file = temp_db("restart");
        block_on(async {
            let mut core = DbCore::new(open_at(&file).await.unwrap());
            core.other = ScopeTrim::with_window(10);
            run_insert(&mut core.conn, &seed_rounds("mem", 13)).await.unwrap();
            let batch = [round_at("mem", 13, "k")];
            run_insert(&mut core.conn, &batch).await.unwrap();
            post_insert(&mut core, &batch).await.unwrap();
            assert_eq!(distinct_rounds(&mut core.conn, "WHERE kind='mem'").await, 10);

            // 队列随 core 整体丢弃（重启）；连接重开、修剪状态全新
            drop(core);
            let mut core = DbCore::new(open_at(&file).await.unwrap());
            core.other = ScopeTrim::with_window(10);
            let batch = [round_at("mem", 14, "k")];
            run_insert(&mut core.conn, &batch).await.unwrap();
            post_insert(&mut core, &batch).await.unwrap();

            let rounds = distinct_rounds(&mut core.conn, "WHERE kind='mem'").await;
            assert_eq!(rounds, 10, "重启后修剪照常工作且零过删（恰剩 window）");
            let min: String =
                sqlx::query_scalar("SELECT MIN(ts) FROM records WHERE kind='mem'")
                    .fetch_one(&mut core.conn)
                    .await
                    .unwrap();
            assert_eq!(min, rfc(5), "仅按新轮推进一格（只出清 off=4），不得多删");
        });
        cleanup(&file);
    }

    /// repeated-interruptions 对抗（修剪 SQL 中途失败）：DELETE 报错必须向上传播
    /// 且**队不弹**——恢复后重发同一 cutoff 的 DELETE 即追平；失败轮不丢已提交数据。
    #[test]
    fn trim_sql_failure_keeps_queue_and_retries_next_round() {
        let file = temp_db("trim_retry");
        block_on(async {
            let mut st = ScopeTrim::with_window(10);
            let mut conn = open_at(&file).await.unwrap();
            run_insert(&mut conn, &seed_rounds("mem", 12)).await.unwrap();
            run_insert(&mut conn, &[round_at("mem", 12, "k")]).await.unwrap();
            st.ensure_bootstrap(&mut conn, SCOPE_OTHER).await;
            st.push(T0 + 12);
            let ring_before = st.ring.len();

            // 中途打断：表被改名 → DELETE 报 no such table，队列必须原样保留
            sqlx::query("ALTER TABLE records RENAME TO records_bak")
                .execute(&mut conn)
                .await
                .unwrap();
            assert!(st.trim_now(&mut conn, SCOPE_OTHER).await.is_err());
            assert_eq!(st.ring.len(), ring_before, "失败不得弹队");
            assert!(matches!(st.phase, TrimPhase::Active), "DELETE 失败不停用范围");

            // 恢复：同一队列、同一 cutoff 重试即追平
            sqlx::query("ALTER TABLE records_bak RENAME TO records")
                .execute(&mut conn)
                .await
                .unwrap();
            let deleted = st.trim_now(&mut conn, SCOPE_OTHER).await.unwrap();
            assert_eq!(deleted, 3, "重试必须真实删掉滞后 3 轮");
            assert_eq!(st.ring.len(), 10, "删成才弹队，队列与库同步");
        });
        cleanup(&file);
    }

    /// 旧 schema（samples，ts=unix 秒 INTEGER）就地迁移：行不丢、history() 可读
    /// （t=unix 秒正确、ts 为 TEXT 且等于 RFC3339），二次 open 幂等 no-op。
    #[test]
    fn legacy_integer_ts_db_migrates_in_place_on_open() {
        let file = temp_db("migration");
        block_on(async {
            // Given：旧表 + 两行已知样本（1700000000 = 2023-11-14T22:13:20Z，+3600s = 23:13:20Z）
            let opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(true);
            let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
            sqlx::query(
                "CREATE TABLE samples(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL, value REAL NOT NULL)",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            for (ts, v) in [(1_700_000_000i64, 1.0f64), (1_700_003_600, 2.0f64)] {
                sqlx::query("INSERT INTO samples(ts,kind,key,value) VALUES(?1,'mem','used',?2)")
                    .bind(ts)
                    .bind(v)
                    .execute(&mut conn)
                    .await
                    .unwrap();
            }
            drop(conn);

            // When：经生产路径 open（open_at 内嵌迁移）
            let mut conn = open_at(&file).await.unwrap();

            // Then：两行无损经 history 读出，投影/文本皆正确
            let hist = history_at(&file, "mem", Some("used"), 10).await;
            assert_eq!(hist.len(), 2, "迁移必须无损");
            assert_eq!(hist[0].ts_secs, 1_700_003_600, "新→旧：最新行在前");
            assert_eq!(hist[0].ts, "2023-11-14T23:13:20Z");
            assert_eq!(hist[1].ts_secs, 1_700_000_000);
            assert_eq!(hist[1].ts, "2023-11-14T22:13:20Z");
            let decl: String =
                sqlx::query_scalar("SELECT type FROM pragma_table_info('records') WHERE name='ts'")
                    .fetch_one(&mut conn)
                    .await
                    .unwrap();
            assert_eq!(decl, "TEXT", "迁移后 ts 列必须为 TEXT");

            // 幂等：二次 open = no-op（行不增不减）
            drop(open_at(&file).await.unwrap());
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM records")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(n, 2);
        });
        cleanup(&file);
    }

    // ---- todo 4：一次性电池行迁移（PRAGMA user_version 守卫）----

    /// 裸连接读 user_version（测试断言用，绕开生产 open_at 的幂等迁移）
    async fn raw_user_version(file: &Path) -> i64 {
        let opts = SqliteConnectOptions::new()
            .filename(file)
            .create_if_missing(false);
        let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut conn)
            .await
            .unwrap()
    }

    /// red-first（GAP-1 存量清洗）：旧契约电池行（current_ma 值本为 µA、temp_c 按
    /// ÷10 存储）经生产 open 改名 + ×10，user_version 置 2，二次 open no-op
    /// （值稳定、不重复 ×10）；对照行（同键非 battery kind）证明 WHERE kind 守卫。
    #[test]
    fn legacy_battery_rows_migrate_once_on_open() {
        let file = temp_db("battery_units");
        block_on(async {
            let opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(true);
            let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
            sqlx::query("CREATE TABLE records(ts TEXT, kind TEXT, key TEXT, value REAL)")
                .execute(&mut conn)
                .await
                .unwrap();
            let seed: [(&str, &str, f64); 5] = [
                ("battery", "current_ma", -500.0),
                ("battery", "temp_c", 3.25), // 旧 ÷10 存储；3.25/32.5 二进制精确，×10 无舍入漂移
                ("battery", "pct", 88.0),
                ("net", "current_ma", -1.0), // kind 守卫：非 battery 同旧键不改名
                ("net", "temp_c", 2.0),      // kind 守卫：非 battery 同键不 ×10
            ];
            for (kind, key, v) in seed {
                sqlx::query(
                    "INSERT INTO records(ts,kind,key,value) VALUES('2024-01-01T00:00:00Z',?1,?2,?3)",
                )
                .bind(kind)
                .bind(key)
                .bind(v)
                .execute(&mut conn)
                .await
                .unwrap();
            }
            drop(conn);

            // When：生产路径 open（open_at 内嵌迁移）
            drop(open_at(&file).await.expect("旧电池行库 open 必须成功"));

            // Then：断言值而非 Ok
            let bat = history_at(&file, "battery", None, 10).await;
            let want = |k: &str| bat.iter().find(|r| r.key == k).map(|r| r.value);
            assert_eq!(want("current_ua"), Some(-500.0), "必须改名且 µA 原值不变");
            assert_eq!(want("current_ma"), None, "旧键不得残留");
            assert_eq!(want("temp_c"), Some(32.5), "旧 ÷10 温度必须 ×10 还原 ℃");
            assert_eq!(want("pct"), Some(88.0), "非迁移范围键值不动");
            let net = history_at(&file, "net", None, 10).await;
            let nwant = |k: &str| net.iter().find(|r| r.key == k).map(|r| r.value);
            assert_eq!(nwant("current_ma"), Some(-1.0), "WHERE kind='battery' 守卫必须生效");
            assert_eq!(nwant("temp_c"), Some(2.0));
            assert_eq!(raw_user_version(&file).await, 2);

            // 二次 open = no-op：值稳定（不重复 ×10）、版本不再动作
            drop(open_at(&file).await.unwrap());
            let bat2 = history_at(&file, "battery", None, 10).await;
            let want2 = |k: &str| bat2.iter().find(|r| r.key == k).map(|r| r.value);
            assert_eq!(want2("temp_c"), Some(32.5), "二次 open 不得重复 ×10");
            assert_eq!(raw_user_version(&file).await, 2);
        });
        cleanup(&file);
    }

    /// red-first（B1 边界）：冷启动空库（文件不存在）走生产 open——必须 Ok
    /// （迁移 UPDATE 不得先于 CREATE TABLE 撞上 `no such table`）且一步盖到
    /// user_version=2、records 表为空。
    #[test]
    fn cold_empty_db_open_stamps_version_without_error() {
        let file = temp_db("cold_empty");
        block_on(async {
            assert!(!file.exists(), "前置：库文件不存在");
            drop(open_at(&file).await.expect("冷空库 open 不得报 no such table"));
            assert_eq!(
                raw_user_version(&file).await,
                2,
                "全新库首次 open 即盖章 2（无旧行可迁移）"
            );
            let opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(false);
            let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM records")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(n, 0);
        });
        cleanup(&file);
    }

    /// 与 INTEGER-ts 遗留重建分支共存（stale-state 对抗）：最古 schema（samples）
    /// 里就带旧契约电池行——rebuild 先转写重建、电池迁移在 rebuild 完成后执行，
    /// 两个 pass 同一次 open 同时生效。
    #[test]
    fn integer_ts_legacy_battery_rows_migrate_after_rebuild() {
        let file = temp_db("legacy_int_battery");
        block_on(async {
            let opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(true);
            let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
            sqlx::query(
                "CREATE TABLE samples(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, kind TEXT NOT NULL, key TEXT NOT NULL, value REAL NOT NULL)",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            for (key, v) in [("current_ma", -500.0f64), ("temp_c", 3.25f64)] {
                sqlx::query(
                    "INSERT INTO samples(ts,kind,key,value) VALUES(1700000000,'battery',?1,?2)",
                )
                .bind(key)
                .bind(v)
                .execute(&mut conn)
                .await
                .unwrap();
            }
            sqlx::query(
                "INSERT INTO samples(ts,kind,key,value) VALUES(1700000000,'mem','used',100)",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            drop(conn);

            drop(open_at(&file).await.expect("INTEGER 遗留库 open 必须成功"));

            let bat = history_at(&file, "battery", None, 10).await;
            assert_eq!(bat.len(), 2, "重建必须无损");
            let want = |k: &str| bat.iter().find(|r| r.key == k).map(|r| r.value);
            assert_eq!(want("current_ua"), Some(-500.0), "rebuild 完成后电池迁移必须生效");
            assert_eq!(want("temp_c"), Some(32.5));
            assert!(
                bat.iter().all(|r| r.ts == "2023-11-14T22:13:20Z"),
                "ts 必须已被重建 pass 转写为 RFC3339"
            );
            assert_eq!(raw_user_version(&file).await, 2);
        });
        cleanup(&file);
    }

    /// 事务原子性（= 崩溃中途等价）：迁移事务内 UPDATE#1（改名）已成功、
    /// UPDATE#2（×10）在缺 value 列的库上报错 → 整体回滚——改名同样未生效、
    /// 版本保持 0 → 崩溃后重开可安全重放，绝无半迁移状态。
    #[test]
    fn failed_battery_migration_rolls_back_rename_and_version() {
        let file = temp_db("battery_txn_rollback");
        block_on(async {
            let opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(true);
            let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
            sqlx::query("CREATE TABLE records(ts TEXT, kind TEXT, key TEXT)")
                .execute(&mut conn)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO records(ts,kind,key) VALUES('2024-01-01T00:00:00Z','battery','current_ma')",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            drop(conn);

            let res = open_at(&file).await;
            assert!(res.is_err(), "迁移失败必须让 open 显式失败（由调用方上报）");

            let opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(false);
            let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
            let key: String = sqlx::query_scalar("SELECT key FROM records")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(key, "current_ma", "回滚必须连 UPDATE#1 一起撤销");
            drop(conn);
            assert_eq!(raw_user_version(&file).await, 0, "失败迁移不得盖章版本");
        });
        cleanup(&file);
    }

    /// 迁移原子性的前提钉桩：PRAGMA user_version 写库头部页，与 UPDATE 同事务
    /// 回滚——提交前崩溃 = 全部未生效，绝不出现「版本已盖章但行未迁移」（丢迁移）
    /// 或「行已迁移但版本未盖」（重放双重 ×10）。
    #[test]
    fn pragma_user_version_participates_in_transaction() {
        let file = temp_db("pragma_txn");
        block_on(async {
            let opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(true);
            let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
            sqlx::query("CREATE TABLE records(ts TEXT, kind TEXT, key TEXT, value REAL)")
                .execute(&mut conn)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO records(ts,kind,key,value) VALUES('2024-01-01T00:00:00Z','battery','temp_c',3.25)",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            let mut tx = conn.begin().await.unwrap();
            sqlx::query("UPDATE records SET value=value*10 WHERE kind='battery' AND key='temp_c'")
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("PRAGMA user_version=2;")
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.rollback().await.unwrap();
            drop(conn);

            assert_eq!(raw_user_version(&file).await, 0, "PRAGMA 必须随事务回滚");
            let opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(false);
            let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
            let v: f64 = sqlx::query_scalar("SELECT value FROM records")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(v, 3.25, "UPDATE 必须随事务回滚");
        });
        cleanup(&file);
    }

    /// 新写入：ts 以 RFC3339 入库，读回可被 DateTime::parse_from_rfc3339 解析，
    /// 且 SQL strftime('%s') 投影与存储时刻一致。
    #[test]
    fn inserted_ts_is_parseable_rfc3339() {
        let file = temp_db("rfc3339");
        block_on(async {
            let now = Utc::now();
            let rec = Record {
                ts: now.to_rfc3339_opts(SecondsFormat::Secs, true),
                ts_secs: now.timestamp(),
                kind: "mem".into(),
                key: "used".into(),
                value: 123.0,
            };
            assert_eq!(insert_at(&file, &[rec.clone()]).await.unwrap(), 1);

            let hist = history_at(&file, "mem", Some("used"), 5).await;
            assert_eq!(hist.len(), 1);
            let parsed = DateTime::parse_from_rfc3339(&hist[0].ts).expect("入库 ts 必须是 RFC3339");
            assert_eq!(parsed.timestamp(), rec.ts_secs);
            assert_eq!(
                hist[0].ts_secs, rec.ts_secs,
                "strftime('%s') 投影必须与存储时刻一致"
            );
        });
        cleanup(&file);
    }

    /// HISTORY_ERRS 为模块级累计计数、跨测试共享：两条错误路径测试互斥执行，
    /// 保证"恰好 +1"断言不受并行干扰（其余测试走 happy 路径，从不加数）。
    static ERR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// red-first（audit bug F12）：open 失败（garbage 文件）不得被吞成静默空 vec——
    /// 返回空 vec（响应形状不变）且 HISTORY_ERRS 恰好 +1（WARN 为唯一新增信号）。
    #[test]
    fn history_open_failure_bumps_err_counter_and_stays_empty() {
        let _guard = ERR_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let file = temp_db("history_err_open");
        std::fs::write(&file, b"not a sqlite database, just garbage bytes").unwrap();

        let before = HISTORY_ERRS.load(Ordering::Relaxed);
        let hist = block_on(history_at(&file, "mem", None, 10));

        assert!(hist.is_empty(), "读失败仍返回空 vec（响应形状不变）");
        assert_eq!(
            HISTORY_ERRS.load(Ordering::Relaxed),
            before + 1,
            "open 失败必须使 HISTORY_ERRS 恰好 +1"
        );
        cleanup(&file);
    }

    /// red-first（audit bug F12）：合法 sqlite 但 records 缺列（wrong-schema）→ SELECT
    /// 报错路径同样不得静默：空 vec + HISTORY_ERRS 恰好 +1。
    #[test]
    fn history_query_failure_bumps_err_counter_and_stays_empty() {
        let _guard = ERR_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let file = temp_db("history_err_query");
        block_on(async {
            // Given：真 sqlite 库，records 仅 (ts,kind)——open_at 的迁移/建表/建索引全通过。
            // 盖章 user_version=2：本库视为已过电池迁移（缺 key/value 列会让该迁移先
            // 报错走 open 路径），确保失败落在被测的查询路径上。
            let opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(true);
            let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
            sqlx::query("CREATE TABLE records(ts TEXT, kind TEXT)")
                .execute(&mut conn)
                .await
                .unwrap();
            sqlx::query("PRAGMA user_version=2;")
                .execute(&mut conn)
                .await
                .unwrap();
            drop(conn);

            let before = HISTORY_ERRS.load(Ordering::Relaxed);
            // When：查询取 key/value 列 → no-such-column
            let hist = history_at(&file, "mem", None, 10).await;

            assert!(hist.is_empty(), "查询失败仍返回空 vec（响应形状不变）");
            assert_eq!(
                HISTORY_ERRS.load(Ordering::Relaxed),
                before + 1,
                "query 失败必须使 HISTORY_ERRS 恰好 +1"
            );
        });
        cleanup(&file);
    }

    // ---- todo 16：常驻连接一次性 init + 节流 WAL 截断 ----

    /// 触碰全局 CONN/CKPT_GATE（及真实库文件）的测试串行化（cargo test 默认并行线程；
    /// 操作器/api 测试的采样也可能经 Db::insert 抢先 init，故断言全部用相对增量）。
    static SHARED_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 红先（GAP-2 验收）：两次顺序写入只 init 一次（旧「每操作开新连接」下 init
    /// 计数会 +2）；随后公开读入口必须复用同一连接（不再 init）且真实读回写入行。
    #[test]
    fn resident_conn_inits_once_and_history_reuses_it() {
        let _guard = SHARED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let before = INIT_RUNS.load(Ordering::SeqCst);
        let now = Utc::now();
        let rec = Record {
            ts: now.to_rfc3339_opts(SecondsFormat::Secs, true),
            ts_secs: now.timestamp(),
            kind: "probe16".into(),
            key: "k".into(),
            value: 7.5,
        };
        block_on(async {
            assert_eq!(Db::insert(&[rec.clone()]).await.unwrap(), 1);
            assert_eq!(Db::insert(&[rec.clone()]).await.unwrap(), 1);
            assert_eq!(
                INIT_RUNS.load(Ordering::SeqCst),
                before + 1,
                "常驻连接下两次顺序写必须恰好 init 一次"
            );
        });
        // stale-state 对抗：CONN 已活时第二次起的全部读写走快路径复用
        let hist = Db::history("probe16", Some("k"), 10);
        assert_eq!(hist.len(), 2, "写入行必须经同一常驻连接真实读回");
        assert!(hist.iter().all(|r| r.value == 7.5), "读回值不得失真");
        assert_eq!(
            INIT_RUNS.load(Ordering::SeqCst),
            before + 1,
            "读路径必须复用常驻连接，不得重 init"
        );
        // 清场：探针行从真实库删除；常驻连接复位（决策 iv 的测试缝）
        block_on(async {
            let cell = shared_conn().await.unwrap();
            sqlx::query("DELETE FROM records WHERE kind='probe16'")
                .execute(&mut cell.lock().await.conn)
                .await
                .unwrap();
        });
        reset_conn_for_test();
    }

    /// 红先（GAP-2 节流 + 决策 iii）：外部读事务卡住 TRUNCATE → BUSY 只记 WARN
    /// 跳过、守卫不推进（下一次调用即重试）；读事务释放后成功截断、守卫推进，
    /// CHECKPOINT_INTERVAL 内立即再调被节流跳过。
    #[test]
    fn wal_checkpoint_paces_and_busy_retries_next_attempt() {
        let _guard = SHARED_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let file = temp_db("ckpt");
        block_on(async {
            *CKPT_GATE.lock().await = None;
            let before = CKPT_RUNS.load(Ordering::SeqCst);
            let now = Utc::now();
            let rec = Record {
                ts: now.to_rfc3339_opts(SecondsFormat::Secs, true),
                ts_secs: now.timestamp(),
                kind: "mem".into(),
                key: "used".into(),
                value: 1.0,
            };
            let mut conn = open_at(&file).await.unwrap();
            assert_eq!(
                run_insert(&mut conn, &[rec]).await.unwrap(),
                1,
                "先经被测连接写一批撑大 WAL（run_insert 不触发截断，连接在持亦无 close 自动 checkpoint）"
            );
            // BUSY 路径不必等满生产 busy_timeout=5000
            sqlx::query("PRAGMA busy_timeout=50;")
                .execute(&mut conn)
                .await
                .unwrap();
            // 模拟外部读者：开读事务占住 WAL 读快照 → TRUNCATE 无法截断（busy=1 行）
            let reader_opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(false);
            let mut reader = SqliteConnection::connect_with(&reader_opts).await.unwrap();
            sqlx::query("BEGIN;").execute(&mut reader).await.unwrap();
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM records")
                .fetch_one(&mut reader)
                .await
                .unwrap();
            // 前置：-wal 已被写入撑大、读事务在持——否则 BUSY 断言是空转
            let wal_pre = std::fs::metadata(format!("{}-wal", file.display())).unwrap();
            assert!(
                wal_pre.len() >= 1024,
                "前置：-wal 必须非空（实际 {} 字节）",
                wal_pre.len()
            );

            checkpoint_paced(&mut conn).await;
            assert_eq!(
                CKPT_RUNS.load(Ordering::SeqCst),
                before + 1,
                "到期必须实际尝试一次"
            );
            checkpoint_paced(&mut conn).await;
            assert_eq!(
                CKPT_RUNS.load(Ordering::SeqCst),
                before + 2,
                "BUSY 不得推进守卫——下一轮必须重试"
            );

            sqlx::query("ROLLBACK;").execute(&mut reader).await.unwrap();
            drop(reader);
            checkpoint_paced(&mut conn).await;
            assert_eq!(CKPT_RUNS.load(Ordering::SeqCst), before + 3);
            checkpoint_paced(&mut conn).await;
            assert_eq!(
                CKPT_RUNS.load(Ordering::SeqCst),
                before + 3,
                "CHECKPOINT_INTERVAL 内必须节流跳过"
            );
            let wal = std::fs::metadata(format!("{}-wal", file.display())).unwrap();
            assert!(
                wal.len() < 1024,
                "TRUNCATE 成功后 -wal 必须被截断，实际 {} 字节",
                wal.len()
            );
            drop(conn);
        });
        cleanup(&file);
    }
}
