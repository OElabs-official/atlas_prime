use std::collections::BTreeMap;

use ntex::web;


#[ntex::main]
pub async fn run_server() -> std::io::Result<()> {
    web::HttpServer::new(|| {
        web::App::new()
        // .service(get_telemetry)

        // .service(ai_query)
        // .service(universal_writer)
        // .at("/status").get(|| async { "Online" })
    })
    .bind(("0.0.0.0", 2000))?
    .run()
    .await
}
