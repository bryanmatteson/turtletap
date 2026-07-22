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

    for (index, (_, surface)) in shell.entries().enumerate() {
        let title = surface.title();
        let text = format!(" {} {} ", surface.status().marker(), title);
        let width = cell_width(&text);
        let style = if shell.active_index() == Some(index) {
            shell.config.theme.selected
        } else {
            shell.config.theme.chrome
        };
        spans.push(Span::styled(text, style));
        spans.push(Span::raw(" "));
        tab_hits.push((Rect::new(x, area.y, width, 1), index));
        x = x.saturating_add(width.saturating_add(1));
    }
    shell.tab_hits = tab_hits;

    frame.render_widget(Line::from(spans), area);
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
    let content = if let Some(notice) = &shell.notice {
        Line::styled(format!(" {notice}"), shell.config.theme.attention)
    } else if shell
        .active_surface()
        .is_some_and(|surface| surface.input_policy() == crate::InputPolicy::Captured)
    {
        Line::from(vec![
            Span::styled(" Ctrl-G", shell.config.theme.accent),
            Span::styled(" then ", shell.config.theme.muted),
            Span::styled("s", shell.config.theme.accent),
            Span::styled(" palette · ", shell.config.theme.muted),
            Span::styled("d", shell.config.theme.accent),
            Span::styled(" detach · ", shell.config.theme.muted),
            Span::styled("?", shell.config.theme.accent),
            Span::styled(" help", shell.config.theme.muted),
        ])
    } else {
        Line::from(vec![
            Span::styled(" Tab", shell.config.theme.accent),
            Span::styled(" switch · ", shell.config.theme.muted),
            Span::styled("Ctrl-D", shell.config.theme.accent),
            Span::styled(" detach · ", shell.config.theme.muted),
            Span::styled("Ctrl-P", shell.config.theme.accent),
            Span::styled(" palette · ", shell.config.theme.muted),
            Span::styled("?", shell.config.theme.accent),
            Span::styled(" help", shell.config.theme.muted),
        ])
    };
    frame.render_widget(content, area);
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
    let mut lines = vec![
        help_line("Ctrl-G d", "Detach and return to the host terminal", shell),
        help_line("Ctrl-P", "Open the command palette", shell),
        help_line("Ctrl-G s", "Open the command palette", shell),
        help_line("Ctrl-G n/p", "Focus next or previous surface", shell),
        help_line("Ctrl-G x", "Close only the active surface", shell),
        help_line("Ctrl-G ?", "Open this help", shell),
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
        Span::styled(format!("{key:<12}"), shell.config.theme.accent),
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
