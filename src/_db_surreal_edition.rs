use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, RocksDb};

use crate::constans::{DB_DFT_DB, DB_DFT_NS};
use crate::prelude::AtlasPath;
use crate::ui::info::AndroidBatInfo;

// 全局数据库实例
pub static DB_INSTANCE: OnceLock<Surreal<Db>> = OnceLock::new();
// 数据库存储路径
// static DB_PATH: OnceLock<PathBuf> = OnceLock::new();

pub struct AtlasDB;

impl AtlasDB {
    /// 获取全局数据库句柄
    pub fn get() -> &'static Surreal<Db> {
        DB_INSTANCE.get().expect("AtlasDB not initialized! Call init() in main.")
    }

    /// DB::init(): 初始化连接、设置 NS/DB
    pub async fn init() -> surrealdb::Result<()> {
        let db_path = AtlasPath::get_db_dir();
        let db = Surreal::new::<RocksDb>(db_path).await?;

        // 默认初始化一个核心业务空间，但不强制后续操作必须在此空间
        db.use_ns(DB_DFT_NS).use_db(DB_DFT_DB).await?;
        
        if DB_INSTANCE.set(db).is_err() {
            // 已初始化则忽略
        }
        Ok(())
    }

    /// 核心执行器：现在只负责接收 SQL 和变量绑定
    /// 所有的业务逻辑（NS/DB 切换）都在 SQL 字符串中完成
    pub async fn execute_raw(sql: &str, vars: BTreeMap<String, serde_json::Value>) -> surrealdb::Result<surrealdb::Response> {
        let mut req = Self::get().query(sql);
        for (k, v) in vars {
            req = req.bind((k, v));
        }
        req.await
    }

    /// DB::record(...): 实时写入原始数据
    /// 这里使用泛型 T，只要实现了 Serialize 即可存入指定 table
    /// 修复后的 record 函数
    /// DB::record(...): 实时写入原始数据 (默认上下文)
    #[deprecated] pub async fn record<T>(table: &str, record: T) -> surrealdb::Result<()>
    where
        // T: Serialize + DeserializeOwned + Send + Sync + 'static,
        T: Serialize  + Send + Sync + 'static,
    {
        let _: Option<serde_json::Value> = Self::get().create(table).content(record).await?;
        Ok(())
    }

    #[deprecated] pub async fn record_to<T>(ns: &str, db_name: &str, table: &str, record: T) -> surrealdb::Result<()> 
    where T: Serialize 
    {
        let client = Self::get();
        client.use_ns(ns).use_db(db_name).await?;
        
        // --- 核心修复：强制先转为纯 JSON Value ---
        let json_value = serde_json::to_value(&record)
            .map_err(|e| surrealdb::Error::Api(surrealdb::error::Api::Query(e.to_string())))?;

        // 现在传给 .content() 的是一个已经处理好的 JSON，不含任何 Rust 原始 Enum
        let _: Option<serde_json::Value> = client
            .create(table)
            .content(json_value) 
            .await?;
            
        Ok(())
    }

    /// 修复后的通用写入接口：只要求 Serialize
    #[deprecated]pub async fn _record_to<T>(
        ns: &str, 
        db: &str, 
        table: &str, 
        record: T
    ) -> surrealdb::Result<()> 
    where 
        T: Serialize + Send + Sync + 'static // 写入只需要序列化，不需要 DeserializeOwned
    {
        let client = Self::get();
        client.use_ns(ns).use_db(db).await?;
        
        // 显式标注返回值类型为 Option<serde_json::Value>
        // 这样可以规避对 T 的反序列化要求
        let _: Option<serde_json::Value> = client
            .create(table)
            .content(record)
            .await?;
            
        Ok(())
    }

    /// 增强版 get_stats: 显式指定上下文，获取任意表的统计信息
    pub async fn get_stats(ns: &str, db_name: &str, table: &str) -> surrealdb::Result<(u64, String)> {
        let db = Self::get();
        
        // 使用原始 SQL 确保上下文切换在单次请求内完成，不影响全局单例状态
        let sql = format!("USE NS {ns}; USE DB {db_name}; SELECT count() FROM {table} GROUP ALL");
        let mut response = db.query(sql).await?;

        // 提取 count (注意：在使用 USE 语句后，count 结果通常在最后一个语句的结果集中)
        let count: u64 = response
            .take::<Vec<serde_json::Value>>(2)? // Index 2 是因为前面有两个 USE 语句
            .first()
            .and_then(|v| v.get("count").and_then(|c| c.as_u64()))
            .unwrap_or(0);

        // 磁盘占用计算保持不变
        let db_path = AtlasPath::get_db_dir();
        let bytes = Self::get_dir_size(db_path).unwrap_or(0);
        let size_str = Self::format_size(bytes);

        Ok((count, size_str))
    }

    fn format_size(bytes: u64) -> String {
        if bytes < 1024 * 1024 {
            format!("{:.2} KB", bytes as f64 / 1024.0)
        } else if bytes < 1024 * 1024 * 1024 {
            format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
        }
    }
    
    /// DB::get_stats(): 获取指定表的行数和数据库总磁盘占用
    #[deprecated] pub async fn _get_stats(table: &str) -> surrealdb::Result<(u64, String)> {
        // 1. 获取行数：SurrealDB 2.x count() 返回对象数组
        let mut response = Self::get()
            .query(format!("SELECT count() FROM {} GROUP ALL", table))
            .await?;

        let count: u64 = response
            .take::<Vec<serde_json::Value>>(0)?
            .first()
            .and_then(|v| v.get("count").and_then(|c| c.as_u64()))
            .unwrap_or(0);

        // 2. 获取磁盘大小并格式化
        let db_path = AtlasPath::get_db_dir();
        let bytes = Self::get_dir_size(db_path).unwrap_or(0);
        let size_str = if bytes < 1024 * 1024 {
            format!("{:.2} KB", bytes as f64 / 1024.0)
        } else if bytes < 1024 * 1024 * 1024 {
            format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
        };

        Ok((count, size_str))
    }

    /// DB::archive_hourly(): 数据降采样逻辑
    /// 将 source 表中过去一小时的数据聚合平均值，存入 target 表
    #[deprecated] pub async fn archive_hourly(source: &str, target: &str) -> surrealdb::Result<()> {
        let sql = format!(
            "INSERT INTO {target} (cpu_temp, battery_level, battery_temp, timestamp)
             SELECT 
                math::mean(cpu_temp) AS cpu_temp, 
                math::mean(battery_level) AS battery_level, 
                math::mean(battery_temp) AS battery_temp,
                time::floor(timestamp, 1h) AS timestamp
             FROM {source}
             WHERE timestamp > time::floor(time::now() - 1h, 1h)
               AND timestamp < time::floor(time::now(), 1h)
             GROUP BY timestamp"
        );

        Self::get().query(sql).await?;
        Ok(())
    }
    
    /// DB::reindex(): 执行索引重建，优化 RocksDB 碎片
    #[deprecated] pub async fn reindex() -> surrealdb::Result<()> {
        Self::get().query("REBUILD INDEX").await?;
        Ok(())
    }


    // 辅助函数：递归计算文件夹大小
     fn get_dir_size(path: PathBuf) -> std::io::Result<u64> {
        let mut size = 0;
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                size += Self::get_dir_size(entry.path())?;
            } else {
                size += metadata.len();
            }
        }
        Ok(size)
    }

    /// get_full_hierarchy(): 扫描并返回全景元数据结构 (NS -> DB -> Table)
    /// 用于 Database Tab 页面的自发现渲染
    /// 扫描并返回全景元数据结构 (NS -> DB -> Table)
    /// get_full_hierarchy(): 扫描并返回全景元数据结构 (NS -> DB -> Table)
    /// 修复了 usize 特征匹配问题，并增强了上下文恢复的安全性
    #[deprecated] pub async fn get_full_hierarchy() -> surrealdb::Result<serde_json::Value> {
        /*
        这是一个典型的 Rust 编译器错误信息的误导情况。当 surrealdb 的 take 方法无法推导返回值类型，或者返回值类型不匹配时，它往往会报出 QueryResult is not implemented for {integer} 这种奇怪的错误，实际上它的潜台词是：“我找不到一个能把索引 0 转换成你想要的那种类型的实现”。

        在 SurrealDB 2.x 中，take 取出的结果通常需要包裹在 Option<T> 中，因为查询结果可能是空的。如果不加 Option，编译器在进行 Trait 匹配时会失败，从而导致这个针对索引类型的报错。
        修复方案

        请将 take(0) 的接收变量显式标注为 Option<serde_json::Value>。

        把你的 get_full_hierarchy 函数修改为如下版本（我同时保留了 InfoResult 结构体，这比纯 Value 解析更安全）：
        */
        let db = Self::get();
        let mut hierarchy = serde_json::json!({});

        // 1. 扫描 Namespaces
        let mut ns_res = db.query("INFO FOR ROOT").await?;
        
        // 【关键修复】：
        // 1. 显式指定泛型为 Option<serde_json::Value>
        // 2. 这里的 0 会被正确识别为索引
        let root_opt: Option<serde_json::Value> = ns_res.take(0)?;
        
        // 安全解包，如果没有数据则给个默认值
        let root_val = root_opt.unwrap_or(serde_json::json!({}));
        let root_info: InfoResult = serde_json::from_value(root_val)
             .unwrap_or(InfoResult { namespaces: None, databases: None, tables: None });
        
        if let Some(nss) = root_info.namespaces {
            for ns in nss.keys() {
                let _ = db.use_ns(ns).await;
                
                // 2. 扫描 Databases
                let mut db_res = db.query("INFO FOR NS").await?;
                // 同样使用 Option<serde_json::Value>
                let ns_opt: Option<serde_json::Value> = db_res.take(0)?;
                let ns_val = ns_opt.unwrap_or(serde_json::json!({}));
                let ns_info: InfoResult = serde_json::from_value(ns_val)
                    .unwrap_or(InfoResult { namespaces: None, databases: None, tables: None });
                
                if let Some(dbs) = ns_info.databases {
                    let mut db_map = serde_json::json!({});
                    
                    for db_name in dbs.keys() {
                        let _ = db.use_db(db_name).await;
                        
                        // 3. 扫描 Tables
                        let mut tb_res = db.query("INFO FOR DB").await?;
                        let db_opt: Option<serde_json::Value> = tb_res.take(0)?;
                        let db_val = db_opt.unwrap_or(serde_json::json!({}));
                        let db_info: InfoResult = serde_json::from_value(db_val)
                            .unwrap_or(InfoResult { namespaces: None, databases: None, tables: None });
                        
                        if let Some(tbs) = db_info.tables {
                            let table_names: Vec<_> = tbs.keys().cloned().collect();
                            db_map[db_name] = serde_json::json!(table_names);
                        }
                    }
                    hierarchy[ns] = db_map;
                }
            }
        }
        
        // 恢复默认上下文
        let _ = db.use_ns("android").use_db("telemetry").await;
        Ok(hierarchy)
    }

    /// 获取带有行数统计的全景视图
    pub async fn get_full_report() -> surrealdb::Result<serde_json::Value> {
        let db = Self::get();
        let mut report = serde_json::json!({});

        // 1. 扫描 Namespaces
        let mut ns_res = db.query("INFO FOR ROOT").await?;
        let root_opt: Option<serde_json::Value> = ns_res.take(0)?;
        let root_val = root_opt.unwrap_or_default();
        
        if let Some(nss) = root_val.get("namespaces").and_then(|v| v.as_object()) {
            for ns in nss.keys() {
                db.use_ns(ns).await?;
                let mut db_res = db.query("INFO FOR NS").await?;
                let ns_opt: Option<serde_json::Value> = db_res.take(0)?;
                
                if let Some(dbs) = ns_opt.and_then(|v| v.get("databases").map(|d| d.to_owned())) 
                                        .and_then(|v| v.as_object().map(|o| o.to_owned())) {
                    let mut db_map = serde_json::json!({});
                    
                    for db_name in dbs.keys() {
                        db.use_db(db_name).await?;
                        let mut tb_res = db.query("INFO FOR DB").await?;
                        let db_opt: Option<serde_json::Value> = tb_res.take(0)?;
                        
                        if let Some(tbs) = db_opt.and_then(|v| v.get("tables").map(|t| t.to_owned()))
                                                .and_then(|v| v.as_object().map(|o| o.to_owned())) {
                            let mut table_info = serde_json::json!({});
                            for table_name in tbs.keys() {
                                // 查询每张表的长度
                                let mut count_res = db.query(format!("SELECT count() FROM {} GROUP ALL", table_name)).await?;
                                let count_val: Option<serde_json::Value> = count_res.take(0)?;
                                let count = count_val.and_then(|v| v.get("count").and_then(|c| c.as_u64())).unwrap_or(0);
                                
                                table_info[table_name] = serde_json::json!(count);
                            }
                            db_map[db_name] = table_info;
                        }
                    }
                    report[ns] = db_map;
                }
            }
        }

        // 恢复默认上下文
        let _ = db.use_ns(DB_DFT_NS).use_db(DB_DFT_DB).await;
        Ok(report)
    }

}
#[derive(Deserialize, Debug)]
struct InfoResult {
    namespaces: Option<BTreeMap<String, String>>,
    databases: Option<BTreeMap<String, String>>,
    tables: Option<BTreeMap<String, String>>,
}

    /*
    . Database Tab 页面设计讨论

既然已经有了这些关联函数，我们的 Database Tab 页面可以设计成一个 “数据库健康仪表盘”。
建议显示的 UI 元素：

    Storage Card (存储卡片)：

        显示路径：~/.local/share/atlas/db

        占用空间：使用 get_stats 返回的 size_str。

    Table List (表清单)：

        telemetry_history: 实时表，显示当前行数。

        telemetry_hourly: 归档表，显示已存储的周期数。

    Operation Bar (操作栏)：

        [F5] Reindex: 触发 reindex() 并显示 Loading。

        [F6] Archive: 手动触发一次降采样测试。

        [F9] Clear Raw: 清理实时表，仅保留归档数据（节省空间）。

        4. 数据粒度自动化的下一步

在 main.rs 中，我们可以启动一个简单的 tokio::spawn 循环：
Rust

tokio::spawn(async move {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600)); // 每小时执行一次
    loop {
        interval.tick().await;
        if let Err(e) = AtlasDB::archive_hourly("telemetry_history", "telemetry_hourly").await {
            // 通过 GlobIO 发送日志事件
            let _ = GlobIO::send().send(GlobalEvent::Log(format!("Archive failed: {}", e)));
        }
    }
});

你觉得在 Database Tab 页面中，是否需要增加一个“实时 SQL 编辑器”来允许你手动删除特定的异常数据？


     */



























