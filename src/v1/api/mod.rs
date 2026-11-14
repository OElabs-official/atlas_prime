//! v1/api: ntex HTTP/JSON API 服务器（黑箱），只暴露 [`Web`]。
//!
//! # 公开接口用法
//! ```ignore
//! use crate::v1::api::Web;
//!
//! // 阻塞运行（自带 ntex System，不依赖外层 tokio）
//! Web::serve("127.0.0.1:8080").ok();
//! // 或后台线程：
//! let h = Web::spawn("127.0.0.1:8080".to_string());
//! ```
//!
//! # 路由表（与 v1 模块一一对应）
//! - `GET /system/cpu|thermal|mem|disk|ip` —— 普通采集，令牌桶 **20 req/s**；
//!   `ip` 路由对应 `system::network`（路径名沿用），payload 除地址三桶外带
//!   `"speed": {"<组>": {"rx_bps": f64, "tx_bps": f64}}`（**分组级**：`wifi` /
//!   `cellular` / 有流量的其他网卡名）——读 [`Operator::net_speeds`] 缓存
//!   （操作器 1Hz 回路喂数据），本路由绝不自读 /proc，首个 emit 前为 `{}`；
//!   别名 `GET /system/cli/network` 同挂本处理器
//!   （同一 payload、`speed` 两处同供，共桶共缓存槽，不走 cli 1 req/s 档）；
//! - `GET /system/gpu` —— GPU 频率/使用率（[`gpu::Gpu`] 原样序列化，kgsl 直读→su 桥）；
//!   与 cli/battery 同类也 fork 子进程，但坐**普通桶**：su spawn 毫秒级、且 respond 缓存兜底；
//!   永不再 404——不可用亦 200（`available:false`+`reason`，卡片隐藏系前端契约）；
//! - `GET /system/proc` —— 进程快照（[`proc::Snapshot`] 原样序列化）：**纯缓存读**
//!   （操作器 5s 回路 `proc::update` 喂数据，本路由绝不自扫 /proc），亦走普通桶，
//!   永 200；首轮前为 warming 形（`procs:[]`、`count:0`、`total_rss_kb:0`、
//!   `scanned_at` 为 null、`reason:"warming"`）；`procs` 按 `rss_kb`
//!   降序，成员 `pid/comm/cmd/rss_kb/cpu_pct/state`；
//! - `GET /system/cli/battery` —— CLI 源采集（fork termux-api），**1 req/s**；
//!   超限不报错、不阻塞：**直接回上一次缓存 JSON**（响应头 `X-Cache: HIT` + `X-Cache-At`）；
//!   缓存语义 = 单槽最新（本服务是实时读数，旧副本只会驻留到下一次放行请求）；
//! - `GET  /task` 状态列表 / `POST /task/update` 重扫配置；
//! - `POST /task/run/{id}` `POST /task/stop/{id}` `GET /task/output/{id}`
//!   `POST /task/input/{id}`（JSON `{"line": "..."}`）；
//! - `GET /db/history?kind=&key=&limit=` —— sqlite 历史行（`kind` 必填：
//!   cpu_freq/temp/mem/swap/battery/disk/net；`key` 省略 = 该 kind 全键；`limit` 默认 300，钳位 1..=5000）。
//!   `net` 的 key = `<组>.rx_bps` / `<组>.tx_bps`（组 = wifi/cellular/有流量的
//!   其他网卡名；bytes/s，1Hz，≈8h 回溯；旧 per-iface 键已退役，随窗口自然出清）。
//!   响应按时间**升序** `[{t:"YYYY-MM-DDTHH:MM:SSZ",k,v}]`——`t` 为 RFC3339 字符串原文
//!   （前端 `Date.parse` 直吃）；DB 读走阻塞池（`Db::history` 内部驱动共享 runtime），不限流；
//!   可选 `&fmt=arr`（增量契约）→ 同一序列的紧凑等价形 `[[ts_secs,"k",v],...]`
//!   （ts 改 unix 秒数值）；缺省/未知 fmt 值 = 默认对象形，默认字节对外契约不变；
//! - `GET /` → 302 重定向到 `/dashboard.html`；
//! - `GET /dashboard.html` → 面板前端（内嵌于二进制，改版 = 改 resources/dashboard.html 重编）：
//!   数据目录（与 v1/db 同源）存在 dashboard.html → **存在即覆盖**，每次读盘（走阻塞池，
//!   改动刷新即见，开发热替换；删文件回落内嵌）；否则直接回内嵌字节，**永不 404**。
//!   不限流，响应带 `cache-control: no-cache` 杜绝浏览器启发式缓存；
//! - `POST /tui/session` 发一次性令牌（body `{"id"}`，令牌绑定任务 id 与 kind；无
//!   PIN；404 unknown_task；**已有活会话 → `mode:"attach"` 免容量**；无会话满员 503
//!   busy；无会话 → `mode:"spawn"`）/ `GET /tui/ws?token=&cols=&rows=`
//!   PTY↔WS 桥（任务绑定**常驻** PTY 会话册，≤64 并发（`MAX_SESSIONS`），镜像多窗口，attach 回放
//!   ≤512KB 环形缓冲；令牌一次性 + 5min TTL，二进制帧 = 裸 PTY
//!   字节双向，文本帧 = resize/ping JSON 控制）/ `POST /tui/kill`（body `{"id"}`，
//!   幂等 `{"killed":bool}`）/ `GET /tui/sessions`（`{"sessions":[{"id","attached"}]}`）
//!   ——本域只管传输（[`tui`]），会话核心（PTY/子进程/槽位/会话册/回放环）在
//!   **task 域**（`task::tui`），不限流（WS 长连接）；
//! - `GET /vendor/{name}` → 内嵌前端资产（xterm 五件白名单名，`include_bytes!` 随二进制
//!   发布），响应带 `cache-control: public, max-age=86400`（24h），其余名字 404 JSON；
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

