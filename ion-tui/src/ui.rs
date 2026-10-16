use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, Tabs},
};

use crate::app::App;

/// Which panel has keyboard focus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Sessions,
    Conversation,
    Runs,
    Events,
}

impl Focus {
    pub fn next(self) -> Self {
        match self {
            Focus::Sessions => Focus::Conversation,
            Focus::Conversation => Focus::Runs,
            Focus::Runs => Focus::Events,
            Focus::Events => Focus::Sessions,
        }
    }
    #[allow(dead_code)]
    pub fn from_index(i: usize) -> Self {
        match i {
            0 => Focus::Sessions,
            1 => Focus::Conversation,
            2 => Focus::Runs,
            _ => Focus::Events,
        }
    }
    pub fn index(self) -> usize {
        match self {
            Focus::Sessions => 0,
            Focus::Conversation => 1,
            Focus::Runs => 2,
            Focus::Events => 3,
        }
    }
}

/// Collapse/expand state for detail groups.
#[derive(Clone, Debug, Default)]
pub struct PanelState {
    /// Tool/tool-batch groups the user manually expanded (key = kind/id).
    pub expanded: std::collections::HashSet<String>,
    pub focus: Focus,
}

impl PanelState {
    pub fn toggle_focus(&mut self) {
        self.focus = self.focus.next();
    }

    /// Running/uncertain tasks default to expanded; everything else collapsed
    /// unless the user expanded it.
    #[allow(dead_code)]
    pub fn is_expanded(&self, key: &str, running: bool) -> bool {
        if running {
            !self.expanded.contains(&format!("-{key}")) // user collapsed it explicitly
        } else {
            self.expanded.contains(key)
        }
    }

    #[allow(dead_code)]
    pub fn toggle(&mut self, key: &str, running: bool) {
        if running {
            let collapse_key = format!("-{key}");
            if self.expanded.contains(&collapse_key) {
                self.expanded.remove(&collapse_key);
            } else {
                self.expanded.insert(collapse_key);
            }
        } else {
            let key = key.to_string();
            if self.expanded.contains(&key) {
                self.expanded.remove(&key);
            } else {
                self.expanded.insert(key);
            }
        }
    }
}

pub fn draw(f: &mut Frame, app: &App, panels: &PanelState) {
    let outer = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(1),
    ])
    .split(f.area());

    let tabs = Tabs::new(vec![
        "[1] Sessions",
        "[2] Conversation",
        "[3] Runs",
        "[4] Events",
    ])
    .select(panels.focus.index())
    .highlight_style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(tabs, outer[0]);

    let cols = Layout::horizontal([
        Constraint::Percentage(30),
        Constraint::Percentage(40),
        Constraint::Percentage(30),
    ])
    .split(outer[1]);

    draw_sessions(f, app, panels, cols[0]);
    draw_conversation(f, app, panels, cols[1]);
    match panels.focus {
        Focus::Runs => draw_runs(f, app, cols[2]),
        Focus::Events => draw_events(f, app, cols[2]),
        _ => draw_runs(f, app, cols[2]),
    }

    let help = Line::from(vec![
        Span::styled(" j/k select  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Tab panel  ", Style::default().fg(Color::DarkGray)),
        Span::styled("p pause  ", Style::default().fg(Color::DarkGray)),
        Span::styled("r resume  ", Style::default().fg(Color::DarkGray)),
        Span::styled("a auto  ", Style::default().fg(Color::DarkGray)),
        Span::styled("e expand  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q quit", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(help), outer[2]);
}

fn draw_sessions(f: &mut Frame, app: &App, panels: &PanelState, area: ratatui::layout::Rect) {
    let focused = panels.focus == Focus::Sessions;
    let title = if focused { "Sessions ◆" } else { "Sessions" };
    let lines: Vec<Line> = app
        .session_rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let cursor = if panels.focus == Focus::Sessions && app.session_cursor == Some(i) {
                Span::styled("> ", Style::default().fg(Color::Cyan))
            } else {
                Span::raw("  ")
            };
            let mut spans = vec![
                cursor,
                Span::raw("  ".repeat(r.indent)),
                Span::raw(r.label.clone()),
            ];
            if r.busy {
                spans.push(Span::styled(" ●", Style::default().fg(Color::Green)));
            }
            if r.durable_active {
                spans.push(Span::styled(" ◐", Style::default().fg(Color::Yellow)));
            }
            let style = if r.kind == crate::sessions::SessionKind::Peer {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            Line::from(spans).style(style)
        })
        .collect();
    let para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(para, area);
}

