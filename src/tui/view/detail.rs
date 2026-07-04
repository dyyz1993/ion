use ratatui::{
    Frame, layout::{Rect, Layout, Constraint, Direction},
    widgets::{Block, Borders, Paragraph, Wrap},
    style::{Style, Modifier},
    text::{Text, Span, Line},
};
use crate::tui::{
    app::{AppState, Panel},
    theme::Theme,
};

pub fn render(f: &mut Frame, state: &mut AppState, area: Rect, theme: Theme, focused: bool) {
    let session_id = match &state.active_session {
        Some(sid) => sid.clone(),
        None => {
            let block = Block::default()
                .title(" Detail ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border_inactive))
                .style(Style::default().bg(theme.panel_bg));
            f.render_widget(Paragraph::new("Select a worker card (Enter)").block(block).style(Style::default().fg(theme.subtext)), area);
            return;
        }
    };

    let worker = state.workers.iter().find(|w|
        w.get("session_id").and_then(|v| v.as_str()) == Some(&session_id)
    );

    let border_color = if focused { theme.border_focused } else { theme.border_inactive };

    if state.focus_mode {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Fill(1), Constraint::Length(18)])
            .split(area);
        let main = chunks[0];
        let side = chunks[1];

        let main_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Fill(1), Constraint::Length(3)])
            .split(main);

        // Chat content
        let agent = worker.and_then(|w| w.get("agent").and_then(|v| v.as_str())).unwrap_or("?");
        let title = format!(" {} · {} ", session_id.chars().take(8).collect::<String>(), agent);

        let content = Paragraph::new(render_chat(state, &session_id))
            .block(Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .style(Style::default().bg(theme.panel_bg)))
            .wrap(Wrap { trim: false });
        f.render_widget(content, main_chunks[0]);

        // Input
        let input_block = Block::default()
            .title(" Input ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(
                if state.focused_panel == Panel::Input { theme.border_focused } else { theme.border_inactive }
            ))
            .style(Style::default().bg(theme.panel_bg));
        let input_para = Paragraph::new(state.input_text.as_str())
            .block(input_block)
            .style(Style::default().fg(theme.text));
        f.render_widget(input_para, main_chunks[1]);

        // Side panel — simple todo placeholder
        let todo_block = Block::default()
            .title(" Panel ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border_inactive))
            .style(Style::default().bg(theme.panel_bg));
        let todo_para = Paragraph::new("Todo / Memory\n(coming soon)")
            .block(todo_block)
            .style(Style::default().fg(theme.subtext));
        f.render_widget(todo_para, side);
    } else {
        // Non-focus: compact detail in right column
        let agent = worker.and_then(|w| w.get("agent").and_then(|v| v.as_str())).unwrap_or("?");
        let model = worker.and_then(|w| w.get("model").and_then(|v| v.as_str())).unwrap_or("?");
        let log = worker.and_then(|w| w.get("log_short").and_then(|v| v.as_str())).unwrap_or("(no output)");
        let lines = vec![
            Line::from(Span::styled(agent, Style::default().fg(theme.accent).add_modifier(Modifier::BOLD))),
            Line::from(Span::styled(model, Style::default().fg(theme.subtext))),
            Line::from(""),
            Line::from(Span::styled(log, Style::default().fg(theme.text))),
        ];
        let p = Paragraph::new(Text::from(lines))
            .block(Block::default()
                .title(" Detail ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .style(Style::default().bg(theme.panel_bg)))
            .wrap(Wrap { trim: false });
        f.render_widget(p, area);
    }
}

fn render_chat<'a>(state: &'a AppState, session_id: &str) -> Text<'a> {
    let mut lines: Vec<Line<'a>> = Vec::new();
    if let Some(buf) = state.log_buffers.get(session_id) {
        for l in buf.iter() {
            lines.push(Line::from(Span::raw(l.as_str())));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::raw("(waiting for output...)")));
    }
    Text::from(lines)
}
