//! v1/task: 声明式任务监督器（黑箱；无 tokio、无 GlobIO、无 UI 依赖）。
//!
//! # 公开接口用法
//! ```ignore
//! use crate::v1::task::{Task, Phase, Stream};
//!
//! let all = Task::update();                    // 重扫配置 + 增量应用 + 返回全量状态
//! for s in Task::status() {                    // 全量快照（纯读）
//!     if s.phase == Phase::Running { println!("{:05} {}", s.pid.unwrap_or(0), s.name); }
//! }
//! Task::run("miniserve").ok();                 // 幂等错误见 TaskError
//! Task::input("miniserve", "reload").ok();     // 写 stdin（环内记 Stdin 回声）
//! let lines = Task::output("miniserve");       // Some(Vec<LogLine>)：流别 + 时间戳
//! Task::stop("miniserve").ok();                // SIGTERM 整组 → 宽限 → SIGKILL
//! ```
//!
//! # 内部架构
//! - 每任务一条 std::thread 监督线程 = 单一状态写者；控制走 std mpsc（无界，Stop 必达）；
//! - 状态机 `Idle→Starting→Running→Stopping→Exited|Failed`；`Starting` 由 run() 在
//!   线程启动前同步置位，封死双重 spawn 窗口；
//! - 子进程独立进程组（process_group(0)），停止走 TERM→宽限→KILL 整组，不产孤儿；
//! - 日志：stdout/stderr 各一条泵线程读至 EOF，入有界环（cap = desc.log_limit），
//!   溢出计入 dropped；input() 同步写一条 Stdin 回声；
//! - RestartPolicy::Always = 监督循环内重 spawn，指数退避 2s..60s，10s 内连崩 3 次
//!   降级 Failed；Warn/Never 不重拉，区别仅在语义标注；
//! - 配置源 = `<project_dir>/atlas_task.json` + `<project_dir>/script/*.ts`（Bun 生成项，
//!   `bun run <file>`；Bun 无权限闸，等价 deno -A，无需 flag）。
//!
//! # 子模块
//! - [`tui`]：浏览器 TUI 会话核心——PTY 下的交互式 shell 也是一种子进程
//!   （令牌门 + 槽位 + 三线程流水线，零 ntex）；WS 传输在 v1/api/tui。

pub mod tui;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::v1::prelude;

// ---------------- 公开数据体 ----------------

/// 崩溃重启策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestartPolicy {
    /// 非手动退出即重拉（指数退避，连崩降级 Failed）
    Always,
    /// 不重拉；异常退出标注为 Failed 供调用方处置
    Warn,
    /// 仅停止，不处理
    Never,
}

/// JSON 任务描述符（兼容旧 atlas_task.json 模式，新增 stop_grace_ms）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDescriptor {
    pub id: String,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub envs: Option<HashMap<String, String>>,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub group: String,
    pub log_limit: Option<usize>,
    pub restart_policy: Option<RestartPolicy>,
    /// SIGTERM 到 SIGKILL 的宽限毫秒，默认 3000
    pub stop_grace_ms: Option<u64>,
}

/// 运行阶段（单一写者 = 该任务的监督线程；Starting 例外，由 run() 置位）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    Starting,
    Running,
    Stopping,
    /// 正常退出或手动停止
    Exited,
    /// 异常退出 / spawn 失败 / 崩溃循环
    Failed,
}

/// 日志行归属的流
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    Stdout,
    Stderr,
    /// input() 写入回显
    Stdin,
}

/// 一行捕获输出
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub at: SystemTime,
    pub stream: Stream,
    pub text: String,
}

/// 终态元数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitMeta {
    pub code: Option<i32>,
    /// 被信号终止时的信号号
    pub signal: Option<i32>,
    pub duration_ms: u64,
    /// true = 经 stop() 发起
    pub manual: bool,
    pub at: SystemTime,
}

/// 一个任务的当前状态快照
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub id: String,
    pub name: String,
    pub group: String,
    pub command: String,
    pub phase: Phase,
    pub pid: Option<u32>,
    pub started_at: Option<SystemTime>,
    pub uptime_ms: u64,
    pub restarts: u32,
    pub last_exit: Option<ExitMeta>,
    pub log_lines: usize,
    pub log_dropped: usize,
}

/// 操作失败原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskError {
    UnknownId(String),
    NotRunning(String),
    AlreadyRunning(String),
    /// 任务已登记但描述符命令无法启动（详情见 output/last_exit）
    Spawn(String),
}

impl core::fmt::Display for TaskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TaskError::UnknownId(id) => write!(f, "unknown task id: {id}"),
            TaskError::NotRunning(id) => write!(f, "task not running: {id}"),
            TaskError::AlreadyRunning(id) => write!(f, "task already running: {id}"),
            TaskError::Spawn(m) => write!(f, "spawn failed: {m}"),
        }
    }
}
impl std::error::Error for TaskError {}

// ---------------- 公开入口 ----------------

/// 唯一公开入口：任务监督器
pub struct Task;

