//! v1/battery — 电池状态采集（直读 termux-api CLI，不依赖 api crate）。
//!
//! # 公开接口用法
//!
//! 对外只有一个结构体 [`Battery`]（唯一公开关联函数 [`Battery::get`]，字段全部 pub，
//! 与 CLI JSON 的 14 个键一一对应；CLI 不可用时整体返回默认值，`present == false`）：
//!
//! ```ignore
//! // crate 内调用路径：crate::v1::system::battery
//! let b = crate::v1::system::battery::Battery::get(); // 一次采集（内部 fork 子进程，~1s）
//!
//! b.percentage;      // u8     电量百分比 0-100
//! b.status;          // String 充放电态: DISCHARGING / CHARGING / FULL / NOT_CHARGING / UNKNOWN
//! b.plugged;         // String 电源类型: UNPLUGGED / AC / USB / WIRELESS
//! b.health;          // String 健康枚举: GOOD / OVERHEAT / DEAD / OVER_VOLTAGE / COLD / UNKNOWN
//! b.voltage;         // i64    电池电压 (mV)，本机实测 3769
//! b.current;         // i64    瞬时电流 (µA)，负值 = 放电
//! b.current_average; // i64    滑均电流 (µA)
//! b.charge_counter;  // i64    剩余电荷量 (µAh)
//! b.cycle;           // i64    充电循环计数
//! b.technology;      // String 电池类型，如 "Li-ion"
//! b.level; b.scale;  // i64    原始电量/量程（与 percentage 冗余）
//! b.present;         // bool   电池在位（CLI 失败时唯一可信判据：false）
//!
//! // 惯用派生值（本模块只给原始字段，派生自行计算）：
//! let watt = b.voltage as f64 * b.current as f64 / 1e9;                  // 瞬时功率 W（mV×µA）
//! let hrs  = b.charge_counter as f64 / b.current.unsigned_abs() as f64;  // 剩余续航 h（µAh/µA）
//! ```
//!
//! 通道说明（弃用 `termuxapi` crate 的原因：该 crate 仅解析 6/14 字段）：
//! - fork `/data/data/com.termux/files/usr/libexec/termux-api BatteryStatus`
//!   → Termux:API App 桥接 Android `BatteryManager`（sticky broadcast）→ JSON；
//! - [`Battery::get`] 外套 **10s TTL 缓存**（审计 sec F-06：1/s 请求不再逐次 fork）：
//!   新鲜窗内回放缓存、零 fork；过期才真同步 fork CLI（实测 ~0.8s，App 冷启更久），
//!   调用仍属阻塞面，用 tokio 时放 `spawn_blocking`；
//! - 温度字段 `Battery::temperature` 已弃用：电池温度统一走
//!   `crate::v1::system::thermal::Temp::get().battery`（sysfs 同源实测一致，滞后 ≤ 其 10s
//!   新鲜窗，对比本通道 CLI sticky broadcast 的不可控滞后）。

use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// CLI 可执行文件（Termux 固定路径，等价于 shell 命令 `termux-battery-status`）
const API_BIN: &str = "/data/data/com.termux/files/usr/libexec/termux-api";

/// 一次电池状态采集结果（本模块唯一对外类型；字段与 CLI JSON 键一一对应）。
#[allow(deprecated)] // 覆盖 serde 派生代码内部对本结构字段的使用，仅外部读取时告警
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Battery {
    /// 电池在位；CLI 失败/JSON 解析失败时为 `false`（唯一可信的失败判据）
    pub present: bool,
    /// 电池化学类型，如 `"Li-ion"`
    pub technology: String,
    /// 健康枚举：`GOOD` / `OVERHEAT` / `DEAD` / `OVER_VOLTAGE` / `COLD` / `UNKNOWN`
    pub health: String,
    /// 充电电源类型：`UNPLUGGED` / `AC` / `USB` / `WIRELESS`
    pub plugged: String,
    /// 充放电状态：`DISCHARGING` / `CHARGING` / `FULL` / `NOT_CHARGING` / `UNKNOWN`
    pub status: String,
    /// 电量百分比 0–100
    pub percentage: u8,
    /// 原始电量（与 `percentage` 冗余，Android BatteryManager 原生字段）
    pub level: i64,
    /// 原始量程（通常为 100）
    pub scale: i64,
    /// 电池电压（mV），本机实测 3769
    pub voltage: i64,
    /// 瞬时电流（µA），**负值 = 放电**
    pub current: i64,
    /// 滑动平均电流（µA），比 `current` 平滑
    pub current_average: i64,
    /// 剩余电荷量（µAh），除以 `current` 绝对值即剩余续航小时数
    pub charge_counter: i64,
    /// 充电循环计数
    pub cycle: i64,
    /// 【已弃用】API 通道的电池温度（°C，sticky broadcast 缓存、可能滞后）。
    /// 改用 `crate::v1::system::thermal::Temp::get().battery`（sysfs 同源、实时）。
    #[deprecated = "电池温度改用 crate::v1::system::thermal::Temp::get().battery（sysfs 同源，滞后 ≤ 其 10s 新鲜窗）"]
    pub temperature: f64,
}

