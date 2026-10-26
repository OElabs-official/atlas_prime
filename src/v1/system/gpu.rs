//! v1/system/gpu — GPU 频率/使用率采集：sysfs 直读 → su root 桥（network devsrc
//! 同款纪律，本地复刻、跨模块不抽共享）。封存域 SELinux 按文件贴标——rig 实测
//! busy% 可直读（"0 %"）而 clock_mhz/max_gpuclk EACCES：故 su 触发判据是
//! **频率锚未直接到手且锚类文件（kgsl clock_mhz / devfreq cur_freq / devfreq 目录）
//! 遭权限拒/缺档**，绝非“任一文件直读成功即整腿成立”。锚到手即零 fork 直装；
//! su 每拍**一次 fork** 抓齐全部文件（固定路径表 + devfreq 通配交设备 shell 展开，
//! `@@@<路径>` 分段），与直读表**并集合并（su 非空段胜出）**。无 root
//! （su 拉起 ENOENT / 桩件非零退出）= `available:false` 持久终态，日志零噪音。
//!
//! 源序：kgsl 主源（`/sys/class/kgsl/kgsl-3d0`：busy%、clock_mhz、max_gpuclk、
//! gpubusy 差分备选）→ devfreq 兜底（`/sys/class/devfreq/{*gpu*,*kgsl*}` 的
//! cur/max_freq；实测 `load` 文件为空 → 空/缺席一律按“无使用率”，永不算错）。
//! **频率是锚**：两源频率俱缺即 available:false。gpubusy 走累计对差分
//! （Δbusy/Δtotal×100 钳 0..=100，首读无使用率；web/操作器并发读偶发丢一拍差分，可接受）。
//!
//! 勿与 thermal 的 `temp.gpu.N` 混同：那是 gpuss-* 温度（℃），本域是频率（MHz）与
//! 使用率（%）——温度≠频率/使用率。无 root 时卡片前端自动隐藏（契约）。

use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// su 子进程等待上限（devsrc 同钳：容 Magisk 守护冷启，越界 kill+wait）
const SU_BOUND: Duration = Duration::from_millis(1500);
/// `try_wait` 轮询步长（notify/devsrc 同款节奏）
const POLL_STEP: Duration = Duration::from_millis(20);

// kgsl 主源四件套（固定清单，su 与直读共用）
const KGSL_BUSY_PCT: &str = "/sys/class/kgsl/kgsl-3d0/gpu_busy_percentage";
const KGSL_CLK: &str = "/sys/class/kgsl/kgsl-3d0/clock_mhz";
const KGSL_MAX: &str = "/sys/class/kgsl/kgsl-3d0/max_gpuclk";
const KGSL_BUSY: &str = "/sys/class/kgsl/kgsl-3d0/gpubusy";

// devfreq 兜底三件套通配（su 内由设备 shell 展开；未命中通配原样落空段）
const DEVFREQ_GLOBS: &str = "/sys/class/devfreq/*gpu*/cur_freq /sys/class/devfreq/*gpu*/max_freq /sys/class/devfreq/*gpu*/load /sys/class/devfreq/*kgsl*/cur_freq /sys/class/devfreq/*kgsl*/max_freq /sys/class/devfreq/*kgsl*/load";
const DEVFREQ_DIRS: &str = "/sys/class/devfreq";

/// 路径→正文（空正文 = 文件空/通配未命中，一律按无数据消化）
type ParsedFiles = BTreeMap<String, String>;
/// 上次 gpubusy/devfreq load 累计对：(来源路径, (busy, total))
type PrevBusy = (String, (u64, u64));

/// GPU 频率/使用率快照（web `/system/gpu` 原样序列化；不可用时卡片隐藏）
#[derive(Debug, Serialize)]
pub struct Gpu {
    /// 频率锚是否拿到（false = 本域缺席，reason 必随）
    pub available: bool,
    /// 使用率 %（gpubusy 差分首读等场景可为 None）
    pub load_pct: Option<f64>,
    /// 当前频率 MHz（锚：available ⇔ 此项 Some）
    pub freq_mhz: Option<f64>,
    /// 封顶频率 MHz
    pub max_mhz: Option<f64>,
    /// 频率锚来源："kgsl" | "devfreq"
    pub source: Option<String>,
    /// 缺席成因（available:true 时必 None）
    pub reason: Option<String>,
}

