//! v1/web: ntex HTTP/JSON API 服务器（黑箱），只暴露 [`Web`]。
//!
//! # 公开接口用法
//! ```ignore
//! use crate::v1::web::Web;
//!
//! // 阻塞运行（自带 ntex System，不依赖外层 tokio）
//! Web::serve("127.0.0.1:8080").ok();
//! // 或后台线程：
//! let h = Web::spawn("127.0.0.1:8080".to_string());
//! ```
//!
//! # 路由表（与 v1 模块一一对应）
//! - `GET /system/cpu|thermal|mem|disk|ip` —— 普通采集，令牌桶 **20 req/s**；
//! - `GET /system/cli/battery` —— CLI 源采集（fork termux-api），**1 req/s**；
//!   超限不报错、不阻塞：**直接回上一次缓存 JSON**（响应头 `X-Cache: HIT` + `X-Cache-At`）；
//!   缓存语义 = 单槽最新（本服务是实时读数，旧副本只会驻留到下一次放行请求）；
//! - `GET  /task` 状态列表 / `POST /task/update` 重扫配置；
//! - `POST /task/run/{id}` `POST /task/stop/{id}` `GET /task/output/{id}`
//!   `POST /task/input/{id}`（JSON `{"line": "..."}`）；
//! - `GET /db/history?kind=&key=&limit=` —— sqlite 历史行（`kind` 必填：
//!   cpu_freq/temp/mem/swap/battery/disk；`key` 省略 = 该 kind 全键；`limit` 默认 300，钳位 1..=5000）。
//!   响应按时间**升序** `[{t,k,v}]`；DB 读走阻塞池（`Db::history` 内部驱动共享 runtime），不限流；
//! - `GET /` → 302 重定向到 `/index.html`；
//! - `GET /index.html` → 数据目录（与 v1/db 同源）下的 index.html 静态面板：
//!   不限流，**每次读盘**（走阻塞池，改动刷新即见），响应带 `cache-control: no-cache`
//!   杜绝浏览器启发式缓存；缺失/不可读 → 404 JSON；
//! - CORS 全放开（本地工具 API）；task / 静态路由不设限（写操作语义，重复提交自有幂等错误）。
//!
//! # 限流设计
//! 全局两个令牌桶（所有 /system 路由共享）：normal 容量20/回填20每秒，
//! cli 容量1/回填1每秒；/system/cli/* 需同时过两桶。桶拒绝且无缓存 → 放行（冷启动保护）。
//!
//! # 异步模型
//! 采集生产者（produce）跑在 ntex 阻塞线程池（`web::block`）上，HTTP worker 全程不阻塞；
//! 全局 Gate 锁只覆盖 `decide` 与 `store` 两个微临界区——produce / JSON 序列化期间
//! 锁已释放，缓存命中在 worker 上就地组包返回，不占线程池。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use directories::ProjectDirs;
use ntex::SharedCfg;
use ntex::http::error::BlockingError;
use ntex::web::{self, App, HttpResponse};
use ntex_cors::Cors;
use serde::Deserialize;

use crate::v1::PROJECT;
use crate::v1::system::{battery, cpu, disk, ip, mem, thermal};
use crate::v1::db::Db;
use crate::v1::task::{Task, TaskError};

// ---------------- 公开入口 ----------------

/// 唯一公开入口：web 服务生命周期管理
pub struct Web;

