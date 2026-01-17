use directories::ProjectDirs;
use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use smart_default::SmartDefault;
use std::fs;
use tokio::sync::RwLock;

use std::sync::{Arc, OnceLock};

use std::env;
use std::path::{Path, PathBuf};

pub type SharedConfig = Arc<RwLock<Config>>;

// --- 全局静态路径句柄 ---
static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
static CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

pub struct AtlasPaths;

impl AtlasPaths {
    /// 初始化并获取路径映射
    fn init() {
        // 1. 确定配置路径 (优先检查当前程序目录下的 override)
        let exe_dir = env::current_exe()
            .map(|p| p.parent().unwrap().to_path_buf())
            .unwrap_or_else(|_| PathBuf::from("."));

        let override_path = exe_dir.join("atlas_cfg_override.json");

        let final_config_path = if override_path.exists() {
            override_path
        } else {
            // 回退到系统标准路径
            ProjectDirs::from("", "", "atlas")
                .map(|p| {
                    let dir = p.config_dir().to_path_buf();
                    let _ = fs::create_dir_all(&dir);
                    dir.join("atlas_cfg.json")
                })
                .unwrap_or_else(|| PathBuf::from("atlas_cfg.json"))
        };

        CONFIG_PATH.get_or_init(|| final_config_path);

        // 2. 初始化数据和缓存目录
        if let Some(proj) = ProjectDirs::from("", "", "atlas") {
            DATA_DIR.get_or_init(|| {
                let path = proj.data_dir().to_path_buf();
                let _ = fs::create_dir_all(&path);
                path
            });
            CACHE_DIR.get_or_init(|| {
                let path = proj.cache_dir().to_path_buf();
                let _ = fs::create_dir_all(&path);
                path
            });
        }
    }

    pub fn config() -> &'static PathBuf {
        CONFIG_PATH.get_or_init(|| PathBuf::from("atlas_cfg.json"))
    }
    pub fn data() -> &'static PathBuf {
        DATA_DIR.get_or_init(|| PathBuf::from("./data"))
    }
    pub fn cache() -> &'static PathBuf {
        CACHE_DIR.get_or_init(|| PathBuf::from("./cache"))
    }
}

/*
// 在 render 时获取只读锁
fn render(&mut self, f: &mut Frame, area: Rect) {
    // 注意：TUI 渲染是同步的，建议在这里使用 try_read()
    // 或者在 update() 时将需要的值提取到组件局部变量中
    if let Ok(conf) = self.config.try_read() {
        let theme = conf.theme_color;
        // ...
    }
}
*/
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)] //处于序列化需要重新包装一层
pub enum AppColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
}

impl AppColor {
    pub fn to_ratatui_color(self) -> Color {
        match self {
            AppColor::Black => Color::Black,
            AppColor::Red => Color::Red,
            AppColor::Green => Color::Green,
            AppColor::Yellow => Color::Yellow,
            AppColor::Blue => Color::Blue,
            AppColor::Magenta => Color::Magenta,
            AppColor::Cyan => Color::Cyan,
            AppColor::White => Color::White,
        }
    }
    // pub fn next(self) -> Self {
    //     match self {
    //         AppColor::Black => AppColor::Red,
    //         AppColor::Red => AppColor::Green,
    //         AppColor::Green => AppColor::Yellow,
    //         AppColor::Yellow => AppColor::Blue,
    //         AppColor::Blue => AppColor::Magenta,
    //         AppColor::Magenta => AppColor::Cyan,
    //         AppColor::Cyan => AppColor::White,
    //         AppColor::White => AppColor::Black,
    //     }
    // }
}

impl std::fmt::Display for AppColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, SmartDefault)]
pub struct Config {
    #[default(AppColor::Black)]
    pub background_color: AppColor,
    #[default(AppColor::White)]
    pub theme_color: AppColor,
    #[default(8)]
    pub refresh_rate_ms: u64,
    //pub cpu_affinity: Option<usize>,

