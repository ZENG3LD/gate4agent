use uzor_tui::{
    split, Block, Color, Constraint, Direction, Line, Modifier, Paragraph, Rect, Span, Style,
    TerminalBuffer, Text, Widget,
};
use gate4agent_node_protocol::{GitSnapshot, WorkspaceEntryKind, WorkspaceInspection};

use crate::app::{
    managed_state_label, AddSpaceField, AgentRowKey, App, ConnectionState, ControlSection,
    CreateWorktreeField, DragState, Focus, GridAxisKind, GridPaneLayout, GridPreset, HitRegion,
    HitTarget, LayoutRects, MenuPlacement, NodeView, PtyColorMode, RosterMode, SessionView,
    SidebarMode, SidebarPresentation, SurfaceMode, WorkspaceView,
};
use crate::pty_palette::{apply_pty_palette, GATE_FG, TERM_BG};

const SIDEBAR_BG: Color = Color::Rgb(24, 24, 37);
const ACTIVE_BG: Color = Color::Rgb(30, 30, 46);
const BORDER: Color = Color::Rgb(49, 50, 68);
const MUTED: Color = Color::Rgb(108, 112, 134);
const DIM: Color = Color::Rgb(147, 153, 178);
const MAUVE: Color = Color::Rgb(203, 166, 247);
const ACTIVE_TAB_TEXT: Color = Color::Rgb(23, 23, 26);
const GREEN: Color = Color::Rgb(166, 227, 161);
const YELLOW: Color = Color::Rgb(249, 226, 175);
const RED: Color = Color::Rgb(243, 139, 168);
const TEAL: Color = Color::Rgb(148, 226, 213);

#[derive(Clone, Copy)]
struct Theme {
    surface: Color,
    panel: Color,
    modal: Color,
    active: Color,
    border: Color,
    text: Color,
    muted: Color,
    dim: Color,
    accent: Color,
    active_tab_text: Color,
    green: Color,
    yellow: Color,
    red: Color,
    teal: Color,
}

impl Theme {
    fn for_mode(mode: PtyColorMode) -> Self {
        match mode {
            PtyColorMode::Inherited => Self {
                surface: Color::Reset,
                panel: Color::Reset,
                modal: Color::Black,
                active: Color::Reset,
                border: Color::Indexed(8),
                text: Color::Reset,
                muted: Color::Indexed(8),
                dim: Color::Indexed(8),
                accent: Color::Magenta,
                active_tab_text: Color::Black,
                green: Color::Green,
                yellow: Color::Yellow,
                red: Color::Red,
                teal: Color::Cyan,
            },
            PtyColorMode::GateOverride => Self {
                surface: TERM_BG,
                panel: SIDEBAR_BG,
                modal: SIDEBAR_BG,
                active: ACTIVE_BG,
                border: BORDER,
                text: GATE_FG,
                muted: MUTED,
                dim: DIM,
                accent: MAUVE,
                active_tab_text: ACTIVE_TAB_TEXT,
                green: GREEN,
                yellow: YELLOW,
                red: RED,
                teal: TEAL,
            },
        }
    }
}

pub fn render(app: &App, buf: &mut TerminalBuffer) -> LayoutRects {
    buf.clear();
    let area = Rect::new(0, 0, buf.width(), buf.height());
    let theme = Theme::for_mode(app.color_mode);
    if area.width < 48 || area.height < 12 {
        Paragraph::new("gate4agent operator needs at least 48x12")
            .style(Style::default().fg(theme.yellow).bg(theme.surface))
            .render(area, buf);
        return LayoutRects::default();
    }

    Paragraph::new("")
        .style(Style::default().fg(theme.text).bg(theme.surface))
        .render(area, buf);
    let (activity_rail, sidebar_content, spaces, agents, right_area) =
        match (app.menu_placement, app.sidebar_presentation) {
            (MenuPlacement::Sidebar, SidebarPresentation::Split) => {
                let sidebar_width = app.sidebar_width.clamp(18, area.width.saturating_sub(24));
                let columns = split(
                    area,
                    Direction::Horizontal,
                    &[Constraint::Fixed(sidebar_width), Constraint::Min(1)],
                );
                let sidebar_content = Rect::new(
                    columns[0].x,
                    columns[0].y,
                    columns[0].width.saturating_sub(1),
                    columns[0].height,
                );
                let split_percent = app.sidebar_split_percent.clamp(20, 80) as u32;
                let top_height = ((sidebar_content.height as u32 * split_percent) / 100) as u16;
                let top_height = top_height.clamp(3, sidebar_content.height.saturating_sub(3));
                let spaces = Rect::new(
                    sidebar_content.x,
                    sidebar_content.y,
                    sidebar_content.width,
                    top_height,
                );
                let agents = Rect::new(
                    sidebar_content.x,
                    sidebar_content.y + top_height,
                    sidebar_content.width,
                    sidebar_content.height.saturating_sub(top_height),
                );
                (Rect::default(), sidebar_content, spaces, agents, columns[1])
            }
            (MenuPlacement::Sidebar, SidebarPresentation::Activity) => {
                let rail_width = 3_u16.min(area.width.saturating_sub(1));
                let content_width = if app.sidebar_collapsed {
                    0
                } else {
                    app.sidebar_width.clamp(18, area.width.saturating_sub(rail_width + 24))
                };
                let total_width = rail_width.saturating_add(content_width);
                let right_area = Rect::new(
                    area.x + total_width,
                    area.y,
                    area.width.saturating_sub(total_width),
                    area.height,
                );
                let rail = Rect::new(area.x, area.y, rail_width, area.height);
                let content = Rect::new(area.x + rail_width, area.y, content_width, area.height);
                let spaces = if matches!(app.control_section, ControlSection::Files | ControlSection::Git) {
                    content
                } else {
                    Rect::default()
                };
                let agents = if matches!(app.control_section, ControlSection::Agents | ControlSection::Workspaces) {
                    content
                } else {
                    Rect::default()
                };
                (rail, content, spaces, agents, right_area)
            }
            (MenuPlacement::Modal, _) => (
                Rect::default(),
                Rect::default(),
                Rect::default(),
                Rect::default(),
                area,
            ),
        };
    let right = split(
        right_area,
        Direction::Vertical,
        &[Constraint::Fixed(1), Constraint::Min(1)],
    );
    let mut layout = LayoutRects {
        activity_rail,
        spaces,
        agents,
        tabs: right[0],
        viewport: right[1],
        control_content: Rect::default(),
        control_modal: Rect::default(),
        grid_panes: Vec::new(),
        grid_drop: Rect::default(),
        tab_drop: Rect::default(),
        hits: Vec::new(),
    };

    if app.menu_placement == MenuPlacement::Sidebar
        && app.sidebar_presentation == SidebarPresentation::Split
    {
        Paragraph::new("")
            .style(Style::default().bg(theme.panel))
            .render(sidebar_content, buf);
        let x = sidebar_content.right();
        for y in sidebar_content.y..sidebar_content.bottom() {
            let cell = buf.get_mut(x, y);
            cell.symbol = "│".into();
            cell.style = Style::default().fg(theme.border).bg(theme.panel);
        }
        render_inspector(app, spaces, buf, &mut layout, theme);
        render_roster(app, agents, buf, &mut layout, theme);
        layout.hits.push(HitRegion {
            rect: Rect::new(sidebar_content.right(), sidebar_content.y, 1, sidebar_content.height),
            target: HitTarget::SidebarWidthDrag,
        });
        layout.hits.push(HitRegion {
            rect: Rect::new(agents.x, agents.y, agents.width, 1),
            target: HitTarget::SidebarSplitDrag,
        });
    } else if app.menu_placement == MenuPlacement::Sidebar {
        render_activity_rail(app, activity_rail, buf, &mut layout, theme);
        if !app.sidebar_collapsed && sidebar_content.width > 0 {
            Paragraph::new("")
                .style(Style::default().bg(theme.panel))
                .render(sidebar_content, buf);
            match app.control_section {
                ControlSection::Files => {
                    render_workspace_files(app, spaces, buf, &mut layout, theme)
                }
                ControlSection::Git => {
                    render_workspace_git(app, spaces, buf, &mut layout, theme)
                }
                ControlSection::Agents => {
                    render_agent_list(app, agents, buf, &mut layout, theme)
                }
                ControlSection::Workspaces => {
                    render_space_list(app, agents, buf, &mut layout, theme)
                }
                ControlSection::Settings => {}
            }
            let divider_x = sidebar_content.right().saturating_sub(1);
            for y in sidebar_content.y..sidebar_content.bottom() {
                let cell = buf.get_mut(divider_x, y);
                cell.symbol = "│".into();
                cell.style = Style::default().fg(theme.border).bg(theme.panel);
            }
            layout.hits.push(HitRegion {
                rect: Rect::new(divider_x, sidebar_content.y, 1, sidebar_content.height),
                target: HitTarget::SidebarWidthDrag,
            });
        }
    }
    render_tabs(app, right[0], buf, &mut layout, theme);
    render_surface(app, right[1], buf, &mut layout, theme);

    if app.focus == Focus::Spawn {
        render_spawn(app, area, buf, theme);
    }
    if app.focus == Focus::AddSpace {
        render_add_space(app, area, buf, theme);
    }
    if app.focus == Focus::CreateWorktree {
        render_create_worktree(app, area, buf, theme);
    }
    if app.focus == Focus::RemoveWorktree {
        render_remove_worktree(app, area, buf, theme);
    }
    if app.focus == Focus::RenameSession {
        render_rename_session(app, area, buf, theme);
    }
    if app.focus == Focus::ForgetSession {
        render_forget_session(app, area, buf, theme);
    }
    if app.focus == Focus::Settings {
        render_settings(app, area, buf, &mut layout, theme);
    }
    render_drag_preview(app, area, buf, &layout, theme);
    if let Some(notice) = &app.notice {
        render_notice(notice, right[1], buf, theme);
    }
    layout
}

fn render_activity_rail(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    fill_rect(area, theme.panel, buf);
    let sections = [
        (ControlSection::Files, "F"),
        (ControlSection::Git, "G"),
        (ControlSection::Agents, "A"),
        (ControlSection::Workspaces, "W"),
    ];
    for (index, (section, label)) in sections.into_iter().enumerate() {
        let y = area.y.saturating_add(index as u16);
        if y >= area.bottom() {
            break;
        }
        let selected = app.control_section == section && !app.sidebar_collapsed;
        let row = Rect::new(area.x, y, area.width, 1);
        Paragraph::new(centered_label(label, area.width as usize))
            .style(
                Style::default()
                    .fg(if selected { theme.active_tab_text } else { theme.muted })
                    .bg(if selected { theme.accent } else { theme.panel })
                    .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
            )
            .render(row, buf);
        layout.hits.push(HitRegion {
            rect: row,
            target: HitTarget::ActivitySection(section),
        });
    }
    if area.height >= 2 {
        let collapse = Rect::new(area.x, area.bottom() - 2, area.width, 1);
        Paragraph::new(centered_label(if app.sidebar_collapsed { ">" } else { "<" }, area.width as usize))
            .style(Style::default().fg(theme.dim).bg(theme.panel))
            .render(collapse, buf);
        layout.hits.push(HitRegion { rect: collapse, target: HitTarget::SidebarCollapse });
        let settings = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        Paragraph::new(centered_label("S", area.width as usize))
            .style(Style::default().fg(theme.muted).bg(theme.panel))
            .render(settings, buf);
        layout.hits.push(HitRegion {
            rect: settings,
            target: HitTarget::ActivitySection(ControlSection::Settings),
        });
    }
}