impl Web {
    /// 阻塞启动：内建 ntex System，可安全地从任意线程（含 tokio 主循环）调用。
    pub fn serve(bind: &str) -> std::io::Result<()> {
        use std::net::ToSocketAddrs;
        let addrs: Vec<std::net::SocketAddr> = bind.to_socket_addrs()?.collect();
        ntex::rt::System::new("atlas-web", ntex::rt::DefaultRuntime).block_on(async move {
            let srv = web::server(async |_| {
                App::new()
                    .middleware(Cors::new().max_age(600).build())
                    .route("/system/cpu", web::get().to(h_cpu))
                    .route("/system/thermal", web::get().to(h_thermal))
                    .route("/system/mem", web::get().to(h_mem))
                    .route("/system/disk", web::get().to(h_disk))
                    .route("/system/ip", web::get().to(h_ip))
                    .route("/system/cli/battery", web::get().to(h_cli_battery))
                    .route("/task", web::get().to(h_task_list))
                    .route("/task/update", web::post().to(h_task_update))
                    .route("/task/run/{id}", web::post().to(h_task_run))
                    .route("/task/stop/{id}", web::post().to(h_task_stop))
                    .route("/task/output/{id}", web::get().to(h_task_output))
                    .route("/task/input/{id}", web::post().to(h_task_input))
                    .route("/db/history", web::get().to(h_db_history))
                    .route("/", web::get().to(h_root))
                    .route("/index.html", web::get().to(h_index))
            });
            let mut srv = Some(srv);
            for a in addrs {
                let s = srv.take().expect("loop once per addr");
                srv = Some(s.bind(a, SharedCfg::default())?);
            }
            srv.expect("no address resolved").run().await
        })
    }

    /// 后台线程启动（返回线程句柄；serve 仅正常绑定失败才返回 Err）
    pub fn spawn(bind: String) -> JoinHandle<std::io::Result<()>> {
        std::thread::spawn(move || Self::serve(&bind))
    }
}

// ---------------- /system 路由（受限流+缓存保护） ----------------

async fn h_cpu() -> HttpResponse {
    respond("/system/cpu", false, || {
        serde_json::to_vec(&serde_json::json!({ "freq_ghz": cpu::freq_ghz() }))
    })
    .await
}
async fn h_thermal() -> HttpResponse {
    respond("/system/thermal", false, || {
        serde_json::to_vec(&thermal::Temp::get())
    })
    .await
}
async fn h_mem() -> HttpResponse {
    respond(
        "/system/mem",
        false,
        || serde_json::to_vec(&mem::Mem::get()),
    )
    .await
}
async fn h_disk() -> HttpResponse {
    respond("/system/disk", false, || {
        serde_json::to_vec(&serde_json::json!({ "disks": disk::Disk::get(), "dirs": disk::dirs() }))
    })
    .await
}
async fn h_ip() -> HttpResponse {
    respond("/system/ip", false, || serde_json::to_vec(&ip::Ip::get())).await
}
async fn h_cli_battery() -> HttpResponse {
    respond("/system/cli/battery", true, || {
        serde_json::to_vec(&battery::Battery::get())
    })
    .await
}

// ---------------- /task 路由（不限流） ----------------

async fn h_task_list() -> HttpResponse {
    HttpResponse::Ok().json(&Task::status())
}
async fn h_task_update() -> HttpResponse {
    // 重扫配置含磁盘 IO → 阻塞池；闭包必须返回 Result 才能进 web::block
    match web::block(move || Ok::<_, std::io::Error>(Task::update())).await {
        Ok(statuses) => HttpResponse::Ok().json(&statuses),
        Err(BlockingError::Error(e)) => {
            HttpResponse::InternalServerError().json(&serde_json::json!({ "error": e.to_string() }))
        }
        Err(BlockingError::Canceled) => HttpResponse::InternalServerError()
            .json(&serde_json::json!({ "error": "blocking pool closed" })),
    }
}
async fn h_task_run(id: web::types::Path<String>) -> HttpResponse {
    terr(
        Task::run(&id),
        || serde_json::json!({ "ok": id.as_str(), "action": "run" }),
    )
}
async fn h_task_stop(id: web::types::Path<String>) -> HttpResponse {
    terr(
        Task::stop(&id),
        || serde_json::json!({ "ok": id.as_str(), "action": "stop" }),
    )
}
async fn h_task_output(id: web::types::Path<String>) -> HttpResponse {
    match Task::output(&id) {
        Some(lines) => HttpResponse::Ok().json(&lines),
        None => HttpResponse::NotFound()
            .json(&serde_json::json!({ "error": format!("unknown task id: {id}") })),
    }
}

