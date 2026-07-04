pub mod tree;
pub mod kanban;
pub mod detail;
pub mod input_box;
pub mod status_bar;
pub mod create_modal;

use ratatui::Frame;
use crate::tui::{
    app::{AppState, Panel},
    layout::AppLayout,
    theme::Theme,
};

pub fn render(f: &mut Frame, state: &mut AppState, layout: &AppLayout) {
    let theme = Theme::cyberpunk();

    // Background fill
    let bg_style = ratatui::style::Style::default().bg(theme.bg);
    f.render_widget(ratatui::widgets::Clear, f.area());
    f.render_widget(ratatui::widgets::Block::default().style(bg_style), f.area());

    if let Some(rect) = layout.tree_rect {
        tree::render(f, state, rect, theme, state.focused_panel == Panel::Tree);
    }
    if let Some(rect) = layout.kanban_rect {
        kanban::render(f, state, rect, theme, state.focused_panel == Panel::Kanban);
    }
    if let Some(rect) = layout.detail_rect {
        detail::render(f, state, rect, theme, state.focused_panel == Panel::Detail);
    }
    // Always render input bar at bottom (in detail area if focus mode)
    if state.focus_mode {
        // Input is rendered inside detail::render when in focus_mode
    } else if let Some(rect) = layout.kanban_rect {
        // Render input as a small bar at bottom of kanban area
        let bottom_area = ratatui::layout::Rect {
            x: rect.x,
            y: rect.y + rect.height.saturating_sub(3),
            width: rect.width,
            height: 3,
        };
        input_box::render(f, state, bottom_area, theme, state.focused_panel == Panel::Input);
    }
    status_bar::render(f, state, layout.status_bar_rect, theme);

    // 模态层（最上层）
    if state.create_modal.is_some() {
        create_modal::render(f, state, f.area(), theme);
    }
}
