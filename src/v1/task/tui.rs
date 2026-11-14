//! v1/task/tui: 浏览器 TUI 会话核心（**任务绑定**的常驻 PTY 会话册），移植自
//! atlas_prototype_group/atlas_agentflow；WS 传输在 `v1/api/tui.rs`，本模块
//! **零 ntex**——task 域管子进程，web 域管字节管道，接缝 = [`attach_or_spawn`]
//! 回 [`SessionRef`] + [`ReplayBuf`] + 订阅 `Receiver<TuiEvent>`，外加
//! [`kill`] / [`session_snapshot`] / [`prune_missing`] 三个控制面纯函数。
//! allow: SIZE_OK — 会话册与 Outbox/pump/reader/writer 是一条不可拆的并发
//! 流水线，原型已验证其承重设计（拆文件 = 重设计）。
//!
//! # 域职责
//! - 令牌门（模块级静态）：[`at_capacity`] / [`issue_token`] / [`take_token`]
//!   ——一次性令牌 TTL 300s，**令牌绑定任务 id 并携带 kind**（[`TokenKind::Spawn`]
//!   开新会话 / [`TokenKind::Attach`] 重挂既有会话）；签发顺带清过期；
//!   存活会话 ≤ 64，[`TuiGuard`] drop 释放槽位；
//! - **会话册** [`sessions`]：键 = 任务 id，一会话/任务。会话是**常驻**的，不再是
//!   ws 连接的附属——桥断开只是脱离（web 侧摘订阅）；回收只有四条路径：
//!   ① 子进程自然退出（pump 广播 exit 帧后摘册）② [`kill`]（幂等击杀）
//!   ③ [`prune_missing`]（`Task::update` 重扫发现任务消失）④ 守护进程重启
//!   （会话册是内存态，重启即清空）。册持有 [`TuiGuard`]（**摘册 = drop = 击杀
//!   子进程 + 释放槽位**）；输出回放环形缓冲（分块，总字节 ≤ 512KB，按事件块
//!   弹旧）与订阅列表在共享核里——多窗口 = 镜像共享（同看同输入），resize
//!   后到覆盖；
//! - [`attach_or_spawn`]（唯一入口，容量权威只在此的 spawn 路径）：Spawn 意图——
//!   已有会话 → **降级 attach**（不占新槽）；查不到 → 先查描述符（查不到 →
//!   [`TuiError::UnknownTask`]，不占槽）→ 占槽 → 开 PTY（请求尺寸）→ 在 PTY 里
//!   spawn **该任务描述符的命令**（[`task_cmd`]，与监督守护实例同一构造语义；
//!   TERM=xterm-256color + COLORTERM=truecolor 先注入、spec envs 可覆盖；
//!   cwd = desc.cwd → $HOME → /）→ 入册 → 订阅 → 拉起 pump 线程（F6：订阅先行，
//!   快速退出的子进程 exit 广播必有落点）。Attach 意图——
//!   查册（会话已消失 → [`TuiError::UnknownTask`]）→ 订阅。**回放快照与订阅注册
//!   同一临界区**（不重帧不漏帧）；
//! - 事件 [`TuiEvent::Exit`] = 子进程退出码（None = 拿不到）。
//!
//! # 第二实例语义
//! 会话里的任务是任务的 **PTY 第二实例**，与监督器的守护实例并存、互不干涉
//! （守护实例无 PTY，无法 re-attach）。常驻服务型任务（如 bun）的两实例可能抢
//! 同一端口——冲突如实打印在终端里，属预期，不设闹钟。
//!
//! # 并发模型（承重部分，逐行移植，勿重设计）
//! 每条会话独占两个阻塞 OS 线程（reader/writer）+ 一条 wait 线程 + **一条 task
//! 域 pump 转发线程**（`tui-pump-{sid}`；零 ntex，用 unpark-waker 驱动
//! [`OutboxRx::blocking_next`]）。PTY 输出经 `Outbox`（mutex + waker 的手写
//! 队列）递给 pump，pump 写回放环并广播给各订阅（std mpsc）；web 侧只泵自家
//! receiver：
//!
//! ```text
//!   task cmd <-> pty master
//!              |  try_clone_reader()        take_writer()
//!        reader thread                 writer thread
//!              |  Outbox (mutex+waker)          ^  std::sync::mpsc
//!              v                                |
//!        pump thread (ring + broadcast)   SessionRef (send/resize)
//! ```
//!
//! pump 是 task 域唯一的会话收束点：Bytes → 写 ring + 广播 subs（断线订阅者
//! 经 Disconnected 摘除）；Exit → 广播后**从册摘除本会话**——摘册 = guard drop
//! = 击杀子进程、释放槽位。ws 断 = web 侧 detach（摘订阅 + attached--），
//! 会话存续。
//!
//! # 锁序矩阵（外 → 内；反向嵌套或持锁跨广播 = 死锁）
//! 1. `sessions()` 册锁——最外；只有册操作（insert/remove/snapshot）可持，
//!    guard/killer 的 drop 一律放册锁之外；`reg`（任务注册表）→ 册锁只发生在
//!    `Task::update` 的 prune 一处，反向绝无；
//! 2. `SessionCore::subs` 锁——会话内锁的外层；
//! 3. `SessionCore::ring` 锁——会话内锁的内层，**只允许嵌在 subs 之下**
//!    （回放+订阅、pump 入块两个临界区同形：subs → ring）；
//! 4. `SessionCore::input` 锁——叶锁：send/resize 持它时不碰任何别的锁。
//!
//! 广播发送、Disconnected 摘除后的 retain、guard drop 均在持锁临界区外完成。

// allow: SIZE_OK —— 见文件头；整条流水线的单一职责 = "PTY 任务绑定会话册"。

use std::collections::{HashMap, VecDeque};
use std::env;
use std::fmt;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;
use std::time::{Duration, Instant};

use rand::RngExt;
use xpty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, PtySystem, native_pty_system};

use super::{TaskDescriptor, reg};

/// 并发存活会话硬上限。
const MAX_SESSIONS: usize = 64;
/// 一次性连接令牌寿命 5 分钟。
const TOKEN_TTL: Duration = Duration::from_secs(300);
/// 回放环形缓冲总字节上限（8 会话顶格 ≈ 4MB，Android 可接受）。
const RING_CAP_BYTES: usize = 512 * 1024;

// ---------------- 令牌门 + 并发槽位（模块级静态） ----------------

/// 令牌意图：Spawn = 无会话则开会话（有会话自动降级 attach）；Attach = 只重挂
/// 既有会话，永不占新槽、永不 spawn。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Spawn,
    Attach,
}

