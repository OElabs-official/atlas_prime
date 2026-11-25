//! v1/service —— runit 服务内置安装/卸载（被动模块：仅被 CLI 一次性调用与守护模式
//! 状态提示，零常驻状态；取代原 resources/sv-install.sh）。
//!
//! --sv-install 流程：复制当前二进制到 $PREFIX/bin/atlas_prime（0755，就地重装则
//! 跳过）→ 以内嵌资产（include_str!，源 _resources/sv/atlas_prime/，与 api 内嵌
//! dashboard 同法）落盘 $SVDIR/atlas_prime 的 run 与 log/run（0700）→ 预建 svlogd
//! 日志目录（svlogd 不会自建目录，缺目录时 log/run 直接退出——设备 E2E 实证）→
//! 全程不写 down 占位，调用 sv-enable 直接上线。
//! SVDIR 解析：显式非空 env SVDIR 优先；未设/为空则兜底 $PREFIX/var/service 并打印所用根。
//! --sv-restart：前置判定（未安装→Err）→ 清 down 占位 → sv-restart/`sv restart` 双退路。

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Termux 固定安装前缀（$PREFIX）；sv-enable/sv-disable 兜底绝对路径同源。
const PREFIX: &str = "/data/data/com.termux/files/usr";
/// runit 服务名 = $SVDIR 下目录名 = 安装后的二进制名。
const SV_NAME: &str = "atlas_prime";

/// 内嵌主服务脚本（源 _resources/sv/atlas_prime/run，落盘 0700）。
pub const RUN_SCRIPT: &str = include_str!("../../_resources/sv/atlas_prime/run");
/// 内嵌日志服务脚本（源 _resources/sv/atlas_prime/log/run，落盘 0700）。
pub const LOG_RUN_SCRIPT: &str = include_str!("../../_resources/sv/atlas_prime/log/run");

/// 安装布局条目：目录（body=None）或内嵌脚本（body=Some），附权限模式。
pub struct Entry {
    pub path: PathBuf,
    pub mode: u32,
    pub body: Option<&'static str>,
}

/// SVDIR 解析（纯函数）：显式非空 env → 该值；未设/为空 → Termux 兜底根。
pub fn resolve_svroot(svdir_env: Option<&OsStr>) -> PathBuf {
    match svdir_env {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(PREFIX).join("var/service"),
    }
}

/// 日志根解析（纯函数）：显式非空 LOGDIR → 该值；未设/为空 → $PREFIX/var/log 兜底。
pub fn resolve_log_root(logdir_env: Option<&OsStr>) -> PathBuf {
    match logdir_env {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(PREFIX).join("var/log"),
    }
}

/// 读进程环境的 SVDIR（安装/卸载/提示三处同源）。
pub fn svroot() -> PathBuf {
    resolve_svroot(env::var_os("SVDIR").as_deref())
}

/// 安装布局（纯函数，零文件系统副作用）：5 条目，目录先于其内脚本，全程无 down 占位。
pub fn plan(svroot: &Path, log_root: &Path) -> Vec<Entry> {
    let svc = svroot.join(SV_NAME);
    vec![
        Entry { path: svc.clone(), mode: 0o700, body: None },
        Entry { path: svc.join("log"), mode: 0o700, body: None },
        Entry { path: svc.join("run"), mode: 0o700, body: Some(RUN_SCRIPT) },
        Entry { path: svc.join("log/run"), mode: 0o700, body: Some(LOG_RUN_SCRIPT) },
        // svlogd 不会自建目录：${LOGDIR:-$PREFIX/var/log}/sv/atlas_prime 必须预建
        Entry { path: log_root.join("sv").join(SV_NAME), mode: 0o700, body: None },
    ]
}

/// 安装后的二进制绝对路径（字面 $PREFIX/bin/atlas_prime）。
fn bin_dest() -> PathBuf {
    PathBuf::from(PREFIX).join("bin").join(SV_NAME)
}

/// 打印 SVDIR/日志根来源（“env 显式”还是“兜底默认”）。
fn root_label(v: &Option<OsString>) -> &'static str {
    if v.as_deref().is_some_and(|s| !s.is_empty()) { "env 显式" } else { "兜底默认" }
}

