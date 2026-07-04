use ratatui::layout::{Constraint, Direction, Layout, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutTier {
    Wide,   // >= 140 cols — three columns
    Medium, // 80-139 cols — two columns
    Narrow, // < 80 cols — single column
}

#[derive(Debug)]
pub struct AppLayout {
    pub tier: LayoutTier,
    pub tree_rect: Option<Rect>,
    pub kanban_rect: Option<Rect>,
    pub detail_rect: Option<Rect>,
    pub status_bar_rect: Rect,
}

impl LayoutTier {
    pub fn from_width(width: u16) -> Self {
        if width >= 140 {
            Self::Wide
        } else if width >= 80 {
            Self::Medium
        } else {
            Self::Narrow
        }
    }
}

/// Compute responsive panel layout
pub fn compute_layout(area: Rect, tier: LayoutTier, focus_mode: bool) -> AppLayout {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Fill(1), Constraint::Length(1)])
        .split(area);
    let main = chunks[0];
    let status_bar_rect = chunks[1];

    if focus_mode {
        return AppLayout {
            tier,
            tree_rect: None,
            kanban_rect: None,
            detail_rect: Some(main),
            status_bar_rect,
        };
    }

    match tier {
        LayoutTier::Wide => {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(28),
                    Constraint::Fill(1),
                    Constraint::Length(40),
                ])
                .split(main);
            AppLayout {
                tier,
                tree_rect: Some(cols[0]),
                kanban_rect: Some(cols[1]),
                detail_rect: Some(cols[2]),
                status_bar_rect,
            }
        }
        LayoutTier::Medium => {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(24), Constraint::Fill(1)])
                .split(main);
            AppLayout {
                tier,
                tree_rect: Some(cols[0]),
                kanban_rect: Some(cols[1]),
                detail_rect: None,
                status_bar_rect,
            }
        }
        LayoutTier::Narrow => {
            AppLayout {
                tier,
                tree_rect: None,
                kanban_rect: Some(main),
                detail_rect: None,
                status_bar_rect,
            }
        }
    }
}

/// How many cards fit in one row (card minimum width 42)
pub fn cards_per_row(width: u16) -> usize {
    std::cmp::max(1, width.saturating_sub(4) as usize / 42)
}
