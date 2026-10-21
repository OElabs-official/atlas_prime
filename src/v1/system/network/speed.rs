//! network/speed — 网卡速度计量原语：数据面分组 + 组和差分 + 固定序输出。
//!
//! 数据源四级（真机 Android Termux 实测前三级在 SELinux untrusted_app 域全被
//! 拒读，adb 桥是唯一活路；在放行域下直读依旧最优）：
//! 1. `/proc/net/dev` 直读（一次读取拿全部网卡的累计 rx/tx 字节）；
//! 2. **adb 桥**：`adb -s localhost:<port> shell cat /proc/net/dev`（端口取自
//!    数据目录 `adb_port` 文件，「无线配对」任务/人工维护、看门狗探活；链路与超时纪律见 [`devsrc`]，
//!    正文与真文件同构、解析器不感知源）；
//! 3. `/sys/class/net/<iface>/statistics/{rx,tx}_bytes` 逐网卡读取；
//! 4. netlink RTM_GETLINK dump 的 IFLA_STATS（见 [`super::netlink`]）。
//!
//! **分组计量**（本模块的核心抽象，选择器 [`group_key`]）：速度不再逐网卡出数，
//! 数据面网卡按固定映射聚合成组，组内**先求计数器总和再差分**（[`Grouper`]）：
//! - `wlan*` → 组 `wifi`；
//! - `rmnet_data*` / `rmnet_link*` / `rmnet[0-9]*` → 组 `cellular`；
//! - 其余数据面网卡（`usb0` / `eth*` / `rndis*` …）→ 自成一组，组名 = 网卡名，
//!   且仅在本拍累计 rx+tx > 0（见过流量）时入组——恒零未知网卡不占坑；
//! - `lo` / `dummy*` / `ifb*` / `tun*` / `br0` 与 rmnet 控制面附件
//!   （`rmnet_ipa0`、`rmnet_ioctl0`、`rmnet_wlan0_9` 等）永不入组。
//!
//! 成员出现/消失会**整组重建基线**（当拍 emit 0，绝不做「新成员历史累加
//! 冒充本拍流量」的假尖峰）；组内计数器倒退（网卡重建/重启清零）饱和减钳 0。
//!
//! 输出恒为固定序 `[wifi, cellular, 其余按名升序]`。
//!
//! 本子模块**无自发运行体**（自主驱动权只在操作器）：这里只提供读数与跨拍
//! 状态机原语，1Hz 节拍、缓存、落库归 [`crate::v1::operator`]。

mod devsrc;

/// 正文源链入口与错误类型上浮（定义在 devsrc，域级路径 `network::read_dev_snapshot` 亦可达）
pub use devsrc::{NetError, read_dev_snapshot};

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// 一次网卡累计计数器快照项：`(网卡名, 累计接收字节, 累计发送字节)`。
pub type Counters = Vec<(String, u64, u64)>;

/// Wi-Fi 组名（全部 `wlan*` 聚合于此）
pub const GROUP_WIFI: &str = "wifi";
/// 蜂窝组名（全部 `rmnet_data*` / `rmnet_link*` / `rmnet[0-9]*` 聚合于此）
pub const GROUP_CELLULAR: &str = "cellular";

/// 数据面网卡分组（纯分类函数）：`Some(组名)` = 入组，`None` = 永不入组。
/// 固定映射 [`GROUP_WIFI`] / [`GROUP_CELLULAR`] 之外的数据面网卡（usb0/eth*/
/// rndis*…）以自身网卡名为组名；`lo` / `dummy*` / `ifb*` / `tun*` / `br0` 与
/// rmnet 控制面附件（非 `_data` / `_link` / 纯数字后缀）返回 `None`。
/// 恒零未知网卡何时出局见 [`Grouper::step`]（分类层不做流量判断）。
pub fn group_key(iface: &str) -> Option<String> {
    if iface == "lo"
        || iface == "br0"
        || iface.starts_with("dummy")
        || iface.starts_with("ifb")
        || iface.starts_with("tun")
    {
        return None;
    }
    if iface.starts_with("wlan") {
        return Some(GROUP_WIFI.to_owned());
    }
    match iface.strip_prefix("rmnet") {
        // rmnet 后接空/纯数字 = rmnet[0-9]*；_data/_link 前缀 = 对应两条通配；
        // 其余 rmnet_*（ipa/ioctl/wlan_x…）为控制面附件，永不入组
        Some(rest) => {
            let cellular = rest.is_empty()
                || rest.starts_with("_data")
                || rest.starts_with("_link")
                || (!rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()));
            cellular.then_some(GROUP_CELLULAR.to_owned())
        }
        None => Some(iface.to_owned()),
    }
}

