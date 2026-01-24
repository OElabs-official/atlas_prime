use crate::db;
use ntex::web;

use crate::ui::info::TelemetryRecord;

#[web::get("/api/telemetry")]
async fn get_telemetry() -> impl web::Responder {
    // 获取最近 50 条强类型的遥测记录
    let data = TelemetryRecord::fetch_and_distribute(50).await;
    
    // 如果 data 是空的，fetch_recent 会返回空 Vec，API 依然返回 []
    web::HttpResponse::Ok().json(&data)
}

// #[web::get("/api/telemetry/hourly")]
// async fn get_telemetry_hourly() -> impl web::Responder {
//     // 这里可以调用我们之前设计的 archive 表查询逻辑
//     // 假设你在 info.rs 里也实现了一个 fetch_hourly
//     let data = TelemetryRecord::fetch_hourly(24).await; 
//     web::HttpResponse::Ok().json(&data)
// }

// #[web::get("/api/telemetry")]
// async fn get_telemetry() -> impl web::Responder {
//     let data = db::get_history(50).await;
//     web::HttpResponse::Ok().json(&data)
// }

#[web::get("/api/status")]
async fn status() -> impl web::Responder {
    web::HttpResponse::Ok().body("Online")
}

// 核心改动：使用同步函数启动，并在内部开启 ntex 的系统运行环境
// pub fn run_server() {
//     let sys = ntex::rt::System::new("web-server");

//     let server = web::server(|| {
//         web::App::new()
//             .service(get_telemetry)
//             .at("/status").get(|| async { "Online" })
//     })
//     .bind(("127.0.0.1", 8080))
//     .expect("无法绑定端口 8080");

//     println!("Web Server starting at http://127.0.0.1:8080");

//     // 启动服务器并阻塞该线程，直到系统被关闭
//     sys.run(|| {
//         let _ = server.run();
//     }).expect("ntex runtime 运行失败");
// }

#[ntex::main]
pub async fn run_server() -> std::io::Result<()> {
    web::HttpServer::new(|| {
        web::App::new().service(get_telemetry)
        .service(get_db_explorer)
        .service(raw_inspect)
        // .at("/status").get(|| async { "Online" })?
    })
    .bind(("0.0.0.0", 2000))?
    .run()
    .await
}


// src/server.rs (假设你的 ntex 路由在这里)

#[web::get("/api/db/explorer")]
async fn get_db_explorer() -> impl web::Responder {
    match crate::db::AtlasDB::get_full_report().await {
        Ok(report) => web::HttpResponse::Ok().json(&report),
        Err(e) => web::HttpResponse::InternalServerError().body(e.to_string()),
    }
}

#[web::post("/api/db/fix")]
async fn fix_old_data() -> impl web::Responder {
    let db = crate::db::AtlasDB::get();
    // 补全所有缺失的 timestamp
    let _ = db.query("UPDATE telemetry_history SET timestamp = time::now() WHERE timestamp = NONE").await;
    web::HttpResponse::Ok().body("Old data patched with current timestamp.")
}


#[web::get("/api/db/raw_inspect")]
async fn raw_inspect() -> impl web::Responder {
    let db = crate::db::AtlasDB::get();
    // 强制查看根信息，不切换任何上下文
    let mut res = db.query("INFO FOR ROOT").await.unwrap();
    let info: Option<serde_json::Value>  = res.take(0).unwrap_or_default();
    web::HttpResponse::Ok().json(&info)
}



// src/server.rs
#[web::post("/api/v1/record/{ns}/{db}/{table}")]
async fn universal_writer(
    path: web::types::Path<(String, String, String)>,
    content: web::types::Json<serde_json::Value>
) -> impl web::Responder {
    let (ns, db_name, table) = path.into_inner();
    let db = crate::db::AtlasDB::get();

    // 动态切换上下文并写入
    // SurrealDB 2.0 建议在 Query 中显式指定，或者在这里 use
    let sql = format!("USE NS {}; USE DB {}; INSERT INTO {} CONTENT $data", ns, db_name, table);
    
    match db.query(sql).bind(("data", content.into_inner())).await {
        Ok(_) => web::HttpResponse::Ok().json(&serde_json::json!({"status": "success"})),
        Err(e) => web::HttpResponse::InternalServerError().body(e.to_string()),
    }
}


