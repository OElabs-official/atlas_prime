//! v1/service —— runit 服务内置安装/卸载（被动模块：仅被 CLI 一次性调用与守护模式
//! 状态提示，零常驻状态；取代原 resources/sv-install.sh）。
//!
//! --sv-install 流程：复制当前二进制到 $PREFIX/bin/atlas_prime（0755，就地重装则
//! 跳过）→ 以内嵌资产（include_str!，源 _resources/sv/atlas_prime/，与 api 内嵌
//! dashboard 同法）落盘 $SVDIR/atlas_prime 的 run 与 log/run（0700）→ 预建 svlogd
//! 日志目录（svlogd 不会自建目录，缺目录时 log/run 直接退出——设备 E2E 实证）→
//! 全程不写 down 占位，调用 sv-enable 直接上线。
//! SVDIR 解析：显式非空 env SVDIR 优先；未设/为空则兜底 $PREFIX/var/service 并打印所用根。

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
            "[atlas] {} 带 down 占位（服务停用中）——运行 'atlas_prime --sv-install' 重新 enable",
            svc.display()
        );
    }
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