use ntex::SharedCfg;
use ntex::http::error::BlockingError;
use ntex::web::{self, App, HttpResponse};
use ntex_cors::Cors;
use serde::Deserialize;

use crate::v1::prelude;
use crate::v1::db::Db;
use crate::v1::operator::Operator;
use crate::v1::system::{battery, cpu, disk, gpu, mem, network, proc, thermal};
use crate::v1::task::{Task, TaskError};

mod tui;

// ---------------- 公开入口 ----------------

/// 唯一公开入口：web 服务生命周期管理
pub struct Web;

impl Web {
    /// 纯解析：bind 串 → 套接字地址列表；解析器错误以 Err 传播，
    /// 解析空列表亦 Err(InvalidInput)（见 [`Self::nonempty_or_err`]）。
    fn resolve_addrs(bind: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        use std::net::ToSocketAddrs;
        Self::nonempty_or_err(bind.to_socket_addrs()?.collect(), bind)
    }

    /// 纯守卫：空地址列表 → Err。**DO NOT** 改回 expect/unwrap——此处原是
    /// `srv.expect("no address resolved")` panic 点，panic=abort 下启动期崩溃
    /// 无声无息（GAP-8），必须以可上报的 Err 呈现。
    fn nonempty_or_err(
        addrs: Vec<std::net::SocketAddr>,
        bind: &str,
    ) -> std::io::Result<Vec<std::net::SocketAddr>> {
        if addrs.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("api bind '{bind}': 名称解析出空地址列表"),
            ));
        }
        Ok(addrs)
    }

    /// 启动探针（GAP-8 上报 seam，main 专用）：serve 监听期 `run()` 永不返回，
    /// web 线程只在解析/绑定失败（或服务器即刻退出）时提前结束——故在 `window`
    /// 内轮询 `is_finished()`（25ms 步进），提前结束即 join 取回真实结果：
    /// `Ok(handle)` = 仍在服务 = 监听成功（交回活句柄，调用方须保活至进程结束）；
    /// `Err(e)` = 绑定失败（错误消息供 main 直接上报，谎报 'listening' 不可能，
    /// 线程已 join 收殓）。线程 panic 亦归一化为 Err（不 panic 扩散）。
    pub fn probe_bind(
        handle: JoinHandle<std::io::Result<()>>,
        window: std::time::Duration,
    ) -> Result<JoinHandle<std::io::Result<()>>, std::io::Error> {
        let deadline = Instant::now() + window;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        if !handle.is_finished() {
            return Ok(handle);
        }
        match handle.join() {
            Ok(Err(e)) => Err(e),
            Ok(Ok(())) => Err(std::io::Error::other(
                "api server 启动后即刻退出（run() 提前返回）",
            )),
            Err(_) => Err(std::io::Error::other("api web 线程启动期 panic")),
        }
    }

    /// 阻塞启动：内建 ntex System，可安全地从任意线程（含 tokio 主循环）调用。
    /// 解析/绑定失败一律 `Err`（全路径无 panic 点），经 [`Self::spawn`] 的
    /// JoinHandle + [`Self::probe_bind`] 上报给 main。
    pub fn serve(bind: &str) -> std::io::Result<()> {
        let addrs = Self::resolve_addrs(bind)?;
        ntex::rt::System::new("atlas-web", ntex::rt::DefaultRuntime).block_on(async move {
            let srv = web::server(async |_| {
                App::new()
                    .middleware(Cors::new().max_age(600).build())
                    .route("/system/cpu", web::get().to(h_cpu))
                     .route("/system/thermal", web::get().to(h_thermal))
                     .route("/system/gpu", web::get().to(h_gpu))
                     .route("/system/proc", web::get().to(h_proc))
                    .route("/system/mem", web::get().to(h_mem))
                    .route("/system/disk", web::get().to(h_disk))
                    .route("/system/ip", web::get().to(h_network))
                    .route("/system/cli/network", web::get().to(h_network))
                    .route("/system/cli/battery", web::get().to(h_cli_battery))
                    .route("/task", web::get().to(h_task_list))
                    .route("/task/update", web::post().to(h_task_update))
                    .route("/task/run/{id}", web::post().to(h_task_run))
                    .route("/task/stop/{id}", web::post().to(h_task_stop))
                    .route("/task/output/{id}", web::get().to(h_task_output))
                    .route("/task/input/{id}", web::post().to(h_task_input))
                    .route("/db/history", web::get().to(h_db_history))
                     .route("/", web::get().to(h_root))
                     .route("/dashboard.html", web::get().to(h_dashboard))
                     .route("/tui/session", web::post().to(tui::h_session))
                     .route("/tui/ws", web::get().to(tui::h_ws))
                     .route("/tui/kill", web::post().to(tui::h_kill))
                     .route("/tui/sessions", web::get().to(tui::h_sessions))
                     .route("/vendor/{name}", web::get().to(tui::h_vendor))
             });
            // 逐址绑定：任一地址失败即 `?` 上抛 Err（addrs 已由 resolve_addrs
            // 保证非空，全路径无 expect/unwrap panic 点）
            let srv = addrs
                .into_iter()
                .try_fold(srv, |s, a| s.bind(a, SharedCfg::default()))?;
            srv.run().await
        })
    }

    /// 后台线程启动（返回线程句柄；serve 解析/绑定失败以 Err 随句柄返回，
    /// 由调用方经 [`Web::probe_bind`] 探取上报——main 必须探测，勿丢弃句柄）
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
/// `GET /system/gpu`：[`gpu::Gpu`] 原样序列化（频率/使用率，kgsl 直读→su 桥）。
/// 与 cli/battery 同类皆 fork，但留普通桶：su spawn 毫秒级、respond 缓存兜底；
/// 永不再 404——不可用亦 200 带 `available:false`（前端契约：卡片隐藏）。
async fn h_gpu() -> HttpResponse {
    respond("/system/gpu", false, || serde_json::to_vec(&gpu::Gpu::get())).await
}
/// `GET /system/proc`：[`proc::current`] **纯缓存读**（操作器 5s 回路喂数据，
/// 本路由绝不自扫 /proc；produce 上阻塞池，锁读即刻）。永 200；首轮前
/// warming 形（契约冻结：procs 空数组 + reason="warming"）。
async fn h_proc() -> HttpResponse {
    respond("/system/proc", false, || serde_json::to_vec(&proc::current())).await
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
/// `/system/ip`（路由名沿用，模块已迁 network）：地址枚举 + `"speed"` 成员。
/// speed 取自 [`Operator::net_speeds`] **纯缓存读**（1Hz 回路喂数据，本路由
/// 绝不自读 /proc）：分组级 `{"<组>": {"rx_bps": f64, "tx_bps": f64}}`
/// （组 = wifi/cellular/有流量的其他网卡名），首个 emit 前 `{}`。
/// 注：serde_json 默认按 key 升序出对象（无 preserve_order），payload 键序
/// 为字母序；[wifi, cellular, …] 的语义序由前端按组名显式排定。
/// 别名 `/system/cli/network`（CLI 风格命名，数据源现实为直读→su 桥链）挂
/// **同一处理器**：payload 逐字节一致、`speed` 成员两处同供；同处理器即
/// 同缓存槽（key=`/system/ip`）同限流档（normal 桶，不走 cli 1 req/s 档）。
/// 编码为**单遍**：[`IpWithSpeed`] 直接把四桶 + speed 写进序列化器（无
/// to_value→mutate→to_vec 双遍历），键序仍按字典序与旧路径逐字节一致。
async fn h_network() -> HttpResponse {
    respond("/system/ip", false, || {
        serde_json::to_vec(&IpWithSpeed {
            ip: network::Ip::get(),
            speed: Operator::net_speeds(),
        })
    })
    .await
}

/// `/system/ip` 单遍响应壳：[`network::Ip`] 四桶与 `"speed"` 摊平进同一 map，
/// 手写 Serialize 直写序列化器，免去旧路径的整树 to_value + 再 to_vec。
/// 条目按**字典序**发射（lan, loopback, public_v4, public_v6, speed）——旧路径
/// 经 Value::Object(BTreeMap) 亦按字典序出键，故 payload 逐字节不变；speed 组
/// 对象经 serde_json::Map（BTreeMap）同样升序，rx_bps/tx_bps 内层键序不变。
struct IpWithSpeed {
    ip: network::Ip,
    speed: Vec<(String, f64, f64)>,
}

impl serde::Serialize for IpWithSpeed {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let mut m = s.serialize_map(Some(5))?;
        m.serialize_entry("lan", &self.ip.lan)?;
        m.serialize_entry("loopback", &self.ip.loopback)?;
        m.serialize_entry("public_v4", &self.ip.public_v4)?;
        m.serialize_entry("public_v6", &self.ip.public_v6)?;
        let mut speed = serde_json::Map::new();
        for (iface, rx, tx) in &self.speed {
            speed.insert(iface.clone(), serde_json::json!({ "rx_bps": rx, "tx_bps": tx }));
        }
        m.serialize_entry("speed", &speed)?;
        m.end()
    }
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

/// TaskError → HTTP 语义映射（404 未知 id / 409 状态冲突 / 500 spawn 失败 /
/// 503 stdin 背压——有界队列满，客户端可退避重试，契约见 task 域 checkbox 21）
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
                TaskError::Backpressure(_) => ntex::http::StatusCode::SERVICE_UNAVAILABLE,
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
    /// 可选编码选择：仅精确 `arr` 启用紧凑数组形；缺省/未知/畸形值一律回落默认。
    fmt: Option<String>,
}