#[derive(Deserialize)]
struct InputIn {
    line: String,
}

async fn h_task_input(
    id: web::types::Path<String>,
    body: web::types::Json<InputIn>,
) -> HttpResponse {
    terr(
        Task::input(&id, &body.line),
        || serde_json::json!({ "ok": id.as_str(), "action": "input" }),
    )
}

/// TaskError → HTTP 语义映射（404 未知 id / 409 状态冲突 / 500 spawn 失败）
fn terr(r: Result<(), TaskError>, ok: impl FnOnce() -> serde_json::Value) -> HttpResponse {
    match r {
        Ok(()) => HttpResponse::Ok().json(&ok()),
        Err(e) => {
            let code = match &e {
                TaskError::UnknownId(_) => ntex::http::StatusCode::NOT_FOUND,
                TaskError::NotRunning(_) | TaskError::AlreadyRunning(_) => {
                    ntex::http::StatusCode::CONFLICT
                }
                TaskError::Spawn(_) => ntex::http::StatusCode::INTERNAL_SERVER_ERROR,
            };
            let mut b = HttpResponse::builder(code);
            b.json(&serde_json::json!({ "error": e.to_string() }))
        }
    }
}

// ---------------- 历史路由（sqlite 只读，不过 Gate） ----------------

#[derive(Deserialize)]
struct HistIn {
    kind: String,
    key: Option<String>,
    limit: Option<u32>,
}

/// `GET /db/history?kind=&key=&limit=` → 按时间升序 `[{t,k,v}]`。
/// DB 读（内含共享 runtime 的 block_on）上 ntex 阻塞池；limit 钳位 1..=5000 防爆量。
async fn h_db_history(q: web::types::Query<HistIn>) -> HttpResponse {
    let kind = q.kind.clone();
    let key = q.key.clone();
    let limit = q.limit.unwrap_or(300).clamp(1, 5000);
    match web::block(move || Ok::<_, std::io::Error>(Db::history(&kind, key.as_deref(), limit))).await {
        Ok(mut rows) => {
            rows.sort_by_key(|r| r.ts_secs); // 黑箱回新→旧，面板按旧→新吃
            let body: Vec<_> = rows
                .iter()
                .map(|r| serde_json::json!({ "t": r.ts_secs, "k": r.key, "v": r.value }))
                .collect();
            HttpResponse::Ok()
                .content_type("application/json; charset=utf-8")
                .header("cache-control", "no-store")
                .json(&body)
        }
        Err(BlockingError::Error(e)) => HttpResponse::InternalServerError()
            .json(&serde_json::json!({ "error": e.to_string() })),
        Err(BlockingError::Canceled) => HttpResponse::InternalServerError()
            .json(&serde_json::json!({ "error": "blocking pool closed" })),
    }
}

// ---------------- 静态路由（面板，不过 Gate、不缓存） ----------------

/// 数据目录下的 index.html（解析与 v1/db 的 db_path 同源：directories + PROJECT 常量）
fn index_path() -> Option<PathBuf> {
    static P: OnceLock<Option<PathBuf>> = OnceLock::new();
    P.get_or_init(|| {
        ProjectDirs::from(PROJECT[0], PROJECT[1], PROJECT[2])
            .map(|p| p.data_dir().join("index.html"))
    })
    .clone()
}

/// `GET /` → 302 指向静态面板
async fn h_root() -> HttpResponse {
    HttpResponse::Found()
        .header("location", "/index.html")
        .body(&b""[..])
}

