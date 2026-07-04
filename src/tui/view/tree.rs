use ratatui::{
    Frame, layout::Rect,
    widgets::{Block, Borders, List, ListItem},
    style::{Style, Modifier, Color},
    text::Text,
};
use crate::tui::{
    app::{AppState, NodeId},
    theme::{self, Theme},
};

pub fn render(f: &mut Frame, state: &mut AppState, area: Rect, theme: Theme, focused: bool) {
    let border_color = if focused { theme.border_focused } else { theme.border_inactive };
    let block = Block::default()
        .title(format!(" Projects · {} ", state.total_projects))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(theme.panel_bg));

    let mut items: Vec<ListItem> = Vec::new();
    for (idx, node) in state.tree_items.iter().enumerate() {
        let is_selected = focused && idx == state.tree_index;
        match node {
            NodeId::Project(name) => {
                let wc = state.projects.iter()
                    .find(|p| p.get("name").and_then(|v| v.as_str()) == Some(name))
                    .and_then(|p| p.get("worker_count").and_then(|v| v.as_u64()))
                    .unwrap_or(0);
                let collapsed = state.collapsed.contains(name);
                let icon = if collapsed { "▶" } else { "▼" };
                let line = format!(" {}  {} ({})", icon, name, wc);
                let bg = if is_selected { Color::Rgb(0x1a, 0x2a, 0x3a) } else { theme.panel_bg };
                let st = Style::default().fg(theme.accent).bg(bg).add_modifier(Modifier::BOLD);
                items.push(ListItem::new(Text::styled(line, st)));
            }
            NodeId::Session(sid) => {
                let w = state.workers.iter().find(|w|
                    w.get("session_id").and_then(|v| v.as_str()) == Some(sid)
                );
                let agent = w.and_then(|w| w.get("agent").and_then(|v| v.as_str())).unwrap_or("?");
                let status = w.and_then(|w| w.get("status").and_then(|v| v.as_str())).unwrap_or("?");
                let (icon, st) = theme::status_icon(status);
                let short_sid = if sid.len() > 8 { &sid[..8] } else { sid };
                let line = format!(" {} {} {}", icon, short_sid, agent);
                let bg = if is_selected { Color::Rgb(0x1a, 0x2a, 0x3a) } else { theme.panel_bg };
                let style = st.bg(bg);
                items.push(ListItem::new(Text::styled(format!("   {}", line), style)));
            }
        }
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_widget(list, area);
}
