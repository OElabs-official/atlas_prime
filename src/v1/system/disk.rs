//! v1/disk — 挂载点容量采集 + 进程标准路径表。
//!
//! # 公开接口用法
//!
//! 对外只有 [`Disk`]（唯一公开关联函数 [`Disk::get`]，一次全量采集）
//! 与路径表 [`dirs`]（`OnceLock` 进程内只计算一次）：
//!
//! ```ignore
//! // crate 内调用路径：crate::v1::system::disk
//! let disks = crate::v1::system::disk::Disk::get();  // Vec<Disk>，按 total_mb 降序
//!
//! let d = &disks[0];
//! d.device;       // String 块设备路径，如 "/dev/block/dm-72"
//! d.fs;           // String 文件系统类型，如 "f2fs"
//! d.mount_point;  // String 挂载点，如 "/data"
//! d.total_mb;     // u64 总容量 MiB（statvfs f_blocks * f_frsize）
//! d.avail_mb;     // u64 非特权可用 MiB（statvfs f_bavail * f_frsize）
//! d.used_mb;      // u64 = total_mb - avail_mb
//!
//! // 路径表（首次调用时经 directories crate 解析并缓存，之后零成本）：
//! let list = crate::v1::system::disk::dirs();        // &'static [String]，分组展示行
//! println!("{list:#?}");                     // 含 Runtime/Project/Base/User 四段
//! ```
//!
//! 内部策略（本机实测：/proc/self/mounts 共 178 行，真实存储面仅数条）：
//! - 解析 `/proc/self/mounts`，只保留 fstype 在白名单（ext4/f2fs/erofs/vfat/fuse…）
//!   **且** 设备源以 `/` 开头的条目，一次滤掉 tmpfs/proc/sysfs/cgroup/pstore 等伪文件系统；
//! - 同一块设备多次挂载（如 /dev/fuse 的 7 个 sdcard 视图、/dev/block/loop 的 apex 克隆）
//!   去重，仅保留挂载点最短的一条；
//! - 容量走 `libc::statvfs`，权限不足（EACCES）的挂载点静默跳过；
//! - 路径表用 `OnceLock` 惰性缓存（本机 Termux std 构建无 `sync::OnceCell`，
//!   `OnceLock` 为等价物），项目段沿用 `crate::v1::PROJECT` 三元组。

use serde::Serialize;
use std::sync::OnceLock;

use directories::{BaseDirs, ProjectDirs, UserDirs};

use crate::v1::PROJECT;

/// 单个真实块设备挂载点的容量快照（本模块对外类型之一）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Disk {
    /// 块设备路径，如 `/dev/block/dm-72`
    pub device: String,
    /// 文件系统类型，如 `f2fs`
    pub fs: String,
    /// 挂载点路径，如 `/data`
    pub mount_point: String,
    /// 总容量（MiB）
    pub total_mb: u64,
    /// 非特权进程可用容量（MiB）
    pub avail_mb: u64,
    /// 已用容量（MiB）= total - avail
    pub used_mb: u64,
}

impl Disk {
    /// 【唯一公开关联函数】采集一次全部真实挂载点，按 `total_mb` 降序返回。
    pub fn get() -> Vec<Self> {
        let Ok(text) = std::fs::read_to_string("/proc/self/mounts") else {
            return Vec::new();
        };

        // 同一设备多次挂载只留挂载点最短的一条（如 /dev/fuse 挂 7 处）
        let mut uniq: Vec<(String, String, String)> = Vec::new(); // (device, fs, mount)
        for (dev, fs, mount) in candidates(&text) {
            match uniq.iter_mut().find(|(d, _, _)| *d == dev) {
                Some((_, f, m)) => {
                    if mount.len() < m.len() {
                        *m = mount;
                        *f = fs;
                    }
                }
                None => uniq.push((dev, fs, mount)),
            }
        }

        let mut out = Vec::new();
        for (dev, fs, mount) in uniq {
            let Some((total_mb, avail_mb)) = statvfs_mb(&mount) else {
                continue; // EACCES 等读取失败的挂载点静默跳过
            };
            out.push(Disk {
                device: dev,
                fs,
                used_mb: total_mb.saturating_sub(avail_mb),
                total_mb,
                avail_mb,
                mount_point: mount,
            });
        }
        out.sort_by(|a, b| {
            b.total_mb
                .cmp(&a.total_mb)
                .then(a.mount_point.cmp(&b.mount_point))
        });
        out
    }
}

/// 进程标准路径表（`OnceLock` 惰性初始化，全进程仅计算一次）。
///
/// 内容为分组展示行（Runtime Context / Project Standard / System Base / User Content 四段），
/// 与旧 UI 的 Directories 面板口径一致。
pub fn dirs() -> &'static [String] {
    static DIRS: OnceLock<Vec<String>> = OnceLock::new();
    DIRS.get_or_init(build_dirs)
}

// ---------------- 以下为内部黑箱 ----------------

/// 视为“真实存储”的文件系统白名单（其余一律视为伪文件系统过滤）
const REAL_FS: [&str; 12] = [
    "ext4", "f2fs", "erofs", "vfat", "exfat", "fuse", "fuseblk", "sdcardfs", "btrfs", "xfs",
    "ntfs", "ntfs3",
];

/// 解析 mounts 文本 → (设备源, fstype, 挂载点) 候选列表。
/// 纯函数，便于单测；挂载点中的八进制转义（`\040` 等）已还原。
fn candidates(text: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(dev), Some(mount), Some(fs)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if !dev.starts_with('/') || !REAL_FS.contains(&fs) {
            continue;
        }
        out.push((dev.to_owned(), fs.to_owned(), unescape_mount(mount)));
    }
    out
}