impl Battery {
    /// 【唯一公开关联函数】带 10s TTL 缓存的电池状态采集（审计 sec F-06/perf：
    /// `/system/cli/battery` 1/s 请求不再逐次 fork CLI）。新鲜窗内直接回放快照；
    /// 过期才真 fork 一轮（[`collect`]，实测 ~0.8s，App 冷启更久），并发迟到者经
    /// 单飞锁并入同轮。异步环境下调用仍属阻塞面，用 `tokio::task::spawn_blocking`
    /// 包裹。CLI 缺失、非零退出或 JSON 解析失败时缓存的即是全默认缺席值。
    pub fn get() -> Self {
        snapshot(Instant::now())
    }
}

// ---------------- 10s TTL 缓存（审计 sec F-06：CLI fork 风暴收敛） ----------------

/// 新鲜窗：窗内回放缓存不再 fork；age 恰等于 TTL 即过期（TTL 到期必重采）。
const CACHE_TTL: Duration = Duration::from_secs(10);

/// 进程级快照格（计划钉死形态 `OnceLock<Mutex<Option<(Instant, Snapshot)>>>`）。
/// 单飞（single-flight）：锁跨 [`collect`] 持有——并发迟到者阻塞至在途 CLI 返回，
/// 随即并入其新鲜缓存，绝不二次 fork；collect 中途 panic 只毒锁不损缓存本体
/// （写回是整体赋值、发生在 collect 返回后），`into_inner` 回收即用旧缓存，过期再采。
static CACHE: OnceLock<Mutex<Option<(Instant, Battery)>>> = OnceLock::new();

/// 采集计数（测试缝，生产构建不挂载）：冷启动区域必恰好 +1；TTL 到期再 +1。
#[cfg(test)]
static COLLECTS: AtomicUsize = AtomicUsize::new(0);

/// 生产绑定：进程级格 + 真采集体 + 墙钟。
fn snapshot(now: Instant) -> Battery {
    snapshot_in(&CACHE, now, collect)
}

/// 时钟与格位可注入的缓存骨架（测试缝：局部格 + 注入 Instant + 计数闭包，
/// 免睡 10s、免与并行测试串扰）。新鲜判据 age < TTL，age 恰等于 TTL 即过期；
/// 时钟倒走/并发先写者的微超前列（checked 取 None）按新鲜消化：回放旧缓存
/// 等窗口自然到期，宁供稍陈数据也不 panic、不重复 fork。
fn snapshot_in(
    cell: &OnceLock<Mutex<Option<(Instant, Battery)>>>,
    now: Instant,
    collect: impl FnOnce() -> Battery,
) -> Battery {
    let cell = cell.get_or_init(Mutex::default);
    let mut cache = cell.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, snap)) = cache.as_ref() {
        let fresh = match now.checked_duration_since(*at) {
            Some(age) => age < CACHE_TTL,
            None => true,
        };
        if fresh {
            return snap.clone();
        }
    }
    let snap = collect();
    *cache = Some((now, snap.clone()));
    snap
}