/// 在 PATH 中查找 helper（sv-enable/sv-disable），兜底 $PREFIX/bin 绝对路径。
fn find_helper(name: &str) -> Option<PathBuf> {
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let cand = dir.join(name);
            if is_exec(&cand) {
                return Some(cand);
            }
        }
    }
    let fb = PathBuf::from(PREFIX).join("bin").join(name);
    is_exec(&fb).then_some(fb)
}

fn is_exec(p: &Path) -> bool {
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// runsvdir 是否在跑：扫 /proc/<pid>/comm（同 uid 可见）。/proc 不可读返回 None（未知）。
fn runsvdir_alive() -> Option<bool> {
    let rd = fs::read_dir("/proc").ok()?;
    for ent in rd.flatten() {
        if !ent.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(comm) = fs::read_to_string(ent.path().join("comm"))
            && comm.trim() == "runsvdir"
        {
            return Some(true);
        }
    }
    Some(false)
}

/// 原子替换式安装二进制：同目录临时文件 → 0755 → rename。
/// rename 换 inode，不触 ETXTBSY（正在运行的旧二进制不受影响）。
fn install_binary(src: &Path, dest: &Path) -> Result<(), String> {
    let tmp = dest.with_extension("new");
    let r = (|| -> std::io::Result<()> {
        fs::copy(src, &tmp)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        fs::rename(&tmp, dest)
    })();
    r.map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!(
            "安装二进制到 {} 失败: {e}（需 $PREFIX/bin 可写；旧的手动实例请先退役再重试）",
            dest.display()
        )
    })
}

/// runsvdir 是否已注册该服务：supervise/ 控制目录就位即已注册（纯判定，无时序）。
fn supervise_ready(svc_dir: &Path) -> bool {
    svc_dir.join("supervise").is_dir()
}

