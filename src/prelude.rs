// use crate::prelude::*; // for any module if needed

use directories::{BaseDirs, ProjectDirs, UserDirs};
use tokio::sync::{RwLock, broadcast};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::{env, fs};

use crate::config::Config;
use crate::message::GlobalEvent;

pub static ATLAS_PATHS: OnceLock<AtlasPath> = OnceLock::new();

#[derive(Debug)]
pub struct AtlasPath {
    // 程序基础路径
    pub exe_dir: PathBuf,
    pub current_dir: PathBuf,

    // Project Dirs (基于 atlas 名称)
    // pub config_file: PathBuf,
    pub proj_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: Option<PathBuf>,
    pub preference_dir: PathBuf,

    // Base Dirs 展开
    pub home_dir: PathBuf,
    pub base_config_dir: PathBuf,
    pub base_data_dir: PathBuf,
    pub base_cache_dir: PathBuf,
    pub runtime_dir: Option<PathBuf>,

    // User Dirs 展开
    pub desktop: Option<PathBuf>,
    pub document: Option<PathBuf>,
    pub download: Option<PathBuf>,
    pub audio: Option<PathBuf>,
    pub picture: Option<PathBuf>,
    pub video: Option<PathBuf>,
    pub public: Option<PathBuf>,
    pub font: Option<PathBuf>,
    pub template: Option<PathBuf>,

    // 业务专用路径
    // pub script_dir: PathBuf,
    // pub db_dir: PathBuf,
}

impl AtlasPath {
    // run at main()
    pub fn init() {
        ATLAS_PATHS.get_or_init(|| {
            let exe_path = env::current_exe().unwrap_or_default();
            let exe_dir = exe_path.parent().unwrap_or(&exe_path).to_path_buf();
            let current_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

            let proj = ProjectDirs::from("org", "oelabs", "atlas")
                .expect("Failed to get project directories");
            let base = BaseDirs::new().expect("Failed to get base directories");
            let user = UserDirs::new().expect("Failed to get user directories");

            // 业务路径：脚本存放在 Home/script，数据库存放在 Data/db
            let script_dir = base.home_dir().join("script");
            let db_dir = proj.data_dir().join("db");

            // 自动创建核心业务目录
            let _ = fs::create_dir_all(&script_dir);
            let _ = fs::create_dir_all(&db_dir);
            let _ = fs::create_dir_all(proj.config_dir());

            Self {
                exe_dir,
                current_dir,

                // config_file: proj.config_dir().join("atlas_cfg.json"),
                proj_dir: proj.data_dir().to_path_buf(),
                cache_dir: proj.cache_dir().to_path_buf(),
                state_dir: proj.state_dir().map(|p| p.to_path_buf()),
                preference_dir: proj.preference_dir().to_path_buf(),

                home_dir: base.home_dir().to_path_buf(),
                base_config_dir: base.config_dir().to_path_buf(),
                base_data_dir: base.data_dir().to_path_buf(),
                base_cache_dir: base.cache_dir().to_path_buf(),
                runtime_dir: base.runtime_dir().map(|p| p.to_path_buf()),

                desktop: user.desktop_dir().map(|p| p.to_path_buf()),
                document: user.document_dir().map(|p| p.to_path_buf()),
                download: user.download_dir().map(|p| p.to_path_buf()),
                audio: user.audio_dir().map(|p| p.to_path_buf()),
                picture: user.picture_dir().map(|p| p.to_path_buf()),
                video: user.video_dir().map(|p| p.to_path_buf()),
                public: user.public_dir().map(|p| p.to_path_buf()),
                font: user.font_dir().map(|p| p.to_path_buf()),
                template: user.template_dir().map(|p| p.to_path_buf()),
            }
        });
    }