/// 真采集体（原 `Battery::get` 正文）：同步 fork termux-api CLI 一次。
fn collect() -> Battery {
    #[cfg(test)]
    COLLECTS.fetch_add(1, Ordering::Relaxed);
    fetch_json()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 内部：执行 CLI 并取回 stdout JSON 文本（非 Android 平台路径不存在，自然返回 None）
fn fetch_json() -> Option<String> {
    let out = std::process::Command::new(API_BIN)
        .arg("BatteryStatus")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{CACHE_TTL, COLLECTS, Battery, snapshot_in};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use std::time::{Duration, Instant};

    /// 触碰进程级 TTL 格/全局采集计数的测试共锁（hermetic 的 ttl_cache_* 用局部格）。
    fn cache_guard() -> MutexGuard<'static, ()> {
        static L: Mutex<()> = Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fake_battery(mark: u8) -> Battery {
        Battery {
            present: true,
            percentage: mark,
            ..Default::default()
        }
    }

    /// TTL 缓存契约（计数精确钉死；红-先：改动前无缓存、1/s 请求逐次 fork，两次
    /// 调用 counter==2）：冷启动恰采一次；TTL 窗内 4 连呼零新增且逐字回放；
    /// age==TTL 边界即过期重采并顶替旧快照；重采写回即成新窗。注入时钟驱动过期。
    #[test]
    fn ttl_cache_fresh_hit_and_expiry_recollect_exact_counts() {
        let cell: OnceLock<Mutex<Option<(Instant, Battery)>>> = OnceLock::new();
        let n = AtomicUsize::new(0);
        let t0 = Instant::now();
        let first = snapshot_in(&cell, t0, || {
            n.fetch_add(1, Ordering::Relaxed);
            fake_battery(11)
        });
        assert_eq!(n.load(Ordering::Relaxed), 1, "冷启动必恰采一次（= 唯一一次 fork）");
        let want = format!("{first:?}");
        for k in 1..=4u32 {
            let hit = snapshot_in(&cell, t0 + Duration::from_millis(250 * u64::from(k)), || {
                n.fetch_add(1, Ordering::Relaxed);
                fake_battery(22)
            });
            assert_eq!(format!("{hit:?}"), want, "窗内命中须逐字回放缓存（零 fork）");
        }
        assert_eq!(n.load(Ordering::Relaxed), 1, "窗内 4 连呼零新增采集（counter==1 红线）");
        let after = snapshot_in(&cell, t0 + CACHE_TTL, || {
            n.fetch_add(1, Ordering::Relaxed);
            fake_battery(33)
        });
        assert_eq!(n.load(Ordering::Relaxed), 2, "TTL 到期（age==TTL）必重采（重 fork CLI）");
        assert_eq!(after.percentage, 33, "过期后新采顶格");
        let replay = snapshot_in(&cell, t0 + CACHE_TTL + Duration::from_millis(1), || {
            n.fetch_add(1, Ordering::Relaxed);
            fake_battery(44)
        });
        assert_eq!(replay.percentage, 33, "新窗回放新值");
        assert_eq!(n.load(Ordering::Relaxed), 2, "全程累计仅两次真 fork");
    }

    /// 生产接线钉：Battery::get() 必走全局 TTL 格。首呼后格必新鲜（10s 窗 ≫ CLI
    /// 一轮 + 测试微秒窗，并发他方只可能命中），第二呼必零 fork 且逐字回放。
    #[test]
    fn production_get_serves_next_call_from_ttl_cache() {
        let _g = cache_guard();
        let first = Battery::get();
        let base = COLLECTS.load(Ordering::Relaxed);
        let again = Battery::get();
        assert_eq!(COLLECTS.load(Ordering::Relaxed), base, "新鲜窗内第二呼零 fork 零采集");
        assert_eq!(format!("{again:?}"), format!("{first:?}"), "回放内容逐字一致");
    }

    /// 本机真机抓取的完整 JSON（14 键全量回归样本）
    const FIXTURE: &str = r#"{
      "present": true, "technology": "Li-ion", "health": "GOOD",
      "plugged": "UNPLUGGED", "status": "DISCHARGING",
      "temperature": 35.3, "voltage": 3769,
      "current": -704687, "current_average": -650000,
      "percentage": 44, "level": 44, "scale": 100,
      "charge_counter": 2191625, "cycle": 0
    }"#;

    #[test]
    fn parse_full_fixture() {
        let b: Battery = serde_json::from_str(FIXTURE).unwrap();
        assert!(b.present);
        assert_eq!((b.percentage, b.voltage, b.current), (44, 3769, -704687));
        assert_eq!(b.status, "DISCHARGING");
        assert_eq!(b.plugged, "UNPLUGGED");
        assert_eq!((b.charge_counter, b.cycle), (2191625, 0));
    }

    #[test]
    fn missing_keys_default_gracefully() {
        let b: Battery = serde_json::from_str(r#"{"percentage": 88}"#).unwrap();
        assert_eq!(b.percentage, 88);
        assert!(!b.present); // 缺键走 serde(default)，不炸
        assert_eq!(b.voltage, 0);
    }

    #[test]
    fn garbage_json_falls_back() {
        // get() 的解析失败路径等价于默认值（CLI 不存在时同路径）
        let b: Battery = serde_json::from_str("not json").unwrap_or_default();
        assert!(!b.present);
        assert_eq!(b.percentage, 0);
    }

    /// 真机冒烟测试（需要 termux-api），默认跳过，用 `-- --ignored` 运行
    #[test]
    #[ignore = "需真机 termux-api：cargo test v1::battery -- --ignored"]
    #[allow(deprecated)]
    fn live_get() {
        let b = Battery::get();
        let watt = b.voltage as f64 * b.current as f64 / 1e9;
        println!(
            "present={} pct={}% status={} plugged={} health={}",
            b.present, b.percentage, b.status, b.plugged, b.health
        );
        println!(
            "v={}mV cur={}uA avg={}uA charge={}uAh cycle={} api_temp={}\u{b0}C  P={watt:.2}W",
            b.voltage, b.current, b.current_average, b.charge_counter, b.cycle, b.temperature
        );
        assert!(b.present, "CLI 采集失败");
        assert!((1..=100).contains(&b.percentage));
        assert!(
            b.voltage > 3000 && b.voltage < 5000,
            "电压应处 3.0-5.0V 区间"
        );
    }
}
