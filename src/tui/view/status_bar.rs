use ratatui::{
    Frame, layout::Rect,
    widgets::Paragraph,
    style::Style,
    text::{Span, Line},
};
use crate::tui::{app::AppState, theme::Theme};

pub fn render(f: &mut Frame, state: &AppState, area: Rect, theme: Theme) {
    let conn_style = if state.connected {
        Style::default().fg(theme.accent)
    } else {
        Style::default().fg(theme.dead)
    };
    let conn = if state.connected { "● connected" } else { "○ disconnected" };

    let live_count = state.workers.iter().filter(|w| {
        w.get("status").and_then(|v| v.as_str()) != Some("dead")
    }).count();

    let line = Line::from(vec![
        Span::styled(conn, conn_style),
        Span::styled(format!(" {} workers | {} live", state.total_workers, live_count), Style::default().fg(theme.subtext)),
        Span::styled(format!(" | {:?}", state.focused_panel), Style::default().fg(theme.subtext)),
        Span::raw(" "),
        Span::styled("Tab:switch Enter:select n:new i:input q:quit", Style::default().fg(theme.subtext)),
    ]);

    let p = Paragraph::new(line)
        .style(Style::default().bg(theme.bg).fg(theme.text));
    f.render_widget(p, area);
}