/// fmt → 编码门控（计划原文：unknown fmt value → default behavior）：
/// 只有大小写敏感的 `arr` 生效，其余（含 `ARR`/`array`/空串）全部默认。
fn fmt_wants_arr(fmt: Option<&str>) -> bool {
    matches!(fmt, Some("arr"))
}

/// 默认编码（外部稳定契约，Q2）：`[{t:<RFC3339>,k,v}]`，t = db TEXT 列原文。
/// 与历史内联 `.json(&Vec<Value>)` 出线逐字节一致（serde_json 无 preserve_order）。
fn hist_body_default(rows: &[crate::v1::db::Record]) -> String {
    let body: Vec<_> = rows
        .iter()
        .map(|r| serde_json::json!({ "t": r.ts, "k": r.key, "v": r.value }))
        .collect();
    serde_json::to_string(&body).unwrap_or_else(|_| "[]".to_string())
}

/// 紧凑编码（`fmt=arr`，GAP-2 冷启动载荷）：`[[ts_secs,"k",v],...]`——零逐行 Map，
/// 直接复用 SQL 投影的 `ts_secs`；键转义交 serde（自定义 net 组名可含引号/反斜杠）；
/// 非有限值 → null（与默认编码 json!(非有限)→null 同语义，解码两形等价）。
fn hist_body_arr(rows: &[crate::v1::db::Record]) -> String {
    let mut s = String::with_capacity(rows.len() * 32 + 2);
    s.push('[');
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('[');
        s.push_str(&r.ts_secs.to_string());
        s.push(',');
        s.push_str(&serde_json::to_string(&r.key).unwrap_or_else(|_| "\"\"".to_string()));
        s.push(',');
        if r.value.is_finite() {
            s.push_str(&r.value.to_string());
        } else {
            s.push_str("null");
        }
        s.push(']');
    }
    s.push(']');
    s
}

