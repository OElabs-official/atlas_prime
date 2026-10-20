//! v1/db: sqlite 历史采样黑箱（sqlx / runtime-tokio；自带单一共享 runtime，不依赖外层）。
//!
//! # 公开接口用法
//! ```ignore
//! use crate::v1::db::Db;
//!
//! Db::start().ok();                          // 采样线程：每 2 分钟一轮，首轮立即（幂等，二次调用 Err）
//! let n = Db::sample_now().ok();             // 手动补一轮（返回写入行数）
//! let hist = Db::history("temp", Some("battery.0"), 200);   // 最近 200 条电池温度
//! Db::stop();                                // 置标志 + Notify 即时唤醒，当前轮收尾即停
//! ```
//!
//! # 存储模型
//! 单表窄长项 `samples(id, ts, kind, key, value)`，一轮采样展开成多行：
//! - `cpu_freq` key=`cpu{i}`（GHz）；`temp` key=`{类别}.{i}`（℃，类别=cpu_core/gpu/camera/pmic/battery）；
//! - `mem` key=total/used/avail、`swap` key=total/used（MB）；
//! - `battery` key=pct/volt_mv/current_ma/temp_c/cycle/plugged/present；
//! - `disk` key=`{挂载点}|used` / `{挂载点}|total`（MB）。
//!
//! 滚动上限 [`MAX_ROUNDS`] **轮**（非行）：一轮 = 同一 ts 的全部样本行。
//! 每轮写入尾部执行 `DELETE ... WHERE ts < (第 N 新的 DISTINCT ts)`，
//! 超过 N 轮即整轮丢弃最老。2 分钟/轮 × 20000 轮 ≈ 27.7 天回溯。
//!
//! # 写入与并发模型
//! 全库共用**一个**常驻 current-thread runtime（同步入口与采样线程同驱动，进程内只建一次）；
//! 每次操作开新连接，连上即执行 `PRAGMA journal_mode=WAL; synchronous=NORMAL; busy_timeout=5000`
//! ——Android F2FS fsync 昂贵，WAL+NORMAL 显著压降写盘抖动，busy_timeout 抗并发锁竞争。
//! 一轮 = 单事务内若干条 **多值 INSERT**（每片 ≤200 行 = ≤800 绑定，低于
//! SQLITE_MAX_VARIABLE_NUMBER=999），提交后整轮修剪。停止即时：[`Db::stop`] 置标志并
//! `Notify::notify_waiters` 唤醒 select 中的睡眠，采样线程微秒级退出（在途轮次完整收尾）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use directories::ProjectDirs;
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{Connection, Row};

use crate::v1::PROJECT;
use crate::v1::system::{battery, cpu, disk, mem, thermal};

/// 滚动上限（采样轮数；一轮 ≈ 百行）
pub const MAX_ROUNDS: i64 = 20_000;
/// 默认采样周期（start 使用）
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(120);

/// 一条历史样本
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    /// Unix 秒
    pub ts_secs: i64,
    pub kind: String,
    pub key: String,
    pub value: f64,
}

/// 失败原因（Open=库文件/建表环境，Sql=SQL 执行）
#[derive(Debug, Clone)]
pub enum DbError {
    Open(String),
    Sql(String),
    /// 采样线程已在运行
    Started,
}

impl core::fmt::Display for DbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DbError::Open(m) => write!(f, "db open/schema failed: {m}"),
            DbError::Sql(m) => write!(f, "db sql failed: {m}"),
            DbError::Started => write!(f, "sampler already started"),
        }
    }
}
impl std::error::Error for DbError {}

/// 唯一公开入口
pub struct Db;

