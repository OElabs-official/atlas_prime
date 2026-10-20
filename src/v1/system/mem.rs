//! v1/mem — RAM 与 Swap 用量采集。
//!
//! # 公开接口用法
//!
//! 对外只有一个结构体 [`Mem`]（唯一公开关联函数 [`Mem::get`]，一次采集）：
//!
//! ```ignore
//! // crate 内调用路径：crate::v1::system::mem
//! let m = crate::v1::system::mem::Mem::get();
//!
//! m.total_mem_mb;  // u64 物理内存总量 MiB（/proc/meminfo: MemTotal）
//! m.used_mem_mb;   // u64 已用 MiB = MemTotal - MemAvailable（口径同 top/htop）
//! m.avail_mem_mb;  // u64 内核评估可回收后真实可用 MiB（MemAvailable）
//! m.total_swap_mb; // u64 Swap 总量 MiB（SwapTotal；zram 设备也计入）
//! m.used_swap_mb;  // u64 Swap 已用 MiB = SwapTotal - SwapFree
//!
//! // 惯用比例：
//! let pct = m.used_mem_mb as f64 * 100.0 / m.total_mem_mb.max(1) as f64;
//! ```
//!
//! 内部策略：
//! - 单次读 `/proc/meminfo`（本机 15GB RAM / 20GB zram 实测 <0.1ms），
//!   按行取 `Key: NNN kB` 的数值，**不依赖 sysinfo 的 `System` 对象**
//!   （`System::new_all()` 会顺带扫描全部进程表，对本用途是纯浪费）；
//! - 内核无 `MemAvailable`（<3.14 老内核）时退回 `MemFree` 口径；
//! - 缺键一律按 0 处理，绝不 panic。

use serde::Serialize;
use std::fs;

const MEMINFO: &str = "/proc/meminfo";

/// 一次 RAM/Swap 用量快照（本模块唯一对外类型）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Mem {
    /// 物理内存总量（MiB）
    pub total_mem_mb: u64,
    /// 已用内存（MiB，= total - avail）
    pub used_mem_mb: u64,
    /// 真实可用内存（MiB，MemAvailable 口径）
    pub avail_mem_mb: u64,
    /// Swap 总量（MiB；Android zram 计入）
    pub total_swap_mb: u64,
    /// Swap 已用（MiB，= total - free）
    pub used_swap_mb: u64,
}

impl Mem {
    /// 【唯一公开关联函数】采集一次内存/Swap 快照；读取失败返回全 0。
    pub fn get() -> Self {
        match fs::read_to_string(MEMINFO) {
            Ok(text) => parse(&text),
            Err(_) => Self::default(),
        }
    }
}

// ---------------- 以下为内部黑箱 ----------------

/// 解析 meminfo 文本（纯函数，便于单测）
fn parse(text: &str) -> Mem {
    let kb = |key: &str| -> u64 { key_value_kb(text, key) };

    let total = kb("MemTotal");
    // MemAvailable 缺失时退回 MemFree（老内核口径）
    let avail = if text.contains("MemAvailable:") {
        kb("MemAvailable")
    } else {
        kb("MemFree")
    };
    let swap_total = kb("SwapTotal");
    let swap_free = kb("SwapFree");

    Mem {
        total_mem_mb: total / 1024,
        used_mem_mb: total.saturating_sub(avail) / 1024,
        avail_mem_mb: avail / 1024,
        total_swap_mb: swap_total / 1024,
        used_swap_mb: swap_total.saturating_sub(swap_free) / 1024,
    }
}

/// 取 `Key:\tVALUE kB` 行中的 VALUE（kB）；缺键返回 0
fn key_value_kb(text: &str, key: &str) -> u64 {
    let prefix = format!("{key}:");
    text.lines()
        .find(|l| l.starts_with(&prefix))
        .and_then(|l| {
            l[prefix.len()..]
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{key_value_kb, parse};

    /// 本机（15GB RAM / 20GB zram swap）/proc/meminfo 头部真实样本
    const FIXTURE: &str = "MemTotal:       15463724 kB\n\
        MemFree:          327460 kB\n\
        MemAvailable:    3212984 kB\n\
        Buffers:            6416 kB\n\
        Cached:          3023416 kB\n\
        SwapTotal:      20971516 kB\n\
        SwapFree:       11662724 kB\n";

    #[test]
    fn parses_real_device_fixture() {
        let m = parse(FIXTURE);
        assert_eq!(m.total_mem_mb, 15463724 / 1024);
        assert_eq!(m.avail_mem_mb, 3212984 / 1024);
        assert_eq!(m.used_mem_mb, (15463724 - 3212984) / 1024);
        assert_eq!(m.total_swap_mb, 20971516 / 1024);
        assert_eq!(m.used_swap_mb, (20971516 - 11662724) / 1024);
    }

    #[test]
    fn falls_back_to_memfree_without_memavailable() {
        let text = "MemTotal: 1000000 kB\nMemFree: 400000 kB\n";
        let m = parse(text);
        assert_eq!(m.avail_mem_mb, 400000 / 1024);
        assert_eq!(m.used_mem_mb, 600000 / 1024);
    }

    #[test]
    fn missing_keys_are_zero_not_panic() {
        let m = parse("garbage line\n");
        assert_eq!(m.total_mem_mb, 0);
        assert_eq!(m.used_swap_mb, 0);
        assert_eq!(key_value_kb("", "MemTotal"), 0);
    }
}