    // 扩展参数（弱类型，用于存储动态增加或插件化的配置）
    #[serde(flatten)] // 这个宏会让所有未定义的字段都落入这个 Map
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}
/*
我们的程序随着版本更新，config可能会发生修改
最优雅的 Rust 处理方案是利用 serde 的属性宏实现版本化平滑升级。
如果你的配置只是增加了字段、重命名了字段，或者某些字段变成了可选，可以通过 serde 的默认值和别名来处理。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub app_name: String,

    // 1. 如果旧文件没这个字段，自动调用 default_refresh_rate()
    #[serde(default = "default_refresh_rate")]
    pub refresh_rate: u64,

    // 2. 如果旧字段叫 "color"，新版改名 "theme_color"
    #[serde(alias = "color")]
    pub theme_color: String,
}

fn default_refresh_rate() -> u64 { 30 }

3. 方案 C：HashMap 兜底（动态兼容）

你之前提到的 HashMap 此时派上了大用场。通过 #[serde(flatten)]，所有无法识别的旧字段都会被塞进 extra 中，程序运行期间可以尝试从 extra 里捞数据。
*/

impl Config {
    fn get_path() -> Option<PathBuf> {
        ProjectDirs::from("", "", "atlas").map(|p| {
            let data_dir = p.data_dir().to_path_buf();
            // 确保数据目录存在（如果不存在则创建）
            let _ = fs::create_dir_all(&data_dir);
            data_dir.join("config.json")
        })
    }

    /// 损坏处理：备份并重置
    fn handle_broken_config(path: &PathBuf) {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mut broken_path = path.clone();
        broken_path.set_extension(format!("{}.broken", timestamp));

        let _ = fs::rename(path, broken_path);

        let default_config = Self::default();
        let _ = default_config.save();
    }

    /// 处理损坏的配置文件：重命名为 broken_xxxx.json 并新建默认文件
    fn _handle_broken_config(path: &PathBuf) {
        let mut broken_path = path.clone();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        broken_path.set_file_name(format!("broken_config_{}.json", timestamp));

        // 重命名旧文件
        let _ = fs::rename(path, broken_path);

        // 创建新的默认文件
        let default_config = Self::default();
        let _ = default_config.save();
    }

    /// 核心加载逻辑：Override > System > Default
    pub fn load() -> Self {
        // 确保路径已初始化
        AtlasPaths::init();
        let path = AtlasPaths::config();

        // 1. 尝试读取
        if !path.exists() {
            let default_cfg = Self::default();
            let _ = default_cfg.save();
            return default_cfg;
        }

        match fs::read_to_string(path) {
            Ok(content) => {
                match serde_json::from_str::<Self>(&content) {
                    Ok(mut config) => {
                        // 可以在这里对加载后的配置做一些运行时校验
                        config
                    }
                    Err(e) => {
                        // 只有非 override 文件损坏才尝试修复（防止破坏用户的 override 手动配置）
                        if !path.to_string_lossy().contains("override") {
                            Self::handle_broken_config(path);
                        }
                        eprintln!("Config error at {:?}: {}", path, e);
                        Self::default()
                    }
                }
            }
            Err(_) => Self::default(),
        }
    }

    pub fn _load() -> Self {
        //
        let path = match Self::get_path() {
            Some(p) => p,
            None => return Self::default(),
        };

        // 1. 如果文件不存在，直接初始化默认配置并保存
        if !path.exists() {
            let default_config = Self::default();
            let _ = default_config.save(); // 我们稍后实现 save
            return default_config;
        }

        // 2. 尝试读取并解析
        match fs::read_to_string(&path) {
            Ok(content) => {
                match serde_json::from_str::<Self>(&content) {
                    Ok(config) => config,
                    Err(e) => {
                        // 解析失败：配置文件损坏
                        Self::handle_broken_config(&path);
                        eprintln!("Config parse error: {}", e);
                        Self::default()
                    }
                }
            }
            Err(_) => {
                // 读取文件失败（可能是权限问题）
                Self::default()
            }
        }
    }

    fn backup_and_recreate(path: &PathBuf) -> std::io::Result<()> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut broken_path = path.clone();
        broken_path.set_file_name(format!("broken_config_{}.json", timestamp));

        // 核心修改：确保先重命名
        fs::rename(path, &broken_path)?;

        // 只有重命名成功后，才写入新的默认配置
        let default_config = Self::default();
        let content = serde_json::to_string_pretty(&default_config).unwrap();
        fs::write(path, content)?;

        Ok(())
    }
    pub fn save(&self) -> std::io::Result<()> {
        let path = AtlasPaths::config();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        fs::write(path, content)
    }

    pub fn _save(&self) -> std::io::Result<()> {
        if let Some(path) = Self::get_path() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, serde_json::to_string_pretty(self)?)?;
        }
        Ok(())
    }
}