fn render_inspector(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    if area.height == 0 {
        return;
    }
    let mut x = area.x;
    for mode in [SidebarMode::Files, SidebarMode::Git] {
        let label = format!(" {} ", mode.id());
        let width = (cell_width(&label) as u16).min(area.right().saturating_sub(x));
        if width == 0 {
            break;
        }
        let selected = app.sidebar_mode == mode;
        Paragraph::new(truncate_cells(&label, width as usize))
            .style(
                Style::default()
                    .fg(if selected { theme.active_tab_text } else { theme.muted })
                    .bg(if selected { theme.accent } else { theme.panel })
                    .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
            )
            .render(Rect::new(x, area.y, width, 1), buf);
        layout.hits.push(HitRegion {
            rect: Rect::new(x, area.y, width, 1),
            target: HitTarget::SidebarMode(mode),
        });
        x = x.saturating_add(width);
    }
    if area.height < 2 {
        return;
    }
    let content = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    match app.sidebar_mode {
        SidebarMode::Files => render_workspace_files(app, content, buf, layout, theme),
        SidebarMode::Git => render_workspace_git(app, content, buf, layout, theme),
    }
}

fn render_space_list(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    if area.height == 0 {
        return;
    }
    fill_rect(area, theme.panel, buf);
    let add_label = " + workspace ";
    let add_width = (cell_width(add_label) as u16).min(area.width);
    Paragraph::new(truncate_cells(add_label, add_width as usize))
        .style(Style::default().fg(theme.teal).bg(theme.panel).add_modifier(Modifier::BOLD))
        .render(Rect::new(area.x, area.y, add_width, 1), buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(area.x, area.y, add_width, 1),
        target: HitTarget::AddSpace,
    });
    let remove_label = " - remove ";
    let remove_width = (cell_width(remove_label) as u16).min(area.width.saturating_sub(add_width));
    let remove_x = area.right().saturating_sub(remove_width);
    if remove_width > 0 {
        Paragraph::new(truncate_cells(remove_label, remove_width as usize))
            .style(Style::default().fg(theme.red).bg(theme.panel))
            .render(Rect::new(remove_x, area.y, remove_width, 1), buf);
        layout.hits.push(HitRegion {
            rect: Rect::new(remove_x, area.y, remove_width, 1),
            target: HitTarget::RemoveSpace,
        });
    }

    let rows = app.space_rows();
    let capacity = area.height.saturating_sub(1) as usize / 2;
    let start = app
        .workspaces_scroll
        .min(rows.len().saturating_sub(capacity));
    for (visible_index, (index, (node_index, workspace_index))) in rows
        .into_iter()
        .enumerate()
        .skip(start)
        .take(capacity)
        .enumerate()
    {
        let y = area.y + 1 + visible_index as u16 * 2;
        let node = &app.nodes[node_index];
        let workspace = &node.workspaces[workspace_index];
        let selected = index == app.selected_space;
        let background = if selected { theme.active } else { theme.panel };
        let (marker, marker_color) = workspace_state(node, workspace, theme);
        fill_rect(Rect::new(area.x, y, area.width, 2.min(area.bottom().saturating_sub(y))), background, buf);
        let spawn_label = " +agent ";
        let spawn_width = (cell_width(spawn_label) as u16).min(area.width);
        let primary_area_width = area.width.saturating_sub(spawn_width);
        let primary_width = primary_area_width.saturating_sub(3) as usize;
        Paragraph::new(Text::from_lines(vec![Line::from_spans(vec![
            Span::styled(format!(" {marker} "), Style::default().fg(marker_color)),
            Span::styled(
                truncate_cells(&workspace.label, primary_width),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
        ])]))
        .style(Style::default().bg(background))
        .render(Rect::new(area.x, y, primary_area_width, 1), buf);
        let spawn_x = area.right().saturating_sub(spawn_width);
        Paragraph::new(spawn_label)
            .style(
                Style::default()
                    .fg(if selected { theme.teal } else { theme.muted })
                    .bg(background)
                    .add_modifier(Modifier::BOLD),
            )
            .render(Rect::new(spawn_x, y, spawn_width, 1), buf);
        let secondary = format!("{} · {}", node.node_id, workspace.canonical_root);
        Paragraph::new(format!(
            "   {}",
            compact_middle_cells(&secondary, area.width.saturating_sub(3) as usize)
        ))
        .style(
            Style::default()
                .fg(if selected { theme.accent } else { theme.muted })
                .bg(background),
        )
        .render(Rect::new(area.x, y + 1, area.width, 1), buf);
        layout.hits.push(HitRegion {
            rect: Rect::new(area.x, y, primary_area_width, 1),
            target: HitTarget::Space(index),
        });
        layout.hits.push(HitRegion {
            rect: Rect::new(area.x, y + 1, area.width, 1),
            target: HitTarget::Space(index),
        });
        layout.hits.push(HitRegion {
            rect: Rect::new(spawn_x, y, spawn_width, 1),
            target: HitTarget::SpawnSpace(index),
        });
    }
}

fn render_inspector_header(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    theme: Theme,
) -> Rect {
    fill_rect(area, theme.panel, buf);
    let selected = app
        .selected_workspace()
        .map(|(node, workspace)| format!("{} / {}", node.node_id, workspace.workspace_id))
        .unwrap_or_else(|| "no workspace".to_owned());
    Paragraph::new(format!(
        " {}",
        compact_middle_cells(&selected, area.width.saturating_sub(1) as usize)
    ))
    .style(Style::default().fg(theme.text).bg(theme.panel).add_modifier(Modifier::BOLD))
    .render(Rect::new(area.x, area.y, area.width, 1), buf);
    Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(1),
    )
}

fn render_workspace_files(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let list = render_inspector_header(app, area, buf, theme);
    let Some(inspection) = app.selected_workspace_inspection() else {
        render_inspector_empty(app, list, buf, "loading workspace...", theme);
        return;
    };
    let entry_indices = app.visible_workspace_entry_indices();
    let capacity = list.height as usize;
    let start = app
        .files_scroll
        .min(entry_indices.len().saturating_sub(capacity));
    for (visible, (visible_index, index)) in entry_indices
        .iter()
        .copied()
        .enumerate()
        .skip(start)
        .take(capacity)
        .enumerate()
    {
        let entry = &inspection.entries[index];
        let y = list.y + visible as u16;
        let selected = visible_index == app.files_cursor;
        let background = if selected { theme.active } else { theme.panel };
        let depth = entry
            .relative_path
            .bytes()
            .filter(|byte| matches!(byte, b'/' | b'\\'))
            .count()
            .min(4);
        let name = entry
            .relative_path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(entry.relative_path.as_str());
        let dirty = workspace_entry_dirty(&entry.relative_path, entry.kind, &inspection.git);
        let dirty_color = dirty.map(|severe| if severe { theme.red } else { theme.yellow });
        let (marker, color) = match entry.kind {
            WorkspaceEntryKind::Directory => (
                if app.directory_is_collapsed(&entry.relative_path) { ">" } else { "v" },
                dirty_color.unwrap_or(theme.teal),
            ),
            WorkspaceEntryKind::File => (
                if dirty.is_some() { "M" } else { "·" },
                dirty_color.unwrap_or(theme.dim),
            ),
        };
        let prefix = format!(" {}{} ", "  ".repeat(depth), marker);
        let name_width = list.width.saturating_sub(cell_width(&prefix) as u16) as usize;
        Paragraph::new(Text::from_lines(vec![Line::from_spans(vec![
            Span::styled(prefix, Style::default().fg(color)),
            Span::styled(
                truncate_cells(name, name_width),
                Style::default().fg(dirty_color.unwrap_or(theme.text)),
            ),
        ])]))
        .style(Style::default().bg(background))
        .render(Rect::new(list.x, y, list.width, 1), buf);
        layout.hits.push(HitRegion {
            rect: Rect::new(list.x, y, list.width, 1),
            target: HitTarget::SidebarItem(index),
        });
    }
    if inspection.tree_truncated && list.height > 0 && entry_indices.len() <= capacity {
        Paragraph::new(" ... tree truncated")
            .style(Style::default().fg(theme.yellow).bg(theme.panel))
            .render(Rect::new(list.x, list.bottom() - 1, list.width, 1), buf);
    }
}

fn render_workspace_git(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let body = render_inspector_header(app, area, buf, theme);
    let Some(inspection) = app.selected_workspace_inspection() else {
        render_inspector_empty(app, body, buf, "loading workspace...", theme);
        return;
    };
    render_git_snapshot(app, inspection, body, buf, layout, theme);
}

fn render_git_snapshot(
    app: &App,
    inspection: &WorkspaceInspection,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    if area.height == 0 {
        return;
    }
    let git = &inspection.git;
    if !git.is_repository {
        let message = git.diagnostic.as_deref().unwrap_or("not a git repository");
        Paragraph::new(format!(" {}", truncate_cells(message, area.width.saturating_sub(1) as usize)))
            .style(Style::default().fg(theme.muted).bg(theme.panel))
            .render(area, buf);
        return;
    }
    let branch = git.branch.as_deref().unwrap_or("detached");
    let create = " + worktree ";
    let branch_width = area.width.saturating_sub(cell_width(create) as u16);
    Paragraph::new(format!(" branch {}", truncate_cells(branch, branch_width.saturating_sub(8) as usize)))
        .style(Style::default().fg(theme.teal).bg(theme.panel).add_modifier(Modifier::BOLD))
        .render(Rect::new(area.x, area.y, branch_width, 1), buf);
    if branch_width < area.width {
        let create_rect = Rect::new(area.x + branch_width, area.y, area.width - branch_width, 1);
        Paragraph::new(create)
            .style(Style::default().fg(theme.active_tab_text).bg(theme.accent).add_modifier(Modifier::BOLD))
            .render(create_rect, buf);
        layout.hits.push(HitRegion { rect: create_rect, target: HitTarget::CreateWorktree });
    }
    let area = Rect::new(area.x, area.y + 1, area.width, area.height.saturating_sub(1));
    let capacity = area.height as usize;
    let detail_count = if git.status.is_empty() { git.recent_commits.len() } else { git.status.len() };
    let count = git.worktrees.len().saturating_add(detail_count);
    if count == 0 {
        Paragraph::new(" clean workspace")
            .style(Style::default().fg(theme.green).bg(theme.panel))
            .render(area, buf);
        return;
    }
    let start = app.git_scroll.min(count.saturating_sub(capacity));
    for (visible, index) in (start..count).take(capacity).enumerate() {
        let y = area.y + visible as u16;
        let row = Rect::new(area.x, y, area.width, 1);
        if let Some(worktree) = git.worktrees.get(index) {
            let state = if worktree.is_main { "main" } else if worktree.locked { "locked" } else if worktree.prunable { "prunable" } else { "worktree" };
            let branch = worktree.branch.as_deref().unwrap_or("detached");
            let action = if worktree.workspace_id.is_some() { " open " } else { " add " };
            let removable = !worktree.is_main && !worktree.is_bare && !worktree.locked && !worktree.prunable;
            let remove = if removable { " rm " } else { "" };
            let reserved = cell_width(action) + cell_width(remove);
            let label = format!(" {state} {branch} · {}", worktree.path);
            layout.hits.push(HitRegion { rect: row, target: HitTarget::Worktree(index) });
            Paragraph::new(truncate_cells(&label, area.width.saturating_sub(reserved as u16) as usize))
                .style(Style::default().fg(theme.text).bg(if index == app.git_cursor { theme.active } else { theme.panel }))
                .render(Rect::new(row.x, row.y, row.width.saturating_sub(reserved as u16), 1), buf);
            let action_x = row.right().saturating_sub(reserved as u16);
            let action_rect = Rect::new(action_x, y, cell_width(action) as u16, 1);
            Paragraph::new(action).style(Style::default().fg(theme.teal).bg(theme.panel)).render(action_rect, buf);
            layout.hits.push(HitRegion {
                rect: action_rect,
                target: if worktree.workspace_id.is_some() { HitTarget::Worktree(index) } else { HitTarget::RegisterWorktree(index) },
            });
            if removable {
                let remove_rect = Rect::new(action_rect.right(), y, cell_width(remove) as u16, 1);
                Paragraph::new(remove).style(Style::default().fg(theme.red).bg(theme.panel)).render(remove_rect, buf);
                layout.hits.push(HitRegion { rect: remove_rect, target: HitTarget::RemoveWorktree(index) });
            }
        } else {
            let detail_index = index - git.worktrees.len();
            let line = if let Some(entry) = git.status.get(detail_index).filter(|_| !git.status.is_empty()) {
                let code = format!("{}{}", entry.index_status, entry.worktree_status);
                Line::from_spans(vec![
                    Span::styled(format!(" {code} "), Style::default().fg(theme.yellow).add_modifier(Modifier::BOLD)),
                    Span::styled(truncate_cells(&entry.path, area.width.saturating_sub(4) as usize), Style::default().fg(theme.text)),
                ])
            } else if let Some(commit) = git.recent_commits.get(detail_index) {
                Line::from_spans(vec![
                    Span::styled(format!(" {} ", commit.id), Style::default().fg(theme.accent)),
                    Span::styled(truncate_cells(&commit.summary, area.width.saturating_sub(commit.id.len() as u16 + 2) as usize), Style::default().fg(theme.dim)),
                ])
            } else {
                Line::from_spans(Vec::new())
            };
            Paragraph::new(Text::from_lines(vec![line]))
                .style(Style::default().bg(if index == app.git_cursor { theme.active } else { theme.panel }))
                .render(row, buf);
            layout.hits.push(HitRegion { rect: row, target: HitTarget::SidebarItem(index) });
        }
    }
}