/// 有界等待 runsvdir 注册（100ms 步长、上限 6000ms；runsvdir 对新建服务目录的
/// 扫描周期 ≤5s——一次性 CLI，阻塞可接受，超时即走降级路径）。
fn wait_supervise_registered(svc_dir: &Path) -> bool {
    let start = std::time::Instant::now();
    loop {
        if supervise_ready(svc_dir) {
            return true;
        }
        if start.elapsed() >= std::time::Duration::from_millis(6000) {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// enable：先等 runsvdir 注册服务目录（全新安装目录刚建时 sv-enable 内的 sv 找不到
/// 控制目录必败 rc=1——真机 SEG-1 实证），注册后走 sv-enable（PATH → $PREFIX/bin 兜底，
/// 子进程环境注入解析后的 SVDIR——sv-* 助手自身依赖 $SVDIR，缺了会踩空，env 已显式
/// 导出时值相同、行为不变）。失败/缺失/超时不判死——目录内无 down 占位，runsvdir 若在跑
/// ≤5s 自动拉起；打印监管状态与手动 enable 路径（SVDIR 导出提示保留，供手动路径用）。
fn enable(svroot: &Path) {
    let svc = svroot.join(SV_NAME);
    let manual = if !wait_supervise_registered(&svc) {
        "runsvdir 6000ms 内未注册服务目录（supervise/ 未出现；runsvdir 未在跑？）".to_string()
    } else {
        match find_helper("sv-enable") {
            Some(bin) => match Command::new(&bin).env("SVDIR", svroot).arg(SV_NAME).status() {
                Ok(st) if st.success() => {
                    eprintln!("[atlas] sv-enable {SV_NAME} 完成");
                    return;
                }
                Ok(st) => format!("sv-enable 退出码 {:?}", st.code()),
                Err(e) => format!("sv-enable 拉起失败: {e}"),
            },
            None => "未找到 sv-enable（termux-services 未安装？）".to_string(),
        }
    };
    eprintln!("[atlas] WARN {manual}，降级为手动 enable");
    match runsvdir_alive() {
        Some(true) => {
            eprintln!("[atlas] runsvdir 在跑且无 down 占位：≤5s 内自动拉起，无需再手动 enable");
        }
        Some(false) => {
            eprintln!("[atlas] runsvdir 未在跑：source $PREFIX/etc/profile.d/start-services.sh（或经 Termux:Boot）拉起监管后服务才会上线");
        }
        None => eprintln!("[atlas] runsvdir 存活状态不可探测（/proc 不可读）"),
    }
    eprintln!("[atlas] 先导出 SVDIR（sv-* 助手依赖）：export SVDIR={}", svroot.display());
    eprintln!("[atlas] 手动 enable：sv-enable {SV_NAME}（服务目录 {}）", svc.display());
}

/// 安装/刷新 runit 服务（幂等：资产覆盖、二进制刷新——同路径就地重装则跳过；
/// 确保 enable，历史 down 占位一律移除，新流程从不写 down）。
/// Err = 可读消息，由 main 以 [atlas] 前缀打印并退非零。
pub fn install() -> Result<(), String> {
    let svdir = env::var_os("SVDIR");
    let logdir = env::var_os("LOGDIR");
    let svroot = resolve_svroot(svdir.as_deref());
    let log_root = resolve_log_root(logdir.as_deref());
    eprintln!("[atlas] SVDIR = {}（{}）", svroot.display(), root_label(&svdir));
    eprintln!("[atlas] 日志根 = {}（{}）", log_root.display(), root_label(&logdir));

    // a. 二进制：current_exe → $PREFIX/bin/atlas_prime（0755）；canonical 同路径 = 就地重装，跳过。
    let exe = env::current_exe().map_err(|e| format!("current_exe 失败: {e}"))?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let dest = bin_dest();
    if dest.canonicalize().ok().as_deref() == Some(exe.as_path()) {
        eprintln!("[atlas] 就地重装：当前二进制即 {}，跳过复制", dest.display());
    } else {
        install_binary(&exe, &dest)?;
        eprintln!("[atlas] 已安装二进制 {}（0755）", dest.display());
    }

    // b/c. 服务资产 + svlogd 日志目录：按纯 plan 逐条落盘并 chmod（0700）。
    for entry in plan(&svroot, &log_root) {
        match entry.body {
            None => fs::create_dir_all(&entry.path),
            Some(body) => fs::write(&entry.path, body),
        }
        .and_then(|()| fs::set_permissions(&entry.path, fs::Permissions::from_mode(entry.mode)))
        .map_err(|e| {
            format!(
                "写入 {} 失败: {e}——若 termux-services 未安装：pkg install termux-services 后开新 Termux 会话",
                entry.path.display()
            )
        })?;
    }

    // d. 无 down 占位：历史 down（旧安装器/手动 sv-disable 所留）移除，重装即重新上线。
    let down = svroot.join(SV_NAME).join("down");
    if let Err(e) = fs::remove_file(&down)
        && e.kind() != ErrorKind::NotFound
    {
        eprintln!("[atlas] 移除历史 down 占位失败（忽略）: {e}");
    }

    // e. enable + f. 下一步块
    enable(&svroot);
    eprintln!("[atlas] 下一步：");
    eprintln!("[atlas]   1) 已 enable 的服务由 runsvdir ≤5s 拉起；查看状态：sv status {SV_NAME}");
    eprintln!("[atlas]   2) Termux:Boot 自启：先手动跑一次 termux-wake-lock，再把");
    eprintln!("[atlas]      'source $PREFIX/etc/profile.d/start-services.sh'");
    eprintln!("[atlas]      写入 ~/.termux/boot/ 下的脚本（如 ~/.termux/boot/start-services.sh）");
    eprintln!("[atlas]   note: 请退役旧的手动拉起方式——flock 单例会把双启动记为一次 crash。");
    Ok(())
}

/// restart 前置判定（纯函数）：服务目录不存在 = 未安装 → 可读 Err（指路 --sv-install）；
/// 存在 → Ok(())，调用方继续走 helper 链。
fn restart_guard(svc: &Path) -> Result<(), String> {
    if svc.is_dir() {
        Ok(())
    } else {
        Err(format!(
            "未安装（{} 不存在）——先运行 'atlas_prime --sv-install'",
            svc.display()
        ))
    }
}

/// 重启 runit 服务并确保 up 态（一次性 CLI，服务本身由 runsvdir 监管、与本进程无关）：
/// 未安装 → Err exit 1；残留 down 占位先移除（restart 语义 = 要它在跑）；helper 优先
/// sv-restart、退 `sv restart`，子进程环境注入解析后的 SVDIR（与 enable/disable 侧同源）；
/// 两者皆不可用/失败 → 打印监管状态与手动路径后 Err（restart 未发生，不假装成功）。
pub fn restart() -> Result<(), String> {
    let svdir = env::var_os("SVDIR");
    let svroot = resolve_svroot(svdir.as_deref());
    eprintln!("[atlas] SVDIR = {}（{}）", svroot.display(), root_label(&svdir));
    let svc = svroot.join(SV_NAME);
    restart_guard(&svc)?;

    if fs::remove_file(svc.join("down")).is_ok() {
        eprintln!("[atlas] 已移除 down 占位（restart 语义 = 确保 up）");
    }

    let mut reasons = Vec::new();
    if let Some(bin) = find_helper("sv-restart") {
        match Command::new(&bin).env("SVDIR", &svroot).arg(SV_NAME).status() {
            Ok(st) if st.success() => {
                eprintln!("[atlas] sv-restart {SV_NAME} 完成：runsvdir 已 TERM→重拉，≤5s 后 'sv status {SV_NAME}' 应见 run");
                return Ok(());
            }
            Ok(st) => reasons.push(format!("sv-restart 退出码 {:?}", st.code())),
            Err(e) => reasons.push(format!("sv-restart 拉起失败: {e}")),
        }
    }
    if let Some(bin) = find_helper("sv") {
        match Command::new(&bin).env("SVDIR", &svroot).args(["restart", SV_NAME]).status() {
            Ok(st) if st.success() => {
                eprintln!("[atlas] sv restart {SV_NAME} 完成：runsvdir 已 TERM→重拉");
                return Ok(());
            }
            Ok(st) => reasons.push(format!("sv restart 退出码 {:?}", st.code())),
            Err(e) => reasons.push(format!("sv 拉起失败: {e}")),
        }
    }
    if reasons.is_empty() {
        reasons.push("未找到 sv-restart / sv（termux-services 未安装？）".to_string());
    }
    for r in &reasons {
        eprintln!("[atlas] WARN {r}");
    }
    match runsvdir_alive() {
        Some(true) => eprintln!("[atlas] runsvdir 在跑：手动路径 export SVDIR={} && sv restart {SV_NAME}", svroot.display()),
        Some(false) => eprintln!("[atlas] runsvdir 未在跑：restart 无处安放——先 source $PREFIX/etc/profile.d/start-services.sh 拉起监管"),
        None => eprintln!("[atlas] runsvdir 存活状态不可探测（/proc 不可读）"),
    }
    Err(format!("restart 未发生：{}", reasons.join("; ")))
}

/// 卸载 runit 服务：best-effort sv-disable（失败忽略）→ 删除服务目录。
/// 未安装同样成功（exit 0）；$PREFIX/bin 二进制与日志历史有意保留。
pub fn uninstall() -> Result<(), String> {
    let svdir = env::var_os("SVDIR");
    let svroot = resolve_svroot(svdir.as_deref());
    eprintln!("[atlas] SVDIR = {}（{}）", svroot.display(), root_label(&svdir));

    if let Some(bin) = find_helper("sv-disable") {
        // 注入解析后的 SVDIR：sv-* 助手自身依赖 $SVDIR（与 enable 侧同源同理）
        match Command::new(&bin).env("SVDIR", &svroot).arg(SV_NAME).status() {
            Ok(st) if st.success() => eprintln!("[atlas] sv-disable {SV_NAME} 完成"),
            Ok(st) => eprintln!("[atlas] sv-disable 退出码 {:?}（忽略，继续删目录）", st.code()),
            Err(e) => eprintln!("[atlas] sv-disable 拉起失败（忽略）: {e}"),
        }
    }

    let svc = svroot.join(SV_NAME);
    match fs::remove_dir_all(&svc) {
        Ok(()) => eprintln!("[atlas] 已卸载服务目录 {}", svc.display()),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            eprintln!("[atlas] 未安装（{} 不存在），无需卸载", svc.display());
        }
        Err(e) => return Err(format!("删除 {} 失败: {e}", svc.display())),
    }
    let log_root = resolve_log_root(env::var_os("LOGDIR").as_deref());
    eprintln!(
        "[atlas] 有意保留：{} 与日志历史 {}",
        bin_dest().display(),
        log_root.join("sv").join(SV_NAME).display()
    );
    Ok(())
}

/// 守护模式 runit 状态提示（flock 拿到后调用）：单 stat，任何 IO 状况静默降级，
/// 绝不打断启动。未安装 → 建议 --sv-install；down 占位在档 → 建议 --sv-install 重新上线。
pub fn status_hint() {
    let svc = svroot().join(SV_NAME);
    if !svc.is_dir() {
        eprintln!("[atlas] 当前未由 runit 监管——运行 'atlas_prime --sv-install' 安装为 runit 服务");
    } else if svc.join("down").exists() {
        eprintln!(
            "[atlas] {} 带 down 占位（服务停用中）——运行 'atlas_prime --sv-restart'（或 --sv-install）重新上线",
            svc.display()
        );
    }
}

// ---------------- APG 服务状态读（GET /system/apg 的数据源） ----------------

/// APG（atlas_package_group）家族成员 `(runit 目录名, 是否活体探测, CamelCase 显示名)`。
/// `live=true` 者读 SVDIR 下 supervise/stat；`live=false` 者尚未发布 = 恒
/// "coming_soon" 占位（前端灰显）。显示名（label）即前端渲染名，`pkg` 仍为目录名。
/// ExpertRAWPipeline 目录名暂用 `expert_raw_pipeline`，发布时按实际服务名改此一行即可。
pub const APG_PKGS: [(&str, bool, &str); 5] = [
    ("atlas_network", true, "Network"),
    ("atlas_ex2", true, "Explorer2"),
    ("agentflow", false, "AgentFLow"),
    ("medianode", false, "MediaNode"),
    ("expert_raw_pipeline", false, "ExpertRAWPipeline"),
];

/// 单包 runit 状态（`GET /system/apg` 响应形状；序列化即对外契约）。
#[derive(serde::Serialize)]
pub struct ApgState {
    pub pkg: &'static str,
    /// CamelCase 显示名（前端渲染用，与 pkg 目录名解耦）
    pub label: &'static str,
    /// run | down | alert | uninstalled | coming_soon
    pub state: &'static str,
    /// 仅 run 态携带（supervise/pid 文本解析）；其余为 None
    pub pid: Option<u32>,
}

/// 读一个服务目录的 supervise/stat 文本（runit 语义：run/down/finish/alert；
/// finish = 一次性服务已跑完 = 视同 down）。目录/文件缺失 = uninstalled。
/// 纯读、零 fork、零常驻。
fn sv_state(pkg_dir: &std::path::Path) -> (&'static str, Option<u32>) {
    let Ok(text) = std::fs::read_to_string(pkg_dir.join("supervise").join("stat")) else {
        return ("uninstalled", None);
    };
    let state = match text.trim() {
        "run" => "run",
        "alert" => "alert",
        _ => "down", // down / finish / 其他脏值一律归 down
    };
    let pid = if state == "run" {
        std::fs::read_to_string(pkg_dir.join("supervise").join("pid"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    } else {
        None
    };
    (state, pid)
}

/// APG 快照：SVDIR 下逐包读 supervise/stat，未发布成员恒 coming_soon。
pub fn apg_snapshot() -> Vec<ApgState> {
    let root = svroot();
    APG_PKGS
        .iter()
        .map(|(pkg, live, label)| {
            if !*live {
                return ApgState { pkg, label, state: "coming_soon", pid: None };
            }
            let (state, pid) = sv_state(&root.join(pkg));
            ApgState { pkg, label, state, pid }
        })
        .collect()
}

/// 纯函数版（可测）：给定 svroot 快照 APG 活体成员（live=true）状态。
fn apg_live_probe(root: &std::path::Path) -> Vec<ApgState> {
    APG_PKGS
        .iter()
        .filter(|(_, live, _)| *live)
        .map(|(pkg, _, label)| {
            let (state, pid) = sv_state(&root.join(pkg));
            ApgState { pkg, label, state, pid }
        })
        .collect()
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 内嵌主服务脚本：非空且携带 exec 行（监督契约：前台 exec + 2>&1 并流）。
    #[test]
    fn run_asset_carries_exec_line() {
        assert!(!RUN_SCRIPT.trim().is_empty());
        assert!(RUN_SCRIPT.contains("exec /data/data/com.termux/files/usr/bin/atlas_prime 2>&1"));
    }

    /// 内嵌日志服务脚本：非空且走 svlogd -tt 轮转（脚本里 svlogd 经 $SVLOGD
    /// 间接调用，故逐 token 断言 + 轮转目录字面量）。
    #[test]
    fn log_run_asset_carries_svlogd_line() {
        assert!(!LOG_RUN_SCRIPT.trim().is_empty());
        assert!(LOG_RUN_SCRIPT.contains("svlogd"), "必须走 svlogd 轮转");
        assert!(LOG_RUN_SCRIPT.contains("-tt"), "svlogd 必须带 -tt（前台+时间戳）");
        assert!(LOG_RUN_SCRIPT.contains("$LOG/sv/atlas_prime"), "轮转目录契约 $LOGDIR/sv/atlas_prime");
    }

    /// SVDIR 解析：显式 env 覆盖兜底；空值视同未设。
    #[test]
    fn svroot_prefers_explicit_env_and_falls_back() {
        let fb = PathBuf::from("/data/data/com.termux/files/usr/var/service");
        assert_eq!(resolve_svroot(Some(OsStr::new("/custom/svc"))), PathBuf::from("/custom/svc"));
        assert_eq!(resolve_svroot(Some(OsStr::new(""))), fb, "空 SVDIR 视同未设，走兜底");
        assert_eq!(resolve_svroot(None), fb);
    }

    /// LOGDIR 解析：同 SVDIR 规则。
    #[test]
    fn log_root_prefers_explicit_env_and_falls_back() {
        let fb = PathBuf::from("/data/data/com.termux/files/usr/var/log");
        assert_eq!(resolve_log_root(Some(OsStr::new("/sdcard/log"))), PathBuf::from("/sdcard/log"));
        assert_eq!(resolve_log_root(Some(OsStr::new(""))), fb);
        assert_eq!(resolve_log_root(None), fb);
    }

    /// 纯布局计划：路径/模式/类型逐项锁定；脚本条目排在其父目录条目之后；无 down 条目。
    #[test]
    fn plan_is_down_free_and_mode_locked() {
        let p = plan(Path::new("/sv"), Path::new("/log"));
        let got: Vec<(String, u32, bool)> = p
            .iter()
            .map(|e| (e.path.to_string_lossy().into_owned(), e.mode, e.body.is_some()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("/sv/atlas_prime".to_string(), 0o700, false),
                ("/sv/atlas_prime/log".to_string(), 0o700, false),
                ("/sv/atlas_prime/run".to_string(), 0o700, true),
                ("/sv/atlas_prime/log/run".to_string(), 0o700, true),
                ("/log/sv/atlas_prime".to_string(), 0o700, false),
            ]
        );
        assert!(p.iter().all(|e| e.path.file_name() != Some(OsStr::new("down"))));
        assert_eq!(p[2].path.parent(), Some(p[0].path.as_path()));
        assert_eq!(p[3].path.parent(), Some(p[1].path.as_path()));
    }

    /// supervise_ready 纯判定：服务目录缺失或无 supervise/ 控制目录 = 未注册；
    /// 控制目录就位 = 已注册（等待逻辑的时序部分不在此测）。
    #[test]
    fn supervise_ready_tracks_control_dir() {
        let base = std::env::temp_dir().join(format!("atlas_sv_qa_{}", std::process::id()));
        let svc = base.join(SV_NAME);
        let _ = std::fs::remove_dir_all(&base);
        assert!(!supervise_ready(&svc), "目录缺失应判未注册");
        std::fs::create_dir_all(svc.join("supervise")).expect("临时 supervise/ 可建");
        assert!(supervise_ready(&svc), "supervise/ 就位应判已注册");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// restart 前置判定（纯函数）：服务目录缺失 → 可读 Err 指路 --sv-install；
    /// 就位 → Ok（IO 与 helper 链不在纯判定内，避免测试依赖 SVDIR 进程环境）。
    #[test]
    fn restart_guard_demands_install_when_absent() {
        let base = std::env::temp_dir().join(format!("atlas_sv_restart_qa_{}", std::process::id()));
        let svc = base.join(SV_NAME);
        let _ = std::fs::remove_dir_all(&base);
        let err = restart_guard(&svc).expect_err("目录缺失必须判未安装");
        assert!(err.contains("未安装") && err.contains("--sv-install"), "指路文案: {err}");
        std::fs::create_dir_all(&svc).expect("临时服务目录可建");
        restart_guard(&svc).expect("目录就位应放行");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 进程环境读路径（svroot()）：SVDIR 显式值覆盖兜底。set_var/remove_var 在
    /// edition 2024 为 unsafe；本二进制内此为本测试独占 SVDIR。
    #[test]
    fn svroot_honors_process_env_override() {
        unsafe { env::set_var("SVDIR", "/env/svc") };
        let got = svroot();
        unsafe { env::remove_var("SVDIR") };
        assert_eq!(got, PathBuf::from("/env/svc"));
    }
}

// APG 探测测试（文件级构造假 SVDIR，零设备依赖；temp_dir 惯例同 task 域测试）
#[cfg(test)]
mod apg_tests {
    use super::apg_live_probe;

    /// 造一个带唯一后缀的假 SVDIR（测毕删除），返回其路径。
    fn tmp_svroot(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atlas_apg_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_sv(dir: &std::path::Path, pkg: &str, stat: &str, pid: Option<u32>) {
        std::fs::create_dir_all(dir.join(pkg).join("supervise")).unwrap();
        std::fs::write(dir.join(pkg).join("supervise").join("stat"), stat).unwrap();
        if let Some(p) = pid {
            std::fs::write(dir.join(pkg).join("supervise").join("pid"), p.to_string()).unwrap();
        }
    }

    /// run 态报 run + pid；down/finish 归 down 且无 pid。
    #[test]
    fn apg_probe_maps_runit_states() {
        let dir = tmp_svroot("states");
        fake_sv(&dir, "atlas_network", "run\n", Some(4242));
        fake_sv(&dir, "atlas_ex2", "down", None);
        let got = apg_live_probe(&dir);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].pkg, "atlas_network");
        assert_eq!(got[0].state, "run");
        assert_eq!(got[0].pid, Some(4242));
        assert_eq!(got[1].state, "down");
        assert_eq!(got[1].pid, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未注册包 = uninstalled（本机 atlas_network/atlas_ex2 常态即此路径）。
    #[test]
    fn apg_probe_missing_dir_is_uninstalled() {
        let dir = tmp_svroot("missing");
        let got = apg_live_probe(&dir);
        assert!(got.iter().all(|s| s.state == "uninstalled" && s.pid.is_none()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// run 态但 pid 文件损坏 → state 仍 run，pid=None（不因附属文件坏掉状态）。
    #[test]
    fn apg_probe_tolerates_broken_pidfile() {
        let dir = tmp_svroot("badpid");
        fake_sv(&dir, "atlas_network", "run", None);
        std::fs::write(dir.join("atlas_network").join("supervise").join("pid"), "x").unwrap();
        let got = apg_live_probe(&dir);
        assert_eq!(got[0].state, "run");
        assert_eq!(got[0].pid, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 成员契约：恰 5 位；前 2 位活体（pkg 目录名 = 真实 runit 名），后 3 位 coming_soon；
    /// 显示名一律 CamelCase（前后端契约，快照仅含活体 2 位）。
    #[test]
    fn apg_family_membership() {
        assert_eq!(super::APG_PKGS.len(), 5);
        assert_eq!(super::APG_PKGS[0], ("atlas_network", true, "Network"));
        assert_eq!(super::APG_PKGS[1], ("atlas_ex2", true, "Explorer2"));
        assert!(super::APG_PKGS[..2].iter().all(|(_, live, _)| *live));
        assert!(super::APG_PKGS[2..].iter().all(|(_, live, _)| !*live));
        let labels: Vec<_> = super::APG_PKGS.iter().map(|(_, _, l)| *l).collect();
        assert_eq!(labels, ["Network", "Explorer2", "AgentFLow", "MediaNode", "ExpertRAWPipeline"]);
        let snap = apg_live_probe(std::path::Path::new("/nonexistent-apg-root"));
        assert_eq!(snap.len(), 2, "活体探测只含 live=true 成员");
        assert!(snap.iter().all(|s| s.state == "uninstalled"));
    }

    /// 全量快照：5 行齐全、带 CamelCase label、未发布成员 coming_soon 且无 pid。
    #[test]
    fn apg_snapshot_carries_labels_and_coming_soon() {
        let snap = super::apg_snapshot();
        assert_eq!(snap.len(), 5);
        assert!(snap.iter().all(|s| !s.label.is_empty()));
        let soon: Vec<_> = snap.iter().filter(|s| s.state == "coming_soon").map(|s| s.pkg).collect();
        assert_eq!(soon, ["agentflow", "medianode", "expert_raw_pipeline"]);
        assert!(snap.iter().filter(|s| s.state == "coming_soon").all(|s| s.pid.is_none()));
    }
}
