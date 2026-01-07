pub mod component;
pub mod info;
pub mod sessions;
pub mod settings;
pub mod welcome;

use ratatui::{prelude::*, widgets::*};

pub fn render_tabs(f: &mut Frame, area: Rect, active_tab: usize) {
    let titles = vec![" 1. System ", " 2. Sessions ", " 3. Settings "];
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title(" Main Menu "))
        .select(active_tab)
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, area);
}