fn render_inspector_empty(app: &App, area: Rect, buf: &mut TerminalBuffer, text: &str, theme: Theme) {
    if area.height == 0 {
        return;
    }
    let text = if app.workspace_inspection_pending() { "loading..." } else { text };
    Paragraph::new(format!(" {text}"))
        .style(Style::default().fg(theme.muted).bg(theme.panel))
        .render(area, buf);
}

fn render_roster(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    if area.height == 0 {
        return;
    }
    for x in area.x..area.right() {
        let cell = buf.get_mut(x, area.y);
        cell.symbol = "─".into();
        cell.style = Style::default().fg(theme.border).bg(theme.panel);
    }
    if area.height < 3 {
        return;
    }
    let mut x = area.x;
    for mode in [RosterMode::Agents, RosterMode::Workspaces] {
        let label = format!(" {} ", mode.id());
        let width = (cell_width(&label) as u16).min(area.right().saturating_sub(x));
        if width == 0 {
            break;
        }
        let selected = app.roster_mode == mode;
        Paragraph::new(truncate_cells(&label, width as usize))
            .style(
                Style::default()
                    .fg(if selected { theme.active_tab_text } else { theme.muted })
                    .bg(if selected { theme.accent } else { theme.panel })
                    .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
            )
            .render(Rect::new(x, area.y + 1, width, 1), buf);
        layout.hits.push(HitRegion {
            rect: Rect::new(x, area.y + 1, width, 1),
            target: HitTarget::RosterMode(mode),
        });
        x = x.saturating_add(width);
    }

    let content = Rect::new(area.x, area.y + 2, area.width, area.height.saturating_sub(2));
    match app.roster_mode {
        RosterMode::Agents => render_agent_list(app, content, buf, layout, theme),
        RosterMode::Workspaces => render_space_list(app, content, buf, layout, theme),
    }
}

fn render_agent_list(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    if area.height == 0 {
        return;
    }
    let add_label = " + agent ";
    let add_width = (cell_width(add_label) as u16).min(area.width);
    let add_x = area.right().saturating_sub(add_width);
    Paragraph::new(add_label)
        .style(Style::default().fg(theme.teal).bg(theme.panel).add_modifier(Modifier::BOLD))
        .render(Rect::new(add_x, area.y, add_width, 1), buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(add_x, area.y, add_width, 1),
        target: HitTarget::AddAgent,
    });

    let rows = app.agent_rows();
    let capacity = area.height.saturating_sub(1) as usize / 2;
    let start = app
        .agents_scroll
        .min(rows.len().saturating_sub(capacity));
    for (visible_index, (index, key)) in rows
        .iter()
        .enumerate()
        .skip(start)
        .take(capacity)
        .enumerate()
    {
        let y = area.y + 1 + visible_index as u16 * 2;
        let selected = index == app.selected_agent;
        let background = if selected { theme.active } else { theme.panel };
        let (title, marker, color, secondary) = match key {
            AgentRowKey::Managed { .. } => {
                let Some(record) = app.find_managed_session(key) else {
                    continue;
                };
                let active = record
                    .active_session
                    .as_ref()
                    .and_then(|address| app.find_session(address));
                let (marker, color, state) = if record.state
                    == gate4agent_node_protocol::ManagedSessionState::Live
                {
                    active
                        .map(|session| session_state(session, theme))
                        .unwrap_or_else(|| managed_session_state(record.state, theme))
                } else {
                    managed_session_state(record.state, theme)
                };
                let state = if record.mode == gate4agent_node_protocol::SessionMode::Inline {
                    format!("{state} inline")
                } else {
                    state.to_owned()
                };
                (
                    record.short_title().to_owned(),
                    marker,
                    color,
                    format!(
                        "{state} | {} | {} | {}",
                        record.provider, record.workspace_id, record.node_id
                    ),
                )
            }
            AgentRowKey::Legacy(address) => {
                let Some(session) = app.find_session(address) else {
                    continue;
                };
                let (marker, color, status) = session_state(session, theme);
                (
                    session.short_title(),
                    marker,
                    color,
                    format!("{status} | {} | {}", address.workspace_id, address.node_id),
                )
            }
        };
        Paragraph::new(Text::from_lines(vec![Line::from_spans(vec![
            Span::styled(format!(" {marker} "), Style::default().fg(color)),
            Span::styled(
                truncate_cells(&title, area.width.saturating_sub(3) as usize),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
        ])]))
        .style(Style::default().bg(background))
        .render(Rect::new(area.x, y, area.width, 1), buf);
        Paragraph::new(format!(
            "   {}",
            compact_middle_cells(&secondary, area.width.saturating_sub(3) as usize)
        ))
        .style(
            Style::default()
                .fg(if selected { color } else { theme.dim })
                .bg(background),
        )
        .render(Rect::new(area.x, y + 1, area.width, 1), buf);
        layout.hits.push(HitRegion {
            rect: Rect::new(area.x, y, area.width, 2),
            target: HitTarget::Agent(index),
        });
    }
}

fn managed_session_state(
    state: gate4agent_node_protocol::ManagedSessionState,
    theme: Theme,
) -> (&'static str, Color, &'static str) {
    use gate4agent_node_protocol::ManagedSessionState;
    match state {
        ManagedSessionState::Live => ("*", theme.green, managed_state_label(state)),
        ManagedSessionState::IdentityPending => ("~", theme.yellow, managed_state_label(state)),
        ManagedSessionState::Dormant => ("o", theme.teal, managed_state_label(state)),
        ManagedSessionState::Unavailable => ("x", theme.red, managed_state_label(state)),
    }
}

fn render_tabs(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let settings_label = " ⚙ ";
    let settings_width = cell_width(settings_label).min(area.width as usize) as u16;
    let tabs_right = area.right().saturating_sub(settings_width);
    Paragraph::new("")
        .style(Style::default().bg(theme.active))
        .render(Rect::new(area.x, area.y, tabs_right.saturating_sub(area.x), 1), buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(area.x, area.y, tabs_right.saturating_sub(area.x), 1),
        target: HitTarget::TabDrop,
    });
    let labels = app
        .tabs
        .iter()
        .map(|tab| {
            app.session_title(&tab.address)
                .unwrap_or_else(|| format!("detached #{}", tab.address.instance_id))
        })
        .collect::<Vec<_>>();
    let grid_label = if app.grid.panes.is_empty() {
        " grid ".to_owned()
    } else {
        format!(" grid:{} ", app.grid.panes.len())
    };
    let preset_width = if app.surface_mode == SurfaceMode::Grid {
        GridPreset::ALL
            .iter()
            .map(|preset| cell_width(&format!(" {} ", preset.id())) as u16)
            .fold(0_u16, |total, width| total.saturating_add(width))
    } else {
        0
    };
    let controls_width = 3_u16
        .saturating_add(cell_width(&grid_label) as u16)
        .saturating_add(preset_width);
    let tabs_limit = tabs_right.saturating_sub(controls_width).max(area.x);
    let available = tabs_limit.saturating_sub(area.x);
    let selected_end = labels
        .iter()
        .take(app.selected_tab.saturating_add(1))
        .map(|label| cell_width(&format!(" {label} ")).min(u16::MAX as usize) as u16)
        .fold(0_u16, |sum, width| sum.saturating_add(width));
    let start = if selected_end.saturating_add(3) <= available {
        0
    } else {
        app.selected_tab.min(labels.len().saturating_sub(1))
    };
    let mut spans = Vec::new();
    let mut x = area.x;
    for (index, label) in labels.iter().enumerate().skip(start) {
        let text = format!(" {label} ");
        let width = cell_width(&text).min(u16::MAX as usize) as u16;
        if x >= tabs_limit {
            break;
        }
        let drawn = width.min(tabs_limit - x);
        let selected = app.surface_mode == SurfaceMode::Tab && index == app.selected_tab;
        spans.push(Span::styled(
            truncate_cells(&text, drawn as usize),
            Style::default()
                .fg(if selected { theme.active_tab_text } else { theme.muted })
                .bg(if selected { theme.accent } else { theme.active })
                .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
        ));
        layout.hits.push(HitRegion {
            rect: Rect::new(x, area.y, drawn, 1),
            target: HitTarget::Tab(index),
        });
        x = x.saturating_add(drawn);
    }
    Paragraph::new(Text::from_lines(vec![Line::from_spans(spans)]))
        .style(Style::default().bg(theme.active))
        .render(Rect::new(area.x, area.y, x.saturating_sub(area.x), 1), buf);
    if x < tabs_limit {
        let width = 3.min(tabs_limit - x);
        Paragraph::new(" + ")
            .style(Style::default().fg(theme.muted).bg(theme.active))
            .render(Rect::new(x, area.y, width, 1), buf);
        layout.hits.push(HitRegion {
            rect: Rect::new(x, area.y, width, 1),
            target: HitTarget::AddTab,
        });
        x = x.saturating_add(width);
    }
    layout.tab_drop = Rect::new(area.x, area.y, x.saturating_sub(area.x), 1);

    let grid_width = (cell_width(&grid_label) as u16).min(tabs_right.saturating_sub(x));
    if grid_width > 0 {
        let selected = app.surface_mode == SurfaceMode::Grid;
        Paragraph::new(truncate_cells(&grid_label, grid_width as usize))
            .style(
                Style::default()
                    .fg(if selected { theme.active_tab_text } else { theme.muted })
                    .bg(if selected { theme.accent } else { theme.active })
                    .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
            )
            .render(Rect::new(x, area.y, grid_width, 1), buf);
        layout.grid_drop = Rect::new(x, area.y, grid_width, 1);
        layout.hits.push(HitRegion {
            rect: layout.grid_drop,
            target: HitTarget::GridToggle,
        });
        x = x.saturating_add(grid_width);
    }
    if app.surface_mode == SurfaceMode::Grid {
        for preset in GridPreset::ALL {
            if x >= tabs_right {
                break;
            }
            let label = format!(" {} ", preset.id());
            let width = (cell_width(&label) as u16).min(tabs_right - x);
            let selected = app.grid.preset == preset;
            Paragraph::new(truncate_cells(&label, width as usize))
                .style(
                    Style::default()
                        .fg(if selected { theme.active_tab_text } else { theme.muted })
                        .bg(if selected { theme.accent } else { theme.active })
                        .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
                )
                .render(Rect::new(x, area.y, width, 1), buf);
            layout.hits.push(HitRegion {
                rect: Rect::new(x, area.y, width, 1),
                target: HitTarget::GridPreset(preset),
            });
            x = x.saturating_add(width);
        }
    }
    let settings_x = area.right().saturating_sub(settings_width);
    Paragraph::new(settings_label)
        .style(
            Style::default()
                .fg(if app.focus == Focus::Settings { theme.accent } else { theme.muted })
                .bg(theme.active),
        )
        .render(Rect::new(settings_x, area.y, settings_width, 1), buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(settings_x, area.y, settings_width, 1),
        target: HitTarget::Settings,
    });
}

