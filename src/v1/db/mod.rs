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
//! - `battery` key=pct/volt_mv/current_ma/temp_c/cycle/plugged/present；
//! - `disk` key=`{挂载点}|used` / `{挂载点}|total`（MB）。
//!
//! 滚动上限按 **kind 分档**（轮粒度：一轮 = 同一 ts 的全部样本行；RFC3339 统一
//! 格式 ⇒ 字典序即时间序，`DELETE ... WHERE kind范围 AND ts < (该范围内第 N 新的
//! DISTINCT ts)` 依旧整轮丢弃）：
//! - `net`（网卡速度，操作器 1Hz 回路写入）：[`NET_MAX_ROUNDS`] = 28800 轮 ≈ 8h 回溯；
//!   key 为**分组级** `<组>.rx_bps` / `<组>.tx_bps`（组 = `wifi` / `cellular` /
//!   有流量的其他网卡名，见 [`crate::v1::system::network::speed`]）；2026-10-20.28
//!   起的分组化改造令旧 per-iface 键（`rmnet_data3.rx_bps` 等）退役——不迁移、
//!   不改写，随 ≈8h 滚动窗口自然出清；
//! - 其余 kind（2 分钟/轮）：[`MAX_ROUNDS`] = 20000 轮 ≈ 27.7 天回溯。
//!
//! net 与非 net 各自只在本范围内数轮、互不混算（否则 1Hz 的海量 ts 会把 120s 类
//! 的窗口压没）。每批落库后按批内所含范围各发一条 DELETE（固定两条 prepared
//! 语句路径；net tick 每轮删一行级，1Hz 下开销可忽略）。
//!
//! # schema 迁移（幂等，每次 open 执行；二次 open = no-op）
//! `pragma_table_info` 查 `records.ts` 声明类型：TEXT → 新 schema，什么都不做；
//! `records` 缺失而旧 `samples`（ts=unix 秒 INTEGER）在 → 单事务内
//! `records_new`(TEXT) ← `strftime('%Y-%m-%dT%H:%M:%SZ', ts, 'unixepoch')` 转写复制
//! （ORDER BY rowid 保历史插入序，rowid 单调延续）→ DROP 旧表 → RENAME；
//! 事务失败整体回滚 = 未迁移，旧行必不丢。两表皆缺 → 全新库就地建新。
//! 旧 `idx_samples_*` 随 DROP 消失，等价的 `idx_records_*` 随后重建。
//!
//! # 读取契约
//! `/db/history` 只经 [`Db::history`] 暴露 unix 秒投影：SELECT 内
//! `CAST(strftime('%s', ts) AS INTEGER)`（迁移后全行均为 RFC3339 TEXT，strftime 统一可析）。
//!
//! # 写入与并发模型
//! 全库共用**一个**常驻 current-thread runtime（同步入口与操作器采样线程同驱动，
//! 进程内只建一次）；每次操作开新连接，连上即执行
//! `PRAGMA journal_mode=WAL; synchronous=NORMAL; busy_timeout=5000`
//! ——Android F2FS fsync 昂贵，WAL+NORMAL 显著压降写盘抖动，busy_timeout 抗并发锁竞争。
//! 一轮 = 单事务内若干条 **多值 INSERT**（每片 ≤200 行 = ≤800 绑定，低于
//! SQLITE_MAX_VARIABLE_NUMBER=999），提交后整轮修剪。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use directories::ProjectDirs;
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{Connection, Row};

use crate::v1::PROJECT;

/// 非 net kind 的滚动上限（采样轮数；一轮 ≈ 百行，2 分钟/轮 ≈ 27.7 天）
pub const MAX_ROUNDS: i64 = 20_000;

/// `net`（网卡速度，1Hz）的滚动上限（轮数 = 秒数；28800 ≈ 8h 回溯）
pub const NET_MAX_ROUNDS: i64 = 28_800;

/// 修剪范围 SQL 片段：net 专属 / 其余全部（两条固定 prepared 语句路径）
const SCOPE_NET: &str = "kind = 'net'";
const SCOPE_OTHER: &str = "kind <> 'net'";

