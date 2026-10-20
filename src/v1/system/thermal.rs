//! v1/thermal — 热传感器分组温度采集。
//!
//! # 公开接口用法
//!
//! 对外只有一个结构体 [`Temp`]（唯一公开关联函数 [`Temp::get`]，字段即类别、全部 pub，
//! 每个字段均为 `Vec<(摄氏度, type名)>`，按 zone 升序；本机不存在的类别为空 Vec）：
//!
//! ```ignore
//! // crate 内调用路径：crate::v1::system::thermal
//! let t = crate::v1::system::thermal::Temp::get();      // 一次采集
//!
//! t.cpu_core[0].0;   // f32  每核CPU探头组第0个探头的摄氏度，如 61.4
//! t.cpu_core[0].1;   // String 对应传感器 type 名，如 "cpu-0-0-0"
//!
//! // 全部公开字段（类别 -> 本机实测样本数与 type 示例）：
//! t.cpu_core     // 16 个 ("cpu-0-0-0".."cpu-1-1-1")   每核 CPU 探头
//! t.cpu_cluster  //  4 个 ("cpuss-0-0"..)               CPU 子系统整体
//! t.gpu          //  8 个 ("gpuss-0"..)                 GPU
//! t.dsp          //  7~8个 ("nsphvx-0".."nsphmx-3")     DSP (Hexagon)
//! t.modem        //  4 个 ("mdmss-0"..)                 基带
//! t.camera       //  3 个 ("camera-0","camera-1","video") 摄像头
//! t.aoss         //  4 个 ("aoss-0"..)                  常开子系统
//! t.ddr          //  1 个 ("ddr")                       内存控制器
//! t.pmic         // 10 个 ("pm8550_tz","sys-therm-0","ac"..) PMIC/皮肤温
//! t.battery      //  1 个 ("battery")                   电池
//! t.radio        //  1~2个 ("sdr0")                     射频前端
//! t.other        //  未识别传感器兜底
//!
//! // 惯用读取：某类别最高温（先判空）
//! let hot = t.cpu_core.iter().map(|(c, _)| *c).fold(f32::MIN, f32::max);
//! ```
//!
//! 内部策略（本机实测：68 zones，全部可通过 sysfs 读取）：
//! - 首次采集时扫描 `/sys/class/thermal/thermal_zone*`，读取每个 zone 的 `type`
//!   并按前缀归类（`OnceLock` 只初始化一次），**不写死 zone 号**
//!   （zone 编号跨机型不稳定，例如本机 zone0 实为 `aoss-0` 而非 CPU 温度）；
//! - `*-bcl-lvl*` 为电池电流限制计数器，不是温度，枚举阶段即排除；
//! - 读数 `-273000`（毫度）代表探头未贴装，过滤后不进入样本组。

use serde::Serialize;
use std::fs;
use std::sync::OnceLock;

const THERMAL_ROOT: &str = "/sys/class/thermal";

/// 探头未贴装时内核上报的占位读数（毫摄氏度，即绝对零度）
const INVALID_MILLI: i64 = -273_000;

/// 一次分组温度采集结果（本模块唯一对外类型）。
///
/// 每个字段是一个传感器类别，内容为 `Vec<(摄氏度, type名)>`（按 zone 升序）；
/// 该机型不存在/全部无效的类别为空 Vec。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Temp {
    /// 每核 CPU 探头（type 形如 `cpu-{cluster}-{core}-{probe}`）
    pub cpu_core: Vec<(f32, String)>,
    /// CPU 子系统整体（`cpuss-*`）
    pub cpu_cluster: Vec<(f32, String)>,
    /// GPU（`gpuss-*`）
    pub gpu: Vec<(f32, String)>,
    /// DSP Hexagon（`nsphvx-*` / `nsphmx-*`）
    pub dsp: Vec<(f32, String)>,
    /// 基带（`mdmss-*`）
    pub modem: Vec<(f32, String)>,
    /// 摄像头（`camera-*` / `video`）
    pub camera: Vec<(f32, String)>,
    /// 常开子系统（`aoss-*`）
    pub aoss: Vec<(f32, String)>,
    /// 内存控制器（`ddr`）
    pub ddr: Vec<(f32, String)>,
    /// PMIC/皮肤温（`pm*_tz` / `sys-therm-*` / `ac`）
    pub pmic: Vec<(f32, String)>,
    /// 电池（`battery`）
    pub battery: Vec<(f32, String)>,
    /// 射频前端（`sdr*` / `mmw*`）
    pub radio: Vec<(f32, String)>,
    /// 未识别
    pub other: Vec<(f32, String)>,
}

impl Temp {
    /// 【唯一公开关联函数】执行一次分组温度采集（传感器列表首次调用时初始化并缓存）
    pub fn get() -> Self {
        // 分桶顺序必须与 CATS（即字段声明顺序）一致
        let mut b: [Vec<(f32, String)>; 12] = std::array::from_fn(|_| Vec::new());
        for s in sensors() {
            if let Some(t) = read_celsius(s.zone) {
                b[cat_index(s.category)].push((t, s.name.clone()));
            }
        }
        let mut it = b.into_iter();
        let mut next = move || it.next().unwrap_or_default();
        Self {
            cpu_core: next(),
            cpu_cluster: next(),
            gpu: next(),
            dsp: next(),
            modem: next(),
            camera: next(),
            aoss: next(),
            ddr: next(),
            pmic: next(),
            battery: next(),
            radio: next(),
            other: next(),
        }
    }
}