fn render_surface(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    match app.surface_mode {
        SurfaceMode::Tab => render_viewport(app, area, buf, layout),
        SurfaceMode::Grid => render_grid(app, area, buf, layout, theme),
    }
}

fn render_viewport(app: &App, area: Rect, buf: &mut TerminalBuffer, layout: &mut LayoutRects) {
    layout.hits.push(HitRegion { rect: area, target: HitTarget::Viewport });
    let Some(session) = app.selected_tab_session() else {
        return;
    };
    render_terminal(
        session,
        area,
        app.color_mode,
        app.terminal_scroll_offset(&session.address),
        buf,
    );
}

fn render_grid(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    layout.hits.push(HitRegion { rect: area, target: HitTarget::Viewport });
    let (column_specs, row_specs): (Vec<(usize, u16)>, Vec<(usize, u16)>) =
        match app.grid.preset {
            GridPreset::Quad => (
                vec![(1, app.grid.column_cuts[1])],
                vec![(1, app.grid.row_cuts[1])],
            ),
            GridPreset::Columns => (
                app.grid
                    .column_cuts
                    .iter()
                    .copied()
                    .enumerate()
                    .collect(),
                Vec::new(),
            ),
            GridPreset::Rows => (
                Vec::new(),
                app.grid.row_cuts.iter().copied().enumerate().collect(),
            ),
        };
    let (columns, column_dividers) = axis_segments(area.x, area.width, &column_specs);
    let (rows, row_dividers) = axis_segments(area.y, area.height, &row_specs);

    for (index, x) in column_dividers.iter().copied() {
        let divider = Rect::new(x, area.y, 1, area.height);
        for y in divider.y..divider.bottom() {
            let cell = buf.get_mut(divider.x, y);
            cell.symbol = "│".into();
            cell.style = Style::default().fg(theme.border).bg(theme.surface);
        }
        layout.hits.push(HitRegion {
            rect: divider,
            target: HitTarget::GridDivider(GridAxisKind::Columns, index),
        });
    }
    for (index, y) in row_dividers.iter().copied() {
        let divider = Rect::new(area.x, y, area.width, 1);
        for x in divider.x..divider.right() {
            let cell = buf.get_mut(x, divider.y);
            cell.symbol = "─".into();
            cell.style = Style::default().fg(theme.border).bg(theme.surface);
        }
        layout.hits.push(HitRegion {
            rect: divider,
            target: HitTarget::GridDivider(GridAxisKind::Rows, index),
        });
    }
    for (_, x) in &column_dividers {
        for (_, y) in &row_dividers {
            let cell = buf.get_mut(*x, *y);
            cell.symbol = "┼".into();
            cell.style = Style::default().fg(theme.border).bg(theme.surface);
        }
    }

    let mut slot = 0;
    for (row_y, row_height) in rows {
        for (column_x, column_width) in columns.iter().copied() {
            if slot >= 4 {
                break;
            }
            let frame = Rect::new(column_x, row_y, column_width, row_height);
            layout.hits.push(HitRegion {
                rect: frame,
                target: HitTarget::GridDropSlot(slot),
            });
            let header = Rect::new(frame.x, frame.y, frame.width, frame.height.min(1));
            let viewport = Rect::new(
                frame.x,
                frame.y.saturating_add(header.height),
                frame.width,
                frame.height.saturating_sub(header.height),
            );
            if let Some(pane) = app.grid.panes.get(slot) {
                let selected = app.grid.focused == slot;
                let label = app
                    .session_title(&pane.address)
                    .unwrap_or_else(|| format!("detached #{}", pane.address.instance_id));
                Paragraph::new(format!(" {label} "))
                    .style(
                        Style::default()
                            .fg(if selected { theme.active_tab_text } else { theme.muted })
                            .bg(if selected { theme.accent } else { theme.active })
                            .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
                    )
                    .render(header, buf);
                layout.hits.push(HitRegion {
                    rect: header,
                    target: HitTarget::GridPaneHeader(slot),
                });
                layout.hits.push(HitRegion {
                    rect: viewport,
                    target: HitTarget::GridPaneBody(slot),
                });
                layout.grid_panes.push(GridPaneLayout {
                    pane_index: slot,
                    frame,
                    header,
                    viewport,
                });
                if let Some(session) = app.find_session(&pane.address) {
                    render_terminal(
                        session,
                        viewport,
                        app.color_mode,
                        app.terminal_scroll_offset(&session.address),
                        buf,
                    );
                }
            } else {
                Paragraph::new(" + drop ")
                    .style(Style::default().fg(theme.muted).bg(theme.active))
                    .render(header, buf);
            }
            slot += 1;
        }
    }
}

fn render_terminal(
    session: &SessionView,
    area: Rect,
    color_mode: PtyColorMode,
    scroll_offset: usize,
    buf: &mut TerminalBuffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if session.terminal_formatted.is_empty() && session.terminal_scrollback.is_empty() {
        return;
    }
    let mut parser = vt100::Parser::new(area.height.max(1), area.width.max(1), 0);
    parser.process(&session.terminal_formatted);
    let mut current = TerminalBuffer::new(area.width, area.height);
    uzor_tui::vt100_to_buffer(parser.screen(), &mut current);
    apply_pty_palette(&mut current, color_mode);
    let history_len = session.terminal_scrollback.len();
    let scroll_offset = scroll_offset.min(history_len);
    let first_logical_row = history_len.saturating_sub(scroll_offset);
    for row in 0..area.height {
        let logical_row = first_logical_row.saturating_add(row as usize);
        let history_row = if logical_row < history_len {
            let mut row_parser = vt100::Parser::new(1, area.width.max(1), 0);
            row_parser.process(&session.terminal_scrollback[logical_row]);
            let mut row_buffer = TerminalBuffer::new(area.width, 1);
            uzor_tui::vt100_to_buffer(row_parser.screen(), &mut row_buffer);
            apply_pty_palette(&mut row_buffer, color_mode);
            Some(row_buffer)
        } else {
            None
        };
        let current_row = logical_row.saturating_sub(history_len) as u16;
        for column in 0..area.width {
            let cell = if let Some(history_row) = &history_row {
                history_row.get(column, 0).clone()
            } else if current_row < area.height {
                current.get(column, current_row).clone()
            } else {
                current.get(column, area.height.saturating_sub(1)).clone()
            };
            buf.set(area.x + column, area.y + row, cell);
        }
    }
}

fn axis_segments(
    origin: u16,
    length: u16,
    cuts: &[(usize, u16)],
) -> (Vec<(u16, u16)>, Vec<(usize, u16)>) {
    let divider_count = cuts.len().min(length.saturating_sub(1) as usize);
    let part_count = divider_count + 1;
    let usable = length.saturating_sub(divider_count as u16);
    if usable == 0 {
        return (vec![(origin, length)], Vec::new());
    }
    let mut boundaries = Vec::with_capacity(divider_count);
    let mut previous = 0_u16;
    for (position, (index, cut)) in cuts.iter().copied().take(divider_count).enumerate() {
        let remaining_parts = divider_count.saturating_sub(position) as u16;
        let minimum = previous.saturating_add(1);
        let maximum = usable.saturating_sub(remaining_parts).max(minimum);
        let boundary = ((u32::from(usable) * u32::from(cut.min(10_000))) / 10_000) as u16;
        let boundary = boundary.clamp(minimum, maximum);
        boundaries.push((index, boundary));
        previous = boundary;
    }

    let mut segments = Vec::with_capacity(part_count);
    let mut dividers = Vec::with_capacity(divider_count);
    let mut previous_boundary = 0_u16;
    for (divider_offset, (index, boundary)) in boundaries.iter().copied().enumerate() {
        let start = origin
            .saturating_add(previous_boundary)
            .saturating_add(divider_offset as u16);
        let width = boundary.saturating_sub(previous_boundary);
        segments.push((start, width));
        let divider = start.saturating_add(width);
        dividers.push((index, divider));
        previous_boundary = boundary;
    }
    let start = origin
        .saturating_add(previous_boundary)
        .saturating_add(divider_count as u16);
    segments.push((start, usable.saturating_sub(previous_boundary)));
    (segments, dividers)
}

fn render_spawn(app: &App, area: Rect, buf: &mut TerminalBuffer, theme: Theme) {
    let Some(spawn) = &app.spawn else {
        return;
    };
    let width = 46.min(area.width.saturating_sub(4));
    let height = 7.min(area.height.saturating_sub(2));
    let dialog = centered(area, width, height);
    fill_rect(dialog, theme.modal, buf);
    let providers = app
        .nodes
        .iter()
        .find(|node| node.node_id == spawn.node_id)
        .and_then(|node| node.workspaces.iter().find(|workspace| workspace.workspace_id == spawn.workspace_id))
        .map(|workspace| {
            workspace
                .providers
                .iter()
                .filter(|inventory| inventory.enabled)
                .map(|inventory| inventory.provider)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .iter()
        .map(|provider| if *provider == spawn.provider { format!("[{provider}]") } else { provider.to_string() })
        .collect::<Vec<_>>()
        .join("  ");
    Paragraph::new(Text::from_lines(vec![
        Line::styled(
            format!("workspace  {} / {}", spawn.node_id, spawn.workspace_id),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Line::styled("Up/Down workspace", Style::default().fg(theme.muted)),
        Line::styled(providers, Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Line::styled("Left/Right provider  Enter spawn  Esc cancel", Style::default().fg(theme.muted)),
    ]))
    .block(
        Block::bordered()
            .title(" new PTY agent ")
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.modal)),
    )
    .style(Style::default().fg(theme.text).bg(theme.modal))
    .render(dialog, buf);
}

fn render_add_space(app: &App, area: Rect, buf: &mut TerminalBuffer, theme: Theme) {
    let Some(dialog) = &app.add_space else {
        return;
    };
    let width = 58.min(area.width.saturating_sub(4));
    let height = 9.min(area.height.saturating_sub(2));
    let modal = centered(area, width, height);
    fill_rect(modal, theme.modal, buf);
    let workspace_prefix = if dialog.field == AddSpaceField::WorkspaceId { ">" } else { " " };
    let root_prefix = if dialog.field == AddSpaceField::Root { ">" } else { " " };
    Paragraph::new(Text::from_lines(vec![
        Line::styled(format!("node  {}", dialog.node_id), Style::default().fg(theme.muted)),
        Line::styled(format!("{workspace_prefix} id    {}", dialog.workspace_id), Style::default().fg(theme.text)),
        Line::styled(format!("{root_prefix} root  {}", dialog.root), Style::default().fg(theme.text)),
        Line::styled("Up/Down node · Tab field · Enter register · Esc cancel", Style::default().fg(theme.muted)),
    ]))
    .block(
        Block::bordered()
            .title(" add space ")
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.modal)),
    )
    .style(Style::default().fg(theme.text).bg(theme.modal))
    .render(modal, buf);
}