impl Task {
    /// 重扫配置（atlas_task.json + script/*.ts）并增量应用，返回最新 [`Task::status`]。
    ///
    /// diff 策略：新增项即登记（autostart 即拉起）；删除项若在跑则停止并摘除；
    /// 签名（command/args/cwd/envs）变更的在跑任务先停止、按新 spec 重新拉起。
    /// 三阶段执行：写锁内只发 Stop 意图并登记待办 → **放锁**有界等待终态
    /// （等待期间不阻塞注册表读写）→ 回锁重拉/摘除。等不到终态也照常推进，
    /// SIGKILL 由监督线程在宽限期后自行兜底。
    ///
    /// 坏损保护：`atlas_task.json` 存在但解析失败/不可读时计 WARN 并**跳过**
    /// gone 清扫与会话对账（空集不可信，运行中任务不误杀）；文件缺失仍按
    /// "空集 = 全部消失"处理。
    pub fn update() -> Vec<Status> {
        let (descs, json_ok) = load_descs();
        let live: Vec<String> = descs.iter().map(|d| d.id.clone()).collect();

        // 阶段 1（持写锁）：diff 应用；在跑项只发 Stop 意图，等待放锁外做。
        // pending_stop = (id, shared, 等待截止)；restart = (id, 新描述符)；stale = 待摘除 id。
        let mut pending_stop: Vec<(String, Arc<Mutex<Shared>>, Instant)> = Vec::new();
        let mut restart: Vec<(String, TaskDescriptor)> = Vec::new();
        let mut stale: Vec<String> = Vec::new();
        {
            let mut reg = reg().write().unwrap_or_else(|e| e.into_inner());
            for d in descs {
                match reg.get_mut(&d.id) {
                    Some(slot) => {
                        if slot.desc.spec_signature() != d.spec_signature() {
                            let phase = slot.phase();
                            if matches!(phase, Phase::Starting | Phase::Running | Phase::Stopping) {
                                // 在跑且签名变更：发 Stop，出锁等终态后再重拉
                                if let Some(tx) = &slot.ctl {
                                    let _ = tx.send(Control::Stop);
                                }
                                let deadline = Instant::now()
                                    + Duration::from_millis(
                                        slot.desc.stop_grace_ms.unwrap_or(DEFAULT_GRACE_MS) + 2000,
                                    );
                                pending_stop.push((
                                    d.id.clone(),
                                    Arc::clone(&slot.shared),
                                    deadline,
                                ));
                                restart.push((d.id.clone(), d.clone()));
                            } else {
                                slot.desc = d.clone(); // 终态：按新 spec 直接重拉
                                let _ = Self::start_slot(slot);
                            }
                        } else {
                            slot.desc = d.clone(); // 仅改名/组/策略等展示字段
                        }
                    }
                    None => {
                        let mut slot = Slot::new(d.clone());
                        if d.autostart {
                            let _ = Self::start_slot(&mut slot);
                        }
                        reg.insert(d.id.clone(), slot);
                    }
                }
            }

            // 配置里没了的任务：在跑的发 Stop 意图（锁外等待），终态的直接摘除。
            // 规格存在但坏损（json_ok=false）时跳过——空集不可信，不得误杀运行中任务。
            if json_ok {
                let gone: Vec<String> =
                    reg.keys().filter(|k| !live.contains(k)).cloned().collect();
                for id in gone {
                    let mut stopping: Option<(Arc<Mutex<Shared>>, Instant)> = None;
                    if let Some(slot) = reg.get_mut(&id) {
                        let phase = slot.phase();
                        if matches!(phase, Phase::Starting | Phase::Running | Phase::Stopping) {
                            if let Some(tx) = &slot.ctl {
                                let _ = tx.send(Control::Stop);
                            }
                            let deadline = Instant::now()
                                + Duration::from_millis(
                                    slot.desc.stop_grace_ms.unwrap_or(DEFAULT_GRACE_MS) + 2000,
                                );
                            stopping = Some((Arc::clone(&slot.shared), deadline));
                        }
                    }
                    if let Some((shared, deadline)) = stopping {
                        pending_stop.push((id.clone(), shared, deadline));
                    }
                    stale.push(id);
                }
            }
        }

        // 阶段 2（写锁已释放）：有界等待各 Stop 意图落地为终态
        for (_id, shared, deadline) in &pending_stop {
            wait_terminal(shared, *deadline);
        }

        // 阶段 3（重持写锁）：按新 spec 重拉、摘除删除项，然后快照返回
        let mut reg = reg().write().unwrap_or_else(|e| e.into_inner());
        for (id, d) in restart {
            if let Some(slot) = reg.get_mut(&id) {
                slot.desc = d;
                let _ = Self::start_slot(slot);
            }
        }
        for id in stale {
            reg.remove(&id);
        }

        // 会话册对账：注册表已消失的任务，其常驻 PTY 会话即刻回收（guard drop 击杀）。
        // 坏损规格下 live 不可信，跳过对账（与 gone 清扫同规则）。
        if json_ok {
            tui::prune_missing(&live);
        }

        Self::snapshot_all(&reg)
    }

    /// 全部任务的运行状态快照（按 id 排序，纯读）
    pub fn status() -> Vec<Status> {
        let reg = reg().read().unwrap_or_else(|e| e.into_inner());
        Self::snapshot_all(&reg)
    }

    /// 启动任务。已在 Starting/Running/Stopping 中则报 [`TaskError::AlreadyRunning`]。
    /// Starting 位在 spawn 线程前同步置位，杜绝并发双拉。
    pub fn run(id: &str) -> Result<(), TaskError> {
        let mut reg = reg().write().unwrap_or_else(|e| e.into_inner());
        let slot = reg
            .get_mut(id)
            .ok_or_else(|| TaskError::UnknownId(id.to_string()))?;
        Self::start_slot(slot)
    }

