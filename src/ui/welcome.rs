use crate::{config::SharedConfig, constants::ART_LOGO, ui::component::Component};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{prelude::*, widgets::*};

pub struct WelcomeComponent {
    pub config: SharedConfig, // 持有共享引用的拷贝，开销极小
    // 可以在这里记录程序启动时间
    show_help: bool,
    help_scroll: u16,
}

impl WelcomeComponent {
    pub fn new(config: SharedConfig) -> Self {
        Self {
            config,
            show_help: false,
            help_scroll: 0,
        }
    }
}

impl Component for WelcomeComponent {
    fn is_fullscreen(&self) -> bool {
        self.show_help // 当显示帮助时，请求全屏
    }

    fn update(&mut self) -> bool {
        // 欢迎界面通常是静态的，除非你想做 Logo 动画
        false
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
    if self.show_help {
        // ==========================================
        // 1. 全屏帮助模式：艺术字置顶 + 帮助内容填充
        // ==========================================
        
        // 这里的 area 已经是 App 传过来的全屏 Rect（因为 is_fullscreen 返回了 true）
        let chunks = Layout::vertical([
            Constraint::Length(crate::constants::ART_LOGO_HEIGHT), // 顶部固定高度给 Logo
            Constraint::Length(1),                                 // 留一行空行作为装饰
            Constraint::Min(0),                                    // 剩余空间全给帮助内容
        ])
        .split(area);

        // --- 渲染置顶 Logo ---
        let logo_lines: Vec<Line> = crate::constants::ART_LOGO
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::Cyan))))
            .collect();

        f.render_widget(
            Paragraph::new(logo_lines).alignment(Alignment::Center),
            chunks[0],
        );

        // --- 渲染全屏帮助内容 ---
        let help_text: Vec<Line> = crate::constants::HELP_CONTENT
            .iter()
            .map(|&l| Line::from(l))
            .collect();

        let help_block = Block::default()
            .title(" Atlas Help & Controls ") // 增加更明显的标题
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));

        f.render_widget(
            Paragraph::new(help_text)
                .block(help_block)
                .scroll((self.help_scroll, 0)),
            chunks[2], // 使用 chunks[2]
        );

    } else {
        // ==========================================
        // 2. 普通欢迎模式：黄金分割布局 (保持原有逻辑)
        // ==========================================
        
        let chunks = Layout::vertical([
            Constraint::Percentage(crate::constants::GOLDEN_RATIO_PC),
            Constraint::Min(0),
        ])
        .split(area);

        // Logo 弹簧布局：推至黄金分割线上方
        let logo_layout = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(crate::constants::ART_LOGO_HEIGHT),
        ])
        .split(chunks[0]);

        let logo_lines: Vec<Line> = crate::constants::ART_LOGO
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::Cyan))))
            .collect();

        f.render_widget(
            Paragraph::new(logo_lines).alignment(Alignment::Center),
            logo_layout[1],
        );

        // 欢迎词与提示
        let sub_chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(chunks[1]);

        f.render_widget(
            Paragraph::new(crate::constants::WELCOME_MSG)
                .alignment(Alignment::Center)
                .style(Style::default().add_modifier(Modifier::BOLD)),
            sub_chunks[0],
        );

        f.render_widget(
            Paragraph::new(crate::constants::HELP_PROMPT)
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::DarkGray)),
            sub_chunks[2],
        );
    }
}
   
    /*
    fn render(&mut self, f: &mut Frame, area: Rect) {
        // 1. 基础分割：黄金分割
        let chunks = Layout::vertical([
            Constraint::Percentage(crate::constants::GOLDEN_RATIO_PC), // chunks[0]
            Constraint::Min(0),                                        // chunks[1]
        ])
        .split(area);

        // 2. 在 chunks[0] 中计算 Logo 的位置，使其底部对齐分割线
        // 我们在上方填充垂直留白 (Spacer)
        let logo_layout = Layout::vertical([
            Constraint::Min(0),                                    // 伸缩留白
            Constraint::Length(crate::constants::ART_LOGO_HEIGHT), // 固定高度的 Logo
        ])
        .split(chunks[0]);

        // 渲染 Logo
        let logo_lines: Vec<Line> = crate::constants::ART_LOGO
            .lines()
            .filter(|l| !l.is_empty()) // 过滤空行
            .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::Cyan))))
            .collect();

        // 这里的 logo_layout[1] 保证了 Logo 紧贴分割线
        f.render_widget(
            Paragraph::new(logo_lines).alignment(Alignment::Center),
            logo_layout[1],
        );

        // 3. 渲染下半部分（保持不变）
        if !self.show_help {
            let sub_chunks = Layout::vertical([
                Constraint::Length(1), // 欢迎词紧贴 Logo
                Constraint::Length(1), // 间距
                Constraint::Length(1), // 帮助提示
            ])
            .split(chunks[1]);

            f.render_widget(
                Paragraph::new(crate::constants::WELCOME_MSG)
                    .alignment(Alignment::Center)
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                sub_chunks[0],
            );

            f.render_widget(
                Paragraph::new(crate::constants::HELP_PROMPT)
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(Color::DarkGray)),
                sub_chunks[2],
            );
        } else {
            // 帮助界面占据 chunks[1] 全域
            let help_text: Vec<Line> = crate::constants::HELP_CONTENT
                .iter()
                .map(|&l| Line::from(l))
                .collect();

            let help_block = Block::default()
                .title(" Atlas Controls ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow));

            f.render_widget(
                Paragraph::new(help_text)
                    .block(help_block)
                    .scroll((self.help_scroll, 0)),
                chunks[1],
            );
        }
    }
    */

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            crate::constants::KEY_HELP => {
                self.show_help = !self.show_help;
                self.help_scroll = 0; // 切换时重置滚动
                true
            }
            KeyCode::Up if self.show_help => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
                true
            }
            KeyCode::Down if self.show_help => {
                self.help_scroll = self.help_scroll.saturating_add(1);
                true
            }
            _ => false,
        }
    }
}
