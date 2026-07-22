use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};

use crate::{
    Shell, SurfaceStatus,
    shell::{Overlay, PaletteAction},
};

pub(crate) fn draw(frame: &mut Frame<'_>, shell: &mut Shell) {
    let area = frame.area();
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    draw_tabs(frame, sections[0], shell);
    draw_active(frame, sections[1], shell);
    draw_status(frame, sections[2], shell);

    match shell.overlay.clone() {
        Some(Overlay::Palette { query, selected }) => {
            draw_palette(frame, area, shell, &query, selected);
        }
        Some(Overlay::Help) => draw_help(frame, area, shell),
        None => {}
    }
}

fn draw_tabs(frame: &mut Frame<'_>, area: Rect, shell: &mut Shell) {
    shell.tab_hits.clear();
    let mut tab_hits = Vec::with_capacity(shell.entries().count());
    let host_title = format!(" {} ", shell.config.title);
    let host_width = cell_width(&host_title);
    let mut spans = vec![
        Span::styled(
            host_title,
            shell.config.theme.accent.add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    let mut x = area.x.saturating_add(host_width.saturating_add(2));

    let active = shell.active_index().unwrap_or_default();
    let tabs: Vec<_> = shell
        .entries()
        .enumerate()
        .map(|(index, (_, surface))| {
            let text = format!(
                " {}:{} {} ",
                index + 1,
                surface.status().marker(),
                surface.title()
            );
            let width = cell_width(&text);
            let style = if active == index {
                shell.config.theme.selected
            } else {
                shell.config.theme.chrome
            };
            (index, text, width, style)
        })
        .collect();
    let available = area.width.saturating_sub(host_width.saturating_add(2));
    let widths: Vec<_> = tabs.iter().map(|(_, _, width, _)| *width).collect();
    let visible = visible_tab_range(&widths, active, available);

    if visible.start > 0 {
        spans.push(Span::styled("‹ ", shell.config.theme.muted));
        x = x.saturating_add(2);
    }
    for (visible_index, (index, text, width, style)) in tabs[visible.clone()].iter().enumerate() {
        spans.push(Span::styled(text.clone(), *style));
        tab_hits.push((Rect::new(x, area.y, *width, 1), *index));
        x = x.saturating_add(*width);
        if visible_index + 1 < visible.len() {
            spans.push(Span::raw(" "));
            x = x.saturating_add(1);
        }
    }
    if visible.end < tabs.len() {
        spans.push(Span::styled(" ›", shell.config.theme.muted));
    }
    shell.tab_hits = tab_hits;

    frame.render_widget(Line::from(spans), area);
}

fn visible_tab_range(widths: &[u16], active: usize, available: u16) -> std::ops::Range<usize> {
    if widths.is_empty() || available == 0 {
        return 0..0;
    }

    let active = active.min(widths.len() - 1);
    let full_width = widths
        .iter()
        .copied()
        .reduce(|total, width| total.saturating_add(1).saturating_add(width))
        .unwrap_or_default();
    if full_width <= available {
        return 0..widths.len();
    }

    let mut best = active..active + 1;
    let mut best_rank = (1_usize, 0_u16, usize::MAX);
    for start in 0..=active {
        let mut tab_width = 0_u16;
        for end in start + 1..=widths.len() {
            tab_width = tab_width
                .saturating_add(widths[end - 1])
                .saturating_add(u16::from(end > start + 1));
            if end <= active {
                continue;
            }
            let occupied = tab_width
                .saturating_add(if start > 0 { 2 } else { 0 })
                .saturating_add(if end < widths.len() { 2 } else { 0 });
            if occupied > available {
                continue;
            }
            let count = end - start;
            let imbalance = active
                .saturating_sub(start)
                .abs_diff(end.saturating_sub(active + 1));
            let rank = (count, occupied, usize::MAX - imbalance);
            if rank > best_rank {
                best = start..end;
                best_rank = rank;
            }
        }
    }
    best
}

fn draw_active(frame: &mut Frame<'_>, area: Rect, shell: &mut Shell) {
    let Some(surface) = shell.active_surface() else {
        let empty = Paragraph::new("No surfaces are open.")
            .style(shell.config.theme.muted)
            .alignment(Alignment::Center)
            .block(Block::bordered().title(" Turtle "));
        frame.render_widget(empty, area);
        return;
    };

    let title = surface.title().into_owned();
    let status = surface.status();
    let title_line = Line::from(vec![
        Span::styled(format!("{} ", status.marker()), status_style(shell, status)),
        Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {}", status.label()), shell.config.theme.muted),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(title_line);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some(surface) = shell.active_surface_mut() {
        surface.render(frame, inner);
    }
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, shell: &Shell) {
    let previous = shell.config.bindings.primary_previous_label();
    let next = shell.config.bindings.primary_next_label();
    let jump = shell.config.bindings.primary_jump_label();
    let palette = shell.config.bindings.primary_palette_label();
    let content = if let Some(notice) = &shell.notice {
        Line::styled(format!(" {notice}"), shell.config.theme.attention)
    } else if shell
        .active_surface()
        .is_some_and(|surface| surface.input_policy() == crate::InputPolicy::Captured)
    {
        Line::from(vec![
            Span::styled(format!(" {previous}/{next}"), shell.config.theme.accent),
            Span::styled(" switch · ", shell.config.theme.muted),
            Span::styled(jump, shell.config.theme.accent),
            Span::styled(" jump · ", shell.config.theme.muted),
            Span::styled(palette, shell.config.theme.accent),
            Span::styled(" commands", shell.config.theme.muted),
        ])
    } else {
        Line::from(vec![
            Span::styled(" Tab", shell.config.theme.accent),
            Span::styled(" switch · ", shell.config.theme.muted),
            Span::styled(jump, shell.config.theme.accent),
            Span::styled(" jump · ", shell.config.theme.muted),
            Span::styled("Ctrl-D", shell.config.theme.accent),
            Span::styled(" detach · ", shell.config.theme.muted),
            Span::styled("?", shell.config.theme.accent),
            Span::styled(" help", shell.config.theme.muted),
        ])
    };
    let position = shell
        .active_position()
        .map_or_else(String::new, |(active, total)| {
            format!("screen {active}/{total} ")
        });
    let position_width = cell_width(&position);
    let sections = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(position_width.min(area.width)),
        ])
        .split(area);
    frame.render_widget(content, sections[0]);
    frame.render_widget(
        Line::styled(position, shell.config.theme.muted).right_aligned(),
        sections[1],
    );
}

fn draw_palette(
    frame: &mut Frame<'_>,
    viewport: Rect,
    shell: &Shell,
    query: &str,
    selected: usize,
) {
    let palette_items = shell.palette_items(query);
    let item_count = palette_items.len();
    let width = viewport.width.saturating_sub(4).clamp(24, 64);
    let desired_height = palette_items.len().saturating_add(5);
    let height = (desired_height as u16)
        .min(viewport.height.saturating_sub(2))
        .max(6);
    let area = centered(viewport, width, height);
    let block = Block::bordered()
        .title(" Command palette ")
        .title_bottom(
            Line::styled(
                " ↑↓ move · Enter run · Esc close ",
                shell.config.theme.muted,
            )
            .centered(),
        )
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(inner);
    let search = Line::from(vec![
        Span::styled("› ", shell.config.theme.accent),
        Span::raw(query.to_owned()),
        Span::styled("_", shell.config.theme.muted),
    ]);
    let items = palette_items.into_iter().map(|item| {
        let prefix = match item.action {
            PaletteAction::SelectSurface(index) if query.is_empty() && index < 9 => {
                format!(" {} ", index + 1)
            }
            PaletteAction::SelectSurface(_) => {
                format!(" {} ", item.status.unwrap_or(SurfaceStatus::Ready).marker())
            }
            _ => " › ".to_owned(),
        };
        let marker_style = item.status.map_or(shell.config.theme.accent, |status| {
            status_style(shell, status)
        });
        ListItem::new(Line::from(vec![
            Span::styled(prefix, marker_style),
            Span::raw(item.label),
            Span::styled(format!("  {}", item.detail), shell.config.theme.muted),
        ]))
    });
    let list = List::new(items)
        .highlight_style(shell.config.theme.selected)
        .highlight_symbol("→");
    let selection = (item_count > 0).then_some(selected.min(item_count.saturating_sub(1)));
    let mut state = ListState::default().with_selected(selection);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(search, sections[0]);
    if selection.is_some() {
        frame.render_stateful_widget(list, sections[1], &mut state);
    } else {
        frame.render_widget(
            Paragraph::new("No matching commands").style(shell.config.theme.muted),
            sections[1],
        );
    }
}

fn draw_help(frame: &mut Frame<'_>, viewport: Rect, shell: &Shell) {
    let previous = shell.config.bindings.primary_previous_label();
    let next = shell.config.bindings.primary_next_label();
    let jump = shell.config.bindings.primary_jump_label();
    let palette = shell.config.bindings.primary_palette_label();
    let leader = shell.config.bindings.primary_leader_label();
    let mut lines = vec![
        help_line(
            &format!("{previous} / {next}"),
            "Focus previous or next screen",
            shell,
        ),
        help_line(&jump, "Jump directly to a numbered screen", shell),
        help_line(&palette, "Open the command palette", shell),
        help_line(
            &format!("{leader} d"),
            "Detach and return to the host terminal",
            shell,
        ),
        help_line(
            &format!("{leader} x"),
            "Close only the active surface",
            shell,
        ),
        help_line(&format!("{leader} ?"), "Show keyboard help", shell),
    ];

    if let Some(surface) = shell.active_surface() {
        let shortcuts = surface.shortcuts();
        if !shortcuts.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!("{} shortcuts", surface.title()),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            for shortcut in shortcuts {
                lines.push(help_line(&shortcut.key, &shortcut.description, shell));
            }
        }
    }

    let width = viewport.width.saturating_sub(4).clamp(28, 72);
    let desired_height = lines.len().saturating_add(4);
    let height = (desired_height as u16)
        .min(viewport.height.saturating_sub(2))
        .max(8);
    let area = centered(viewport, width, height);
    let help = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::bordered()
            .title(" Help ")
            .title_bottom(Line::styled(" Esc close ", shell.config.theme.muted).centered())
            .padding(Padding::horizontal(1)),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(help, area);
}

fn help_line(key: &str, description: &str, shell: &Shell) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<18}"), shell.config.theme.accent),
        Span::raw(description.to_owned()),
    ])
}

fn status_style(shell: &Shell, status: SurfaceStatus) -> Style {
    match status {
        SurfaceStatus::Ready => shell.config.theme.muted,
        SurfaceStatus::Working => shell.config.theme.working,
        SurfaceStatus::Attention => shell.config.theme.attention,
        SurfaceStatus::Failed => shell.config.theme.failed,
        SurfaceStatus::Complete => shell.config.theme.complete,
    }
}

fn centered(parent: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(parent.width);
    let height = height.min(parent.height);
    Rect::new(
        parent.x + parent.width.saturating_sub(width) / 2,
        parent.y + parent.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn cell_width(text: &str) -> u16 {
    Line::from(text).width().min(usize::from(u16::MAX)) as u16
}
