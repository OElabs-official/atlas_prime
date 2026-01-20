use crossterm::event::KeyCode;
use directories::{ProjectDirs, UserDirs};
use ratatui::style::Color;

use ratatui::layout::Constraint;
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, RocksDb};

use crate::app::GlobSend;
use crate::config::SharedConfig;
use crate::ui::component::Component;
use crate::ui::info::InfoComponent;
use crate::ui::task_control::TaskControlComponent;
use crate::ui::welcome::WelcomeComponent;


/// 2. 标签页唯一标识
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabId {
    Welcome,
    Info,
    TaskControl, // Sessions,
}

/// 3. 页面注册信息
impl TabId {
    /// 获取所有标签页的顺序列表
    pub const ALL: &[Self] = &[
        // Self::Info,
        Self::Welcome,
        Self::TaskControl,
        Self::Info,
        // Self::Sessions
    ];

    /// 对应的显示标题
    pub fn title(&self) -> &'static str {
        match self {
            Self::Welcome => "  Welcome ",
            Self::Info => "  System Info ",
            Self::TaskControl => " Task Control ",
            // Self::Sessions => " [2] Session Manager ",
        }
    }

    pub fn init() -> Vec<Box<dyn Component>> {
        let mut output = vec![];
        for id in TabId::ALL.iter() {
            let comp = id.gen_component();
            output.push(comp);
        }
        output
    }
    fn gen_component(&self) -> Box<dyn Component> {
        match self {
            Self::Welcome => Box::new(WelcomeComponent::init()),
            Self::Info => Box::new(InfoComponent::init()),
            Self::TaskControl => Box::new(TaskControlComponent::init()),
            // Self::Sessions => " [2] Session Manager ",
        }
    }
    /// 页面对应的主色调（可选，用于联动状态栏）
    // pub fn theme_color(&self) -> Color {
    //     match self {
    //         Self::Welcome => Color::Cyan,
    //         Self::Info => Color::Green,
    //         Self::Sessions => Color::Magenta,
    //     }
    // }

    /// 从索引转换
    pub fn from_index(index: usize) -> Self {
        Self::ALL.get(index).copied().unwrap_or(Self::Welcome)
    }
}

// 2. 界面文字内容
pub const APP_TITLE: &str = " ATLAS PRIME ";

pub const WELCOME_MSG: &str = "Next-generation Compute Platform";
pub const HELP_PROMPT: &str = "Press 'h' to toggle help & controls";
pub const ART_LOGO: &str = r#"
     █████  ████████ ██        █████  ███████
    ██   ██    ██    ██       ██   ██ ██     
    ███████    ██    ██       ███████ ███████
    ██   ██    ██    ██       ██   ██      ██
    ██   ██    ██    ████████ ██   ██ ███████
        "#;
// 2. 帮助区域内容（数组形式，方便翻页）
pub const ART_LOGO_HEIGHT: u16 = 6;
pub const HELP_CONTENT: &[&str] = &[
    "--- Navigation ---",
    "Alt + Left/Right  : Switch between Tabs immediately",
    "Alt + [1-9]       : Jump to specific Tab",
    "Tab               : Cycle focus within the current page",
    "",
    "--- Actions ---",
    "Up/Down Arrows    : Scroll lists or content",
    "Esc               : Clear notifications or close popups",
    "Ctrl + C          : Force quit Atlas (Safety Exit)",
];

// 3. 布局比例 (黄金分割)
pub const GOLDEN_RATIO_PC: u16 = 62; // 61.8%
pub const KEY_HELP: KeyCode = KeyCode::Char('h');

pub const INFO_UPDATE_INTERVAL_BASE: u64 = 3;
pub const INFO_UPDATE_INTERVAL_SLOW_TIMES: u64 = 5;
pub const INFO_UPDATE_INTERVAL_SLOWEST: u64 = 100;
pub const HISTORY_CAP: usize = 1024;

/// 底部状态栏的横向布局约束
/// 0: 按键提示 (Left)
/// 1: 文字通知 (Center)
/// 2: 进度展示 (Right)
/// 底部状态栏的横向布局常量
pub const FOOTER_LAYOUT: [Constraint; 3] = [
    Constraint::Fill(1),    // 左侧：按键提示 (Hint)
    Constraint::Fill(1),    // 中间：状态通知 (Notify)
    Constraint::Length(22), // 右侧：进度条 (Progress)
];

/// 全局主布局常量 (顶部标签, 中间内容, 底部状态栏)
pub const MAIN_LAYOUT: [Constraint; 3] = [
    Constraint::Length(3), // Tab 栏高度
    Constraint::Min(0),    // 内容区自适应
    Constraint::Length(1), // 状态栏高度
];

pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub const TASK_RAW_JSON: &str = r#"[
    {"id": "deno", "name": "🦕DenoConSole", "command": "deno", "args": [], "autostart": false, "group": "Srv", "log_limit": 4096},    
    {"id": "ps", "name": "ProcessList", "command": "ps", "args": ["aux"], "autostart": false, "group": "Srv", "log_limit": 1024},    
    {"id": "x11", "name": "Start X Server", "command": "startx", "args": [], "autostart": false, "group": "Sys", "log_limit": 100},
    {
    "id": "backup_arch",
    "name": "Backup ArchLinux",
    "command": "sh",
    "args": ["-c", "proot-distro backup archlinux --output ~/archlinux_backup_$(date +%Y_%m_%d).tar"],
    "autostart": false,
    "group": "HEAVY",
    "restart_policy": "Warn",
    "log_limit": 500
  },
  {
    "id": "miniserve",
    "name": "File Server (Miniserve)",
    "command": "miniserve",
    "args": ["-p", "13670", "-u", "-H", "-U", "-o","overwrite", "-r", "-g", "-C", "-D", "-W", "."],
    "autostart": false,
    "group": "SERVICE",
    "restart_policy": "Always",
    "log_limit": 1000
  },
  {
    "id": "tx11",
    "name": "Termux X11 Display",
    "command": "termux-x11",
    "args": [":0", "-xstartup", "dbus-launch --exit-with-session startlxqt"],
    "autostart": false,
    "group": "LIGHT",
    "restart_policy": "Never",
    "log_limit": 200
  },
  {
    "id": "backup_codex",
    "name": "Backup Code-X",
    "command": "tar",
    "args": ["-cvf", "code-x_backup.tar", "code-x"],
    "autostart": false,
    "group": "HEAVY",
    "restart_policy": "Warn",
    "log_limit": 500
  }
]"#;