// pub fn get_db_dir() -> &'static PathBuf {
//     DB_PATH.get_or_init(|| {
//         let base_dirs = BaseDirs::new().expect("无法获取系统基础目录");

//         // 获取数据根目录 (Linux/Android 为 ~/.local/share)
//         let mut path = base_dirs.data_dir().to_path_buf();

//         // 直接在根目录下创建你的项目文件夹
//         path.push("monitor");
//         path.push("db");

//         if !path.exists() {
//             std::fs::create_dir_all(&path).expect("创建数据库目录失败");
//         }
//         path
//     })
// }

// pub async fn init_db() -> surrealdb::Result<()> {
//     let path = get_db_dir();

//     // 修复 1: RocksDb 期待的是 PathBuf 或 &Path (实现了 IntoEndpoint)
//     // 直接传入 path (它是 &PathBuf) 可能在某些版本下推导有问题
//     // 建议直接使用 .as_path() 或 path 变量
//     let db = Surreal::new::<RocksDb>(path.as_path()).await?;

//     db.use_ns("android").use_db("telemetry").await?;

//     let test_content = serde_json::json!({
//         "cpu_temp": 36.5,
//         "battery_level": 80,
//         "battery_temp": 30.0
//     });

//     // 手动插入一条测试数据
//     // let _: Option<serde_json::Value> = db.create("telemetry_history")
//     //     .content(serde_json::json!({"cpu_temp": 36.5, "battery_level": 80, "battery_temp": 30.0}))
//     //     .await?;