    /// 请求停止（异步语义：发 Stop 意图即返回，终态由监督线程写入，用 status() 轮询）。
    /// 未到终态时报 [`TaskError::NotRunning`]（幂等：可先 status() 判读）。
    pub fn stop(id: &str) -> Result<(), TaskError> {
        let reg = reg().read().unwrap_or_else(|e| e.into_inner());
        let slot = reg
            .get(id)
            .ok_or_else(|| TaskError::UnknownId(id.to_string()))?;
        let phase = slot.phase();
        if !matches!(phase, Phase::Starting | Phase::Running | Phase::Stopping) {
            return Err(TaskError::NotRunning(id.to_string()));
        }
        if let Some(tx) = &slot.ctl {
            let _ = tx.send(Control::Stop);
        }
        Ok(())
    }

    /// 向任务 stdin 写一行（自动补 \n）；环形缓冲同步记录 Stdin 回声。
    pub fn input(id: &str, line: &str) -> Result<(), TaskError> {
        let reg = reg().read().unwrap_or_else(|e| e.into_inner());
        let slot = reg
            .get(id)
            .ok_or_else(|| TaskError::UnknownId(id.to_string()))?;
        if slot.phase() != Phase::Running {
            return Err(TaskError::NotRunning(id.to_string()));
        }
        let tx = slot
            .ctl
            .as_ref()
            .ok_or_else(|| TaskError::NotRunning(id.to_string()))?;
        tx.send(Control::Line(line.to_string()))
            .map_err(|_| TaskError::NotRunning(id.to_string()))?;
        slot.push_log(Stream::Stdin, line.to_string());
        Ok(())
    }

    /// 任务的捕获输出（stdout/stderr/stdin 回声，环形缓冲现存内容）。未登记返回 None。
    pub fn output(id: &str) -> Option<Vec<LogLine>> {
        let reg = reg().read().unwrap_or_else(|e| e.into_inner());
        let slot = reg.get(id)?;
        let ring = slot.logs.lock().unwrap_or_else(|e| e.into_inner());
        Some(ring.lines.iter().cloned().collect())
    }

    // ---- 内部 ----

    fn snapshot_all(reg: &BTreeMap<String, Slot>) -> Vec<Status> {
        reg.values().map(Slot::snapshot).collect()
    }

    fn start_slot(slot: &mut Slot) -> Result<(), TaskError> {
        let phase = slot.phase();
        if matches!(phase, Phase::Starting | Phase::Running | Phase::Stopping) {
            return Err(TaskError::AlreadyRunning(slot.desc.id.clone()));
        }
        let (tx, rx) = channel::<Control>();
        slot.ctl = Some(tx);
        {
            let mut s = slot.shared.lock().unwrap_or_else(|e| e.into_inner());
            s.phase = Phase::Starting;
            s.pid = None;
            s.started_at = None;
        }
        let h = std::thread::spawn({
            let desc = slot.desc.clone();
            let shared = Arc::clone(&slot.shared);
            let logs = Arc::clone(&slot.logs);
            move || supervise(desc, shared, logs, rx)
        });
        slot._join = Some(h);
        Ok(())
    }
}

// ---------------- 黑箱内部 ----------------

const DEFAULT_LOG_LINES: usize = 1000;
const DEFAULT_GRACE_MS: u64 = 3000;
const POLL: Duration = Duration::from_millis(20);
const CRASH_WINDOW: Duration = Duration::from_secs(10);
const CRASH_LIMIT: u32 = 3;

enum Control {
    Stop,
    Line(String),
}

#[derive(Debug)]
struct Shared {
    phase: Phase,
    pid: Option<u32>,
    started_at: Option<SystemTime>,
    restarts: u32,
    last_exit: Option<ExitMeta>,
}

struct Ring {
    lines: VecDeque<LogLine>,
    cap: usize,
    dropped: usize,
}

impl Ring {
    fn push(&mut self, line: LogLine) {
        if self.lines.len() >= self.cap {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(line);
    }
}

struct Slot {
    desc: TaskDescriptor,
    shared: Arc<Mutex<Shared>>,
    logs: Arc<Mutex<Ring>>,
    ctl: Option<Sender<Control>>,
    _join: Option<JoinHandle<()>>,
}

impl Slot {
    fn new(desc: TaskDescriptor) -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared {
                phase: Phase::Idle,
                pid: None,
                started_at: None,
                restarts: 0,
                last_exit: None,
            })),
            logs: Arc::new(Mutex::new(Ring {
                lines: VecDeque::new(),
                cap: desc.log_limit.unwrap_or(DEFAULT_LOG_LINES).max(16),
                dropped: 0,
            })),
            desc,
            ctl: None,
            _join: None,
        }
    }

    fn phase(&self) -> Phase {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).phase
    }

    fn push_log(&self, stream: Stream, text: String) {
        let mut ring = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        ring.push(LogLine {
            at: SystemTime::now(),
            stream,
            text,
        });
    }

    fn snapshot(&self) -> Status {
        let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let ring = self.logs.lock().unwrap_or_else(|e| e.into_inner());
        Status {
            id: self.desc.id.clone(),
            name: self.desc.name.clone(),
            group: self.desc.group.clone(),
            command: self.desc.command.clone(),
            phase: s.phase,
            pid: s.pid,
            started_at: s.started_at,
            uptime_ms: s
                .started_at
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            restarts: s.restarts,
            last_exit: s.last_exit.clone(),
            log_lines: ring.lines.len(),
            log_dropped: ring.dropped,
        }
    }
}

// status_locked 的真实实现（占位函数替换，BTreeMap 借用友好）

