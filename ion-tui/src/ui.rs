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
    Runs,
    Events,
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
        self.focus = match self.focus {
            Focus::Runs => Focus::Events,
            Focus::Events => Focus::Runs,
        };
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
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(1),
    ])
    .split(f.area());

    let tabs = Tabs::new(vec!["[1] Runs", "[2] Events"])
        .select(match panels.focus {
            Focus::Runs => 0,
            Focus::Events => 1,
        })
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, chunks[0]);

    match panels.focus {
        Focus::Runs => draw_runs(f, app, chunks[1]),
        Focus::Events => draw_events(f, app, chunks[1]),
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
    f.render_widget(Paragraph::new(help), chunks[2]);
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
    let title = format!("Events ({} total)", app.events.len());
    let para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
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
        assert_eq!(panels.focus, Focus::Runs);
        panels.toggle_focus();
        assert_eq!(panels.focus, Focus::Events);
        panels.toggle_focus();
        assert_eq!(panels.focus, Focus::Runs);
    }
}
