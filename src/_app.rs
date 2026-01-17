use crate::config::{Config, SharedConfig};
use crate::constants::{APP_TITLE, TabId};
use crate::message::{ActiveNotification, GlobalEvent, NotificationLevel, ProgressType};
use crate::ui::component::Component;
use crate::ui::info::InfoComponent;
use crate::ui::sessions::SessionsComponent;
use crate::ui::welcome::WelcomeComponent;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Alignment;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs},
};
use tokio::time::{Duration, Interval, MissedTickBehavior, interval};

pub struct App {
    pub config: SharedConfig,                // 持有共享引用的拷贝，开销极小
    pub components: Vec<Box<dyn Component>>, // 由于组件是动态分发的，导致不能使用async trait
    pub active_tab: usize,

    // 限速器相关
    pub needs_render: bool,
    pub render_interval: Interval,

    // 消息总线
    pub glob_tx: tokio::sync::broadcast::Sender<GlobalEvent>,
    // 内部订阅者，用于 App 自身处理通知逻辑
    pub glob_rx: tokio::sync::broadcast::Receiver<GlobalEvent>,

    // 通知管理状态
    pub current_notification: Option<ActiveNotification>,
}


impl Component for App {
    fn update(&mut self) -> bool {
        let mut changed = false;

        // 1. 处理新事件
        while let Ok(event) = self.glob_rx.try_recv() {
            match event {
                GlobalEvent::Notify(msg, level) => {
                    self.current_notification = Some(ActiveNotification {
                        content: msg,
                        level,
                        progress: None,
                        created_at: std::time::Instant::now(),
                    });
                    changed = true;
                }
                GlobalEvent::SyncProgress(p) => {
                    // 进度更新通常视为 Info 级别
                    self.current_notification = Some(ActiveNotification {
                        content: "Processing...".to_string(),
                        level: NotificationLevel::Info,
                        progress: Some(p),
                        created_at: std::time::Instant::now(),
                    });
                    changed = true;
                    // self.needs_render=true;
                }
                GlobalEvent::ClearError => {
                    if let Some(n) = &self.current_notification {
                        if matches!(n.level, NotificationLevel::Error) {
                            self.current_notification = None;
                            changed = true;
                        }
                    }
                }
                _ => {},
            }
        }

        // 2. 检查自动清除 (只针对 Info 和 Warning)
        if let Some(n) = &self.current_notification {
            if !matches!(n.level, NotificationLevel::Error) {
                if n.created_at.elapsed().as_secs() >= 10 {
                    self.current_notification = None;
                    changed = true;
                }
            }
        }

        // 1. 驱动所有子组件更新（确保后台数据流不堆积）
        for comp in self.components.iter_mut() {
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

    // 在创建组件（new）时将 Config 的引用或克隆传进去，或者让组件自己持有所需的配置。这样 render 签名就统一为： fn render(&mut self, f: &mut Frame, area: Rect);
    fn render(&mut self, f: &mut Frame, area: Rect) {
        // 统一布局管理
        let chunks = Layout::vertical([
            Constraint::Length(3), // Tabs
            Constraint::Min(0),    // Content
            Constraint::Length(1), // Footer
        ])
        .split(area);

        // 渲染 Tab 栏 (内部逻辑)
        self.render_navigation(f, chunks[0]);

        // 转发渲染请求给当前活动的子组件
        if let Some(comp) = self.components.get_mut(self.active_tab) {
            comp.render(f, chunks[1]);
        }

        // 渲染状态栏
        self.render_status_bar(f, chunks[2]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        // 1. 优先交给当前组件处理
        if let Some(comp) = self.components.get_mut(self.active_tab) {
            if comp.handle_key(key) {
                self.request_render();
                return true;
            }
        }

        // 2. 如果子组件不处理，处理全局快捷键（如切换 Tab）
        let mut consumed = true;
        match key.code {
            KeyCode::Char('q') => {
                /* 注意：退出通常在 main 处理，或通过信号量 */
                consumed = false;
            }
            KeyCode::Right => self.next_tab(),
            KeyCode::Left => self.prev_tab(),
            KeyCode::Char('c') => {
                // 通过发送事件或直接修改状态来清除错误
                if let Some(n) = &self.current_notification {
                    if matches!(n.level, NotificationLevel::Error) {
                        self.current_notification = None;
                        self.request_render();
                        return true;
                    }
                }
            }
            _ => consumed = false,
        }

        if consumed {
            self.request_render();
        }
        consumed
    }
}

impl App {
    fn render_navigation(&self, f: &mut Frame, area: Rect) {
        let titles: Vec<&str> = TabId::ALL.iter().map(|t| t.title()).collect();

        let active_tab_id = TabId::from_index(self.active_tab);

        let tabs = Tabs::new(titles)
            .block(Block::default().borders(Borders::ALL).title(APP_TITLE))
            .select(self.active_tab)
            .highlight_style(
                Style::default()
                    .fg(active_tab_id.theme_color()) // 颜色随标签页自动切换
                    .add_modifier(Modifier::BOLD),
            );

        f.render_widget(tabs, area);
    }

    fn render_status_bar(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::horizontal([
            Constraint::Min(40),    // 左侧：固定快捷键
            Constraint::Length(50), // 右侧：通知与进度
        ])
        .split(area);

        // --- 左侧：原有快捷键 ---
        let help_text = vec![
            Span::styled(
                " ATLAS ",
                Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" | "),
            Span::styled("Tab", Style::default().fg(Color::Yellow)),
            Span::raw(": ←/→ | "),
            Span::styled("Quit", Style::default().fg(Color::Red)),
            Span::raw(": Q "),
        ];
        f.render_widget(Paragraph::new(Line::from(help_text)), chunks[0]);

        // 在状态栏左侧显示当前模式
        let mode_tag = Span::styled(
            format!(" MODE: {:?} ", self.active_tab),
            Style::default().bg(Color::Cyan).fg(Color::Black),
        );

        // --- 右侧：通知逻辑 ---
        if let Some(n) = &self.current_notification {
            let color = match n.level {
                NotificationLevel::Info => Color::Green,
                NotificationLevel::Warning => Color::Yellow,
                NotificationLevel::Error => Color::Red,
            };

            // 构造通知文字
            let mut spans = vec![Span::styled(&n.content, Style::default().fg(color))];

            // 如果有进度条，追加显示
            if let Some(ref p) = n.progress {
                let p_text = match p {
                    ProgressType::Percentage(v) => format!(" [{}%]", v),
                    ProgressType::TaskCount(curr, total) => format!(" [{}/{}]", curr, total),
                    ProgressType::Indeterminate => " [...]".to_string(),
                };
                spans.push(Span::styled(
                    p_text,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
            }

            // 如果是错误，提示按 C 清除
            if matches!(n.level, NotificationLevel::Error) {
                spans.push(Span::styled(
                    " (Press 'C' to clear)",
                    Style::default().fg(Color::Gray).italic(),
                ));
            }

            let notify_para = Paragraph::new(Line::from(spans)).alignment(Alignment::Right);

            f.render_widget(notify_para, chunks[1]);
        }
    }

    pub async fn new(config: SharedConfig) -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(100);
        // 2. App 订阅自己，用于处理 Statusbar 的通知显示
        let event_rx = tx.subscribe();

        let conf_guard = config.read().await;
        let interval_ms = conf_guard.refresh_rate_ms;
        drop(conf_guard);

        // 设定渲染频率上限，例如 60 FPS (16ms)
        let mut render_interval = interval(Duration::from_millis(8));
        // 如果系统繁忙导致跳帧，后续不进行补帧，而是直接等待下一个周期
        render_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // 模拟一个全局计时器同步广播
        // 测试用进度条，正式程序需要移除
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let mut p = 0;
            loop {
                p = (p + 1) % 101;
                let _ = tx_clone.send(GlobalEvent::SyncProgress(ProgressType::Percentage(p)));
                tokio::time::sleep(std::time::Duration::from_millis(8)).await;
            }
        });


        // 按照 TabId::ALL 的顺序初始化组件
        let mut components: Vec<Box<dyn Component>> = Vec::new();
        for id in TabId::ALL.iter() {
            let comp: Box<dyn Component> = match id {
                TabId::Welcome => Box::new(WelcomeComponent::new(config.clone())),
                TabId::Info => Box::new(InfoComponent::new(config.clone(), tx.clone())),
                TabId::Sessions => Box::new(SessionsComponent::new(config.clone(), tx.clone())),
            };
            components.push(comp);
        }

        Self {
            config,
            components,
            active_tab: 0,
            glob_tx: tx,
            render_interval,
            needs_render: true,
            current_notification: Default::default(),
            glob_rx: event_rx,
        } // 初始需要渲染第一帧
    }

    // 标记需要重绘
    pub fn request_render(&mut self) {
        self.needs_render = true;
    }

    // 检查是否到了渲染窗口期且确实需要渲染
    pub async fn wait_for_render(&mut self) {
        self.render_interval.tick().await;
    }

    // 检查是否到了渲染窗口期且确实需要渲染
    pub fn should_draw(&self) -> bool {
        self.needs_render
    }

    // 渲染完成后重置标志位
    pub fn clear_render_request(&mut self) {
        /* main 唯一 应该调用 clear_render_request 的地方。它必须紧跟在 terminal.draw 成功之后
        唯一的例外情况  只有一种极其特殊的情况，你可能需要手动在其他地方调用 clear（但通常不建议）：当你想强制跳过某一帧渲染时。 但在标准的 TUI 开发中，这种需求几乎不存在。
         */
        self.needs_render = false;
    }


    pub fn tick(&mut self) -> bool {
        let mut any_changed = false;

        // 1. 处理全局消息总线上的事件 (App 自身的订阅者)
        // 这样当任何地方发出 GlobalEvent::Notify 时，App 的状态会被更新
        while let Ok(event) = self.glob_rx.try_recv() {
            match event {
                GlobalEvent::Notify(content, level) => {
                    self.current_notification = Some(ActiveNotification {
                        content,
                        level,
                        progress: None,
                        created_at: std::time::Instant::now(),
                    });
                    any_changed = true;
                }
                GlobalEvent::SyncProgress(progress) => {
                    if let Some(ref mut note) = self.current_notification {
                        note.progress = Some(progress);
                        any_changed = true;
                    }
                }
                GlobalEvent::ClearError => {
                    self.current_notification = None;
                    any_changed = true;
                }
                _ => {} // 其他事件（如 PushData）由组件内部的 rx 处理
            }
        }

        // 2. 自动清理过期通知 (例如 Info 级别的通知 5秒后消失)
        if let Some(ref note) = self.current_notification {
            let duration = match note.level {
                NotificationLevel::Info => Duration::from_secs(5),
                NotificationLevel::Warning => Duration::from_secs(10),
                NotificationLevel::Error => Duration::from_secs(999999), // 错误不自动消失
            };
            if note.created_at.elapsed() > duration {
                self.current_notification = None;
                any_changed = true;
            }
        }

        // 3. 驱动所有组件更新
        for comp in self.components.iter_mut() {
            if comp.update() {
                any_changed = true;
            }
        }

        if any_changed {
            //self.needs_render = true; // 内部标记，供 main 的 render_clock 检查
            self.request_render();
        }
        any_changed
    }

    // 我们应该让 所有 组件在后台始终保持更新（或者至少在 tick 时整体更新），并确保主循环正确分发信号。
    //  确保无论当前在哪个 Tab，所有组件都能收割广播消息，防止缓冲区阻塞。
    pub fn _tick(&mut self) -> bool {
        let mut any_changed = false;

        // 核心修改：迭代所有组件进行更新，而不仅仅是 active_tab
        // 这样即使在后台的组件也能更新数据，切换回来时就是最新状态
        for comp in self.components.iter_mut() {
            if comp.update() {
                any_changed = true;
            }
        }

        if any_changed {
            self.request_render();
        }
        any_changed
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

// #[derive(Clone, Debug)]
// pub enum _GlobalEvent {
//     // 进度值 0-100
//     SyncProgress(u16),
//     // 全局通知
//     Notify(String),
// }

