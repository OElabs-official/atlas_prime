use crate::{config::SharedConfig, ui::component::Component};
use ratatui::{prelude::*, widgets::*};

pub struct WelcomeComponent {
    pub config: SharedConfig, // 持有共享引用的拷贝，开销极小
    // 可以在这里记录程序启动时间
    start_time: std::time::Instant,
}

impl WelcomeComponent {
    pub fn new(config: SharedConfig) -> Self {
        Self {
            start_time: std::time::Instant::now(),
            config,
        }
    }
}

impl Component for WelcomeComponent {
    fn update(&mut self) -> bool {
        // 欢迎界面通常是静态的，除非你想做 Logo 动画
        false
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Percentage(40), // 顶部 Logo 区域
            Constraint::Percentage(40), // 中间 帮助/描述
            Constraint::Percentage(20), // 底部 版本信息
        ])
        .split(area);

        // 1. ASCII ART LOGO
        let logo = r#"
     █████  ████████ ██        █████  ███████
    ██   ██    ██    ██       ██   ██ ██     
    ███████    ██    ██       ███████ ███████
    ██   ██    ██    ██       ██   ██      ██
    ██   ██    ██    ████████ ██   ██ ███████
        "#;
        let logo_widget = Paragraph::new(logo).alignment(Alignment::Center).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        f.render_widget(logo_widget, chunks[0]);

        // 2. 欢迎词与帮助
        let help_text = vec![
            Line::from(vec![
                Span::raw("Welcome to "),
                Span::styled(
                    "Atlas",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" - Your TUI Management Toolkit"),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  ← / →  ", Style::default().bg(Color::DarkGray)),
                Span::raw(" Switch Tabs "),
            ]),
            Line::from(vec![
                Span::styled("    Q    ", Style::default().bg(Color::DarkGray)),
                Span::raw(" Quit App "),
            ]),
        ];

        let help_widget = Paragraph::new(help_text)
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::NONE));
        f.render_widget(help_widget, chunks[1]);

        // 3. 底部状态
        let uptime = self.start_time.elapsed().as_secs();
        let footer = Paragraph::new(format!("Version 0.1.0 | Uptime: {}s", uptime))
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Gray));
        f.render_widget(footer, chunks[2]);
    }

    fn handle_key(&mut self, _key: crossterm::event::KeyEvent) -> bool {
        false
    }
}
