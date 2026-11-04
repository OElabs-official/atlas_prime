//! v1/system/proc — 进程观测面：/proc pid 表直读（仅本 uid 可见）→ 快照缓存。
//!
//! Termux 无特权域下 /proc readdir 只枚举本 uid 的 pid（hidepid），此即本域
//! 可见边界——只治自家进程，无 root 桥（本域源链就一条腿）。每 pid 四件：
//! status（VmRSS/State/Name）、stat（utime+stime、starttime——comm 可含空格与
//! 括号，按**最后一个 ')'** 切野）、cmdline（NUL→空格、trim、截 160 字符，空件
//! 回落 comm）、comm。读取中途消失/拒权的 pid 静默跳过——pid 表是竞态对象，
//! 缺席≠错误。
//!
//! CPU% 为跨拍差分：模块级基线表 `HashMap<pid,(starttime,ticks,wall_ns)>`
//! （与 [`super::gpu`] prev_busy 同款 Mutex 持态），每拍
//! `Δticks ÷ USER_HZ ÷ Δwall_secs × 100`。首见只立基线不给速率（0.0）；
//! starttime 漂移 = pid 复用转世，按新进程重置基线（0.0）。基线每拍整表重建
//! （消失 pid 自然出局）。
//!
//! 自发运行体归操作器（本模块只供 scan/update/current 原语，同 speed 纪律）：
//! 5s 一轮 spawn_blocking 现扫后整拍换入 [`update`]，web 经 [`current`] 纯缓存
//! 读、绝不 inline 现扫；本域任何数据不落库。

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Instant;

use chrono::{SecondsFormat, Utc};

/// 内核 USER_HZ（sysconf(_SC_CLK_TCK)）：aarch64 Android 恒 100，
/// stat 的 utime/stime/starttime 皆以此为单位计 tick
const USER_HZ: f64 = 100.0;

/// cmd 展示截断上限（字符数，非字节）
const CMD_MAX: usize = 160;

/// 单进程行（`/system/proc` procs 成员，字段即冻结契约）
#[derive(Debug, Serialize)]
pub struct ProcRow {
    pub pid: u32,
    pub comm: String,
    pub cmd: String,
    pub rss_kb: u64,
    /// 跨拍差分 CPU%（首见/pid 复用拍恒 0.0）
    pub cpu_pct: f64,
    /// 进程状态单字符（R|S|D|Z|T…，status State 野首字）
    pub state: char,
}

/// 一轮进程快照（web `/system/proc` 原样序列化；procs 按 rss_kb 降序）
#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub procs: Vec<ProcRow>,
    pub count: usize,
    pub total_rss_kb: u64,
    /// 采集时刻 RFC3339（秒精度 UTC）；预热前 None
    pub scanned_at: Option<String>,
    /// 预热成因（首轮后恒 None）
    pub reason: Option<String>,
}

impl Snapshot {
    /// 预热形（首轮 update 前 web 所见）
    pub fn warming() -> Snapshot {
        Snapshot {
            procs: Vec::new(),
            count: 0,
            total_rss_kb: 0,
            scanned_at: None,
            reason: Some("warming".to_string()),
        }
    }
}

// ---------------- 跨拍基线（CPU% 差分持态） ----------------

/// 基线条目：(starttime ticks, utime+stime 累计 ticks, 单调墙钟 ns)
#[derive(Clone, Copy, Debug)]
struct Base {
    starttime: u64,
    cpu_ticks: u64,
    wall_ns: u64,
}

fn baselines() -> &'static Mutex<HashMap<u32, Base>> {
    static S: OnceLock<Mutex<HashMap<u32, Base>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 单调纳秒（进程启动锚点，免疫墙钟跳变）
fn mono_ns() -> u64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// 跨拍差分（纯函数）：cpu_pct = Δticks ÷ USER_HZ ÷ Δwall_secs × 100，
/// prev/cur 同形（cur 即本拍待存基线）。starttime 漂移（pid 复用）→ 0.0
/// （新进程，上拍账目无关）；窗口未前进 → 0.0；ticks 倒退饱和钳 0。
fn cpu_pct(prev: &Base, cur: &Base) -> f64 {
    if prev.starttime != cur.starttime {
        return 0.0;
    }
    let dt_secs = cur.wall_ns.saturating_sub(prev.wall_ns) as f64 / 1e9;
    if dt_secs <= 0.0 {
        return 0.0;
    }
    let cpu_secs = cur.cpu_ticks.saturating_sub(prev.cpu_ticks) as f64 / USER_HZ;
    cpu_secs / dt_secs * 100.0
}