/// 已签发未消费的令牌 →（绑定的任务 id，意图，签发时刻）。
fn tokens() -> &'static Mutex<HashMap<String, (String, TokenKind, Instant)>> {
    static T: OnceLock<Mutex<HashMap<String, (String, TokenKind, Instant)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 存活会话计数（[`TuiGuard`] drop 时递减）。
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// 满员判定（签发侧提前拒，省一次往返；权威预订仍在 [`attach_or_spawn`] 的
/// spawn 路径——attach 永不占新槽，满员时照常放行）。
pub fn at_capacity() -> bool {
    LIVE.load(Ordering::Relaxed) >= MAX_SESSIONS
}

/// 签发一枚一次性令牌（绑定任务 id 与意图），顺带清掉过期条目。
pub fn issue_token(task_id: &str, kind: TokenKind) -> String {
    let token = random_hex();
    let mut t = tokens().lock().unwrap_or_else(|e| e.into_inner());
    t.retain(|_, (_, _, issued)| issued.elapsed() < TOKEN_TTL);
    t.insert(token.clone(), (task_id.to_owned(), kind, Instant::now()));
    token
}

/// 校验并消费：一枚令牌只配得上一条订阅。有效回绑定的（任务 id，意图）；
/// 过期/缺失/重放一律 `None`（消费即失效，与是否过期同码处理）。
pub fn take_token(token: &str) -> Option<(String, TokenKind)> {
    let mut t = tokens().lock().unwrap_or_else(|e| e.into_inner());
    match t.remove(token) {
        Some((task_id, kind, issued)) if issued.elapsed() < TOKEN_TTL => Some((task_id, kind)),
        _ => None,
    }
}

/// 任务是否活在注册表里（签发侧先行校验，省一次无效往返）。
pub fn task_exists(id: &str) -> bool {
    descriptor(id).is_some()
}

/// 读任务描述符快照（读锁只持一瞬间）。子模块可访问父模块私有项
/// （`reg` / `Slot.desc`），经 `use super::{reg, TaskDescriptor}` 引入。
fn descriptor(id: &str) -> Option<TaskDescriptor> {
    let r = reg().read().unwrap_or_else(|e| e.into_inner());
    r.get(id).map(|slot| slot.desc.clone())
}

/// 占一个会话槽位的守卫；drop 即释放。
struct SlotGuard;

impl Drop for SlotGuard {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 预订会话槽位，满员返回 `None`。
fn try_acquire() -> Option<SlotGuard> {
    LIVE.try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        if n < MAX_SESSIONS { Some(n + 1) } else { None }
    })
    .ok()
    .map(|_| SlotGuard)
}

/// 16 随机字节的小写 hex（32 字符）。
fn random_hex() -> String {
    let mut buf = [0u8; 16];
    rand::rng().fill(&mut buf[..]);
    let mut s = String::with_capacity(32);
    for byte in buf {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// 单调递增会话 id，仅用于日志关联。
fn next_sid() -> u64 {
    static SID: AtomicU64 = AtomicU64::new(1);
    SID.fetch_add(1, Ordering::Relaxed)
}

/// 单调递增订阅 id（detach / Disconnected 摘除的键）。
fn next_sub_id() -> u64 {
    static SUB: AtomicU64 = AtomicU64::new(1);
    SUB.fetch_add(1, Ordering::Relaxed)
}

/// PTY 会话的命令构造 = 任务描述符的 command + args（命令不经 shell 解释，
/// args 原样进 execve，怪字符无需转义）。交叉引用 `supervise()`（task/mod.rs
/// 的 `Command::new(&desc.command)` 构造处）——**两处命令构造须同步**：
/// 环境 = 继承 + `TERM=xterm-256color` + `COLORTERM=truecolor`（TUI 应用无 TERM
/// 拒跑），spec `envs` 后置注入、可覆盖这两项；cwd = `desc.cwd` → `$HOME`
/// （须为目录）→ `/`。
fn task_cmd(desc: &TaskDescriptor) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(&desc.command);
    cmd.args(&desc.args);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    if let Some(envs) = &desc.envs {
        // xpty 的 CommandBuilder 无 envs（std Command 有），逐条注入等效。
        for (k, v) in envs {
            cmd.env(k, v); // spec 可覆盖 TERM/COLORTERM
        }
    }
    let cwd = desc.cwd.clone().unwrap_or_else(|| {
        env::var("HOME")
            .ok()
            .filter(|h| Path::new(h).is_dir())
            .unwrap_or_else(|| "/".to_owned())
    });
    cmd.cwd(cwd);
    cmd
}

// ---------------- 公开类型 ----------------

/// 会话起步失败（细节进 stderr，传输层只做状态码映射）。
#[derive(Debug)]
pub enum TuiError {
    /// 任务 id 不在注册表，或 attach 时会话已消失（→ 404），携带 id。
    UnknownTask(String),
    /// 并发已满（→ 503，只可能发生在 spawn 路径）。
    Busy,
    /// PTY/线程起步失败（→ 500），携带归因文本。
    Pty(String),
}

impl fmt::Display for TuiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TuiError::UnknownTask(id) => write!(f, "任务不存在: {id}"),
            TuiError::Busy => write!(f, "会话已满"),
            TuiError::Pty(s) => write!(f, "pty 起步失败: {s}"),
        }
    }
}

/// PTY reader 线程 →（经 pump）订阅端的事件流。
pub enum TuiEvent {
    /// 裸 PTY 输出字节。
    Bytes(Vec<u8>),
    /// 子进程已退出（None = 拿不到退出码）。
    Exit(Option<i32>),
}

/// 会话守卫：drop 击杀子进程，保证 reader 线程的阻塞 read 以 EIO 返回，
/// 任何断连路径都不漏线程、不漏进程。守卫由**会话册**持有：四条回收路径
/// （子进程退出 / [`kill`] / [`prune_missing`] / 守护重启）摘册即 drop 它——
/// 不再是「传输层派发器收束后 drop」（那会把会话生命周期绑死在 ws 上）。
pub struct TuiGuard {
    sid: u64,
    /// 在下面的 kill 解除 `wait()` 阻塞之前，先把 wait 线程从 outbox 摘线。
    /// 字段顺序承重：Rust 按声明序自上而下 drop，故本字段先释放通道。
    _in_tx: SyncSender<Vec<u8>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    _slot: SlotGuard,
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        match self.killer.kill() {
            Ok(()) => eprintln!("atlas: tui sid={} 会话关闭，子进程已击杀", self.sid),
            Err(e) => {
                eprintln!("atlas: tui sid={} 击杀子进程失败（多半早已退出）: {e}", self.sid)
            }
        }
    }
}