impl Gpu {
    /// 【公开入口】一轮采集：直读建表 → 频率锚已可析即直装（permissive 域零 fork）；
    /// 锚缺且锚类文件遭权限拒/缺档才走 su 腿，抓体与直读表并集合并（su 非空段胜出）；
    /// su 腿本身失败（NoSu/拒权/超时）以 SuFail 成因为由缺席。
    pub fn get() -> Gpu {
        let d = direct_read();
        let files = if freq_anchor(&d.files).is_some() {
            d.files
        } else if d.freq_denied {
            match su_leg() {
                Ok(text) => merge_files(d.files, parse_proc_grab(&text)),
                Err(su) => return Gpu::absent(su.reason()),
            }
        } else if let Some(msg) = d.bad {
            return Gpu::absent(msg);
        } else {
            return Gpu::absent("sysfs 未见 GPU 频率源（kgsl/devfreq 俱缺）".to_string());
        };
        let prev = prev_busy().lock().unwrap_or_else(|e| e.into_inner()).take();
        let (gpu, next) = assemble(&files, prev);
        *prev_busy().lock().unwrap_or_else(|e| e.into_inner()) = next;
        gpu
    }

    fn absent(reason: String) -> Gpu {
        Gpu {
            available: false,
            load_pct: None,
            freq_mhz: None,
            max_mhz: None,
            source: None,
            reason: Some(reason),
        }
    }
}

// ---------------- 直读腿 ----------------

/// 直读腿产物：文件表 + 锚类文件是否遭权限拒/缺档 + 非权限类异常（su 帮不上）
struct Direct {
    files: ParsedFiles,
    /// 锚类（KGSL_CLK / devfreq cur_freq / devfreq 目录本身）命中 PermissionDenied/NotFound
    freq_denied: bool,
    bad: Option<String>,
}

/// 直读整表（零 fork）。封存域按文件贴标：busy% 可读≠锚可读，锚判据见 [`freq_anchor`]，
/// 故此处只如实记账——锚可析即直装、锚被权限拒/缺档才够格下 su 腿。
fn direct_read() -> Direct {
    let mut d = Direct {
        files: ParsedFiles::new(),
        freq_denied: false,
        bad: None,
    };
    for path in [KGSL_BUSY_PCT, KGSL_CLK, KGSL_MAX, KGSL_BUSY] {
        attempt(&mut d, path, path == KGSL_CLK);
    }
    let (cands, dir_denied) = devfreq_candidates();
    d.freq_denied |= dir_denied;
    for path in cands {
        attempt(&mut d, &path, path.ends_with("/cur_freq"));
    }
    d
}

/// 单路径直读记账：空件/权限拒/缺档静默（锚类拒档另立旗），其余 IO 错留话
fn attempt(d: &mut Direct, path: &str, freq_src: bool) {
    match std::fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => {}
        Ok(t) => {
            d.files.insert(path.to_string(), t);
        }
        Err(e) if matches!(e.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound) => {
            if freq_src {
                d.freq_denied = true;
            }
        }
        Err(e) => {
            if d.bad.is_none() {
                d.bad = Some(format!("sysfs 直读异常 {path}: {e}"));
            }
        }
    }
}

/// 直读侧 devfreq 候选（无 glob crate：read_dir 按名筛 gpu/kgsl，与 su 通配同判据）。
/// 返回目录不可读旗——目录被拒/缺失时 cur_freq 无从枚举，等同锚类缺档。
fn devfreq_candidates() -> (Vec<String>, bool) {
    let rd = match std::fs::read_dir(DEVFREQ_DIRS) {
        Ok(rd) => rd,
        Err(e) => {
            let denied = matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            );
            return (Vec::new(), denied);
        }
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if name.contains("gpu") || name.contains("kgsl") {
            for leaf in ["cur_freq", "max_freq", "load"] {
                out.push(format!("{DEVFREQ_DIRS}/{name}/{leaf}"));
            }
        }
    }
    (out, false)
}