// ---------------- 纯解析（fixture 单测钉死，不碰 /proc） ----------------

/// status 单 pid 解析产物（缺野按缺席值：rss 0 / state 'S' / 名空）
#[derive(Debug, PartialEq, Eq)]
struct PStatus {
    rss_kb: u64,
    state: char,
    name: String,
}

fn parse_status(text: &str) -> PStatus {
    let mut p = PStatus {
        rss_kb: 0,
        state: 'S',
        name: String::new(),
    };
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("VmRSS:") {
            p.rss_kb = v
                .split_whitespace()
                .next()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("State:") {
            p.state = v.trim().chars().next().unwrap_or('S');
        } else if let Some(v) = line.strip_prefix("Name:") {
            p.name = v.trim().to_string();
        }
    }
    p
}

/// stat 解析产物：comm + (utime+stime) 累计 ticks + starttime
#[derive(Debug, PartialEq, Eq)]
struct PStat {
    comm: String,
    cpu_ticks: u64,
    starttime: u64,
}

/// comm 可含空格与括号（`7 (my (weird) proc) S ...`），全部字段按**最后一个
/// ')'** 之后切：其后首词 = 字段 3（state），故 utime=下标 11、stime=下标 12、
/// starttime=下标 19。
fn parse_stat(text: &str) -> Option<PStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close < open {
        return None;
    }
    let comm = text[open + 1..close].to_string();
    let f: Vec<&str> = text[close + 1..].split_whitespace().collect();
    if f.len() < 20 {
        return None;
    }
    let utime: u64 = f[11].parse().ok()?;
    let stime: u64 = f[12].parse().ok()?;
    let starttime: u64 = f[19].parse().ok()?;
    Some(PStat {
        comm,
        cpu_ticks: utime.saturating_add(stime),
        starttime,
    })
}

/// cmdline 正文 → cmd：NUL→空格、trim、截 [`CMD_MAX`] 字符；
/// 空正文（无 cmdline 或全 NUL）回落 comm
fn join_cmdline(raw: &[u8], comm: &str) -> String {
    let s = String::from_utf8_lossy(raw).replace('\0', " ");
    let s = s.trim();
    if s.is_empty() {
        return comm.to_string();
    }
    s.chars().take(CMD_MAX).collect()
}

// ---------------- 现扫 ----------------

/// 一轮现扫（阻塞 IO，放 spawn_blocking 调用）：/proc readdir → 仅数字目录 →
/// 逐 pid 四件 → 基线差分 CPU% → rss_kb 降序（同值按 pid 升序，输出确定性）。
/// 消失/拒权 pid 静默跳过；/proc 本体不可读 = 空表（本机即无自家进程可见）。
pub fn scan() -> Snapshot {
    let wall_ns = mono_ns(); // 全轮同一时刻戳：基线与差分共用
    let prev = baselines()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut next: HashMap<u32, Base> = HashMap::new();
    let mut procs: Vec<ProcRow> = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
                continue; // 非数字目录名（self/sys/net…）不是 pid
            };
            let dir = e.path();
            let Ok(status) = std::fs::read_to_string(dir.join("status")) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else {
                continue;
            };
            let Some(st) = parse_stat(&stat) else {
                continue;
            };
            let ps = parse_status(&status);
            // comm 回落链：comm 文件 → status Name → stat 括号内 comm
            let mut comm = std::fs::read_to_string(dir.join("comm"))
                .map(|t| t.trim().to_string())
                .unwrap_or_default();
            if comm.is_empty() {
                comm = if ps.name.is_empty() { st.comm } else { ps.name };
            }
            let cmd = match std::fs::read(dir.join("cmdline")) {
                Ok(b) => join_cmdline(&b, &comm),
                Err(_) => comm.clone(),
            };
            let cur = Base {
                starttime: st.starttime,
                cpu_ticks: st.cpu_ticks,
                wall_ns,
            };
            let pct = match prev.get(&pid) {
                Some(b) => cpu_pct(b, &cur),
                None => 0.0, // 首见：只立基线
            };
            next.insert(pid, cur);
            procs.push(ProcRow {
                pid,
                comm,
                cmd,
                rss_kb: ps.rss_kb,
                cpu_pct: pct,
                state: ps.state,
            });
        }
    }
    *baselines().lock().unwrap_or_else(|e| e.into_inner()) = next;
    let total_rss_kb = procs
        .iter()
        .fold(0u64, |acc, r| acc.saturating_add(r.rss_kb));
    procs.sort_by(|a, b| b.rss_kb.cmp(&a.rss_kb).then(a.pid.cmp(&b.pid)));
    Snapshot {
        count: procs.len(),
        total_rss_kb,
        procs,
        scanned_at: Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
        reason: None,
    }
}