/// PTY 入向有界队列深度（audit perf F11 同契约）：与 task stdin 泵同形,
/// 64 帧 × 64KiB 帧上限 = 每会话个位数 MiB 确定性内存界。队满丢帧（按键流
/// 的背压形态就是丢弃过载，绝不允许无界排队拖爆会话）。
const PTY_INPUT_QUEUE: usize = 64;

/// 入向句柄：按键/粘贴字节 + 改尺寸。共享核的 `Mutex<TuiInput>` 内容；
/// 桥拆除后 [`Self::send`] 自然失败。
pub struct TuiInput {
    in_tx: SyncSender<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
}

impl TuiInput {
    /// 写入 PTY（按键 / 粘贴 / SGR 鼠标序列的裸字节）。桥已拆则静默丢弃；
    /// 有界队列（[`PTY_INPUT_QUEUE`]）满时丢最旧不如丢最新——本帧直接丢弃,
    /// 与 task 域「队满立即背压」同语义（本路径无错误回通道，回 WS 即丢包）。
    fn send(&self, bytes: Vec<u8>) {
        if let Err(e) = self.in_tx.try_send(bytes)
            && matches!(e, TrySendError::Full(_)) {
                eprintln!("atlas: tui 入向队列满（{PTY_INPUT_QUEUE} 帧），丢弃一帧");
            }
    }

    /// 改 PTY 尺寸（行列）。多窗口共享同一 PTY，后到的 resize 覆盖先到的。
    fn resize(&self, cols: u16, rows: u16) {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        match self.master.resize(size) {
            Ok(()) => eprintln!("atlas: tui pty 改尺寸 {cols}x{rows}"),
            Err(e) => eprintln!("atlas: tui pty 改尺寸失败: {e}"),
        }
    }
}

/// 回放块序列：PTY 输出按**事件边界**成块（不切碎一个事件），旧 → 新。
/// attach 时整份灌给新订阅，其后增量从订阅 receiver 出。
pub type ReplayBuf = Vec<Vec<u8>>;

/// 输出回放环形缓冲：分块存（弹旧以**事件块**为单位，绝不切一半留半个
/// 转义序列），总字节 ≤ cap；超限从最旧整块弹出。单个超限巨块独占保留
/// （PTY 读块 16KB ≪ 512KB，实际不会触发）。
struct OutRing {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    cap: usize,
}

impl OutRing {
    fn new() -> Self {
        Self {
            chunks: VecDeque::new(),
            bytes: 0,
            cap: RING_CAP_BYTES,
        }
    }

    fn push(&mut self, chunk: Vec<u8>) {
        self.bytes += chunk.len();
        self.chunks.push_back(chunk);
        // 弹旧弹到不超限或只剩最新一块为止。
        while self.bytes > self.cap && self.chunks.len() > 1 {
            let Some(old) = self.chunks.pop_front() else {
                break;
            };
            self.bytes -= old.len();
        }
    }

    fn snapshot(&self) -> ReplayBuf {
        self.chunks.iter().cloned().collect()
    }
}

/// 一条镜像订阅（pump 广播的扇出点）。
struct Sub {
    id: u64,
    tx: Sender<TuiEvent>,
}

/// 会话共享核（`Arc`，Send+Sync）：pump 线程与各 web 订阅看同一份。
/// 锁序见文件头矩阵：subs 外、ring 内、input 叶。
struct SessionCore {
    sid: u64,
    /// 入向（按键字节 + resize）。互斥是叶锁，human 速度的按键不心疼。
    input: Mutex<TuiInput>,
    ring: Mutex<OutRing>,
    subs: Mutex<Vec<Sub>>,
    /// 活订阅数（= /tui/sessions 的 attached）。
    attached: AtomicUsize,
}

/// 会话册条目：册独占所有权 = 击杀与槽位的单一真相；共享核被 pump 与各
/// 订阅 `Arc` 引用——**订阅可以比会话活得短暂，槽位不会**（guard 随册走）。
struct SessionRec {
    core: Arc<SessionCore>,
    _guard: TuiGuard,
}

/// 会话册：键 = 任务 id，一会话/任务。**常驻**——ws 断开只是脱离；回收只有
/// 四条路径：子进程退出 / [`kill`] / [`prune_missing`] / 守护进程重启（内存态）。
///
/// # 锁序矩阵（外 → 内；反向嵌套或持锁跨广播 = 死锁）
/// 1. 本册锁——最外；guard drop 放册锁外；反向（持 subs/ring 拿册锁）绝无；
///    `reg`（任务注册表）→ 册锁只出现在 `Task::update` 的 prune；
/// 2. `SessionCore::subs` 锁；
/// 3. `SessionCore::ring` 锁——只嵌在 subs 之下（回放+订阅、pump 入块两临界区同形）；
/// 4. `SessionCore::input` 锁——叶锁。
///
/// 广播发送与击杀（killer.drop）必须在所有锁外：pump 的 Bytes/Exit 临界区只
/// 做「入块 + 快照订阅者」，出锁后再 send；Disconnected 摘除单持 subs 锁。
fn sessions() -> &'static Mutex<HashMap<String, SessionRec>> {
    static S: OnceLock<Mutex<HashMap<String, SessionRec>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// web 侧订阅句柄（每订阅一枚）：入向转发 + 退订。**没有击杀权**——会话存续
/// 只由 task 域四条回收路径决定；drop 即退订（detach 的 RAII 兜底）。
pub struct SessionRef {
    core: Arc<SessionCore>,
    sub_id: u64,
    detached: AtomicBool,
}

impl SessionRef {
    /// 写入 PTY（按键/粘贴/SGR 鼠标裸字节）。镜像共享：全体订阅同看结果。
    pub fn send(&self, bytes: Vec<u8>) {
        self.core
            .input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(bytes);
    }

    /// 改 PTY 尺寸（后到覆盖先到，单 PTY 单尺寸）。
    pub fn resize(&self, cols: u16, rows: u16) {
        self.core
            .input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resize(cols, rows);
    }

