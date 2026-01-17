use crate::config::SharedConfig;
use crate::constants::{APP_TITLE, FOOTER_LAYOUT, TabId};
// 引入新的 message 定义
use crate::message::{GlobalEvent, Progress, StatusLevel};
use crate::ui::app_button::button_components_init;
use crate::ui::component::Component;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs},
};
use tokio::sync::broadcast;
use tokio::time::{Duration, Instant, Interval, MissedTickBehavior, interval};

pub type GlobSend = broadcast::Sender<GlobalEvent>;
pub type GlobRecv = broadcast::Receiver<GlobalEvent>;

/*
/// App 内部持有的状态快照（用于渲染 Footer）
pub struct ActiveStatus {
    pub content: String,
    pub level: StatusLevel,
    pub progress: Option<Progress>,
    pub start_time: Instant,
}
*/
pub struct App {
    // --- 基础配置 ---
    pub config: SharedConfig,

    // --- 标签页组件、索引、控制焦点 ---
    pub components: Vec<Box<dyn Component>>,
    pub active_tab: usize,
    pub focus_on_content: bool,                     // 新增：焦点控制
    pub button_components: Vec<Box<dyn Component>>, // 底部通知组件

    // --- 消息总线 --- 在有需要的时候广播通道clone一份给子组件
    pub glob_send: GlobSend,
    pub glob_recv: GlobRecv,
    //pub current_status: Option<ActiveStatus>, // 新增：合并后的状态显示

    // --- 重绘标记 ---
    pub re_rend_mark: bool,
}

impl Component for App {
    fn update(&mut self) -> bool {
        let mut changed = false;
        // 1. 处理新事件
        while let Ok(event) = self.glob_recv.try_recv() {
            match event {
                GlobalEvent::Status(_, _, _) => changed = true,
                _ => {}
            }
        }

        // 需要重新设计
        // // 2. 检查自动清除 (只针对 Info 和 Warning)
        // if let Some(n) = &self.current_status {
        //     if !matches!(n.level, NotificationLevel::Error) {
        //         if n.created_at.elapsed().as_secs() >= 10 {
        //             self.current_status = None;
        //             changed = true;
        //         }
        //     }
        // }

        // 1. 驱动所有子组件更新（确保后台数据流不堆积）
        for comp in self.components.iter_mut() {
            if comp.update() {
                changed = true;
            }
        }
        for comp in self.button_components.iter_mut() {
            if comp.update() {
                changed = true;
            }
        }
        // 2. 如果有更新，给自己打上“脏标记”
        if changed {
            self.request_render();
        }
        changed
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        // 统一布局管理
        let chunks = Layout::vertical([
            Constraint::Length(3), // Tabs
            Constraint::Min(0),    // Content
            Constraint::Length(1), // Footer
        ])
        .split(area);

        {
            // 渲染 Tab 栏 (内部逻辑)

            let titles: Vec<&str> = TabId::ALL.iter().map(|t| t.title()).collect();

            //let active_tab_id = TabId::from_index(self.active_tab);

            let tabs = Tabs::new(titles)
                .block(Block::default().borders(Borders::ALL).title(APP_TITLE))
                .select(self.active_tab)
                .highlight_style(
                    Style::default()
                        //.fg(active_tab_id.theme_color()) // 颜色随标签页自动切换
                        .add_modifier(Modifier::BOLD),
                );
            f.render_widget(tabs, chunks[0]);
            // f.render_widget(tabs, area);
        }

        // 转发渲染请求给当前活动的子组件
        if let Some(comp) = self.components.get_mut(self.active_tab) {
            comp.render(f, chunks[1]);
        }

        // --- 3. 渲染底部状态栏 ---
        let footer_chunks = Layout::horizontal(FOOTER_LAYOUT).split(chunks[2]);

        // 按照固定索引渲染：0-Hint, 1-Notify, 2-Progress
        if self.button_components.len() >= 3 {
            self.button_components[0].render(f, footer_chunks[0]);
            self.button_components[1].render(f, footer_chunks[1]);
            self.button_components[2].render(f, footer_chunks[2]);
        }
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        // 键盘处理和屏幕焦点需要再讨论处理
        // A. 优先让底部组件处理事件 (例如 Error 状态下的 Esc 清除)
        for btn in self.button_components.iter_mut() {
            if btn.handle_key(key) {
                self.re_rend_mark = true;
                return true;
            }
        }

        // B. 根据焦点分发逻辑
        if self.focus_on_content {
            // 焦点在内容区：分发给当前 Tab
            if let Some(comp) = self.components.get_mut(self.active_tab) {
                if comp.handle_key(key) {
                    self.re_rend_mark = true;
                    return true;
                }
            }
        } else {
            // 焦点在 Tab 栏：处理切换逻辑
            match key.code {
                KeyCode::Right => {
                    self.next_tab();
                    return true;
                }
                KeyCode::Left => {
                    self.prev_tab();
                    return true;
                }
                KeyCode::Char(c) if c.is_digit(10) => {
                    let idx = (c.to_digit(10).unwrap() as usize).saturating_sub(1);
                    if idx < self.components.len() {
                        self.active_tab = idx;
                        self.re_rend_mark = true;
                        return true;
                    }
                }
                _ => {}
            }
        }

        // C. 处理全局功能键 (Tab 切换焦点 / Esc 退出)
        match key.code {
            KeyCode::Tab => {
                self.focus_on_content = !self.focus_on_content;
                self.re_rend_mark = true;
                true
            }
            _ => false,
        }
    }