/// `GET /index.html` → 数据目录 index.html。每次读盘（不缓存，改动刷新即见）；
/// 文件 IO 上 ntex 阻塞池，worker 不阻塞；缺失/不可读一律 404 JSON。
async fn h_index() -> HttpResponse {
    let Some(path) = index_path() else {
        return HttpResponse::NotFound()
            .json(&serde_json::json!({ "error": "no project data dir" }));
    };
    // block 把闭包内的 io::Error 平并进 BlockingError::Error → 读盘失败（缺失/不可读）一律 404
    match web::block(move || std::fs::read(&path)).await {
            Ok(body) => HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .header("cache-control", "no-cache")
                .body(body),
        Err(BlockingError::Error(e)) => HttpResponse::NotFound()
            .json(&serde_json::json!({ "error": format!("index.html: {e}") })),
        Err(BlockingError::Canceled) => HttpResponse::InternalServerError()
            .json(&serde_json::json!({ "error": "blocking pool closed" })),
    }
}

// ---------------- 限流 + 缓存黑箱 ----------------

/// 令牌桶：容量即突发额度，按时间回填
struct Bucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn new(capacity: f64, refill_per_sec: f64, now: Instant) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity,
            last: now,
        }
    }

    /// 取 1 枚令牌；不足返回 false 且不扣
    fn take(&mut self, now: Instant) -> bool {
        let dt = now
            .checked_duration_since(self.last)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        self.last = now;
        self.tokens = (self.tokens + self.refill_per_sec * dt).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

enum Decision {
    Proceed,
    Hit { at: SystemTime, body: Vec<u8> },
}

struct Gate {
    normal: Bucket,
    cli: Bucket,
    cache: HashMap<String, (SystemTime, Vec<u8>)>,
}

impl Gate {
    fn new(now: Instant) -> Self {
        Self {
            normal: Bucket::new(20.0, 20.0, now),
            cli: Bucket::new(1.0, 1.0, now),
            cache: HashMap::new(),
        }
    }

    /// 放行判定：桶全部通过 → Proceed；被拒且有缓存 → Hit；被拒但无缓存 → Proceed（冷启动）
    fn decide(&mut self, path: &str, cli: bool, now: Instant) -> Decision {
        let mut allowed = self.normal.take(now);
        if cli {
            allowed &= self.cli.take(now);
        }
        if !allowed {
            if let Some((at, body)) = self.cache.get(path) {
                return Decision::Hit {
                    at: *at,
                    body: body.clone(),
                };
            }
        }
        Decision::Proceed
    }

    fn store(&mut self, path: &str, body: Vec<u8>) {
        self.cache
            .insert(path.to_string(), (SystemTime::now(), body));
    }
}

fn gate() -> &'static Mutex<Gate> {
    static G: OnceLock<Mutex<Gate>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(Gate::new(Instant::now())))
}