fn render_create_worktree(app: &App, area: Rect, buf: &mut TerminalBuffer, theme: Theme) {
    let Some(dialog) = &app.create_worktree else {
        return;
    };
    let width = 72.min(area.width.saturating_sub(4));
    let height = 12.min(area.height.saturating_sub(2));
    let modal = centered(area, width, height);
    fill_rect(modal, theme.modal, buf);
    let prefix = |field| if dialog.field == field { ">" } else { " " };
    Paragraph::new(Text::from_lines(vec![
        Line::styled(
            format!("source  {} / {}", dialog.node_id, dialog.source_workspace_id),
            Style::default().fg(theme.muted),
        ),
        Line::styled(
            format!("{} id      {}", prefix(CreateWorktreeField::WorkspaceId), dialog.workspace_id),
            Style::default().fg(theme.text),
        ),
        Line::styled(
            format!("{} root    {}", prefix(CreateWorktreeField::TargetRoot), dialog.target_root),
            Style::default().fg(theme.text),
        ),
        Line::styled(
            format!("{} branch  {}", prefix(CreateWorktreeField::Branch), dialog.branch),
            Style::default().fg(theme.text),
        ),
        Line::styled(
            format!("{} base    {}", prefix(CreateWorktreeField::Base), if dialog.base.is_empty() { "(HEAD)" } else { &dialog.base }),
            Style::default().fg(theme.text),
        ),
        Line::styled("Tab field · Enter create and register · Esc cancel", Style::default().fg(theme.muted)),
    ]))
    .block(
        Block::bordered()
            .title(" create git worktree ")
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.modal)),
    )
    .style(Style::default().fg(theme.text).bg(theme.modal))
    .render(modal, buf);
}

fn render_remove_worktree(app: &App, area: Rect, buf: &mut TerminalBuffer, theme: Theme) {
    let Some(dialog) = &app.remove_worktree else {
        return;
    };
    let width = 68.min(area.width.saturating_sub(4));
    let height = 8.min(area.height.saturating_sub(2));
    let modal = centered(area, width, height);
    fill_rect(modal, theme.modal, buf);
    Paragraph::new(Text::from_lines(vec![
        Line::styled(
            dialog.branch.as_deref().unwrap_or("detached worktree"),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Line::styled(truncate_cells(&dialog.target_root, width.saturating_sub(4) as usize), Style::default().fg(theme.dim)),
        Line::styled("Git refuses dirty or unsafe removal; no force is used.", Style::default().fg(theme.yellow)),
        Line::styled("Enter/y remove · n/Esc cancel", Style::default().fg(theme.muted)),
    ]))
    .block(
        Block::bordered()
            .title(" confirm worktree removal ")
            .border_style(Style::default().fg(theme.red))
            .style(Style::default().bg(theme.modal)),
    )
    .style(Style::default().fg(theme.text).bg(theme.modal))
    .render(modal, buf);
}

fn render_rename_session(app: &App, area: Rect, buf: &mut TerminalBuffer, theme: Theme) {
    let Some(dialog) = &app.rename_session else {
        return;
    };
    let width = 58.min(area.width.saturating_sub(4));
    let height = 7.min(area.height.saturating_sub(2));
    let modal = centered(area, width, height);
    fill_rect(modal, theme.modal, buf);
    Paragraph::new(Text::from_lines(vec![
        Line::styled(
            format!("{} / {}", dialog.node_id, dialog.record_id),
            Style::default().fg(theme.muted),
        ),
        Line::styled(
            format!("> {}", dialog.display_name),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Line::styled("Enter rename | Esc cancel", Style::default().fg(theme.muted)),
    ]))
    .block(
        Block::bordered()
            .title(" rename session ")
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.modal)),
    )
    .style(Style::default().fg(theme.text).bg(theme.modal))
    .render(modal, buf);
}

fn render_forget_session(app: &App, area: Rect, buf: &mut TerminalBuffer, theme: Theme) {
    let Some(dialog) = &app.forget_session else {
        return;
    };
    let width = 62.min(area.width.saturating_sub(4));
    let height = 8.min(area.height.saturating_sub(2));
    let modal = centered(area, width, height);
    fill_rect(modal, theme.modal, buf);
    Paragraph::new(Text::from_lines(vec![
        Line::styled(
            dialog.display_name.clone(),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!("{} / {}", dialog.node_id, dialog.record_id),
            Style::default().fg(theme.dim),
        ),
        Line::styled(
            "Only the Gate4Agent record is removed; provider history stays intact.",
            Style::default().fg(theme.yellow),
        ),
        Line::styled("Enter/y forget | n/Esc cancel", Style::default().fg(theme.muted)),
    ]))
    .block(
        Block::bordered()
            .title(" forget dormant session ")
            .border_style(Style::default().fg(theme.red))
            .style(Style::default().bg(theme.modal)),
    )
    .style(Style::default().fg(theme.text).bg(theme.modal))
    .render(modal, buf);
}

fn render_settings(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let expanded = app.menu_placement == MenuPlacement::Modal;
    let available_width = area.width.saturating_sub(6);
    let available_height = area.height.saturating_sub(4);
    let (default_width, default_height) = control_modal_default_size(app);
    let (requested_width, requested_height) = if expanded {
        app.control_modal_size
            .unwrap_or((default_width, default_height))
    } else {
        (44, 7)
    };
    let width = requested_width
        .clamp(36.min(available_width), available_width);
    let height = requested_height
        .clamp(6.min(available_height), available_height);
    let modal = positioned_modal(area, width, height, app.control_modal_position);
    layout.control_modal = modal;
    fill_rect(modal, theme.modal, buf);
    Block::bordered()
        .title(if expanded { " control " } else { " settings " })
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.modal))
        .render(modal, buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(modal.x, modal.y, modal.width, 1),
        target: HitTarget::ControlDrag,
    });
    if expanded && modal.width > 0 && modal.height > 0 {
        let resize = Rect::new(modal.right() - 1, modal.bottom() - 1, 1, 1);
        let cell = buf.get_mut(resize.x, resize.y);
        cell.symbol = "◢".into();
        cell.style = Style::default().fg(theme.accent).bg(theme.modal);
        layout.hits.push(HitRegion {
            rect: resize,
            target: HitTarget::ControlResize,
        });
    }
    if modal.width < 12 || modal.height < 5 {
        return;
    }
    let inner = Rect::new(
        modal.x + 1,
        modal.y + 1,
        modal.width.saturating_sub(2),
        modal.height.saturating_sub(2),
    );
    fill_rect(inner, theme.modal, buf);
    if !expanded {
        render_settings_controls(app, inner, buf, layout, theme);
        return;
    }

    let section_count = ControlSection::ALL.len() as u16;
    let base_width = inner.width / section_count;
    let remainder = inner.width % section_count;
    let mut x = inner.x;
    for (index, section) in ControlSection::ALL.into_iter().enumerate() {
        let width = base_width + u16::from((index as u16) < remainder);
        let label = centered_label(section.id(), width as usize);
        let selected = app.control_section == section;
        Paragraph::new(label)
            .style(
                Style::default()
                    .fg(if selected { theme.active_tab_text } else { theme.muted })
                    .bg(if selected { theme.accent } else { theme.modal })
                    .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
            )
            .render(Rect::new(x, inner.y, width, 1), buf);
        layout.hits.push(HitRegion {
            rect: Rect::new(x, inner.y, width, 1),
            target: HitTarget::ControlSection(section),
        });
        x = x.saturating_add(width);
    }

    let content = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.bottom().saturating_sub(inner.y + 1),
    );
    layout.control_content = content;
    fill_rect(content, theme.modal, buf);
    let mut content_theme = theme;
    content_theme.panel = theme.modal;
    content_theme.active = theme.modal;
    match app.control_section {
        ControlSection::Files => render_workspace_files(app, content, buf, layout, content_theme),
        ControlSection::Git => render_workspace_git(app, content, buf, layout, content_theme),
        ControlSection::Agents => render_agent_list(app, content, buf, layout, content_theme),
        ControlSection::Workspaces => render_space_list(app, content, buf, layout, content_theme),
        ControlSection::Settings => render_settings_controls(app, content, buf, layout, content_theme),
    }
}

fn control_modal_default_size(app: &App) -> (u16, u16) {
    let tab_width = ControlSection::ALL
        .iter()
        .map(|section| cell_width(section.id()) + 2)
        .sum::<usize>()
        .max(42);
    let (content_width, content_rows) = match app.control_section {
        ControlSection::Files => {
            let indices = app.visible_workspace_entry_indices();
            let width = app
                .selected_workspace_inspection()
                .map(|inspection| {
                    indices
                        .iter()
                        .filter_map(|index| inspection.entries.get(*index))
                        .map(|entry| cell_width(&entry.relative_path) + 8)
                        .max()
                        .unwrap_or(24)
                })
                .unwrap_or(24);
            (width, indices.len().saturating_add(1))
        }
        ControlSection::Git => {
            let Some(inspection) = app.selected_workspace_inspection() else {
                return ((tab_width + 2).min(u16::MAX as usize) as u16, 8);
            };
            let git = &inspection.git;
            if !git.status.is_empty() {
                (
                    git.status
                        .iter()
                        .map(|entry| cell_width(&entry.path) + 5)
                        .max()
                        .unwrap_or(24),
                    git.status.len().saturating_add(2),
                )
            } else {
                (
                    git.recent_commits
                        .iter()
                        .map(|commit| cell_width(&commit.summary) + commit.id.len() + 3)
                        .max()
                        .unwrap_or(24),
                    git.recent_commits.len().saturating_add(2),
                )
            }
        }
        ControlSection::Agents => {
            let rows = app.agent_rows();
            let width = rows
                .iter()
                .map(|key| {
                    if let Some(record) = app.find_managed_session(key) {
                        return cell_width(record.short_title())
                            .max(cell_width(&record.workspace_id) + cell_width(&record.node_id) + 10)
                            + 3;
                    }
                    let Some(address) = app.agent_row_active_address(key) else {
                        return 24;
                    };
                    app.find_session(&address).map_or(24, |session| {
                        cell_width(&session.short_title())
                            .max(cell_width(&session.address.workspace_id) + cell_width(&session.address.node_id) + 7)
                            + 3
                    })
                })
                .max()
                .unwrap_or(24);
            (width, rows.len().saturating_mul(2).saturating_add(1))
        }
        ControlSection::Workspaces => {
            let rows = app.space_rows();
            let width = rows
                .iter()
                .map(|(node_index, workspace_index)| {
                    let node = &app.nodes[*node_index];
                    let workspace = &node.workspaces[*workspace_index];
                    cell_width(&workspace.label)
                        .max(cell_width(&workspace.canonical_root) + cell_width(&node.node_id) + 3)
                        + 12
                })
                .max()
                .unwrap_or(24);
            (width, rows.len().saturating_mul(2).saturating_add(1))
        }
        ControlSection::Settings => (40, 5),
    };
    let width = tab_width.max(content_width).saturating_add(2).clamp(44, 96);
    let height = content_rows.saturating_add(3).clamp(7, 30);
    (
        width.min(u16::MAX as usize) as u16,
        height.min(u16::MAX as usize) as u16,
    )
}

fn centered_label(label: &str, width: usize) -> String {
    let label = truncate_cells(label, width);
    let label_width = cell_width(&label);
    let left = width.saturating_sub(label_width) / 2;
    let right = width.saturating_sub(label_width).saturating_sub(left);
    format!("{}{}{}", " ".repeat(left), label, " ".repeat(right))
}