/// `GET /db/history?kind=&key=&limit=[&fmt=arr]` → 按时间升序。默认
/// `[{t:<RFC3339>,k,v}]`（`t` = db TEXT 列原文，外部契约）；`fmt=arr` 回落前缀外
/// 唯一形态 `[[ts_secs,"k",v],...]`（紧凑、同一序列的等价编码）；未知 fmt 值 = 默认。
/// 内部按 `ts_secs`（unix 秒投影）排序——迁移后全行统一 RFC3339 格式，字典序即时间序，二者等价。
/// DB 读（内含共享 runtime 的 block_on）上 ntex 阻塞池；limit 钳位 1..=5000 防爆量。
async fn h_db_history(q: web::types::Query<HistIn>) -> HttpResponse {
    let kind = q.kind.clone();
    let key = q.key.clone();
    let limit = q.limit.unwrap_or(300).clamp(1, 5000);
    let as_arr = fmt_wants_arr(q.fmt.as_deref());
    match web::block(move || Ok::<_, std::io::Error>(Db::history(&kind, key.as_deref(), limit)))
        .await
    {
        Ok(mut rows) => {
            rows.sort_by_key(|r| r.ts_secs); // 黑箱回新→旧，面板按旧→新吃
            let body = if as_arr {
                hist_body_arr(&rows)
            } else {
                hist_body_default(&rows)
            };
            HttpResponse::Ok()
                .content_type("application/json; charset=utf-8")
                .header("cache-control", "no-store")
                .body(body)
        }
        Err(BlockingError::Error(e)) => {
            HttpResponse::InternalServerError().json(&serde_json::json!({ "error": e.to_string() }))
        }
        Err(BlockingError::Canceled) => HttpResponse::InternalServerError()
            .json(&serde_json::json!({ "error": "blocking pool closed" })),
    }
}

// ---------------- 静态路由（面板，不过 Gate、不缓存） ----------------

/// 面板前端内嵌于二进制随发布（UI 随二进制发布；改版 = 改 resources/dashboard.html 重编）。
pub const DASHBOARD_HTML: &str = include_str!("../../../resources/dashboard.html");

/// 数据目录下的 dashboard.html（开发热替换覆盖文件；解析与 v1/db 的 db_path 同源：v1::prelude 数据目录）
fn dashboard_path() -> Option<PathBuf> {
    prelude::data_dir().map(|d| d.join("dashboard.html"))
}

/// 本次要服务的字节来源（判定见 [`pick_serve`]）。
enum Page {
    /// 覆盖文件的新鲜字节
    Fresh(Vec<u8>),
    /// 内嵌字节（默认答案）
    Embedded,
    /// 阻塞池关停（本路由唯一非 200 路径）
    PoolClosed,
}

/// 覆盖优先级（纯判定）：覆盖文件读成功 → 该文件优先；读失败（含缺失/不可读）→
/// 内嵌回落，本路由永不 404；仅阻塞池关停另走 500。
fn pick_serve(read: Result<Vec<u8>, BlockingError<std::io::Error>>) -> Page {
    match read {
        Ok(bytes) => Page::Fresh(bytes),
        Err(BlockingError::Error(_)) => Page::Embedded,
        Err(BlockingError::Canceled) => Page::PoolClosed,
    }
}

/// `GET /` → 302 指向静态面板
async fn h_root() -> HttpResponse {
    HttpResponse::Found()
        .header("location", "/dashboard.html")
        .body(&b""[..])
}