    /// 退订：从 subs 摘除 + attached--。幂等（detach 标志兜底），桥拆除路径
    /// 无论走 shutdown、服务 drop 还是本句柄 drop 都只减一次。
    pub fn detach(&self) {
        if self.detached.swap(true, Ordering::AcqRel) {
            return; // 已退过
        }
        let removed = {
            let mut subs = self.core.subs.lock().unwrap_or_else(|e| e.into_inner());
            let before = subs.len();
            subs.retain(|s| s.id != self.sub_id);
            before != subs.len()
        };
        if removed {
            self.core.attached.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for SessionRef {
    fn drop(&mut self) {
        self.detach();
    }
}

// ---------------- 会话册公开面（web 层只搬运） ----------------

/// 册快照（供 `GET /tui/sessions` 与「该任务是否已有会话」判定）：仅活会话；
/// attached = 活订阅数。子进程退出即被 pump 摘册，不驻留。
pub fn session_snapshot() -> Vec<(String, usize)> {
    sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(id, rec)| (id.clone(), rec.core.attached.load(Ordering::Relaxed)))
        .collect()
}

/// 按令牌意图接上会话（ws 升级的唯一入口）：
/// - [`TokenKind::Spawn`]：查册已有 → **降级 attach**（不占槽不重开）；查不到 →
///   开会话 + 入册 + 订阅（容量权威只在此路径）；
/// - [`TokenKind::Attach`]：查册，会话已消失 → [`TuiError::UnknownTask`]。
///
/// 回（订阅句柄, 回放块, 事件接收端）；回放快照与订阅注册同一临界区，
/// 其后事件从接收端出——不重帧不漏帧。
pub fn attach_or_spawn(
    task_id: &str,
    kind: TokenKind,
    cols: u16,
    rows: u16,
) -> Result<(SessionRef, ReplayBuf, Receiver<TuiEvent>), TuiError> {
    match kind {
        TokenKind::Attach => match session_core(task_id) {
            Some(core) => Ok(subscribe(&core)),
            None => {
                eprintln!("atlas: tui 任务 {task_id} 会话已消失，拒绝重挂");
                Err(TuiError::UnknownTask(task_id.to_owned()))
            }
        },
        TokenKind::Spawn => spawn_or_attach(task_id, cols, rows),
    }
}

/// 击杀（`POST /tui/kill` 侧）：摘册 = guard drop（子进程杀、槽位释放）；
/// waiter 随即上报 exit，pump 向全体订阅者广播 exit 帧后收束。幂等：
/// 无会话回 false（两条回收路径并发击杀时输家即为此状）。
pub fn kill(task_id: &str) -> bool {
    let victim = sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(task_id);
    let Some(victim) = victim else {
        return false;
    };
    drop(victim); // 击杀在册锁外完成（guard drop 会 eprintln，不持锁）
    eprintln!("atlas: tui 任务 {task_id} 会话被击杀");
    true
}

/// 注册表对账（`Task::update` 重扫处挂的钩子）：valid_ids 之外的任务的会话
/// 按 kill 同款回收（摘册 = guard drop）。只碰册锁；valid_ids 须是当前注册表
/// id 的快照（调用方持 reg 写锁，reg → 册锁顺序全局唯一）。
pub fn prune_missing(valid_ids: &[String]) {
    let victims: Vec<SessionRec> = {
        let mut led = sessions().lock().unwrap_or_else(|e| e.into_inner());
        let gone: Vec<String> = led
            .keys()
            .filter(|k| !valid_ids.iter().any(|v| v == *k))
            .cloned()
            .collect();
        gone.into_iter().filter_map(|id| led.remove(&id)).collect()
    };
    if !victims.is_empty() {
        eprintln!("atlas: tui prune 回收 {} 条任务已消失的会话", victims.len());
    }
    drop(victims); // 击杀在册锁外
}

// ---------------- 册内操作 ----------------

/// 查任务当前会话的共享核（在册 = 活）。
fn session_core(task_id: &str) -> Option<Arc<SessionCore>> {
    sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(task_id)
        .map(|rec| Arc::clone(&rec.core))
}

/// 订阅注册 + 回放快照（**subs → ring 同一临界区**）：每条输出块要么已在
/// 快照里、要么从其 receiver 出——attach 与新字节竞态时不重帧不漏帧。
fn subscribe(core: &Arc<SessionCore>) -> (SessionRef, ReplayBuf, Receiver<TuiEvent>) {
    let (tx, rx) = channel::<TuiEvent>();
    let sub_id = next_sub_id();
    let replay = {
        let mut subs = core.subs.lock().unwrap_or_else(|e| e.into_inner());
        let ring = core.ring.lock().unwrap_or_else(|e| e.into_inner());
        subs.push(Sub { id: sub_id, tx });
        core.attached.fetch_add(1, Ordering::Relaxed);
        ring.snapshot()
    };
    (
        SessionRef {
            core: Arc::clone(core),
            sub_id,
            detached: AtomicBool::new(false),
        },
        replay,
        rx,
    )
}

/// Spawn 意图主路径：已有会话 → 降级 attach（attach 不占槽）；没有 → 查描述符
/// （查不到 → UnknownTask，**不占槽**）→ 占槽开 PTY → 入册 → 订阅 → 起 pump
/// 线程（订阅先行，F6：pump 先起会让快速退出的 Exit 广播落空）。
/// 并发双 spawn（同任务两窗口同时点按钮）由入册时的二次查册裁决，
/// 输家 guard 就地 drop = 新子进程即杀、槽位即还。
fn spawn_or_attach(
    task_id: &str,
    cols: u16,
    rows: u16,
) -> Result<(SessionRef, ReplayBuf, Receiver<TuiEvent>), TuiError> {
    if let Some(core) = session_core(task_id) {
        return Ok(subscribe(&core)); // 降级：已有活会话，spawn 意图只是镜像进来
    }
    let Some(desc) = descriptor(task_id) else {
        eprintln!("atlas: tui 任务 {task_id} 不在注册表，拒绝开会话");
        return Err(TuiError::UnknownTask(task_id.to_owned()));
    };

    let (guard, input, outbox_rx) = open_desc(task_id, &desc, cols, rows)?;
    let core = Arc::new(SessionCore {
        sid: guard.sid,
        input: Mutex::new(input),
        ring: Mutex::new(OutRing::new()),
        subs: Mutex::new(Vec::new()),
        attached: AtomicUsize::new(0),
    });
    let rec = SessionRec {
        core: Arc::clone(&core),
        _guard: guard,
    };
    {
        let mut led = sessions().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = led.get(task_id) {
            // 双 spawn 竞态输家：让位给赢家会话，自开的子进程即杀、槽位即还。
            let winner = Arc::clone(&existing.core);
            drop(led);
            drop(rec);
            return Ok(subscribe(&winner));
        }
        led.insert(task_id.to_owned(), rec);
    }

    // F6：订阅先于 pump 拉起。open_desc 返回时子进程可能已退出；若 pump 先起、
    // 订阅后补，exit 广播打在空 subs 表上，Exit 就此蒸发（订阅者永挂）。
    // 泵未起时 outbox 已缓冲全部输出，起泵后经订阅端原样送达——不重不漏。
    let sub = subscribe(&core);

    let pump_core = Arc::clone(&core);
    let pump_task = task_id.to_owned();
    match thread::Builder::new()
        .name(format!("tui-pump-{}", pump_core.sid))
        .spawn(move || pump_loop(pump_core.sid, pump_task, pump_core, outbox_rx))
    {
        Ok(_) => Ok(sub),
        Err(e) => {
            let msg = format!("{e:?}");
            eprintln!("atlas: tui 任务 {task_id} pump 线程起步失败: {msg}");
            drop(sub); // 早订阅就地作废：detach 幂等，subs/attached 同步归还
            let victim = sessions()
                .lock()
                .unwrap_or_else(|er| er.into_inner())
                .remove(task_id);
            drop(victim); // guard drop：击杀子进程、槽位释放
            Err(TuiError::Pty(msg))
        }
    }
}

/// task 域 pump 转发线程（每会话一条 OS 线程）：消费 outbox → 写回放环 +
/// 广播各订阅；exit 事件 → 广播后**摘册**（guard drop = 击杀语义照旧、槽位
/// 释放）。本线程是 task 域唯一的会话收束点；web 侧只泵自家 receiver。
fn pump_loop(sid: u64, task_id: String, core: Arc<SessionCore>, mut outbox: OutboxRx) {
    while let Some(ev) = outbox.blocking_next() {
        match ev {
            TuiEvent::Bytes(chunk) => {
                // 入块 + 快照订阅者（subs → ring 同一临界区）：每个块要么进
                // 新订阅的回放快照、要么进其接收端，不重不漏。广播在锁外。
                let targets: Vec<(u64, Sender<TuiEvent>)> = {
                    let subs = core.subs.lock().unwrap_or_else(|e| e.into_inner());
                    let mut ring = core.ring.lock().unwrap_or_else(|e| e.into_inner());
                    ring.push(chunk.clone());
                    subs.iter().map(|s| (s.id, s.tx.clone())).collect()
                };
                let mut dead: Vec<u64> = Vec::new();
                for (id, tx) in targets {
                    if tx.send(TuiEvent::Bytes(chunk.clone())).is_err() {
                        dead.push(id); // 接收端已拆（桥拆除中，detach/册回收自会收尾）
                    }
                }
                if !dead.is_empty() {
                    let mut subs = core.subs.lock().unwrap_or_else(|e| e.into_inner());
                    let before = subs.len();
                    subs.retain(|s| !dead.contains(&s.id));
                    // F1#3：谁真正摘除 Sub 谁负责减 attached（与 detach() 的 removed 判定同构，
                    // 条目删除的原子性保证每个 Sub 至多被减一次，不会下溢）。
                    core.attached
                        .fetch_sub(before - subs.len(), Ordering::Relaxed);
                }
            }
            TuiEvent::Exit(code) => {
                let targets: Vec<Sender<TuiEvent>> = {
                    let subs = core.subs.lock().unwrap_or_else(|e| e.into_inner());
                    subs.iter().map(|s| s.tx.clone()).collect()
                };
                for tx in targets {
                    let _ = tx.send(TuiEvent::Exit(code));
                }
                let victim = sessions()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&task_id);
                drop(victim); // guard drop：子进程已死（击杀报错误差如实），槽位释放
                eprintln!("atlas: tui sid={sid} 任务 {task_id} 会话回收（子进程退出 code={code:?}）");
                return; // 尾随字节已无读者（exit 帧 + Close 已发），pump 收束
            }
        }
    }
    // outbox 关闭而未及 exit：waiter 上报 exit 只在册（经 guard 的 sender）或桥
    // （经 input 的 sender）至少一家存活时发生——走到这里说明册已摘、订阅已全退，
    // 会话已回收，这里仅收束线程。
    eprintln!("atlas: tui sid={sid} 任务 {task_id} pump 随 outbox 关闭收束");
}

// ---------------- 会话起步（承重流水线） ----------------

/// 起步失败统一包装（错误细节只进 stderr，调用方只做状态码映射）。
macro_rules! or_pty_err {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(err) => {
                let msg = format!("{err:?}");
                eprintln!("atlas: tui pty 起步失败: {msg}");
                // 守卫未建立前，槽位由本 return 的局部 drop 释放。
                return Err(TuiError::Pty(msg));
            }
        }
    };
}

