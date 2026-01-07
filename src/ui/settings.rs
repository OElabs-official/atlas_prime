use crate::app::App;
use ratatui::{prelude::*, widgets::*};

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let cfg;
    {
        let r = app.config.try_read();
        if let Ok(x) = r {
            cfg = x.clone()
        } else {
            cfg = Default::default();
        }
    }
    let text = format!(
        "Background [b]: {}\nText [t]: {}\nRefresh Rate [+/-]: {}s\n\nPress keys to toggle.",
        cfg.background_color, cfg.theme_color, cfg.refresh_rate_ms
    );
    f.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("Settings")),
        area,
    );
}
