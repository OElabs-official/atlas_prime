//! v1/api/tui: 浏览器 TUI 的 **WS 传输层**——路由、帧语义、HTTP 状态码映射。
//! 会话核心（PTY、任务子进程、令牌门、**常驻会话册**、回放环）在 [`task::tui`]：
//! task 域管子进程，web 域管字节管道（本模块零会话状态，纯搬运；拆除只退订，
//! **不杀会话**——回收唯 task 域四路径：子进程退出 / `/tui/kill` / 注册表
//! prune / 守护重启）。
//!
//! # 路由（均不过限流 Gate：WS 长连接、会话控制、vendor 静态）
//! - `POST /tui/session`（body `{"id":"<任务 id>"}`）—— 发放**绑定该任务**的一次性
//!   连接令牌（无 PIN），判定顺序：① 任务不在注册表 → 404 `{"error":"unknown_task"}`；
//!   ② 该任务**已有活会话** → 200 `{"token","mode":"attach"}`（attach 不查容量、
//!   不 spawn）；③ 无会话且满员 → 503 `{"error":"busy"}`；④ 无会话 → 200
//!   `{"token","mode":"spawn"}`；旧客户端只读 `token` 字段，`mode` 是增量；
//! - `GET /tui/ws?token=&cols=&rows=` —— 校验并消费令牌（任务 id 与 kind 已在
//!   令牌内）后升级 WS：spawn → 开会话+入册+订阅；attach → 查册（会话已消失 →
//!   404 `unknown_task`）→ **回放环形缓冲**（字节按事件边界成块发出）→ 订阅。
//!   此后两模式无差别：镜像广播、共享入向、resize 后到覆盖。坏/过期/已用令牌
//!   401 `bad_token`、spawn 超额 503 `busy`、会话起步失败 500。
//!   二进制帧 = 裸 PTY 字节（双向）；文本帧 = JSON 控制
//!   （`{"type":"resize","cols":..,"rows":..}` / `{"type":"ping"}`，回 `{"type":"pong"}`，
//!   未知帧忽略）；子进程退出先发 `{"type":"exit","code":<int|null>}` 再 Close。
//!   cols/rows 解析失败取默认 80/24（panic=abort 发行版，绝不在客户端输入上 panic）；
//! - `POST /tui/kill`（body `{"id"}`）—— 幂等击杀：有会话 → 击杀（子进程杀、
//!   槽位释放、全体订阅者收 exit 帧 + Close）回 `{"killed":true}`；无会话回
//!   `{"killed":false}`（恒 200）；
//! - `GET /tui/sessions` → `{"sessions":[{"id":"<任务 id>","attached":<n>}]}`
//!   （仅活会话）；
//! - `GET /vendor/{name}` —— 白名单前端资产（xterm 五件），字节 `include_bytes!` 内嵌
//!   随二进制发布；`cache-control: public, max-age=86400`（24h）；其余名字 404 JSON。
//!
//! # 传输结构（承重的并发设计在 task::tui，这里只是接驳）
//! pump 任务先灌回放块，再把订阅 `Receiver<TuiEvent>` 转发进 `WsSink`（std
//! mpsc 的阻塞收取走 ntex 阻塞池，IO 线程不阻塞）；入向 WS 帧由 `BridgeService`
//! 处理：Binary 帧直写 [`SessionRef`]，Text 帧是 JSON 控制消息（resize / ping）。
//! 拆除（shutdown / 句柄 drop）= 摘订阅 + attached--，**会话不死**；task 域
//! pump 线程继续喂回放环，等下一个窗口 attach。

use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use ntex::io::{DispatchItem, Reason};
use ntex::rt;
use ntex::service::{Ctx, Service};
use ntex::util::{ByteString, Bytes};
use ntex::web::{self, HttpRequest, HttpResponse};
use ntex::ws::error::WsError;
use serde::Deserialize;

use crate::v1::task::tui::{self, ReplayBuf, SessionRef, TuiEvent};

/// 浏览器入向 JSON 控制帧。
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ClientCtl {
    #[serde(rename = "resize")]
    Resize { cols: u16, rows: u16 },
    #[serde(rename = "ping")]
    Ping,
}