/// 起一条任务绑定会话的流水线：占槽位 → 开 PTY → 在 PTY 里 spawn 该任务的
/// 命令（第二实例）→ writer/waiter/reader 三条线程。成功回（守卫, 入向,
/// outbox 消费端）；任何起步失败自动释放槽位/击杀子进程。调用方（
/// [`spawn_or_attach`]）负责查描述符、入册、起 pump。
fn open_desc(
    task_id: &str,
    desc: &TaskDescriptor,
    cols: u16,
    rows: u16,
) -> Result<(TuiGuard, TuiInput, OutboxRx), TuiError> {
    let Some(slot) = try_acquire() else {
        eprintln!("atlas: tui 会话数达上限，拒绝开会话");
        return Err(TuiError::Busy);
    };

    let sid = next_sid();

    // 开 PTY、按请求尺寸起任务命令。
    let pty = native_pty_system();
    let pair = or_pty_err!(pty.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }));

    let child = or_pty_err!(pair.slave.spawn_command(task_cmd(desc)));
    let killer = child.clone_killer();
    let (outbox, closer, outbox_rx) = Outbox::channel();
    let (in_tx, in_rx) = sync_channel::<Vec<u8>>(PTY_INPUT_QUEUE);
    // 自此任何错误路径都经此守卫击杀子进程。
    let guard = TuiGuard {
        sid,
        _in_tx: in_tx.clone(),
        killer,
        _slot: slot,
    };
    let reader = or_pty_err!(pair.master.try_clone_reader());
    let mut writer = or_pty_err!(pair.master.take_writer());
    // 丢弃父进程手里的 slave 副本，子进程死亡后 reader 才见 EOF。
    drop(pair.slave);

    or_pty_err!(
        thread::Builder::new()
            .name(format!("pty-writer-{sid}"))
            .spawn(move || {
                // 最后一个 Sender 随桥拆除而消失（或写失败）时收束。
                while let Ok(chunk) = in_rx.recv() {
                    if writer.write_all(&chunk).and_then(|_| writer.flush()).is_err() {
                        eprintln!("atlas: tui sid={sid} pty writer 停止（IO 错误）");
                        break;
                    }
                }
            })
    );

    // wait 线程持第二枚 reader 句柄，活跃的 reader 不挡会话拆除：守卫的
    // kill 解除 `wait()`，本句柄随守卫 drop 后最终也释放 reader 的 `read()`。
    let reader2 = or_pty_err!(pair.master.try_clone_reader());
    let in_tx2 = in_tx.clone();
    or_pty_err!(
        thread::Builder::new()
            .name(format!("pty-waiter-{sid}"))
            .spawn(move || waiter_loop(sid, child, closer, reader2, in_tx2))
    );

    or_pty_err!(
        thread::Builder::new()
            .name(format!("pty-reader-{sid}"))
            .spawn(move || reader_loop(sid, reader, outbox))
    );

    eprintln!("atlas: tui sid={sid} 任务 {task_id} 会话已起（{cols}x{rows}）");

    let input = TuiInput {
        in_tx,
        master: pair.master,
    };
    Ok((guard, input, outbox_rx))
}