impl Db {
    /// 启动周期采样线程（首轮立即，其后 2 分钟/轮）。幂等：重复调用返回 [`DbError::Started`]。
    pub fn start() -> Result<(), DbError> {
        static HANDLE: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();
        if RUNNING.swap(true, Ordering::SeqCst) {
            return Err(DbError::Started);
        }
        stop_flag().store(false, Ordering::SeqCst);
        let h = std::thread::spawn(|| rt().block_on(run_sampler()));
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

    /// 立即采一轮并落库（含滚动修剪），返回写入行数。
    /// 同步入口：驱动共享 runtime（[`rt`]），可在任意（非该 runtime 内部的）线程上下文调用。
    pub fn sample_now() -> Result<usize, DbError> {
        block_on(sample_now())
    }

    /// 最近历史（新→旧）。key=None 时聚合该 kind 全部键。
    pub fn history(kind: &str, key: Option<&str>, limit: u32) -> Vec<Record> {
        let kind = kind.to_string();
        let key = key.map(|k| k.to_string());
        block_on(async move {
            let mut conn = match open().await {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let q = match &key {
                Some(_) => {
                    "SELECT ts,kind,key,value FROM samples WHERE kind=?1 AND key=?2 ORDER BY id DESC LIMIT ?3"
                }
                None => {
                    "SELECT ts,kind,key,value FROM samples WHERE kind=?1 ORDER BY id DESC LIMIT ?2"
                }
            };
            let mut q = sqlx::query::<sqlx::Sqlite>(q).bind(&kind);
            if let Some(k) = &key {
                q = q.bind(k);
            }
            q = q.bind(limit as i64);
            let rows = q.fetch_all(&mut conn).await.unwrap_or_default();
            rows.iter()
                .map(|r| Record {
                    ts_secs: r.try_get::<i64, _>(0).unwrap_or(0),
                    kind: r.try_get::<String, _>(1).unwrap_or_default(),
                    key: r.try_get::<String, _>(2).unwrap_or_default(),
                    value: r.try_get::<f64, _>(3).unwrap_or(0.0),
                })
                .collect()
        })
    }

    /// 当前存量行数（观察 10000 上限用）
    pub fn rows() -> Result<i64, DbError> {
        block_on(async {
            let mut conn = open().await?;
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM samples")
                .fetch_one(&mut conn)
                .await
                .map_err(|e| DbError::Sql(e.to_string()))
        })
    }
}

// ---------------- 黑箱内部 ----------------

/// 单条多值 INSERT 的最大行数（4 列 × 200 = 800 绑定 < SQLITE_MAX_VARIABLE_NUMBER=999）
const INSERT_CHUNK: usize = 200;

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

/// 全库唯一常驻 runtime：同步入口（[`block_on`]）与采样线程共用，进程内只建一次
fn rt() -> &'static tokio::runtime::Runtime {
    static S: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    S.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("db shared runtime")
    })
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    rt().block_on(f)
}

/// 采样线程主体（跑在共享 runtime 上）：首轮立即，此后每 [`SAMPLE_INTERVAL`] 一轮。
/// 等待阶段 = sleep 与停止唤醒的 select 竞争；唤醒/轮次结束后复查停止标志，
/// 置位即退出——stop() 后微秒级返回睡眠，在途轮次完整收尾。
async fn run_sampler() {
    let _ = sample_now().await;
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
        let _ = sample_now().await;
    }
    RUNNING.store(false, Ordering::SeqCst);
}

fn db_path() -> Result<PathBuf, DbError> {
    static P: OnceLock<Option<PathBuf>> = OnceLock::new();
    P.get_or_init(|| {
        ProjectDirs::from(PROJECT[0], PROJECT[1], PROJECT[2]).map(|p| {
            let d = p.data_dir().to_path_buf();
            let _ = std::fs::create_dir_all(&d);
            d.join("atlas_history.db")
        })
    })
    .clone()
    .ok_or_else(|| DbError::Open("no project data dir".into()))
}

async fn open() -> Result<SqliteConnection, DbError> {
    let path = db_path()?;
    let opts = SqliteConnectOptions::new()
        .filename(&path)
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
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS samples(
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             ts INTEGER NOT NULL,
             kind TEXT NOT NULL,
             key TEXT NOT NULL,
             value REAL NOT NULL
         )",
    )
    .execute(&mut conn)
    .await
    .map_err(|e| DbError::Open(e.to_string()))?;
    for ddl in [
        "CREATE INDEX IF NOT EXISTS idx_samples_kind_ts ON samples(kind, ts)",
        "CREATE INDEX IF NOT EXISTS idx_samples_ts ON samples(ts)",
    ] {
        sqlx::query(ddl)
            .execute(&mut conn)
            .await
            .map_err(|e| DbError::Open(e.to_string()))?;
    }
    Ok(conn)
}