// ---------------- 最新快照缓存（单槽整换） ----------------

/// 操作器 5s 回路写、web 读路径（阻塞池）读；None = 预热期
fn latest() -> &'static RwLock<Option<Arc<Snapshot>>> {
    static S: OnceLock<RwLock<Option<Arc<Snapshot>>>> = OnceLock::new();
    S.get_or_init(|| RwLock::new(None))
}

/// 收编一轮结果（单槽整换：读者所见恒为完整旧拍或完整新拍）
pub fn update(snap: Snapshot) {
    *latest().write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(snap));
}

/// 缓存读（web 唯一读路径）：首轮前回预热形，绝不现扫
pub fn current() -> Arc<Snapshot> {
    latest()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| Arc::new(Snapshot::warming()))
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parses_rss_state_name_including_cjk() {
        let p = parse_status(
            "Name:\t输入法守护\nState:\tS (sleeping)\nVmRSS:\t   42108 kB\nThreads:\t9\n",
        );
        assert_eq!(p.rss_kb, 42108);
        assert_eq!(p.state, 'S');
        assert_eq!(p.name, "输入法守护", "CJK comm 原样保留");
        // 缺 VmRSS 行（理论形）：rss 缺席值 0，其余照收
        let q = parse_status("Name:\tx\nState:\tR (running)\n");
        assert_eq!(q.rss_kb, 0);
        assert_eq!(q.state, 'R');
    }

    #[test]
    fn stat_cuts_at_last_paren_with_weird_comm() {
        // comm 含空格与括号：字段全按最后一个 ')' 后数
        let s = parse_stat(
            "7 (my (weird) proc) S 1 7 7 0 -1 0 0 0 0 0 1000 250 0 0 0 0 0 0 12345 0 0",
        )
        .expect("合法 stat 必可析");
        assert_eq!(s.comm, "my (weird) proc");
        assert_eq!(s.cpu_ticks, 1250, "utime+stime=1000+250");
        assert_eq!(s.starttime, 12345);
        // 常规单括号 comm 同样成立
        let t = parse_stat(
            "1 (atlas) R 0 1 1 0 -1 0 0 0 0 0 40 10 0 0 0 0 0 0 777",
        )
        .expect("常规形必可析");
        assert_eq!((t.comm.as_str(), t.cpu_ticks, t.starttime), ("atlas", 50, 777));
        // 字段不足 / 无括号：拒析（调用侧跳过该 pid）
        assert!(parse_stat("2 (x) S 1 2").is_none());
        assert!(parse_stat("garbage").is_none());
    }

    #[test]
    fn cmdline_joins_nul_trims_and_falls_back_to_comm() {
        assert_eq!(
            join_cmdline(b"cargo test v1::\0--nocapture\0", "cargo"),
            "cargo test v1:: --nocapture"
        );
        assert_eq!(join_cmdline(b"\0\0", "atlasd"), "atlasd", "空正文回落 comm");
        assert_eq!(join_cmdline(b"   ", "atlasd"), "atlasd", "全空白亦空正文");
        let long = join_cmdline([b'a'; 200].as_slice(), "a");
        assert_eq!(long.chars().count(), CMD_MAX, "截 160 字符（非字节）");
        let cjk = join_cmdline("输入\0法\0".as_bytes(), "法");
        assert_eq!(cjk, "输入 法", "CJK 按字符截不断码点");
    }

    #[test]
    fn cpu_delta_math_and_pid_reuse_reset() {
        let prev = Base {
            starttime: 500,
            cpu_ticks: 1000,
            wall_ns: 0,
        };
        let at = |starttime: u64, cpu_ticks: u64, wall_ns: u64| Base {
            starttime,
            cpu_ticks,
            wall_ns,
        };
        // Δticks=500 ÷ 100 = 5 CPU 秒，窗口 5s → 恰 100%
        assert_eq!(cpu_pct(&prev, &at(500, 1500, 5_000_000_000)), 100.0);
        // 半载：Δticks=250 ÷ 100 = 2.5s / 5s = 50%
        assert_eq!(cpu_pct(&prev, &at(500, 1250, 5_000_000_000)), 50.0);
        // starttime 漂移 = pid 复用转世 → 0.0（重置基线由调用侧整表重建完成）
        assert_eq!(cpu_pct(&prev, &at(501, 9_999_999, 5_000_000_000)), 0.0);
        // 窗口未前进 / ticks 倒退 → 0.0 与饱和钳 0
        assert_eq!(cpu_pct(&prev, &at(500, 1600, 0)), 0.0);
        assert_eq!(cpu_pct(&prev, &at(500, 900, 5_000_000_000)), 0.0);
    }

    #[test]
    fn warming_shape_serialization_is_frozen_contract() {
        let v: serde_json::Value = serde_json::to_value(Snapshot::warming()).unwrap();
        assert_eq!(v["procs"], serde_json::json!([]));
        assert_eq!(v["count"], serde_json::json!(0));
        assert_eq!(v["total_rss_kb"], serde_json::json!(0));
        assert_eq!(v["scanned_at"], serde_json::Value::Null);
        assert_eq!(v["reason"], serde_json::json!("warming"));
        // serde_json 无 preserve_order：对象键按字母序出（与 /system/ip 同款注记），
        // 契约钉的是键集合，不是出键顺序；键集合等式即缺席证明——
        // 任何已移除字段（如原耗时键）重现都会多出成员而红
        let mut keys: Vec<&str> = v.as_object().expect("对象形").keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["count", "procs", "reason", "scanned_at", "total_rss_kb"],
            "键集合 = 冻结契约五键，无多余成员"
        );
    }

    #[test]
    fn cache_roundtrip_update_then_current() {
        let mut s = Snapshot::warming();
        s.procs = vec![ProcRow {
            pid: 4242,
            comm: "unittest".into(),
            cmd: "unittest --fixture".into(),
            rss_kb: 1024,
            cpu_pct: 0.0,
            state: 'S',
        }];
        s.count = 1;
        s.total_rss_kb = 1024;
        s.scanned_at = Some("2026-01-01T00:00:00Z".into());
        s.reason = None;
        update(s);
        let got = current();
        assert_eq!(got.count, 1);
        assert_eq!(got.procs[0].pid, 4242);
        assert_eq!(got.reason, None, "update 后预热成因消失");
        assert!(got.scanned_at.is_some());
    }

    /// 真机冒烟：无 root 域下 scan() 必回自洽快照（本进程必在表内、rss>0、
    /// count=procs.len()、scanned_at 已填、无预热成因）。
    /// `cargo test v1::system::proc -- --ignored`
    #[test]
    #[ignore]
    fn live_scan_covers_self() {
        let s = scan();
        println!("live proc: {} procs, total {} KiB", s.count, s.total_rss_kb);
        let me = std::process::id();
        assert_eq!(s.count, s.procs.len());
        assert!(s.reason.is_none() && s.scanned_at.is_some());
        let mine = s.procs.iter().find(|r| r.pid == me).expect("本进程必可见");
        assert!(mine.rss_kb > 0, "自家 rss 必正: {mine:?}");
        assert!(
            s.procs.windows(2).all(|w| w[0].rss_kb >= w[1].rss_kb),
            "rss 降序"
        );
        assert!(s.procs.iter().all(|r| r.cpu_pct >= 0.0));
    }
}
