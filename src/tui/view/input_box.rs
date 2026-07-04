use ratatui::{
    Frame, layout::Rect,
    widgets::{Block, Borders, Paragraph},
    style::Style,
};
use crate::tui::{app::AppState, theme::Theme};

pub fn render(f: &mut Frame, state: &AppState, area: Rect, theme: Theme, focused: bool) {
    let border_color = if focused { theme.border_focused } else { theme.border_inactive };
    let block = Block::default()
        .title(" Input ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme.panel_bg));
    let para = Paragraph::new(state.input_text.as_str())
        .block(block)
        .style(Style::default().fg(theme.text));
    f.render_widget(para, area);
}