fn draw_conversation(f: &mut Frame, app: &App, panels: &PanelState, area: ratatui::layout::Rect) {
    let focused = panels.focus == Focus::Conversation;
    let title = match app.conversation_session.as_deref() {
        Some(sid) => format!("Conversation ◆ {sid}"),
        None => "Conversation (select a session)".into(),
    };
    let title = if focused {
        format!("◆ {title}")
    } else {
        title
    };
    let mut lines: Vec<Line> = Vec::new();
    if app.conversation_lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no messages)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for l in crate::conversation::render_lines(&app.conversation_lines) {
            let style = if l.starts_with("❯") {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if l.starts_with("▸") {
                Style::default().fg(Color::Yellow)
            } else if l.starts_with('·') {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(l, style)));
        }
    }
    for l in app.render_task_detail() {
        lines.push(Line::from(Span::styled(
            l,
            Style::default().fg(Color::Cyan),
        )));
    }
    let para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(para, area);
}

fn draw_runs(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let rows = app.runs.iter().enumerate().map(|(i, r)| {
        let selected = app.selected == Some(i);
        let style = if selected {
            Style::default().bg(Color::DarkGray)
        } else {
            Style::default()
        };
        let state_style = match r.state.as_str() {
            "running" => Style::default().fg(Color::Green),
            "paused" | "recovering" => Style::default().fg(Color::Yellow),
            "blocked" => Style::default().fg(Color::Red),
            "succeeded" => Style::default().fg(Color::Cyan),
            _ => Style::default().fg(Color::Gray),
        };
        Row::new(vec![
            r.run_id.clone(),
            r.session_id.clone(),
            Span::styled(r.state.clone(), state_style).to_string(),
            format!("v{}", r.version),
        ])
        .style(style)
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(22),
            Constraint::Length(38),
            Constraint::Length(14),
            Constraint::Length(6),
        ],
    )
    .header(
        Row::new(vec!["RUN", "SESSION", "STATE", "VER"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title("Runs"));
    f.render_widget(table, area);
}

fn draw_events(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let max = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = crate::state::render_events(&app.events, max.max(1))
        .into_iter()
        .map(|l| {
            let running = l.contains("running") || l.contains("claimed");
            let style = if running {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            Line::from(Span::styled(l, style))
        })
        .collect();
    let mut spans: Vec<Line> = lines;
    if !spans.is_empty() {
        spans.push(Line::from(""));
    }
    // Task detail collapse list shares this panel (below events).
    for l in app.render_task_detail() {
        let expanded = l.contains("(expanded)");
        let running = l.contains("running");
        let style = if expanded {
            Style::default().fg(Color::Cyan)
        } else if running {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Line::from(Span::styled(l, style)));
    }
    let title = format!("Events ({} total)", app.events.len());
    let para = Paragraph::new(spans).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(para, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_items_default_expanded_and_user_can_collapse() {
        let mut panels = PanelState::default();
        assert!(panels.is_expanded("tool-1", true));
        panels.toggle("tool-1", true);
        assert!(!panels.is_expanded("tool-1", true));
        panels.toggle("tool-1", true);
        assert!(panels.is_expanded("tool-1", true));
    }

    #[test]
    fn idle_items_default_collapsed_and_user_can_expand() {
        let mut panels = PanelState::default();
        assert!(!panels.is_expanded("tool-2", false));
        panels.toggle("tool-2", false);
        assert!(panels.is_expanded("tool-2", false));
        panels.toggle("tool-2", false);
        assert!(!panels.is_expanded("tool-2", false));
    }

    #[test]
    fn focus_toggles_between_panels() {
        let mut panels = PanelState::default();
        assert_eq!(panels.focus, Focus::Sessions);
        panels.toggle_focus();
        assert_eq!(panels.focus, Focus::Conversation);
        panels.toggle_focus();
        assert_eq!(panels.focus, Focus::Runs);
        panels.toggle_focus();
        assert_eq!(panels.focus, Focus::Events);
        panels.toggle_focus();
        assert_eq!(panels.focus, Focus::Sessions);
    }
}