/// 采一轮：采集（纯读）→ 单事务多值批量写入 → 滚动修剪。返回写入行数。
async fn sample_now() -> Result<usize, DbError> {
    let records = collect();
    let mut conn = open().await?;
    let mut tx = conn
        .begin()
        .await
        .map_err(|e| DbError::Sql(e.to_string()))?;
    // 每片一条多值 INSERT（行主序绑定，占位符连续编号）；空轮不进入循环、不发空语句
    for chunk in records.chunks(INSERT_CHUNK) {
        let mut sql = String::from("INSERT INTO samples(ts,kind,key,value) VALUES ");
        for i in 0..chunk.len() {
            if i > 0 {
                sql.push(',');
            }
            let b = i * 4;
            sql.push_str(&format!("(?{},?{},?{},?{})", b + 1, b + 2, b + 3, b + 4));
        }
        let mut q = sqlx::query(&sql);
        for r in chunk {
            q = q.bind(r.ts_secs).bind(&r.kind).bind(&r.key).bind(r.value);
        }
        q.execute(&mut *tx)
            .await
            .map_err(|e| DbError::Sql(e.to_string()))?;
    }
    tx.commit().await.map_err(|e| DbError::Sql(e.to_string()))?;
    trim_rounds(&mut conn, MAX_ROUNDS).await?;
    Ok(records.len())
}

/// 滚动修剪：只保留最新 rounds 轮（同 ts = 同轮；轮数不足时子查询为 NULL，不删）
async fn trim_rounds(conn: &mut SqliteConnection, rounds: i64) -> Result<u64, DbError> {
    Ok(sqlx::query(
        "DELETE FROM samples WHERE ts < (SELECT DISTINCT ts FROM samples ORDER BY ts DESC LIMIT 1 OFFSET ?1)",
    )
    .bind(rounds - 1)
    .execute(conn)
    .await
    .map_err(|e| DbError::Sql(e.to_string()))?
    .rows_affected())
}

/// 一轮全量采集：system 各黑箱 → 窄长样本行
fn collect() -> Vec<Record> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut out = Vec::new();
    {
        let mut push = |kind: &str, key: String, value: f64| {
            out.push(Record {
                ts_secs: ts,
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
        // 沿用 CLI 温度源；迁移 thermal::Temp.battery 属 system 侧改造，不在 db 黑箱内进行
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

    #[test]
    fn collect_covers_every_domain() {
        let v = collect();
        for kind in ["cpu_freq", "temp", "mem", "swap", "battery", "disk"] {
            assert!(v.iter().any(|r| r.kind == kind), "缺 {kind} 采样");
        }
        assert!(v.iter().all(|r| r.ts_secs > 0));
    }

    #[test]
    fn trim_keeps_newest_rounds_whole() {
        let file = db_path()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("trim_test_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&file);
        block_on(async move {
            let opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(true);
            let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
            sqlx::query(
                "CREATE TABLE samples(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER, kind TEXT, key TEXT, value REAL)",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            for i in 0..12i64 {
                // 每轮两行，验证按轮整删
                for k in ["a", "b"] {
                    sqlx::query("INSERT INTO samples(ts,kind,key,value) VALUES(?1,'k',?2,1)")
                        .bind(i)
                        .bind(k)
                        .execute(&mut conn)
                        .await
                        .unwrap();
                }
            }
            let deleted = trim_rounds(&mut conn, 10).await.unwrap();
            assert_eq!(deleted, 4, "应恰好删最老 2 轮 = 4 行");
            let oldest: i64 = sqlx::query_scalar("SELECT MIN(ts) FROM samples")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(oldest, 2, "剩余最老轮应为 ts=2");
            let rounds: i64 = sqlx::query_scalar("SELECT COUNT(DISTINCT ts) FROM samples")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(rounds, 10, "应剩 10 轮");
            drop(conn);
            let _ = std::fs::remove_file(&file);
        });
    }

    /// 真机闭环：sample_now → rows → history。
    /// `cargo test v1::db -- --ignored --test-threads=1`
    #[test]
    #[ignore]
    fn live_roundtrip() {
        let n = Db::sample_now().unwrap();
        assert!(n > 10, "单轮应写 >10 行，实际 {n}");
        let total = Db::rows().unwrap();
        assert!(total >= n as i64);
        let hist = Db::history("mem", Some("used"), 5);
        assert!(!hist.is_empty() && hist[0].value > 0.0);
    }

    /// 真机即时停止：采样线程运行中 stop() 应毫秒级退出（而非等满 2 分钟）。
    /// `cargo test v1::db live_instant_stop -- --ignored --test-threads=1`
    #[test]
    #[ignore]
    fn live_instant_stop() {
        Db::start().expect("首次 start 应成功");
        std::thread::sleep(Duration::from_secs(2)); // 越过首轮采样
        let t0 = std::time::Instant::now();
        Db::stop();
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