fn reg() -> &'static RwLock<BTreeMap<String, Slot>> {
    static REG: OnceLock<RwLock<BTreeMap<String, Slot>>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(BTreeMap::new()))
}

impl TaskDescriptor {
    /// 影响运行行为的最小签名：命令/参数/工作目录/环境。展示字段变化不触发重启。
    fn spec_signature(&self) -> String {
        format!(
            "{}\u{1}{}\u{1}{}\u{1}#{:?}",
            self.command,
            self.args.join("\u{2}"),
            self.cwd.as_deref().unwrap_or(""),
            self.envs.as_ref().map(|h| {
                let mut kv: Vec<_> = h.iter().collect();
                kv.sort();
                kv
            })
        )
    }
}

/// 监督线程主体：spawn → 双泵 → 控制循环 → 退出分类 →（Always 策略）退避重拉。
/// 该线程是 Shared 状态的唯一写者（Starting 由 run() 预置除外）。
fn supervise(
    desc: TaskDescriptor,
    shared: Arc<Mutex<Shared>>,
    logs: Arc<Mutex<Ring>>,
    rx: Receiver<Control>,
) {
    let grace_ms = desc.stop_grace_ms.unwrap_or(DEFAULT_GRACE_MS);
    let mut fast_crashes: u32 = 0;
    let mut attempt: u32 = 0;

    loop {
        set_phase(&shared, |s| {
            s.phase = Phase::Starting;
            s.pid = None;
            s.started_at = None;
        });

        // 交叉引用：task/tui.rs 的 task_cmd()（PTY 第二实例）镜像本构造——两处命令构造须同步。
        let mut cmd = Command::new(&desc.command);
        cmd.args(&desc.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 独立进程组（pgid = 子 pid）：停止时整组信号，不留孤儿
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        if let Some(cwd) = &desc.cwd {
            cmd.current_dir(cwd);
        }
        if let Some(envs) = &desc.envs {
            cmd.envs(envs); // 继承当前环境后覆盖（旧版死字段修复）
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("{}: {e}", desc.command);
                set_phase(&shared, |s| {
                    s.phase = Phase::Failed;
                    s.last_exit = Some(ExitMeta {
                        code: None,
                        signal: None,
                        duration_ms: 0,
                        manual: false,
                        at: SystemTime::now(),
                    });
                });
                push(&logs, Stream::Stderr, format!("[spawn] {msg}"));
                return;
            }
        };

        let pid = child.id();
        let started = Instant::now();
        set_phase(&shared, |s| {
            s.phase = Phase::Running;
            s.pid = Some(pid);
            s.started_at = Some(SystemTime::now());
        });

        // 双泵各自读到 EOF 才退出（旧版单侧 EOF 截尾修复）
        let out_h = pump(child.stdout.take(), Arc::clone(&logs), Stream::Stdout);
        let err_h = pump(child.stderr.take(), Arc::clone(&logs), Stream::Stderr);
        let mut stdin: Option<ChildStdin> = child.stdin.take();

        let mut manual = false;
        let status: Option<ExitStatus> = loop {
            // 带超时阻塞接收：无消息时最多睡 POLL 即转去查子进程，无忙轮询
            match rx.recv_timeout(POLL) {
                Ok(Control::Line(text)) => {
                    if let Some(si) = stdin.as_mut() {
                        let _ = writeln!(si, "{text}");
                        let _ = si.flush();
                    }
                }
                Ok(Control::Stop) => {
                    manual = true;
                    set_phase(&shared, |s| s.phase = Phase::Stopping);
                    escalate(&mut child, pid, grace_ms);
                    break child.wait().ok();
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // 槽位被摘除：视同停止请求，收尸后线程自然结束
                    manual = true;
                    escalate(&mut child, pid, 0);
                    break child.wait().ok();
                }
                Err(RecvTimeoutError::Timeout) => match child.try_wait() {
                    Ok(Some(st)) => break Some(st),
                    Ok(None) => {}
                    // try_wait 报错（如 ECHILD）时子进程可能已死：先收尸再退出，避免僵尸
                    Err(_) => {
                        let _ = child.wait();
                        break None;
                    }
                },
            }
        };

        drop(stdin);
        if let Some(h) = out_h {
            let _ = h.join();
        }
        if let Some(h) = err_h {
            let _ = h.join();
        }

        let duration_ms = started.elapsed().as_millis() as u64;
        let (code, signal, success) = match &status {
            Some(st) => (st.code(), signal_of(st), st.success()),
            None => (None, None, false),
        };
        let meta = ExitMeta {
            code,
            signal,
            duration_ms,
            manual,
            at: SystemTime::now(),
        };

        if manual || success {
            set_phase(&shared, |s| {
                s.phase = Phase::Exited;
                s.pid = None;
                s.last_exit = Some(meta);
            });
            return;
        }

        // 异常退出 → 策略分派
        let policy = desc.restart_policy.unwrap_or(RestartPolicy::Never);
        match policy {
            RestartPolicy::Always => {
                set_phase(&shared, |s| s.last_exit = Some(meta));
                if Duration::from_millis(duration_ms) < CRASH_WINDOW {
                    fast_crashes += 1;
                } else {
                    fast_crashes = 0;
                }
                if fast_crashes >= CRASH_LIMIT {
                    set_phase(&shared, |s| s.phase = Phase::Failed);
                    push(
                        &logs,
                        Stream::Stderr,
                        format!(
                            "[task] crash-loop detected ({fast_crashes}x in {}s window), giving up",
                            CRASH_WINDOW.as_secs()
                        ),
                    );
                    return;
                }
                set_phase(&shared, |s| {
                    s.phase = Phase::Starting;
                    s.restarts += 1;
                });
                let backoff = Duration::from_secs(2u64.saturating_pow(attempt + 1).min(60));
                attempt += 1;
                std::thread::sleep(backoff);
                continue; // 监督循环重 spawn
            }
            // Warn 与 Never 的执行一致（不重拉）；Warn 的额外语义由调用方
            // 依据 status().last_exit 呈现，v1 不做展示。
            RestartPolicy::Warn | RestartPolicy::Never => {
                set_phase(&shared, |s| {
                    s.phase = Phase::Failed;
                    s.pid = None;
                    s.last_exit = Some(meta);
                });
                return;
            }
        }
    }
}

