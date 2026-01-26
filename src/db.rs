use sqlx::{sqlite::SqliteConnectOptions, SqlitePool, Row};
use std::sync::OnceLock;
use std::str::FromStr;
use crate::constans::DATABASE_FILE;
use crate::prelude::AtlasPath;

static SQLITE_POOL: OnceLock<SqlitePool> = OnceLock::new();

pub struct Database;

impl Database {
    /// 初始化 SQLite 连接池
    pub async fn init() -> Result<(), String> {
        if SQLITE_POOL.get().is_some() { return Ok(()); }

        let db_path = AtlasPath::get().proj_dir.join(DATABASE_FILE);
        let opt = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
            .map_err(|e| e.to_string())?
            .create_if_missing(true);

        let pool = SqlitePool::connect_with(opt).await
            .map_err(|e| format!("SQLite Init Error: {}", e))?;
        
        SQLITE_POOL.set(pool).ok();
        Ok(())
    }

    /// 获取连接池句柄
    pub fn pool() -> &'static SqlitePool {
        SQLITE_POOL.get().expect("Database NOT initialized.")
    }

    /// [核心接口] 允许模块注册自己的表结构
    pub async fn setup_table(ddl: &str) -> Result<(), String> {
        sqlx::query(ddl)
            .execute(Self::pool())
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }   
}