//     if DB_INSTANCE.set(db).is_err() {
//         eprintln!("警告: DB 全局变量已被设置过");
//     }
//     Ok(())
// }



pub async fn _rotate_data() -> surrealdb::Result<()> {
    if let Some(db) = DB_INSTANCE.get() {
        // 例如：只保留最后 10000 条数据，防止手机存储被撑爆
        let _ = db
            .query("DELETE telemetry_history ORDER BY id ASC LIMIT 1000")
            .await?;
    }
    Ok(())
}





/*
3. 处理“数据变大”的策略：索引与分区

当数据规模扩大时，查询速度会变慢。SurrealDB 提供了几种应对方案：

    索引（Index）：在 App 启动时确保关键字段（如 session_id, timestamp）已建立索引。

    字段投影：在 render 或 update 需要数据时，只查询需要的字段，而不是 SELECT *。

    分页加载：利用 SurrealDB 强大的 LIMIT 和 START 语法，只加载当前屏幕显示的 Session 数据。

4. 解决同步组件与异步 DB 的冲突

由于你的 Component::update 是同步的，你无法在里面直接 .await 数据库查询。

推荐方案：影子状态 (Shadow State)

    后台任务：在 tokio::spawn 中异步读取数据库。

    消息传递：查询完成后，通过 mpsc 发送给组件。

    同步更新：组件在同步的 update 中 try_recv 消息并更新 UI 缓存。

5. 配置文件与数据库的联动

既然引入了 HashMap 的动态配置，你可以将这些配置持久化到 SurrealDB 中。

    Config -> DB：将 config.toml 作为初始配置。

    运行时修改：用户在 TUI 界面修改了设置后，不仅更新 Arc<RwLock<Config>>，同时触发一条异步命令更新数据库中的 settings 表。

6. 准备清单 (Checklist)

    [ ] 安装 RocksDb 依赖：在 Linux 上可能需要安装 librocksdb-dev 或在 Windows 上配置 LLVM，因为嵌入式模式需要编译存储引擎。

    [ ] 定义模型实体：利用 serde 为你的数据实体（如 SessionRecord）派生 Serialize 和 Deserialize。

    [ ] 错误处理：由于数据库操作可能失败，建议定义一个 GlobalEvent::Error(String)，当数据库出问题时，能通过广播在 TUI 的状态栏显示警告。
*/
