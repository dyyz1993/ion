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
        Span::styled(
            format!(" {}  ", app.status),
            Style::default().fg(Color::Cyan),
        ),
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
    let mut lines: Vec<Line> = Vec::new();
    for (i, r) in app.session_rows.iter().enumerate() {
        let cursor = if focused && app.session_cursor == Some(i) {
            Span::styled("> ", Style::default().fg(Color::Cyan))
        } else {
            Span::raw("  ")
        };
        if r.session_id.starts_with("group:") {
            lines.push(Line::from(vec![
                cursor,
                Span::styled(
                    r.label.clone(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            continue;
        }
        let badge = r
            .agent
            .clone()
            .map(|a| format!("[{a}] "))
            .unwrap_or_default();
        let pct = if r.ctx_max == 0 {
            0
        } else {
            u64::div_ceil(r.ctx_used * 100, r.ctx_max)
        };
        let status = format!(
            "ctx {}% · {}s · 子{} 同{}",
            pct,
            r.runtime_ms / 1000,
            usize::from(r.busy),
            usize::from(r.durable_active)
        );
        let status_style = if pct > 85 {
            Style::default().fg(Color::Red)
        } else if pct > 60 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut spans = vec![
            cursor,
            Span::raw("  ".repeat(r.indent)),
            Span::styled(badge, Style::default().fg(Color::Magenta)),
            Span::raw(r.label.clone()),
        ];
        if r.busy {
            spans.push(Span::styled(" ●", Style::default().fg(Color::Green)));
        }
        if r.durable_active {
            spans.push(Span::styled(" ◐", Style::default().fg(Color::Yellow)));
        }
        lines.push(Line::from(spans));
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(status, status_style),
        ]));
    }
    let para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(para, area);
}

fn draw_conversation(f: &mut Frame, app: &App, panels: &PanelState, area: ratatui::layout::Rect) {
    let focused = panels.focus == Focus::Conversation;
    let title = match app.conversation_session.as_deref() {
        Some(sid) => format!("Conversation ◆ {sid}"),
        None => "Conversation (select a session)".into(),
    };
    let rows = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1), // attachments
        Constraint::Length(3), // composer
        Constraint::Length(1), // context status
    ])
    .split(area);

    let mut lines: Vec<Line> = Vec::new();
    for l in crate::approvals::render_approvals(&app.approvals, app.approval_cursor) {
        let style = if l.starts_with("> ⏸") || l.starts_with("✓") {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        };
        lines.push(Line::from(Span::styled(l, style)));
    }
    if !app.approvals.is_empty() {
        lines.push(String::new().into());
    }
    if app.conversation_lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no messages)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let turns = crate::conversation::group_turns(&app.conversation_lines);
        for l in crate::conversation::render_turns(&turns, &app.expanded_turns) {
            let style = if l.contains("历史轮次") || l.starts_with("  ▸ 第") {
                Style::default().fg(Color::DarkGray)
            } else if l.starts_with("❯") {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else if l.starts_with("▸") || l.starts_with("▾") {
                Style::default().fg(Color::Yellow)
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
    f.render_widget(para, rows[0]);

    // Attachments row.
    let attach_text = if app.composer.attachments.is_empty() {
        "(no attachments)".to_string()
    } else {
        app.composer
            .attachments
            .iter()
            .map(|a| format!("📎 {a}"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    f.render_widget(
        Paragraph::new(attach_text).style(Style::default().fg(Color::DarkGray)),
        rows[1],
    );

    // Composer: bordered box with a visible cursor block.
    let draft = if app.approvals.is_empty() {
        format!("{}▊", app.composer.draft)
    } else {
        "等待审批决策…".to_string()
    };
    let mut composer_title = if focused {
        " Message ◆ "
    } else {
        " Message "
    };
    if app.selected_busy {
        composer_title = " Message (⏹ Esc 停止) ";
    }
    let composer_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let composer = Paragraph::new(draft)
        .block(Block::default().borders(Borders::ALL).title(composer_title))
        .style(composer_style);
    f.render_widget(composer, rows[2]);

    // Context/model/runtime strip.
    let ctx = app.composer.ctx_percent();
    let ctx_color = if ctx > 85 {
        Color::Red
    } else if ctx > 60 {
        Color::Yellow
    } else {
        Color::Green
    };
    let status = format!(
        "{} · ctx {} {}% ({}/{}) · {}ms · 子{} · 同{}",
        app.composer.model,
        app.composer.ctx_bar(10),
        ctx,
        app.composer.ctx_used,
        app.composer.ctx_max,
        app.composer.runtime_ms,
        app.composer.child_count,
        app.composer.peer_count
    );
    f.render_widget(
        Paragraph::new(status).style(Style::default().fg(ctx_color)),
        rows[3],
    );
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
