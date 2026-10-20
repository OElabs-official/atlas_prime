//! v1/system: 底层硬件/系统采集黑箱（sysfs / procfs / termux-api CLI / libc）。
//!
//! 设计要点（由本机实测验证，骁龙8Gen2 / 8核 / 68 thermal zones）：
//! - 静态拓扑（核数、热传感器列表、路径表）均经 [`std::sync::OnceLock`] 只初始化一次
//!   （OnceCell 等价物，本机 Termux std 构建无 `sync::OnceCell`）；
//! - 核数从 `/sys/devices/system/cpu/present` 解析（注意：不能用 nproc，
//!   Android 会限制进程亲和性导致 nproc < 实际核数）；
//! - 热传感器按 type 前缀分类，不写死 zone 号，跨机型更稳；
//! - 全部数据源无子进程 `mount`/`free`/`ip`：挂载走 /proc + statvfs，
//!   内存走 /proc/meminfo，IP 走 getifaddrs，仅电池必须经 termux-api CLI。
//!
//! 对外接口刻意收窄为七个公开面（调用路径 `crate::v1::system::*`）：
//! - [`cpu::freq_ghz`]：各核当前频率（GHz）；
//! - [`thermal::Temp`]：温度结构体，`Temp::get()` 采集，`温度.类别[索引].0/.1` 取摄氏度/type名；
//! - [`battery::Battery`]：电池状态结构体，`Battery::get()` 采集（直读 CLI JSON，不依赖 api crate）；
//! - [`disk::Disk`]：挂载点容量结构体，`Disk::get()` 全量返回（按容量降序）；
//! - [`disk::dirs`]：进程标准路径表，`OnceLock` 缓存，返回 `&'static [String]`；
//! - [`ip::Ip`]：本机地址结构体，`Ip::get()` 按 回环/局域网/公网v4(HTTP回显+DNS TXT)/公网v6 分桶；
//! - [`mem::Mem`]：RAM/Swap 用量结构体，`Mem::get()` 采集（单次读 /proc/meminfo）。

#[allow(dead_code)]
pub mod battery;
#[allow(dead_code)]
pub mod cpu;
#[allow(dead_code)]
pub mod disk;
#[allow(dead_code)]
pub mod ip;
#[allow(dead_code)]
pub mod mem;
#[allow(dead_code)]
pub mod thermal;

#[cfg(test)]
mod tests {
    use super::thermal::Temp;

    /// 真机冒烟测试（需要 sysfs），默认跳过，用 `cargo test -- --ignored` 运行
    #[test]
    #[ignore]
    fn live_collection() {
        println!("freq_ghz = {:?}", super::cpu::freq_ghz());
        let t = Temp::get();
        println!(
            "cpu_core x{}  hottest={}°C ({})",
            t.cpu_core.len(),
            t.cpu_core.iter().map(|(v, _)| *v).fold(f32::MIN, f32::max),
            t.cpu_core[0].1
        );
        println!(
            "groups: gpu={} camera={} pmic={} battery={}",
            t.gpu.len(),
            t.camera.len(),
            t.pmic.len(),
            t.battery.len()
        );
        let bat = super::battery::Battery::get();
        println!(
            "battery: {}% {} {}  sysfs_temp={:?}",
            bat.percentage,
            bat.status,
            bat.plugged,
            t.battery.first().map(|(v, _)| *v)
        );

        let disks = super::disk::Disk::get();
        for d in &disks {
            println!(
                "disk: {} {} {}  {}MiB used/{}MiB",
                d.mount_point, d.device, d.fs, d.used_mb, d.total_mb
            );
        }
        println!(
            "dirs: {} 行（缓存={:?}）",
            super::disk::dirs().len(),
            super::disk::dirs().as_ptr()
        );
        let net = super::ip::Ip::get();
        println!(
            "ip: loopback={:?} lan={}条 public_v4={:?} public_v6={:?}",
            net.loopback,
            net.lan.len(),
            net.public_v4,
            net.public_v6
        );
        let m = super::mem::Mem::get();
        println!(
            "mem: {}/{}MiB used (avail {}MiB)  swap: {}/{}MiB",
            m.used_mem_mb, m.total_mem_mb, m.avail_mem_mb, m.used_swap_mb, m.total_swap_mb
        );

        assert!(!super::cpu::freq_ghz().is_empty());
        assert!(!t.cpu_core.is_empty());
        assert!(t.cpu_core.iter().all(|(v, _)| *v > -50.0 && *v < 150.0));
        assert!(!disks.is_empty(), "至少应有 / 与 /data 两个真实挂载");
        assert!(disks.iter().any(|d| d.mount_point == "/data"));
        assert!(!super::disk::dirs().is_empty());
        assert!(m.total_mem_mb > 0 && m.used_mem_mb > 0);
        assert!(!net.loopback.is_empty(), "lo 回环必现");
    }
}