/// 有界等待监督线程写入终态（Exited/Failed/Idle，25ms 轮询；供 update 在
/// 注册表写锁外等待 Stop 落地）。超时即返回，SIGKILL 由监督线程宽限后兜底。
fn wait_terminal(shared: &Arc<Mutex<Shared>>, deadline: Instant) {
    while Instant::now() < deadline {
        let phase = shared.lock().unwrap_or_else(|e| e.into_inner()).phase;
        if !matches!(phase, Phase::Starting | Phase::Running | Phase::Stopping) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// SIGTERM 整组 → 宽限轮询 → SIGKILL 整组
fn escalate(child: &mut Child, pid: u32, grace_ms: u64) {
    let pgid = -(pid as i32);
    unsafe {
        libc::kill(pgid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_millis(grace_ms);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            return;
        }
        std::thread::sleep(POLL);
    }
    unsafe {
        libc::kill(pgid, libc::SIGKILL);
    }
}

fn signal_of(st: &ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        st.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = st;
        None
    }
}

fn set_phase(shared: &Arc<Mutex<Shared>>, f: impl FnOnce(&mut Shared)) {
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut s);
}

fn push(logs: &Arc<Mutex<Ring>>, stream: Stream, text: String) {
    let mut r = logs.lock().unwrap_or_else(|e| e.into_inner());
    r.push(LogLine {
        at: SystemTime::now(),
        stream,
        text,
    });
}

fn pump<T: Read + Send + 'static>(
    pipe: Option<T>,
    logs: Arc<Mutex<Ring>>,
    stream: Stream,
) -> Option<JoinHandle<()>> {
    let inner = pipe?;
    Some(std::thread::spawn(move || {
        let mut reader = BufReader::new(inner);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
                    push(&logs, stream, trimmed);
                    line.clear();
                }
                Err(_) => break,
            }
        }
    }))
}

// ---------------- 配置加载与 diff 辅助 ----------------

fn project_dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        // 测试接缝：test_spec_dir 置位时指向 tempdir，绝不触碰真实数据目录。
        let g = test_spec_dir().lock().unwrap_or_else(|e| e.into_inner());
        if g.is_some() {
            return (*g).clone();
        }
    }
    prelude::data_dir().map(PathBuf::from)
}

/// 测试专属数据目录覆写（None = 走真实路径）。仅 task 内测试在 reg 串行锁置位。
#[cfg(test)]
fn test_spec_dir() -> &'static std::sync::Mutex<Option<PathBuf>> {
    static D: std::sync::OnceLock<std::sync::Mutex<Option<PathBuf>>> = std::sync::OnceLock::new();
    D.get_or_init(|| std::sync::Mutex::new(None))
}