#[web::post("/api/v1/ai/query")]
async fn ai_query(
    sql: String // 直接接收原始 SQL 字符串
) -> impl web::Responder {
    // 这里可以加上一些安全过滤逻辑，防止 AI 删库
    if sql.to_uppercase().contains("DELETE") {
         return web::HttpResponse::Forbidden().body("Read-only for AI");
    }
    
    let db = crate::db::AtlasDB::get();
    match db.query(sql).await {
        Ok(mut res) => {
            let val: Option<serde_json::Value> = res.take(0).unwrap_or_default();
            web::HttpResponse::Ok().json(&val)
        },
        Err(e) => web::HttpResponse::BadRequest().body(e.to_string()),
    }
}



/// 增强型动态写入网关
/// 路径：/api/v1/db/{ns}/{db}/{table}
#[web::post("/api/v1/db/{ns}/{db}/{table}")]
async fn api_db_writer(
    path: web::types::Path<(String, String, String)>,
    content: web::types::Json<serde_json::Value>,
) -> impl web::Responder {
    let (ns, db_name, table) = path.into_inner();
    let db = crate::db::AtlasDB::get();

    // 1. 构造“自动初始化 + 插入”的复合 SQL
    // 使用变量绑定 $data 彻底避免 SQL 注入风险
    let sql = format!(
        "DEFINE NAMESPACE IF NOT EXISTS {ns}; 
         USE NAMESPACE {ns}; 
         DEFINE DATABASE IF NOT EXISTS {db_name}; 
         USE DATABASE {db_name}; 
         INSERT INTO {table} CONTENT $data;"
    );

    // 2. 执行并处理结果
    match db.query(sql).bind(("data", content.into_inner())).await {
        Ok(mut res) => {
            // 检查最后一个语句（INSERT）是否成功
            if let Err(e) = res.check() {
                return web::HttpResponse::BadRequest().json(&serde_json::json!({
                    "error": e.to_string(),
                    "context": "Execution failed"
                }));
            }
            web::HttpResponse::Ok().json(&serde_json::json!({"status": "recorded"}))
        }
        Err(e) => web::HttpResponse::InternalServerError().body(e.to_string()),
    }
}

/// 增强型统计查询接口
#[web::get("/api/v1/db/stats/{ns}/{db}/{table}")]
async fn api_db_stats(
    path: web::types::Path<(String, String, String)>,
) -> impl web::Responder {
    let (ns, db_name, table) = path.into_inner();
    
    match crate::db::AtlasDB::get_stats(&ns, &db_name, &table).await {
        Ok((count, size)) => web::HttpResponse::Ok().json(&serde_json::json!({
            "table": table,
            "count": count,
            "disk_usage": size
        })),
        Err(e) => web::HttpResponse::InternalServerError().body(e.to_string()),
    }
}

/// AI 专用：动态 SQL 接口（受限）
#[web::post("/api/v1/db/query")]
async fn api_ai_query(
    body: String
) -> impl web::Responder {
    // 简单的安全审计：禁止 AI 执行删除操作
    let upper_sql = body.to_uppercase();
    if upper_sql.contains("REMOVE") || upper_sql.contains("DELETE") {
        return web::HttpResponse::Forbidden().body("Write/Delete operations are restricted for AI endpoint");
    }

    let db = crate::db::AtlasDB::get();
    match db.query(body).await {
        Ok(mut res) => {
            let val: Vec<serde_json::Value> = res.take(0).unwrap_or_default();
            web::HttpResponse::Ok().json(&val)
        }
        Err(e) => web::HttpResponse::BadRequest().body(e.to_string()),
    }
}