/// 阻塞字节泵：把 PTY 输出灌进 outbox 直到 read 失败。Linux 上只有当子进程
/// *连同* 其他 slave-fd 持有者全部消失后 read 才以 EIO 返回，故本线程可能
/// 比 app 活得久；`outbox` 在此 drop = 关闭队列，exit 事件由 `waiter_loop`
/// 独立上报。
fn reader_loop(sid: u64, mut reader: Box<dyn Read + Send>, outbox: Outbox) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => outbox.push(TuiEvent::Bytes(buf[..n].to_vec())),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    eprintln!("atlas: tui sid={sid} pty reader 达 EOF");
    // `outbox` 在此 drop：关闭队列。
}

/// 阻塞在 `wait()` 直到子进程退出；若桥仍挂着（经 `in_tx` 判定：拆除期间
/// 其余 sender 均已 drop）则在 outbox 上上报 exit。消费端不会卡在本线程：
/// 本函数返回即 drop `closer`，队列无论如何都会关闭。
fn waiter_loop(
    sid: u64,
    mut child: Box<dyn Child + Send + Sync>,
    closer: OutboxCloser,
    _reader: Box<dyn Read + Send>,
    in_tx: SyncSender<Vec<u8>>,
) {
    let status = child.wait().ok();
    let code = status.map(|s| s.exit_code() as i32);
    eprintln!("atlas: tui sid={sid} pty 子进程退出 code={code:?}");
    // 哨兵探测桥存续：Full 亦证 writer 活着（有界队列, audit perf F11），
    // 唯 Disconnected = 会话已拆, 不再上报 Exit。
    if !matches!(in_tx.try_send(Vec::new()), Err(TrySendError::Disconnected(_))) {
        Outbox(Arc::clone(&closer.0)).push(TuiEvent::Exit(code));
    }
    // `closer` 在此 drop：关闭队列，消费端收束。
}

// ---------------- Outbox（手写单消费者、多唤醒队列） ----------------

/// 连接阻塞 reader 线程与 pump 消费线程。生产端（`Outbox`）是 `Send`；
/// 消费端（`OutboxRx`）活在 pump 线程。丢弃 `Outbox` 即关闭队列并唤醒挂起消费者。
struct OutboxInner {
    queue: Mutex<VecDeque<TuiEvent>>,
    waker: Mutex<Option<Waker>>,
    closed: AtomicBool,
}

struct Outbox(Arc<OutboxInner>);
struct OutboxRx(Arc<OutboxInner>);

impl Outbox {
    /// 生产句柄、Outbox 关闭信号、消费端。
    ///
    /// 中间端（`OutboxCloser`）的存在是为了让 PTY wait 线程能在 reader
    /// 仍阻塞于 `read()` 时也上报子进程退出：关闭它（或任其 drop）即关闭
    /// 队列，不与 reader 抢跑。
    fn channel() -> (Outbox, OutboxCloser, OutboxRx) {
        let inner = Arc::new(OutboxInner {
            queue: Mutex::new(VecDeque::new()),
            waker: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        (
            Outbox(Arc::clone(&inner)),
            OutboxCloser(Arc::clone(&inner)),
            OutboxRx(inner),
        )
    }

    /// 入队并唤醒可能在等的消费端。
    fn push(&self, ev: TuiEvent) {
        self.0
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(ev);
        self.wake();
    }

    fn wake(&self) {
        if let Some(w) = self
            .0
            .waker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            w.wake();
        }
    }
}

/// 第二个句柄，drop 即关闭 outbox（由 PTY wait 线程持有）。与 `Outbox`
/// 自身的 drop-close 一起，保证两条 PTY 线程都消失后消费端必然终止，
/// 且任一线程都不等待另一条。
struct OutboxCloser(Arc<OutboxInner>);

impl Drop for OutboxCloser {
    fn drop(&mut self) {
        self.0.closed.store(true, Ordering::Release);
        if let Some(w) = self
            .0
            .waker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            w.wake();
        }
    }
}

/// 「wake = unpark 本线程」的 waker——让没有异步运行时的 OS 线程（pump）
/// 驱动 [`OutboxRx`]。
struct ParkWaker(thread::Thread);

impl Wake for ParkWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

