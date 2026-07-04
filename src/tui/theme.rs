use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub bg: Color,
    pub panel_bg: Color,
    pub text: Color,
    pub subtext: Color,
    pub accent: Color,
    pub danger: Color,
    pub warning: Color,
    pub dead: Color,
    pub border_focused: Color,
    pub border_normal: Color,
    pub border_inactive: Color,
}

impl Theme {
    pub const fn cyberpunk() -> Self {
        Self {
            bg: Color::Rgb(0x0a, 0x0e, 0x1a),
            panel_bg: Color::Rgb(0x11, 0x18, 0x27),
            text: Color::Rgb(0xc8, 0xd3, 0xf5),
            subtext: Color::Rgb(0x5b, 0x6b, 0x9c),
            accent: Color::Rgb(0x00, 0xff, 0xd1),
            danger: Color::Rgb(0xff, 0x2d, 0x95),
            warning: Color::Rgb(0xff, 0xb8, 0x00),
            dead: Color::Rgb(0x7a, 0x1f, 0x3d),
            border_focused: Color::Rgb(0x00, 0xff, 0xd1),
            border_normal: Color::Rgb(0x1f, 0x4d, 0x5c),
            border_inactive: Color::Rgb(0x2a, 0x33, 0x49),
        }
    }
}

/// State display icon and style
pub fn status_icon(status: &str) -> (&'static str, Style) {
    let theme = Theme::cyberpunk();
    match status {
        "busy"  => ("▶", Style::default().fg(theme.danger).add_modifier(Modifier::BOLD)),
        "idle"  => ("⏸", Style::default().fg(theme.accent)),
        "stale" => ("⚠", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
        "dead"  => ("⨯", Style::default().fg(theme.dead)),
        _       => ("?", Style::default().fg(theme.subtext)),
    }
}
