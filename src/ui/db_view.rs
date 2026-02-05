use crate::{
    config::{Config, SharedConfig}, constans::{DATABASE_FILE, INFO_UPDATE_INTERVAL_BASE, INFO_UPDATE_INTERVAL_SLOW_TIMES}, prelude::{Component, DynamicPayload, GlobIO, GlobRecv, GlobalEvent, ProjectPath}, 
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{prelude::*, widgets::*};
use std::sync::Arc;
use std::time::Duration;

const SQLITE_STATS_KEY: &str = "sqlite_table_stats";

#[derive(Clone, Debug)]
pub struct TableStat {
    pub name: String,
    pub count: i64,
}

pub struct DatabaseComponent {
    pub config: SharedConfig,
    glob_recv: GlobRecv,
    tables: Vec<TableStat>,
    table_state: TableState,
    is_loading: bool,
}

impl Component for DatabaseComponent {
    fn init() -> Self {
        let inst = Self {
            config: Config::get(),
            glob_recv: GlobIO::recv(),
            tables: Vec::new(),
            table_state: TableState::default(),
            is_loading: true,
        };

        // 启动自动化周期抓取任务
        Self::spawn_periodic_monitor();
        inst
    }

    fn update(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.glob_recv.try_recv() {
            if let GlobalEvent::Data { key, data } = event {
                if key == SQLITE_STATS_KEY {
                    if let Ok(stats) = data.0.downcast::<Vec<TableStat>>() {
                        self.tables = (*stats).clone();
                        self.is_loading = false;
                        if self.table_state.selected().is_none() && !self.tables.is_empty() {
                            self.table_state.select(Some(0));
                        }
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    fn render(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Table
            Constraint::Length(1), // Footer/Hint
        ])
        .split(area);

        // 1. Header
        let db_path = crate::prelude::ProjectPath::get().proj_dir.join(DATABASE_FILE);
        f.render_widget(
            Paragraph::new(format!(" 📂 DB Path: {} ", db_path.display()))
                .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Blue))),
            chunks[0],
        );

        // 2. Table
        let header_cells = ["Table Name", "Row Count"]
            .iter()
            .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
        
        let rows = self.tables.iter().map(|t| {
            Row::new(vec![
                Cell::from(t.name.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(t.count.to_string()).style(Style::default().fg(Color::Green)),
            ])
        });

        let table = Table::new(rows, [Constraint::Percentage(70), Constraint::Percentage(30)])
            .header(Row::new(header_cells).height(1).bottom_margin(1))
            .block(Block::default().title(" Schema Overview ").borders(Borders::LEFT | Borders::RIGHT))
            .highlight_style(Style::default().bg(Color::Rgb(50, 50, 50)))
            .highlight_symbol(">> ");

        f.render_stateful_widget(table, chunks[1], &mut self.table_state);

        // 3. Footer
        let refresh_sec = INFO_UPDATE_INTERVAL_BASE * INFO_UPDATE_INTERVAL_SLOW_TIMES;
        let hint = if self.is_loading {
            " Loading database schema... ".into()
        } else {
            format!(" Auto-refresh every {}s | 'r' to force | ↑↓ to move ", refresh_sec)
        };
        f.render_widget(Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)), chunks[2]);
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('r') => {
                self.is_loading = true;
                Self::spawn_fetch_stats();
                true
            }
            KeyCode::Up => {
                let i = match self.table_state.selected() {
                    Some(i) => if i == 0 { self.tables.len().saturating_sub(1) } else { i - 1 },
                    None => 0,
                };
                self.table_state.select(Some(i));
                true
            }
            KeyCode::Down => {
                let i = match self.table_state.selected() {
                    Some(i) => if i >= self.tables.len().saturating_sub(1) { 0 } else { i + 1 },
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
    /// 核心逻辑：计算刷新周期并建立后台长线任务
    fn spawn_periodic_monitor() {
        tokio::spawn(async move {
            let interval_duration = Duration::from_secs(INFO_UPDATE_INTERVAL_BASE * INFO_UPDATE_INTERVAL_SLOW_TIMES);
            let mut interval = tokio::time::interval(interval_duration);
            
            loop {
                interval.tick().await;
                Self::spawn_fetch_stats();
            }
        });
    }

    fn spawn_fetch_stats() {
        tokio::spawn(async move {
            let glob_send = GlobIO::send();
            let pool = crate::db::Database::pool();

            let table_names: Vec<String> = sqlx::query_scalar(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"
            )
            .fetch_all(pool)
            .await
            .unwrap_or_default();

            let mut stats = Vec::new();
            for name in table_names {
                let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {}", name))
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
                stats.push(TableStat { name, count });
            }

            let _ = glob_send.send(GlobalEvent::Data {
                key: SQLITE_STATS_KEY,
                data: DynamicPayload(Arc::new(stats)),
            });
        });
    }
}