/// 单条多值 INSERT 的最大行数（4 列 × 200 = 800 绑定 < SQLITE_MAX_VARIABLE_NUMBER=999）
const INSERT_CHUNK: usize = 200;

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
    /// 写入一轮记录（单事务多值插入 + 滚动修剪）。唯一调用方 = 操作器。
    pub(crate) async fn insert(records: &[Record]) -> Result<usize, DbError> {
        insert_at(&db_path()?, records).await
    }

    /// 最近历史（新→旧）。key=None 时聚合该 kind 全部键。
    /// `ts_secs` = unix 秒 SQL 投影；调用方自行升序排序。
    pub fn history(kind: &str, key: Option<&str>, limit: u32) -> Vec<Record> {
        let kind = kind.to_string();
        let key = key.map(|k| k.to_string());
        block_on(async move {
            let Ok(path) = db_path() else {
                return Vec::new();
            };
            history_at(&path, &kind, key.as_deref(), limit).await
        })
    }

    /// 当前存量行数（观察分 kind 滚动上限用）
    pub fn rows() -> Result<i64, DbError> {
        block_on(async {
            let mut conn = open().await?;
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM records")
                .fetch_one(&mut conn)
                .await
                .map_err(|e| DbError::Sql(e.to_string()))
        })
    }
}

// ---------------- 黑箱内部 ----------------

async fn history_at(path: &Path, kind: &str, key: Option<&str>, limit: u32) -> Vec<Record> {
    let Ok(mut conn) = open_at(path).await else {
        return Vec::new();
    };
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
    let rows = q.fetch_all(&mut conn).await.unwrap_or_default();
    rows.iter()
        .map(|r| Record {
            ts_secs: r.try_get::<i64, _>(0).unwrap_or(0),
            ts: r.try_get::<String, _>(1).unwrap_or_default(),
            kind: r.try_get::<String, _>(2).unwrap_or_default(),
            key: r.try_get::<String, _>(3).unwrap_or_default(),
            value: r.try_get::<f64, _>(4).unwrap_or(0.0),
        })
        .collect()
}

async fn insert_at(path: &Path, records: &[Record]) -> Result<usize, DbError> {
    let mut conn = open_at(path).await?;
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
    // 按批内所含范围各自修剪：net 批只修剪 net，120s 批只修剪其余 kind
    if records.iter().any(|r| r.kind == "net") {
        trim_rounds(&mut conn, SCOPE_NET, NET_MAX_ROUNDS).await?;
    }
    if records.iter().any(|r| r.kind != "net") {
        trim_rounds(&mut conn, SCOPE_OTHER, MAX_ROUNDS).await?;
    }
    Ok(records.len())
}

/// 滚动修剪（单范围）：该 kind 范围内只保留最新 rounds 轮（同 ts = 同轮；
/// RFC3339 字典序即时间序；轮数不足时子查询为 NULL，不删）。
/// `scope` 只接收本模块常量片段，不进外部输入。
async fn trim_rounds(
    conn: &mut SqliteConnection,
    scope: &str,
    rounds: i64,
) -> Result<u64, DbError> {
    let sql = format!(
        "DELETE FROM records WHERE {scope} AND ts < \
         (SELECT DISTINCT ts FROM records WHERE {scope} ORDER BY ts DESC LIMIT 1 OFFSET ?1)"
    );
    Ok(sqlx::query(&sql)
        .bind(rounds - 1)
        .execute(conn)
        .await
        .map_err(|e| DbError::Sql(e.to_string()))?
        .rows_affected())
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
    open_at(&db_path()?).await
}

/// 连接 + WAL 三 pragma + 幂等迁移 + 建表/建索引（每次操作开新连接）
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

