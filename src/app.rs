use crate::config::{Config, SharedConfig};
use crate::constans::{APP_TITLE, FOOTER_LAYOUT, TabId};
// 引入新的 message 定义
use crate::message::{GlobalEvent, Progress, StatusLevel};
use crate::prelude::GlobIO;
use crate::ui::app_button::button_components_init;
use crate::ui::component::Component;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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


pub struct App {
    // --- 基础配置 ---
    pub config: SharedConfig,

    // --- 标签页组件、索引、控制焦点 ---
    pub components: Vec<Box<dyn Component>>,
    pub active_tab: usize,
    // pub focus_on_content: bool,                     // 新增：焦点控制
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
            let titles: Vec<Line> = TabId::ALL
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    if i == self.active_tab {
                        // 激活状态：加粗、反色或特定高亮色
                        Line::from(Span::styled(
                            format!(" {} ", t.title()),
                            Style::default()
                                .fg(Color::Yellow) // 这里可以换成 TabId 定义的 theme_color
                                .add_modifier(Modifier::BOLD)
                                .add_modifier(Modifier::REVERSED), // 反色效果非常醒目
                        ))
                    } else {
                        // 未激活状态：灰色
                        Line::from(Span::styled(
                            format!(" {} ", t.title()),
                            Style::default().fg(Color::Gray),
                        ))
                    }
                })
                .collect();

            let tabs = Tabs::new(titles)
                .block(Block::default().borders(Borders::ALL).title(APP_TITLE))
                .select(self.active_tab)
                // 这个 highlight_style 是作用于整体选中效果的补充
                .highlight_style(Style::default().add_modifier(Modifier::UNDERLINED));

            f.render_widget(tabs, chunks[0]);
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

    // --- app.rs ---
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};

        // 1. 最高优先级：全局标签页切换 (Alt + Arrows / Alt + Digits)
        if key.modifiers.contains(KeyModifiers::ALT) {
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
                        self.request_render();
                        return true;
                    }
                }
                _ => {}
            }
        }

        // 2. 底部组件处理 (例如 Esc 键清除错误弹窗)
        for btn in self.button_components.iter_mut() {
            if btn.handle_key(key) {
                self.request_render();
                return true;
            }
        }

        // 3. 直接分发给当前激活的子组件 (不再判断 focus_on_content)
        // 现在的逻辑是：除非是 Alt 组合键，否则所有按键都交给内容区处理
        if let Some(comp) = self.components.get_mut(self.active_tab) {
            if comp.handle_key(key) {
                self.request_render();
                return true;
            }
        }

        false
    }
    fn init() -> Self
    where
        Self: Sized,
    {
        // 3. 初始化标签页 , 由constants.rs定义内所有标签页
        let components: Vec<Box<dyn Component>> = TabId::init();
        let button_components: Vec<Box<dyn Component>> =
            button_components_init();

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
            config:Config::get(),
            components,
            active_tab: 0,
            // focus_on_content: false,
            re_rend_mark: true,
            glob_send:GlobIO::send(),
            glob_recv:GlobIO::recv(),
            button_components,
        }
    }
}
impl App {

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


}
