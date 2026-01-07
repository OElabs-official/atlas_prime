use ratatui::{prelude::*, widgets::*};
use crate::ui::component::Component;

pub struct SubSession {
    pub title: String,
    pub items: Vec<String>,
    pub state: ListState,
}

impl SubSession {
    pub fn new(title: &str, items: Vec<&str>) -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            title: title.to_string(),
            items: items.iter().map(|s| s.to_string()).collect(),
            state,
        }
    }
}

impl Component for SubSession {
    fn update(&mut self) {} // 子会话可以有自己的刷新逻辑

    fn render(&mut self, f: &mut Frame, area: Rect, config: &crate::config::Config) {
        let items: Vec<ListItem> = self.items.iter().map(|i| ListItem::new(i.as_str())).collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(self.title.as_str()))
            .highlight_style(Style::default().bg(Color::Blue))
            .highlight_symbol(">> ");
        
        f.render_stateful_widget(list, area, &mut self.state);
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Up => {
                let i = self.state.selected().unwrap_or(0);
                self.state.select(Some(if i == 0 { self.items.len() - 1 } else { i - 1 }));
                true
            }
            KeyCode::Down => {
                let i = self.state.selected().unwrap_or(0);
                self.state.select(Some((i + 1) % self.items.len()));
                true
            }
            _ => false,
        }
    }
}