/// 直读表 ∪ su 抓体表（纯函数）：su 非空段胜出——封存域直读可能拿到空壳/陈旧值；
/// 空段（通配未命中/单文件被拒）不得抹掉直读实值。
fn merge_files(mut direct: ParsedFiles, su: ParsedFiles) -> ParsedFiles {
    for (k, v) in su {
        if !v.trim().is_empty() {
            direct.insert(k, v);
        }
    }
    direct
}

// ---------------- su 腿（devsrc 纪律本地复刻） ----------------

/// su 腿三分成因（无 thiserror，随仓库风格；折叠进 Gpu::reason 呈现）
#[derive(Debug)]
enum SuFail {
    /// spawn ENOENT = 未 root（或 su 不在 PATH），干净终态
    NoSu,
    /// 非零退出（Magisk 拒授权即此形态）/ 管道故障
    Failed(String),
    /// [`SU_BOUND`] 内未退出，已 kill+wait
    Timeout,
}

impl SuFail {
    fn reason(&self) -> String {
        match self {
            SuFail::NoSu => "设备无 su（未 root），GPU 读数不可用".to_string(),
            SuFail::Failed(d) => format!("su 桥失败: {d}"),
            SuFail::Timeout => format!("su 桥超 {SU_BOUND:?} 未退出，已 kill"),
        }
    }
}

/// su 腿 = 计划 + 取回（每拍一次 fork 抓齐全部文件）
fn su_leg() -> Result<String, SuFail> {
    let (prog, args) = su_plan();
    su_fetch(&prog, &args, SU_BOUND)
}

/// 纯计划器（单一事实源，单测钉死）：单条 for 循环整抓，`@@@"$f"` 分段，
/// cat 失败消音（通配未命中/单文件被拒 → 空段）。devfreq 通配由设备 shell 展开。
/// 尾缀 `; true`：循环退出码=末条 cat（rig 实测 busmon 无 load 节点 → 假性非零），
/// 正文完备性由解析判、不由退出码判；拒授权时内层 shell 根本不执行、`; true` 不跑，
/// su 自身非零退出原样保留——glob-miss 洗白、denial 不洗。
fn su_plan() -> (String, Vec<String>) {
    (
        "su".to_string(),
        vec![
            "-c".to_string(),
            format!(
                "for f in {} {}; do echo \"@@@$f\"; cat \"$f\" 2>/dev/null; done; true",
                [KGSL_BUSY_PCT, KGSL_CLK, KGSL_MAX, KGSL_BUSY].join(" "),
                DEVFREQ_GLOBS
            ),
        ],
    )
}

/// su 子进程取正文：stdin null / stdout 管道 / stderr null，`try_wait` 轮询至
/// `bound` 越界 kill+wait；正文几 KB、退出后一次读到 EOF 即可（devsrc 同构）。
fn su_fetch(prog: &str, args: &[String], bound: Duration) -> Result<String, SuFail> {
    let mut child = match Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(SuFail::NoSu),
        Err(e) => return Err(SuFail::Failed(format!("进程拉起失败: {e}"))),
    };
    let mut pipe = child.stdout.take(); // piped() ⇒ Some
    let deadline = Instant::now() + bound;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Err(SuFail::Failed(format!("非零退出 {:?}", status.code())));
                }
                let Some(pipe) = pipe.as_mut() else {
                    return Err(SuFail::Failed("stdout 管道句柄缺失（内部）".to_string()));
                };
                let mut buf = Vec::new();
                pipe.read_to_end(&mut buf)
                    .map_err(|e| SuFail::Failed(format!("管道读取失败: {e}")))?;
                return Ok(String::from_utf8_lossy(&buf).into_owned());
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SuFail::Failed(format!("try_wait 失败: {e}")));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SuFail::Timeout);
        }
        std::thread::sleep(POLL_STEP);
    }
}

// ---------------- 纯解析/装配（真机 fixture 单测钉死） ----------------

/// su 抓体 → 路径→正文表：`@@@` 开段，首标记前杂音丢弃，重复标记后来者覆盖。
fn parse_proc_grab(text: &str) -> ParsedFiles {
    let mut out = ParsedFiles::new();
    let mut key: Option<String> = None;
    let mut body = String::new();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("@@@") {
            if let Some(k) = key.take() {
                out.insert(k, body.clone());
            }
            key = Some(path.to_string());
            body.clear();
        } else if key.is_some() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(line);
        }
    }
    if let Some(k) = key {
        out.insert(k, body);
    }
    out
}

