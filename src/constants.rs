use crossterm::event::KeyCode;

// 1. 全局快捷键
pub const KEY_QUIT: KeyCode = KeyCode::Char('q');
pub const KEY_TAB_NEXT: KeyCode = KeyCode::Right;
pub const KEY_TAB_PREV: KeyCode = KeyCode::Left;

// 2. 界面文字内容
pub const APP_TITLE: &str = " ATLAS PRIME ";
pub const TAB_TITLES: &[&str] = &[" 0. Home ", " 1. System ", " 2. Sessions "];