/// 还原 /proc/mounts 的八进制转义：`\040`=空格 `\011`=tab `\012`=换行 `\134`=反斜杠
fn unescape_mount(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let (Some('0'), Some(o), Some(d)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if let (Some(o), Some(d)) = (o.to_digit(8), d.to_digit(8)) {
            if let Some(ch) = char::from_u32(o * 8 + d) {
                out.push(ch);
            }
        }
    }
    out
}

/// statvfs 一次挂载点的容量（MiB）；读取失败（权限等）返回 None
fn statvfs_mb(mount: &str) -> Option<(u64, u64)> {
    const MB: u64 = 1024 * 1024;
    let cpath = std::ffi::CString::new(mount.to_owned()).ok()?;
    // SAFETY: zeroed statvfs 是合法初始状态，statvfs(2) 只写该结构体
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: cpath 为有效 C 字符串，&mut st 为有效指针
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    let total = st.f_blocks as u64;
    let avail = st.f_bavail as u64;
    Some((total * frsize / MB, avail * frsize / MB))
}

/// 首次调用时构建分组路径表（此后由 OnceLock 缓存）
fn build_dirs() -> Vec<String> {
    let mut list = Vec::new();

    // 1. 程序运行环境
    list.push("--- [ Runtime Context ] ---".to_string());
    list.push(format!(
        "Executable Dir: {:?}",
        std::env::current_exe().unwrap_or_default()
    ));
    list.push(format!(
        "Working Dir:    {:?}",
        std::env::current_dir().unwrap_or_default()
    ));

    // 2. 项目标准路径（与 crate::v1::PROJECT 三元组一致）
    if let Some(proj) = ProjectDirs::from(PROJECT[0], PROJECT[1], PROJECT[2]) {
        list.push("\n--- [ Project Standard Dirs ] ---".to_string());
        list.push(format!("Data Root:   {:?}", proj.data_dir()));
        list.push(format!("Cache Root:  {:?}", proj.cache_dir()));
        list.push(format!("Preferences: {:?}", proj.preference_dir()));
        if let Some(state) = proj.state_dir() {
            list.push(format!("State Root:  {:?}", state));
        }
    }

    // 3. 系统基础路径
    if let Some(base) = BaseDirs::new() {
        list.push("\n--- [ System Base Dirs ] ---".to_string());
        list.push(format!("Home:        {:?}", base.home_dir()));
        list.push(format!("Base Config: {:?}", base.config_dir()));
        list.push(format!("Base Data:   {:?}", base.data_dir()));
        list.push(format!("Base Cache:  {:?}", base.cache_dir()));
        if let Some(runtime) = base.runtime_dir() {
            list.push(format!("Runtime:     {:?}", runtime));
        }
    }

    // 4. 用户内容目录（Android 上多数不存在，存在才列出）
    if let Some(user) = UserDirs::new() {
        list.push("\n--- [ User Content Dirs ] ---".to_string());
        list.push(format!("Home:      {:?}", user.home_dir()));
        let opts: [(&str, Option<&std::path::Path>); 8] = [
            ("Downloads", user.download_dir()),
            ("Documents", user.document_dir()),
            ("Pictures", user.picture_dir()),
            ("Videos", user.video_dir()),
            ("Audios", user.audio_dir()),
            ("Desktop", user.desktop_dir()),
            ("Public", user.public_dir()),
            ("Templates", user.template_dir()),
        ];
        for (label, p) in opts {
            if let Some(p) = p {
                list.push(format!("{label}: {p:?}"));
            }
        }
    }

    list
}

#[cfg(test)]
mod tests {
    use super::{candidates, dirs, unescape_mount};

    /// 本机（Android 14 / Termux）/proc/self/mounts 的真实样本行混排
    const FIXTURE: &str = "/dev/block/dm-14 / erofs ro,seclabel,relatime 0 0\n\
        /dev/block/dm-72 /data f2fs rw,lazytime,seclabel 0 0\n\
        /dev/fuse /storage/emulated fuse rw,nosuid,nodev,noexec 0 0\n\
        /dev/fuse /mnt/user/0/emulated fuse rw,nosuid,nodev,noexec 0 0\n\
        tmpfs /tmp tmpfs rw,seclabel 0 0\n\
        proc /proc proc rw,relatime 0 0\n\
        sysfs /sys sysfs rw,relatime 0 0\n\
        none /sys/fs/fuse/connections fusectl rw 0 0\n\
        /dev/block/dm-90 /apex/com.android.adbd@7823 erofs ro 0 0\n";

    #[test]
    fn keeps_only_real_block_mounts() {
        let got = candidates(FIXTURE);
        let mounts: Vec<&str> = got.iter().map(|(_, _, m)| m.as_str()).collect();
        assert!(mounts.contains(&"/"), "erofs / 应保留");
        assert!(mounts.contains(&"/data"), "f2fs /data 应保留");
        assert!(mounts.contains(&"/storage/emulated"), "sdcard fuse 应保留");
        assert!(!mounts.contains(&"/tmp"), "tmpfs 应过滤");
        assert!(!mounts.contains(&"/proc"), "proc 应过滤");
        assert!(
            !mounts.contains(&"/sys/fs/fuse/connections"),
            "设备源非 / 开头应过滤"
        );
    }

    #[test]
    fn unescapes_octal_paths() {
        assert_eq!(unescape_mount("/mnt/My\\040Disk"), "/mnt/My Disk");
        assert_eq!(unescape_mount("/plain"), "/plain");
    }

    #[test]
    fn dirs_is_cached_once() {
        let a = dirs();
        let b = dirs();
        assert!(std::ptr::eq(a, b), "OnceLock 应返回同一块内存");
        assert!(a.iter().any(|l| l.contains("Home")), "路径表应包含 Home 行");
    }
}