/// 解析 `/proc/net/dev` 正文（纯函数，单测吃 fixture 字符串）：
/// 每网卡一行，冒号左侧为网卡名，右侧列以空白分隔（对齐宽度不定）；
/// 第 1 列 = 累计接收字节，第 9 列 = 累计发送字节（接收段共 8 列）。
/// 表头两行不含冒号自然跳过；数值列不足 9 的行跳过；
/// 请求的网卡在正文中缺席 = 结果里不出现（无占位、无错误）。
pub fn parse_dev_stats(text: &str) -> Counters {
    let mut out = Counters::new();
    for line in text.lines() {
        let Some((name, cols)) = line.split_once(':') else {
            continue; // 表头行（Inter-|… / face |…）无冒号
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let nums: Vec<u64> = cols.split_whitespace().filter_map(|c| c.parse().ok()).collect();
        if nums.len() < 9 {
            continue;
        }
        out.push((name.to_owned(), nums[0], nums[8]));
    }
    out
}

/// 数据面网卡累计计数器快照（吞错门面对外版）：四级全灭 = 空快照。
/// 需要区分「域被拒」与「桥故障」的调用方（操作器降级计数）用 [`snapshot_result`]。
pub fn snapshot() -> Counters {
    snapshot_result().unwrap_or_default()
}

/// 数据面网卡累计计数器快照（带因版）：直读 → adb 桥（[`devsrc`]）→
/// sysfs statistics → netlink IFLA_STATS 四级顺试，命中即按 [`group_key`]
/// 可入组过滤（分组聚合与恒零淘汰在 [`Grouper`]，不在快照层）。
/// 四级全灭回 [`NetError`]（最后一级正文链的成因），供调用侧做降级/声张。
pub fn snapshot_result() -> Result<Counters, NetError> {
    let raw = match devsrc::read_dev_snapshot() {
        Ok(text) => parse_dev_stats(&text),
        Err(e) => {
            let s = sysfs_snapshot();
            if !s.is_empty() {
                s
            } else {
                let n = super::netlink::link_stats();
                if n.is_empty() {
                    return Err(e);
                }
                n
            }
        }
    };
    Ok(raw.into_iter().filter(|(name, ..)| group_key(name).is_some()).collect())
}

/// sysfs 兜底：/sys/class/net/<iface>/statistics 两文件逐网卡读，缺文件者跳过
fn sysfs_snapshot() -> Counters {
    let mut out = Counters::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if group_key(&name).is_none() {
            continue;
        }
        let stat = e.path().join("statistics");
        let (Some(rx), Some(tx)) = (
            read_u64(&stat.join("rx_bytes")),
            read_u64(&stat.join("tx_bytes")),
        ) else {
            continue;
        };
        out.push((name, rx, tx));
    }
    out
}

