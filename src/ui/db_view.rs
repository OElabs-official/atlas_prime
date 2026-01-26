use crate::{
    app::{GlobIO, GlobRecv},
    config::{Config, SharedConfig},
    ui::component::Component,
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{prelude::*, widgets::*};
use std::sync::Arc;
use std::time::Instant;

const DB_UPDATE_INTERVAL: u64 = 10; // 每 10 秒刷新一次
const DB_STATS_KEY: &str = "db_stats_data";

// 数据类型：(集合名称, 文档数量)
type CollectionInfo = (String, u64);
type DbStatsData = Vec<CollectionInfo>;

pub struct DatabaseComponent {
    pub config: SharedConfig,
    glob_recv: GlobRecv,
    
    // UI 状态
    collections: DbStatsData,
    table_state: TableState,
    last_refresh: Instant,
    is_loading: bool,
}
impl Component for DatabaseComponent {
    fn init() -> Self
    where
        Self: Sized,
    {
        let inst = Self {
            config: Config::get(),
            glob_recv: GlobIO::recv(),
            collections: Vec::new(),
            table_state: TableState::default(),
            last_refresh: Instant::now(),
            is_loading: true,
        };

        // 启动首次及后续的定期刷新任务
        Self::spawn_refresh_task();

        inst
    }

    fn update(&mut self) -> bool {
        let mut changed = false;
        // 检查是否有新的数据库统计数据到达
        while let Ok(event) = self.glob_recv.try_recv() {
            if let crate::app::GlobalEvent::Data { key, data } = event {
                if key == DB_STATS_KEY {
                    if let Ok(stats) = data.downcast::<DbStatsData>() {
                        self.collections = (*stats).clone();
                        self.is_loading = false;
                        self.last_refresh = Instant::now();
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Length(3), // 标题栏
            Constraint::Min(0),    // 内容区
            Constraint::Length(1), // 状态栏
        ])
        .split(area);

        // --- 1. 标题渲染 ---
        let header = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" MongoDB Explorer ");
        
        let title_text = format!(" Database: atlas_prmime | Total Collections: {}", self.collections.len());
        f.render_widget(Paragraph::new(title_text).block(header).alignment(Alignment::Center), chunks[0]);

        // --- 2. 集合表格渲染 ---
        let header_cells = ["Collection Name", "Document Count"]
            .iter()
            .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
        let header_row = Row::new(header_cells).height(1).bottom_margin(1);

        let rows = self.collections.iter().map(|(name, count)| {
            Row::new(vec![
                Cell::from(name.clone()).style(Style::default().fg(Color::White)),
                Cell::from(count.to_string()).style(Style::default().fg(Color::Green)),
            ])
        });

        let table = Table::new(rows, [Constraint::Percentage(70), Constraint::Percentage(30)])
            .header(header_row)
            .block(Block::default().borders(Borders::LEFT | Borders::RIGHT))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::REVERSED))
            .highlight_symbol(">> ");

        f.render_stateful_widget(table, chunks[1], &mut self.table_state);

        // --- 3. 底部状态栏 ---
        let status_msg = if self.is_loading {
            " Loading data from MongoDB... ".to_string()
        } else {
            format!(" Last updated: {:?} ago | Press 'r' to refresh ", self.last_refresh.elapsed())
        };
        f.render_widget(Paragraph::new(status_msg).style(Style::default().fg(Color::DarkGray)), chunks[2]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('r') => {
                self.is_loading = true;
                Self::spawn_refresh_task();
                true
            }
            KeyCode::Up => {
                let i = match self.table_state.selected() {
                    Some(i) => if i == 0 { self.collections.len().saturating_sub(1) } else { i - 1 },
                    None => 0,
                };
                self.table_state.select(Some(i));
                true
            }
            KeyCode::Down => {
                let i = match self.table_state.selected() {
                    Some(i) => if i >= self.collections.len().saturating_sub(1) { 0 } else { i + 1 },
                    None => 0,
                };
                self.table_state.select(Some(i));
                true
            }
            _ => false,
        }
    }
}
impl DatabaseComponent {
    fn spawn_refresh_task() {
        tokio::spawn(async move {
            let glob_send = GlobIO::send();
            let client = crate::db::Mongo::client().await;
            let db = client.database(crate::ui::info::DATABASE_NAME); // 使用之前的常量

            let mut stats: DbStatsData = Vec::new();

            // 获取所有集合名称
            if let Ok(names) = db.list_collection_names().await {
                for name in names {
                    // 获取每个集合的文档估算数量
                    let count = db.collection::<serde_json::Value>(&name)
                        .estimated_document_count()
                        .await
                        .unwrap_or(0);
                    stats.push((name, count));
                }
            }

            // 排序：按文档数量降序
            stats.sort_by(|a, b| b.1.cmp(&a.1));

            let _ = glob_send.send(crate::app::GlobalEvent::Data {
                key: DB_STATS_KEY,
                data: crate::app::DynamicPayload(Arc::new(stats)),
            });
        });
    }
}