fn render_settings_controls(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    fill_rect(area, theme.modal, buf);
    if area.height == 0 {
        return;
    }
    let style_text = if app.color_mode == PtyColorMode::Inherited {
        " style [inherit]|gate "
    } else {
        " style inherit|[gate] "
    };
    let style_width = (cell_width(style_text) as u16).min(area.width);
    Paragraph::new(style_text)
        .style(Style::default().fg(theme.teal).bg(theme.modal))
        .render(Rect::new(area.x, area.y, style_width, 1), buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(area.x, area.y, style_width, 1),
        target: HitTarget::SettingsStyle,
    });
    if area.height < 2 {
        return;
    }
    let placement_text = if app.menu_placement == MenuPlacement::Sidebar {
        " menu [sidebar]|modal "
    } else {
        " menu sidebar|[modal] "
    };
    let placement_width = (cell_width(placement_text) as u16).min(area.width);
    Paragraph::new(placement_text)
        .style(Style::default().fg(theme.teal).bg(theme.modal))
        .render(Rect::new(area.x, area.y + 1, placement_width, 1), buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(area.x, area.y + 1, placement_width, 1),
        target: HitTarget::SettingsPlacement,
    });
    if area.height < 3 {
        return;
    }
    let presentation_text = if app.sidebar_presentation == SidebarPresentation::Split {
        " sidebar [split]|activity "
    } else {
        " sidebar split|[activity] "
    };
    let presentation_width = (cell_width(presentation_text) as u16).min(area.width);
    Paragraph::new(presentation_text)
        .style(Style::default().fg(theme.teal).bg(theme.modal))
        .render(Rect::new(area.x, area.y + 2, presentation_width, 1), buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(area.x, area.y + 2, presentation_width, 1),
        target: HitTarget::SettingsPresentation,
    });
    if area.height < 4 {
        return;
    }
    let collapsed_text = if app.sidebar_collapsed {
        " panel shown|[collapsed] "
    } else {
        " panel [shown]|collapsed "
    };
    let collapsed_width = (cell_width(collapsed_text) as u16).min(area.width);
    Paragraph::new(collapsed_text)
        .style(Style::default().fg(theme.teal).bg(theme.modal))
        .render(Rect::new(area.x, area.y + 3, collapsed_width, 1), buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(area.x, area.y + 3, collapsed_width, 1),
        target: HitTarget::SettingsSidebarCollapsed,
    });
}

fn render_drag_preview(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &LayoutRects,
    theme: Theme,
) {
    let Some(DragState::SessionChip {
        address,
        current_column,
        current_row,
        moved: true,
        ..
    }) = &app.drag_state
    else {
        return;
    };
    let drop = layout.hits.iter().rev().find(|hit| {
        hit.rect.contains(*current_column, *current_row)
            && matches!(
                &hit.target,
                HitTarget::Tab(_)
                    | HitTarget::AddTab
                    | HitTarget::TabDrop
                    | HitTarget::GridToggle
                    | HitTarget::GridPreset(_)
                    | HitTarget::GridPaneHeader(_)
                    | HitTarget::GridPaneBody(_)
                    | HitTarget::GridDropSlot(_)
            )
    });
    if let Some(drop) = drop {
        match &drop.target {
            HitTarget::GridPaneHeader(index)
            | HitTarget::GridPaneBody(index)
            | HitTarget::GridDropSlot(index) => {
                let target = layout
                    .grid_panes
                    .iter()
                    .find(|pane| pane.pane_index == *index)
                    .map(|pane| pane.frame)
                    .unwrap_or(drop.rect);
                draw_outline(target, theme.accent, theme.surface, buf);
            }
            _ => {
                for row in drop.rect.y..drop.rect.bottom() {
                    for column in drop.rect.x..drop.rect.right() {
                        let cell = buf.get_mut(column, row);
                        cell.style = cell
                            .style
                            .fg(theme.active_tab_text)
                            .bg(theme.accent)
                            .add_modifier(Modifier::BOLD);
                    }
                }
            }
        }
    }

    let title = app
        .session_title(address)
        .unwrap_or_else(|| format!("detached #{}", address.instance_id));
    let label = format!(" {title} ");
    let width = (cell_width(&label) as u16).min(area.width);
    if width == 0 || area.height == 0 {
        return;
    }
    let maximum_x = area.right().saturating_sub(width);
    let x = current_column
        .saturating_sub(width / 2)
        .clamp(area.x, maximum_x);
    let y = (*current_row).clamp(area.y, area.bottom().saturating_sub(1));
    Paragraph::new(truncate_cells(&label, width as usize))
        .style(
            Style::default()
                .fg(theme.active_tab_text)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )
        .render(Rect::new(x, y, width, 1), buf);
}

fn draw_outline(area: Rect, color: Color, background: Color, buf: &mut TerminalBuffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    for x in area.x..area.right() {
        let top = buf.get_mut(x, area.y);
        top.symbol = "─".into();
        top.style = Style::default().fg(color).bg(background);
        if area.height > 1 {
            let bottom = buf.get_mut(x, area.bottom() - 1);
            bottom.symbol = "─".into();
            bottom.style = Style::default().fg(color).bg(background);
        }
    }
    for y in area.y..area.bottom() {
        let left = buf.get_mut(area.x, y);
        left.symbol = "│".into();
        left.style = Style::default().fg(color).bg(background);
        if area.width > 1 {
            let right = buf.get_mut(area.right() - 1, y);
            right.symbol = "│".into();
            right.style = Style::default().fg(color).bg(background);
        }
    }
    if area.width > 1 && area.height > 1 {
        for (x, y, symbol) in [
            (area.x, area.y, "┌"),
            (area.right() - 1, area.y, "┐"),
            (area.x, area.bottom() - 1, "└"),
            (area.right() - 1, area.bottom() - 1, "┘"),
        ] {
            let cell = buf.get_mut(x, y);
            cell.symbol = symbol.into();
            cell.style = Style::default().fg(color).bg(background);
        }
    }
}

fn render_notice(notice: &str, viewport: Rect, buf: &mut TerminalBuffer, theme: Theme) {
    let width = (cell_width(notice) as u16 + 2).min(viewport.width);
    if width == 0 || viewport.height == 0 {
        return;
    }
    Paragraph::new(format!(" {notice}"))
        .style(Style::default().fg(theme.yellow).bg(theme.active))
        .render(
            Rect::new(
                viewport.right().saturating_sub(width),
                viewport.bottom().saturating_sub(1),
                width,
                1,
            ),
            buf,
        );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn positioned_modal(
    area: Rect,
    width: u16,
    height: u16,
    position: Option<(u16, u16)>,
) -> Rect {
    let centered = centered(area, width, height);
    let Some((x, y)) = position else {
        return centered;
    };
    Rect::new(
        x.clamp(area.x, area.right().saturating_sub(width)),
        y.clamp(area.y, area.bottom().saturating_sub(height)),
        width,
        height,
    )
}

fn fill_rect(area: Rect, background: Color, buf: &mut TerminalBuffer) {
    for row in area.y..area.bottom() {
        for column in area.x..area.right() {
            let cell = buf.get_mut(column, row);
            cell.symbol = " ".into();
            cell.style = Style::default().bg(background);
        }
    }
}

fn workspace_entry_dirty(
    relative_path: &str,
    kind: WorkspaceEntryKind,
    git: &GitSnapshot,
) -> Option<bool> {
    let path = relative_path.replace('\\', "/");
    let directory_prefix = format!("{path}/");
    git.status
        .iter()
        .filter(|entry| {
            let changed_path = entry.path.replace('\\', "/");
            match kind {
                WorkspaceEntryKind::Directory => {
                    changed_path == path || changed_path.starts_with(&directory_prefix)
                }
                WorkspaceEntryKind::File => changed_path == path,
            }
        })
        .map(|entry| {
            entry
                .index_status
                .chars()
                .chain(entry.worktree_status.chars())
                .any(|status| matches!(status, 'D' | 'U'))
        })
        .reduce(|left, right| left || right)
}

fn workspace_state(node: &NodeView, workspace: &WorkspaceView, theme: Theme) -> (&'static str, Color) {
    if !matches!(node.connection, ConnectionState::Connected) {
        return ("·", theme.muted);
    }
    if workspace.sessions.iter().any(|session| session.attention) {
        ("●", theme.red)
    } else if workspace.sessions.iter().any(|session| session.running) {
        ("●", theme.yellow)
    } else if !workspace.sessions.is_empty() {
        ("●", theme.teal)
    } else {
        ("·", theme.muted)
    }
}

fn session_state<'a>(session: &'a SessionView, theme: Theme) -> (&'static str, Color, &'a str) {
    if session.attention {
        ("●", theme.red, "blocked")
    } else if session.running {
        ("●", theme.yellow, "working")
    } else if session.stoppable {
        ("●", theme.yellow, session.status.as_str())
    } else if session.restartable {
        ("●", theme.teal, "done")
    } else {
        ("○", theme.green, "idle")
    }
}

fn cell_width(value: &str) -> usize {
    value.chars().map(char_cell_width).sum()
}

fn char_cell_width(character: char) -> usize {
    let code = character as u32;
    if character.is_control()
        || matches!(
            code,
            0x0300..=0x036f
                | 0x1ab0..=0x1aff
                | 0x1dc0..=0x1dff
                | 0x20d0..=0x20ff
                | 0xfe00..=0xfe0f
                | 0xfe20..=0xfe2f
        )
    {
        0
    } else if matches!(
        code,
        0x1100..=0x115f
            | 0x2329..=0x232a
            | 0x2e80..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f300..=0x1faff
            | 0x20000..=0x3fffd
    ) {
        2
    } else {
        1
    }
}

fn truncate_cells(value: &str, max_cells: usize) -> String {
    if cell_width(value) <= max_cells {
        return value.to_owned();
    }
    if max_cells == 0 {
        return String::new();
    }
    format!("{}…", take_prefix_cells(value, max_cells - 1))
}

fn compact_middle_cells(value: &str, max_cells: usize) -> String {
    if cell_width(value) <= max_cells {
        return value.to_owned();
    }
    if max_cells == 0 {
        return String::new();
    }
    let head = (max_cells - 1) / 2;
    let tail = max_cells - head - 1;
    format!("{}…{}", take_prefix_cells(value, head), take_suffix_cells(value, tail))
}

fn take_prefix_cells(value: &str, max_cells: usize) -> String {
    let mut used = 0;
    value
        .chars()
        .take_while(|character| {
            let width = char_cell_width(*character);
            if used + width > max_cells {
                false
            } else {
                used += width;
                true
            }
        })
        .collect()
}