// ---------------- 以下为内部黑箱 ----------------

/// 内部传感器类别（不对外暴露）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    CpuCore,
    CpuCluster,
    Gpu,
    Dsp,
    Modem,
    Camera,
    Aoss,
    Ddr,
    Pmic,
    Battery,
    Radio,
    Other,
}

/// 类别顺序 == `Temp` 字段声明顺序
const CATS: [Category; 12] = [
    Category::CpuCore,
    Category::CpuCluster,
    Category::Gpu,
    Category::Dsp,
    Category::Modem,
    Category::Camera,
    Category::Aoss,
    Category::Ddr,
    Category::Pmic,
    Category::Battery,
    Category::Radio,
    Category::Other,
];

fn cat_index(cat: Category) -> usize {
    CATS.iter().position(|&c| c == cat).unwrap_or(0)
}

/// 由 zone type 归类；`None` 表示不是温度传感器（如 BCL 计数器），应跳过
fn classify(type_name: &str) -> Option<Category> {
    let n = type_name;
    if n.contains("bcl") {
        return None; // 电池电流限制计数器，非温度
    }
    Some(if n.starts_with("cpu-") {
        Category::CpuCore
    } else if n.starts_with("cpuss") {
        Category::CpuCluster
    } else if n.starts_with("gpuss") {
        Category::Gpu
    } else if n.starts_with("nsphvx") || n.starts_with("nsphmx") {
        Category::Dsp
    } else if n.starts_with("mdmss") {
        Category::Modem
    } else if n.starts_with("camera") || n == "video" {
        Category::Camera
    } else if n.starts_with("aoss") {
        Category::Aoss
    } else if n == "ddr" {
        Category::Ddr
    } else if n == "battery" {
        Category::Battery
    } else if n.starts_with("sdr") || n.starts_with("mmw") {
        Category::Radio
    } else if n.ends_with("_tz") || n.starts_with("pm") || n == "ac" || n.starts_with("sys-therm") {
        Category::Pmic
    } else {
        Category::Other
    })
}

/// 内部传感器静态信息（初始化时确定，运行期不变）
struct Sensor {
    zone: u32,
    name: String,
    category: Category,
}

/// 传感器列表（本机 std 构建无 `sync::OnceCell`，用等价的 `OnceLock`）
static SENSORS: OnceLock<Vec<Sensor>> = OnceLock::new();

/// 惰性获取传感器列表（首次调用扫描 sysfs 并缓存）
fn sensors() -> &'static [Sensor] {
    SENSORS.get_or_init(scan_sensors)
}

/// 初始化时扫描一次：枚举所有 thermal_zone，读取 type 并归类
fn scan_sensors() -> Vec<Sensor> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(THERMAL_ROOT) else {
        return out;
    };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(fname) = fname.to_str() else {
            continue;
        };
        let Some(zone) = fname
            .strip_prefix("thermal_zone")
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(type_name) = fs::read_to_string(format!("{THERMAL_ROOT}/{fname}/type")) else {
            continue;
        };
        let type_name = type_name.trim().to_owned();
        if let Some(category) = classify(&type_name) {
            out.push(Sensor {
                zone,
                name: type_name,
                category,
            });
        }
    }
    out.sort_by_key(|s| (cat_index(s.category), s.zone));
    out
}

/// 读取单个传感器的温度（°C）。探头无效（-273°C 占位）或读取失败返回 `None`。
fn read_celsius(zone: u32) -> Option<f32> {
    let raw = fs::read_to_string(format!("{THERMAL_ROOT}/thermal_zone{zone}/temp"))
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()?;
    if raw == INVALID_MILLI {
        None
    } else {
        Some(raw as f32 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{Category, classify};

    #[test]
    fn classify_real_device_types() {
        // 本机（骁龙8Gen2）实测 type 覆盖
        assert_eq!(classify("cpu-0-3-0"), Some(Category::CpuCore));
        assert_eq!(classify("cpuss-0-0"), Some(Category::CpuCluster));
        assert_eq!(classify("gpuss-4"), Some(Category::Gpu));
        assert_eq!(classify("nsphmx-1"), Some(Category::Dsp));
        assert_eq!(classify("mdmss-2"), Some(Category::Modem));
        assert_eq!(classify("camera-0"), Some(Category::Camera));
        assert_eq!(classify("video"), Some(Category::Camera));
        assert_eq!(classify("aoss-0"), Some(Category::Aoss));
        assert_eq!(classify("ddr"), Some(Category::Ddr));
        assert_eq!(classify("pm8550_tz"), Some(Category::Pmic));
        assert_eq!(classify("sys-therm-0"), Some(Category::Pmic));
        assert_eq!(classify("ac"), Some(Category::Pmic));
        assert_eq!(classify("battery"), Some(Category::Battery));
        assert_eq!(classify("sdr0"), Some(Category::Radio));
        assert_eq!(classify("mmw0"), Some(Category::Radio));
    }

    #[test]
    fn bcl_is_not_a_temperature() {
        assert_eq!(classify("pm8550-bcl-lvl0"), None);
    }
}