fn read_u64(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// 一组的当前拍聚合态：成员累计和 + 成员集合（集合等值 = 差分合法性的判据）
#[derive(Clone, Default, PartialEq, Eq)]
struct GroupSum {
    rx: u64,
    tx: u64,
    members: BTreeSet<String>,
}

/// 组和差分状态机（跨拍持态，1Hz 回路每拍喂一次 [`Grouper::step`]）。
/// 上拍各组 `(rx_sum, tx_sum, 成员集合)` 存于 [`Grouper::base`]，本拍按组比对。
#[derive(Default)]
pub struct Grouper {
    base: HashMap<String, GroupSum>,
}

impl Grouper {
    /// 一拍：快照 → 按 [`group_key`] 逐组求和 → 与上拍组和差分 → 固定序输出
    /// `[wifi, cellular, 其余按名升序]`。规则：
    /// - **先和后差**：组速率 = 组成员计数器总和的拍间差 ÷ dt（成员增减带来的
    ///   组和跳变不算流量，见下条）；
    /// - 新组首现 / 成员集合变化 / `dt_secs <= 0`（时钟异常）→ 当拍该组 emit 0
    ///   并以本拍重建基线（no fabricated spikes，取「emit 0」档）；
    /// - 组内计数器倒退（网卡重建 / 重启清零）→ 饱和减钳 0；
    /// - 动态组（非 wifi/cellular）本拍累计和为 0 → 不入组、不占基线（恒零未知
    ///   网卡出局）；一旦见过流量即常驻，空闲拍 emit 0；
    /// - 整组消失（成员全没）→ 本拍不 emit、基线拆除，再现时按新组重建。
    pub fn step(&mut self, cur: &Counters, dt_secs: f64) -> Vec<(String, f64, f64)> {
        let inv = if dt_secs > 0.0 { 1.0 / dt_secs } else { 0.0 };
        let mut sums: BTreeMap<String, GroupSum> = BTreeMap::new();
        for (iface, rx, tx) in cur {
            let Some(g) = group_key(iface) else {
                continue;
            };
            let e = sums.entry(g).or_default();
            e.rx = e.rx.saturating_add(*rx);
            e.tx = e.tx.saturating_add(*tx);
            e.members.insert(iface.clone());
        }
        // 恒零未知网卡出局（wifi/cellular 只要有成员就常驻，闲置拍显式 0）
        sums.retain(|g, e| {
            g == GROUP_WIFI || g == GROUP_CELLULAR || e.rx.saturating_add(e.tx) > 0
        });
        // 消失组：拆基线（再现 = 新组，重建，不补发）
        self.base.retain(|g, _| sums.contains_key(g));

        let mut fixed: [Option<(String, f64, f64)>; 2] = [None, None];
        let mut others: Vec<(String, f64, f64)> = Vec::new();
        for (g, sum) in sums {
            let rate = match &self.base.get(&g) {
                Some(b) if b.members == sum.members => (
                    sum.rx.saturating_sub(b.rx) as f64 * inv,
                    sum.tx.saturating_sub(b.tx) as f64 * inv,
                ),
                // 新组 / 成员变化：无一致基线，emit 0 并重建
                _ => (0.0, 0.0),
            };
            self.base.insert(g.clone(), sum);
            match g.as_str() {
                GROUP_WIFI => fixed[0] = Some((g, rate.0, rate.1)),
                GROUP_CELLULAR => fixed[1] = Some((g, rate.0, rate.1)),
                // BTreeMap 遍历序 = 组名升序
                _ => others.push((g, rate.0, rate.1)),
            }
        }
        fixed.into_iter().flatten().chain(others).collect()
    }
}

// ---------------- 测试 ----------------

#[cfg(test)]
mod tests {
    use super::{
        Counters, GROUP_CELLULAR, GROUP_WIFI, Grouper, group_key, parse_dev_stats,
    };

    /// 真机形状 fixture：表头两行 + 多空格对齐（lo 前置空格、冒号紧贴、
    /// rmnet 行单空格紧凑）+ 数据面/非数据面混排
    const FIXTURE: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  1234567    9876    0    0    0     0          0         0  1234567    9876    0    0    0     0       0          0
 wlan0: 987654321  456789    0    0    0     0          0         0 123456789   345678    0    0    0     0       0          0
rmnet_data3:55555555 55555 0 0 0 0 0 0 66666666 66666 0 0 0 0 0 0
   dummy0: 111 11 0 0 0 0 0 0 222 22 0 0 0 0 0 0
";

    #[test]
    fn parses_aligned_rows_and_absent_iface_just_missing() {
        let got = parse_dev_stats(FIXTURE);
        assert_eq!(
            got.len(),
            4,
            "表头须跳过、4 条网卡行须解析出: {got:?}"
        );
        assert_eq!(got[0], ("lo".to_string(), 1_234_567, 1_234_567));
        assert_eq!(got[1], ("wlan0".to_string(), 987_654_321, 123_456_789));
        assert_eq!(
            got[2],
            ("rmnet_data3".to_string(), 55_555_555, 66_666_666)
        );
        // 正文缺席的网卡：不出现即正确（无占位、无错误）
        assert!(!got.iter().any(|(n, ..)| n == "br0"), "br0 不在正文中");
    }

    /// 分组映射表：固定两组 + 动态自组 + 永不入组全集
    #[test]
    fn group_mapping_table_is_exact() {
        for ok in [
            "wlan0", "wlan1", "wlan12", // 全部 wlan* → wifi
        ] {
            assert_eq!(group_key(ok).as_deref(), Some(GROUP_WIFI), "{ok}");
        }
        for ok in [
            "rmnet", "rmnet0", "rmnet42", // rmnet[0-9]*
            "rmnet_data0", "rmnet_data3", // rmnet_data*
            "rmnet_link7", // rmnet_link*
        ] {
            assert_eq!(group_key(ok).as_deref(), Some(GROUP_CELLULAR), "{ok}");
        }
        for ok in ["usb0", "eth0", "rndis0", "ath0"] {
            assert_eq!(group_key(ok).as_deref(), Some(ok), "{ok} 自组 = 网卡名");
        }
        for no in [
            "lo",
            "dummy0",
            "ifb0",
            "tun0",
            "br0",
            "rmnet_ipa0",     // 控制面附件
            "rmnet_ioctl0",   // 控制面附件
            "rmnet_wlan0_9",  // 控制面附件
        ] {
            assert_eq!(group_key(no), None, "{no} 永不入组");
        }
    }

    fn c(rows: &[(&str, u64, u64)]) -> Counters {
        rows.iter()
            .map(|(n, r, t)| (n.to_string(), *r, *t))
            .collect()
    }

    /// 组和差分主路径：成员先求和再差分；动态组恒零出局、见流量即入组（当拍 0）；
    /// 输出固定序 [wifi, cellular, others-升序]
    #[test]
    fn group_sum_diff_and_dynamic_membership_ordering() {
        let mut g = Grouper::default();
        let t1 = g.step(
            &c(&[
                ("wlan0", 100, 10),
                ("rmnet_data0", 10, 5),
                ("rmnet_data3", 20, 5),
                ("usb0", 0, 0), // 恒零未知 → 出局
                ("lo", 999, 999),
            ]),
            1.0,
        );
        assert_eq!(
            t1,
            vec![(GROUP_WIFI.into(), 0.0, 0.0), (GROUP_CELLULAR.into(), 0.0, 0.0)],
            "首拍只立基线；usb0 恒零不入、lo 永不入组"
        );
        let t2 = g.step(
            &c(&[
                ("wlan0", 300, 110),
                ("rmnet_data0", 30, 5),
                ("rmnet_data3", 40, 5),
                ("usb0", 7, 3), // 见过流量 → 自组入列，新基线当拍 0
                ("eth0", 5, 1),
            ]),
            2.0,
        );
        assert_eq!(
            t2,
            vec![
                (GROUP_WIFI.into(), 100.0, 50.0),    // (200B, 100B)/2s
                (GROUP_CELLULAR.into(), 20.0, 0.0),  // 组和 (70-30)/2s，tx 0
                ("eth0".into(), 0.0, 0.0),           // 新组当拍 0
                ("usb0".into(), 0.0, 0.0),           // 新组当拍 0，名升序在 eth0 后
            ]
        );
    }

    /// 成员出现/消失 = 整组重建基线：当拍 emit 0，无假尖峰；次拍恢复真实差分；
    /// 其余组不受牵连
    #[test]
    fn membership_change_rebuilds_group_without_spike() {
        let mut g = Grouper::default();
        g.step(&c(&[("wlan0", 100, 0), ("rmnet_data0", 1_000, 0)]), 1.0);
        // rmnet_data5 携 999_999 历史累加突然出现：蜂窝组和跳变不许当流量算
        let t = g.step(
            &c(&[("wlan0", 200, 0), ("rmnet_data0", 2_000, 0), ("rmnet_data5", 999_999, 0)]),
            1.0,
        );
        assert_eq!(
            t,
            vec![(GROUP_WIFI.into(), 100.0, 0.0), (GROUP_CELLULAR.into(), 0.0, 0.0)],
            "wifi 照常差分，cellular 重建基线 emit 0（假尖峰 = (1_001_000-1_000)/1s）"
        );
        // 次拍成员稳定：恢复真实差分（含新成员的增量）
        let t2 = g.step(
            &c(&[("wlan0", 200, 0), ("rmnet_data0", 2_500, 0), ("rmnet_data5", 1_000_499, 0)]),
            1.0,
        );
        assert_eq!(t2[1], (GROUP_CELLULAR.into(), 1_000.0, 0.0));
        // 成员消失（rmnet_data5 没了）→ 同样重建；整组消失（无 wlan）→ 不 emit
        let t3 = g.step(&c(&[("rmnet_data0", 3_000, 0)]), 1.0);
        assert_eq!(t3, vec![(GROUP_CELLULAR.into(), 0.0, 0.0)], "蜂窝减员重建；wifi 整组消失不 emit");
    }

    /// 组内计数器倒退（成员未变）饱和钳 0；dt<=0 速率全 0 但组照常输出
    #[test]
    fn counter_reset_clamps_and_clock_anomaly_zeroes() {
        let mut g = Grouper::default();
        g.step(&c(&[("wlan0", 5_000, 5_000)]), 1.0);
        let reset = g.step(&c(&[("wlan0", 40, 40)]), 1.0);
        assert_eq!(reset, vec![(GROUP_WIFI.into(), 0.0, 0.0)], "倒退钳 0");
        g.step(&c(&[("wlan0", 1_040, 40)]), 1.0);
        let frozen = g.step(&c(&[("wlan0", 2_040, 40)]), 0.0);
        assert_eq!(frozen, vec![(GROUP_WIFI.into(), 0.0, 0.0)], "dt<=0 全 0、组照常");
    }

    /// 无因丢弃禁止：快照内每个可入组网卡必被某输出组覆盖（求和守恒），
    /// 永不入组者绝不出现
    #[test]
    fn nothing_dropped_without_cause() {
        let mut g = Grouper::default();
        let snap = c(&[
            ("lo", 1, 1),
            ("dummy0", 2, 2),
            ("ifb0", 3, 3),
            ("tun0", 4, 4),
            ("br0", 5, 5),
            ("rmnet_ipa0", 6, 6),
            ("wlan0", 10, 1),
            ("wlan1", 20, 2),
            ("rmnet_data7", 30, 3),
            ("rmnet_link1", 40, 4),
            ("rmnet9", 50, 5),
            ("usb0", 60, 6),
            ("rndis0", 0, 0), // 恒零未知 = 被排除的唯一合法成因
        ]);
        let out = g.step(&snap, 1.0); // 首拍全 0，看的是组集合本身
        let names: Vec<&str> = out.iter().map(|(k, ..)| k.as_str()).collect();
        assert_eq!(names, vec![GROUP_WIFI, GROUP_CELLULAR, "usb0"]);
        // 求和守恒：每成员 rx 各 +10 → 组速率恰为成员增量之和
        //（wifi=20、cellular=30、usb0=10；漏掉任何成员都会露馅）
        let out2 = g.step(
            &c(&[
                ("lo", 1, 1),
                ("dummy0", 2, 2),
                ("ifb0", 3, 3),
                ("tun0", 4, 4),
                ("br0", 5, 5),
                ("rmnet_ipa0", 6, 6),
                ("wlan0", 20, 1),
                ("wlan1", 30, 2),
                ("rmnet_data7", 40, 3),
                ("rmnet_link1", 50, 4),
                ("rmnet9", 60, 5),
                ("usb0", 70, 6),
                ("rndis0", 0, 0),
            ]),
            1.0,
        );
        assert_eq!(
            out2,
            vec![
                (GROUP_WIFI.into(), 20.0, 0.0),
                (GROUP_CELLULAR.into(), 30.0, 0.0),
                ("usb0".into(), 10.0, 0.0),
            ]
        );
        let out3 = g.step(&c(&[]), 1.0); // 快照清空 → 组全体消失
        assert!(out3.is_empty(), "快照清空则组全体消失");
    }

    /// 真机读数冒烟，默认跳过：三级数据源受 SELinux 域约束——sealed 域（Termux
    /// untrusted_app 实测三级全拒）空快照即正确；放行域（root/shell 等）必须
    /// 出数且分组成立（细域诊断见 [`super::netlink::live_nlaccess_probe`] 同款路径）。
    #[test]
    #[ignore]
    fn live_snapshot_reads_counters() {
        let s = super::snapshot();
        if s.is_empty() {
            println!("sealed 域：/proc/net/dev、sysfs、netlink 全拒读 → 空快照为正确行为");
            return;
        }
        println!("dataplane snapshot: {s:?}");
        assert!(
            s.iter().all(|(n, ..)| super::group_key(n).is_some()),
            "快照只应含可入组网卡: {s:?}"
        );
        let mut g = Grouper::default();
        let first = g.step(&s, 1.0);
        let keys: Vec<&str> = first.iter().map(|(k, ..)| k.as_str()).collect();
        assert!(
            keys.iter().all(|k| *k == GROUP_WIFI || *k == GROUP_CELLULAR || super::group_key(k).is_some()),
            "speed 键 ⊆ {{wifi, cellular}} ∪ 非零其他，实际 {keys:?}"
        );
        let mut expect: Vec<&str> = Vec::new();
        if keys.contains(&GROUP_WIFI) {
            expect.push(GROUP_WIFI);
        }
        if keys.contains(&GROUP_CELLULAR) {
            expect.push(GROUP_CELLULAR);
        }
        let mut rest: Vec<&str> = keys
            .iter()
            .copied()
            .filter(|k| *k != GROUP_WIFI && *k != GROUP_CELLULAR)
            .collect();
        rest.sort_unstable();
        expect.extend(rest);
        assert_eq!(keys, expect, "固定序 [wifi, cellular, 其余升序]");
    }
}