    fn init(config: SharedConfig, glob_send: GlobSend, glob_recv: GlobRecv) -> Self
    where
        Self: Sized,
    {
        // 3. 初始化标签页 , 由constants.rs定义内所有标签页
        let components: Vec<Box<dyn Component>> = TabId::init(config.clone(), glob_send.clone());
        let button_components: Vec<Box<dyn Component>> =
            button_components_init(config.clone(), glob_send.clone());

        {
            // 模拟一个全局计时器同步广播
            // 测试用进度条，正式程序需要移除
            // let tx_clone = glob_send.clone();
            // tokio::spawn(async move {
            //     let mut p = 0;
            //     loop {
            //         p = (p + 1) % 101;
            //         let _ = tx_clone.send(GlobalEvent::Status(Default::default(), StatusLevel::Info, Some(Progress::Percent(p))));
            //         // let _ = tx_clone.send(GlobalEvent::SyncProgress(ProgressType::Percentage(p)));
            //         tokio::time::sleep(std::time::Duration::from_millis(8)).await;
            //     }
            // });
        }

        Self {
            config,
            components,
            active_tab: 0,
            focus_on_content: false,
            re_rend_mark: true,
            glob_send,
            glob_recv,
            button_components,
        }
    }
}
impl App {
    /*
    /// 处理 AppAction 指令
    fn handle_action(&mut self, action: AppAction) -> bool {
        match action {
            AppAction::FocusNext => {
                // 在 Tab栏 和 内容区 之间切换
                self.focus_on_content = match self.focus_on_content {
                    FocusArea::TabBar => FocusArea::MainContent,
                    FocusArea::MainContent => FocusArea::TabBar,
                };
                true
            }
            AppAction::SwitchTab(idx) => {
                if idx < self.components.len() && idx != self.active_tab {
                    self.active_tab = idx;
                    true
                } else {
                    false
                }
            }
            AppAction::MoveTab(delta) => {
                let len = self.components.len() as isize;
                let new_idx = (self.active_tab as isize + delta).rem_euclid(len) as usize;
                if new_idx != self.active_tab {
                    self.active_tab = new_idx;
                    true
                } else {
                    false
                }
            }
            AppAction::ClearStatus => {
                self.current_status = None;
                true
            }
            AppAction::Quit => {
                // Quit 通常由 main loop 捕获处理，这里只作为状态变更
                // 如果需要 App 内部处理退出清理，写在这里
                false
            }
        }
    }
    */
    // --- 渲染相关 ---

    pub fn request_render(&mut self) {
        self.re_rend_mark = true;
    }

    pub fn should_draw(&self) -> bool {
        self.re_rend_mark
    }

    pub fn clear_render_request(&mut self) {
        self.re_rend_mark = false;
    }

    pub fn next_tab(&mut self) {
        self.active_tab = (self.active_tab + 1) % self.components.len();
        self.request_render(); // 必须加入
    }

    pub fn prev_tab(&mut self) {
        if self.active_tab == 0 {
            self.active_tab = self.components.len() - 1;
        } else {
            self.active_tab -= 1;
        }
        self.request_render(); // 必须加入
    }

    /*  ai 重写
    fn render_tabs(&self, f: &mut Frame, area: Rect) {
        let titles: Vec<&str> = TabId::ALL.iter().map(|t| t.title()).collect();
        let active_id = TabId::from_index(self.active_tab);

        // 根据焦点状态决定 Tab 栏的样式
        let (border_style, title_style) = if self.focus_on_content == FocusArea::TabBar {
            (
                Style::default().fg(Color::Yellow), // 激活时边框黄色
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            )
        } else {
            (
                Style::default().fg(Color::DarkGray), // 失焦时暗淡
                Style::default().fg(Color::Gray)
            )
        };

        let tabs = Tabs::new(titles)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(APP_TITLE)
                    .title_style(title_style)
                    .border_style(border_style)
            )
            .select(self.active_tab)
            .highlight_style(
                Style::default()
                    .fg(active_id.theme_color())
                    .add_modifier(Modifier::BOLD)
            );

        f.render_widget(tabs, area);
    }
    */
}
