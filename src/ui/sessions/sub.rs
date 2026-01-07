use crate::{config::SharedConfig, ui::component::Component};
use ratatui::{prelude::*, widgets::*};
use tokio::sync::mpsc;

pub struct SubSession {
    pub config: SharedConfig, // 持有共享引用的拷贝，开销极小

    pub title: String,
    pub items: Vec<String>,
    pub loading: bool,
    pub state: ListState,
    rx: Option<mpsc::UnboundedReceiver<Vec<String>>>,

    pub sync_p: u16,
}

impl SubSession {
    pub fn new(config: SharedConfig, title: &str) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            title: title.to_string(),
            items: vec![],
            loading: false,
            state,
            rx: None,
            sync_p: 0,
            config,
        }
    }

    pub fn start_loading(&mut self) {
        if self.loading {
            return;
        }
        self.loading = true;
        let (tx, rx) = mpsc::unbounded_channel();
        self.rx = Some(rx);
        let title_clone = self.title.clone();

        tokio::spawn(async move {
            // 模拟耗时磁盘 IO
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let data = vec![
                format!("{} Result A", title_clone),
                format!("{} Result B", title_clone),
            ];
            let _ = tx.send(data);
        });
    }
}

impl Component for SubSession {
    fn update(&mut self) -> bool {
        let mut changed = false;
        /*
        1. 逻辑优化建议
        目前的逻辑在 rx 为 None 或 Empty 时返回 false 是正确的。但考虑到一个组件可能同时监听多个数据源（比如既有文件列表 rx，又有全局进度条 event_rx），建议采用“累加标记”模式：
        */

        // 1. 处理异步列表数据
        if let Some(ref mut rx) = self.rx {
            if let Ok(data) = rx.try_recv() {
                self.items = data;
                self.loading = false;
                self.rx = None;
                changed = true; // 标记需要重绘
            }
        }

        // 2. 如果有全局进度条同步 (GlobalEvent)，也需要检查
        // 假设你之前加了 sync_p
        // while let Ok(event) = self.event_rx.try_recv() {
        //     if let GlobalEvent::SyncProgress(p) = event {
        //         self.sync_p = p;
        //         changed = true; // 即使列表没变，进度条跳动也需要重绘
        //     }
        // }

        changed
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        // 将区域分为列表区和底部的进度条区
        let chunks = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1), // 形式 B: 紧凑的 LineGauge
        ])
        .split(area);

        if self.loading {
            f.render_widget(
                Paragraph::new("Async Loading...").alignment(Alignment::Center),
                chunks[0],
            );
        } else {
            let items: Vec<ListItem> = self
                .items
                .iter()
                .map(|i| ListItem::new(i.as_str()))
                .collect();
            f.render_stateful_widget(
                List::new(items).highlight_symbol(">> "),
                chunks[0],
                &mut self.state,
            );
        }

        // 形式 B: LineGauge 同步展示全局进度
        let line_gauge = LineGauge::default()
            .filled_style(Style::default().fg(Color::Cyan))
            .ratio(self.sync_p as f64 / 100.0);
        f.render_widget(line_gauge, chunks[1]);
    }

    fn handle_key(&mut self, _key: crossterm::event::KeyEvent) -> bool {
        // ... 滚动逻辑 ...
        false
    }
}