impl OutboxRx {
    /// 出队核心：有事件回 Ready；队列排空且已关闭回 Ready(None)；否则注册
    /// waker、复查竞态后 Pending。
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        {
            let mut q = self.0.queue.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ev) = q.pop_front() {
                return Poll::Ready(Some(ev));
            }
            if self.0.closed.load(Ordering::Acquire) {
                return Poll::Ready(None);
            }
        }
        *self.0.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
        // 注册 waker 后复查一次：出队与注册之间挤进来的 push / close
        // 不会丢唤醒。
        let racy = !self
            .0
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
            || self.0.closed.load(Ordering::Acquire);
        if racy {
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }

    /// 阻塞出队（task 域 pump 线程专用）：park 等生产者 wake（= unpark）。
    /// 队列排空且已关闭后回 `None`；虚假唤醒无害——循环重查。
    fn blocking_next(&mut self) -> Option<TuiEvent> {
        let waker = Waker::from(Arc::new(ParkWaker(thread::current())));
        let mut cx = Context::from_waker(&waker);
        loop {
            match self.poll_next(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => thread::park(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 触碰 LIVE / 会话册的用例串行化（共享静态，慢 ARM 上时序敏感）。锁本体在
    /// task 域（`super::ledger_test_lock`）：task 侧驱动 `Task::update` 的规格对账
    /// 用例同样持它——`update()` 末段的 `tui::prune_missing` 会把兄弟用例在册的
    /// 活 PTY 会话 guard-drop 击杀（信号死亡经 xpty 记为 exit_code=1，负载下
    /// 表现为本用例偶发「exit 1」而非 7）。
    fn ledger_lock() -> std::sync::MutexGuard<'static, ()> {
        super::super::ledger_test_lock()
    }

    // ---- 无 PTY 的册条目夹具（真 guard 需真子进程，纯结构测试用假句柄） ----

    #[derive(Debug)]
    struct FakeKiller;

    impl ChildKiller for FakeKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(Self)
        }
    }

    #[derive(Debug)]
    struct FakeMaster;

    impl MasterPty for FakeMaster {
        fn resize(&self, _size: PtySize) -> xpty::Result<()> {
            Ok(())
        }

        fn get_size(&self) -> xpty::Result<PtySize> {
            Ok(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
        }

        fn try_clone_reader(&self) -> xpty::Result<Box<dyn Read + Send>> {
            Err(xpty::Error::other("test fake"))
        }

        fn take_writer(&self) -> xpty::Result<Box<dyn Write + Send>> {
            Err(xpty::Error::other("test fake"))
        }
    }

    /// 向册注入一条假会话（占槽，无 pump/无 PTY）：kill / prune 回收路径的夹具。
    fn inject_fake_session(task_id: &str) {
        let slot = try_acquire().expect("槽位预订");
        let (tx, _rx) = sync_channel::<Vec<u8>>(PTY_INPUT_QUEUE);
        let sid = next_sid();
        let guard = TuiGuard {
            sid,
            _in_tx: tx.clone(),
            killer: Box::new(FakeKiller),
            _slot: slot,
        };
        let input = TuiInput {
            in_tx: tx,
            master: Box::new(FakeMaster),
        };
        let core = Arc::new(SessionCore {
            sid,
            input: Mutex::new(input),
            ring: Mutex::new(OutRing::new()),
            subs: Mutex::new(Vec::new()),
            attached: AtomicUsize::new(0),
        });
        sessions()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(task_id.to_owned(), SessionRec { core, _guard: guard });
    }

    // ---- 令牌门 ----

    #[test]
    fn token_binds_task_id_kind_and_is_single_use() {
        // Given: 一枚绑定任务 "x"、Spawn 意图的新令牌
        let token = issue_token("x", TokenKind::Spawn);
        // Then: 首次消费回绑定 (id, kind)；重放（已消费）回 None
        assert_eq!(take_token(&token), Some(("x".to_owned(), TokenKind::Spawn)));
        assert_eq!(take_token(&token), None, "重放必须失败");
    }

    #[test]
    fn expired_token_evicted_on_issue() {
        // Given: 一枚已过期的令牌躺在 map 里
        let stale = random_hex();
        let stale_at = Instant::now() - TOKEN_TTL - Duration::from_secs(1);
        tokens()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(stale.clone(), ("t".to_owned(), TokenKind::Attach, stale_at));
        // When: 签发新令牌（顺带清过期）
        let fresh = issue_token("x", TokenKind::Spawn);
        // Then: 过期条目被剔除，新令牌可用且 kind 随令牌回传
        let still_there = tokens()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&stale);
        assert!(!still_there, "签发顺带清过期必须剔除陈旧条目");
        assert_eq!(take_token(&fresh), Some(("x".to_owned(), TokenKind::Spawn)));
    }

    #[test]
    fn task_exists_and_descriptor_false_for_unknown_id() {
        // "ghost" 不是任何测试注册的任务；纯读，不碰真注册表写入。
        assert!(!task_exists("ghost"));
        assert!(descriptor("ghost").is_none());
    }

    #[test]
    fn attach_token_never_occupies_slot() {
        let _g = ledger_lock();
        let live0 = LIVE.load(Ordering::Relaxed);
        // Given/When: 对不存在会话的任务走全 attach 流程（签发→消费→attach 失败）
        let token = issue_token("ghost-attach", TokenKind::Attach);
        assert_eq!(
            take_token(&token),
            Some(("ghost-attach".to_owned(), TokenKind::Attach))
        );
        assert!(matches!(
            attach_or_spawn("ghost-attach", TokenKind::Attach, 80, 24),
            Err(TuiError::UnknownTask(_))
        ));
        // Then: 全程 LIVE 不动——attach 令牌与 attach 路径永不占槽
        assert_eq!(LIVE.load(Ordering::Relaxed), live0);
    }

    // ---- 回收路径 ----

    #[test]
    fn kill_is_idempotent() {
        let _g = ledger_lock();
        assert!(!kill("kill_ghost"), "无会话必须 false");
        inject_fake_session("kill1");
        assert!(session_snapshot().iter().any(|(id, _)| id == "kill1"));
        assert!(kill("kill1"), "活会话必须 true");
        assert!(!kill("kill1"), "二次击杀（会话已摘）必须仍 false——幂等");
        assert!(!session_snapshot().iter().any(|(id, _)| id == "kill1"));
    }

    #[test]
    fn prune_missing_reclaims_gone_tasks() {
        let _g = ledger_lock();
        inject_fake_session("prune_a");
        inject_fake_session("prune_b");
        // When: 有效集只含 prune_a（prune_b 的任务已从注册表消失）
        prune_missing(&["prune_a".to_owned(), "other".to_owned()]);
        // Then: prune_b 会话被回收（guard drop），prune_a 幸存
        let snap = session_snapshot();
        assert!(snap.iter().any(|(id, _)| id == "prune_a"));
        assert!(!snap.iter().any(|(id, _)| id == "prune_b"));
        kill("prune_a"); // 清场
    }

    // ---- 回放环 ----

    #[test]
    fn ring_evicts_whole_oldest_chunks_over_cap() {
        let mut ring = OutRing {
            chunks: VecDeque::new(),
            bytes: 0,
            cap: 4096,
        };
        for i in 0..8u8 {
            ring.push(vec![b'a' + i; 1024]);
        }
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 4, "超限必须按整事件块弹出，只留最新 4 块");
        assert_eq!(ring.bytes, 4096);
        assert_eq!(snap[0][0], b'e', "最旧块应是第 5 块（a..d 已弹）");
        assert_eq!(snap.last().unwrap()[0], b'h');
        // 单块超限：独占保留（弹出单位是"别的块"，不切碎本块）
        ring.push(vec![b'x'; 9000]);
        assert_eq!(ring.snapshot().len(), 1);
        assert_eq!(ring.bytes, 9000);
    }

    // ---- 真 PTY 端到端（全套件仅此一条；回环命令，慢 ARM 友好） ----

    #[cfg(unix)]
    #[test]
    fn live_session_spawn_replay_detach_kill() {
        let _g = ledger_lock();
        let d = TaskDescriptor {
            id: "tui_live1".into(),
            name: "live".into(),
            command: "sh".into(),
            args: vec!["-c".into(), "printf READY; exec cat".into()],
            cwd: None,
            envs: None,
            autostart: false,
            group: "test".into(),
            log_limit: Some(64),
            restart_policy: Some(super::super::RestartPolicy::Never),
            stop_grace_ms: Some(500),
        };
        super::super::reg()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert("tui_live1".into(), super::super::Slot::new(d));

        let live0 = LIVE.load(Ordering::Relaxed);
        let (sref, replay, rx) =
            attach_or_spawn("tui_live1", TokenKind::Spawn, 80, 24).expect("spawn 必须成功");
        assert!(replay.is_empty(), "新会话回放为空");

        // 等 PTY 输出 READY 进环（慢 ARM 上限 10s）
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(&seen).contains("READY") && Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(TuiEvent::Bytes(b)) => seen.extend_from_slice(&b),
                Ok(TuiEvent::Exit(_)) => break,
                Err(_) => {} // 超时/断链交给循环条件裁决
            }
        }
        assert!(
            String::from_utf8_lossy(&seen).contains("READY"),
            "PTY 输出未见 READY: {seen:?}"
        );

        // Spawn 意图遇已有会话 → 降级 attach：回放必须含前序输出
        let (sref2, replay2, rx2) =
            attach_or_spawn("tui_live1", TokenKind::Spawn, 80, 24).expect("降级 attach 必须成功");
        assert!(
            String::from_utf8_lossy(&replay2.concat()).contains("READY"),
            "回放必须含前序输出块"
        );
        assert!(
            session_snapshot()
                .iter()
                .any(|(id, n)| id == "tui_live1" && *n == 2),
            "镜像双订阅 attached=2"
        );

        // 脱离 = 摘订阅，会话存续
        sref.detach();
        assert!(
            session_snapshot()
                .iter()
                .any(|(id, n)| id == "tui_live1" && *n == 1)
        );

        // 击杀：exit 帧广播给幸存订阅、摘册放槽；幂等
        assert!(kill("tui_live1"));
        assert!(!kill("tui_live1"), "kill 幂等");
        assert!(!session_snapshot().iter().any(|(id, _)| id == "tui_live1"));
        let mut got_exit = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match rx2.recv_timeout(Duration::from_millis(500)) {
                Ok(TuiEvent::Exit(_)) => {
                    got_exit = true;
                    break;
                }
                Ok(TuiEvent::Bytes(_)) => {}
                Err(_) => {
                    got_exit = true; // Disconnected = 会话已回收，同样收束
                    break;
                }
            }
        }
        assert!(got_exit, "幸存订阅必须收到 exit 帧（或通道拆净）");
        assert_eq!(LIVE.load(Ordering::Relaxed), live0, "击杀后槽位必须释放");
        sref2.detach();
        super::super::reg()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove("tui_live1");
    }

    // ---- F6：快速退出竞态（订阅必须先于 pump 拉起） ----

    #[cfg(unix)]
    #[test]
    fn exit_never_evaporates() {
        let _g = ledger_lock();
        let d = TaskDescriptor {
            id: "tui_race1".into(),
            name: "race".into(),
            command: "sh".into(),
            args: vec!["-c".into(), "exit 7".into()],
            cwd: None,
            envs: None,
            autostart: false,
            group: "test".into(),
            log_limit: Some(64),
            restart_policy: Some(super::super::RestartPolicy::Never),
            stop_grace_ms: Some(500),
        };
        super::super::reg()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert("tui_race1".into(), super::super::Slot::new(d));

        let live0 = LIVE.load(Ordering::Relaxed);
        for i in 0..20 {
            let (sref, _replay, rx) =
                attach_or_spawn("tui_race1", TokenKind::Spawn, 80, 24).expect("spawn 必须成功");
            // 订阅若先于 pump 建立，exit 广播必有落点：Exit 必达（Bytes 忽略）。
            let mut got: Option<Option<i32>> = None;
            let deadline = Instant::now() + Duration::from_secs(10);
            while got.is_none() && Instant::now() < deadline {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(TuiEvent::Exit(code)) => got = Some(code),
                    Ok(TuiEvent::Bytes(_)) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(_) => {} // 超时：交给循环条件裁决
                }
            }
            assert_eq!(got, Some(Some(7)), "第 {i} 轮：快速退出的 Exit 蒸发（竞态）");
            // 收尾等摘册：pump 广播 exit 后才摘册；在册时下一轮会降级 attach
            // 到垂死会话，必须等它真正回收再开下一轮。
            let deadline = Instant::now() + Duration::from_secs(10);
            while session_snapshot().iter().any(|(id, _)| id == "tui_race1")
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(10));
            }
            drop(sref);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while LIVE.load(Ordering::Relaxed) != live0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(LIVE.load(Ordering::Relaxed), live0, "20 轮回收后槽位必须归零");
        super::super::reg()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove("tui_race1");
    }

    #[test]
    fn early_subscribe_drop_is_clean() {
        let _g = ledger_lock();
        let live0 = LIVE.load(Ordering::Relaxed);
        inject_fake_session("race_early");
        let core = session_core("race_early").expect("夹具会话在册");
        let att0 = core.attached.load(Ordering::Relaxed);
        // 订阅后立即退订（对应 pump 起步失败路径：早订阅的句柄被就地 drop）：
        // 不 panic、不漏 attached 计数、双退订幂等。
        let (sref, replay, _rx) = subscribe(&core);
        assert!(replay.is_empty(), "空环回放为空");
        assert_eq!(core.attached.load(Ordering::Relaxed), att0 + 1);
        sref.detach();
        sref.detach(); // 幂等：重复退订只减一次
        drop(sref); // Drop 再触发 detach，仍幂等
        assert_eq!(core.attached.load(Ordering::Relaxed), att0);
        assert!(kill("race_early"));
        assert_eq!(LIVE.load(Ordering::Relaxed), live0, "假会话回收后槽位归还");
    }
}