/// IO 线程任务：先灌回放块（attach 重挂画面 = 重放即得），再把订阅事件转发
/// 给浏览器。收束三源：socket 拆除（send 失败）、会话回收（exit 帧 / 通道拆净）、
/// 阻塞池关停。子进程退出时先发 `exit` 控制帧，再 Close。
async fn pump(sink: web::ws::WsSink, replay: ReplayBuf, rx: Receiver<TuiEvent>) {
    for chunk in replay {
        // 回放：按事件边界成块的字节流，xterm 按收序重放即复原画面
        if sink
            .send(web::ws::Message::Binary(Bytes::from(chunk)))
            .await
            .is_err()
        {
            return;
        }
    }
    // ntex 阻塞池闭包要求 Sync 捕获，std Receiver 只 Send——这把 Mutex
    // 从头到尾被本任务独占（每轮闭包持一份 Arc 克隆、阻塞收取），永不与人争。
    let rx = Arc::new(Mutex::new(rx));
    loop {
        // std mpsc 阻塞收取上 ntex 阻塞池（IO 线程绝不阻塞）；桥拆除时
        // detach 摘走 Sender → 通道拆净 → Err → 本任务收束，不占池线程。
        let rx2 = Arc::clone(&rx);
        match web::block(move || {
            let ev = rx2.lock().unwrap_or_else(|e| e.into_inner()).recv();
            Ok::<_, std::io::Error>(ev)
        })
        .await
        {
            Ok(Ok(TuiEvent::Bytes(b))) => {
                if sink
                    .send(web::ws::Message::Binary(Bytes::from(b)))
                    .await
                    .is_err()
                {
                    return; // 连接已断，dispatcher 自行收尾
                }
            }
            Ok(Ok(TuiEvent::Exit(code))) => {
                let frame = serde_json::json!({"type": "exit", "code": code});
                let text = ByteString::from(frame.to_string());
                if sink.send(web::ws::Message::Text(text)).await.is_err() {
                    return;
                }
                let _ = sink.send(web::ws::Message::Close(None)).await;
                return; // task 域 pump 已摘册回收，无尾随帧
            }
            Ok(Err(_)) => return, // 全部 Sender 消失（会话回收 / 已退订）
            Err(_) => return,     // 阻塞池关停
        }
    }
}

/// 单订阅 WS 服务。只跑在 ntex worker（IO）线程上，可随意用 `Cell`。
/// 本服务**不拥有会话**——只持一条订阅；drop = 退订（会话存续）。
struct BridgeService {
    /// pump 任务在首次就绪轮询（连接建立）时 spawn。
    started: Cell<bool>,
    /// 已回发过 Close 帧；再收 Close 就直接关 io。
    close_sent: Cell<bool>,
    /// 拆除开始后置位；入向帧此后丢弃。
    in_closed: Cell<bool>,
    /// 回放块 + 事件接收端，首次就绪轮询取走 spawn pump。
    src: RefCell<Option<(ReplayBuf, Receiver<TuiEvent>)>>,
    /// 会话订阅句柄（按键/粘贴 → PTY；resize → master）。drop / shutdown 退订。
    session: SessionRef,
}

impl Service<web::ws::WsSink, DispatchItem<web::ws::WsSink>> for BridgeService {
    type Res = Option<web::ws::Message>;
    type Error = WsError<Infallible>;

    async fn ready(&self, ctx: Ctx<'_, Self, web::ws::WsSink>) -> Result<(), Self::Error> {
        // 派发器在派发任何条目之前先轮询就绪——这是 WsSink 克隆最早存在的点。
        if !self.started.get() {
            self.started.set(true);
            if let Some((replay, rx)) = self.src.borrow_mut().take() {
                rt::spawn(pump(ctx.st().clone(), replay, rx));
            }
        }
        Ok(())
    }

    async fn shutdown(&self, _ctx: Ctx<'_, Self, web::ws::WsSink>) {
        // 拆除开始：停止喂 PTY + 退订（attached--，幂等）。**不杀会话**——
        // task 域 pump 继续喂回放环，等下一个窗口 attach。
        self.in_closed.set(true);
        self.session.detach();
    }

