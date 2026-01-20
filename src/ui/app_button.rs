use crate::{
    app::{GlobRecv, GlobSend},
    config::SharedConfig,
    constants::SPINNER_FRAMES,
    message::{GlobalEvent, Progress, StatusLevel},
    ui::component::Component,
};
use crossterm::event::KeyEvent;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Gauge, Paragraph},
};
use std::time::{Duration, Instant};

pub struct HintComponent {
    pub config: SharedConfig,
}

impl Component for HintComponent {
    fn init(config: SharedConfig, _send: GlobSend, _recv: GlobRecv) -> Self {
        Self { config }
    }

    fn update(&mut self) -> bool {
        false
    } // 静态组件无需更新

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let spans = vec![
            // 标签切换提示
            Span::styled(
                " Alt + ←/→ ",
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Tabs"),
            Span::raw(" | "), // 分隔符
            // 焦点内交互提示（既然现在 Tab 给了子组件）
            Span::styled(" Tab ", Style::default().bg(Color::Cyan).fg(Color::Black)),
            Span::raw(" Focus"),
            Span::raw(" | "),
            // 退出提示
            Span::styled(
                " Ctrl+C ",
                Style::default()
                    .bg(Color::Red)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Quit"),
        ];

        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn handle_key(&mut self, _key: KeyEvent) -> bool {
        false
    }
}

pub struct NotifyComponent {
    current: Option<(String, StatusLevel, Instant)>,
    rx: GlobRecv,
}

impl Component for NotifyComponent {
    fn init(_config: SharedConfig, _send: GlobSend, recv: GlobRecv) -> Self {
        Self {
            current: None,
            rx: recv,
        }
    }

    fn update(&mut self) -> bool {
        let mut changed = false;

        // 1. 接收新消息
        while let Ok(msg) = self.rx.try_recv() {
            if let GlobalEvent::Status(content, level, _) = msg {
                self.current = Some((content, level, Instant::now()));
                changed = true;
            }
        }

        // 2. 自动过期逻辑 (例如 5 秒消失)
        if let Some((_, level, start_time)) = &self.current {
            let timeout = if *level == StatusLevel::Error { 60 } else { 5 };
            if start_time.elapsed() > Duration::from_secs(timeout) {
                self.current = None;
                changed = true;
            }
        }
        changed
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        if let Some((content, level, _)) = &self.current {
            let color = match level {
                StatusLevel::Info => Color::Cyan,
                StatusLevel::Success => Color::Green,
                StatusLevel::Warning => Color::Yellow,
                StatusLevel::Error => Color::Red,
            };
            let p = Paragraph::new(content.as_str())
                .style(Style::default().fg(color))
                .alignment(Alignment::Center);
            f.render_widget(p, area);
        }
    }

    fn handle_key(&mut self, _key: KeyEvent) -> bool {
        false
    }
}

pub struct ProgressComponent {
    state: Option<(Progress, StatusLevel)>,
    rx: GlobRecv,
    tick_count: u64,
}

impl Component for ProgressComponent {
    fn init(_config: SharedConfig, _send: GlobSend, recv: GlobRecv) -> Self {
        Self {
            state: None,
            rx: recv,
            tick_count: 0,
        }
    }

    fn update(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            if let GlobalEvent::Status(_, level, Some(prog)) = msg {
                self.state = Some((prog, level));
                changed = true;
            }
        }
        // 2. 推进动画时钟
        self.tick_count = self.tick_count.wrapping_add(1);
        // 3. 如果正在 Loading，每隔几帧（例如10帧，约80ms）强制重绘一次动画
        if let Some((Progress::Loading, _)) = &self.state {
            if self.tick_count % 10 == 0 {
                changed = true;
            }
        }

        changed
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let (prog, level) = match &self.state {
            Some(s) => s,
            None => return,
        };

        let color = match level {
            StatusLevel::Error => Color::Red,
            StatusLevel::Warning => Color::Yellow,
            _ => Color::Green,
        };

        // 布局划分：左侧是进度条/动画，右侧是文字描述
        let chunks = Layout::horizontal([
            Constraint::Length(20), // 进度条自适应
            Constraint::Length(4),  // 进度文字固定长度
        ])
        .split(area);

        match prog {
            Progress::Percent(p) => {
                // 1. 渲染进度条 (使用 Ratatui 自带的 Gauge)
                let gauge = Gauge::default()
                    .gauge_style(Style::default().fg(color).bg(Color::Black))
                    .use_unicode(true)
                    .ratio(*p as f64 / 100.0)
                    .label(""); // 标签我们手动画在右边
                f.render_widget(gauge, chunks[0]);

                // 2. 渲染右侧百分比文字
                let text = format!("{:>3}%", p);
                f.render_widget(Paragraph::new(text).alignment(Alignment::Right), chunks[1]);
            }
            Progress::TaskCount(curr, total) => {
                let ratio = if *total > 0 {
                    *curr as f64 / *total as f64
                } else {
                    0.0
                };

                // 渲染进度条
                let gauge = Gauge::default()
                    .gauge_style(Style::default().fg(color))
                    .use_unicode(true)
                    .ratio(ratio.clamp(0.0, 1.0))
                    .label("");
                f.render_widget(gauge, chunks[0]);

                // 渲染任务数计数
                let text = format!("{}/{}", curr, total);
                f.render_widget(Paragraph::new(text).alignment(Alignment::Right), chunks[1]);
            }
            Progress::Loading => {
                // 渲染 Loading 动画
                let frame_idx = (self.tick_count / 10) as usize % SPINNER_FRAMES.len();
                let spinner = SPINNER_FRAMES[frame_idx];

                let text = format!("{} Loading...", spinner);
                f.render_widget(
                    Paragraph::new(text)
                        .style(Style::default().fg(color))
                        .alignment(Alignment::Right),
                    area, // Loading 状态直接占满整个区域靠右显示
                );
            }
        }
    }

    fn handle_key(&mut self, _key: KeyEvent) -> bool {
        false
    }
}

pub fn button_components_init(
    config: SharedConfig,
    glob_send: GlobSend,
) -> Vec<Box<dyn Component>> {
    vec![
        Box::new(HintComponent::init(
            config.clone(),
            glob_send.clone(),
            glob_send.subscribe(),
        )),
        Box::new(NotifyComponent::init(
            config.clone(),
            glob_send.clone(),
            glob_send.subscribe(),
        )),
        Box::new(ProgressComponent::init(
            config.clone(),
            glob_send.clone(),
            glob_send.subscribe(),
        )),
    ]
}