    pub fn get() -> &'static AtlasPath {
        ATLAS_PATHS.get().expect("AtlasPath not initialized! Call init() in main.")
    }

    /// 获取配置数据文件路径 (支持 override 检查)
    pub fn get_config_path() -> PathBuf {
        let p = Self::get();
        let override_path = p.exe_dir.join("atlas_cfg_override.json");
        if override_path.exists() {
            override_path
        } else {
            // 注意：这里使用 base_config_dir 可能更符合 ProjectDirs 逻辑
            let path = p.base_config_dir.join("atlas/atlas_cfg.json");
            if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
            path
        }
    }

    /// 获取脚本存放目录 (关联函数)
    pub fn get_script_dir() -> PathBuf {
        let p = Self::get();
        let path = p.home_dir.join("script");
        let _ = fs::create_dir_all(&path);
        path
    }

    /// 获取数据库存放目录 (关联函数)
    pub fn get_db_dir() -> PathBuf {
        let p = Self::get();
        let path = p.proj_dir.join("db");
        let _ = fs::create_dir_all(&path);
        path
    }

    pub fn collect_dirs() -> Vec<String> {
        let p = Self::get();
        let mut list = Vec::new();

        // 1. 程序基础环境
        list.push("--- [ Runtime Context ] ---".to_string());
        list.push(format!("Executable Dir: {:?}", p.exe_dir));
        list.push(format!("Working Dir:    {:?}", p.current_dir));

        // 2. 项目标准路径 (ProjectDirs)
        list.push("\n--- [ Project Standard Dirs ] ---".to_string());
        list.push(format!("Data Root:   {:?}", p.proj_dir));
        list.push(format!("Cache Root:  {:?}", p.cache_dir));
        list.push(format!("Preferences: {:?}", p.preference_dir));
        if let Some(state) = &p.state_dir {
            list.push(format!("State Root:  {:?}", state));
        }

        // 3. 基础系统路径 (BaseDirs)
        list.push("\n--- [ System Base Dirs ] ---".to_string());
        list.push(format!("Home:        {:?}", p.home_dir));
        list.push(format!("Base Config: {:?}", p.base_config_dir));
        list.push(format!("Base Data:   {:?}", p.base_data_dir));
        list.push(format!("Base Cache:  {:?}", p.base_cache_dir));
        if let Some(runtime) = &p.runtime_dir {
            list.push(format!("Runtime:     {:?}", runtime));
        }

        // 4. 业务逻辑生成的路径 (Dynamic Paths)
        list.push("\n--- [ Resolved Business Paths ] ---".to_string());
        list.push(format!("Config File: {:?}", Self::get_config_path()));
        list.push(format!("Scripts Dir: {:?}", Self::get_script_dir()));
        list.push(format!("Database Dir: {:?}", Self::get_db_dir()));

        // 5. 用户常用目录 (UserDirs - 筛选展示)
        list.push("\n--- [ User Content Dirs ] ---".to_string());
        if let Some(d) = &p.download { list.push(format!("Downloads: {:?}", d)); }
        if let Some(d) = &p.document { list.push(format!("Documents: {:?}", d)); }
        if let Some(d) = &p.desktop  { list.push(format!("Desktop:   {:?}", d)); }
        if let Some(d) = &p.picture  { list.push(format!("Pictures:  {:?}", d)); }
        if let Some(d) = &p.video    { list.push(format!("Videos:    {:?}", d)); }
        if let Some(d) = &p.audio    { list.push(format!("Audios:    {:?}", d)); }
        if let Some(d) = &p.public    { list.push(format!("Public:   {:?}", d)); }
        if let Some(d) = &p.font    { list.push(format!("Fonts:     {:?}", d)); }
        if let Some(d) = &p.template    { list.push(format!("Template:  {:?}", d)); }

        list
    }


}





pub type GlobSend = broadcast::Sender<GlobalEvent>;
pub type GlobRecv = broadcast::Receiver<GlobalEvent>;

pub struct GlobIO;

// 全局静态实例
static GLOB_SENDER: OnceLock<GlobSend> = OnceLock::new();

impl GlobIO {
    /// 初始化通信总线，需在 main 启动早期调用
    /// capacity: 缓冲区大小，例如 100
    pub fn init() {
        GLOB_SENDER.get_or_init(|| {
            let (tx, _) = broadcast::channel(1024);
            tx
        });
    }

    /// 获取发送端句柄 (Clone 是廉价的)
    pub fn send() -> GlobSend {
        GLOB_SENDER
            .get()
            .expect("GlobIO 尚未初始化! 请在 main 中先调用 GlobIO::init()")
            .clone()
    }

    /// 获取一个新的接收端
    pub fn recv() -> GlobRecv {
        GLOB_SENDER
            .get()
            .expect("GlobIO 尚未初始化!")
            .subscribe()
    }
}







pub type SharedConfig = Arc<RwLock<Config>>;

static GLOBAL_CONFIG: OnceLock<SharedConfig> = OnceLock::new();

impl Config {
    /// 初始化全局配置
    pub fn init() {
        GLOBAL_CONFIG.get_or_init(|| {
            let path = AtlasPath::get_config_path();
            
            Arc::new(RwLock::new(Config::load_from_disk()))
        });
    }

    /// 获取全局配置句柄
    pub fn get() -> SharedConfig {
        GLOBAL_CONFIG
            .get()
            .expect("Config 尚未初始化! 请在 main 中调用 Config::init()")
            .clone()
    }

    /// 便捷方法：保存当前配置到磁盘
    pub async fn save_global() -> std::io::Result<()> {
        if let Some(cfg_lock) = GLOBAL_CONFIG.get() {
            let cfg = cfg_lock.read().await;//.unwrap();
            cfg.save()?;
            // let path = AtlasPath::get_config_path();
            // let content = serde_json::to_string_pretty(&*cfg).unwrap();
            // fs::write(path, content)?;
        }
        Ok(())
    }
}








