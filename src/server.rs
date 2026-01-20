use ntex::web;
use crate::db;

#[web::get("/api/telemetry")]
async fn get_telemetry() -> impl web::Responder {
    let data = db::get_history(50).await;
    web::HttpResponse::Ok().json(&data)
}

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
        web::App::new()
            .service(get_telemetry)
            // .at("/status").get(|| async { "Online" })?
    })
    .bind(("0.0.0.0", 2000))?
    .run()
    .await
}