/// 非空条目取用（空正文按无数据消化）
fn get_str<'a>(files: &'a ParsedFiles, path: &str) -> Option<&'a str> {
    files
        .get(path)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
}

/// 首个非空 devfreq 叶子文件（cur_freq/max_freq 兜底频率用）
fn devfreq_get<'a>(files: &'a ParsedFiles, leaf: &str) -> Option<&'a str> {
    let suffix = format!("/{leaf}");
    files
        .iter()
        .find(|(k, v)| k.contains("/devfreq/") && k.ends_with(&suffix) && !v.trim().is_empty())
        .map(|(_, v)| v.trim())
}

/// 首个 devfreq load 路径（差分备选；空文件在此即缺席）
fn devfreq_load_path(files: &ParsedFiles) -> Option<String> {
    files
        .keys()
        .find(|k| k.contains("/devfreq/") && k.ends_with("/load"))
        .cloned()
}

/// `"8 %"` → 8.0（钳 0..=100；NaN/inf/杂音 → None）
fn parse_util(text: &str) -> Option<f64> {
    let v: f64 = text.replace('%', "").trim().parse().ok()?;
    v.is_finite().then(|| v.clamp(0.0, 100.0))
}

/// `"441"` → 441.0（clock_mhz 形态，取首词）
fn parse_mhz(text: &str) -> Option<f64> {
    text.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// `"587000000"` → 587.0（sysfs Hz 整型 → MHz）
fn parse_hz_mhz(text: &str) -> Option<f64> {
    text.split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .filter(|h| *h > 0)
        .map(|h| h as f64 / 1e6)
}

/// `"171469 1012085"` → (busy, total) 累计对
fn parse_pair(text: &str) -> Option<(u64, u64)> {
    let mut it = text.split_whitespace();
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

/// 累计对差分：同路径前值存在且窗口前进 → Δbusy/Δtotal×100 钳 0..=100；
/// 首读/换源/计数器倒退一律“无使用率”（不是错）。
fn busy_delta(prev: Option<&PrevBusy>, path: &str, cur: (u64, u64)) -> Option<f64> {
    let (p, pb, pt) = match prev {
        Some((p, (pb, pt))) => (p.as_str(), *pb, *pt),
        None => return None,
    };
    if p != path || cur.1 <= pt || cur.0 < pb {
        return None;
    }
    Some(((cur.0 - pb) as f64 / (cur.1 - pt) as f64 * 100.0).clamp(0.0, 100.0))
}

/// 频率锚（单一事实源，编排与装配共用）：kgsl clock_mhz 优先，devfreq cur_freq 兜底
fn freq_anchor(files: &ParsedFiles) -> Option<(f64, &'static str)> {
    if let Some(m) = get_str(files, KGSL_CLK).and_then(parse_mhz) {
        return Some((m, "kgsl"));
    }
    devfreq_get(files, "cur_freq")
        .and_then(parse_hz_mhz)
        .map(|m| (m, "devfreq"))
}

/// 抓体表 + 差分前值 → 快照 + 新前值（纯函数，双次连调验 Δ 数学）
fn assemble(files: &ParsedFiles, prev: Option<PrevBusy>) -> (Gpu, Option<PrevBusy>) {
    let (freq_mhz, source) = match freq_anchor(files) {
        Some((m, s)) => (Some(m), Some(s.to_string())),
        None => (None, None),
    };
    let max_mhz = get_str(files, KGSL_MAX)
        .and_then(parse_hz_mhz)
        .or_else(|| devfreq_get(files, "max_freq").and_then(parse_hz_mhz));
    let mut next_prev = None;
    let mut load = get_str(files, KGSL_BUSY_PCT).and_then(parse_util);
    if let Some(pair) = get_str(files, KGSL_BUSY).and_then(parse_pair) {
        load = load.or(busy_delta(prev.as_ref(), KGSL_BUSY, pair));
        next_prev = Some((KGSL_BUSY.to_string(), pair));
    } else if load.is_none() {
        if let Some(path) = devfreq_load_path(files) {
            if let Some(pair) = get_str(files, &path).and_then(parse_pair) {
                load = busy_delta(prev.as_ref(), &path, pair);
                next_prev = Some((path, pair));
            }
        }
    }
    match freq_mhz {
        Some(f) => (
            Gpu {
                available: true,
                load_pct: load,
                freq_mhz: Some(f),
                max_mhz,
                source,
                reason: None,
            },
            next_prev,
        ),
        // 频率锚缺失：整卡缺席（前端隐藏），差分前值仍延续
        None => (
            Gpu::absent("sysfs 未见 GPU 频率源（kgsl/devfreq 俱缺）".to_string()),
            next_prev,
        ),
    }
}

/// gpubusy/load 差分前值（跨拍状态；毒锁回收即内部态，无丢数据风险）
fn prev_busy() -> &'static Mutex<Option<PrevBusy>> {
    static S: Mutex<Option<PrevBusy>> = Mutex::new(None);
    &S
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::*;

    fn grab(pairs: &[(&str, &str)]) -> ParsedFiles {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// 真机 fixture（Magisk  rig·Adreno 650，su 抓体逐字节形态）
    const RIG_KGSL: &str = "@@@/sys/class/kgsl/kgsl-3d0/gpu_busy_percentage\n8 %\n@@@/sys/class/kgsl/kgsl-3d0/clock_mhz\n441\n@@@/sys/class/kgsl/kgsl-3d0/max_gpuclk\n587000000\n@@@/sys/class/kgsl/kgsl-3d0/gpubusy\n171469 1012085\n";

    #[test]
    fn parse_grab_maps_markers_to_sections() {
        let f = parse_proc_grab(RIG_KGSL);
        assert_eq!(f.len(), 4);
        assert_eq!(f[KGSL_BUSY_PCT], "8 %");
        assert_eq!(f[KGSL_CLK], "441");
        assert_eq!(f[KGSL_MAX], "587000000");
        assert_eq!(f[KGSL_BUSY], "171469 1012085");
    }

    #[test]
    fn parse_grab_drops_preamble_keeps_empty_sections() {
        let f = parse_proc_grab("Warning: 杂音\n@@@/a\n\n@@@/b\nx y z\n");
        assert_eq!(f.len(), 2);
        assert_eq!(f["/a"], "", "空段保留为缺席");
        assert_eq!(f["/b"], "x y z");
        assert!(parse_proc_grab("").is_empty());
        assert!(parse_proc_grab("纯杂音无标记").is_empty());
    }

    #[test]
    fn parses_util_clock_hz_pair_and_junk() {
        assert_eq!(parse_util("8 %"), Some(8.0));
        assert_eq!(parse_util(" 140 % "), Some(100.0), "越界钳 100");
        assert_eq!(parse_util("nan"), None);
        assert_eq!(parse_util("not-a-%"), None);
        assert_eq!(parse_mhz("441"), Some(441.0));
        assert_eq!(parse_mhz("441\njunk"), Some(441.0));
        assert_eq!(parse_mhz("--"), None);
        assert_eq!(parse_hz_mhz("587000000"), Some(587.0));
        assert_eq!(parse_hz_mhz("-1"), None);
        assert_eq!(parse_pair("171469 1012085"), Some((171469, 1012085)));
        assert_eq!(parse_pair("171469"), None);
    }

    #[test]
    fn kgsl_fixture_assembles_full_reading() {
        let f = parse_proc_grab(RIG_KGSL);
        let (g, prev) = assemble(&f, None);
        assert!(g.available, "kgsl 全件必可用");
        assert_eq!(g.load_pct, Some(8.0), "busy% 优先于差分");
        assert_eq!(g.freq_mhz, Some(441.0));
        assert_eq!(g.max_mhz, Some(587.0));
        assert_eq!(g.source.as_deref(), Some("kgsl"));
        assert!(g.reason.is_none());
        assert_eq!(
            prev.as_ref().map(|p| (p.0.as_str(), p.1)),
            Some((KGSL_BUSY, (171469u64, 1012085u64))),
            "gpubusy 对须续存供下拍差分"
        );
    }

    #[test]
    fn gpubusy_delta_first_read_silent_second_read_math() {
        let f1 = grab(&[(KGSL_CLK, "441"), (KGSL_BUSY, "100 1000")]);
        let (g1, prev) = assemble(&f1, None);
        assert!(g1.available && g1.load_pct.is_none(), "首读无差分窗口: {g1:?}");
        let f2 = grab(&[(KGSL_CLK, "441"), (KGSL_BUSY, "250 2000")]);
        let (g2, _) = assemble(&f2, prev);
        assert_eq!(g2.load_pct, Some(15.0), "Δbusy/Δtotal=150/1000");
        let f3 = grab(&[(KGSL_CLK, "441"), (KGSL_BUSY, "50 3000")]);
        let (g3, _) = assemble(&f3, Some((KGSL_BUSY.to_string(), (250, 2000))));
        assert_eq!(g3.load_pct, None, "计数器倒退按无使用率，非 0");
    }

    #[test]
    fn devfreq_fallback_empty_load_is_not_error() {
        let f = parse_proc_grab("@@@/sys/class/devfreq/3d00000.qcom,kgsl-3d0/cur_freq\n441000000\n@@@/sys/class/devfreq/3d00000.qcom,kgsl-3d0/max_freq\n587000000\n@@@/sys/class/devfreq/3d00000.qcom,kgsl-3d0/load\n\n");
        let (g, prev) = assemble(&f, None);
        assert!(g.available && g.freq_mhz == Some(441.0) && g.max_mhz == Some(587.0));
        assert_eq!(g.source.as_deref(), Some("devfreq"));
        assert_eq!(g.load_pct, None, "load 空文件 = 无使用率，不报错");
        assert!(prev.is_none());
    }

    #[test]
    fn empty_grab_is_clean_absence() {
        let (g, _) = assemble(&ParsedFiles::new(), None);
        assert!(!g.available && g.reason.is_some() && g.freq_mhz.is_none() && g.source.is_none());
    }

    /// 封存域回归（rig 实测形状钉死）：直读按文件贴标只拿到 busy%（"0 %"），
    /// clock/max EACCES——旧逻辑“任一直读成功即整腿成立”在此丢锚、永不走 su。
    /// 现判据：锚缺 + 锚类拒档 → su 腿抓体与直读表合并 → 完整装配。
    #[test]
    fn sealed_domain_partial_direct_merges_su_grab_to_full_reading() {
        const RIG_GRAB: &str = "@@@/sys/class/kgsl/kgsl-3d0/gpu_busy_percentage\n0 %\n@@@/sys/class/kgsl/kgsl-3d0/clock_mhz\n441\n@@@/sys/class/kgsl/kgsl-3d0/max_gpuclk\n587000000\n@@@/sys/class/kgsl/kgsl-3d0/gpubusy\n0 0\n@@@/sys/class/devfreq/3d00000.qcom,kgsl-3d0/cur_freq\n441600000\n";
        let direct = grab(&[(KGSL_BUSY_PCT, "0 %")]);
        assert!(freq_anchor(&direct).is_none(), "直读表无锚才够格下 su 腿");
        let merged = merge_files(direct, parse_proc_grab(RIG_GRAB));
        let (g, prev) = assemble(&merged, None);
        assert!(g.available, "合并表含 clock，必可用: {g:?}");
        assert_eq!(g.freq_mhz, Some(441.0));
        assert_eq!(g.source.as_deref(), Some("kgsl"), "kgsl clock 优先于 devfreq");
        assert_eq!(g.max_mhz, Some(587.0));
        assert_eq!(g.load_pct, Some(0.0));
        assert!(prev.is_some(), "gpubusy 对须入差分记账");
    }

    #[test]
    fn merge_keeps_direct_entries_over_empty_su_sections() {
        let direct = grab(&[(KGSL_CLK, "441"), (KGSL_BUSY, "100 200")]);
        let merged = merge_files(
            direct,
            parse_proc_grab("@@@/sys/class/kgsl/kgsl-3d0/gpubusy\n\n@@@/sys/class/devfreq/x.kgsl/load\n"),
        );
        assert_eq!(merged[KGSL_BUSY], "100 200", "su 空段不得抹掉直读实值");
        assert_eq!(merged[KGSL_CLK], "441");
    }

    #[test]
    fn su_plan_pins_grab_command_shape() {
        let (prog, args) = su_plan();
        assert_eq!(prog, "su");
        assert_eq!(args[0], "-c");
        let cmd = &args[1];
        assert!(cmd.starts_with("for f in "), "单条 for 计划: {cmd}");
        assert!(cmd.contains(KGSL_CLK) && cmd.contains(KGSL_BUSY));
        assert!(cmd.contains("/sys/class/devfreq/*gpu*/cur_freq"));
        assert!(cmd.contains("/sys/class/devfreq/*kgsl*/load"));
        assert!(cmd.contains("cat \"$f\" 2>/dev/null"), "cat 失败须消音: {cmd}");
        assert!(cmd.ends_with("done; true"), "尾部 true: {cmd}");
        // `; true` 钉死：循环退出码=末条 cat，rig 上 busmon 无 load 节点即假性非零——
        // glob-miss（已授权）洗成 0，正文完备性由解析判；拒授权时内层 shell 不执行、
        // `; true` 不跑，su 非零退出保留 → SuFailed 终态不受洗白波及。
    }

    #[test]
    fn su_fetch_missing_binary_maps_to_no_su() {
        assert!(matches!(
            su_fetch("atlas-no-such-su-zzz", &[], SU_BOUND),
            Err(SuFail::NoSu)
        ));
    }

    #[test]
    fn su_fetch_passes_stdout_and_kills_overrun() {
        let got = su_fetch(
            "sh",
            &["-c".into(), "printf '@@@/x\\n1\\n'".into()],
            SU_BOUND,
        )
        .expect("子进程 stdout 原样取回");
        assert_eq!(got, "@@@/x\n1\n");
        let t0 = Instant::now();
        assert!(matches!(
            su_fetch("sh", &["-c".into(), "sleep 5".into()], Duration::from_millis(300)),
            Err(SuFail::Timeout)
        ));
        assert!(t0.elapsed() < Duration::from_secs(2), "kill 须生效: {:?}", t0.elapsed());
    }

    /// 开发机契约：单次 get() 必回干净判定——available ⇔ freq+source、!available ⇔
    /// reason，永不 panic。本开发机实观：kgsl 直读即中（域未封存）→ available:true；
    /// 规格假设的无 kgsl 开发机走 su ENOENT/桩件拒 → available:false + reason，同样过。
    #[test]
    fn get_returns_clean_verdict_on_dev_box() {
        let g = Gpu::get();
        println!("dev-box GPU: {g:?}");
        assert_eq!(g.available, g.freq_mhz.is_some(), "频率锚 ⇔ available: {g:?}");
        assert_eq!(g.available, g.source.is_some(), "available 必带源: {g:?}");
        assert_eq!(g.available, g.reason.is_none(), "缺席必带成因: {g:?}");
    }

    /// 真机契约：仅当 su 确实不可达（reason 含「无 su」）才允许缺席 skip-pass；
    /// rooted rig 必须经 su 桥、permissive 域必须经直读给出频率——「有 kgsl 节点却报
    /// 无频率源」即桥失效，属 FAILURE（旧版 skip-pass 曾掩盖封存域丢锚 bug）。
    /// `cargo test v1::system::gpu -- --ignored`
    #[test]
    #[ignore]
    fn live_gpu_reads() {
        let g = Gpu::get();
        println!(
            "live GPU: available={} load={:?}% freq={:?}MHz max={:?}MHz src={:?} reason={:?}",
            g.available, g.load_pct, g.freq_mhz, g.max_mhz, g.source, g.reason
        );
        if g.available {
            assert!(g.freq_mhz.is_some_and(|f| f > 0.0), "available 必带正频率: {g:?}");
        } else if g.reason.as_deref().is_some_and(|r| r.contains("无 su")) {
            println!("无 root（su 不可达）：缺席即正确行为（{}）", g.reason.unwrap());
        } else {
            panic!("设备有 GPU 节点却未给出频率——桥/直读链路失效，实为 {g:?}");
        }
    }
}