    async fn call(
        &self,
        item: DispatchItem<web::ws::WsSink>,
        ctx: Ctx<'_, Self, web::ws::WsSink>,
    ) -> Result<Self::Res, Self::Error> {
        use web::ws::{CloseCode, CloseReason, Frame, Message};

        match item {
            // 裸按键 / SGR 鼠标序列：直写会话入向（镜像共享输入）。
            DispatchItem::Item(Frame::Binary(b)) => {
                if !self.in_closed.get() {
                    self.session.send(b.to_vec());
                }
                Ok(None)
            }
            DispatchItem::Item(Frame::Text(t)) => {
                match serde_json::from_slice::<ClientCtl>(&t) {
                    Ok(ClientCtl::Resize { cols, rows }) => {
                        self.session.resize(cols, rows);
                        Ok(None)
                    }
                    Ok(ClientCtl::Ping) => Ok(Some(Message::Text(ByteString::from(
                        "{\"type\":\"pong\"}",
                    )))),
                    Err(_) => Ok(None), // 未知控制帧忽略
                }
            }
            DispatchItem::Item(Frame::Ping(p)) => Ok(Some(Message::Pong(p))),
            DispatchItem::Item(Frame::Pong(_)) => Ok(None),
            DispatchItem::Item(Frame::Close(_)) => {
                if !self.close_sent.get() {
                    self.close_sent.set(true);
                    Ok(Some(Message::Close(None))) // 先回声，等 ack
                } else {
                    ctx.st().io().close(); // 第二次 close：收尾
                    Ok(None)
                }
            }
            DispatchItem::Item(Frame::Continuation(_)) => Ok(None),
            DispatchItem::Control(_) => Ok(None),
            // Stop 原因映射照抄 ntex 自带 ws DispatchService。
            DispatchItem::Stop(Reason::Service) => Ok(Some(Message::Close(Some(CloseReason {
                code: CloseCode::Away,
                description: None,
            })))),
            DispatchItem::Stop(Reason::Io(None)) => Ok(None),
            DispatchItem::Stop(Reason::Io(e)) => Err(WsError::Disconnected(e)),
            DispatchItem::Stop(Reason::KeepAlive) => Err(WsError::KeepAlive),
            DispatchItem::Stop(Reason::ReadTimeout) => Err(WsError::ReadTimeout),
            DispatchItem::Stop(Reason::WriteTimeout) => Err(WsError::WriteTimeout),
            DispatchItem::Stop(Reason::Decoder(e)) => Err(WsError::Protocol(e)),
            DispatchItem::Stop(Reason::Encoder(e)) => Err(WsError::Protocol(e)),
        }
    }
}

/// 把 `a=1&b=2` 解析成自有对。令牌是服务端生成的 hex、尺寸是数字，
/// 这里无需 percent-decode。
fn parse_query(qs: &str) -> Vec<(String, String)> {
    qs.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k.to_owned(), v.to_owned())
        })
        .collect()
}

/// `POST /tui/session` 请求体：令牌要绑定的任务 id。JSON 缺失/无 id 由
/// ntex 萃取失败自动 400，无需手写判定。
#[derive(Debug, Deserialize)]
pub struct SessionIn {
    pub id: String,
}

/// `POST /tui/kill` 请求体：要击杀会话的任务 id。
#[derive(Debug, Deserialize)]
pub struct KillIn {
    pub id: String,
}

/// `POST /tui/session`：为指定任务签发一次性令牌（无 PIN）。判定顺序：
/// ① 任务不在注册表 404 → ② 已有活会话发 attach 令牌（**不查容量**，attach
/// 不占新槽）→ ③ 无会话且满员 503 → ④ 发 spawn 令牌。权威槽位预订仍在
/// [`task::tui::attach_or_spawn`] 的 spawn 路径，这里只是提前拒发省一次往返。
pub async fn h_session(body: web::types::Json<SessionIn>) -> HttpResponse {
    if !tui::task_exists(&body.id) {
        eprintln!("atlas: tui 任务 {} 不在注册表，拒发令牌", body.id);
        return HttpResponse::NotFound()
            .json(&serde_json::json!({ "error": "unknown_task" }));
    }
    if tui::session_snapshot()
        .iter()
        .any(|(id, _)| *id == body.id)
    {
        let token = tui::issue_token(&body.id, tui::TokenKind::Attach);
        return HttpResponse::Ok().json(&serde_json::json!({ "token": token, "mode": "attach" }));
    }
    if tui::at_capacity() {
        eprintln!("atlas: tui 会话已满，拒发 spawn 令牌");
        return HttpResponse::ServiceUnavailable()
            .json(&serde_json::json!({ "error": "busy" }));
    }
    let token = tui::issue_token(&body.id, tui::TokenKind::Spawn);
    HttpResponse::Ok().json(&serde_json::json!({ "token": token, "mode": "spawn" }))
}

