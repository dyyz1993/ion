use ratatui::{
    Frame, layout::Rect,
    widgets::{Block, Borders, Paragraph},
    style::{Style, Color},
    text::{Span, Line},
};
use crate::tui::{
    app::AppState,
    theme::{self, Theme},
    layout,
};

pub fn render(f: &mut Frame, state: &mut AppState, area: Rect, theme: Theme, focused: bool) {
    let cols = layout::cards_per_row(area.width);
    if cols == 0 || state.workers.is_empty() {
        let block = Block::default()
            .title(" Workers · 0 ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if focused { theme.border_focused } else { theme.border_inactive }))
            .style(Style::default().bg(theme.panel_bg));
        f.render_widget(Paragraph::new("No active workers").block(block), area);
        return;
    }

    let card_width = area.width / cols as u16;
    if card_width < 4 { return; }

    for (i, w) in state.workers.iter().enumerate() {
        let row = i / cols;
        let col = i % cols;
        let x = area.x + col as u16 * card_width;
        let y = area.y + row as u16 * 7;
        if y + 7 > area.y + area.height { break; }
        let card_rect = Rect::new(x, y, card_width.saturating_sub(1), 7);

        let agent = w.get("agent").and_then(|v| v.as_str()).unwrap_or("?");
        let model = w.get("model").and_then(|v| v.as_str()).unwrap_or("?");
        let status = w.get("status").and_then(|v| v.as_str()).unwrap_or("?");
        let log_short = w.get("log_short").and_then(|v| v.as_str()).unwrap_or("");
        let started_at = w.get("started_at").and_then(|v| v.as_i64()).unwrap_or(0);

        let uptime = if started_at > 0 {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
            let secs = (now_ms - started_at) / 1000;
            format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
        } else { "--:--:--".into() };

        let (icon, st) = theme::status_icon(status);
        let selected = focused && state.kanban_selected == Some(i);
        let border_color = if selected { theme.accent } else if focused { theme.border_focused } else { theme.border_inactive };

        let lines = vec![
            Line::from(vec![
                Span::styled(format!(" {} {} ", icon, agent), st),
            ]),
            Line::from(vec![
                Span::styled(format!(" {} · {} ", uptime, status.to_uppercase()), st),
            ]),
            Line::from(vec![
                Span::styled(model, Style::default().fg(theme.subtext)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    if log_short.len() > 50 { format!("{}...", &log_short[..50]) } else { log_short.to_string() },
                    Style::default().fg(theme.text),
                ),
            ]),
        ];

        let para = Paragraph::new(lines)
            .block(Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .style(Style::default().bg(theme.panel_bg)));
        f.render_widget(para, card_rect);
    }

    // Title overlay
    f.render_widget(
        Block::default()
            .title(format!(" Workers · {} ", state.total_workers))
            .borders(Borders::NONE)
            .style(Style::default().bg(Color::Reset)),
        area,
    );
}
