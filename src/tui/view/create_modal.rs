use ratatui::{
    Frame, layout::{Rect, Layout, Constraint, Direction, Alignment},
    widgets::{Block, Borders, Paragraph, Clear},
    style::{Style, Modifier, Color},
    text::{Span, Line},
};
use crate::tui::{app::{AppState, CreateField}, theme::Theme};

pub fn render(f: &mut Frame, state: &mut AppState, area: Rect, theme: Theme) {
    let modal = match state.create_modal.as_ref() {
        Some(m) => m,
        None => return,
    };

    // 居中模态：宽 60，高 12
    let modal_w = 64.min(area.width.saturating_sub(4));
    let modal_h = 12;
    let x = area.x + (area.width.saturating_sub(modal_w)) / 2;
    let y = area.y + (area.height.saturating_sub(modal_h)) / 2;
    let modal_area = Rect::new(x, y, modal_w, modal_h);

    // 先清空背景（半透明效果用纯色覆盖模拟）
    f.render_widget(Clear, modal_area);

    let block = Block::default()
        .title(Span::styled(
            " ✦ Create New Worker ",
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.panel_bg));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),  // 说明
            Constraint::Length(3),  // path 字段
            Constraint::Length(3),  // agent 字段
            Constraint::Length(2),  // 提示
        ])
        .margin(1)
        .split(modal_area);

    f.render_widget(block, modal_area);

    // 说明
    let hint = Paragraph::new(Line::from(vec![
        Span::styled("Worker 会在指定项目目录下工作", Style::default().fg(theme.subtext)),
    ]));
    f.render_widget(hint, chunks[0]);

    // Path 字段
    let path_focused = modal.field == CreateField::Path;
    let path_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " Project Path ",
            Style::default().fg(if path_focused { theme.accent } else { theme.subtext }),
        ))
        .border_style(Style::default().fg(if path_focused { theme.accent } else { theme.border_inactive }));
    let path_para = Paragraph::new(modal.path.as_str())
        .block(path_block)
        .style(Style::default().fg(theme.text));
    f.render_widget(path_para, chunks[1]);

    // Agent 字段
    let agent_focused = modal.field == CreateField::Agent;
    let agent_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " Agent (build/explore/plan/reviewer) ",
            Style::default().fg(if agent_focused { theme.accent } else { theme.subtext }),
        ))
        .border_style(Style::default().fg(if agent_focused { theme.accent } else { theme.border_inactive }));
    let agent_para = Paragraph::new(modal.agent.as_str())
        .block(agent_block)
        .style(Style::default().fg(theme.text));
    f.render_widget(agent_para, chunks[2]);

    // 提示
    let mut hints = vec![
        Span::styled("Tab", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::raw(" 切字段  "),
        Span::styled("Enter", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::raw(" 创建  "),
        Span::styled("Esc", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::raw(" 取消"),
    ];
    if let Some(e) = &modal.error {
        hints.push(Span::raw("  "));
        hints.push(Span::styled(format!("⚠ {e}"), Style::default().fg(theme.warning)));
    }
    let help = Paragraph::new(Line::from(hints))
        .alignment(Alignment::Center);
    f.render_widget(help, chunks[3]);

    // 设置光标位置到当前字段
    let (cursor_x, cursor_y) = match modal.field {
        CreateField::Path => {
            (chunks[1].x + 1 + modal.path.len() as u16, chunks[1].y + 1)
        }
        CreateField::Agent => {
            (chunks[2].x + 1 + modal.agent.len() as u16, chunks[2].y + 1)
        }
    };
    f.set_cursor(cursor_x, cursor_y);
}