/// `GET /dashboard.html` → 面板。数据目录 dashboard.html 存在即覆盖（每次读盘，开发
/// 热替换；删文件回落内嵌）；否则直接回内嵌 [`DASHBOARD_HTML`]。文件 IO 上 ntex 阻塞池，
/// worker 不阻塞；内嵌恒有答案，本路由不再有 404。
async fn h_dashboard() -> HttpResponse {
    let page = match dashboard_path() {
        Some(path) => pick_serve(web::block(move || std::fs::read(&path)).await),
        None => Page::Embedded,
    };
    match page {
        Page::Fresh(bytes) => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .header("cache-control", "no-cache")
            .body(bytes),
        Page::Embedded => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .header("cache-control", "no-cache")
            .body(DASHBOARD_HTML),
        Page::PoolClosed => HttpResponse::InternalServerError()
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

    // ---- checkbox 29（后半）：h_network 单遍编码与旧双遍路径逐字节同一 ----

    #[test]
    fn ip_with_speed_bytes_identical_to_legacy_double_pass() {
        // 参考实现 = 改造前的 to_value→mutate→to_vec 双遍路径原样
        fn legacy(ip: network::Ip, speeds: Vec<(String, f64, f64)>) -> Vec<u8> {
            let mut v = serde_json::to_value(ip).unwrap();
            let mut speed = serde_json::Map::new();
            for (iface, rx, tx) in speeds {
                speed.insert(iface, serde_json::json!({ "rx_bps": rx, "tx_bps": tx }));
            }
            v["speed"] = serde_json::Value::Object(speed);
            serde_json::to_vec(&v).unwrap()
        }
        let mk = || network::Ip {
            loopback: vec![("lo".into(), "127.0.0.1".into()), ("lo".into(), "::1".into())],
            lan: vec![("wlan0".into(), "192.168.1.7".into())],
            public_v4: Some("203.0.113.9".into()),
            public_v6: vec!["2001:db8::42".into()],
        };
        // 组名乱序喂入，钉死两侧都按 BTreeMap 字典序出键
        let sp = || {
            vec![
                ("wlan0".to_string(), 1.5_f64, 2.25_f64),
                ("wifi".to_string(), 12345.0, 67890.0),
                ("cellular".to_string(), 0.0, 0.0),
            ]
        };
        assert_eq!(
            serde_json::to_vec(&IpWithSpeed { ip: mk(), speed: sp() }).unwrap(),
            legacy(mk(), sp()),
            "单遍编码 payload 必须与旧双遍路径逐字节一致"
        );
        // 边界形：全空 + public_v4=None（首轮 / 离线负缓存形态）
        assert_eq!(
            serde_json::to_vec(&IpWithSpeed { ip: network::Ip::default(), speed: vec![] }).unwrap(),
            legacy(network::Ip::default(), vec![]),
        );
    }

    // ---- checkbox 21：stdin 背压的 HTTP 语义必须钉死为 503（非肉眼判读）----

    #[test]
    fn terr_maps_backpressure_to_503() {
        let r = terr(
            Err(TaskError::Backpressure("t1".into())),
            || serde_json::json!({ "ok": "t1", "action": "input" }),
        );
        assert_eq!(
            r.status(),
            ntex::http::StatusCode::SERVICE_UNAVAILABLE,
            "有界泵队列满必须以 503 回绝（客户端退避重试契约）"
        );
    }

    // ---- GAP-8：bind 失败必须经探针 seam 上报，不得静默谎报 / panic-abort ----

    /// 占位监听先占死端口 → `Web::spawn` + `Web::probe_bind` 必报
    /// `Err(AddrInUse)`（main 据此打印 FAILED 并 exit 1；修复前此错误被
    /// 丢弃，main 无条件打印 'listening' 谎报）。
    #[test]
    fn probe_bind_reports_addr_in_use() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").expect("占位端口");
        let addr = holder.local_addr().expect("占位地址").to_string();
        let h = Web::spawn(addr.clone());
        // Err 形即线程已被 probe join 收殓（成功句柄只在 Ok 形交回）——
        // 线程 panic-abort 会直接带崩测试进程，能走到断言即证明非 panic 路径。
        match Web::probe_bind(h, Duration::from_millis(500)) {
            Ok(_) => panic!("端口被占：probe_bind 必报 Err，不得谎报 listening"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse, "实为: {e}"),
        }
    }

    /// stale_state 反证：端口空出后第二次尝试照常监听 → probe 报 Ok 且线程存活。
    #[test]
    fn probe_bind_ok_and_server_alive_on_free_port() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").expect("占位端口");
        let addr = holder.local_addr().expect("占位地址").to_string();
        drop(holder); // 空出端口再复用（loopback 瞬态端口重绑竞态可忽略）
        let h = Web::spawn(addr.clone());
        let live = Web::probe_bind(h, Duration::from_millis(500))
            .unwrap_or_else(|e| panic!("空闲端口 {addr} 须报 Ok(listening): {e}"));
        assert!(!live.is_finished(), "绑定成功后 run() 须常驻（线程不得提前退出）");
    }

    /// 纯部分（计划验收）：解析空列表 → Err 路径（原 `expect("no address resolved")`
    /// panic 点，panic=abort 下为无声崩溃）；非法输入 → 解析器 Err 传播；合法字面地址 → 非空。
    #[test]
    fn resolver_empty_yields_err_not_panic() {
        let e = Web::nonempty_or_err(vec![], "x:1")
            .expect_err("空地址列表必走 Err（替代原 expect panic 点）");
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "实为: {e}");
        assert!(Web::resolve_addrs("!!not a host!!").is_err(), "缺端口须 Err");
        assert!(Web::resolve_addrs("!!not a host!!:80").is_err(), "不可解析主机须 Err");
        assert!(!Web::resolve_addrs("127.0.0.1:0").unwrap().is_empty(), "字面地址须非空");
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

    #[test]
    fn dashboard_override_wins_embedded_fallback() {
        // 覆盖文件读到字节 → 该文件优先（存在即覆盖）
        assert!(matches!(
            pick_serve(Ok(b"<html>override".to_vec())),
            Page::Fresh(_)
        ));
        // 读失败（缺失/不可读）→ 内嵌回落，本路由永不 404
        assert!(matches!(
            pick_serve(Err(BlockingError::Error(std::io::Error::from(
                std::io::ErrorKind::NotFound
            )))),
            Page::Embedded
        ));
        // 阻塞池关停 → 500 语义，不冒充内嵌面板
        assert!(matches!(
            pick_serve(Err(BlockingError::Canceled)),
            Page::PoolClosed
        ));
        // 内嵌字节必须指向真文件（根目录 dashboard.html 的 rev 标记随编译进二进制）
        assert!(DASHBOARD_HTML.contains("<!-- rev "));
    }

    // ---------- /db/history 编码（task-28：默认字节金样 + fmt=arr 等价） ----------

    fn rec(ts: &str, secs: i64, k: &str, v: f64) -> crate::v1::db::Record {
        crate::v1::db::Record {
            ts: ts.to_string(),
            ts_secs: secs,
            kind: "battery".to_string(),
            key: k.to_string(),
            value: v,
        }
    }

    /// 金样钉死（外部契约 Q2）：默认编码 = 今日内联 `.json(&Vec<Value>)` 的字节
    /// （serde_json 无 preserve_order → 键序 k,t,v；f64 走 ryu 最短往返，88.0 → "88.0"）。
    #[test]
    fn history_default_body_is_byte_golden() {
        let rows = [
            rec("2026-11-14T12:00:00+00:00", 1794657600, "pct", 88.0),
            rec("2026-11-14T12:02:00+00:00", 1794657720, "volt_mv", 4200.0),
            rec("2026-11-14T12:04:00+00:00", 1794657840, "current_ua", -1234.5),
        ];
        assert_eq!(
            hist_body_default(&rows),
            concat!(
                r#"[{"k":"pct","t":"2026-11-14T12:00:00+00:00","v":88.0},"#,
                r#"{"k":"volt_mv","t":"2026-11-14T12:02:00+00:00","v":4200.0},"#,
                r#"{"k":"current_ua","t":"2026-11-14T12:04:00+00:00","v":-1234.5}]"#
            )
        );
    }

    /// arr 形态金样：`[[ts_secs,"k",v],...]`——无逐行 Map；键经 serde 转义；
    /// 非有限值 → null（与默认编码 json!(NaN)→null 同语义）。
    #[test]
    fn history_arr_body_is_compact_golden() {
        let rows = [
            rec("2026-11-14T12:00:00+00:00", 1794657600, "pct", 88.0),
            rec("2026-11-14T12:02:00+00:00", 1794657720, "a\"b\\c", 4.25),
            rec("2026-11-14T12:04:00+00:00", 1794657840, "temp_c", f64::NAN),
        ];
        assert_eq!(
            hist_body_arr(&rows),
            r#"[[1794657600,"pct",88],[1794657720,"a\"b\\c",4.25],[1794657840,"temp_c",null]]"#
        );
    }

    /// 解码等价（反“假成功”：逐元素比对解出的三元组序列，不是比长度）：
    /// 同一批行经两种编码出线，解码 → (unix 秒, 键, 值) 序列必须逐项相等。
    #[test]
    fn history_two_encodings_decode_to_same_series() {
        let rows = [
            rec("2026-11-14T12:00:00+00:00", 1794657600, "pct", 88.0),
            rec("2026-11-14T12:02:00+00:00", 1794657720, "current_ua", -0.0),
            rec("2026-11-14T12:04:00+00:00", 1794657840, "temp_c", 0.3),
            rec("2026-11-14T12:06:00+00:00", 1794657960, "volt_mv", 1e21),
            rec("2026-11-14T12:08:00+00:00", 1794658080, "cur\"2", f64::NAN),
            rec("2026-11-14T12:10:00+00:00", 1794658200, "health", -350123.75),
        ];
        // 默认编码：t = RFC3339 串 → 解析回 unix 秒（消费者视角）
        let def: Vec<serde_json::Value> =
            serde_json::from_str(&hist_body_default(&rows)).expect("默认编码必须是合法 JSON");
        let want: Vec<(i64, String, Option<f64>)> = def
            .iter()
            .map(|r| {
                let t = chrono::DateTime::parse_from_rfc3339(r["t"].as_str().unwrap())
                    .expect("t 必须是 RFC3339")
                    .timestamp();
                (t, r["k"].as_str().unwrap().to_string(), r["v"].as_f64())
            })
            .collect();
        // arr 编码：三元组直取
        let arr: Vec<serde_json::Value> =
            serde_json::from_str(&hist_body_arr(&rows)).expect("arr 编码必须是合法 JSON");
        let got: Vec<(i64, String, Option<f64>)> = arr
            .iter()
            .map(|r| {
                let r = r.as_array().expect("arr 行必须是三元组数组");
                (
                    r[0].as_i64().expect("ts_secs 必须是整数"),
                    r[1].as_str().expect("键必须是字符串").to_string(),
                    r[2].as_f64(),
                )
            })
            .collect();
        assert_eq!(got.len(), want.len(), "行数必须一致");
        assert_eq!(got, want, "两种编码解码后的序列必须逐项相等");
    }

    /// fmt 门控（计划原文：unknown fmt value → default behavior）：只有精确 `arr` 生效。
    #[test]
    fn history_fmt_gate_accepts_only_exact_arr() {
        assert!(fmt_wants_arr(Some("arr")));
        for bad in [None, Some(""), Some("ARR"), Some("array"), Some("arr "), Some("json")] {
            assert!(!fmt_wants_arr(bad), "{bad:?} 必须回落默认编码");
        }
    }

    /// 载荷收益（验收线：kind=battery limit=4500 → arr ≤ 60% 默认字节）：
    /// 合成 640 轮 × 7 键 = 4480 行真实形态 battery 样本实测字节数。
    #[test]
    fn history_arr_payload_is_at_most_60_percent_of_default() {
        const T0: i64 = 1794528000; // 2026-11-13T00:00:00+00:00
        let mut rows = Vec::new();
        for round in 0..640i64 {
            let secs = T0 + round * 120;
            let ts = chrono::DateTime::from_timestamp(secs, 0)
                .expect("合成秒必须可析")
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let stamp = chrono::DateTime::parse_from_rfc3339(&ts).expect("合成 ts 必须合法");
            assert_eq!(stamp.timestamp(), secs, "合成 ts/ts_secs 必须同一时刻");
            rows.push(rec(&ts, secs, "pct", (round % 100) as f64));
            rows.push(rec(&ts, secs, "volt_mv", 4000.0 + round as f64));
            rows.push(rec(&ts, secs, "current_ua", -(300_000.0 + round as f64 * 7.5)));
            rows.push(rec(&ts, secs, "temp_c", 31.0 + (round % 9) as f64 * 0.4));
            rows.push(rec(&ts, secs, "status", 1.0));
            rows.push(rec(&ts, secs, "plugged", 0.0));
            rows.push(rec(&ts, secs, "health", 2.0));
        }
        let def = hist_body_default(&rows);
        let arr = hist_body_arr(&rows);
        let ratio = arr.len() as f64 / def.len() as f64;
        println!(
            "fmt=arr 载荷实测：{} B vs 默认 {} B → {:.1}%（验收线 ≤ 60%）",
            arr.len(),
            def.len(),
            ratio * 100.0
        );
        assert!(ratio <= 0.60, "arr 载荷 {}/{} = {:.1}% 必须 ≤ 60%", arr.len(), def.len(), ratio * 100.0);
    }

    /// 真机冒烟：起服务 → 裸 HTTP GET 各路由 → 操作器 1Hz 速度回路两拍 →
    /// speed 成员 + kind=net 历史 → 关停。
    /// `cargo test v1::api -- --ignored --test-threads=1`
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
        // 先落一轮，保证 /db/history 必有行可验
        let _ = crate::v1::operator::Operator::sample_now();
        let hist = get("/db/history?kind=mem&key=used&limit=5");
        assert!(
            hist.contains("200 OK") && hist.contains("application/json"),
            "{hist}"
        );
        // 契约：t = RFC3339 字符串，按时间升序（以解析后 unix 秒验证序）
        let body = hist.split_once("\r\n\r\n").map_or("", |p| p.1);
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(body).expect("/db/history 应回 JSON 数组");
        assert!(!rows.is_empty(), "/db/history 应非空：{body}");
        let mut stamps = Vec::new();
        for r in &rows {
            let t = r
                .get("t")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("t 必须是字符串：{r}"));
            stamps.push(
                chrono::DateTime::parse_from_rfc3339(t)
                    .unwrap_or_else(|e| panic!("t 必须可析为 RFC3339，实为 {t}：{e}"))
                    .timestamp(),
            );
        }
        assert!(
            stamps.windows(2).all(|w| w[0] <= w[1]),
            "/db/history 必须升序：{stamps:?}"
        );
        // task-28 契约（活体面）：fmt=arr 紧凑形 + 头 parity + 未知 fmt 回落。
        // 此刻操作器未 start（kind=mem 无新写），两请求间数据恒定，可逐字节比。
        let hist_arr = get("/db/history?kind=mem&key=used&limit=5&fmt=arr");
        assert!(hist_arr.contains("200 OK"), "{hist_arr}");
        let hline = |raw: &str, name: &str| {
            raw.split("\r\n")
                .find(|l| l.to_ascii_lowercase().starts_with(&format!("{name}:")))
                .unwrap_or("")
                .to_ascii_lowercase()
        };
        // 缓存/类型头与默认编码逐字节一致（计划：本任务不动任何头）
        for name in ["content-type", "cache-control"] {
            assert_eq!(
                hline(&hist, name),
                hline(&hist_arr, name),
                "{name} 头必须与默认编码一致（默认：{hist}）"
            );
        }
        let arr_body = hist_arr.split_once("\r\n\r\n").map_or("", |p| p.1);
        let arows: Vec<serde_json::Value> =
            serde_json::from_str(arr_body).expect("fmt=arr 应回 JSON 数组");
        assert_eq!(arows.len(), rows.len(), "两种编码行数必须一致：{arr_body}");
        for (a, o) in arows.iter().zip(&rows) {
            let a = a.as_array().expect("arr 行必须是三元组数组");
            assert_eq!(a.len(), 3, "arr 行必须是三元组：{a:?}");
            assert_eq!(a[1].as_str().unwrap(), o["k"].as_str().unwrap());
            assert_eq!(a[2].as_f64(), o["v"].as_f64());
            let t = chrono::DateTime::parse_from_rfc3339(o["t"].as_str().unwrap())
                .unwrap()
                .timestamp();
            assert_eq!(a[0].as_i64().unwrap(), t, "arr 秒与默认 t 必须同一时刻");
        }
        // 未知 fmt → 默认行为：与默认响应体逐字节相同
        let hist_bad = get("/db/history?kind=mem&key=used&limit=5&fmt=ARR");
        assert_eq!(
            hist_bad.split_once("\r\n\r\n").map_or("", |p| p.1),
            body,
            "未知 fmt 必须回落默认字节"
        );
        // 网卡速度契约（域感知）：当前 SELinux 域存在可读数据源时，1Hz 回路
        // 两拍（基线 + 2 次 emit）后必须喂缓存并落 kind=net 且升序；sealed 域
        // （Termux untrusted_app 三级源全拒读，见 network::netlink 头）下
        // speed={} 与无 net 行同为正确行为。speed 对象本身无条件必须在。
        let seeded = !network::speed::snapshot().is_empty();
        Operator::start().expect("速度回路 start 应成功");
        std::thread::sleep(Duration::from_millis(2300));
        let ip = get("/system/ip");
        assert!(ip.contains("200 OK") && ip.contains("public_v4"), "{ip}");
        let net: serde_json::Value =
            serde_json::from_str(ip.split_once("\r\n\r\n").map_or("", |p| p.1))
                .expect("/system/ip 应回 JSON");
        let speed = net
            .get("speed")
            .and_then(|v| v.as_object())
            .expect("/system/ip payload 必须含 speed 对象");
        if seeded {
            assert!(!speed.is_empty(), "数据源可用时 2s+ 后 speed 必须已喂数: {net}");
            // 分组契约：speed 键 ⊆ {wifi, cellular} ∪ 非零其他（排除类名字
            // 任何情况下不得出现；其他键 = 见过流量的数据面网卡自组）
            for k in speed.keys() {
                let excluded = k == "lo"
                    || k == "br0"
                    || k.starts_with("dummy")
                    || k.starts_with("ifb")
                    || k.starts_with("tun")
                    || k.starts_with("rmnet");
                assert!(
                    k == "wifi" || k == "cellular" || !excluded,
                    "speed 键 ⊆ {{wifi, cellular}} ∪ 非零其他，实为 {k}: {net}"
                );
            }
            let hist_net = get("/db/history?kind=net&limit=1000");
            assert!(hist_net.contains("200 OK"), "{hist_net}");
            let net_rows: Vec<serde_json::Value> = serde_json::from_str(
                hist_net.split_once("\r\n\r\n").map_or("", |p| p.1),
            )
            .expect("kind=net 历史应回 JSON 数组");
            assert!(
                net_rows.len() >= 4,
                "两拍 × (rx+tx) 应 ≥4 行，实际 {}",
                net_rows.len()
            );
            assert!(
                net_rows.iter().any(|r| {
                    r.get("k")
                        .and_then(|v| v.as_str())
                        .is_some_and(|k| k.ends_with(".rx_bps") || k.ends_with(".tx_bps"))
                }),
                "key 应为 <组>.rx_bps/.tx_bps 形制: {net_rows:?}"
            );
            let mut stamps_net = Vec::new();
            for r in &net_rows {
                stamps_net.push(
                    chrono::DateTime::parse_from_rfc3339(
                        r["t"].as_str().expect("t 必须字符串"),
                    )
                    .expect("t 必须 RFC3339")
                    .timestamp(),
                );
            }
            assert!(
                stamps_net.windows(2).all(|w| w[0] <= w[1]),
                "kind=net 必须按时间升序: {stamps_net:?}"
            );
        } else {
            println!("sealed 域：speed 计量无数据源 → speed={speed:?}、无 kind=net 行为正确");
        }
        Operator::stop();
        let nf = get("/nope");
        assert!(nf.contains("404"), "{nf}");
        // 静态面板：/ 必 302 → /dashboard.html（头名大小写不敏感）
        let root = get("/");
        assert!(
            root.contains("302") && root.to_lowercase().contains("location: /dashboard.html"),
            "{root}"
        );
        // /dashboard.html：内嵌恒答（覆盖文件存在与否皆 200）→ 200 text/html + rev 标记
        let dash = get("/dashboard.html");
        let dash_l = dash.to_lowercase();
        assert!(
            dash.contains("200")
                && dash_l.contains("text/html")
                && dash.contains("<!-- rev "),
            "{dash}"
        );
        let _ = h;
    }
}