/// 坏损规格计数（WARN 的无 stderr 捕获断言接缝，仿 db 的报错计数模式）。
#[cfg(test)]
static SPEC_CORRUPT_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 读 atlas_task.json + 扫描 script/*.ts 生成描述符全集；按 id 去重（JSON 优先）。
/// 第二元素 json_ok：false = 规格文件存在但不可读/解析坏损（空集合不可信，
/// Task::update 据此跳过 gone 清扫与会话对账，保住运行中任务）。
fn load_descs() -> (Vec<TaskDescriptor>, bool) {
    match project_dir() {
        Some(dir) => load_descs_at(&dir),
        None => (Vec::new(), true),
    }
}

/// `load_descs` 的 path-taking 实现（测试接缝：直传 tempdir，仿 db 的 `*_at` 风格）。
fn load_descs_at(dir: &Path) -> (Vec<TaskDescriptor>, bool) {
    let _ = fs::create_dir_all(dir);

    let mut out: Vec<TaskDescriptor> = Vec::new();
    let list = dir.join("atlas_task.json");
    let mut json_ok = true;
    match fs::read_to_string(&list) {
        Ok(text) => match serde_json::from_str::<Vec<TaskDescriptor>>(&text) {
            Ok(v) => out.extend(v),
            Err(e) => {
                json_ok = false;
                warn_corrupt_spec(&list, &e);
            }
        }
        // 文件缺失 = read-miss：维持"空集 = 全部消失"的既有语义。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // 存在但不可读（权限/IO）：同样按坏损处理，绝不当空集。
        Err(e) => {
            json_ok = false;
            warn_corrupt_spec(&list, &e);
        }
    }

    let scripts = dir.join("script");
    if let Ok(entries) = fs::read_dir(&scripts) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("ts") {
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let id = format!("bun_{stem}");
                if out.iter().any(|d| d.id == id) {
                    continue;
                }
                out.push(TaskDescriptor {
                    id,
                    name: format!("🍞 {stem}"),
                    command: "bun".into(),
                    args: vec!["run".into(), path.to_string_lossy().into_owned()],
                    cwd: Some(scripts.to_string_lossy().into_owned()),
                    envs: None,
                    autostart: false,
                    group: "Scripts".into(),
                    log_limit: Some(1000),
                    restart_policy: Some(RestartPolicy::Never),
                    stop_grace_ms: None,
                });
            }
        }
    }
    (out, json_ok)
}

/// 坏损规格的一行 WARN（含路径 + 错误）+ 测试计数。守护进程内不做重试循环。
fn warn_corrupt_spec(path: &Path, err: &dyn std::error::Error) {
    #[cfg(test)]
    SPEC_CORRUPT_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    eprintln!("[task] WARN corrupt task spec {}: {err}", path.display());
}

// ---------------- 测试 ----------------

/// 用例级**会话册串行锁**（域内共享）：tui.rs 的册用例与本域驱动 `Task::update`
/// 的规格对账用例共持——`update()` 末段的 `tui::prune_missing` 是册副作用，会把
/// 兄弟用例在册的活 PTY 会话 guard-drop 击杀（xpty 把信号死亡记成 exit_code=1，
/// 负载下表现为偶发「exit 1」）。锁序全局一致：本锁 → spec 锁 → reg → 册锁。
#[cfg(test)]
pub(crate) fn ledger_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: Mutex<()> = Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(id: &str, cmd: &str, args: &[&str]) -> TaskDescriptor {
        TaskDescriptor {
            id: id.into(),
            name: id.into(),
            command: cmd.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            envs: None,
            autostart: false,
            group: "test".into(),
            log_limit: Some(64),
            restart_policy: Some(RestartPolicy::Never),
            stop_grace_ms: Some(500),
        }
    }

    fn poll_until<F: FnMut() -> bool>(mut f: F, ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    /// 串行化所有触碰全局注册表 / 测试规格目录的用例：Task::update 的 gone 清扫
    /// 会把并发用例刚插入的槽位当"配置已消失"误杀，静态目录覆写也须互斥置位。
    fn spec_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static L: Mutex<()> = Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// tempdir 规格目录夹具：置位 test_spec_dir 覆写，drop 时清除并删除目录。
    /// 用例须持 spec_test_lock（全局覆写单槽）。绝不触碰 ~/.local/share 真实文件。
    #[cfg(unix)]
    struct SpecDirGuard {
        dir: PathBuf,
    }

    #[cfg(unix)]
    impl SpecDirGuard {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "atlas_task_spec_{tag}_{}_{n}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            *test_spec_dir().lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.clone());
            Self { dir }
        }
        fn write_spec(&self, body: &str) {
            fs::write(self.spec_path(), body).unwrap();
        }
        fn spec_path(&self) -> PathBuf {
            self.dir.join("atlas_task.json")
        }
        /// 写一份可解析的有效规格：注册表里的"外来槽位"（非本用例 owned）原样
        /// 序列化进 live 集，使 update 的 gone 清扫与 tui 对账不误伤兄弟用例的
        /// 任务（兄弟 tui 用例也向全局 reg 注入槽位）；无外来槽位时即字面 `[]`。
        /// 快照→写文件→update 紧邻调用，插入竞态残余窗口为微秒级。
        fn write_valid_spec_excluding(&self, owned: &[&str]) {
            let foreign: Vec<TaskDescriptor> = reg()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|(id, _)| !owned.iter().any(|o| *o == id.as_str()))
                .map(|(_, slot)| slot.desc.clone())
                .collect();
            fs::write(self.spec_path(), serde_json::to_string(&foreign).unwrap()).unwrap();
        }
    }

    #[cfg(unix)]
    impl Drop for SpecDirGuard {
        fn drop(&mut self) {
            *test_spec_dir().lock().unwrap_or_else(|e| e.into_inner()) = None;
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn corrupt_hits() -> usize {
        SPEC_CORRUPT_HITS.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 起一个挂起任务（sleep 60，须保持 Running 供清扫观察）。
    #[cfg(unix)]
    fn spawn_sleeper_slot(id: &str) {
        let d = desc(id, "sh", &["-c", "sleep 60"]);
        reg().write().unwrap().insert(id.to_string(), Slot::new(d));
        Task::run(id).unwrap();
        assert!(
            poll_until(
                || Task::status()
                    .iter()
                    .any(|s| s.id == id && s.phase == Phase::Running && s.pid.is_some()),
                5000
            ),
            "{id}: 未进入 Running"
        );
    }

    #[test]
    fn descriptor_full_and_minimal_parse() {
        let full = r#"[{"id":"a","name":"A","command":"sh","args":["-c","true"],"cwd":null,
            "envs":{"K":"V"},"autostart":true,"group":"G","log_limit":10,
            "restart_policy":"Always","stop_grace_ms":1500}]"#;
        let v: Vec<TaskDescriptor> = serde_json::from_str(full).unwrap();
        assert_eq!(v[0].restart_policy, Some(RestartPolicy::Always));
        assert_eq!(v[0].stop_grace_ms, Some(1500));
        assert!(v[0].autostart);

        let min = r#"[{"id":"b","name":"B","command":"ls","args":[]}]"#;
        let v: Vec<TaskDescriptor> = serde_json::from_str(min).unwrap();
        assert!(!v[0].autostart);
        assert_eq!(v[0].group, "");
    }

    #[test]
    fn ring_caps_and_counts_dropped() {
        let mut ring = Ring {
            lines: VecDeque::new(),
            cap: 3,
            dropped: 0,
        };
        for i in 0..5 {
            ring.push(LogLine {
                at: SystemTime::now(),
                stream: Stream::Stdout,
                text: i.to_string(),
            });
        }
        assert_eq!(ring.lines.len(), 3);
        assert_eq!(ring.dropped, 2);
        assert_eq!(ring.lines.back().unwrap().text, "4");
    }

    #[test]
    fn unknown_ids_error_cleanly() {
        assert_eq!(Task::run("nope"), Err(TaskError::UnknownId("nope".into())));
        assert_eq!(Task::stop("nope"), Err(TaskError::UnknownId("nope".into())));
        assert_eq!(
            Task::input("nope", "x"),
            Err(TaskError::UnknownId("nope".into()))
        );
        assert!(Task::output("nope").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn full_lifecycle_run_input_output_stop() {
        // 隔离：直接向注册表注入槽位，不依赖真实配置文件
        let _g = spec_test_lock();
        let d = desc("life1", "sh", &["-c", "echo ready; exec cat"]);
        reg().write().unwrap().insert("life1".into(), Slot::new(d));

        Task::run("life1").unwrap();
        // 双重启动被拒
        assert_eq!(
            Task::run("life1"),
            Err(TaskError::AlreadyRunning("life1".into()))
        );

        assert!(
            poll_until(
                || Task::status()
                    .iter()
                    .any(|s| s.id == "life1" && s.phase == Phase::Running && s.pid.is_some()),
                5000
            ),
            "未进入 Running"
        );
        assert!(
            poll_until(
                || Task::output("life1")
                    .map(|l| l
                        .iter()
                        .any(|x| x.stream == Stream::Stdout && x.text == "ready"))
                    .unwrap_or(false),
                5000
            ),
            "stdout 未见 ready"
        );

        Task::input("life1", "ping").unwrap();
        assert!(
            poll_until(
                || {
                    let out = Task::output("life1").unwrap_or_default();
                    out.iter()
                        .any(|x| x.stream == Stream::Stdout && x.text == "ping")
                        && out
                            .iter()
                            .any(|x| x.stream == Stream::Stdin && x.text == "ping")
                },
                5000
            ),
            "stdin 回声或 cat 回显缺失"
        );

        Task::stop("life1").unwrap();
        assert!(
            poll_until(
                || Task::status().iter().any(|s| s.id == "life1"
                    && s.phase == Phase::Exited
                    && s.last_exit.as_ref().is_some_and(|m| m.manual)),
                5000
            ),
            "未到 Exited(manual)"
        );
        // 终态幂等保护
        assert_eq!(
            Task::stop("life1"),
            Err(TaskError::NotRunning("life1".into()))
        );
        reg().write().unwrap().remove("life1");
    }

    #[cfg(unix)]
    #[test]
    fn crash_exit_lands_failed_with_meta() {
        let _g = spec_test_lock();
        let d = desc("fail1", "sh", &["-c", "exit 3"]);
        reg().write().unwrap().insert("fail1".into(), Slot::new(d));
        Task::run("fail1").unwrap();
        assert!(
            poll_until(
                || Task::status().iter().any(|s| s.id == "fail1"
                    && s.phase == Phase::Failed
                    && s.last_exit.as_ref().is_some_and(|m| m.code == Some(3))),
                5000
            ),
            "未到 Failed(code=3)"
        );
        reg().write().unwrap().remove("fail1");
    }

    #[cfg(unix)]
    #[test]
    fn group_kill_sweeps_grandchildren() {
        // sh 起一个睡死的孙子；停止后整组必须消亡（孙子不能被 setsid 逃逸）
        let _g = spec_test_lock();
        let d = desc("grp1", "sh", &["-c", "sleep 120 & echo ready; wait"]);
        reg().write().unwrap().insert("grp1".into(), Slot::new(d));
        Task::run("grp1").unwrap();
        assert!(
            poll_until(
                || Task::status()
                    .iter()
                    .any(|s| s.id == "grp1" && s.phase == Phase::Running && s.pid.is_some()),
                5000
            ),
            "未进入 Running"
        );
        let pid = Task::status()
            .iter()
            .find(|s| s.id == "grp1")
            .and_then(|s| s.pid)
            .unwrap();
        Task::stop("grp1").unwrap();
        assert!(
            poll_until(
                || Task::status()
                    .iter()
                    .any(|s| s.id == "grp1" && matches!(s.phase, Phase::Exited | Phase::Failed)),
                5000
            ),
            "未到终态"
        );
        // 孙子 sleep 应与组同灭：kill(pid, 0) 对整组报错
        let gone = unsafe { libc::kill(-(pid as i32), 0) } == -1;
        assert!(gone, "进程组仍有残留（孤儿）");
        reg().write().unwrap().remove("grp1");
    }

    // ---- 坏损规格保护（audit F5：坏配置不得当"空配置集"误杀运行中任务）----

    #[cfg(unix)]
    #[test]
    fn corrupt_spec_keeps_running_tasks_alive() {
        // Given: 垃圾字节 / 截断 JSON / 空文件 三种畸形规格各驱一轮
        let _g = spec_test_lock();
        // 坏损轮的 update 跳过册对账（json_ok=false），本锁今日非必需；持它是为
        // 锁死该不变量：日后若混入有效载荷，prune 侧击自动被互斥接管。
        let _l = ledger_test_lock();
        let sd = SpecDirGuard::new("corrupt");
        let payloads: [(&str, &str); 3] = [
            ("garbage", "\u{0}\u{1b}\u{7f}not-json\u{8}"),
            ("truncated", "[{\"id\":\"cfg\",\"name\":\"C\",\"command\":\"sh\","),
            ("empty", ""),
        ];
        for (tag, body) in payloads {
            let id = format!("cfg_{tag}");
            sd.write_spec(body);
            spawn_sleeper_slot(&id);

            // When: 重复中断下连续两次重扫
            let hits0 = corrupt_hits();
            let snap1 = Task::update();
            let snap2 = Task::update();

            // Then: 每次坏损都计 WARN；任务不被摘除、始终 Running；pid 不变（未重启）
            assert!(
                corrupt_hits() >= hits0 + 2,
                "{tag}: corrupt WARN 计数未推进"
            );
            let s1 = snap1.iter().find(|s| s.id == id);
            let s2 = snap2.iter().find(|s| s.id == id);
            assert!(
                s1.is_some_and(|s| s.phase == Phase::Running)
                    && s2.is_some_and(|s| s.phase == Phase::Running),
                "{tag}: corrupt spec 误停止/摘除了任务 {id}"
            );
            assert_eq!(
                s1.and_then(|s| s.pid),
                s2.and_then(|s| s.pid),
                "{tag}: corrupt spec 不应重启任务"
            );
            // 清场：摘槽 → ctl 断连 → 监督线程 escalate 收尸子进程
            reg().write().unwrap().remove(&id);
        }
    }

    /// 等注册表只剩本用例槽位（兄弟 tui 用例的注入不可控时，缺失规格用例
    /// 无法用规格保护外来槽——语义本就是"全部消失"，只能等静默窗口）。
    #[cfg(unix)]
    fn wait_reg_quiet(owned: &[&str]) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let quiet = reg()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .all(|k| owned.iter().any(|o| *o == k.as_str()));
            if quiet {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("注册表被并发用例长期占用，无法独验 gone 语义");
    }

    #[cfg(unix)]
    #[test]
    fn valid_empty_spec_still_sweeps_gone_tasks() {
        // Given: 可解析的空规格 + 一个配置里的挂起任务。持册锁独占：兄弟 tui
        // 用例此刻不得向册注入活会话（update 的会话对账会 SIGKILL 它们）；
        // write_valid_spec_excluding 保留为第二道防线（快照→写文件仍紧邻调用）。
        let _g = spec_test_lock();
        let _l = ledger_test_lock();
        let sd = SpecDirGuard::new("sweep");
        spawn_sleeper_slot("gone1");
        let pid = Task::status()
            .iter()
            .find(|s| s.id == "gone1")
            .and_then(|s| s.pid)
            .unwrap();
        let hits0 = corrupt_hits();
        sd.write_valid_spec_excluding(&["gone1"]);

        // When
        let snap = Task::update();

        // Then: 真空集照旧 = 消失任务被摘（既有语义保留），且不计 corrupt；子进程随组消亡
        assert!(
            snap.iter().all(|s| s.id != "gone1"),
            "可解析空规格必须仍摘除消失任务"
        );
        assert_eq!(corrupt_hits(), hits0, "可解析规格不是坏损");
        let gone = unsafe { libc::kill(-(pid as i32), 0) } == -1;
        assert!(gone, "被扫任务进程组仍有残留（孤儿）");
    }

    #[cfg(unix)]
    #[test]
    fn missing_spec_file_keeps_gone_semantics() {
        // Given: tempdir 里没有 atlas_task.json（read-miss ≠ 坏损）。文件缺失时
        // 任何在册任务都该被扫。持册锁独占：缺失规格 = 空 live 集，update 的
        // 会话对账会击杀兄弟 tui 用例在册的活会话，必须互斥。
        let _g = spec_test_lock();
        let _l = ledger_test_lock();
        let _sd = SpecDirGuard::new("miss");
        spawn_sleeper_slot("gone2");
        wait_reg_quiet(&["gone2"]);
        let hits0 = corrupt_hits();

        // When
        let snap = Task::update();

        // Then: 维持"空集 = 全部消失"的既有语义，且不计 corrupt
        assert!(
            snap.iter().all(|s| s.id != "gone2"),
            "缺失规格文件应维持原全部消失语义"
        );
        assert_eq!(corrupt_hits(), hits0, "文件缺失不是坏损");
    }

    #[test]
    fn load_descs_at_distinguishes_missing_valid_and_corrupt() {
        let _g = spec_test_lock();
        let dir = std::env::temp_dir().join(format!(
            "atlas_task_spec_loader_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        // 缺失文件 = read-miss：空集 + ok
        let (v, ok) = load_descs_at(&dir);
        assert!(v.is_empty() && ok, "缺失文件应为 read-miss（ok）");
        // 可解析且非空：描述符入集 + ok
        fs::write(
            dir.join("atlas_task.json"),
            r#"[{"id":"x","name":"X","command":"true"}]"#,
        )
        .unwrap();
        let (v, ok) = load_descs_at(&dir);
        assert!(ok && v.iter().any(|d| d.id == "x"), "有效规格应加载且 ok");
        // 截断 JSON：空集 + !ok + 计数恰 +1
        fs::write(dir.join("atlas_task.json"), "[{").unwrap();
        let hits0 = corrupt_hits();
        let (v, ok) = load_descs_at(&dir);
        assert!(!ok && v.is_empty(), "坏损规格应为空集 + !ok");
        assert_eq!(corrupt_hits(), hits0 + 1, "坏损恰好计一次 WARN");
        let _ = fs::remove_dir_all(&dir);
    }
}