fn take_suffix_cells(value: &str, max_cells: usize) -> String {
    let mut used = 0;
    let mut chars = value
        .chars()
        .rev()
        .take_while(|character| {
            let width = char_cell_width(*character);
            if used + width > max_cells {
                false
            } else {
                used += width;
                true
            }
        })
        .collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_node_protocol::{
        GitCommitSummary, GitStatusEntry, GitWorktreeSnapshot, ManagedSessionState, SessionMode,
        WorkspaceEntry, WorkspaceId,
    };
    use gate4agent_types::TerminalMouseProtocolEncoding;
    use crate::app::{
        DragSource, ManagedSessionView, NodeView, Provider, ProviderInventory, SessionAddress,
        SessionTab, SessionView, WorkspaceView,
    };

    fn fixture(mode: PtyColorMode) -> App {
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 1,
            generation: 1,
        };
        let mut app = App::default();
        app.color_mode = mode;
        app.nodes.push(NodeView {
            node_id: "node-a".to_owned(),
            endpoint: "pipe".to_owned(),
            connection: ConnectionState::Connected,
            controller_owned: true,
            event_sequence: 1,
            session_records: Vec::new(),
            workspaces: vec![WorkspaceView {
                workspace_id: "workspace-a".to_owned(),
                label: "nemo".to_owned(),
                canonical_root: r"C:\work\nemo".to_owned(),
                providers: vec![ProviderInventory { provider: Provider::Kimi, enabled: true }],
                sessions: vec![SessionView {
                    address: address.clone(),
                    provider: Provider::Kimi,
                    status: "running".to_owned(),
                    running: true,
                    stoppable: true,
                    removable: false,
                    restartable: false,
                    attention: false,
                    has_provider_session_identity: true,
                    terminal_formatted: b"\x1b[38;2;80;160;255;48;2;0;51;102mK".to_vec(),
                    terminal_scrollback: Vec::new(),
                    terminal_alternate_screen: false,
                    terminal_mouse_protocol_enabled: false,
                    terminal_mouse_protocol_encoding: TerminalMouseProtocolEncoding::Default,
                    terminal_cursor: Some((0, 1)),
                }],
            }],
        });
        app.tabs.push(SessionTab { address });
        app
    }

    fn inspection() -> WorkspaceInspection {
        WorkspaceInspection {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            entries: vec![
                WorkspaceEntry {
                    relative_path: "src".to_owned(),
                    kind: WorkspaceEntryKind::Directory,
                },
                WorkspaceEntry {
                    relative_path: "src/main.rs".to_owned(),
                    kind: WorkspaceEntryKind::File,
                },
            ],
            tree_truncated: false,
            git: GitSnapshot {
                is_repository: true,
                branch: Some("main".to_owned()),
                status: vec![GitStatusEntry {
                    index_status: " ".to_owned(),
                    worktree_status: "M".to_owned(),
                    path: "src/main.rs".to_owned(),
                }],
                recent_commits: vec![GitCommitSummary {
                    id: "abcdef0".to_owned(),
                    summary: "ship workspace controls".to_owned(),
                }],
                worktrees: Vec::new(),
                truncated: false,
                diagnostic: None,
            },
        }
    }

    #[test]
    fn sidebar_geometry_uses_configured_width_split_and_drag_hits() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.sidebar_width = 32;
        app.sidebar_split_percent = 40;
        let mut buf = TerminalBuffer::new(100, 24);
        let layout = render(&app, &mut buf);
        assert_eq!(layout.spaces.width, 31);
        assert_eq!(layout.agents.width, 31);
        assert_eq!(layout.spaces.height, 9);
        assert_eq!(layout.agents.height, 15);
        assert_eq!(layout.tabs.height, 1);
        assert_eq!(layout.viewport, Rect::new(32, 1, 68, 23));
        assert_eq!(buf.get(31, 0).symbol, "│");
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::SidebarWidthDrag));
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::SidebarSplitDrag));
    }

    #[test]
    fn inspector_roster_and_tabs_have_compact_mode_and_action_hits() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.roster_mode = RosterMode::Workspaces;
        let mut buf = TerminalBuffer::new(100, 24);
        let workspace_layout = render(&app, &mut buf);
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::AddSpace));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::RemoveSpace));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::SpawnSpace(0)));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::SidebarMode(SidebarMode::Files)));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::SidebarMode(SidebarMode::Git)));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::RosterMode(RosterMode::Agents)));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::RosterMode(RosterMode::Workspaces)));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::AddTab));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::Settings));

        app.roster_mode = RosterMode::Agents;
        let agent_layout = render(&app, &mut buf);
        assert!(agent_layout.hits.iter().any(|hit| hit.target == HitTarget::AddAgent));
        assert!(agent_layout.hits.iter().all(|hit| !matches!(hit.target, HitTarget::Viewport) || hit.rect == agent_layout.viewport));
    }

    #[test]
    fn workspace_window_uses_scroll_offset_without_moving_selection() {
        let mut app = fixture(PtyColorMode::Inherited);
        let template = app.nodes[0].workspaces[0].clone();
        for index in 1..8 {
            let mut workspace = template.clone();
            workspace.workspace_id = format!("workspace-{index}");
            workspace.label = format!("space-{index}");
            workspace.canonical_root = format!(r"C:\work\space-{index}");
            workspace.sessions.clear();
            app.nodes[0].workspaces.push(workspace);
        }
        app.selected_space = 7;
        app.roster_mode = RosterMode::Workspaces;
        let mut buf = TerminalBuffer::new(60, 12);

        let top_layout = render(&app, &mut buf);
        assert!(top_layout.hits.iter().any(|hit| hit.target == HitTarget::Space(0)));
        assert!(!top_layout.hits.iter().any(|hit| hit.target == HitTarget::Space(7)));

        app.workspaces_scroll = 7;
        let scrolled_layout = render(&app, &mut buf);
        assert!(scrolled_layout.hits.iter().any(|hit| hit.target == HitTarget::Space(7)));
        assert!(!scrolled_layout.hits.iter().any(|hit| hit.target == HitTarget::Space(0)));
    }

    #[test]
    fn terminal_scrollback_shifts_history_into_the_viewport() {
        let mut app = fixture(PtyColorMode::Inherited);
        let address = app.tabs[0].address.clone();
        let session = &mut app.nodes[0].workspaces[0].sessions[0];
        session.terminal_scrollback = vec![b"H".to_vec()];
        session.terminal_formatted = b"C".to_vec();
        app.terminal_scroll_offsets.insert(address, 1);
        let mut buf = TerminalBuffer::new(100, 24);

        let layout = render(&app, &mut buf);

        assert_eq!(buf.get(layout.viewport.x, layout.viewport.y).symbol, "H");
        assert_eq!(buf.get(layout.viewport.x, layout.viewport.y + 1).symbol, "C");
    }

    #[test]
    fn inherited_keeps_native_pty_rgb_and_background() {
        let app = fixture(PtyColorMode::Inherited);
        let mut buf = TerminalBuffer::new(100, 24);
        let layout = render(&app, &mut buf);
        let cell = buf.get(layout.viewport.x, layout.viewport.y);
        assert_eq!(cell.symbol, "K");
        assert_eq!(cell.style.fg, Color::Rgb(80, 160, 255));
        assert_eq!(cell.style.bg, Color::Rgb(0, 51, 102));
    }

    #[test]
    fn gate_override_removes_provider_solid_background() {
        let app = fixture(PtyColorMode::GateOverride);
        let mut buf = TerminalBuffer::new(100, 24);
        let layout = render(&app, &mut buf);
        let cell = buf.get(layout.viewport.x, layout.viewport.y);
        assert_eq!(cell.symbol, "K");
        assert_ne!(cell.style.fg, Color::Rgb(80, 160, 255));
        assert_eq!(cell.style.bg, TERM_BG);
    }

    #[test]
    fn no_session_viewport_is_blank_without_fake_copy() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.tabs.clear();
        let mut buf = TerminalBuffer::new(100, 24);
        let layout = render(&app, &mut buf);
        assert_eq!(buf.get(layout.viewport.x, layout.viewport.y).symbol, " ");
    }

    #[test]
    fn inherited_modals_are_opaque_over_provider_output() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.focus = Focus::AddSpace;
        app.add_space = Some(crate::app::AddSpaceDialog {
            node_id: "node-a".to_owned(),
            workspace_id: "scratch".to_owned(),
            root: r"C:\work\scratch".to_owned(),
            field: AddSpaceField::WorkspaceId,
        });
        let mut buf = TerminalBuffer::new(100, 24);

        render(&app, &mut buf);

        let modal = centered(Rect::new(0, 0, 100, 24), 58, 9);
        let blank_interior = buf.get(modal.x + 2, modal.y + 6);
        assert_eq!(blank_interior.symbol, " ");
        assert_eq!(blank_interior.style.bg, Color::Black);
    }

    #[test]
    fn modal_menu_placement_gives_tabs_and_viewport_the_full_width() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.menu_placement = MenuPlacement::Modal;
        let mut buf = TerminalBuffer::new(100, 24);

        let layout = render(&app, &mut buf);

        assert_eq!(layout.spaces, Rect::default());
        assert_eq!(layout.agents, Rect::default());
        assert_eq!(layout.tabs, Rect::new(0, 0, 100, 1));
        assert_eq!(layout.viewport, Rect::new(0, 1, 100, 23));
        assert!(!layout.hits.iter().any(|hit| matches!(
            hit.target,
            HitTarget::SidebarMode(_) | HitTarget::RosterMode(_)
        )));
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::Settings));
    }

    #[test]
    fn sidebar_gear_opens_only_compact_positioned_settings() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.focus = Focus::Settings;
        app.control_modal_position = Some((99, 99));
        let mut buf = TerminalBuffer::new(100, 24);

        let layout = render(&app, &mut buf);

        assert_eq!(layout.control_modal, Rect::new(56, 17, 44, 7));
        assert_eq!(layout.control_content, Rect::default());
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::ControlDrag));
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::SettingsStyle));
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::SettingsPlacement));
        assert!(!layout.hits.iter().any(|hit| matches!(hit.target, HitTarget::ControlSection(_))));
        assert_eq!(buf.get(layout.control_modal.x + 2, layout.control_modal.y + 4).style.bg, Color::Black);
    }

    #[test]
    fn control_modal_is_opaque_and_reuses_operational_sections() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.apply_workspace_inspection("node-a".to_owned(), inspection());
        app.menu_placement = MenuPlacement::Modal;
        app.control_section = ControlSection::Files;
        app.focus = Focus::Settings;
        let mut buf = TerminalBuffer::new(100, 24);

        let layout = render(&app, &mut buf);

        for section in ControlSection::ALL {
            assert!(layout
                .hits
                .iter()
                .any(|hit| hit.target == HitTarget::ControlSection(section)));
        }
        let section_hits = layout
            .hits
            .iter()
            .filter(|hit| matches!(hit.target, HitTarget::ControlSection(_)))
            .collect::<Vec<_>>();
        let narrowest = section_hits.iter().map(|hit| hit.rect.width).min().unwrap();
        let widest = section_hits.iter().map(|hit| hit.rect.width).max().unwrap();
        assert!(widest.saturating_sub(narrowest) <= 1);
        assert_eq!(
            section_hits.iter().map(|hit| hit.rect.width).sum::<u16>(),
            layout.control_modal.width.saturating_sub(2)
        );
        assert_eq!(ControlSection::ALL.len(), 5);
        assert!(layout.control_content.width > 0);
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::ControlDrag));
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::ControlResize));
        assert!(layout.control_modal.width < 96);
        assert!(layout.control_modal.height < 24);
        assert!(!layout.hits.iter().any(|hit| hit.target == HitTarget::RefreshWorkspace));
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::SidebarItem(0)));

        app.control_modal_size = Some((72, 18));
        let resized_layout = render(&app, &mut buf);
        assert_eq!((resized_layout.control_modal.width, resized_layout.control_modal.height), (72, 18));

        app.control_section = ControlSection::Settings;
        let settings_layout = render(&app, &mut buf);
        assert!(settings_layout.hits.iter().any(|hit| hit.target == HitTarget::SettingsStyle));
        let placement_hit = settings_layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::SettingsPlacement)
            .expect("menu placement control");
        let row = (placement_hit.rect.x..placement_hit.rect.right())
            .map(|x| buf.get(x, placement_hit.rect.y).symbol.as_str())
            .collect::<String>();
        assert!(row.contains("menu sidebar|[modal]"));
        assert_eq!(buf.get(settings_layout.control_content.x, settings_layout.control_content.bottom() - 1).style.bg, Color::Black);

        app.control_section = ControlSection::Workspaces;
        let workspace_layout = render(&app, &mut buf);
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::AddSpace));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::RemoveSpace));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::SpawnSpace(0)));
    }

    #[test]
    fn files_and_git_modes_render_bounded_workspace_inspection() {
        let mut app = fixture(PtyColorMode::GateOverride);
        app.apply_workspace_inspection("node-a".to_owned(), inspection());
        app.sidebar_mode = SidebarMode::Files;
        let mut files = TerminalBuffer::new(100, 24);
        let files_layout = render(&app, &mut files);
        assert!(!files_layout.hits.iter().any(|hit| hit.target == HitTarget::RefreshWorkspace));
        assert!(files_layout.hits.iter().any(|hit| hit.target == HitTarget::SidebarItem(0)));

        app.sidebar_mode = SidebarMode::Git;
        let mut git = TerminalBuffer::new(100, 24);
        let git_layout = render(&app, &mut git);
        assert!(!git_layout.hits.iter().any(|hit| hit.target == HitTarget::RefreshWorkspace));
        assert!(git_layout.hits.iter().any(|hit| hit.target == HitTarget::SidebarItem(0)));
    }

    #[test]
    fn git_worktree_rows_expose_create_open_register_remove_and_shift_detail_hits() {
        let mut app = fixture(PtyColorMode::GateOverride);
        let mut snapshot = inspection();
        snapshot.git.worktrees = vec![
            GitWorktreeSnapshot {
                path: r"C:\work\main".to_owned(),
                head: "aaaa".to_owned(),
                branch: Some("main".to_owned()),
                is_bare: false,
                is_main: true,
                locked: false,
                lock_reason: None,
                prunable: false,
                prunable_reason: None,
                workspace_id: Some(WorkspaceId::new("workspace-a").unwrap()),
            },
            GitWorktreeSnapshot {
                path: r"C:\work\feature".to_owned(),
                head: "bbbb".to_owned(),
                branch: Some("feature/a".to_owned()),
                is_bare: false,
                is_main: false,
                locked: false,
                lock_reason: None,
                prunable: false,
                prunable_reason: None,
                workspace_id: None,
            },
        ];
        app.apply_workspace_inspection("node-a".to_owned(), snapshot);
        app.sidebar_mode = SidebarMode::Git;
        let mut buf = TerminalBuffer::new(100, 24);

        let layout = render(&app, &mut buf);

        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::CreateWorktree));
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::Worktree(0)));
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::RegisterWorktree(1)));
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::RemoveWorktree(1)));
        assert!(!layout.hits.iter().any(|hit| hit.target == HitTarget::RemoveWorktree(0)));
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::SidebarItem(2)));
    }

    #[test]
    fn files_render_dirty_state_and_hide_collapsed_children() {
        let mut app = fixture(PtyColorMode::GateOverride);
        app.apply_workspace_inspection("node-a".to_owned(), inspection());
        app.sidebar_mode = SidebarMode::Files;
        let mut expanded = TerminalBuffer::new(100, 24);
        let expanded_layout = render(&app, &mut expanded);
        let child_hit = expanded_layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::SidebarItem(1))
            .expect("expanded file row");
        let marker = expanded.get(child_hit.rect.x + 3, child_hit.rect.y);
        assert_eq!(marker.symbol, "M");
        assert_eq!(marker.style.fg, YELLOW);

        app.collapsed_directories.insert((
            "node-a".to_owned(),
            "workspace-a".to_owned(),
            "src".to_owned(),
        ));
        let mut collapsed = TerminalBuffer::new(100, 24);
        let collapsed_layout = render(&app, &mut collapsed);
        assert!(!collapsed_layout
            .hits
            .iter()
            .any(|hit| hit.target == HitTarget::SidebarItem(1)));
        let directory_hit = collapsed_layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::SidebarItem(0))
            .expect("collapsed directory row");
        assert_eq!(collapsed.get(directory_hit.rect.x + 1, directory_hit.rect.y).symbol, ">");
    }

    #[test]
    fn grid_renders_independent_native_ptys_and_four_drop_slots() {
        let mut app = fixture(PtyColorMode::Inherited);
        let first = app.tabs[0].address.clone();
        let second = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 2,
            generation: 1,
        };
        let mut second_session = app.nodes[0].workspaces[0].sessions[0].clone();
        second_session.address = second.clone();
        second_session.terminal_formatted = b"B".to_vec();
        app.nodes[0].workspaces[0].sessions.push(second_session);
        assert!(app.move_address_to_grid(first, None));
        assert!(app.move_address_to_grid(second, None));
        app.set_grid_preset(GridPreset::Quad);
        let mut buf = TerminalBuffer::new(120, 32);

        let layout = render(&app, &mut buf);

        assert_eq!(layout.grid_panes.len(), 2);
        assert_eq!(
            layout
                .hits
                .iter()
                .filter(|hit| matches!(hit.target, HitTarget::GridDropSlot(_)))
                .count(),
            4
        );
        assert!(layout.hits.iter().any(|hit| {
            hit.target == HitTarget::GridDivider(GridAxisKind::Columns, 1)
        }));
        assert!(layout.hits.iter().any(|hit| {
            hit.target == HitTarget::GridDivider(GridAxisKind::Rows, 1)
        }));
        let column = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::GridDivider(GridAxisKind::Columns, 1))
            .unwrap()
            .rect;
        let row = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::GridDivider(GridAxisKind::Rows, 1))
            .unwrap()
            .rect;
        assert_eq!(buf.get(column.x, column.y).symbol, "│");
        assert_eq!(buf.get(row.x, row.y).symbol, "─");
        assert_eq!(buf.get(column.x, row.y).symbol, "┼");
        assert_eq!(
            buf.get(layout.grid_panes[0].viewport.x, layout.grid_panes[0].viewport.y)
                .symbol,
            "K"
        );
        assert_eq!(
            buf.get(layout.grid_panes[1].viewport.x, layout.grid_panes[1].viewport.y)
                .symbol,
            "B"
        );
        assert!(layout.grid_panes[0].viewport.right() < layout.grid_panes[1].viewport.x);
    }

    #[test]
    fn session_drag_renders_a_pointer_ghost_and_drop_outline() {
        let mut app = fixture(PtyColorMode::GateOverride);
        let address = app.tabs[0].address.clone();
        assert!(app.move_address_to_grid(address.clone(), None));
        let mut buf = TerminalBuffer::new(120, 32);
        let base = render(&app, &mut buf);
        let target = base
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::GridDropSlot(1))
            .unwrap()
            .rect;
        let current_column = target.x + target.width / 2;
        let current_row = target.y + target.height / 2;
        app.drag_state = Some(DragState::SessionChip {
            source: DragSource::Pane(0),
            address,
            start_column: 1,
            start_row: 1,
            current_column,
            current_row,
            moved: true,
        });

        render(&app, &mut buf);

        assert_eq!(buf.get(target.x, target.y).symbol, "┌");
        assert_eq!(buf.get(target.x, target.y).style.fg, MAUVE);
        assert_eq!(buf.get(current_column, current_row).style.bg, MAUVE);
    }

    #[test]
    fn grid_presets_keep_four_slots_and_expose_matching_dividers() {
        let mut app = fixture(PtyColorMode::GateOverride);
        let address = app.tabs[0].address.clone();
        assert!(app.move_address_to_grid(address, None));
        app.menu_placement = MenuPlacement::Modal;
        let mut buf = TerminalBuffer::new(120, 32);

        for preset in GridPreset::ALL {
            app.set_grid_preset(preset);
            let layout = render(&app, &mut buf);
            assert_eq!(
                layout
                    .hits
                    .iter()
                    .filter(|hit| matches!(hit.target, HitTarget::GridDropSlot(_)))
                    .count(),
                4
            );
            let column_dividers = layout
                .hits
                .iter()
                .filter(|hit| matches!(hit.target, HitTarget::GridDivider(GridAxisKind::Columns, _)))
                .count();
            let row_dividers = layout
                .hits
                .iter()
                .filter(|hit| matches!(hit.target, HitTarget::GridDivider(GridAxisKind::Rows, _)))
                .count();
            match preset {
                GridPreset::Quad => assert_eq!((column_dividers, row_dividers), (1, 1)),
                GridPreset::Columns => assert_eq!((column_dividers, row_dividers), (3, 0)),
                GridPreset::Rows => assert_eq!((column_dividers, row_dividers), (0, 3)),
            }
            assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::GridToggle));
            assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::TabDrop));
            assert!(GridPreset::ALL.iter().all(|preset| layout
                .hits
                .iter()
                .any(|hit| hit.target == HitTarget::GridPreset(*preset))));
        }
    }

    #[test]
    fn axis_segments_preserve_extent_and_minimum_cells() {
        let (segments, dividers) = axis_segments(
            7,
            41,
            &[(0, 2_000), (1, 6_000), (2, 8_500)],
        );
        assert_eq!(segments.len(), 4);
        assert_eq!(dividers.len(), 3);
        assert!(segments.iter().all(|(_, width)| *width > 0));
        assert_eq!(
            segments.iter().map(|(_, width)| *width).sum::<u16>()
                + dividers.len() as u16,
            41
        );
        assert_eq!(segments[0].0, 7);
        assert_eq!(segments.last().unwrap().0 + segments.last().unwrap().1, 48);
    }

    #[test]
    fn managed_session_states_render_in_sidebar_and_modal_from_the_same_rows() {
        let mut app = fixture(PtyColorMode::GateOverride);
        let states = [
            ("pending", ManagedSessionState::IdentityPending),
            ("live", ManagedSessionState::Live),
            ("dormant", ManagedSessionState::Dormant),
            ("unavailable", ManagedSessionState::Unavailable),
        ];
        let live_address = app.tabs[0].address.clone();
        for (name, state) in states {
            app.nodes[0].session_records.push(ManagedSessionView {
                node_id: "node-a".to_owned(),
                record_id: format!("record-{name}"),
                display_name: format!("{name} session"),
                provider: Provider::Codex,
                mode: SessionMode::Pty,
                state,
                workspace_id: "workspace-a".to_owned(),
                canonical_root: r"C:\work\nemo".to_owned(),
                has_provider_session_identity: state != ManagedSessionState::IdentityPending,
                active_session: matches!(
                    state,
                    ManagedSessionState::Live | ManagedSessionState::IdentityPending
                )
                    .then_some(live_address.clone()),
                last_error: (state == ManagedSessionState::Unavailable)
                    .then_some("workspace missing".to_owned()),
            });
        }
        let mut sidebar = TerminalBuffer::new(100, 24);
        let sidebar_layout = render(&app, &mut sidebar);
        assert_eq!(
            sidebar_layout
                .hits
                .iter()
                .filter(|hit| matches!(hit.target, HitTarget::Agent(_)))
                .count(),
            4
        );
        let sidebar_text = (0..sidebar.height())
            .flat_map(|row| (0..sidebar.width()).map(move |column| (column, row)))
            .map(|(column, row)| sidebar.get(column, row).symbol.as_str())
            .collect::<String>();
        assert!(sidebar_text.contains("identity"));

        app.menu_placement = MenuPlacement::Modal;
        app.focus = Focus::Settings;
        app.control_section = ControlSection::Agents;
        app.control_modal_size = Some((70, 18));
        let mut modal = TerminalBuffer::new(100, 24);
        let modal_layout = render(&app, &mut modal);
        assert_eq!(
            modal_layout
                .hits
                .iter()
                .filter(|hit| matches!(hit.target, HitTarget::Agent(_)))
                .count(),
            4
        );
    }

    #[test]
    fn activity_rail_keeps_controls_when_selected_sidebar_is_collapsed() {
        let mut app = fixture(PtyColorMode::GateOverride);
        app.sidebar_presentation = SidebarPresentation::Activity;
        app.control_section = ControlSection::Agents;
        let mut expanded = TerminalBuffer::new(100, 24);
        let expanded_layout = render(&app, &mut expanded);
        assert_eq!(expanded_layout.activity_rail.width, 3);
        assert!(expanded_layout.agents.width > 0);
        assert!(ControlSection::ALL.iter().take(4).all(|section| expanded_layout
            .hits
            .iter()
            .any(|hit| hit.target == HitTarget::ActivitySection(*section))));

        app.sidebar_collapsed = true;
        let mut collapsed = TerminalBuffer::new(100, 24);
        let collapsed_layout = render(&app, &mut collapsed);
        assert_eq!(collapsed_layout.activity_rail.width, 3);
        assert_eq!(collapsed_layout.agents.width, 0);
        assert_eq!(collapsed_layout.tabs.x, 3);
        assert!(collapsed_layout
            .hits
            .iter()
            .any(|hit| hit.target == HitTarget::SidebarCollapse));
    }
}
