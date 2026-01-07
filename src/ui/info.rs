use crate::{
config::{AppColor, Config, SharedConfig}, message::{GlobalEvent, ProgressType}, ui::component::Component
};
use ratatui::{prelude::*, widgets::*};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

pub struct InfoComponent {
    pub config: SharedConfig, // 持有共享引用的拷贝，开销极小

    cpu_usage: String,
    rx: mpsc::Receiver<String>,

    progress: u16,
    event_rx: tokio::sync::broadcast::Receiver<GlobalEvent>,
}

impl InfoComponent {
    pub fn new(config: SharedConfig, event_rx: broadcast::Receiver<GlobalEvent>) -> Self {
        let (tx, cpu_rx) = mpsc::channel(1);

        // 异步后台任务：采集 CPU
        tokio::spawn(async move {
            let mut sys = sysinfo::System::new_all();
            loop {
                sys.refresh_cpu_all();
                let usage = format!("{:.1}%", sys.global_cpu_usage());
                if tx.send(usage).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
        });

        Self {
            cpu_usage: "0%".into(),
            rx: cpu_rx,
            progress: 0,
            event_rx,
            config,
        }
    }
}

impl Component for InfoComponent {
    fn update(&mut self) -> bool {
        /*
        要使 update 函数返回合理的 bool 值，核心逻辑是：只要任何一个数据源（MPSC 通道或 Broadcast 频道）在本次调用中产生了新数据，就将标志位设为 true。
        如果不返回 true，主循环就不会触发重绘，用户也就看不到最新的 CPU 使用率或进度条变化。
        */
        let mut changed = false;

        // 1. 收割 CPU 异步数据
        // 使用 while 处理所有堆积的消息，确保数据是最新的
        while let Ok(new_val) = self.rx.try_recv() {
            if self.cpu_usage != new_val {
                self.cpu_usage = new_val;
                changed = true; // 数据变了，需要重绘
            }
        }

        // 2. 收割全局广播进度
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                GlobalEvent::SyncProgress(ProgressType::Percentage(p)) => {
                    self.progress = p;
                    changed = true; // 关键：发现了新数据，标记为已改变
                }
                // 如果未来有其他 GlobalEvent，可以在这里继续处理并设置 changed = true
                _ => {}
            }
        }

        changed // 返回是否需要重绘的标志
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        // 将垂直空间分为两块：上方内容，下方进度条
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),    // 占用剩余所有空间
                Constraint::Length(3), // 固定 3 行给进度条
            ])
            .split(area);

        // 绘制 CPU 监控内容
        let info_text = vec![
            Line::from(vec![
                Span::raw(" 系统状态: "),
                Span::styled("运行中", Style::default().fg(Color::Green)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw(" 当前 CPU 使用率: "),
                Span::styled(
                    &self.cpu_usage,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
        ];

        let theme_color;
        {
            let g = self.config.try_read(); // 异步运行时内不能使用blocking_read() 
            if let Ok(x) = g {
                theme_color = x.clone().theme_color
            } else {
                theme_color = AppColor::Cyan
            }
        }
        let info_block = Paragraph::new(info_text).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" System Monitor ")
                .border_style(Style::default().fg(theme_color.to_ratatui_color())),
        );

        frame.render_widget(info_block, chunks[0]);

        // 绘制同步进度条 (形式 A: Gauge)
        let gauge = Gauge::default()
            .block(
                Block::default()
                    .title(" 全局同步进度 (Broadcast) ")
                    .borders(Borders::ALL),
            )
            .gauge_style(Style::default().fg(Color::Magenta).bg(Color::Black))
            .percent(self.progress);

        frame.render_widget(gauge, chunks[1]);
    }

    fn handle_key(&mut self, _key: crossterm::event::KeyEvent) -> bool {
        false
    }
}
