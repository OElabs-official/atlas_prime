use futures::TryStreamExt as _;
// src/db.rs
use mongodb::{Client as MongoClient, Database, Collection, options::ClientOptions};
use mongodb::bson::{doc, Document};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use serde::{Serialize, de::DeserializeOwned};

use crate::prelude::AtlasPath;



impl AtlasPath {
    // ... 原有代码 ...

    /// 获取数据库密码文件路径
    pub fn get_db_pass_path() -> PathBuf {
        let p = Self::get();
        // 放在 proj_dir (Project Standard Dirs) 下的 dbpass.json
        p.proj_dir.join("dbpass.json")
    }

    /// 读取数据库凭据
    /// 返回值: (用户名, 密码)
    pub fn read_db_credentials() -> (String, String) {
        let path = Self::get_db_pass_path();
        
        // 1. 读取文件
        if let Ok(content) = fs::read_to_string(&path) {
            // 2. 解析 JSON Array: ["username", "password"]
            if let Ok(creds) = serde_json::from_str::<Vec<String>>(&content) {
                if creds.len() >= 2 {
                    return (creds[0].clone(), creds[1].clone());
                }
            }
        }

        // 3. 如果文件不存在或解析失败，返回默认值（或抛出错误）
        // 建议在初始化阶段如果没读到就报错提醒用户创建
        println!("⚠️  Warning: Could not read credentials from {:?}. Using defaults.", path);
        ("admin".to_string(), "password".to_string())
    }
}




static MONGO_CLIENT: OnceLock<MongoClient> = OnceLock::new();

pub struct Mongo;

impl Mongo {
/// 初始化连接（带认证）
    /// 建议在程序启动时调用一次，之后通过 client() 获取句柄
    pub async fn init(user: &str, pass: &str, host: &str, port: u16) -> Result<(), String> {
        if MONGO_CLIENT.get().is_some() { return Ok(()); }

        // 格式: mongodb://user:pass@host:port
        let uri = format!("mongodb://{}:{}@{}:{}", user, pass, host, port);
        
        let mut client_options = ClientOptions::parse(uri).await
            .map_err(|e| format!("URI Parse Error: {}", e))?;
        
        // 设置应用名称（方便在 MongoDB 日志中追踪）
        client_options.app_name = Some("AtlasPrime".to_string());

        let client = MongoClient::with_options(client_options)
            .map_err(|e| format!("Client Init Error: {}", e))?;
        
        MONGO_CLIENT.set(client).ok();
        Ok(())
    }

    /// 获取已初始化的客户端句柄
    pub async fn client() -> &'static MongoClient {
        MONGO_CLIENT.get().expect("Mongo Client NOT initialized. Call Mongo::init first.")
    }


    /// 通用：获取集合句柄
    pub async fn collection<T:Send+Sync>(db_name: &str, coll_name: &str) -> Collection<T> {
        Self::client().await.database(db_name).collection::<T>(coll_name)
    }

    /// 通用：保存（插入）数据
    pub async fn save<T:Send+Sync>(db_name: &str, coll_name: &str, data: T) -> Result<(), String> 
    where T: Serialize 
    {
        let coll = Self::collection::<T>(db_name, coll_name).await;
        coll.insert_one(data).await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// 通用：拉取最近的历史记录
    pub async fn fetch_recent<T>(db_name: &str, coll_name: &str, limit: i64) -> Vec<T>
    where T: DeserializeOwned + Send + Sync 
    {
        let coll = Self::collection::<T>(db_name, coll_name).await;
        let find_options = mongodb::options::FindOptions::builder()
            .sort(doc! { "timestamp": -1 }) // 默认按时间戳倒序
            .limit(limit)
            .build();

        match coll.find(doc! {}).with_options(find_options).await {
            Ok(mut cursor) => {
                let mut results = Vec::new();
                while let Ok(Some(item)) = cursor.try_next().await {
                    results.push(item);
                }
                results
            },
            Err(_) => vec![]
        }
    }
}