/// `GET /tui/ws?token=..&cols=..&rows=..`：验令牌并取回（任务 id, kind），
/// 按 kind 走 [`task::tui::attach_or_spawn`]（task 域），然后把连接交给 WS
/// 派发器。服务 drop = 退订，会话存续。
pub async fn h_ws(req: HttpRequest) -> HttpResponse {
    let mut token = String::new();
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;
    for (k, v) in parse_query(req.query_string()) {
        match k.as_str() {
            "token" => token = v,
            "cols" => cols = v.parse().unwrap_or(80),
            "rows" => rows = v.parse().unwrap_or(24),
            _ => {}
        }
    }

    let Some((task_id, kind)) = tui::take_token(&token) else {
        eprintln!("atlas: tui 令牌无效/过期/已用，拒绝升级");
        return HttpResponse::Unauthorized()
            .json(&serde_json::json!({ "error": "bad_token" }));
    };
    let (session, replay, rx) = match tui::attach_or_spawn(&task_id, kind, cols, rows) {
        Ok(v) => v,
        Err(tui::TuiError::UnknownTask(_)) => {
            // spawn：签发后任务被摘除；attach：会话刚消失（退出/击杀）——
            // 都回 404，前端解绑/回未启动态。
            return HttpResponse::NotFound()
                .json(&serde_json::json!({ "error": "unknown_task" }));
        }
        Err(tui::TuiError::Busy) => {
            return HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({ "error": "busy" }));
        }
        Err(e) => {
            eprintln!("atlas: tui 任务 {task_id} {e}");
            return HttpResponse::InternalServerError()
                .json(&serde_json::json!({ "error": "pty_start_failed" }));
        }
    };

    let service = BridgeService {
        started: Cell::new(false),
        close_sent: Cell::new(false),
        in_closed: Cell::new(false),
        src: RefCell::new(Some((replay, rx))),
        session,
    };

    // 跑到连接断开为止；服务随调用点 drop → SessionRef 退订（会话不死，
    // task 域 pump 继续喂回放环）。
    match web::ws::start_with(&req, None, service).await {
        Ok(()) => eprintln!("atlas: tui ws 任务 {task_id} 派发器正常收束（会话已脱离）"),
        Err(e) => eprintln!("atlas: tui ws 任务 {task_id} 派发器以错误收束: {e:?}"),
    }
    HttpResponse::Ok().body(Bytes::new())
}

/// `POST /tui/kill` body `{"id"}`：幂等击杀会话（子进程杀、槽位释放、全体
/// 订阅者收 exit 帧 + Close）。恒 200：`{"killed":true|false}`。
pub async fn h_kill(body: web::types::Json<KillIn>) -> HttpResponse {
    let killed = tui::kill(&body.id);
    HttpResponse::Ok().json(&serde_json::json!({ "killed": killed }))
}

/// `GET /tui/sessions` → `{"sessions":[{"id","attached"}]}`（仅活会话；
/// 子进程退出即被 task 域 pump 摘册，不驻留）。
pub async fn h_sessions() -> HttpResponse {
    let sessions: Vec<_> = tui::session_snapshot()
        .into_iter()
        .map(|(id, attached)| serde_json::json!({ "id": id, "attached": attached }))
        .collect();
    HttpResponse::Ok().json(&serde_json::json!({ "sessions": sessions }))
}

// ---------------- /vendor 前端资产（内嵌，白名单五件） ----------------

/// 白名单资产：名字 → 内嵌字节 → MIME。路径相对本文件上溯至仓库根 _resources/vendor/。
const VENDOR: &[(&str, &[u8], &str)] = &[
    (
        "xterm.css",
        include_bytes!("../../../_resources/vendor/xterm.css"),
        "text/css; charset=utf-8",
    ),
    (
        "xterm.js",
        include_bytes!("../../../_resources/vendor/xterm.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "addon-fit.js",
        include_bytes!("../../../_resources/vendor/addon-fit.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "addon-webgl.js",
        include_bytes!("../../../_resources/vendor/addon-webgl.js"),
        "text/javascript; charset=utf-8",
    ),
    (
        "addon-unicode-graphemes.js",
        include_bytes!("../../../_resources/vendor/addon-unicode-graphemes.js"),
        "text/javascript; charset=utf-8",
    ),
];

/// `GET /vendor/{name}` → 白名单内回内嵌字节（24h 公共缓存），其余 404 JSON。
pub async fn h_vendor(name: web::types::Path<String>) -> HttpResponse {
    match VENDOR.iter().find(|(n, ..)| *n == name.as_str()) {
        Some((_, bytes, mime)) => HttpResponse::Ok()
            .content_type(*mime)
            .header("cache-control", "public, max-age=86400")
            .body(&bytes[..]),
        None => {
            HttpResponse::NotFound().json(&serde_json::json!({ "error": "not_found" }))
        }
    }
}
