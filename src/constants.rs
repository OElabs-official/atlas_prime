use crossterm::event::KeyCode;
use ratatui::style::Color;

// 1. 全局快捷键
pub const KEY_QUIT: KeyCode = KeyCode::Char('q');
pub const KEY_CLEAR_NOTIFY: KeyCode = KeyCode::Char('c');
pub const KEY_TAB_NEXT: KeyCode = KeyCode::Right;
pub const KEY_TAB_PREV: KeyCode = KeyCode::Left;

/// 2. 标签页唯一标识
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabId {
    Welcome,
    Info,
    // Sessions,
}

/// 3. 页面注册信息
impl TabId {
    /// 获取所有标签页的顺序列表
    pub const ALL: &[Self] = &[
        Self::Info,
        Self::Welcome,
        Self::Info,
        // Self::Sessions
    ];

    /// 对应的显示标题
    pub fn title(&self) -> &'static str {
        match self {
            Self::Welcome => "  Welcome ",
            Self::Info => "  System Info ",
            // Self::Sessions => " [2] Session Manager ",
        }
    }

    pub fn init(config: SharedConfig, glob_send: GlobSend) -> Vec<Box<dyn Component>> {
        let mut output = vec![];
        for id in TabId::ALL.iter() {
            let comp = id.gen_component(config.clone(), glob_send.clone());
            output.push(comp);
        }
        output
    }
    fn gen_component(&self, config: SharedConfig, glob_send: GlobSend) -> Box<dyn Component> {
        match self {
            Self::Welcome => Box::new(WelcomeComponent::init(
                config,
                glob_send.clone(),
                glob_send.subscribe(),
            )),
            Self::Info => Box::new(InfoComponent::init(
                config,
                glob_send.clone(),
                glob_send.subscribe(),
            )),
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
    "--- GLOBAL CONTROLS ---",
    "q          : Quit Atlas",
    "Left/Right : Switch Tabs",
    "c          : Clear Notifications",
    "",
    "--- NAVIGATION ---",
    "Tab 0: Welcome - This Screen",
    "Tab 1: System  - Resource Monitor",
    "Tab 2: Session - Manage Instances",
    "",
    "--- SCROLLING ---",
    "Up/Down    : Scroll Help Content",
    "h          : Close this panel",
];

// 3. 布局比例 (黄金分割)
pub const GOLDEN_RATIO_PC: u16 = 62; // 61.8%
pub const KEY_HELP: KeyCode = KeyCode::Char('h');

pub const INFO_UPDATE_INTERVAL_BASE: u64 = 2;
pub const INFO_UPDATE_INTERVAL_SLOW_TIMES: u64 = 10;
pub const INFO_UPDATE_INTERVAL_SLOWEST: u64 = 100;
pub const HISTORY_CAP: usize = 1024;

use ratatui::layout::Constraint;
use ureq::config;

use crate::app::GlobSend;
use crate::config::SharedConfig;
use crate::ui::component::Component;
use crate::ui::info::InfoComponent;
use crate::ui::welcome::WelcomeComponent;

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
