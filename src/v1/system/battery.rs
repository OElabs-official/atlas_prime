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
//! - [`Battery::get`] 为**同步子进程调用（实测 ~0.8s，App 冷启可能更久）**：
//!   禁止放进每 tick 的 UI 热路径；用 tokio 时放 `spawn_blocking` 或维持 60s 长周期；
//! - 温度字段 `Battery::temperature` 已弃用：电池温度统一走
//!   `crate::v1::system::thermal::Temp::get().battery`（sysfs 同源实测一致、实时、无缓存滞后）。

use serde::{Deserialize, Serialize};

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
    #[deprecated = "电池温度改用 crate::v1::system::thermal::Temp::get().battery（sysfs 同源、实时、无缓存滞后）"]
    pub temperature: f64,
}

impl Battery {
    /// 【唯一公开关联函数】执行一次电池状态采集。
    ///
    /// 同步 fork CLI 子进程（实测 ~0.8s，Termux:API App 冷启时更久）：
    /// 勿放进每 tick 的 UI 热路径；异步环境下用 `tokio::task::spawn_blocking` 包裹。
    /// CLI 缺失、非零退出或 JSON 解析失败时返回全默认值（`present == false`）。
    pub fn get() -> Self {
        Self::fetch_json()
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
}

#[cfg(test)]
mod tests {
    use super::Battery;

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
