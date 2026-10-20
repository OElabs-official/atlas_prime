//! v1/cpu — CPU 当前频率读取（对外黑箱，仅暴露 [`freq_ghz`]）。
//!
//! # 公开接口用法
//!
//! ```ignore
//! // crate 内调用路径：crate::v1::system::cpu
//! let freqs: Vec<f32> = crate::v1::system::cpu::freq_ghz(); // 唯一公开函数
//! //  freqs.len()      = 在线核心数（本机 = 8）
//! //  freqs[i]         = 第 i 个在线核的当前频率，单位 GHz（读取失败为 0.0）
//! //  顺序 = 核心编号升序，如 [0.8, 0.8, ..., 3.36]
//! let max_ghz = freqs.iter().copied().fold(0.0, f32::max);
//! ```
//!
//! 数据源：
//! - 核数：`/sys/devices/system/cpu/present`（内核 CPU 拓扑的权威来源，
//!   支持 `0-7` / `0-3,8-11` 等区间列表格式）。
//! - 频率：`/sys/devices/system/cpu/cpu{N}/cpufreq/scaling_cur_freq`，
//!   由内核 cpufreq governor 实时反映，单位 kHz。各核路径在拓扑初始化时
//!   一次性拼好并缓存，热路径零分配。
//!
//! 注意：Android/Termux 下进程亲和性常被限制（如 `Cpus_allowed_list: 0-1,4-5`），
//! `nproc` / `available_parallelism` 会小于物理核数，因此核数只认 sysfs。

use std::collections::BTreeSet;
use std::fs;
use std::sync::OnceLock;

const PRESENT_PATH: &str = "/sys/devices/system/cpu/present";

/// 全局唯一一次初始化的 CPU 拓扑（本机 std 构建无 `sync::OnceCell`，用等价的 `OnceLock`）
static TOPOLOGY: OnceLock<Topology> = OnceLock::new();

/// CPU 拓扑（内部黑箱：在线核编号列表 + 各自 scaling_cur_freq 路径，一次构建）
struct Topology {
    cores: Vec<u32>,
    paths: Vec<std::path::PathBuf>,
}

/// 惰性获取 CPU 拓扑（首次调用时读 sysfs 并拼好各核频率路径，之后直接命中缓存）
fn topology() -> &'static Topology {
    TOPOLOGY.get_or_init(|| {
        let cores = read_present_cores().unwrap_or_else(fallback_cores);
        let paths = cores
            .iter()
            .map(|&core| {
                std::path::PathBuf::from(format!(
                    "/sys/devices/system/cpu/cpu{core}/cpufreq/scaling_cur_freq"
                ))
            })
            .collect();
        Topology { cores, paths }
    })
}

/// 在线 CPU 核心数（首次调用触发探测，之后 O(1)）
#[inline]
fn cpu_count() -> usize {
    topology().cores.len()
}

/// 【唯一对外接口】读取各核当前频率，单位 GHz，返回顺序与核心编号升序一致。
///
/// - 返回长度 = 在线核心数（本机 = 8）；
/// - 单核读取失败时该位置填 `0.0`。
pub fn freq_ghz() -> Vec<f32> {
    topology()
        .paths
        .iter()
        .map(|path| read_khz_from(path).unwrap_or(0))
        .map(|khz| khz as f32 / 1_000_000.0)
        .collect()
}

/// 从单个 scaling_cur_freq 文件解析 kHz 值
fn read_khz_from(path: &std::path::Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse::<u64>().ok()
}

/// 解析 `/sys/devices/system/cpu/present`，如 `"0-7\n"` 或 `"0-3,8-11\n"`
fn read_present_cores() -> Option<Vec<u32>> {
    let raw = fs::read_to_string(PRESENT_PATH).ok()?;
    parse_present_list(raw.trim())
}

/// 解析区间列表字符串（纯函数，便于单测）
fn parse_present_list(s: &str) -> Option<Vec<u32>> {
    if s.is_empty() {
        return None;
    }
    let mut set = BTreeSet::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        if let Some((lo, hi)) = part.split_once('-') {
            let lo: u32 = lo.trim().parse().ok()?;
            let hi: u32 = hi.trim().parse().ok()?;
            if hi < lo {
                return None;
            }
            set.extend(lo..=hi);
        } else {
            set.insert(part.parse().ok()?);
        }
    }
    if set.is_empty() {
        None
    } else {
        Some(set.into_iter().collect())
    }
}

/// sysfs 不可读时的兜底：退化为线程并行度（可能因亲和性受限而偏小）
fn fallback_cores() -> Vec<u32> {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    (0..n as u32).collect()
}

#[cfg(test)]
mod tests {
    use super::{cpu_count, parse_present_list};

    #[test]
    fn single_range() {
        assert_eq!(parse_present_list("0-7"), Some((0..8).collect()));
    }

    #[test]
    fn mixed_ranges_and_singles() {
        assert_eq!(
            parse_present_list("0-3,8,10-11"),
            Some(vec![0, 1, 2, 3, 8, 10, 11])
        );
    }

    #[test]
    fn dedup_and_sort() {
        assert_eq!(parse_present_list("5,1,1-2"), Some(vec![1, 2, 5]));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_present_list(""), None);
        assert_eq!(parse_present_list("a-b"), None);
        assert_eq!(parse_present_list("7-1"), None);
    }

    #[test]
    fn count_at_least_one() {
        // 本机探测应为 8 核；非 sysfs 环境兜底也至少有 1
        assert!(cpu_count() >= 1);
    }
}
