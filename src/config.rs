use directories::ProjectDirs;
use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type SharedConfig = Arc<RwLock<Config>>;
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
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
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
    pub fn next(self) -> Self {
        match self {
            AppColor::Black => AppColor::Red,
            AppColor::Red => AppColor::Green,
            AppColor::Green => AppColor::Yellow,
            AppColor::Yellow => AppColor::Blue,
            AppColor::Blue => AppColor::Magenta,
            AppColor::Magenta => AppColor::Cyan,
            AppColor::Cyan => AppColor::White,
            AppColor::White => AppColor::Black,
        }
    }
}

impl std::fmt::Display for AppColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    pub background_color: AppColor,
    pub theme_color: AppColor,
    pub refresh_rate_ms: u64,
    pub cpu_affinity: Option<usize>,

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
impl Default for Config {
    fn default() -> Self {
        Self {
            background_color: AppColor::Black,
            theme_color: AppColor::White,
            refresh_rate_ms: 8,
            cpu_affinity: None,
            extra: Default::default(),
        }
    }
}

impl Config {
    fn _get_path() -> Option<PathBuf> {
        ProjectDirs::from("", "", "atlas").map(|p| p.data_dir().join("config.json"))
    }
    fn get_path() -> Option<PathBuf> {
        ProjectDirs::from("", "", "atlas").map(|p| {
            let data_dir = p.data_dir().to_path_buf();
            // 确保数据目录存在（如果不存在则创建）
            let _ = fs::create_dir_all(&data_dir);
            data_dir.join("config.json")
        })
    }

    /// 处理损坏的配置文件：重命名为 broken_xxxx.json 并新建默认文件
    fn handle_broken_config(path: &PathBuf) {
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

    pub fn load() -> Self {
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

    pub fn _load3() -> Self {
        let path = match Self::get_path() {
            Some(p) => p,
            None => return Self::default(),
        };

        if !path.exists() {
            let def = Self::default();
            let _ = def.save();
            return def;
        }

        let content = fs::read_to_string(&path).unwrap_or_default();

        // 如果文件为空，直接返回默认
        if content.trim().is_empty() {
            return Self::default();
        }

        match serde_json::from_str::<Self>(&content) {
            Ok(config) => config,
            Err(e) => {
                // 记录错误，通过打印或日志，在 TUI 启动前可见
                eprintln!("Config parse error: {}", e);

                // 执行备份
                if let Err(rename_err) = Self::backup_and_recreate(&path) {
                    eprintln!("Failed to backup config: {}", rename_err);
                }

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
    pub fn _load() -> Self {
        Self::get_path()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default()
    }
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(path) = Self::get_path() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, serde_json::to_string_pretty(self)?)?;
        }
        Ok(())
    }
}

use notify::{RecursiveMode, Watcher, event};
use std::path::Path;

use crate::message::{GlobalEvent, NotificationLevel};

pub async fn setup_config_watcher(
    shared_config: SharedConfig,
    render_tx: tokio::sync::mpsc::Sender<()>,
    event_tx: tokio::sync::broadcast::Sender<GlobalEvent>,
) {
    tokio::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        // 创建文件监听器
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(e) = res {
                if e.kind.is_modify() {
                    let _ = tx.blocking_send(());
                }
            }
        })
        .expect("Failed to create watcher");

        watcher
            .watch(Path::new("config.toml"), RecursiveMode::NonRecursive)
            .ok();

        while let Some(_) = rx.recv().await {
            // 1. 读取新配置
            let new_conf = Config::load();

            // 2. 写入共享内存
            {
                let mut guard = shared_config.write().await;
                *guard = new_conf;
                //println!("Config hot-reloaded!");

                let _ = event_tx.send(GlobalEvent::Notify(
                    "Config Hot-Reloaded".into(),
                    NotificationLevel::Info,
                ));
                let _ = render_tx.send(()).await;
            }

            // 3. 强制触发全局重绘
            let _ = render_tx.send(()).await;
            // 发送广播消息，通知 App 数据已变
            //let _ = global_tx.send(GlobalEvent::ConfigChanged); ?
        }
    });
}
