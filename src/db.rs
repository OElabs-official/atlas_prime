use directories::{BaseDirs, ProjectDirs};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, RocksDb};

use crate::ui::info::AndroidBatInfo;

// 全局数据库实例
pub static DB: OnceLock<Surreal<Db>> = OnceLock::new();
// 数据库存储路径
static DB_PATH: OnceLock<PathBuf> = OnceLock::new();
// 内部结构体用于写入，方便后期维护和扩展
#[derive(Serialize)]
struct TelemetryRecord {
    cpu_temp: f32,
    battery_level: u8,
    battery_temp: f64,
}

pub fn get_db_dir() -> &'static PathBuf {
    DB_PATH.get_or_init(|| {
        let base_dirs = BaseDirs::new().expect("无法获取系统基础目录");

        // 获取数据根目录 (Linux/Android 为 ~/.local/share)
        let mut path = base_dirs.data_dir().to_path_buf();

        // 直接在根目录下创建你的项目文件夹
        path.push("monitor");
        path.push("db");

        if !path.exists() {
            std::fs::create_dir_all(&path).expect("创建数据库目录失败");
        }
        path
    })
}

pub async fn init_db() -> surrealdb::Result<()> {
    let path = get_db_dir();

    // 修复 1: RocksDb 期待的是 PathBuf 或 &Path (实现了 IntoEndpoint)
    // 直接传入 path (它是 &PathBuf) 可能在某些版本下推导有问题
    // 建议直接使用 .as_path() 或 path 变量
    let db = Surreal::new::<RocksDb>(path.as_path()).await?;

    db.use_ns("android").use_db("telemetry").await?;

    let test_content = serde_json::json!({
        "cpu_temp": 36.5,
        "battery_level": 80,
        "battery_temp": 30.0
    });

    // 手动插入一条测试数据
    // let _: Option<serde_json::Value> = db.create("telemetry_history")
    //     .content(serde_json::json!({"cpu_temp": 36.5, "battery_level": 80, "battery_temp": 30.0}))
    //     .await?;

    if DB.set(db).is_err() {
        eprintln!("警告: DB 全局变量已被设置过");
    }
    Ok(())
}

pub async fn record_telemetry(cpu: f32, bat: u8, bat_temp: f64) {
    if let Some(db) = DB.get() {
        // 修复 2: .create() 在 2.x 中返回的是 Option<T> 或特定的 Result
        // 我们通常只需要知道是否写入成功，不需要强制转换成 Vec
        let record = TelemetryRecord {
            cpu_temp: cpu,
            battery_level: bat,
            battery_temp: bat_temp,
        };

        // 在 SurrealDB 中，create 某个 table 返回的是单条记录 Option<T>
        // 如果你需要插入并忽略结果，直接让它推导为 Option 即可
        let _: Option<serde_json::Value> = db
            .create("telemetry_history")
            .content(record)
            .await
            .unwrap_or(None);
    }
}

pub async fn rotate_data() -> surrealdb::Result<()> {
    if let Some(db) = DB.get() {
        // 例如：只保留最后 10000 条数据，防止手机存储被撑爆
        let _ = db
            .query("DELETE telemetry_history ORDER BY id ASC LIMIT 1000")
            .await?;
    }
    Ok(())
}

/// 获取最近的历史记录
pub async fn _get_history(limit: usize) -> Vec<serde_json::Value> {
    if let Some(db) = DB.get() {
        // 尝试查询所有数据
        let sql = format!(
            "SELECT * FROM telemetry_history ORDER BY id DESC LIMIT {}",
            limit
        );
        let mut res = match db.query(sql).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("DB查询语法错误: {}", e);
                return vec![];
            }
        };

        // take(0) 代表取第一个查询语句的结果
        match res.take::<Vec<serde_json::Value>>(0) {
            Ok(v) => {
                // println!("DEBUG: 数据库查询到 {} 条记录", v.length());
                v
            }
            Err(e) => {
                eprintln!("解析数据失败: {}", e);
                vec![]
            }
        }
    } else {
        vec![]
    }
}

pub async fn get_history(limit: usize) -> Vec<serde_json::Value> {
    if let Some(db) = DB.get() {
        // 使用 type::string(id) 将 RecordId 显式转换为普通 String
        // 同时使用 VALUE 关键字获取干净的对象数组
        let sql = format!(
            "SELECT *, type::string(id) AS id FROM telemetry_history ORDER BY id DESC LIMIT {}",
            limit
        );

        let mut res = match db.query(sql).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("SQL 语法错误: {}", e);
                return vec![];
            }
        };

        // 获取第一个语句的结果
        match res.take::<Vec<serde_json::Value>>(0) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("核心解析失败: {:?}", e);
                // 如果还是解析失败，尝试打印第一条数据的原始形态，看看到底是什么
                vec![]
            }
        }
    } else {
        vec![]
    }
}

// 这里的类型要对应你的 (u8, String, f64) 结构
pub async fn get_bat_history_ui(limit: usize) -> Vec<AndroidBatInfo> {
    let raw_data = get_history(limit).await;
    raw_data
        .into_iter()
        .map(|v| {
            (
                v.get("battery_level").and_then(|l| l.as_u64()).unwrap_or(0) as u8,
                "History".to_string(), // 数据库里没存状态字符串，可以用占位符
                v.get("battery_temp")
                    .and_then(|t| t.as_f64())
                    .unwrap_or(0.0),
            )
        })
        .collect()
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
