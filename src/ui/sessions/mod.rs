use super::sessions::sub::SubSession;
use crate::message::{GlobalEvent, ProgressType};
use crate::ui::component::Component;
use crate::{ config::SharedConfig};
use ratatui::{prelude::*, widgets::*};
use tokio::sync::broadcast;
pub mod sub;

pub struct SessionsComponent {
    pub config: SharedConfig, // 持有共享引用的拷贝，开销极小

    primary_tabs: Vec<String>,
    primary_index: usize,
    sub_sessions: Vec<Vec<SubSession>>,
    secondary_index: usize,

    event_rx: broadcast::Receiver<GlobalEvent>,
}

impl SessionsComponent {
    pub fn new(config: SharedConfig, tx: broadcast::Sender<GlobalEvent>) -> Self {
        let mut s1 = SubSession::new(config.clone(), "Local Files");
        let s2 = SubSession::new(config.clone(), "Remote Logs");
        s1.start_loading(); // 初始化时自动开始异步加载

        Self {
            primary_tabs: vec!["DISK".into(), "NET".into()],
            primary_index: 0,
            sub_sessions: vec![
                vec![s1, SubSession::new(config.clone(), "Backup")],
                vec![s2, SubSession::new(config.clone(), "API Metrics")],
            ],
            secondary_index: 0,
            event_rx: tx.subscribe(), // 订阅全局频道
            config,
        }
    }
}

impl Component for SessionsComponent {
    fn update(&mut self) -> bool {
        //由于 SessionsComponent 包含嵌套的子组件（SubSession），它也必须负责向下传递 update。
        let mut changed = false;

        // 1. 先尝试接收自己的广播（如果有）
        while let Ok(event) = self.event_rx.try_recv() {
            if let GlobalEvent::SyncProgress(ProgressType::Percentage(p)) = event {
                // 将进度同步给所有嵌套的子组件
                for row in self.sub_sessions.iter_mut() {
                    for sub in row.iter_mut() {
                        sub.sync_p = p;
                    }
                }
                changed = true;
            }
        }

        // 2. 驱动当前子组件的内部 update (处理它的 MPSC 通道)
        if let Some(sub) = self.sub_sessions[self.primary_index].get_mut(self.secondary_index) {
            if sub.update() {
                changed = true;
            }
        }

        changed
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);

        // 渲染一级 Tab
        let titles = self.primary_tabs.iter().cloned();
        f.render_widget(
            Tabs::new(titles)
                .select(self.primary_index)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Session Hub "),
                )
                .highlight_style(Style::default().fg(Color::Yellow)),
            chunks[0],
        );

        let inner_layout =
            Layout::horizontal([Constraint::Length(18), Constraint::Min(0)]).split(chunks[1]);

        // 渲染二级侧边栏
        let side_items: Vec<ListItem> = self.sub_sessions[self.primary_index]
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let style = if i == self.secondary_index {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(format!(" {} ", s.title)).style(style)
            })
            .collect();

        f.render_widget(
            List::new(side_items).block(Block::default().borders(Borders::ALL).title("Sub")),
            inner_layout[0],
        );

        // 3. 渲染具体的 SubSession
        if let Some(sub) = self.sub_sessions[self.primary_index].get_mut(self.secondary_index) {
            sub.render(f, inner_layout[1]);
        }
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::KeyCode;
        // 优先交给子组件处理（如滚动）
        if self.sub_sessions[self.primary_index][self.secondary_index].handle_key(key) {
            return true;
        }

        match key.code {
            KeyCode::Left | KeyCode::Right => {
                self.primary_index = (self.primary_index + 1) % self.primary_tabs.len();
                true
            }
            KeyCode::Tab | KeyCode::Down
                if !key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                let len = self.sub_sessions[self.primary_index].len();
                self.secondary_index = (self.secondary_index + 1) % len;
                // 切换时可以触发逻辑
                self.sub_sessions[self.primary_index][self.secondary_index].start_loading();
                true
            }
            _ => false,
        }
    }
}