/// 就地 schema 迁移（幂等；判定树见模块文档「schema 迁移」）
async fn migrate(conn: &mut SqliteConnection) -> Result<(), DbError> {
    match ts_decl_type(conn, "records").await? {
        // 新 schema（含全新库首次建表后的每一次 open）：no-op
        Some(t) if t.eq_ignore_ascii_case("TEXT") => Ok(()),
        // records 在而 ts 非 TEXT（异常遗留）：就地重建
        Some(_) => rebuild(conn, "records").await,
        None => match ts_decl_type(conn, "samples").await? {
            // 两表皆缺：全新库，建表交给 open_at
            None => Ok(()),
            // 旧表但 ts 已是 TEXT（历史手工迁移的理论遗留）：纯改名
            Some(t) if t.eq_ignore_ascii_case("TEXT") => {
                sqlx::query("ALTER TABLE samples RENAME TO records")
                    .execute(&mut *conn)
                    .await
                    .map(|_| ())
                    .map_err(|e| DbError::Open(e.to_string()))
            }
            // 主迁移路径：unix 秒 INTEGER → RFC3339 TEXT
            Some(_) => rebuild(conn, "samples").await,
        },
    }
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

    #[test]
    fn trim_keeps_newest_rounds_whole() {
        let file = temp_db("trim");
        block_on(async {
            let opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(true);
            let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
            sqlx::query("CREATE TABLE records(ts TEXT, kind TEXT, key TEXT, value REAL)")
                .execute(&mut conn)
                .await
                .unwrap();
            for i in 0..12i64 {
                // 每轮两行，验证按轮整删；ts=RFC3339，字典序即时间序
                let ts = format!("2024-01-01T{i:02}:00:00Z");
                for k in ["a", "b"] {
                    sqlx::query("INSERT INTO records(ts,kind,key,value) VALUES(?1,'k',?2,1)")
                        .bind(&ts)
                        .bind(k)
                        .execute(&mut conn)
                        .await
                        .unwrap();
                }
            }
            let deleted = trim_rounds(&mut conn, SCOPE_OTHER, 10).await.unwrap();
            assert_eq!(deleted, 4, "应恰好删最老 2 轮 = 4 行");
            let oldest: String = sqlx::query_scalar("SELECT MIN(ts) FROM records")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(oldest, "2024-01-01T02:00:00Z", "剩余最老轮应为第 3 轮");
            let rounds: i64 = sqlx::query_scalar("SELECT COUNT(DISTINCT ts) FROM records")
                .fetch_one(&mut conn)
                .await
                .unwrap();
            assert_eq!(rounds, 10, "应剩 10 轮");
            drop(conn);
        });
        cleanup(&file);
    }

    /// net 与非 net 各自独立数轮：对 SCOPE_OTHER 修剪不动 net 行，反之亦然；
    /// 两档上限不同（此处以 10 / 5 小规模复刻 20000 / 28800 的分档语义）。
    #[test]
    fn trim_per_kind_scopes_are_independent() {
        let file = temp_db("trim_scope");
        block_on(async {
            let opts = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(true);
            let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
            sqlx::query("CREATE TABLE records(ts TEXT, kind TEXT, key TEXT, value REAL)")
                .execute(&mut conn)
                .await
                .unwrap();
            for i in 0..12i64 {
                // 同一 ts 同时写 net 与非 net 各一行：混在同一时间轴上验证范围隔离
                let ts = format!("2024-01-01T{i:02}:00:00Z");
                for kind in ["net", "mem"] {
                    sqlx::query("INSERT INTO records(ts,kind,key,value) VALUES(?1,?2,'k',1)")
                        .bind(&ts)
                        .bind(kind)
                        .execute(&mut conn)
                        .await
                        .unwrap();
                }
            }
            let d1 = trim_rounds(&mut conn, SCOPE_OTHER, 10).await.unwrap();
            assert_eq!(d1, 2, "非 net 应只删自己最老 2 轮");
            let d2 = trim_rounds(&mut conn, SCOPE_NET, 5).await.unwrap();
            assert_eq!(d2, 7, "net 应删自己最老 7 轮");
            for (kind, want) in [("mem", 10), ("net", 5)] {
                let left: i64 = sqlx::query_scalar(&format!(
                    "SELECT COUNT(DISTINCT ts) FROM records WHERE kind='{kind}'"
                ))
                .fetch_one(&mut conn)
                .await
                .unwrap();
                assert_eq!(left, want, "{kind} 应剩 {want} 轮");
            }
            drop(conn);
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
}