/// 单一出口：判定（只在此持锁）→ 命中就地回缓存 / 放行则生产者上 ntex 阻塞池
/// （不持锁）→ 重新持锁写缓存后回 MISS。
///
/// `produce` 需 `Send + Sync + 'static`（ntex `web::block` 对闭包的硬要求），
/// 序列化错误与阻塞池关停（`Canceled`）统一映射为 500 JSON。
async fn respond(
    path: &'static str,
    cli: bool,
    produce: impl FnOnce() -> Result<Vec<u8>, serde_json::Error> + Send + Sync + 'static,
) -> HttpResponse {
    let decision = {
        let g = gate();
        let mut g = g.lock().unwrap_or_else(|e| e.into_inner());
        g.decide(path, cli, Instant::now())
    }; // 守卫在此释放：produce / 序列化期间不持 Gate 锁
    match decision {
        Decision::Hit { at, body } => {
            let secs = at
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            HttpResponse::Ok()
                .content_type("application/json; charset=utf-8")
                .header("x-cache", "HIT")
                .header("x-cache-at", secs.to_string())
                .body(body)
        }
        Decision::Proceed => match web::block(produce).await {
            // 闭包内的序列化错误由 block 统一包进 BlockingError::Error
            Ok(body) => {
                let g = gate();
                g.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .store(path, body.clone());
                HttpResponse::Ok()
                    .content_type("application/json; charset=utf-8")
                    .header("x-cache", "MISS")
                    .body(body)
            }
            Err(BlockingError::Error(e)) => HttpResponse::InternalServerError()
                .json(&serde_json::json!({ "error": e.to_string() })),
            Err(BlockingError::Canceled) => HttpResponse::InternalServerError()
                .json(&serde_json::json!({ "error": "blocking pool closed" })),
        },
    }
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn bucket_bursts_then_rejects_then_refills() {
        let now = Instant::now();
        let mut b = Bucket::new(3.0, 3.0, now);
        assert!(b.take(now) && b.take(now) && b.take(now));
        assert!(!b.take(now), "突发额度耗尽必须拒绝");
        assert!(b.take(t(400)), "400ms 后已回填 >1 枚");
    }

    #[test]
    fn cli_path_over_rate_returns_cached_body() {
        let now = Instant::now();
        let mut g = Gate::new(now);
        assert!(matches!(
            g.decide("/system/cli/battery", true, now),
            Decision::Proceed
        ));
        g.store("/system/cli/battery", b"{\"p\":42}".to_vec());
        // 立刻再来：cli 桶（容量1）必拒 → 回缓存
        match g.decide("/system/cli/battery", true, t(50)) {
            Decision::Hit { body, .. } => assert_eq!(body, b"{\"p\":42}"),
            Decision::Proceed => panic!("应命中缓存"),
        }
    }

    #[test]
    fn over_rate_without_cache_proceeds_once() {
        let now = Instant::now();
        let mut g = Gate::new(now);
        // 抽干 normal 桶
        for _ in 0..20 {
            assert!(g.decide("/system/cpu", false, now).matches_proceed());
        }
        // 无缓存的第 21 次：放行（冷启动保护），有缓存后转 HIT
        assert!(matches!(
            g.decide("/system/cpu", false, now),
            Decision::Proceed
        ));
        g.store("/system/cpu", b"{\"freq_ghz\":[]}".to_vec());
        assert!(matches!(
            g.decide("/system/cpu", false, now),
            Decision::Hit { .. }
        ));
    }

    trait Proceedish {
        fn matches_proceed(self) -> bool;
    }
    impl Proceedish for Decision {
        fn matches_proceed(self) -> bool {
            matches!(self, Decision::Proceed)
        }
    }

    /// 真机冒烟：起服务 → 裸 HTTP GET 两条路由 → 关停。
    /// `cargo test v1::web -- --ignored --test-threads=1`
    #[test]
    #[ignore]
    fn live_serve_smoke() {
        let h = Web::spawn("127.0.0.1:18099".to_string());
        std::thread::sleep(Duration::from_millis(300));
        let get = |path: &str| {
            use std::io::{Read, Write};
            let mut s = std::net::TcpStream::connect("127.0.0.1:18099").unwrap();
            write!(s, "GET {path} HTTP/1.0\r\n\r\n").unwrap();
            let mut buf = String::new();
            s.read_to_string(&mut buf).unwrap();
            buf
        };
        let mem = get("/system/mem");
        assert!(
            mem.contains("200 OK") && mem.contains("total_mem_mb"),
            "{mem}"
        );
        let tasks = get("/task");
        assert!(tasks.contains("200 OK"), "{tasks}");
        let hist = get("/db/history?kind=mem&key=used&limit=5");
        assert!(
            hist.contains("200 OK") && hist.contains("application/json"),
            "{hist}"
        );
        let nf = get("/nope");
        assert!(nf.contains("404"), "{nf}");
        // 静态面板：/ 必 302 → /index.html（头名大小写不敏感）
        let root = get("/");
        assert!(
            root.contains("302")
                && root.to_lowercase().contains("location: /index.html"),
            "{root}"
        );
        // /index.html：数据目录文件存在 → 200 text/html；缺失 → 404 JSON。两者皆可。
        let idx = get("/index.html");
        let idx_l = idx.to_lowercase();
        assert!(
            (idx.contains("200") && idx_l.contains("text/html"))
                || (idx.contains("404") && idx_l.contains("application/json")),
            "{idx}"
        );
        let _ = h;
    }
}
