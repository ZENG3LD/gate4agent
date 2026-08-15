use uzor_tui::{
    split, Block, Color, Constraint, Direction, Line, Modifier, Paragraph, Rect, Span, Style,
    TerminalBuffer, Text, Widget,
};
use gate4agent_c2_protocol::C2RelayRoute;
use gate4agent_node_protocol::{
    GitSnapshot, ManagedWorktreeCleanupFailure, ManagedWorktreeLeaseSnapshot,
    ManagedWorktreeLeaseState, ManagedWorktreeRetention, ResolvedSpawnReceipt, SessionMode,
    WorkspaceEntryKind, WorkspaceInspection,
};
use gate4agent_observation_api::{ProjectionAvailability, ProjectionFreshness};
use gate4agent_observation_engine::{ContextOccupancyProvenance, ContextOccupancySnapshot, CorrelationProjection, CorrelationState, SessionProjection};

use crate::app::{
    compact_task_id, host_path_display, managed_state_label, repository_path_display,
    normalize_workspace_entry_input, provider_supports_native_resume,
    repository_path_file_name_display, surface_drop_zone, AddSpaceField, AgentBoardCard,
    AgentBoardColumn, AgentRowKey, App,
    ConnectionState, ControlSection, CreateWorktreeField, DragState, ExistingSessionField,
    ContextUsageHover, ContextUsageSegment, ContextUsageSegmentHit,
    ExistingSessionMode, ExistingSessionOperation, Focus, FolderBrowserField,
    AgentRunGitScopeView, GitLocationDialogKind, HitRegion, HitTarget, LaunchContextMode, LaunchField, LaunchTarget,
    LayoutRects, MenuPlacement, NativeSessionGroupKey, NativeSessionTreeItem, NodeView, PreviewTabPhase, PreviewTabView, PtyColorMode, RosterMode, SessionView,
    ObservationPersistenceState, SessionMonitorKey, SessionMonitorSection, SessionMonitorTarget,
    SessionMonitorView, SidebarMode, SurfaceTab,
    NativeSessionCatalogState, NativeSessionPreviewState, SidebarPresentation, SurfacePaneLayout,
    WorkspaceFileState, WorkspaceFileTabView, WorkspaceGitPaneMode, WorkspaceGitState,
    WorkspaceGitTabView, WorkspaceView,
    MAX_BROWSER_LOADED_ENTRIES, RESTORE_VIA_SKILL_DISABLED_REASON,
};
use crate::pty_palette::{apply_pty_palette, GATE_FG, TERM_BG};
use crate::surface::{
    LayoutPreset, PaneBranch, PaneId, PaneNode, PaneSplitPath, SplitAxis, SurfaceDropZone,
};

const SIDEBAR_BG: Color = Color::Rgb(24, 24, 37);
const ACTIVE_BG: Color = Color::Rgb(30, 30, 46);
const BORDER: Color = Color::Rgb(49, 50, 68);
const MUTED: Color = Color::Rgb(108, 112, 134);
const DIM: Color = Color::Rgb(147, 153, 178);
const MAUVE: Color = Color::Rgb(203, 166, 247);

fn relay_route_label(route: C2RelayRoute) -> &'static str {
    match route {
        C2RelayRoute::LocalIpc => "Local IPC",
        C2RelayRoute::SshForwardedLoopback => "SSH forwarded",
        C2RelayRoute::Unknown => "Unknown",
    }
}

fn node_route_value(app: &App, node_id: &str) -> String {
    let route = app
        .nodes
        .iter()
        .find(|node| node.node_id == node_id)
        .map(|node| node.relay_route)
        .unwrap_or(C2RelayRoute::Unknown);
    format!("{node_id} | {}", relay_route_label(route))
}
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
    diff_added: Color,
    diff_deleted: Color,
    diff_hunk: Color,
    diff_meta: Color,
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
                diff_added: Color::Indexed(22),
                diff_deleted: Color::Indexed(52),
                diff_hunk: Color::Indexed(24),
                diff_meta: Color::Indexed(0),
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
                diff_added: Color::Indexed(22),
                diff_deleted: Color::Indexed(52),
                diff_hunk: Color::Indexed(24),
                diff_meta: Color::Indexed(0),
            },
        }
    }
}

pub fn render(app: &App, buf: &mut TerminalBuffer) -> LayoutRects {
    buf.clear();
    let area = Rect::new(0, 0, buf.width(), buf.height());
    let theme = Theme::for_mode(app.color_mode);
    if area.width < 56 || area.height < 14 {
        Paragraph::new("gate4agent operator needs at least 56x14")
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
        spawn_modal: Rect::default(),
        existing_session_modal: Rect::default(),
        add_space_modal: Rect::default(),
        folder_browser_modal: Rect::default(),
        create_worktree_modal: Rect::default(),
        surface_panes: Vec::new(),
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
                ControlSection::Agents | ControlSection::Workspaces => match app.roster_mode {
                    RosterMode::Agents | RosterMode::NativeSessions => {
                        render_agents_surface(app, agents, buf, &mut layout, theme)
                    }
                    RosterMode::Workspaces => render_space_list(app, agents, buf, &mut layout, theme),
                },
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
        render_spawn(app, area, buf, &mut layout, theme);
    }
    if app.focus == Focus::ExistingSession {
        render_existing_session(app, area, buf, &mut layout, theme);
    }
    if app.focus == Focus::AddSpace {
        render_add_space(app, area, buf, &mut layout, theme);
    }
    if app.focus == Focus::FolderBrowser {
        render_folder_browser(app, area, buf, &mut layout, theme);
    }
    if app.focus == Focus::CreateWorkspaceEntry {
        render_create_workspace_entry(app, area, buf, &mut layout, theme);
    }
    if app.focus == Focus::CreateWorktree {
        render_create_worktree(app, area, buf, &mut layout, theme);
    }
    if app.focus == Focus::RemoveWorktree {
        render_remove_worktree(app, area, buf, theme);
    }
    if app.focus == Focus::RenameSession {
        render_rename_session(app, area, buf, theme);
    }
    if app.focus == Focus::TaskId {
        render_task_id(app, area, buf, theme);
    }
    if app.focus == Focus::ForgetSession {
        render_forget_session(app, area, buf, theme);
    }
    if app.focus == Focus::History {
        render_history(app, area, buf, theme);
    }
    if app.focus == Focus::Settings {
        render_settings(app, area, buf, &mut layout, theme);
    }
    if app.agent_menu.is_some() {
        render_agent_menu(app, area, buf, &mut layout, theme);
    }
    if app.native_session_menu.is_some() {
        render_native_session_menu(app, area, buf, &mut layout, theme);
    }
    render_drag_preview(app, area, buf, &layout, theme);
    if let Some(notice) = &app.notice {
        render_notice(notice, right[1], buf, theme);
    }
    render_context_usage_tooltip(app.context_usage_hover, area, buf, &layout, theme);
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
    let sections = [(ControlSection::Files, "F"), (ControlSection::Git, "G")];
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
    for (offset, mode) in [RosterMode::Agents, RosterMode::Workspaces]
        .into_iter()
        .enumerate()
    {
        let y = area.y.saturating_add(2 + offset as u16);
        if y >= area.bottom() {
            break;
        }
        let selected = (app.roster_mode == mode
            || (mode == RosterMode::Agents && app.roster_mode == RosterMode::NativeSessions))
            && app.control_section == match mode {
                RosterMode::Agents => ControlSection::Agents,
                RosterMode::Workspaces => ControlSection::Workspaces,
                RosterMode::NativeSessions => unreachable!("native sessions are not visible"),
            }
            && !app.sidebar_collapsed;
        let row = Rect::new(area.x, y, area.width, 1);
        Paragraph::new(centered_label(mode.compact_id(), area.width as usize))
            .style(
                Style::default()
                    .fg(if selected { theme.active_tab_text } else { theme.muted })
                    .bg(if selected { theme.accent } else { theme.panel })
                    .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
            )
            .render(row, buf);
        layout.hits.push(HitRegion {
            rect: row,
            target: HitTarget::RosterMode(mode),
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
        let secondary = format!(
            "{} | {} | {}",
            node.node_id,
            relay_route_label(node.relay_route),
            host_path_display(&workspace.canonical_root),
        );
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
    layout: &mut LayoutRects,
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
    if app.agent_run_lens.is_some() && area.width > 0 && area.height > 0 {
        let all_label = "[all]";
        let all_width = (cell_width(all_label) as u16).min(area.width);
        let all = Rect::new(area.right().saturating_sub(all_width), area.y, all_width, 1);
        Paragraph::new(all_label)
            .style(Style::default().fg(theme.active_tab_text).bg(theme.accent).add_modifier(Modifier::BOLD))
            .render(all, buf);
        layout.hits.push(HitRegion { rect: all, target: HitTarget::AgentRunAll });
    }
    let scope_rows = u16::from(app.agent_run_lens.is_some() && area.height > 1);
    if scope_rows > 0 {
        let scope = match app.agent_run_git_scope_view() {
            AgentRunGitScopeView::Loading => " run scope: loading...".to_owned(),
            AgentRunGitScopeView::SharedWorkspace => " run scope: shared workspace".to_owned(),
            AgentRunGitScopeView::ExclusiveManagedWorktree { branch, base_commit } => {
                format!(" run scope: exclusive managed worktree | {branch} @ {base_commit}")
            }
            AgentRunGitScopeView::SharedManagedWorktree { branch, base_commit } => {
                format!(" run scope: shared managed worktree | {branch} @ {base_commit}")
            }
        };
        Paragraph::new(truncate_cells(&scope, area.width as usize))
            .style(Style::default().fg(theme.muted).bg(theme.panel))
            .render(Rect::new(area.x, area.y + 1, area.width, 1), buf);
    }
    Rect::new(
        area.x,
        area.y + 1 + scope_rows,
        area.width,
        area.height.saturating_sub(1 + scope_rows),
    )
}

fn render_workspace_files(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let list = render_inspector_header(app, area, buf, layout, theme);
    if area.width > 0 && area.height > 0 && app.selected_workspace().is_some() {
        let directory_label = "[+>]";
        let file_label = "[+.]";
        let directory_width = cell_width(directory_label) as u16;
        let file_width = cell_width(file_label) as u16;
        let lens_offset = if app.agent_run_lens.is_some() { 6 } else { 0 };
        if area.width >= directory_width.saturating_add(lens_offset) {
            let directory = Rect::new(
                area.right().saturating_sub(directory_width).saturating_sub(lens_offset),
                area.y,
                directory_width,
                1,
            );
            Paragraph::new(directory_label)
                .style(Style::default().fg(theme.teal).bg(theme.panel).add_modifier(Modifier::BOLD))
                .render(directory, buf);
            layout.hits.push(HitRegion {
                rect: directory,
                target: HitTarget::NewDirectory,
            });
            if directory.x >= area.x.saturating_add(file_width + 1) {
                let file = Rect::new(directory.x - file_width - 1, area.y, file_width, 1);
                Paragraph::new(file_label)
                    .style(Style::default().fg(theme.teal).bg(theme.panel).add_modifier(Modifier::BOLD))
                    .render(file, buf);
                layout.hits.push(HitRegion {
                    rect: file,
                    target: HitTarget::NewFile,
                });
            }
        }
    }
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
        let depth = entry.relative_path.depth().min(4);
        let name = repository_path_file_name_display(&entry.relative_path);
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
                truncate_cells(&name, name_width),
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

fn render_create_workspace_entry(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let Some(dialog) = &app.create_workspace_entry else {
        return;
    };
    let width = 76.min(area.width.saturating_sub(4));
    let height = 10.min(area.height.saturating_sub(2));
    let modal = positioned_modal(area, width, height, None);
    fill_rect(modal, theme.modal, buf);
    let title = match dialog.kind {
        WorkspaceEntryKind::File => " create file ",
        WorkspaceEntryKind::Directory => " create directory ",
    };
    Block::bordered()
        .title(title)
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.modal))
        .render(modal, buf);
    if modal.width < 3 || modal.height < 3 {
        return;
    }
    let inner = Rect::new(modal.x + 1, modal.y + 1, modal.width - 2, modal.height - 2);
    render_modal_line(
        format!("  workspace  {}/{}", dialog.node_id, dialog.workspace_id),
        inner,
        0,
        Style::default().fg(theme.muted).bg(theme.modal),
        buf,
    );
    render_modal_line(
        format!("> path from workspace root [ {} ]", dialog.path),
        inner,
        2,
        Style::default()
            .fg(theme.active_tab_text)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD),
        buf,
    );
    let target = normalize_workspace_entry_input(&dialog.path);
    render_modal_line(
        format!(
            "  target     {}/{}",
            dialog.workspace_id,
            if target.is_empty() { "new/hello.txt" } else { &target },
        ),
        inner,
        3,
        Style::default().fg(theme.dim).bg(theme.modal),
        buf,
    );
    let status = if dialog.pending {
        format!("{} creating without overwrite...", app.activity_spinner())
    } else if let Some(error) = &dialog.error {
        error.clone()
    } else {
        "Enter creates. No overwrite; parent must already exist.".to_owned()
    };
    render_modal_line(
        status,
        inner,
        4,
        Style::default()
            .fg(if dialog.error.is_some() { theme.red } else { theme.muted })
            .bg(theme.modal),
        buf,
    );
    let action_y = inner.bottom().saturating_sub(1);
    let submit = Rect::new(
        inner.right().saturating_sub(9),
        action_y,
        9.min(inner.width),
        1,
    );
    Paragraph::new("[Create]")
        .style(Style::default().fg(theme.active_tab_text).bg(theme.accent).add_modifier(Modifier::BOLD))
        .render(submit, buf);
    push_modal_hit(layout, submit, HitTarget::CreateWorkspaceEntrySubmit);
    let cancel = Rect::new(
        submit.x.saturating_sub(9).max(inner.x),
        action_y,
        8.min(inner.width),
        1,
    );
    Paragraph::new("[Cancel]")
        .style(Style::default().fg(theme.text).bg(theme.active))
        .render(cancel, buf);
    push_modal_hit(layout, cancel, HitTarget::CreateWorkspaceEntryCancel);
}

fn render_workspace_git(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let body = render_inspector_header(app, area, buf, layout, theme);
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
    let detail_count = git.status.len().saturating_add(git.recent_commits.len());
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
            let label = format!(" {state} {branch} · {}", host_path_display(&worktree.path));
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
            let line = if let Some(entry) = git.status.get(detail_index) {
                let code = format!("{}{}", entry.index_status, entry.worktree_status);
                let current_path = repository_path_display(&entry.path);
                let path = entry.previous_path.as_ref().map_or(current_path.clone(), |previous| {
                    format!("{} -> {current_path}", repository_path_display(previous))
                });
                Line::from_spans(vec![
                    Span::styled(format!(" {code} "), Style::default().fg(theme.yellow).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        truncate_cells(
                            &path,
                            area.width.saturating_sub(4) as usize,
                        ),
                        Style::default().fg(theme.text),
                    ),
                ])
            } else if let Some(commit) = git.recent_commits.get(detail_index.saturating_sub(git.status.len())) {
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
        let label = format!(" {} ", mode.compact_id());
        let width = (cell_width(&label) as u16).min(area.right().saturating_sub(x));
        if width == 0 {
            break;
        }
        let selected = app.roster_mode == mode
            || (mode == RosterMode::Agents && app.roster_mode == RosterMode::NativeSessions);
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
    let board_label = " [B Board] ";
    let board_width = (cell_width(board_label) as u16)
        .min(area.right().saturating_sub(x));
    if board_width > 0 {
        let board_x = area.right().saturating_sub(board_width);
        if board_x >= x {
            Paragraph::new(board_label)
                .style(
                    Style::default()
                        .fg(theme.teal)
                        .bg(theme.panel)
                        .add_modifier(Modifier::BOLD),
                )
                .render(Rect::new(board_x, area.y + 1, board_width, 1), buf);
            layout.hits.push(HitRegion {
                rect: Rect::new(board_x, area.y + 1, board_width, 1),
                target: HitTarget::AgentBoardOpen,
            });
        }
    }

    let content = Rect::new(area.x, area.y + 2, area.width, area.height.saturating_sub(2));
    match app.roster_mode {
        RosterMode::Agents | RosterMode::NativeSessions => {
            render_agents_surface(app, content, buf, layout, theme)
        }
        RosterMode::Workspaces => render_space_list(app, content, buf, layout, theme),
    }
}

fn render_agents_surface(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    if area.height == 0 {
        return;
    }
    if app.existing_session.is_none() || area.height < 5 {
        render_agent_list(app, area, buf, layout, theme);
        return;
    }

    // Native results remain a node-scoped subtree until the backend supplies stable
    // global identity annotations. Do not synthesize global rows or deduplicate here.
    render_native_session_list(app, area, buf, layout, theme);
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
    let roster_label = " managed/live agents ";
    let roster_width = (cell_width(roster_label) as u16).min(area.width);
    Paragraph::new(roster_label)
        .style(Style::default().fg(theme.muted).bg(theme.panel).add_modifier(Modifier::BOLD))
        .render(Rect::new(area.x, area.y, roster_width, 1), buf);
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
    let visible_rows = area.height.saturating_sub(1) as usize;
    let start = app
        .agents_scroll
        .min(app.agent_roster_max_scroll(visible_rows));
    let mut y = area.y + 1;
    for (index, key) in rows.iter().enumerate().skip(start) {
        let expanded = app.agent_progress_expanded(key);
        let card_height = if expanded { 5 } else { 2 };
        if y.saturating_add(2) > area.bottom() {
            break;
        }
        let rendered_height = card_height.min(area.bottom().saturating_sub(y));
        let selected = index == app.selected_agent;
        let background = if selected { theme.active } else { theme.panel };
        let compact_actions_only = area.width < 55;
        let more_label = if area.width < 30 { "[act]" } else { "[actions]" };
        let more_width = (cell_width(more_label) as u16).min(area.width);
        let run_label = if area.width < 40 {
            if app.agent_run_lens_key() == Some(key) { "[all]" } else { "[ws]" }
        } else if app.agent_run_lens_key() == Some(key) {
            "[global]"
        } else {
            "[workspace]"
        };
        let mut run_width = (cell_width(run_label) as u16).min(area.width);
        let progress_label = if area.width < 40 {
            if expanded { "[d-]" } else { "[d+]" }
        } else if expanded {
            "[details -]"
        } else {
            "[details +]"
        };
        let mut progress_width = (cell_width(progress_label) as u16).min(area.width);
        let order_label = "[move]";
        let mut order_width = if matches!(key, AgentRowKey::Managed { .. }) {
            cell_width(order_label) as u16
        } else {
            0
        };
        if compact_actions_only {
            run_width = 0;
            progress_width = 0;
            order_width = 0;
        }
        let reserved_controls = more_width
            .saturating_add(run_width)
            .saturating_add(progress_width)
            .saturating_add(order_width)
            .saturating_add(5);
        if reserved_controls > area.width {
            order_width = 0;
        }
        if more_width
            .saturating_add(run_width)
            .saturating_add(progress_width)
            .saturating_add(4)
            > area.width
        {
            progress_width = 0;
        }
        if more_width
            .saturating_add(run_width)
            .saturating_add(3)
            > area.width
        {
            run_width = 0;
        }
        let title_width = area
            .width
            .saturating_sub(3)
            .saturating_sub(more_width)
            .saturating_sub(run_width)
            .saturating_sub(progress_width)
            .saturating_sub(1)
            .saturating_sub(1)
            .saturating_sub(1)
            .saturating_sub(order_width)
            .saturating_sub(u16::from(order_width > 0));
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
                let mut secondary = format!(
                    "{state} | {} | {} | {}",
                    record.provider, record.workspace_id, record.node_id
                );
                if let Some(bundle) = record.bundle.as_ref() {
                    secondary.push_str(&format!(" | b:{}@{}", bundle.id, bundle.revision));
                }
                if let Some(context_id) = record.context_id.as_ref() {
                    secondary.push_str(&format!(" | c:{context_id}"));
                }
                if let Some(task_id) = record.task_binding.as_ref()
                    .and_then(|binding| binding.task_id.as_ref())
                {
                    secondary.push_str(&format!(" | {}", compact_task_id(task_id)));
                }
                let title = app.agent_local_alias(key)
                    .map(str::to_owned)
                    .unwrap_or_else(|| record.short_title().to_owned());
                if app.agent_local_alias(key).is_some() {
                    secondary = format!("{} | {secondary}", record.short_title());
                }
                (
                    title,
                    marker,
                    color,
                    secondary,
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
                truncate_cells(&title, title_width as usize),
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
            rect: Rect::new(area.x, y, area.width, rendered_height),
            target: HitTarget::Agent(index),
        });
        if order_width > 0 {
            let order = Rect::new(area.x, y, order_width, 1);
            Paragraph::new(order_label)
                .style(Style::default().fg(theme.muted).bg(background))
                .render(order, buf);
            layout.hits.push(HitRegion {
                rect: order,
                target: HitTarget::AgentOrderHandle(key.clone()),
            });
        }
        if app.agent_is_pinned(key) && area.width > order_width {
            let pin_x = area.x.saturating_add(order_width);
            Paragraph::new("P")
                .style(Style::default().fg(theme.yellow).bg(background))
                .render(Rect::new(pin_x, y, 1, 1), buf);
        }
        if more_width > 0 {
            let more_x = area.right().saturating_sub(more_width);
            Paragraph::new(more_label)
                .style(
                    Style::default()
                        .fg(if selected { theme.accent } else { theme.muted })
                        .bg(background)
                        .add_modifier(Modifier::BOLD),
                )
                .render(Rect::new(more_x, y, more_width, 1), buf);
            layout.hits.push(HitRegion {
                rect: Rect::new(more_x, y, more_width, 1),
                target: HitTarget::AgentMore(index),
            });
        }
        if run_width > 0 {
            let run_x = area
                .right()
                .saturating_sub(more_width)
                .saturating_sub(run_width)
                .saturating_sub(1);
            let run = Rect::new(run_x, y, run_width, 1);
            Paragraph::new(run_label)
                .style(
                    Style::default()
                        .fg(if app.agent_run_lens_key() == Some(key) { theme.active_tab_text } else { theme.teal })
                        .bg(if app.agent_run_lens_key() == Some(key) { theme.accent } else { background })
                        .add_modifier(Modifier::BOLD),
                )
                .render(run, buf);
            layout.hits.push(HitRegion {
                rect: run,
                target: HitTarget::AgentRun(key.clone()),
            });
        }
        if progress_width > 0 {
            let progress_x = area
                .right()
                .saturating_sub(more_width)
                .saturating_sub(run_width)
                .saturating_sub(progress_width)
                .saturating_sub(2);
            let progress = Rect::new(progress_x, y, progress_width, 1);
            Paragraph::new(progress_label)
                .style(
                    Style::default()
                        .fg(if expanded { theme.active_tab_text } else { theme.teal })
                        .bg(if expanded { theme.accent } else { background })
                        .add_modifier(Modifier::BOLD),
                )
                .render(progress, buf);
            layout.hits.push(HitRegion {
                rect: progress,
                target: HitTarget::AgentProgressToggle(key.clone()),
            });
        }
        if expanded {
            for (offset, line) in agent_progress_lines(app, key).into_iter().enumerate() {
                let progress_y = y.saturating_add(2 + offset as u16);
                if progress_y >= area.bottom() {
                    break;
                }
                Paragraph::new(format!("   {}", compact_middle_cells(&line, area.width.saturating_sub(3) as usize)))
                    .style(Style::default().fg(theme.dim).bg(background))
                    .render(Rect::new(area.x, progress_y, area.width, 1), buf);
            }
        }
        y = y.saturating_add(card_height);
    }
}

fn agent_progress_lines(app: &App, key: &AgentRowKey) -> [String; 3] {
    let (progress, unavailable) = match key {
        AgentRowKey::Managed { .. } => {
            let Some(record) = app.find_managed_session(key) else {
                return unavailable_agent_progress_lines("unavailable");
            };
            if record.state == gate4agent_node_protocol::ManagedSessionState::Dormant {
                (None, "unavailable until resume")
            } else {
                let progress = record.active_session.as_ref()
                    .and_then(|address| app.find_session(address))
                    .and_then(|session| session.progress.as_ref());
                (progress, if record.state == gate4agent_node_protocol::ManagedSessionState::Live {
                    "syncing"
                } else {
                    "unavailable"
                })
            }
        }
        AgentRowKey::Legacy(address) => {
            let Some(session) = app.find_session(address) else {
                return unavailable_agent_progress_lines("unavailable");
            };
            (session.progress.as_ref(), if session.running { "syncing" } else { "unavailable" })
        }
    };
    let Some(progress) = progress else {
        return unavailable_agent_progress_lines(unavailable);
    };
    let current = match progress.current {
        gate4agent_node_protocol::AgentProgressCurrentV1::Idle => "idle",
        gate4agent_node_protocol::AgentProgressCurrentV1::Working => "working",
        gate4agent_node_protocol::AgentProgressCurrentV1::WaitingForInput => "waiting for input",
        gate4agent_node_protocol::AgentProgressCurrentV1::Blocked => "blocked",
    };
    let freshness = if progress.stale {
        "stale".to_owned()
    } else if progress.gap_count > 0 {
        format!("incomplete after event gap {}", progress.gap_count)
    } else {
        "fresh".to_owned()
    };
    let partial = if progress.truncated { " | partial" } else { "" };
    let attention = progress.attention.as_ref().map(|attention| {
        let kind = match attention.kind {
            gate4agent_node_protocol::AgentProgressAttentionKindV1::Approval => "approval",
            gate4agent_node_protocol::AgentProgressAttentionKindV1::Question => "question",
        };
        attention.tool_label.as_ref()
            .map(|tool| format!(" | attention {kind}: {tool}"))
            .unwrap_or_else(|| format!(" | attention {kind}"))
    }).unwrap_or_default();
    let labels = if progress.active_tool_labels.is_empty() {
        String::new()
    } else {
        format!(" [{}]", progress.active_tool_labels.join(", "))
    };
    let usage = progress.usage.map(|usage| format!(
        " | usage in {} out {} cache {}+{} reason {}",
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
        usage.reasoning_tokens,
    )).unwrap_or_default();
    [
        format!("{current} | {freshness}{partial}"),
        format!("tools {}{labels}{attention}", progress.active_tool_count),
        format!("turns {}{usage} | subagents {}", progress.completed_turns, progress.subagent_count),
    ]
}

fn unavailable_agent_progress_lines(state: &str) -> [String; 3] {
    [
        state.to_owned(),
        "tools unavailable | attention unavailable".to_owned(),
        "turns unavailable | usage unavailable | subagents unavailable".to_owned(),
    ]
}

fn render_native_session_list(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let Some(dialog) = app.existing_session.as_ref() else {
        Paragraph::new(" Native sessions are not initialized")
            .style(Style::default().fg(theme.muted).bg(theme.panel))
            .render(area, buf);
        return;
    };
    if area.height < 5 {
        return;
    }
    let node_value = node_route_value(app, &dialog.node_id);
    render_native_route_selector(
        "Node",
        &node_value,
        dialog.field == ExistingSessionField::Node,
        ExistingSessionField::Node,
        area,
        0,
        theme,
        buf,
        layout,
    );
    let status = match &dialog.catalog {
        NativeSessionCatalogState::Loading => format!(
            "loading... {} route(s) pending, {} session(s)",
            dialog.pending_routes.len(),
            dialog.rows.len(),
        ),
        NativeSessionCatalogState::Empty => format!(
            "{} managed/live agent(s) | no provider history",
            app.agent_rows().len(),
        ),
        NativeSessionCatalogState::Unavailable(reason) => format!("unavailable: {reason}"),
        NativeSessionCatalogState::Error(message) => format!("failed: {message}"),
        NativeSessionCatalogState::Ready => format!(
            "{} managed/live agent(s) | {} provider history session(s) - read-only",
            app.agent_rows().len(),
            dialog.rows.len(),
        ),
    };
    render_modal_line(
        status,
        area,
        1,
        Style::default().fg(theme.muted).bg(theme.panel),
        buf,
    );
    let provider_totals = dialog.native_history_provider_totals();
    let provider_summary = if provider_totals.is_empty() {
        "providers -".to_owned()
    } else {
        format!(
            "providers {}",
            provider_totals
                .into_iter()
                .map(|(provider, total)| format!("{provider}:{total}"))
                .collect::<Vec<_>>()
                .join(" "),
        )
    };
    render_modal_line(
        provider_summary,
        area,
        2,
        Style::default().fg(theme.dim).bg(theme.panel),
        buf,
    );
    let items = app.native_session_tree_items();
    let capacity = area.height.saturating_sub(5) as usize;
    let item_height = |item: &NativeSessionTreeItem| match item {
        NativeSessionTreeItem::Agent { agent_index, .. } => app.agent_rows()
            .get(*agent_index)
            .map(|key| {
                if area.width >= 55 && app.agent_progress_expanded(key) {
                    4
                } else {
                    1
                }
            })
            .unwrap_or(1),
        _ => 1,
    };
    let mut start = dialog.scroll.min(items.len().saturating_sub(1));
    if dialog.tree_cursor < start {
        start = dialog.tree_cursor;
    } else {
        while start < dialog.tree_cursor
            && items[start..=dialog.tree_cursor]
                .iter()
                .map(&item_height)
                .sum::<usize>()
                > capacity
        {
            start += 1;
        }
    }
    let mut y = area.y + 3;
    let rows_bottom = area.y.saturating_add(3).saturating_add(capacity as u16);
    for (tree_index, item) in items.iter().enumerate().skip(start) {
        let height = item_height(item) as u16;
        if y >= rows_bottom {
            break;
        }
        let selected = tree_index == dialog.tree_cursor;
        let row_area = Rect::new(area.x, y, area.width, 1);
        let (label, target, foreground) = match item {
            NativeSessionTreeItem::Agent { agent_index, nested } => {
                let Some(key) = app.agent_rows().get(*agent_index).cloned() else {
                    continue;
                };
                let indent = if *nested { "       " } else { " " };
                match &key {
                    AgentRowKey::Managed { .. } => {
                        let Some(record) = app.find_managed_session(&key) else {
                            continue;
                        };
                        let active = record.active_session.as_ref()
                            .and_then(|address| app.find_session(address));
                        let (marker, color, state) = active
                            .map(|session| session_state(session, theme))
                            .unwrap_or_else(|| managed_session_state(record.state, theme));
                        let mut detail = format!(
                            "{state} | {} | {}",
                            record.workspace_id,
                            record.node_id,
                        );
                        if let Some(bundle) = record.bundle.as_ref() {
                            detail.push_str(&format!(" | b:{}@{}", bundle.id, bundle.revision));
                        }
                        if let Some(context_id) = record.context_id.as_ref() {
                            detail.push_str(&format!(" | c:{context_id}"));
                        }
                        let primary = app.agent_local_alias(&key)
                            .map(str::to_owned)
                            .unwrap_or_else(|| record.short_title().to_owned());
                        let authoritative = if app.agent_local_alias(&key).is_some() {
                            format!("{} | ", record.short_title())
                        } else {
                            String::new()
                        };
                        (
                            format!(
                                "{}{}{}{} {} | {}{}",
                                if selected { ">" } else { " " },
                                indent,
                                if app.agent_is_pinned(&key) { "P" } else { "" },
                                marker,
                                primary,
                                authoritative,
                                detail,
                            ),
                            HitTarget::Agent(*agent_index),
                            color,
                        )
                    }
                    AgentRowKey::Legacy(address) => {
                        let Some(session) = app.find_session(address) else {
                            continue;
                        };
                        let (marker, color, state) = session_state(session, theme);
                        (
                            format!(
                                "{}{}{} {} | {state} | {} | {}",
                                if selected { ">" } else { " " },
                                indent,
                                marker,
                                session.short_title(),
                                address.workspace_id,
                                address.node_id,
                            ),
                            HitTarget::Agent(*agent_index),
                            color,
                        )
                    }
                }
            }
            NativeSessionTreeItem::Workspace {
                key,
                label,
                session_count,
                collapsed,
                external,
            } => (
                format!(
                    "{}{} {} {}{}",
                    if selected { ">" } else { " " },
                    if *external && !matches!(key, NativeSessionGroupKey::OtherProjects) {
                        "     "
                    } else {
                        ""
                    },
                    if *collapsed { "+" } else { "-" },
                    label,
                    if *session_count > 0 {
                        format!(" ({session_count})")
                    } else {
                        String::new()
                    },
                ),
                HitTarget::NativeWorkspaceGroup(key.clone()),
                theme.teal,
            ),
            NativeSessionTreeItem::Provider {
                group,
                provider,
                session_count,
                collapsed,
            } => (
                format!(
                    "{}{}{} [{}]{}",
                    if selected { ">" } else { " " },
                    "   ",
                    if *collapsed { "+" } else { "-" },
                    provider,
                    if *session_count > 0 {
                        format!(" ({session_count})")
                    } else {
                        String::new()
                    },
                ),
                HitTarget::NativeProviderGroup(group.clone(), provider.clone()),
                theme.yellow,
            ),
            NativeSessionTreeItem::Session { row_index } => {
                let row = &dialog.rows[*row_index];
                let title = row.title.as_deref()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or("Existing session");
                (
                    format!("{}       {}", if selected { ">" } else { " " }, title),
                    HitTarget::ExistingSessionRow(*row_index),
                    theme.text,
                )
            }
            NativeSessionTreeItem::LoadMore {
                route,
                window,
                remaining_count,
                loading,
            } => {
                let kind = match window {
                    gate4agent_types::NativeSessionCatalogWindow::Recent => "more recent",
                    gate4agent_types::NativeSessionCatalogWindow::Older => "older",
                };
                (
                    format!(
                        "{}       {}",
                        if selected { ">" } else { " " },
                        if *loading {
                            format!("Loading {kind} sessions...")
                        } else {
                            format!(
                                "Show {remaining_count} {kind} [{}] sessions...",
                                route.provider,
                            )
                        },
                    ),
                    HitTarget::NativeSessionsLoadMore(
                        route.clone(),
                        *window,
                    ),
                    theme.teal,
                )
            }
        };
        let agent_run_key = match item {
            NativeSessionTreeItem::Agent { agent_index, .. } => {
                app.agent_rows().get(*agent_index).cloned()
            }
            _ => None,
        };
        let agent_index = match item {
            NativeSessionTreeItem::Agent { agent_index, .. } => Some(*agent_index),
            _ => None,
        };
        let compact_agent_actions = agent_run_key.is_some() && area.width < 55;
        let agent_expanded = agent_run_key.as_ref()
            .is_some_and(|key| app.agent_progress_expanded(key));
        let progress_label = if agent_expanded { "[details -]" } else { "[details +]" };
        let progress_width = if agent_run_key.is_some() && !compact_agent_actions {
            (cell_width(progress_label) as u16).min(area.width)
        } else {
            0
        };
        let agent_action_label = if area.width < 30 { "[act]" } else { "[actions]" };
        let agent_action_width = if compact_agent_actions {
            (cell_width(agent_action_label) as u16).min(area.width)
        } else {
            0
        };
        let native_action_label = "[menu]";
        let native_action_width = if matches!(item, NativeSessionTreeItem::Session { .. }) {
            (cell_width(native_action_label) as u16).min(area.width)
        } else {
            0
        };
        let label_width = if compact_agent_actions {
            area.width
                .saturating_sub(agent_action_width)
                .saturating_sub(u16::from(agent_action_width > 0))
        } else if agent_run_key.is_some() {
            area.width.saturating_sub(6).saturating_sub(progress_width).saturating_sub(1)
        } else {
            area.width
                .saturating_sub(native_action_width)
                .saturating_sub(u16::from(native_action_width > 0))
        };
        Paragraph::new(truncate_cells(&label, label_width as usize))
            .style(
                Style::default()
                    .fg(if selected { theme.active_tab_text } else { foreground })
                    .bg(if selected { theme.accent } else { theme.panel })
                    .add_modifier(if matches!(item, NativeSessionTreeItem::Workspace { .. } | NativeSessionTreeItem::Provider { .. }) {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )
            .render(row_area, buf);
        layout.hits.push(HitRegion {
            rect: Rect::new(area.x, y, area.width, height.min(rows_bottom.saturating_sub(y))),
            target,
        });
        if let NativeSessionTreeItem::Session { row_index } = item {
            let actions = Rect::new(
                row_area.right().saturating_sub(native_action_width),
                row_area.y,
                native_action_width,
                1,
            );
            Paragraph::new(native_action_label)
                .style(
                    Style::default()
                        .fg(theme.teal)
                        .bg(if selected { theme.accent } else { theme.panel })
                        .add_modifier(Modifier::BOLD),
                )
                .render(actions, buf);
            layout.hits.push(HitRegion {
                rect: actions,
                target: HitTarget::NativeSessionMore(*row_index),
            });
        }
        if let Some(agent_index) = agent_index.filter(|_| compact_agent_actions) {
            let actions = Rect::new(
                row_area.right().saturating_sub(agent_action_width),
                row_area.y,
                agent_action_width,
                1,
            );
            Paragraph::new(agent_action_label)
                .style(
                    Style::default()
                        .fg(theme.teal)
                        .bg(if selected { theme.accent } else { theme.panel })
                        .add_modifier(Modifier::BOLD),
                )
                .render(actions, buf);
            layout.hits.push(HitRegion {
                rect: actions,
                target: HitTarget::AgentMore(agent_index),
            });
        }
        if let Some(key) = agent_run_key {
            if compact_agent_actions {
                y = y.saturating_add(height);
                continue;
            }
            if matches!(key, AgentRowKey::Managed { .. }) {
                let handle = Rect::new(
                    row_area.x,
                    row_area.y,
                    (cell_width("[move]") as u16).min(row_area.width),
                    1,
                );
                Paragraph::new("[move]")
                    .style(Style::default().fg(theme.muted).bg(if selected { theme.accent } else { theme.panel }))
                    .render(handle, buf);
                layout.hits.push(HitRegion {
                    rect: handle,
                    target: HitTarget::AgentOrderHandle(key.clone()),
                });
            }
            let run_label = if app.agent_run_lens_key() == Some(&key) {
                "[global]"
            } else {
                "[workspace]"
            };
            let run_width = (cell_width(run_label) as u16).min(area.width);
            let run = Rect::new(
                row_area.right().saturating_sub(run_width),
                row_area.y,
                run_width,
                1,
            );
            Paragraph::new(run_label)
                .style(
                    Style::default()
                        .fg(if app.agent_run_lens_key() == Some(&key) { theme.active_tab_text } else { theme.teal })
                        .bg(if app.agent_run_lens_key() == Some(&key) { theme.accent } else if selected { theme.accent } else { theme.panel })
                        .add_modifier(Modifier::BOLD),
                )
                .render(run, buf);
            layout.hits.push(HitRegion {
                rect: run,
                target: HitTarget::AgentRun(key.clone()),
            });
            let progress = Rect::new(
                run.x.saturating_sub(progress_width).saturating_sub(1),
                row_area.y,
                progress_width,
                1,
            );
            Paragraph::new(progress_label)
                .style(
                    Style::default()
                        .fg(if agent_expanded { theme.active_tab_text } else { theme.teal })
                        .bg(if agent_expanded { theme.accent } else if selected { theme.accent } else { theme.panel })
                        .add_modifier(Modifier::BOLD),
                )
                .render(progress, buf);
            layout.hits.push(HitRegion {
                rect: progress,
                target: HitTarget::AgentProgressToggle(key.clone()),
            });
            if agent_expanded {
                for (offset, line) in agent_progress_lines(app, &key).into_iter().enumerate() {
                    let progress_y = y.saturating_add(1 + offset as u16);
                    if progress_y >= rows_bottom {
                        break;
                    }
                    Paragraph::new(format!("       {}", compact_middle_cells(&line, area.width.saturating_sub(7) as usize)))
                        .style(Style::default().fg(theme.dim).bg(if selected { theme.active } else { theme.panel }))
                        .render(Rect::new(area.x, progress_y, area.width, 1), buf);
                }
            }
        }
        y = y.saturating_add(height);
    }
    let open = Rect::new(
        area.x,
        area.bottom().saturating_sub(1),
        area.width.min(19),
        1,
    );
    Paragraph::new("[Open transcript]")
        .style(Style::default().fg(theme.teal).bg(theme.panel).add_modifier(Modifier::BOLD))
        .render(open, buf);
    layout.hits.push(HitRegion {
        rect: open,
        target: HitTarget::NativeSessionsOpen,
    });
    let add_label = "+ agent";
    let add_width = (cell_width(add_label) as u16).min(area.width);
    let add = Rect::new(
        area.right().saturating_sub(add_width),
        area.bottom().saturating_sub(1),
        add_width,
        1,
    );
    Paragraph::new(add_label)
        .style(Style::default().fg(theme.teal).bg(theme.panel).add_modifier(Modifier::BOLD))
        .render(add, buf);
    layout.hits.push(HitRegion {
        rect: add,
        target: HitTarget::AddAgent,
    });
}

fn render_native_route_selector(
    label: &str,
    value: &str,
    active: bool,
    field: ExistingSessionField,
    area: Rect,
    row: u16,
    theme: Theme,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
) {
    let text = format!("{} {:<3} < {} >", if active { ">" } else { " " }, label, value);
    let rect = modal_row(area, row);
    Paragraph::new(truncate_cells(&text, area.width as usize))
        .style(Style::default()
            .fg(if active { theme.active_tab_text } else { theme.text })
            .bg(if active { theme.accent } else { theme.panel }))
        .render(rect, buf);
    push_modal_hit(layout, rect, HitTarget::ExistingSessionField(field));
    if area.width > 2 {
        Paragraph::new("<>")
            .style(Style::default()
                .fg(if active { theme.active_tab_text } else { theme.teal })
                .bg(if active { theme.accent } else { theme.panel }))
            .render(Rect::new(area.right().saturating_sub(2), area.y + row, 2, 1), buf);
        push_modal_hit(
            layout,
            Rect::new(area.right().saturating_sub(2), area.y + row, 1, 1),
            HitTarget::ExistingSessionFieldPrevious(field),
        );
        push_modal_hit(
            layout,
            Rect::new(area.right().saturating_sub(1), area.y + row, 1, 1),
            HitTarget::ExistingSessionFieldNext(field),
        );
    }
}

fn render_agent_menu(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let Some(menu) = app.agent_menu.as_ref() else {
        return;
    };
    let items = app.agent_menu_items();
    if items.is_empty() || area.width < 4 || area.height < 3 {
        return;
    }
    let width = 56.min(area.width);
    let height = (items.len() as u16 + 2).min(area.height);
    let x = menu
        .anchor_column
        .min(area.right().saturating_sub(width))
        .max(area.x);
    let y = menu
        .anchor_row
        .min(area.bottom().saturating_sub(height))
        .max(area.y);
    let menu_area = Rect::new(x, y, width, height);
    fill_rect(menu_area, theme.modal, buf);
    Block::bordered()
        .title(" agent actions ")
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.modal))
        .render(menu_area, buf);
    let inner = Rect::new(
        menu_area.x.saturating_add(1),
        menu_area.y.saturating_add(1),
        menu_area.width.saturating_sub(2),
        menu_area.height.saturating_sub(2),
    );
    let capacity = inner.height as usize;
    let start = if capacity == 0 {
        0
    } else if menu.selected >= capacity {
        menu.selected.saturating_add(1).saturating_sub(capacity)
    } else {
        0
    };
    for (row_index, (index, item)) in items
        .iter()
        .enumerate()
        .skip(start)
        .take(capacity)
        .enumerate()
    {
        let selected = index == menu.selected;
        let label = if item.enabled {
            item.label.clone()
        } else {
            format!(
                "{} - {}",
                item.label,
                item.disabled_reason.as_deref().unwrap_or("unsupported")
            )
        };
        let row = Rect::new(inner.x, inner.y + row_index as u16, inner.width, 1);
        Paragraph::new(format!(
            "{} {}",
            if selected { ">" } else { " " },
            truncate_cells(&label, inner.width.saturating_sub(2) as usize)
        ))
        .style(
            Style::default()
                .fg(if item.enabled { theme.text } else { theme.muted })
                .bg(if selected { theme.active } else { theme.modal })
                .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
        )
        .render(row, buf);
        layout.hits.push(HitRegion {
            rect: row,
            target: HitTarget::AgentMenuAction(item.action),
        });
    }
}

fn render_native_session_menu(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let Some(menu) = app.native_session_menu.as_ref() else {
        return;
    };
    let items = app.native_session_menu_items();
    if items.is_empty() || area.width < 4 || area.height < 3 {
        return;
    }
    let width = 64.min(area.width);
    let height = (items.len() as u16 + 3).min(area.height);
    let x = menu
        .anchor_column
        .min(area.right().saturating_sub(width))
        .max(area.x);
    let y = menu
        .anchor_row
        .min(area.bottom().saturating_sub(height))
        .max(area.y);
    let menu_area = Rect::new(x, y, width, height);
    fill_rect(menu_area, theme.modal, buf);
    Block::bordered()
        .title(" provider history actions ")
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.modal))
        .render(menu_area, buf);
    let inner = Rect::new(
        menu_area.x.saturating_add(1),
        menu_area.y.saturating_add(1),
        menu_area.width.saturating_sub(2),
        menu_area.height.saturating_sub(2),
    );
    for (index, item) in items.iter().take(inner.height as usize).enumerate() {
        let selected = index == menu.selected;
        let label = if item.enabled {
            item.label.clone()
        } else {
            format!(
                "{} - {}",
                item.label,
                item.disabled_reason.as_deref().unwrap_or("unsupported"),
            )
        };
        let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
        Paragraph::new(format!(
            "{} {}",
            if selected { ">" } else { " " },
            truncate_cells(&label, inner.width.saturating_sub(2) as usize),
        ))
        .style(
            Style::default()
                .fg(if item.enabled { theme.text } else { theme.muted })
                .bg(if selected { theme.active } else { theme.modal })
                .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
        )
        .render(row, buf);
        layout.hits.push(HitRegion {
            rect: row,
            target: HitTarget::NativeSessionMenuAction(item.action),
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
    let settings_label = " [S] ";
    let settings_width = cell_width(settings_label).min(area.width as usize) as u16;
    let tabs_right = area.right().saturating_sub(settings_width);
    Paragraph::new("")
        .style(Style::default().bg(theme.active))
        .render(Rect::new(area.x, area.y, tabs_right.saturating_sub(area.x), 1), buf);
    let layout_label = " [#] ";
    let preset_width = if app.layout_menu_open {
        LayoutPreset::ALL
            .iter()
            .map(|preset| cell_width(&format!(" {} ", preset.id())) as u16)
            .fold(0_u16, |total, width| total.saturating_add(width))
    } else {
        0
    };
    let controls_width = 3_u16
        .saturating_add(cell_width(layout_label) as u16)
        .saturating_add(preset_width);
    let controls_right = tabs_right.min(area.x.saturating_add(controls_width));
    let mut x = area.x;
    if x < controls_right {
        let width = 3.min(controls_right - x);
        Paragraph::new(" + ")
            .style(Style::default().fg(theme.muted).bg(theme.active))
            .render(Rect::new(x, area.y, width, 1), buf);
        layout.hits.push(HitRegion {
            rect: Rect::new(x, area.y, width, 1),
            target: HitTarget::AddTab,
        });
        x = x.saturating_add(width);
    }
    let layout_width = (cell_width(layout_label) as u16).min(controls_right.saturating_sub(x));
    if layout_width > 0 {
        Paragraph::new(truncate_cells(layout_label, layout_width as usize))
            .style(
                Style::default()
                    .fg(if app.layout_menu_open { theme.active_tab_text } else { theme.muted })
                    .bg(if app.layout_menu_open { theme.accent } else { theme.active })
                    .add_modifier(Modifier::BOLD),
            )
            .render(Rect::new(x, area.y, layout_width, 1), buf);
        layout.hits.push(HitRegion {
            rect: Rect::new(x, area.y, layout_width, 1),
            target: HitTarget::LayoutMenuToggle,
        });
        x = x.saturating_add(layout_width);
    }
    if app.layout_menu_open {
        for preset in LayoutPreset::ALL {
            if x >= controls_right {
                break;
            }
            let label = format!(" {} ", preset.id());
            let width = (cell_width(&label) as u16).min(controls_right - x);
            let selected = app.surface.preset == Some(preset);
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
                target: HitTarget::LayoutPreset(preset),
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
    layout.hits.push(HitRegion { rect: area, target: HitTarget::Viewport });
    let mut path = Vec::new();
    render_surface_node(app, &app.surface.root, area, &mut path, buf, layout, theme);
}

fn render_surface_node(
    app: &App,
    node: &PaneNode,
    area: Rect,
    path: &mut Vec<PaneBranch>,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    match node {
        PaneNode::Leaf(pane_id) => render_surface_pane(app, *pane_id, area, buf, layout, theme),
        PaneNode::Split {
            axis,
            ratio_bps,
            first,
            second,
        } => {
            let (first_area, divider, second_area) =
                split_surface_rect(area, *axis, *ratio_bps);
            draw_surface_divider(divider, *axis, buf, theme);
            path.push(PaneBranch::First);
            render_surface_node(app, first, first_area, path, buf, layout, theme);
            path.pop();
            path.push(PaneBranch::Second);
            render_surface_node(app, second, second_area, path, buf, layout, theme);
            path.pop();
            if divider.width > 0 && divider.height > 0 {
                layout.hits.push(HitRegion {
                    rect: divider,
                    target: HitTarget::SurfaceDivider {
                        path: PaneSplitPath(path.clone()),
                        axis: *axis,
                        area,
                    },
                });
            }
        }
    }
}

fn render_surface_pane(
    app: &App,
    pane_id: PaneId,
    frame: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let harness_board = app.agent_board_mode == crate::app::AgentBoardMode::HarnessKanban
        && app.surface.panes.get(&pane_id)
            .and_then(|pane| pane.active_tab()) == Some(&SurfaceTab::AgentBoard);
    let header = Rect::new(frame.x, frame.y, frame.width, frame.height.min(1));
    let actions = Rect::new(
        frame.x,
        frame.y.saturating_add(header.height),
        frame.width,
        frame.height.saturating_sub(header.height).min(if harness_board { 0 } else { 1 }),
    );
    let viewport = Rect::new(
        frame.x,
        frame.y.saturating_add(header.height).saturating_add(actions.height),
        frame.width,
        frame.height.saturating_sub(header.height).saturating_sub(actions.height),
    );
    layout.surface_panes.push(SurfacePaneLayout {
        pane_id,
        frame,
        header,
        viewport,
    });
    layout.hits.push(HitRegion {
        rect: viewport,
        target: HitTarget::SurfacePaneBody(pane_id),
    });
    layout.hits.push(HitRegion {
        rect: header,
        target: HitTarget::SurfacePaneHeader(pane_id),
    });
    let Some(pane) = app.surface.panes.get(&pane_id) else {
        return;
    };
    let focused = app.surface.focused == pane_id;
    render_surface_pane_tabs(app, pane_id, pane, focused, header, buf, layout, theme);
    render_surface_toolbar(
        app,
        pane_id,
        pane.active_tab(),
        actions,
        buf,
        layout,
        theme,
    );
    match pane.active_tab() {
        Some(SurfaceTab::AgentBoard) => {
            render_agent_board(app, viewport, buf, layout, theme);
        }
        Some(SurfaceTab::SessionMonitor(key)) => {
            if let Some(monitor) = app.session_monitor(key) {
                render_session_monitor(app, monitor, viewport, buf, layout, theme);
            }
        }
        Some(SurfaceTab::Pty(address)) => {
            if let Some(session) = app.find_session(address) {
                render_terminal(
                    session,
                    viewport,
                    app.color_mode,
                    app.terminal_scroll_offset(&session.address),
                    app.terminal_selection.as_ref(),
                    theme,
                    buf,
                );
            }
        }
        Some(SurfaceTab::Preview(key)) => {
            if let Some(preview) = app.preview_tabs.get(key) {
                render_preview_tab(
                    preview,
                    pane_id,
                    viewport,
                    buf,
                    layout,
                    theme,
                    app.activity_spinner(),
                );
            }
        }
        Some(SurfaceTab::File(key)) => {
            if let Some(file) = app.file_tabs.get(key) {
                if let Some(history) = file.inline_history.as_ref() {
                    render_workspace_file_history(
                        history,
                        pane_id,
                        viewport,
                        buf,
                        layout,
                        theme,
                        app.activity_spinner(),
                    );
                } else if file.state == WorkspaceFileState::Ready {
                    let content = Rect::new(
                        viewport.x,
                        viewport.y,
                        viewport.width.saturating_sub(if viewport.width > 2 { 1 } else { 0 }),
                        viewport.height,
                    );
                    render_workspace_file_tab_rich(
                        file,
                        key.path.as_utf8().unwrap_or_default(),
                        content,
                        buf,
                        theme,
                    );
                    render_file_scrollbar(file, pane_id, viewport, buf, layout, theme);
                } else {
                    render_workspace_file_tab(
                        file,
                        key.path.as_utf8().unwrap_or_default(),
                        viewport,
                        buf,
                        theme,
                    );
                }
            }
        }
        Some(SurfaceTab::Git(key)) => {
            if let Some(git) = app.git_tabs.get(key) {
                render_workspace_git_tab(
                    git,
                    pane_id,
                    viewport,
                    buf,
                    layout,
                    theme,
                    app.activity_spinner(),
                );
            }
        }
        None => {}
    }
}

fn render_session_monitor_toolbar(
    app: &App,
    pane_id: PaneId,
    key: &SessionMonitorKey,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    fill_rect(area, theme.active, buf);
    let selected = app.session_monitor(key)
        .map(|monitor| monitor.section)
        .unwrap_or_default();
    let selected_index = SessionMonitorSection::ALL
        .iter()
        .position(|section| *section == selected)
        .unwrap_or(0);
    let segment_width = |section: SessionMonitorSection| {
        (cell_width(section.label()) + 2).min(u16::MAX as usize) as u16
    };
    let total_width = SessionMonitorSection::ALL
        .iter()
        .map(|section| segment_width(*section))
        .fold(0_u16, u16::saturating_add);
    let start = if total_width <= area.width { 0 } else { selected_index };
    let mut x = area.x;
    for section in SessionMonitorSection::ALL.iter().copied().skip(start) {
        if x >= area.right() {
            break;
        }
        let label = format!(" {} ", section.label());
        let width = segment_width(section).min(area.right().saturating_sub(x));
        if width == 0 {
            break;
        }
        let rect = Rect::new(x, area.y, width, 1);
        Paragraph::new(truncate_cells(&label, width as usize))
            .style(
                Style::default()
                    .fg(if section == selected { theme.active_tab_text } else { theme.muted })
                    .bg(if section == selected { theme.accent } else { theme.active })
                    .add_modifier(if section == selected { Modifier::BOLD } else { Modifier::empty() }),
            )
            .render(rect, buf);
        layout.hits.push(HitRegion {
            rect,
            target: HitTarget::SessionMonitorSection(pane_id, section),
        });
        x = x.saturating_add(width);
    }
}

fn render_session_monitor(
    app: &App,
    monitor: &SessionMonitorView,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    fill_rect(area, theme.surface, buf);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width.min(104);
    let content = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y,
        width,
        area.height,
    );
    fill_rect(content, theme.panel, buf);
    let mut lines = session_monitor_lines(app, monitor);
    let visible = content.height as usize;
    let maximum = lines.len().saturating_sub(visible);
    let scroll = monitor.section_scroll().min(maximum);
    lines = lines.into_iter().skip(scroll).take(visible).collect();
    for (index, line) in lines.iter().enumerate() {
        let y = content.y.saturating_add(index as u16);
        if y >= content.bottom() {
            break;
        }
        let source_index = scroll.saturating_add(index);
        if monitor.section == SessionMonitorSection::Usage && source_index == 3 {
            if let Some(projection) = app.session_monitor_projection(&monitor.key) {
                if let Ok((snapshot, context_window)) = available_context_occupancy(projection) {
                    render_context_usage_bar(
                        snapshot,
                        context_window,
                        Rect::new(content.x, y, content.width.min(72), 1),
                        buf,
                        layout,
                        theme,
                    );
                    continue;
                }
            }
        }
        Paragraph::new(truncate_cells(line, content.width as usize))
            .style(Style::default().fg(if index == 0 { theme.text } else { theme.dim }).bg(theme.panel))
            .render(Rect::new(content.x, y, content.width, 1), buf);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ContextUsageBarSegment {
    hit: ContextUsageSegmentHit,
    width: u16,
    color: Color,
}

fn available_context_occupancy(
    projection: &SessionProjection,
) -> Result<(ContextOccupancySnapshot, u64), &'static str> {
    let snapshot = projection
        .usage
        .context_occupancy
        .ok_or("no authoritative usage snapshot")?;
    let context_window = exact_context_window(snapshot)?;
    if projection.availability != ProjectionAvailability::Current
        || projection.freshness != ProjectionFreshness::Live
    {
        return Err("projection is not current and live");
    }
    if projection.transport_incomplete
        || projection.incomplete_evidence.contains(&snapshot.evidence)
    {
        return Err("usage source is incomplete after a gap");
    }
    Ok((snapshot, context_window))
}

fn exact_context_window(snapshot: ContextOccupancySnapshot) -> Result<u64, &'static str> {
    if snapshot.provenance != ContextOccupancyProvenance::ExactCurrentWindow {
        return Err("usage accounting is not an exact current-window signal");
    }
    if snapshot.evidence
        != gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider
    {
        return Err("exact current-window source is not structured provider evidence");
    }
    let context_window = snapshot
        .context_window
        .ok_or("context window was not reported by this source")?;
    if context_window == 0 {
        return Err("reported context window is zero");
    }
    Ok(context_window)
}

fn context_usage_bar_segments(
    snapshot: ContextOccupancySnapshot,
    context_window: u64,
    width: u16,
    theme: Theme,
) -> Vec<ContextUsageBarSegment> {
    let occupied = snapshot.occupied_tokens();
    let remaining = context_window.saturating_sub(occupied);
    let values = [
        (ContextUsageSegment::UncachedInput, snapshot.uncached_input_tokens, theme.accent),
        (ContextUsageSegment::CacheRead, snapshot.cache_read_tokens, theme.teal),
        (ContextUsageSegment::CacheWrite, snapshot.cache_write_tokens, theme.yellow),
        (ContextUsageSegment::ProviderOutput, snapshot.output_tokens, theme.green),
        (ContextUsageSegment::Unattributed, snapshot.unattributed_tokens, theme.red),
        (ContextUsageSegment::Remaining, remaining, theme.border),
    ];
    let mut unassigned_width = width;
    let mut segments = values
        .into_iter()
        .map(|(segment, tokens, color)| {
            let proportional = ((tokens as u128 * width as u128) / context_window as u128)
                .min(width as u128) as u16;
            let segment_width = proportional.min(unassigned_width);
            unassigned_width = unassigned_width.saturating_sub(segment_width);
            ContextUsageBarSegment {
                hit: ContextUsageSegmentHit {
                    segment,
                    tokens,
                    context_window,
                    evidence: snapshot.evidence,
                },
                width: segment_width,
                color,
            }
        })
        .collect::<Vec<_>>();
    let assigned = segments
        .iter()
        .map(|segment| segment.width)
        .fold(0_u16, u16::saturating_add)
        .min(width);
    let leftover = width.saturating_sub(assigned);
    if leftover != 0 {
        let recipient = if remaining != 0 {
            segments.len().saturating_sub(1)
        } else {
            segments
                .iter()
                .rposition(|segment| segment.hit.tokens != 0)
                .unwrap_or(0)
        };
        segments[recipient].width = segments[recipient].width.saturating_add(leftover).min(width);
    }
    segments
}

fn render_context_usage_bar(
    snapshot: ContextOccupancySnapshot,
    context_window: u64,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let mut x = area.x;
    for segment in context_usage_bar_segments(snapshot, context_window, area.width, theme) {
        let width = segment.width.min(area.right().saturating_sub(x));
        if width == 0 {
            continue;
        }
        let rect = Rect::new(x, area.y, width, 1);
        fill_rect(rect, segment.color, buf);
        layout.hits.push(HitRegion {
            rect,
            target: HitTarget::ContextUsageSegment(segment.hit),
        });
        x = x.saturating_add(width);
        if x >= area.right() {
            break;
        }
    }
}

fn render_context_usage_tooltip(
    hover: Option<ContextUsageHover>,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &LayoutRects,
    theme: Theme,
) {
    let Some(hover) = hover else {
        return;
    };
    let still_hovered = layout.hits.iter().rev().any(|region| {
        region.rect.contains(hover.column, hover.row)
            && matches!(&region.target, HitTarget::ContextUsageSegment(hit) if *hit == hover.hit)
    });
    if !still_hovered || area.width == 0 || area.height < 3 {
        return;
    }
    let basis_points = (hover.hit.tokens as u128 * 10_000_u128)
        / hover.hit.context_window as u128;
    let percent_whole = basis_points / 100;
    let percent_fraction = basis_points % 100;
    let detail = format!(
        "{}: {} tokens | share {}/{} ({percent_whole}.{percent_fraction:02}%) | window {} tokens",
        hover.hit.segment.label(),
        hover.hit.tokens,
        hover.hit.tokens,
        hover.hit.context_window,
        hover.hit.context_window,
    );
    let source = format!(
        "Current / Live | source {}",
        observation_evidence_label(hover.hit.evidence),
    );
    let formula = "Formula: used = uncached input + cache read + cache write + provider output + unattributed (reasoning excluded)";
    let desired_width = cell_width(&detail)
        .max(cell_width(&source))
        .max(cell_width(formula))
        .min(120);
    let width = (desired_width as u16).min(area.width);
    let x = hover
        .column
        .min(area.right().saturating_sub(width))
        .max(area.x);
    let preferred_y = hover.row.saturating_add(1);
    let y = if preferred_y.saturating_add(3) <= area.bottom() {
        preferred_y
    } else {
        hover.row.saturating_sub(3).max(area.y)
    };
    let rect = Rect::new(x, y, width, 3);
    fill_rect(rect, theme.modal, buf);
    Paragraph::new(format!(
        "{}\n{}\n{}",
        truncate_cells(&detail, width as usize),
        truncate_cells(&source, width as usize),
        truncate_cells(formula, width as usize),
    ))
    .style(Style::default().fg(theme.text).bg(theme.modal))
    .render(rect, buf);
}

fn session_monitor_lines(app: &App, monitor: &SessionMonitorView) -> Vec<String> {
    let mut lines = vec![format!("Session Monitor | {}", monitor.section.label())];
    let persistence = match &app.observation_persistence {
        ObservationPersistenceState::Available { revision } => {
            format!("Persistence: committed revision {revision}")
        }
        ObservationPersistenceState::Unavailable(reason) => {
            format!("Persistence unavailable: {reason}")
        }
    };
    let monitor_node = if monitor.node_restarted {
        None
    } else {
        app.nodes.iter().find(|node| {
            node.node_id == monitor.key.target.node_id()
                && node.incarnation_id == Some(monitor.key.target.incarnation())
        })
    };
    lines.push(format!(
        "{persistence} | Node route: {}",
        relay_route_label(
            monitor_node
                .map(|node| node.relay_route)
                .unwrap_or(C2RelayRoute::Unknown),
        ),
    ));
    if !monitor.support.known {
        lines.push("Observation support unknown: waiting for node metadata".to_owned());
        return lines;
    }
    if matches!(&monitor.key.target, SessionMonitorTarget::Managed { .. })
        && !monitor.support.managed_target
    {
        lines.push("Managed target unavailable: capability not negotiated".to_owned());
        return lines;
    }
    if !monitor.support.events {
        lines.push("Observation events unavailable: capability not negotiated".to_owned());
        return lines;
    }
    let Some(projection) = app.session_monitor_projection(&monitor.key) else {
        lines.push("Waiting for observation events".to_owned());
        if matches!(monitor.section, SessionMonitorSection::Workflow | SessionMonitorSection::FilesGit)
            && !monitor.support.workflow_detail
        {
            lines.push("Workflow detail unavailable: capability not negotiated".to_owned());
        }
        return lines;
    };
    if matches!(monitor.section, SessionMonitorSection::Workflow | SessionMonitorSection::FilesGit)
        && !monitor.support.workflow_detail
    {
        lines.push("Workflow detail unavailable: capability not negotiated".to_owned());
        match monitor.section {
            SessionMonitorSection::Workflow => lines.push(format!(
                "Todo snapshots: {}",
                observation_capability_label(projection, |capabilities| capabilities.todo),
            )),
            SessionMonitorSection::FilesGit => lines.push(format!(
                "File-change events: {}",
                observation_capability_label(projection, |capabilities| capabilities.file_changes),
            )),
            _ => {}
        }
        return lines;
    }

    match monitor.section {
        SessionMonitorSection::Overview => {
            let incarnation = monitor.key.target.incarnation().to_string();
            match &monitor.key.target {
                SessionMonitorTarget::Runtime { address, .. } => lines.push(format!(
                    "Identity: runtime {} / {} / #{}:{} / incarnation {}",
                    address.node_id,
                    address.workspace_id,
                    address.instance_id,
                    address.generation,
                    &incarnation[..incarnation.len().min(8)],
                )),
                SessionMonitorTarget::Managed { node_id, record_id, .. } => {
                    lines.push(format!(
                        "Identity: managed {node_id} / @{record_id} / incarnation {}",
                        &incarnation[..incarnation.len().min(8)],
                    ));
                    if let Some(record) = app.find_managed_session(&monitor.key.agent) {
                        lines.push(format!(
                            "Managed record: {} | {}",
                            managed_state_label(record.state),
                            if record.active_session.is_some() {
                                "active runtime linked"
                            } else {
                                "no active runtime"
                            },
                        ));
                    } else {
                        lines.push("Managed record: unavailable | retained facts".to_owned());
                    }
                }
            }
            let transport = if monitor.node_restarted {
                "node restarted; historical session"
            } else {
                monitor_node
                    .map(|node| match node.connection {
                        ConnectionState::Connected => "connected",
                        ConnectionState::Connecting => "connecting",
                        ConnectionState::Resyncing => "resyncing",
                        ConnectionState::Disconnected(_) => "disconnected; retained facts",
                    })
                    .unwrap_or("historical; retained facts")
            };
            lines.push(format!("Transport: {transport}"));
            lines.push(format!(
                "Projection: {} | {}",
                projection_availability_label(projection.availability),
                projection_freshness_label(projection.freshness),
            ));
            let last = projection.timeline.back();
            let source_epoch = projection.timeline.iter()
                .filter(|entry| matches!(entry.kind, gate4agent_node_protocol::ObservationKindV1::SourceReset))
                .count();
            lines.push(format!(
                "Source: epoch {} | sequence {} | {}",
                source_epoch,
                last.map(|entry| entry.cursor.sequence.to_string()).unwrap_or_else(|| "waiting".to_owned()),
                if projection.incomplete_evidence.is_empty() {
                    "no reported source gap"
                } else {
                    "partial"
                },
            ));
            if let Some(last) = last {
                lines.push(format!("Evidence: {}", observation_evidence_label(last.evidence)));
                if last.evidence == gate4agent_node_protocol::ObservationEvidenceV1::PtyHint {
                    lines.push("PTY hint is non-authoritative".to_owned());
                }
            }
            lines.push(format!("Observations retained: {}", projection.timeline.len()));
        }
        SessionMonitorSection::Workflow => {
            let current = projection.timeline.iter().rev()
                .find(|entry| !matches!(
                    entry.kind,
                    gate4agent_node_protocol::ObservationKindV1::TodoSnapshot { .. }
                        | gate4agent_node_protocol::ObservationKindV1::Usage { .. }
                        | gate4agent_node_protocol::ObservationKindV1::ContextWindowUsage { .. }
                        | gate4agent_node_protocol::ObservationKindV1::FileChanged { .. }
                        | gate4agent_node_protocol::ObservationKindV1::Gap { .. }
                        | gate4agent_node_protocol::ObservationKindV1::SourceReset
                        | gate4agent_node_protocol::ObservationKindV1::Stale
                ));
            lines.push(format!(
                "Current: {}",
                current.map(|entry| crate::app::observation_kind_label(&entry.kind))
                    .unwrap_or("not observed")
            ));
            if let Some(todo) = projection.todos.current.as_ref() {
                let todos = &todo.items;
                let pending = todos.iter().filter(|item| matches!(item.state, gate4agent_node_protocol::ObservationTodoStateV1::Pending)).count();
                let active = todos.iter().filter(|item| matches!(item.state, gate4agent_node_protocol::ObservationTodoStateV1::InProgress)).count();
                let completed = todos.iter().filter(|item| matches!(item.state, gate4agent_node_protocol::ObservationTodoStateV1::Completed)).count();
                let unknown = todos.len().saturating_sub(pending + active + completed);
                lines.push(format!("Todo snapshot: {} items | pending {pending} | active {active} | completed {completed} | unknown {unknown}", todos.len()));
                lines.push(format!("Todo complete: {}", todo.complete));
                if projection.todos.conflict {
                    lines.push("Todo revision conflict; projection is partial".to_owned());
                }
            } else {
                lines.push(format!(
                    "Todo snapshots: {}",
                    observation_capability_label(projection, |capabilities| capabilities.todo),
                ));
            }
            if !projection.stale_evidence.is_empty() {
                lines.push("Source marked stale; workflow projection is partial".to_owned());
            }
        }
        SessionMonitorSection::Subagents => {
            if projection.subagents.is_empty() {
                lines.push(format!(
                    "Subagent events: {}",
                    observation_capability_label(projection, |capabilities| capabilities.subagents),
                ));
            }
            for fact in projection.subagents.iter().rev() {
                lines.push(correlation_line(fact));
            }
        }
        SessionMonitorSection::Tools => {
            if projection.tools.is_empty() && projection.owned_processes.is_empty() {
                lines.push(format!(
                    "Tool events: {}",
                    observation_capability_label(projection, |capabilities| capabilities.tools),
                ));
                lines.push(format!(
                    "Owned-process events: {}",
                    observation_capability_label(projection, |capabilities| capabilities.owned_processes),
                ));
            }
            for fact in projection.tools.iter().rev() {
                lines.push(format!("tool | {}", correlation_line(fact)));
            }
            for fact in projection.owned_processes.iter().rev() {
                lines.push(format!("process | {}", correlation_line(fact)));
            }
        }
        SessionMonitorSection::Approvals => {
            if projection.interactions.is_empty() {
                lines.push(format!(
                    "Attention events: {}",
                    observation_capability_label(projection, |capabilities| capabilities.attention),
                ));
            }
            for fact in projection.interactions.iter().rev() {
                lines.push(correlation_line(fact));
            }
        }
        SessionMonitorSection::FilesGit => {
            if projection.files.is_empty() {
                lines.push(format!(
                    "File-change events: {}",
                    observation_capability_label(projection, |capabilities| capabilities.file_changes),
                ));
            }
            for file in projection.files.iter().rev() {
                lines.push(format!(
                    "Changed: {} | {}",
                    file.path.as_deref().unwrap_or("path not reported"),
                    observation_evidence_label(file.evidence),
                ));
            }
        }
        SessionMonitorSection::Usage => {
            let usage = projection.usage.observed_delta;
            match available_context_occupancy(projection) {
                Ok((snapshot, context_window)) => {
                    let occupied = snapshot.occupied_tokens();
                    let status = if occupied > context_window {
                        format!(
                            "Context: {occupied} / {context_window} tokens | over-capacity; exceeds reported window by {}",
                            occupied.saturating_sub(context_window),
                        )
                    } else {
                        format!("Context: {occupied} / {context_window} tokens")
                    };
                    lines.push(status);
                    lines.push(String::new());
                    lines.push(format!("Uncached input: {}", snapshot.uncached_input_tokens));
                    lines.push(format!("Cache read: {}", snapshot.cache_read_tokens));
                    lines.push(format!("Cache write: {}", snapshot.cache_write_tokens));
                    lines.push(format!("Provider output: {}", snapshot.output_tokens));
                    lines.push(format!("Unattributed: {}", snapshot.unattributed_tokens));
                    lines.push(format!(
                        "Remaining: {}",
                        context_window.saturating_sub(occupied),
                    ));
                    lines.push(format!(
                        "Source: {} | exact current-window signal",
                        observation_evidence_label(snapshot.evidence),
                    ));
                }
                Err(reason) => lines.push(format!("Context: not reported | {reason}")),
            }
            if projection.usage.last_cumulative.is_some()
                || usage.input_tokens != 0
                || usage.output_tokens != 0
                || usage.cache_read_tokens != 0
                || usage.cache_write_tokens != 0
                || usage.reasoning_tokens != 0
            {
                lines.push(format!("Observed input delta: {}", usage.input_tokens));
                lines.push(format!("Observed output delta: {}", usage.output_tokens));
                lines.push(format!("Observed cache read / write delta: {} / {}", usage.cache_read_tokens, usage.cache_write_tokens));
                lines.push(format!("Observed reasoning delta: {}", usage.reasoning_tokens));
                lines.push("Accounting: observed delta; not active-context occupancy".to_owned());
            } else {
                lines.push(format!(
                    "Usage events: {}",
                    observation_capability_label(projection, |capabilities| capabilities.usage),
                ));
            }
            if let Some(history) = projection.history.as_ref() {
                let messages = if history.message_count_exact {
                    format!("{}", history.message_count)
                } else {
                    format!("{} observed; total incomplete", history.message_count)
                };
                let turns = history
                    .completed_turn_count
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_owned());
                let tokens = history
                    .total_tokens
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_owned());
                lines.push(format!(
                    "History summary: messages {messages} | completed turns {turns} | total tokens {tokens}",
                ));
            } else {
                lines.push(format!(
                    "History summary: {}",
                    observation_capability_label(projection, |capabilities| capabilities.history_summary),
                ));
            }
        }
        SessionMonitorSection::Timeline => {
            for item in projection.timeline.iter().rev() {
                lines.push(format!(
                    "node {} | received {} | {} | {}",
                    item.cursor.sequence,
                    item.received_at_ms,
                    observation_evidence_label(item.evidence),
                    crate::app::observation_kind_label(&item.kind),
                ));
            }
        }
    }
    lines
}

fn observation_capability_label(
    projection: &SessionProjection,
    supported: impl Fn(gate4agent_node_protocol::ObservationCapabilitiesV1) -> bool,
) -> &'static str {
    if projection.source_capabilities.is_empty() {
        "not observed"
    } else if projection
        .source_capabilities
        .iter()
        .any(|source| supported(source.capabilities))
    {
        "supported; no event observed"
    } else {
        "not supported by observed sources"
    }
}

fn projection_availability_label(availability: ProjectionAvailability) -> &'static str {
    match availability {
        ProjectionAvailability::Unknown => "unknown",
        ProjectionAvailability::NotObserved => "not observed",
        ProjectionAvailability::Current => "current",
        ProjectionAvailability::Partial => "partial",
        ProjectionAvailability::Frozen => "frozen",
    }
}

fn projection_freshness_label(freshness: ProjectionFreshness) -> &'static str {
    match freshness {
        ProjectionFreshness::Live => "live",
        ProjectionFreshness::LastKnown => "last known",
        ProjectionFreshness::Stale => "stale",
        ProjectionFreshness::IncompleteAfterGap => "incomplete after gap",
        ProjectionFreshness::ReplacedIncarnation => "replaced incarnation",
        ProjectionFreshness::Unavailable => "unavailable",
    }
}

fn correlation_line(fact: &CorrelationProjection) -> String {
    let class = fact.class.as_deref().unwrap_or("class not reported");
    let state = match fact.state {
        CorrelationState::Pending => "pending".to_owned(),
        CorrelationState::Completed { success } => success
            .map(|success| if success { "completed | success" } else { "completed | failure" })
            .unwrap_or("completed")
            .to_owned(),
        CorrelationState::Resolved { outcome } => {
            format!("resolved | {}", interaction_outcome_label(outcome))
        }
        CorrelationState::UnknownAfterGap => "unknown after gap".to_owned(),
        CorrelationState::OrphanCompletion { success } => success
            .map(|success| if success { "orphan completion | success" } else { "orphan completion | failure" })
            .unwrap_or("orphan completion")
            .to_owned(),
        CorrelationState::OrphanResolution { outcome } => {
            format!("orphan resolution | {}", interaction_outcome_label(outcome))
        }
    };
    format!("{class} | {state} | {}", observation_evidence_label(fact.evidence))
}

fn observation_evidence_label(evidence: gate4agent_node_protocol::ObservationEvidenceV1) -> &'static str {
    match evidence {
        gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider => "structured provider",
        gate4agent_node_protocol::ObservationEvidenceV1::ManagedHook => "managed hook",
        gate4agent_node_protocol::ObservationEvidenceV1::NodeLifecycle => "node lifecycle",
        gate4agent_node_protocol::ObservationEvidenceV1::WorkspaceObservation => "workspace observation",
        gate4agent_node_protocol::ObservationEvidenceV1::PtyHint => "PTY hint",
        gate4agent_node_protocol::ObservationEvidenceV1::HistoryProjection => "history",
    }
}

fn interaction_outcome_label(
    outcome: gate4agent_node_protocol::ObservationInteractionOutcomeV1,
) -> &'static str {
    match outcome {
        gate4agent_node_protocol::ObservationInteractionOutcomeV1::Approved => "approved",
        gate4agent_node_protocol::ObservationInteractionOutcomeV1::Answered => "answered",
        gate4agent_node_protocol::ObservationInteractionOutcomeV1::Denied => "denied",
        gate4agent_node_protocol::ObservationInteractionOutcomeV1::Interrupted => "interrupted",
        gate4agent_node_protocol::ObservationInteractionOutcomeV1::TurnEnded => "turn ended",
        gate4agent_node_protocol::ObservationInteractionOutcomeV1::Superseded => "superseded",
    }
}

fn render_agent_board(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if app.agent_board_mode == crate::app::AgentBoardMode::HarnessKanban {
        render_harness_kanban(app, area, buf, layout, theme);
        return;
    }
    let cards = app.agent_board_cards();
    let area = if let Some(task_id) = app.agent_board.task_filter.as_ref().filter(|_| !cards.is_empty()) {
        let header = format!("Task {task_id} · {} observed runs", cards.len());
        Paragraph::new(truncate_cells(&header, area.width as usize))
            .style(Style::default().fg(theme.text).bg(theme.active).add_modifier(Modifier::BOLD))
            .render(Rect::new(area.x, area.y, area.width, 1), buf);
        Rect::new(area.x, area.y.saturating_add(1), area.width, area.height.saturating_sub(1))
    } else {
        area
    };
    if cards.is_empty() {
        Paragraph::new(
            "No managed or live agents in the current topology. Provider history is read-only; resume a session to monitor it here.",
        )
            .style(Style::default().fg(theme.muted).bg(theme.surface))
            .render(area, buf);
        return;
    }
    let visible_count = (area.width / 24).clamp(1, AgentBoardColumn::ALL.len() as u16) as usize;
    let maximum_start = AgentBoardColumn::ALL.len().saturating_sub(visible_count);
    let mut start = app.agent_board.column_scroll.min(maximum_start);
    if let Some(selected) = app.agent_board.selected.as_ref() {
        if let Some(card) = cards.iter().find(|card| &card.key == selected) {
            if let Some(index) = AgentBoardColumn::ALL
                .iter()
                .position(|column| *column == card.column)
            {
                if index < start {
                    start = index;
                } else if index >= start.saturating_add(visible_count) {
                    start = index.saturating_add(1).saturating_sub(visible_count);
                }
            }
        }
    }
    let base_width = area.width / visible_count as u16;
    let remainder = area.width % visible_count as u16;
    let card_height = 5_u16;
    let mut x = area.x;
    for visible_index in 0..visible_count {
        let column = AgentBoardColumn::ALL[start + visible_index];
        let width = base_width + u16::from((visible_index as u16) < remainder);
        let column_cards = cards
            .iter()
            .filter(|card| card.column == column)
            .collect::<Vec<_>>();
        Paragraph::new(format!("{} ({})", column.label(), column_cards.len()))
            .style(
                Style::default()
                    .fg(theme.text)
                    .bg(theme.active)
                    .add_modifier(Modifier::BOLD),
            )
            .render(Rect::new(x, area.y, width.saturating_sub(1), 1), buf);
        if column_cards.is_empty() {
            if area.height > 1 {
                Paragraph::new("No sessions observed")
                    .style(Style::default().fg(theme.muted).bg(theme.surface))
                    .render(Rect::new(x, area.y + 1, width.saturating_sub(1), 1), buf);
            }
            x = x.saturating_add(width);
            continue;
        }
        let capacity = area.height.saturating_sub(1) as usize / card_height as usize;
        if capacity == 0 {
            x = x.saturating_add(width);
            continue;
        }
        let mut offset = app
            .agent_board
            .vertical_offsets
            .get(&column)
            .copied()
            .unwrap_or(0)
            .min(column_cards.len().saturating_sub(1));
        if let Some(selected) = app.agent_board.selected.as_ref() {
            if let Some(index) = column_cards.iter().position(|card| &card.key == selected) {
                if index < offset {
                    offset = index;
                } else if index >= offset.saturating_add(capacity) {
                    offset = index.saturating_add(1).saturating_sub(capacity);
                }
            }
        }
        for (slot, card) in column_cards.iter().skip(offset).take(capacity).enumerate() {
            let y = area.y.saturating_add(1 + slot as u16 * card_height);
            let card_area = Rect::new(
                x,
                y,
                width.saturating_sub(1),
                card_height.min(area.bottom().saturating_sub(y)),
            );
            render_agent_board_card(app, card, card_area, buf, layout, theme);
        }
        x = x.saturating_add(width);
    }
}

fn render_harness_kanban(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    use crate::app::HarnessKanbanColumn;
    if area.width == 0 || area.height == 0 {
        return;
    }
    if let Some(monitor) = app.harness_kanban.monitor.as_ref() {
        render_harness_monitor(monitor, area, buf, theme);
        return;
    }
    draw_harness_board_frame(area, buf, theme);
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let visible_count = (inner.width / 22)
        .clamp(1, HarnessKanbanColumn::ALL.len() as u16) as usize;
    let maximum_start = HarnessKanbanColumn::ALL.len().saturating_sub(visible_count);
    let start = app.harness_kanban.column_scroll.min(maximum_start);
    let button_style = Style::default().fg(theme.teal).bg(theme.active);
    let selected_tab_style = Style::default()
        .fg(theme.text)
        .bg(theme.panel)
        .add_modifier(Modifier::BOLD);
    let global_header = Rect::new(inner.x, inner.y, inner.width, inner.height.min(1));
    fill_rect(global_header, theme.active, buf);
    let mut action_x = render_toolbar_segment(
        "[Tasks]",
        global_header.x,
        global_header,
        selected_tab_style,
        Some(HitTarget::HarnessBoardMode(crate::app::AgentBoardMode::HarnessKanban)),
        buf,
        layout,
    ).saturating_add(1);
    action_x = render_toolbar_segment(
        "[Runtime]",
        action_x,
        global_header,
        button_style,
        Some(HitTarget::HarnessBoardMode(crate::app::AgentBoardMode::SessionMonitoring)),
        buf,
        layout,
    ).saturating_add(1);
    for (label, target) in [
        ("[New task]", HitTarget::HarnessTaskCreate),
        ("[Refresh]", HitTarget::HarnessTaskRefresh),
        ("[Run next Ready]", HitTarget::HarnessScheduleNext),
    ] {
        action_x = render_toolbar_segment(
            label,
            action_x,
            global_header,
            button_style,
            Some(target),
            buf,
            layout,
        ).saturating_add(1);
    }
    let mut header_rows = 1_u16;
    if let Some(task) = app.harness_selected_task() {
        let context_area = Rect::new(
            inner.x,
            inner.y.saturating_add(header_rows),
            inner.width,
            inner.height.saturating_sub(header_rows).min(1),
        );
        fill_rect(context_area, theme.active, buf);
        let column = HarnessKanbanColumn::from_state(task.state);
        let index = HarnessKanbanColumn::ALL.iter()
            .position(|candidate| *candidate == column).unwrap_or(0);
        let previous = HarnessKanbanColumn::ALL[..index].iter().rev()
            .map(|column| column.state())
            .find(|state| crate::app::harness_operator_move_allowed(task.state, *state));
        let next = HarnessKanbanColumn::ALL[index.saturating_add(1)..].iter()
            .map(|column| column.state())
            .find(|state| crate::app::harness_operator_move_allowed(task.state, *state));
        let mut context_x = context_area.x;
        for state in previous.into_iter().chain(next) {
            let label = format!("[Move to {}]", HarnessKanbanColumn::from_state(state).label());
            context_x = render_toolbar_segment(
                &label,
                context_x,
                context_area,
                button_style,
                Some(HitTarget::HarnessTaskMove(state)),
                buf,
                layout,
            ).saturating_add(1);
        }
        if !matches!(task.state, gate4agent_harness_client::HarnessTaskStateV1::Done | gate4agent_harness_client::HarnessTaskStateV1::Cancelled) {
            context_x = render_toolbar_segment(
                "[Cancel task]",
                context_x,
                context_area,
                button_style,
                Some(HitTarget::HarnessTaskCancel),
                buf,
                layout,
            ).saturating_add(1);
        }
        if matches!(task.state, gate4agent_harness_client::HarnessTaskStateV1::Failed | gate4agent_harness_client::HarnessTaskStateV1::Cancelled) {
            context_x = render_toolbar_segment(
                "[Retry task]",
                context_x,
                context_area,
                button_style,
                Some(HitTarget::HarnessTaskRetry),
                buf,
                layout,
            ).saturating_add(1);
        }
        if app.harness_selected_bound_run().is_some() {
            let _ = render_toolbar_segment(
                "[Run details]",
                context_x,
                context_area,
                button_style,
                Some(HitTarget::HarnessTaskMonitor),
                buf,
                layout,
            );
        }
        header_rows = header_rows.saturating_add(1);
    }
    let status = app.harness_kanban.stale_reason.as_ref()
        .map(|reason| (
            format!("STALE | last complete Harness snapshot retained | {reason}"),
            theme.yellow,
        ))
        .or_else(|| app.harness_kanban.pending_refresh.map(|_| (
            "Refreshing authoritative Harness snapshot...".to_owned(),
            theme.muted,
        )));
    if let Some((status, color)) = status {
        let status_area = Rect::new(
            inner.x,
            inner.y.saturating_add(header_rows),
            inner.width,
            inner.height.saturating_sub(header_rows).min(1),
        );
        Paragraph::new(truncate_cells(&status, status_area.width as usize))
            .style(Style::default().fg(color).bg(theme.active).add_modifier(Modifier::BOLD))
            .render(status_area, buf);
        header_rows = header_rows.saturating_add(1);
    }
    let header_area = Rect::new(inner.x, inner.y, inner.width, header_rows.min(inner.height));
    let header_divider_y = inner.y.saturating_add(header_rows);
    draw_harness_board_divider(area, header_divider_y, buf, theme);
    let column_header_y = header_divider_y.saturating_add(1);
    let table_divider_y = column_header_y.saturating_add(1);
    draw_harness_board_divider(area, table_divider_y, buf, theme);
    let body_top = table_divider_y.saturating_add(1);
    let body_bottom = area.bottom().saturating_sub(1);
    let body_height = body_bottom.saturating_sub(body_top);
    let tasks = app.harness_tasks();
    let separator_count = visible_count.saturating_sub(1) as u16;
    let table_width = inner.width.saturating_sub(separator_count);
    let base_width = table_width / visible_count as u16;
    let remainder = table_width % visible_count as u16;
    let card_height = 5_u16;
    let mut x = inner.x;
    for visible_index in 0..visible_count {
        let column = HarnessKanbanColumn::ALL[start + visible_index];
        let width = base_width + u16::from((visible_index as u16) < remainder);
        let column_tasks = tasks.iter()
            .copied()
            .filter(|task| HarnessKanbanColumn::from_state(task.state) == column)
            .collect::<Vec<_>>();
        Paragraph::new(truncate_cells(
            &format!("{} ({})", column.label(), column_tasks.len()),
            width.saturating_sub(2) as usize,
        ))
            .style(Style::default().fg(theme.text).bg(theme.surface).add_modifier(Modifier::BOLD))
            .render(Rect::new(
                x.saturating_add(1),
                column_header_y,
                width.saturating_sub(2),
                u16::from(column_header_y < area.bottom()),
            ), buf);
        if column_tasks.is_empty() && body_height > 0 {
            Paragraph::new("No tasks")
                .style(Style::default().fg(theme.muted).bg(theme.surface))
                .render(Rect::new(
                    x.saturating_add(1),
                    body_top,
                    width.saturating_sub(2),
                    1,
                ), buf);
        }
        let capacity = body_height as usize / card_height as usize;
        let mut offset = app.harness_kanban.vertical_offsets.get(&column)
            .copied().unwrap_or(0).min(column_tasks.len().saturating_sub(1));
        if let Some(selected) = app.harness_kanban.selected.as_ref() {
            if let Some(index) = column_tasks.iter().position(|task| &task.task_id == selected) {
                if index < offset {
                    offset = index;
                } else if index >= offset.saturating_add(capacity.max(1)) {
                    offset = index.saturating_add(1).saturating_sub(capacity.max(1));
                }
            }
        }
        for (slot, task) in column_tasks.iter().skip(offset).take(capacity).enumerate() {
            let y = body_top.saturating_add(slot as u16 * card_height);
            render_harness_task_card(
                app,
                task,
                Rect::new(
                    x.saturating_add(1),
                    y,
                    width.saturating_sub(2),
                    card_height.min(body_bottom.saturating_sub(y)),
                ),
                buf,
                layout,
                theme,
            );
        }
        x = x.saturating_add(width);
        if visible_index + 1 < visible_count {
            for y in column_header_y..body_bottom {
                let separator = buf.get_mut(x, y);
                separator.symbol = if y == table_divider_y { "┼".into() } else { "│".into() };
                separator.style = Style::default().fg(theme.border).bg(theme.surface);
            }
            if area.height > 1 {
                let bottom = buf.get_mut(x, area.bottom() - 1);
                bottom.symbol = "┴".into();
                bottom.style = Style::default().fg(theme.border).bg(theme.surface);
            }
            x = x.saturating_add(1);
        }
    }
    if app.harness_kanban.hover_position
        .is_some_and(|(column, row)| header_area.contains(column, row))
    {
        let left_arrow = Rect::new(header_area.x, global_header.y, header_area.width.min(1), 1);
        let right_arrow = Rect::new(
            header_area.right().saturating_sub(1),
            global_header.y,
            header_area.width.min(1),
            1,
        );
        if start > 0 {
            Paragraph::new("‹")
                .style(Style::default().fg(theme.teal).bg(theme.active).add_modifier(Modifier::BOLD))
                .render(left_arrow, buf);
            layout.hits.push(HitRegion {
                rect: left_arrow,
                target: HitTarget::HarnessColumnScroll(start.saturating_sub(1)),
            });
        }
        if start < maximum_start {
            Paragraph::new("›")
                .style(Style::default().fg(theme.teal).bg(theme.active).add_modifier(Modifier::BOLD))
                .render(right_arrow, buf);
            layout.hits.push(HitRegion {
                rect: right_arrow,
                target: HitTarget::HarnessColumnScroll(start.saturating_add(1)),
            });
        }
    }
    if let Some(composer) = app.harness_kanban.composer.as_ref() {
        let width = area.width.min(72).max(1);
        let height = area.height.min(8).max(1);
        let modal = Rect::new(
            area.x.saturating_add(area.width.saturating_sub(width) / 2),
            area.y.saturating_add(area.height.saturating_sub(height) / 2),
            width,
            height,
        );
        draw_harness_composer_frame(modal, buf, theme);
        Paragraph::new("Create Harness task | Tab / Enter / Esc also work")
            .style(Style::default().fg(theme.text).bg(theme.panel).add_modifier(Modifier::BOLD))
            .render(Rect::new(
                modal.x.saturating_add(1),
                modal.y.saturating_add(1),
                modal.width.saturating_sub(2),
                1,
            ), buf);
        let title_marker = if composer.field == crate::app::HarnessTaskComposerField::Title { ">" } else { " " };
        let body_marker = if composer.field == crate::app::HarnessTaskComposerField::Body { ">" } else { " " };
        let title_rect = Rect::new(
            modal.x.saturating_add(1),
            modal.y.saturating_add(3),
            modal.width.saturating_sub(2),
            1,
        );
        Paragraph::new(truncate_cells(&format!("{title_marker} Title: {}", composer.title), title_rect.width as usize))
            .style(Style::default().fg(theme.teal).bg(theme.panel))
            .render(title_rect, buf);
        layout.hits.push(HitRegion {
            rect: title_rect,
            target: HitTarget::HarnessComposerField(crate::app::HarnessTaskComposerField::Title),
        });
        let body_rect = Rect::new(
            modal.x.saturating_add(1),
            modal.y.saturating_add(4),
            modal.width.saturating_sub(2),
            1,
        );
        Paragraph::new(truncate_cells(&format!("{body_marker} Body: {}", composer.body), body_rect.width as usize))
            .style(Style::default().fg(theme.dim).bg(theme.panel))
            .render(body_rect, buf);
        layout.hits.push(HitRegion {
            rect: body_rect,
            target: HitTarget::HarnessComposerField(crate::app::HarnessTaskComposerField::Body),
        });
        let buttons = Rect::new(
            modal.x.saturating_add(1),
            modal.bottom().saturating_sub(2),
            modal.width.saturating_sub(2),
            1,
        );
        let create_end = render_toolbar_segment(
            "[Create in Backlog]",
            buttons.x,
            buttons,
            Style::default().fg(theme.teal).bg(theme.panel),
            Some(HitTarget::HarnessComposerCreate),
            buf,
            layout,
        );
        let _ = render_toolbar_segment(
            "[Cancel]",
            create_end.saturating_add(1),
            buttons,
            Style::default().fg(theme.yellow).bg(theme.panel),
            Some(HitTarget::HarnessComposerCancel),
            buf,
            layout,
        );
    }
}

fn draw_harness_composer_frame(
    area: Rect,
    buf: &mut TerminalBuffer,
    theme: Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    fill_rect(area, theme.panel, buf);
    let style = Style::default().fg(theme.accent).bg(theme.panel);
    for x in area.x..area.right() {
        let top = buf.get_mut(x, area.y);
        top.symbol = "─".into();
        top.style = style;
        if area.height > 1 {
            let bottom = buf.get_mut(x, area.bottom() - 1);
            bottom.symbol = "─".into();
            bottom.style = style;
        }
    }
    for y in area.y..area.bottom() {
        let left = buf.get_mut(area.x, y);
        left.symbol = "│".into();
        left.style = style;
        if area.width > 1 {
            let right = buf.get_mut(area.right() - 1, y);
            right.symbol = "│".into();
            right.style = style;
        }
    }
    if area.width > 1 && area.height > 1 {
        for (x, y, symbol) in [
            (area.x, area.y, "┌"),
            (area.right() - 1, area.y, "┐"),
            (area.x, area.bottom() - 1, "└"),
            (area.right() - 1, area.bottom() - 1, "┘"),
        ] {
            let corner = buf.get_mut(x, y);
            corner.symbol = symbol.into();
            corner.style = style;
        }
    }
    if area.height > 3 {
        let divider_y = area.y.saturating_add(2);
        for x in area.x..area.right() {
            let divider = buf.get_mut(x, divider_y);
            divider.symbol = if x == area.x {
                "├".into()
            } else if x == area.right().saturating_sub(1) {
                "┤".into()
            } else {
                "─".into()
            };
            divider.style = style;
        }
    }
}

fn draw_harness_board_frame(
    area: Rect,
    buf: &mut TerminalBuffer,
    theme: Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    fill_rect(area, theme.surface, buf);
    let border_style = Style::default().fg(theme.border).bg(theme.surface);
    for x in area.x..area.right() {
        let top = buf.get_mut(x, area.y);
        top.symbol = "─".into();
        top.style = border_style;
        if area.height > 1 {
            let bottom = buf.get_mut(x, area.bottom() - 1);
            bottom.symbol = "─".into();
            bottom.style = border_style;
        }
    }
    for y in area.y..area.bottom() {
        let left = buf.get_mut(area.x, y);
        left.symbol = "│".into();
        left.style = border_style;
        if area.width > 1 {
            let right = buf.get_mut(area.right() - 1, y);
            right.symbol = "│".into();
            right.style = border_style;
        }
    }
    if area.width > 1 && area.height > 1 {
        for (x, y, symbol) in [
            (area.x, area.y, "┌"),
            (area.right() - 1, area.y, "┐"),
            (area.x, area.bottom() - 1, "└"),
            (area.right() - 1, area.bottom() - 1, "┘"),
        ] {
            let corner = buf.get_mut(x, y);
            corner.symbol = symbol.into();
            corner.style = border_style;
        }
    }
}

fn draw_harness_board_divider(
    area: Rect,
    y: u16,
    buf: &mut TerminalBuffer,
    theme: Theme,
) {
    if y <= area.y || y >= area.bottom().saturating_sub(1) {
        return;
    }
    let style = Style::default().fg(theme.border).bg(theme.surface);
    for x in area.x..area.right() {
        let cell = buf.get_mut(x, y);
        cell.symbol = if x == area.x {
            "├".into()
        } else if x == area.right().saturating_sub(1) {
            "┤".into()
        } else {
            "─".into()
        };
        cell.style = style;
    }
}

fn render_harness_task_card(
    app: &App,
    task: &gate4agent_harness_client::RedactedTaskV1,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let selected = app.harness_kanban.selected.as_ref() == Some(&task.task_id);
    let background = if selected { theme.active } else { theme.panel };
    fill_rect(area, background, buf);
    layout.hits.push(HitRegion {
        rect: area,
        target: HitTarget::HarnessTaskCard(task.task_id.clone()),
    });
    Paragraph::new(truncate_cells(&task.title, area.width as usize))
        .style(Style::default().fg(theme.text).bg(background).add_modifier(Modifier::BOLD))
        .render(Rect::new(area.x, area.y, area.width, 1), buf);
    if area.height > 1 {
        let id = task.task_id.as_str();
        let compact = format!("{}... | rev {}", &id[..id.len().min(14)], task.revision.get());
        Paragraph::new(truncate_cells(&compact, area.width as usize))
            .style(Style::default().fg(theme.dim).bg(background))
            .render(Rect::new(area.x, area.y + 1, area.width, 1), buf);
    }
    if area.height > 2 {
        let links = format!("deps {} | runs {}", task.dependency_ids.len(), task.run_ids.len());
        Paragraph::new(truncate_cells(&links, area.width as usize))
            .style(Style::default().fg(theme.muted).bg(background))
            .render(Rect::new(area.x, area.y + 2, area.width, 1), buf);
    }
    if area.height > 3 {
        Paragraph::new(truncate_cells(&task.body, area.width as usize))
            .style(Style::default().fg(theme.dim).bg(background))
            .render(Rect::new(area.x, area.y + 3, area.width, 1), buf);
    }
    if area.height > 4 && selected && app.harness_selected_bound_run().is_some() {
        Paragraph::new("[details]")
            .style(Style::default().fg(theme.teal).bg(background))
            .render(Rect::new(area.x, area.y + 4, area.width.min(9), 1), buf);
    }
}

fn render_harness_monitor(
    view: &crate::app::HarnessRunMonitorView,
    area: Rect,
    buf: &mut TerminalBuffer,
    theme: Theme,
) {
    fill_rect(area, theme.surface, buf);
    let heading = format!(
        "Harness run details | {} | binding {:?} | Esc board",
        view.run.run_id,
        view.run.binding,
    );
    Paragraph::new(truncate_cells(&heading, area.width as usize))
        .style(Style::default().fg(theme.text).bg(theme.active).add_modifier(Modifier::BOLD))
        .render(Rect::new(area.x, area.y, area.width, 1), buf);
    let status = if view.loading {
        "loading authoritative run details...".to_owned()
    } else if let Some(reason) = view.stale_reason.as_ref() {
        format!("STALE | {reason}")
    } else if let Some(monitor) = view.monitor.as_ref() {
        format!(
            "availability {:?} | freshness {:?} | todos {}/{} | tools {} | subagents {}",
            monitor.availability,
            monitor.freshness,
            monitor.todo_completed,
            monitor.todo_total,
            monitor.active_tools,
            monitor.active_subagents,
        )
    } else {
        "run details unavailable".to_owned()
    };
    if area.height > 1 {
        Paragraph::new(truncate_cells(&status, area.width as usize))
            .style(Style::default().fg(if view.stale_reason.is_some() { theme.yellow } else { theme.dim }).bg(theme.surface))
            .render(Rect::new(area.x, area.y + 1, area.width, 1), buf);
    }
    if area.height > 3 {
        let tokens = view.monitor.as_ref().map(|monitor| format!(
            "tokens in {} | out {} | cache read {} | reasoning {}",
            monitor.input_tokens,
            monitor.output_tokens,
            monitor.cache_read_tokens,
            monitor.reasoning_tokens,
        )).unwrap_or_default();
        Paragraph::new(truncate_cells(&tokens, area.width as usize))
            .style(Style::default().fg(theme.dim).bg(theme.surface))
            .render(Rect::new(area.x, area.y + 3, area.width, 1), buf);
    }
    for (index, event) in view.timeline.iter().rev()
        .take(area.height.saturating_sub(5) as usize).enumerate()
    {
        let line = format!("#{} {:?} via {:?}", event.sequence, event.category, event.evidence);
        Paragraph::new(truncate_cells(&line, area.width as usize))
            .style(Style::default().fg(theme.muted).bg(theme.surface))
            .render(Rect::new(area.x, area.y + 5 + index as u16, area.width, 1), buf);
    }
}

fn render_agent_board_card(
    app: &App,
    card: &AgentBoardCard,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let selected = app.agent_board.selected.as_ref() == Some(&card.key);
    let background = if selected { theme.active } else { theme.panel };
    fill_rect(area, background, buf);
    layout.hits.push(HitRegion {
        rect: area,
        target: HitTarget::AgentBoardCard(card.key.clone()),
    });
    let (primary, secondary) = match &card.key {
        AgentRowKey::Managed { .. } => {
            let Some(record) = app.find_managed_session(&card.key) else {
                return;
            };
            let primary = app
                .agent_local_alias(&card.key)
                .unwrap_or(&record.display_name)
                .to_owned();
            let state = managed_state_label(record.state);
            let task = record.task_binding.as_ref()
                .and_then(|binding| binding.task_id.as_ref())
                .map(|task_id| format!(" | {}", compact_task_id(task_id)))
                .unwrap_or_default();
            (
                primary,
                format!("{} | {state} | {} | {}{task}", record.provider, record.node_id, record.workspace_id),
            )
        }
        AgentRowKey::Legacy(address) => {
            let Some(session) = app.find_session(address) else {
                return;
            };
            (
                format!("{} agent", session.provider),
                format!(
                    "{} | {} | {}",
                    if session.running { "running" } else { "stopped" },
                    address.node_id,
                    address.workspace_id,
                ),
            )
        }
    };
    let open_label = "[open]";
    let open_width = (cell_width(open_label) as u16).min(area.width);
    let open_rect = Rect::new(area.right().saturating_sub(open_width), area.y, open_width, 1);
    let pin = if app.agent_is_pinned(&card.key) { "P " } else { "" };
    let title_width = area.width.saturating_sub(open_width).saturating_sub(1);
    Paragraph::new(truncate_cells(&format!("{pin}{primary}"), title_width as usize))
        .style(
            Style::default()
                .fg(theme.text)
                .bg(background)
                .add_modifier(Modifier::BOLD),
        )
        .render(Rect::new(area.x, area.y, title_width, 1), buf);
    if open_width > 0 {
        Paragraph::new(open_label)
            .style(Style::default().fg(theme.teal).bg(background))
            .render(open_rect, buf);
        layout.hits.push(HitRegion {
            rect: open_rect,
            target: HitTarget::AgentBoardCardOpen(card.key.clone()),
        });
    }
    if area.height > 1 {
        Paragraph::new(truncate_cells(&secondary, area.width as usize))
            .style(Style::default().fg(theme.dim).bg(background))
            .render(Rect::new(area.x, area.y + 1, area.width, 1), buf);
    }
    if area.height > 2 {
        let status = format!(
            "status: {}{}",
            card.reason,
            if card.partial { " partial" } else { "" },
        );
        Paragraph::new(truncate_cells(&status, area.width as usize))
            .style(Style::default().fg(theme.yellow).bg(background))
            .render(Rect::new(area.x, area.y + 2, area.width, 1), buf);
    }
    if area.height > 3 {
        let summary = app
            .agent_board_fresh_progress(card)
            .map(agent_board_progress_summary)
            .unwrap_or_else(|| "progress unavailable".to_owned());
        Paragraph::new(truncate_cells(&summary, area.width as usize))
            .style(Style::default().fg(theme.dim).bg(background))
            .render(Rect::new(area.x, area.y + 3, area.width, 1), buf);
    }
    if area.height > 4 {
        let run_label = if app.agent_run_lens_key() == Some(&card.key) {
            "[global]"
        } else {
            "[workspace]"
        };
        let progress_label = "[monitor]";
        let run_width = (cell_width(run_label) as u16).min(area.width);
        let run_rect = Rect::new(area.x, area.y + 4, run_width, 1);
        Paragraph::new(run_label)
            .style(Style::default().fg(theme.teal).bg(background))
            .render(run_rect, buf);
        if run_width > 0 {
            layout.hits.push(HitRegion {
                rect: run_rect,
                target: HitTarget::AgentBoardCardRun(card.key.clone()),
            });
        }
        let progress_x = run_rect.right().saturating_add(1);
        let progress_width = (cell_width(progress_label) as u16)
            .min(area.right().saturating_sub(progress_x));
        if progress_width > 0 {
            let progress_rect = Rect::new(progress_x, area.y + 4, progress_width, 1);
            Paragraph::new(progress_label)
                .style(Style::default().fg(theme.teal).bg(background))
                .render(progress_rect, buf);
            layout.hits.push(HitRegion {
                rect: progress_rect,
                target: HitTarget::AgentBoardCardProgress(card.key.clone()),
            });
        }
    }
}

fn agent_board_progress_summary(progress: &gate4agent_node_protocol::AgentProgressV1) -> String {
    let tool_classes = progress
        .active_tool_labels
        .iter()
        .filter_map(|label| match label.to_ascii_lowercase().as_str() {
            "read" => Some("Read"),
            "write" => Some("Write"),
            "edit" => Some("Edit"),
            "shell" => Some("Shell"),
            "search" => Some("Search"),
            "browse" => Some("Browse"),
            "git" => Some("Git"),
            "ask" => Some("Ask"),
            "task" => Some("Task"),
            _ => None,
        })
        .collect::<Vec<_>>();
    let tools = if tool_classes.is_empty() {
        format!("tools {}", progress.active_tool_count)
    } else {
        format!("tools {} [{}]", progress.active_tool_count, tool_classes.join(","))
    };
    let usage = progress
        .usage
        .map(|usage| format!(" | tokens {}/{}", usage.input_tokens, usage.output_tokens))
        .unwrap_or_default();
    format!(
        "turns {} | {tools}{usage} | subagents {}",
        progress.completed_turns,
        progress.subagent_count,
    )
}

fn render_surface_pane_tabs(
    app: &App,
    pane_id: PaneId,
    pane: &crate::surface::Pane<SurfaceTab>,
    focused: bool,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    fill_rect(area, theme.active, buf);
    if area.width == 0 || pane.tabs.is_empty() {
        return;
    }
    let labels = pane
        .tabs
        .iter()
        .map(|tab| surface_pane_tab_title(app, tab))
        .collect::<Vec<_>>();
    let selected_end = labels
        .iter()
        .take(pane.active.saturating_add(1))
        .map(|label| cell_width(&format!(" {label} ")).min(u16::MAX as usize) as u16)
        .fold(0_u16, |sum, width| sum.saturating_add(width));
    let start = if selected_end <= area.width {
        0
    } else {
        pane.active.min(labels.len().saturating_sub(1))
    };
    let mut x = area.x;
    for (index, label) in labels.iter().enumerate().skip(start) {
        if x >= area.right() {
            break;
        }
        let text = format!(" {label} ");
        let width = (cell_width(&text) as u16).min(area.right().saturating_sub(x));
        let selected = index == pane.active;
        let selected_background = if focused { theme.accent } else { theme.panel };
        Paragraph::new(truncate_cells(&text, width as usize))
            .style(
                Style::default()
                    .fg(if selected && focused { theme.active_tab_text } else if selected { theme.text } else { theme.muted })
                    .bg(if selected { selected_background } else { theme.active })
                    .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
            )
            .render(Rect::new(x, area.y, width, 1), buf);
        layout.hits.push(HitRegion {
            rect: Rect::new(x, area.y, width, 1),
            target: HitTarget::SurfaceTab(pane_id, index),
        });
        x = x.saturating_add(width);
    }
}

fn surface_pane_tab_title(app: &App, tab: &SurfaceTab) -> String {
    match tab {
        SurfaceTab::AgentBoard => "Agent board".to_owned(),
        SurfaceTab::SessionMonitor(key) => match &key.target {
            SessionMonitorTarget::Runtime { address, .. } => format!(
                "Monitor #{}:{}",
                address.instance_id,
                address.generation,
            ),
            SessionMonitorTarget::Managed { record_id, .. } => format!("Monitor @{record_id}"),
        },
        SurfaceTab::File(key) => repository_path_file_name_display(&key.path),
        SurfaceTab::Git(key) => git_surface_title(app, key),
        SurfaceTab::Preview(key) => app
            .preview_tabs
            .get(key)
            .map(|preview| preview.title.clone())
            .unwrap_or_else(|| "detached session".to_owned()),
        SurfaceTab::Pty(_) => app.surface_tab_title(tab),
    }
}

fn git_surface_title(app: &App, key: &crate::app::WorkspaceGitTabKey) -> String {
    let Some(tab) = app.git_tabs.get(key) else {
        return format!("{} commits", key.workspace_id);
    };
    let target = tab
        .diff
        .as_ref()
        .map(|diff| &diff.target)
        .or(tab.pending_diff.as_ref());
    if let Some(target) = target {
        use crate::app::WorkspaceGitDiffTarget;
        let path_name = |path: &Option<gate4agent_node_protocol::RepositoryPath>| {
            path.as_ref()
                .map(repository_path_file_name_display)
                .unwrap_or_else(|| key.workspace_id.clone())
        };
        return match target {
            WorkspaceGitDiffTarget::Working { path } => {
                format!("{} changes", path_name(path))
            }
            WorkspaceGitDiffTarget::Staged { path } => {
                format!("{} staged", path_name(path))
            }
            WorkspaceGitDiffTarget::Commit { revision, path } => format!(
                "{} @ {}",
                path_name(path),
                &revision[..revision.len().min(8)],
            ),
        };
    }
    tab.history_path
        .as_ref()
        .map(|path| format!("{} commits", repository_path_file_name_display(path)))
        .unwrap_or_else(|| format!("{} commits", key.workspace_id))
}

fn render_surface_toolbar(
    app: &App,
    pane_id: PaneId,
    tab: Option<&SurfaceTab>,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    fill_rect(area, theme.active, buf);
    let button_style = Style::default().fg(theme.teal).bg(theme.active);
    let selected_tab_style = Style::default().fg(theme.text).bg(theme.panel).add_modifier(Modifier::BOLD);
    let detail_style = Style::default().fg(theme.muted).bg(theme.active);
    let mut x = area.x;
    match tab {
        Some(SurfaceTab::AgentBoard) => {
            if app.agent_board_mode == crate::app::AgentBoardMode::HarnessKanban {
                return;
            }
            if app.harness_kanban.enabled {
                x = render_toolbar_segment(
                    "[Harness tasks]",
                    x,
                    area,
                    button_style,
                    Some(HitTarget::HarnessBoardMode(crate::app::AgentBoardMode::HarnessKanban)),
                    buf,
                    layout,
                );
                x = render_toolbar_segment(
                    "[Runtime sessions]",
                    x.saturating_add(1),
                    area,
                    selected_tab_style,
                    Some(HitTarget::HarnessBoardMode(crate::app::AgentBoardMode::SessionMonitoring)),
                    buf,
                    layout,
                );
                x = render_toolbar_segment(
                    if app.agent_board.task_filter.is_some() { "[All sessions]" } else { "[Task filter]" },
                    x.saturating_add(1),
                    area,
                    button_style,
                    Some(HitTarget::AgentBoardTaskFilter),
                    buf,
                    layout,
                );
            } else {
                x = render_toolbar_segment(
                    if app.agent_board.task_filter.is_some() { "[All sessions]" } else { "[Task filter]" },
                    x,
                    area,
                    button_style,
                    Some(HitTarget::AgentBoardTaskFilter),
                    buf,
                    layout,
                );
            }
            let _ = render_toolbar_segment(
                " Agent board | observed runtime states | t task filter | arrows navigate | Enter open ",
                x.saturating_add(1),
                area,
                detail_style,
                None,
                buf,
                layout,
            );
        }
        Some(SurfaceTab::SessionMonitor(key)) => {
            render_session_monitor_toolbar(app, pane_id, key, area, buf, layout, theme);
        }
        Some(SurfaceTab::File(key)) => {
            let file = app.file_tabs.get(key);
            let ready = file.is_some_and(|file| file.state == WorkspaceFileState::Ready);
            if ready {
                if let Some(history) = file.and_then(|file| file.inline_history.as_ref()) {
                    x = render_toolbar_segment(
                        "[Source]",
                        x,
                        area,
                        button_style,
                        Some(HitTarget::FileSource(pane_id)),
                        buf,
                        layout,
                    );
                    if history.mode == WorkspaceGitPaneMode::Detail {
                        x = render_toolbar_segment(
                            "[Commits]",
                            x.saturating_add(1),
                            area,
                            button_style,
                            Some(HitTarget::FileHistoryBack(pane_id)),
                            buf,
                            layout,
                        );
                        if history.selected > 0 {
                            x = render_toolbar_segment(
                                "<",
                                x.saturating_add(1),
                                area,
                                button_style,
                                Some(HitTarget::FileHistoryPrevious(pane_id)),
                                buf,
                                layout,
                            );
                        }
                        if history.selected.saturating_add(1) < history.commits.len() {
                            x = render_toolbar_segment(
                                ">",
                                x.saturating_add(1),
                                area,
                                button_style,
                                Some(HitTarget::FileHistoryNext(pane_id)),
                                buf,
                                layout,
                            );
                        }
                        let position = format!(
                            " commit {}/{} ",
                            history.selected.saturating_add(1).min(history.commits.len()),
                            history.commits.len(),
                        );
                        let _ = render_toolbar_segment(
                            &position,
                            x.saturating_add(1),
                            area,
                            detail_style,
                            None,
                            buf,
                            layout,
                        );
                    } else {
                        let _ = render_toolbar_segment(
                            " commits | Up/Down select | Enter diff ",
                            x.saturating_add(1),
                            area,
                            detail_style,
                            None,
                            buf,
                            layout,
                        );
                    }
                    return;
                }
                x = render_toolbar_segment(
                    "[History]",
                    x,
                    area,
                    button_style,
                    Some(HitTarget::FileHistory(pane_id)),
                    buf,
                    layout,
                );
                x = render_toolbar_segment(
                    "[Changes]",
                    x.saturating_add(1),
                    area,
                    button_style,
                    Some(HitTarget::FileChanges(pane_id)),
                    buf,
                    layout,
                );
                let edit_label = if app.file_tabs.get(key).is_some_and(|file| file.edit_mode) {
                    "[View]"
                } else {
                    "[Edit]"
                };
                x = render_toolbar_segment(
                    edit_label,
                    x.saturating_add(1),
                    area,
                    button_style,
                    Some(HitTarget::FileEdit(pane_id)),
                    buf,
                    layout,
                );
                let _ = render_toolbar_segment(
                    "[Save]",
                    x.saturating_add(1),
                    area,
                    button_style,
                    Some(HitTarget::FileSave(pane_id)),
                    buf,
                    layout,
                );
            }
        }
        Some(SurfaceTab::Git(key)) => {
            let git = app.git_tabs.get(key);
            let mode = git.and_then(|git| git.diff.as_ref()).map_or(
                "commits",
                |diff| match &diff.target {
                    crate::app::WorkspaceGitDiffTarget::Working { .. } => "working changes",
                    crate::app::WorkspaceGitDiffTarget::Staged { .. } => "staged changes",
                    crate::app::WorkspaceGitDiffTarget::Commit { .. } => "commit diff",
                },
            );
            let _ = render_toolbar_segment(
                &format!(" {mode} | Enter diff | w working | s staged "),
                x,
                area,
                detail_style,
                None,
                buf,
                layout,
            );
        }
        Some(SurfaceTab::Pty(_)) => {
            let _ = render_toolbar_segment(
                " live PTY | drag select | wheel scrollback ",
                x,
                area,
                detail_style,
                None,
                buf,
                layout,
            );
        }
        Some(SurfaceTab::Preview(key)) => {
            let preview = app.preview_tabs.get(key);
            let skin = preview
                .map(|preview| ProviderPreviewSkin::for_provider(preview.provider.as_str()))
                .unwrap_or(ProviderPreviewSkin::Generic);
            let _ = render_toolbar_segment(
                &format!(" {} | read-only transcript | no input ", skin.name()),
                x,
                area,
                Style::default().fg(skin.accent(theme)).bg(theme.active),
                None,
                buf,
                layout,
            );
        }
        None => {}
    }
}

fn render_toolbar_segment(
    label: &str,
    x: u16,
    area: Rect,
    style: Style,
    target: Option<HitTarget>,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
) -> u16 {
    if x >= area.right() {
        return x;
    }
    let width = (cell_width(label) as u16).min(area.right().saturating_sub(x));
    if width == 0 {
        return x;
    }
    let rect = Rect::new(x, area.y, width, 1);
    Paragraph::new(truncate_cells(label, width as usize))
        .style(style)
        .render(rect, buf);
    if let Some(target) = target {
        layout.hits.push(HitRegion { rect, target });
    }
    x.saturating_add(width)
}

fn render_workspace_file_tab(
    tab: &WorkspaceFileTabView,
    _path: &str,
    area: Rect,
    buf: &mut TerminalBuffer,
    theme: Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    fill_rect(area, theme.surface, buf);
    match &tab.state {
        WorkspaceFileState::Loading => {
            Paragraph::new(" loading file...")
                .style(Style::default().fg(theme.muted).bg(theme.surface))
                .render(area, buf);
        }
        WorkspaceFileState::NonUtf8 { byte_len } => {
            Paragraph::new(format!(" binary file ({byte_len} bytes); text editor unavailable"))
                .style(Style::default().fg(theme.yellow).bg(theme.surface))
                .render(area, buf);
        }
        WorkspaceFileState::TooLarge { limit_bytes } => {
            Paragraph::new(format!(" file exceeds the {limit_bytes}-byte editor limit"))
                .style(Style::default().fg(theme.yellow).bg(theme.surface))
                .render(area, buf);
        }
        WorkspaceFileState::Error(message) => {
            Paragraph::new(format!(" × {message}"))
                .style(Style::default().fg(theme.red).bg(theme.surface))
                .render(area, buf);
        }
        WorkspaceFileState::Ready => {
            let status_height = usize::from(area.height > 1);
            let rows = area.height as usize - status_height;
            let number_width = tab.editor.line_count().max(1).to_string().len().max(3);
            let content_width = (area.width as usize).saturating_sub(number_width + 3);
            let lines = tab.editor.visible_lines(rows, content_width);
            let first_line = tab.editor.scroll_line();
            for (row, line) in lines.iter().enumerate() {
                let y = area.y + row as u16;
                let number = format!("{:>width$} ", first_line + row + 1, width = number_width);
                let text = format!(
                    "{}{}{}",
                    if line.clipped_left { "‹" } else { "" },
                    line.text,
                    if line.clipped_right { "›" } else { "" },
                );
                Paragraph::new(Text::from_lines(vec![Line::from_spans(vec![
                    Span::styled(number, Style::default().fg(theme.muted)),
                    Span::styled(text, Style::default().fg(theme.text)),
                ])]))
                .style(Style::default().bg(theme.surface))
                .render(Rect::new(area.x, y, area.width, 1), buf);
            }
            if status_height > 0 {
                let status = format!(
                    " {} | {} | {} bytes | {}",
                    if tab.edit_mode { "EDIT" } else { "VIEW" },
                    if tab.editor.dirty() { "modified" } else { "saved" },
                    tab.editor.byte_len(),
                    match tab.editor.sync_state() {
                        crate::text_editor::SyncState::Clean => "e edit",
                        crate::text_editor::SyncState::Dirty => "Ctrl+S save",
                        crate::text_editor::SyncState::Saving => "saving...",
                        crate::text_editor::SyncState::Conflict(_) => "save conflict",
                        crate::text_editor::SyncState::Error(_) => "save failed",
                    },
                );
                Paragraph::new(truncate_cells(&status, area.width as usize))
                    .style(Style::default().fg(theme.teal).bg(theme.active))
                    .render(Rect::new(area.x, area.bottom() - 1, area.width, 1), buf);
            }
        }
    }
}

fn render_workspace_file_tab_rich(
    tab: &WorkspaceFileTabView,
    path: &str,
    area: Rect,
    buf: &mut TerminalBuffer,
    theme: Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    fill_rect(area, theme.surface, buf);
    match &tab.state {
        WorkspaceFileState::Loading => {
            Paragraph::new(" loading file...")
                .style(Style::default().fg(theme.muted).bg(theme.surface))
                .render(area, buf);
        }
        WorkspaceFileState::NonUtf8 { byte_len } => {
            Paragraph::new(format!(" binary file ({byte_len} bytes); text editor unavailable"))
                .style(Style::default().fg(theme.yellow).bg(theme.surface))
                .render(area, buf);
        }
        WorkspaceFileState::TooLarge { limit_bytes } => {
            Paragraph::new(format!(" file exceeds the {limit_bytes}-byte editor limit"))
                .style(Style::default().fg(theme.yellow).bg(theme.surface))
                .render(area, buf);
        }
        WorkspaceFileState::Error(message) => {
            Paragraph::new(format!(" x {message}"))
                .style(Style::default().fg(theme.red).bg(theme.surface))
                .render(area, buf);
        }
        WorkspaceFileState::Ready => {
            let status_height = usize::from(area.height > 1);
            let rows = area.height as usize - status_height;
            let number_width = tab.editor.line_count().max(1).to_string().len().max(3);
            let content_width = (area.width as usize).saturating_sub(number_width + 3);
            let lines = tab.editor.visible_lines(rows, content_width);
            let first_line = tab.editor.scroll_line();
            let selection = tab.editor.selection_range();
            let syntax_language = crate::text_editor::syntax_language_for_path(path);
            for (row, line) in lines.iter().enumerate() {
                let y = area.y + row as u16;
                let logical_line = first_line + row;
                let Some(source) = tab
                    .editor
                    .render_line_slice(logical_line, 0, usize::MAX)
                else {
                    continue;
                };
                let syntax = crate::text_editor::syntax_spans_for_line(syntax_language, source.text);
                let visible_start_byte = byte_offset_for_scalar_column(
                    source.text,
                    tab.editor.scroll_column(),
                );
                let line_start_byte = tab.editor.line_byte_start(logical_line).unwrap_or_default();
                let mut syntax_index = 0;
                let mut spans = vec![Span::styled(
                    format!("{:>width$} ", logical_line + 1, width = number_width),
                    Style::default().fg(theme.muted),
                )];
                if line.clipped_left {
                    spans.push(Span::styled("<", Style::default().fg(theme.muted)));
                }
                for (column_offset, (offset, character)) in line.text.char_indices().enumerate() {
                    let column = tab.editor.scroll_column().saturating_add(column_offset);
                    let byte_offset = line_start_byte
                        .saturating_add(visible_start_byte)
                        .saturating_add(offset);
                    let style = if file_byte_is_selected(selection.as_ref(), byte_offset) {
                        Style::default()
                            .fg(theme.active_tab_text)
                            .bg(theme.accent)
                    } else {
                        syntax_style_for_column(&syntax, &mut syntax_index, column, theme)
                    };
                    spans.push(Span::styled(character.to_string(), style));
                }
                if line.clipped_right {
                    spans.push(Span::styled(">", Style::default().fg(theme.muted)));
                }
                Paragraph::new(Text::from_lines(vec![Line::from_spans(spans)]))
                    .style(Style::default().bg(theme.surface))
                    .render(Rect::new(area.x, y, area.width, 1), buf);
            }
            if status_height > 0 {
                let sync = match tab.editor.sync_state() {
                    crate::text_editor::SyncState::Clean => "saved",
                    crate::text_editor::SyncState::Dirty => "modified",
                    crate::text_editor::SyncState::Saving => "saving...",
                    crate::text_editor::SyncState::Conflict(_) => "save conflict",
                    crate::text_editor::SyncState::Error(_) => "save failed",
                };
                let status = format!(
                    " {} | {} | {} bytes | click/drag select | Ctrl+A/C/X/V",
                    if tab.edit_mode { "EDIT" } else { "VIEW" },
                    sync,
                    tab.editor.byte_len(),
                );
                Paragraph::new(truncate_cells(&status, area.width as usize))
                    .style(Style::default().fg(theme.teal).bg(theme.active))
                    .render(Rect::new(area.x, area.bottom() - 1, area.width, 1), buf);
            }
        }
    }
}

fn render_file_scrollbar(
    tab: &WorkspaceFileTabView,
    pane_id: PaneId,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let track_height = area.height.saturating_sub(1);
    if area.width <= 2 || track_height == 0 {
        return;
    }
    let track = Rect::new(area.right().saturating_sub(1), area.y, 1, track_height);
    for y in track.y..track.bottom() {
        let cell = buf.get_mut(track.x, y);
        cell.symbol = "|".into();
        cell.style = Style::default().fg(theme.border).bg(theme.surface);
    }
    let line_count = tab.editor.line_count().max(1);
    let visible_rows = track_height as usize;
    let thumb_height = if line_count <= visible_rows {
        track_height
    } else {
        ((visible_rows.saturating_mul(visible_rows) + line_count - 1) / line_count)
            .max(1)
            .min(visible_rows) as u16
    };
    let travel = track_height.saturating_sub(thumb_height);
    let last_line = line_count.saturating_sub(1);
    let thumb_offset = if last_line == 0 {
        0
    } else {
        (tab.editor.scroll_line().min(last_line).saturating_mul(travel as usize) / last_line)
            as u16
    };
    for y in track.y.saturating_add(thumb_offset)
        ..track.y.saturating_add(thumb_offset).saturating_add(thumb_height)
    {
        let cell = buf.get_mut(track.x, y);
        cell.symbol = "#".into();
        cell.style = Style::default().fg(theme.accent).bg(theme.surface);
    }
    layout.hits.push(HitRegion {
        rect: track,
        target: HitTarget::FileScrollbar(pane_id),
    });
}

fn byte_offset_for_scalar_column(text: &str, column: usize) -> usize {
    text.char_indices()
        .nth(column)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len())
}

fn file_byte_is_selected(selection: Option<&std::ops::Range<usize>>, byte_offset: usize) -> bool {
    selection.is_some_and(|selection| selection.contains(&byte_offset))
}

fn syntax_style_for_column(
    spans: &[crate::text_editor::SyntaxSpan],
    syntax_index: &mut usize,
    column: usize,
    theme: Theme,
) -> Style {
    use crate::text_editor::SyntaxClass;

    while spans
        .get(*syntax_index)
        .is_some_and(|span| span.end_column <= column)
    {
        *syntax_index += 1;
    }
    let class = spans
        .get(*syntax_index)
        .filter(|span| span.start_column <= column && column < span.end_column)
        .map(|span| span.class)
        .unwrap_or(SyntaxClass::Plain);
    match class {
        SyntaxClass::Plain => Style::default().fg(theme.text),
        SyntaxClass::Keyword => Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        SyntaxClass::String => Style::default().fg(theme.green),
        SyntaxClass::Number | SyntaxClass::Boolean => Style::default().fg(theme.yellow),
        SyntaxClass::Comment => Style::default().fg(theme.muted),
        SyntaxClass::Heading => Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        SyntaxClass::Emphasis => Style::default().fg(theme.teal),
        SyntaxClass::Key => Style::default().fg(theme.teal).add_modifier(Modifier::BOLD),
        SyntaxClass::Punctuation => Style::default().fg(theme.dim),
    }
}

fn render_workspace_file_history(
    tab: &WorkspaceGitTabView,
    pane_id: PaneId,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
    spinner: char,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    fill_rect(area, theme.surface, buf);
    match &tab.state {
        WorkspaceGitState::Loading => {
            Paragraph::new(format!(" {spinner} loading file history..."))
                .style(Style::default().fg(theme.muted).bg(theme.surface))
                .render(area, buf);
            return;
        }
        WorkspaceGitState::Error(message) => {
            Paragraph::new(format!(" x {message}"))
                .style(Style::default().fg(theme.red).bg(theme.surface))
                .render(area, buf);
            return;
        }
        WorkspaceGitState::Ready => {}
    }

    if tab.mode == WorkspaceGitPaneMode::List {
        let footer_height = usize::from(area.height > 1);
        let capacity = area.height as usize - footer_height;
        let start = tab.list_scroll.min(tab.commits.len().saturating_sub(capacity));
        for (visible, (index, commit)) in tab
            .commits
            .iter()
            .enumerate()
            .skip(start)
            .take(capacity)
            .enumerate()
        {
            let short_id = &commit.id[..commit.id.len().min(8)];
            let label = format!(" {short_id} {}", commit.subject);
            let row = Rect::new(area.x, area.y + visible as u16, area.width, 1);
            Paragraph::new(truncate_cells(&label, area.width as usize))
                .style(
                    Style::default()
                        .fg(theme.text)
                        .bg(if index == tab.selected { theme.active } else { theme.surface }),
                )
                .render(row, buf);
            layout.hits.push(HitRegion {
                rect: row,
                target: HitTarget::FileHistoryCommit(pane_id, index),
            });
        }
        if tab.commits.is_empty() && capacity > 0 {
            Paragraph::new(" no commits found for this file")
                .style(Style::default().fg(theme.muted).bg(theme.surface))
                .render(Rect::new(area.x, area.y, area.width, 1), buf);
        }
        if footer_height > 0 {
            let footer = if tab.has_more {
                " Up/Down select | Enter open | l older "
            } else {
                " Up/Down select | Enter open "
            };
            Paragraph::new(truncate_cells(footer, area.width as usize))
                .style(Style::default().fg(theme.teal).bg(theme.active))
                .render(Rect::new(area.x, area.bottom() - 1, area.width, 1), buf);
        }
        return;
    }

    let lines = git_detail_lines(tab, spinner);
    let maximum_scroll = lines.len().saturating_sub(area.height as usize);
    let detail_scroll = tab.detail_scroll.min(maximum_scroll);
    for (row, line) in lines
        .iter()
        .skip(detail_scroll)
        .take(area.height as usize)
        .enumerate()
    {
        let style = git_detail_line_style(line.kind, theme);
        let row_area = Rect::new(area.x, area.y + row as u16, area.width, 1);
        fill_rect(row_area, style.bg, buf);
        Paragraph::new(truncate_cells(&line.text, row_area.width as usize))
            .style(style)
            .render(row_area, buf);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitDetailLineKind {
    Metadata,
    Summary,
    Added,
    Deleted,
    Hunk,
    DiffMeta,
    Context,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitDetailLine {
    text: String,
    kind: GitDetailLineKind,
}

fn git_detail_lines(tab: &WorkspaceGitTabView, spinner: char) -> Vec<GitDetailLine> {
    let mut lines = Vec::new();
    if let Some(commit) = tab.commits.get(tab.selected) {
        for text in [
            format!("Commit: {}", commit.id),
            format!("Message: {}", commit.subject),
        ] {
            lines.push(GitDetailLine { text, kind: GitDetailLineKind::Metadata });
        }
        if !commit.parents.is_empty() {
            lines.push(GitDetailLine {
                text: format!(
                    "Parents: {}",
                    commit
                        .parents
                        .iter()
                        .map(|parent| &parent[..parent.len().min(12)])
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                kind: GitDetailLineKind::Metadata,
            });
        }
        for text in [
            format!("Author: {} <{}>", commit.author_name, commit.author_email),
            format!("Author date: {}", commit.authored_at),
            format!("Committer: {} <{}>", commit.committer_name, commit.committer_email),
            format!("Commit date: {}", commit.committed_at),
            format!(
                "Signature: {}{}",
                commit.signature_status,
                commit
                    .signer
                    .as_ref()
                    .map_or(String::new(), |signer| format!(" ({signer})")),
            ),
        ] {
            lines.push(GitDetailLine { text, kind: GitDetailLineKind::Metadata });
        }
        lines.push(GitDetailLine { text: String::new(), kind: GitDetailLineKind::Context });
    }
    if let Some(error) = tab.diff_error.as_ref() {
        lines.push(GitDetailLine {
            text: format!("x {error}"),
            kind: GitDetailLineKind::Error,
        });
    } else if let Some(diff) = tab.diff.as_ref() {
        let stats = git_diff_stats(&diff.text);
        lines.push(GitDetailLine {
            text: format!(
                "Summary: {} file{} | +{} -{} | {} bytes{}",
                stats.files,
                if stats.files == 1 { "" } else { "s" },
                stats.added,
                stats.deleted,
                diff.byte_len,
                if diff.truncated { " | truncated" } else { "" },
            ),
            kind: GitDetailLineKind::Summary,
        });
        lines.push(GitDetailLine {
            text: format!("Diff: {}", git_diff_target_label(&diff.target)),
            kind: GitDetailLineKind::DiffMeta,
        });
        lines.extend(diff.text.lines().map(|text| GitDetailLine {
            text: text.to_owned(),
            kind: git_diff_line_kind(text),
        }));
    } else if let Some(target) = tab.pending_diff.as_ref() {
        lines.push(GitDetailLine {
            text: format!("{spinner} loading diff {}...", git_diff_target_label(target)),
            kind: GitDetailLineKind::Metadata,
        });
    }
    lines
}

fn git_diff_line_kind(line: &str) -> GitDetailLineKind {
    if line.starts_with("+++") || line.starts_with("---") || is_git_diff_meta(line) {
        GitDetailLineKind::DiffMeta
    } else if line.starts_with('+') {
        GitDetailLineKind::Added
    } else if line.starts_with('-') {
        GitDetailLineKind::Deleted
    } else if line.starts_with("@@") {
        GitDetailLineKind::Hunk
    } else {
        GitDetailLineKind::Context
    }
}

fn is_git_diff_meta(line: &str) -> bool {
    [
        "diff --git ",
        "index ",
        "new file mode ",
        "deleted file mode ",
        "old mode ",
        "new mode ",
        "similarity index ",
        "dissimilarity index ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
        "Binary files ",
        "\\ No newline at end of file",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn git_detail_line_style(kind: GitDetailLineKind, theme: Theme) -> Style {
    match kind {
        GitDetailLineKind::Metadata => Style::default().fg(theme.muted).bg(theme.surface),
        GitDetailLineKind::Summary => Style::default()
            .fg(theme.teal)
            .bg(theme.active)
            .add_modifier(Modifier::BOLD),
        GitDetailLineKind::Added => Style::default().fg(theme.green).bg(theme.diff_added),
        GitDetailLineKind::Deleted => Style::default().fg(theme.red).bg(theme.diff_deleted),
        GitDetailLineKind::Hunk => Style::default()
            .fg(theme.teal)
            .bg(theme.diff_hunk)
            .add_modifier(Modifier::BOLD),
        GitDetailLineKind::DiffMeta => Style::default().fg(theme.muted).bg(theme.diff_meta),
        GitDetailLineKind::Context => Style::default().fg(theme.text).bg(theme.surface),
        GitDetailLineKind::Error => Style::default().fg(theme.red).bg(theme.surface),
    }
}

fn render_workspace_git_tab(
    tab: &WorkspaceGitTabView,
    pane_id: PaneId,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
    spinner: char,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    fill_rect(area, theme.surface, buf);
    match &tab.state {
        WorkspaceGitState::Loading => {
            Paragraph::new(format!(" {spinner} loading Git history..."))
                .style(Style::default().fg(theme.muted).bg(theme.surface))
                .render(area, buf);
            return;
        }
        WorkspaceGitState::Error(message) => {
            Paragraph::new(format!(" x {message}"))
                .style(Style::default().fg(theme.red).bg(theme.surface))
                .render(area, buf);
            return;
        }
        WorkspaceGitState::Ready => {}
    }
    if tab.mode == WorkspaceGitPaneMode::List {
        let capacity = area.height.saturating_sub(1) as usize;
        let start = tab.list_scroll.min(tab.commits.len().saturating_sub(capacity));
        for (visible, (index, commit)) in tab
            .commits
            .iter()
            .enumerate()
            .skip(start)
            .take(capacity)
            .enumerate()
        {
            let label = format!(" {} {}", &commit.id[..commit.id.len().min(8)], commit.subject);
            let row = Rect::new(area.x, area.y + visible as u16, area.width, 1);
            Paragraph::new(truncate_cells(&label, area.width as usize))
                .style(Style::default().fg(theme.text).bg(if index == tab.selected { theme.active } else { theme.surface }))
                .render(row, buf);
            layout.hits.push(HitRegion {
                rect: row,
                target: HitTarget::GitCommit(pane_id, index),
            });
        }
        let footer = if tab.has_more {
            " Up/Down select | Enter open | w working | s staged | l older "
        } else {
            " Up/Down select | Enter open | w working | s staged "
        };
        Paragraph::new(truncate_cells(footer, area.width as usize))
            .style(Style::default().fg(theme.teal).bg(theme.active))
            .render(Rect::new(area.x, area.bottom() - 1, area.width, 1), buf);
        return;
    }

    let detail_height = area.height.saturating_sub(1);
    let details = Rect::new(area.x, area.y, area.width, detail_height);
    let lines = git_detail_lines(tab, spinner);
    let maximum_scroll = lines.len().saturating_sub(details.height as usize);
    let detail_scroll = tab.detail_scroll.min(maximum_scroll);
    for (row, line) in lines.iter().skip(detail_scroll).take(details.height as usize).enumerate() {
        let row_area = Rect::new(details.x, details.y + row as u16, details.width, 1);
        let style = git_detail_line_style(line.kind, theme);
        fill_rect(row_area, style.bg, buf);
        Paragraph::new(truncate_cells(&line.text, details.width as usize))
            .style(style)
            .render(row_area, buf);
    }
    if area.height > 0 {
        let footer = " [Back] Esc/Backspace | Up/Down scroll | Left/Right commit | PgUp/PgDn ";
        let footer_area = Rect::new(area.x, area.bottom() - 1, area.width, 1);
        Paragraph::new(truncate_cells(footer, area.width as usize))
            .style(Style::default().fg(theme.teal).bg(theme.active))
            .render(footer_area, buf);
        let back_width = 7.min(area.width);
        layout.hits.push(HitRegion {
            rect: Rect::new(area.x + 1.min(area.width.saturating_sub(1)), footer_area.y, back_width, 1),
            target: HitTarget::GitBack(pane_id),
        });
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct GitDiffStats {
    files: usize,
    added: usize,
    deleted: usize,
}

fn git_diff_stats(text: &str) -> GitDiffStats {
    let mut stats = GitDiffStats::default();
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            stats.files = stats.files.saturating_add(1);
        } else if line.starts_with('+') && !line.starts_with("+++") {
            stats.added = stats.added.saturating_add(1);
        } else if line.starts_with('-') && !line.starts_with("---") {
            stats.deleted = stats.deleted.saturating_add(1);
        }
    }
    stats
}

fn git_diff_target_label(target: &crate::app::WorkspaceGitDiffTarget) -> String {
    use crate::app::WorkspaceGitDiffTarget;

    let path_suffix = |path: &Option<gate4agent_node_protocol::RepositoryPath>| {
        path.as_ref()
            .map(|path| format!(" | {}", path.display_text()))
            .unwrap_or_default()
    };
    match target {
        WorkspaceGitDiffTarget::Working { path } => {
            format!("working tree{}", path_suffix(path))
        }
        WorkspaceGitDiffTarget::Staged { path } => format!("staged{}", path_suffix(path)),
        WorkspaceGitDiffTarget::Commit { revision, path } => format!(
            "commit {}{}",
            &revision[..revision.len().min(12)],
            path_suffix(path),
        ),
    }
}

fn render_preview_tab(
    tab: &PreviewTabView,
    pane_id: PaneId,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
    activity_spinner: char,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let skin = ProviderPreviewSkin::for_provider(tab.provider.as_str());
    fill_rect(area, theme.surface, buf);
    let lines = native_agent_chat_lines(tab, area.width as usize, activity_spinner);
    let resume_available = preview_resume_available(tab);
    let action_height = usize::from(resume_available && area.height > 1);
    let summary_height = usize::from(area.height as usize > action_height);
    let capacity = area.height as usize - action_height - summary_height;
    let maximum_start = lines.len().saturating_sub(capacity);
    let start = if tab.scroll > maximum_start {
        maximum_start.saturating_sub(usize::MAX.saturating_sub(tab.scroll))
    } else {
        tab.scroll
    };
    if summary_height > 0 {
        let visible_end = start.saturating_add(capacity).min(lines.len());
        let position = if lines.is_empty() || capacity == 0 {
            String::new()
        } else {
            format!(" | lines {}-{}/{}", start.saturating_add(1), visible_end, lines.len())
        };
        let summary = format!(" read-only | {}{}", preview_history_summary(tab), position);
        Paragraph::new(truncate_cells(&summary, area.width as usize))
            .style(Style::default().fg(theme.muted).bg(theme.active))
            .render(Rect::new(area.x, area.y, area.width, 1), buf);
    }
    for (row, line) in lines.iter().skip(start).take(capacity).enumerate() {
        let rect = Rect::new(
            area.x,
            area.y + summary_height as u16 + row as u16,
            area.width,
            1,
        );
        let (marker_style, text_style) = match line.kind {
            NativeAgentChatLineKind::User => (
                Style::default().fg(theme.teal).bg(theme.surface).add_modifier(Modifier::BOLD),
                Style::default().fg(theme.text).bg(theme.surface),
            ),
            NativeAgentChatLineKind::Assistant => (
                Style::default().fg(skin.accent(theme)).bg(theme.surface).add_modifier(Modifier::BOLD),
                Style::default().fg(theme.text).bg(theme.surface),
            ),
            NativeAgentChatLineKind::Error => (
                Style::default().fg(theme.red).bg(theme.surface).add_modifier(Modifier::BOLD),
                Style::default().fg(theme.red).bg(theme.surface),
            ),
            NativeAgentChatLineKind::Status => (
                Style::default().fg(skin.accent(theme)).bg(theme.surface),
                Style::default().fg(theme.dim).bg(theme.surface),
            ),
            NativeAgentChatLineKind::Empty => (
                Style::default().fg(theme.muted).bg(theme.surface),
                Style::default().fg(theme.muted).bg(theme.surface),
            ),
            NativeAgentChatLineKind::Gap => (
                Style::default().bg(theme.surface),
                Style::default().bg(theme.surface),
            ),
        };
        let marker_width = cell_width(&line.marker);
        let text_width = (area.width as usize).saturating_sub(marker_width);
        Paragraph::new(Text::from_lines(vec![Line::from_spans(vec![
            Span::styled(line.marker.clone(), marker_style),
            Span::styled(truncate_cells(&line.text, text_width), text_style),
        ])]))
        .style(Style::default().bg(theme.surface))
        .render(rect, buf);
    }
    if action_height > 0 {
        let label = if area.width as usize >= cell_width("[Resume session]") {
            "[Resume session]"
        } else {
            "[Resume]"
        };
        let width = (cell_width(label) as u16).min(area.width);
        let footer = Rect::new(
            area.x,
            area.bottom().saturating_sub(1),
            area.width,
            1,
        );
        fill_rect(footer, theme.active, buf);
        let hint_width = area.width.saturating_sub(width);
        if hint_width > 0 {
            Paragraph::new(truncate_cells(
                " read-only transcript | Up/Down scroll | Home/End ",
                hint_width as usize,
            ))
            .style(Style::default().fg(theme.muted).bg(theme.active))
            .render(Rect::new(area.x, footer.y, hint_width, 1), buf);
        }
        let rect = Rect::new(
            area.right().saturating_sub(width),
            area.bottom().saturating_sub(1),
            width,
            1,
        );
        Paragraph::new(label)
            .style(Style::default().fg(theme.teal).bg(theme.active).add_modifier(Modifier::BOLD))
            .render(rect, buf);
        layout.hits.push(HitRegion {
            rect,
            target: HitTarget::PreviewResume(pane_id),
        });
    }
}

fn preview_history_summary(tab: &PreviewTabView) -> String {
    match &tab.preview {
        NativeSessionPreviewState::Loading => "loading bounded history".to_owned(),
        NativeSessionPreviewState::Unavailable(_) => "history unavailable".to_owned(),
        NativeSessionPreviewState::Error(_) => "history failed".to_owned(),
        NativeSessionPreviewState::Ready(preview)
        | NativeSessionPreviewState::Empty(preview) => {
            let visible = preview.messages.len();
            let messages = if preview.message_count_exact {
                if preview.truncated || visible < preview.message_count as usize {
                    format!("latest {visible} of {} messages", preview.message_count)
                } else {
                    format!("{} messages", preview.message_count)
                }
            } else {
                format!("latest {visible} messages; total incomplete")
            };
            let turns = preview
                .completed_turn_count
                .map(|count| format!(" | {count} completed turns"))
                .unwrap_or_default();
            let tokens = preview
                .total_tokens
                .map(|count| count.to_string())
                .unwrap_or_else(|| "unknown".to_owned());
            format!("{messages}{turns} | total tokens {tokens}")
        }
    }
}

fn preview_resume_available(tab: &PreviewTabView) -> bool {
    tab.resume_available
        && provider_supports_native_resume(&tab.provider)
        && tab.phase == PreviewTabPhase::Hydrated
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeAgentChatLineKind {
    User,
    Assistant,
    Error,
    Status,
    Empty,
    Gap,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NativeAgentChatLine {
    marker: String,
    text: String,
    kind: NativeAgentChatLineKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderPreviewSkin {
    Claude,
    Codex,
    Kimi,
    Grok,
    Qwen,
    Generic,
}

impl ProviderPreviewSkin {
    fn for_provider(provider: &str) -> Self {
        match provider {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "kimi" => Self::Kimi,
            "grok" => Self::Grok,
            "qwen" | "qwen-code" => Self::Qwen,
            _ => Self::Generic,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE",
            Self::Codex => "CODEX",
            Self::Kimi => "KIMI",
            Self::Grok => "GROK",
            Self::Qwen => "QWEN",
            Self::Generic => "SESSION",
        }
    }

    fn assistant_label(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE",
            Self::Codex => "CODEX",
            Self::Kimi => "KIMI",
            Self::Grok => "GROK",
            Self::Qwen => "QWEN",
            Self::Generic => "AGENT",
        }
    }

    fn accent(self, theme: Theme) -> Color {
        match self {
            Self::Claude => theme.yellow,
            Self::Codex => theme.green,
            Self::Kimi => theme.teal,
            Self::Grok => theme.text,
            Self::Qwen => theme.accent,
            Self::Generic => theme.muted,
        }
    }

}

fn native_agent_chat_lines(
    tab: &PreviewTabView,
    width: usize,
    activity_spinner: char,
) -> Vec<NativeAgentChatLine> {
    let width = width.max(1);
    let mut lines = Vec::new();
    match &tab.preview {
        NativeSessionPreviewState::Loading => push_agent_status(
            &mut lines,
            &format!("{activity_spinner} Loading session…"),
            width,
        ),
        NativeSessionPreviewState::Unavailable(reason) => {
            push_agent_error(&mut lines, &format!("Session unavailable: {reason}"), width)
        }
        NativeSessionPreviewState::Error(message) => {
            push_agent_error(&mut lines, &format!("Session failed: {message}"), width)
        }
        NativeSessionPreviewState::Ready(preview)
        | NativeSessionPreviewState::Empty(preview) => {
            for message in &preview.messages {
                push_agent_turn(
                    &mut lines,
                    &message.role,
                    &message.text,
                    width,
                    ProviderPreviewSkin::for_provider(tab.provider.as_str()),
                );
            }
            if preview.messages.is_empty() {
                lines.push(NativeAgentChatLine {
                    marker: "  ".to_owned(),
                    text: "No conversation turns yet.".to_owned(),
                    kind: NativeAgentChatLineKind::Empty,
                });
            }
        }
    }
    match tab.phase {
        PreviewTabPhase::Hydrated => {}
        PreviewTabPhase::Indexing => push_agent_status(
            &mut lines,
            &format!("{activity_spinner} Linking session…"),
            width,
        ),
        PreviewTabPhase::Resuming => push_agent_status(
            &mut lines,
            &format!("{activity_spinner} Reconnecting session…"),
            width,
        ),
    }
    if let Some(message) = tab.reconnect_error.as_deref() {
        push_agent_error(&mut lines, message, width);
    }
    lines
}

fn push_agent_turn(
    lines: &mut Vec<NativeAgentChatLine>,
    role: &str,
    text: &str,
    width: usize,
    skin: ProviderPreviewSkin,
) {
    let normalized = role.trim().to_ascii_lowercase();
    if normalized == "user" {
        let marker = transcript_marker("YOU", width);
        let continuation = " ".repeat(cell_width(&marker));
        let wrapped = wrap_preview_text(text, width.saturating_sub(cell_width(&marker)).max(1));
        for (index, line) in wrapped.into_iter().enumerate() {
            lines.push(NativeAgentChatLine {
                marker: if index == 0 { marker.clone() } else { continuation.clone() },
                text: line,
                kind: NativeAgentChatLineKind::User,
            });
        }
    } else if normalized == "assistant" {
        let label = transcript_marker(skin.assistant_label(), width);
        let continuation = " ".repeat(cell_width(&label));
        let wrapped = wrap_preview_text(text, width.saturating_sub(cell_width(&label)).max(1));
        for (index, line) in wrapped.into_iter().enumerate() {
            lines.push(NativeAgentChatLine {
                marker: if index == 0 { label.clone() } else { continuation.clone() },
                text: line,
                kind: NativeAgentChatLineKind::Assistant,
            });
        }
    } else {
        return;
    }
    lines.push(NativeAgentChatLine {
        marker: String::new(),
        text: String::new(),
        kind: NativeAgentChatLineKind::Gap,
    });
}

fn transcript_marker(label: &str, width: usize) -> String {
    if width >= 12 {
        format!("{label:<9} ")
    } else {
        format!("{} ", label.chars().next().unwrap_or('?'))
    }
}

fn push_agent_error(lines: &mut Vec<NativeAgentChatLine>, text: &str, width: usize) {
    let marker = transcript_marker("ERROR", width);
    let continuation = " ".repeat(cell_width(&marker));
    for (index, line) in wrap_preview_text(
        text,
        width.saturating_sub(cell_width(&marker)).max(1),
    )
        .into_iter()
        .enumerate()
    {
        lines.push(NativeAgentChatLine {
            marker: if index == 0 { marker.clone() } else { continuation.clone() },
            text: line,
            kind: NativeAgentChatLineKind::Error,
        });
    }
}

fn push_agent_status(lines: &mut Vec<NativeAgentChatLine>, text: &str, width: usize) {
    if !lines.is_empty() && !matches!(lines.last().map(|line| line.kind), Some(NativeAgentChatLineKind::Gap)) {
        lines.push(NativeAgentChatLine {
            marker: String::new(),
            text: String::new(),
            kind: NativeAgentChatLineKind::Gap,
        });
    }
    lines.push(NativeAgentChatLine {
        marker: transcript_marker("STATUS", width),
        text: text.to_owned(),
        kind: NativeAgentChatLineKind::Status,
    });
}

fn split_surface_rect(area: Rect, axis: SplitAxis, ratio_bps: u16) -> (Rect, Rect, Rect) {
    let extent = match axis {
        SplitAxis::Horizontal => area.width,
        SplitAxis::Vertical => area.height,
    };
    if extent < 3 {
        return (area, Rect::default(), Rect::default());
    }
    let usable = extent - 1;
    let first_extent = ((u32::from(usable) * u32::from(ratio_bps.min(10_000))) / 10_000)
        .clamp(1, u32::from(usable - 1)) as u16;
    let second_extent = usable - first_extent;
    match axis {
        SplitAxis::Horizontal => (
            Rect::new(area.x, area.y, first_extent, area.height),
            Rect::new(area.x + first_extent, area.y, 1, area.height),
            Rect::new(area.x + first_extent + 1, area.y, second_extent, area.height),
        ),
        SplitAxis::Vertical => (
            Rect::new(area.x, area.y, area.width, first_extent),
            Rect::new(area.x, area.y + first_extent, area.width, 1),
            Rect::new(area.x, area.y + first_extent + 1, area.width, second_extent),
        ),
    }
}

fn draw_surface_divider(
    divider: Rect,
    axis: SplitAxis,
    buf: &mut TerminalBuffer,
    theme: Theme,
) {
    for row in divider.y..divider.bottom() {
        for column in divider.x..divider.right() {
            let cell = buf.get_mut(column, row);
            cell.symbol = match axis {
                SplitAxis::Horizontal => "|".into(),
                SplitAxis::Vertical => "-".into(),
            };
            cell.style = Style::default().fg(theme.border).bg(theme.surface);
        }
    }
}

fn render_terminal(
    session: &SessionView,
    area: Rect,
    color_mode: PtyColorMode,
    scroll_offset: usize,
    selection: Option<&crate::app::TerminalSelection>,
    theme: Theme,
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
    if let Some(selection) = selection.filter(|selection| selection.address == session.address) {
        let (start, end) = if (selection.start.1, selection.start.0)
            <= (selection.end.1, selection.end.0)
        {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
        for row in start.1..=end.1.min(area.height.saturating_sub(1)) {
            let from = if row == start.1 { start.0 } else { 0 };
            let to = if row == end.1 {
                end.0.saturating_add(1).min(area.width)
            } else {
                area.width
            };
            for column in from..to {
                let cell = buf.get_mut(area.x + column, area.y + row);
                cell.style = cell
                    .style
                    .fg(theme.active_tab_text)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD);
            }
        }
    }
}

fn render_existing_session(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let Some(session) = &app.existing_session else {
        return;
    };
    let width = 112.min(area.width);
    let height = 24.min(area.height);
    let dialog = positioned_modal(area, width, height, app.existing_session_modal_position);
    layout.existing_session_modal = dialog;
    fill_rect(dialog, theme.modal, buf);
    Block::bordered()
        .title(if session.mode == ExistingSessionMode::Catalog && session.preview.is_some() {
            " native session details "
        } else {
            " existing sessions "
        })
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.modal))
        .render(dialog, buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(dialog.x, dialog.y, dialog.width, 1),
        target: HitTarget::ExistingSessionDrag,
    });
    if dialog.width < 3 || dialog.height < 3 {
        return;
    }
    let inner = Rect::new(dialog.x + 1, dialog.y + 1, dialog.width - 2, dialog.height - 2);
    match session.mode {
        ExistingSessionMode::Catalog => render_existing_session_catalog(
            session, inner, theme, buf, layout,
        ),
        ExistingSessionMode::AdvancedImport => render_existing_session_advanced(
            app, session, inner, theme, buf, layout,
        ),
    }
}

fn render_existing_session_catalog(
    session: &crate::app::ExistingSessionDialog,
    area: Rect,
    theme: Theme,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
) {
    let buttons_row = area.height.saturating_sub(5);
    let message_height = usize::from(buttons_row.saturating_sub(6));
    let mut message_lines = Vec::new();
    match session.preview.as_ref() {
        Some(NativeSessionPreviewState::Ready(preview))
        | Some(NativeSessionPreviewState::Empty(preview)) => {
            let source = session.preview_record_id.as_deref().unwrap_or("native catalog");
            render_modal_line(
                format!("  Title       {}", preview.title.as_deref().unwrap_or(&session.display_name)),
                area, 0, Style::default().fg(theme.text).bg(theme.modal), buf,
            );
            render_modal_line(
                format!("  Source      {}", truncate_cells(source, area.width.saturating_sub(14) as usize)),
                area, 1, Style::default().fg(theme.dim).bg(theme.modal), buf,
            );
            render_modal_line(
                format!("  Modified    {}    Model  {}", preview.modified_at.as_deref().unwrap_or("-"), preview.model.as_deref().unwrap_or("-")),
                area, 2, Style::default().fg(theme.dim).bg(theme.modal), buf,
            );
            let message_summary = if preview.message_count_exact {
                if preview.truncated {
                    format!("last {} of {}", preview.messages.len(), preview.message_count)
                } else {
                    preview.message_count.to_string()
                }
            } else {
                format!("showing latest {}", preview.messages.len())
            };
            render_modal_line(
                format!("  Messages    {message_summary}    Completed turns  {}",
                    preview.completed_turn_count.map(|count| count.to_string()).unwrap_or_else(|| "-".to_owned())),
                area, 3, Style::default().fg(theme.dim).bg(theme.modal), buf,
            );
            for message in &preview.messages {
                message_lines.push((format!("{}:", message.role.to_uppercase()), true));
                for line in wrap_preview_text(&message.text, area.width.saturating_sub(4) as usize) {
                    message_lines.push((format!("  {line}"), false));
                }
            }
            if preview.messages.is_empty() {
                message_lines.push(("No user/assistant messages are available in this bounded preview.".to_owned(), false));
            }
        }
        Some(NativeSessionPreviewState::Loading) => {
            render_modal_line("  Loading bounded native session preview...", area, 2,
                Style::default().fg(theme.yellow).bg(theme.modal), buf);
        }
        Some(NativeSessionPreviewState::Unavailable(reason)) => {
            render_modal_line(format!("  Preview unavailable: {reason}"), area, 2,
                Style::default().fg(theme.yellow).bg(theme.modal), buf);
        }
        Some(NativeSessionPreviewState::Error(message)) => {
            render_modal_line(format!("  Preview or resume failed: {message}"), area, 2,
                Style::default().fg(theme.red).bg(theme.modal), buf);
        }
        None => {
            render_modal_line("  Select a Native session from the sidebar to inspect it.", area, 2,
                Style::default().fg(theme.muted).bg(theme.modal), buf);
        }
    }
    let start = session.preview_scroll.min(message_lines.len().saturating_sub(message_height));
    for (visible, (line, heading)) in message_lines.iter().skip(start).take(message_height).enumerate() {
        let style = if *heading {
            Style::default().fg(theme.teal).bg(theme.modal).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text).bg(theme.modal)
        };
        render_modal_line(line, area, 5 + visible as u16, style, buf);
    }
    if let Some(operation) = &session.operation {
        let phase = match operation {
            ExistingSessionOperation::IndexingOnly { .. } => "Index phase: saving exact provider session ID...",
            ExistingSessionOperation::IndexingForResume { .. } => "Native resume phase: indexing exact provider session ID...",
            ExistingSessionOperation::IndexingNativeForResume { .. } => "Native resume phase: linking selected history...",
            ExistingSessionOperation::Resuming { .. } => "Native resume phase: runtime admission / provider resume...",
        };
        render_modal_line(
            phase,
            area,
            buttons_row.saturating_sub(1),
            Style::default().fg(theme.yellow).bg(theme.modal),
            buf,
        );
    } else if let Some(message) = session.operation_error.as_deref() {
        render_modal_line(
            format!("Native resume failed: {message}"),
            area,
            buttons_row.saturating_sub(1),
            Style::default().fg(theme.red).bg(theme.modal),
            buf,
        );
    }
    render_existing_session_field(
        "Ask (not sent)",
        &session.ask_after_resume,
        session.field == ExistingSessionField::AskAfterResume,
        false,
        ExistingSessionField::AskAfterResume,
        area,
        buttons_row,
        theme,
        buf,
        layout,
    );
    let action_row = buttons_row.saturating_add(2);
    let can_resume = session.preview.as_ref().is_some_and(|preview| {
        matches!(preview, NativeSessionPreviewState::Ready(_) | NativeSessionPreviewState::Empty(_))
    }) && session.operation.is_none()
        && session.rows.get(session.selected).is_some_and(|row| {
            row.route.scope == gate4agent_types::NativeSessionCatalogScope::Workspace
                && provider_supports_native_resume(&row.route.provider)
        });
    let resume_label = "[Resume and open PTY]";
    let native_resume = Rect::new(area.x, area.y + action_row, 23.min(area.width), 1);
    Paragraph::new(resume_label)
        .style(if can_resume {
            Style::default().fg(theme.active_tab_text).bg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted).bg(theme.modal)
        })
        .render(native_resume, buf);
    if can_resume {
        push_modal_hit(layout, native_resume, HitTarget::ExistingSessionNativeResume);
    }
    let restore = Rect::new(native_resume.right().saturating_add(1), native_resume.y, 20.min(area.width), 1);
    Paragraph::new("[Restore via skill]")
        .style(Style::default().fg(theme.muted).bg(theme.modal))
        .render(restore, buf);
    push_modal_hit(layout, restore, HitTarget::ExistingSessionRestoreViaSkill);
    let advanced = Rect::new(restore.right().saturating_add(1), restore.y, 19.min(area.width), 1);
    Paragraph::new("[Advanced import]")
        .style(Style::default().fg(theme.teal).bg(theme.modal).add_modifier(Modifier::BOLD))
        .render(advanced, buf);
    push_modal_hit(layout, advanced, HitTarget::ExistingSessionAdvancedImport);
    let cancel = Rect::new(area.right().saturating_sub(6), native_resume.y, 6.min(area.width), 1);
    Paragraph::new("[Back]")
        .style(Style::default().fg(theme.text).bg(theme.active))
        .render(cancel, buf);
    push_modal_hit(layout, cancel, HitTarget::ExistingSessionBackToCatalog);
    let disabled_reason = session.rows.get(session.selected)
        .filter(|row| {
            row.route.scope != gate4agent_types::NativeSessionCatalogScope::Workspace
                || !provider_supports_native_resume(&row.route.provider)
        })
        .map(|row| {
            if row.route.scope == gate4agent_types::NativeSessionCatalogScope::Unregistered {
                "Preview available; register this project as a workspace to resume".to_owned()
            } else {
                format!(
                    "{} history and preview are available; native resume is not supported",
                    row.route.provider,
                )
            }
        })
        .unwrap_or_else(|| RESTORE_VIA_SKILL_DISABLED_REASON.to_owned());
    render_modal_line(
        disabled_reason,
        area,
        action_row + 2,
        Style::default().fg(theme.muted).bg(theme.modal),
        buf,
    );
    render_modal_line(
        "Tab messages/query | Enter resumes and opens PTY; query is retained but not sent | Esc back",
        area,
        action_row + 3,
        Style::default().fg(theme.muted).bg(theme.modal),
        buf,
    );
}

fn render_existing_session_advanced(
    app: &App,
    session: &crate::app::ExistingSessionDialog,
    area: Rect,
    theme: Theme,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
) {
    let node_value = node_route_value(app, &session.node_id);
    render_existing_session_field(
        "Node", &node_value, session.field == ExistingSessionField::Node, true,
        ExistingSessionField::Node, area, 0, theme, buf, layout,
    );
    render_existing_session_field(
        "Workspace", &session.workspace_id, session.field == ExistingSessionField::Workspace, true,
        ExistingSessionField::Workspace, area, 1, theme, buf, layout,
    );
    let provider = session.provider.to_string();
    render_existing_session_field(
        "Provider", &provider, session.field == ExistingSessionField::Provider, true,
        ExistingSessionField::Provider, area, 2, theme, buf, layout,
    );
    render_existing_session_field(
        "Display name",
        &session.display_name,
        session.field == ExistingSessionField::DisplayName,
        false,
        ExistingSessionField::DisplayName,
        area,
        4,
        theme,
        buf,
        layout,
    );
    render_existing_session_field(
        "Native session ID",
        &session.session_id,
        session.field == ExistingSessionField::SessionId,
        false,
        ExistingSessionField::SessionId,
        area,
        5,
        theme,
        buf,
        layout,
    );
    let import = Rect::new(area.right().saturating_sub(8), area.y + 8, 8.min(area.width), 1);
    Paragraph::new("[Import]")
        .style(Style::default().fg(theme.active_tab_text).bg(theme.accent).add_modifier(Modifier::BOLD))
        .render(import, buf);
    push_modal_hit(layout, import, HitTarget::ExistingSessionImport);
    let back = Rect::new(area.x, import.y, 18.min(area.width), 1);
    Paragraph::new("[Back to sessions]")
        .style(Style::default().fg(theme.teal).bg(theme.modal).add_modifier(Modifier::BOLD))
        .render(back, buf);
    push_modal_hit(layout, back, HitTarget::ExistingSessionBackToCatalog);
    render_modal_line(
        "Advanced import indexes only an exact provider session ID; it does not copy raw messages or provider paths.",
        area,
        11,
        Style::default().fg(theme.muted).bg(theme.modal),
        buf,
    );
    render_modal_line(
        "Tab fields | arrows select | Enter import | b back to sessions | Esc cancel",
        area,
        13,
        Style::default().fg(theme.muted).bg(theme.modal),
        buf,
    );
}

fn render_existing_session_field(
    label: &str,
    value: &str,
    active: bool,
    cycles: bool,
    field: ExistingSessionField,
    area: Rect,
    row: u16,
    theme: Theme,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
) {
    let style = if active {
        Style::default()
            .fg(theme.active_tab_text)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text).bg(theme.modal)
    };
    let rendered_value = if cycles {
        format!("[< {value} >]")
    } else {
        format!("[ {value} ]")
    };
    let prefix = format!("{} {:<18} ", if active { ">" } else { " " }, label);
    let line = format!("{prefix}{rendered_value}");
    render_modal_line(line.clone(), area, row, style, buf);
    let line_width = (cell_width(&line) as u16).min(area.width);
    push_modal_hit(
        layout,
        Rect::new(area.x, area.y.saturating_add(row), line_width, 1),
        HitTarget::ExistingSessionField(field),
    );
    if cycles {
        let value_start = area.x.saturating_add(cell_width(&prefix) as u16);
        let left_x = value_start.saturating_add(1);
        let right_x = value_start
            .saturating_add(4)
            .saturating_add(cell_width(value) as u16);
        if left_x < area.right() {
            push_modal_hit(
                layout,
                Rect::new(left_x, area.y.saturating_add(row), 1, 1),
                HitTarget::ExistingSessionFieldPrevious(field),
            );
        }
        if right_x < area.right() {
            push_modal_hit(
                layout,
                Rect::new(right_x, area.y.saturating_add(row), 1, 1),
                HitTarget::ExistingSessionFieldNext(field),
            );
        }
    }
}

fn render_spawn(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let Some(spawn) = &app.spawn else {
        return;
    };
    let width = 86.min(area.width);
    let height = 19.min(area.height);
    let dialog = positioned_modal(area, width, height, app.spawn_modal_position);
    layout.spawn_modal = dialog;
    fill_rect(dialog, theme.modal, buf);
    Block::bordered()
        .title(" session lab / launch ")
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.modal))
        .render(dialog, buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(dialog.x, dialog.y, dialog.width, 1),
        target: HitTarget::SpawnDrag,
    });
    if dialog.width < 3 || dialog.height < 3 {
        return;
    }
    let inner = Rect::new(dialog.x + 1, dialog.y + 1, dialog.width - 2, dialog.height - 2);
    let node_choices = app.launch_field_choice_count(spawn, LaunchField::Node);
    let node_value = node_route_value(app, &spawn.node_id);
    render_launch_field(
        "Node",
        &node_value,
        spawn.field == LaunchField::Node,
        node_choices.is_some_and(|choices| choices > 1),
        LaunchField::Node,
        inner,
        0,
        theme,
        buf,
        layout,
    );
    let workspace_choices = app.launch_field_choice_count(spawn, LaunchField::Workspace);
    let workspace_field_end = render_launch_field(
        "Workspace",
        &spawn.workspace_id,
        spawn.field == LaunchField::Workspace,
        workspace_choices.is_some_and(|choices| choices > 1),
        LaunchField::Workspace,
        inner,
        1,
        theme,
        buf,
        layout,
    );
    render_inline_launch_action(
        "[Browse\u{2026}]",
        workspace_field_end,
        inner,
        1,
        true,
        HitTarget::SpawnRegisterWorkspace,
        theme,
        buf,
        layout,
    );
    let target_field_end = render_launch_field(
        "Git location",
        match spawn.target {
            LaunchTarget::ExistingWorkspace => "Existing workspace",
            LaunchTarget::NewLinkedWorktree => "New linked worktree",
            LaunchTarget::NewStandaloneRepository => "New standalone repository",
        },
        spawn.field == LaunchField::GitLocation,
        true,
        LaunchField::GitLocation,
        inner,
        2,
        theme,
        buf,
        layout,
    );
    render_inline_launch_action(
        "[Configure\u{2026}]",
        target_field_end,
        inner,
        2,
        spawn.target != LaunchTarget::ExistingWorkspace,
        HitTarget::SpawnConfigureGitLocation,
        theme,
        buf,
        layout,
    );
    render_launch_field(
        "Provider",
        &spawn.provider.to_string(),
        spawn.field == LaunchField::Provider,
        true,
        LaunchField::Provider,
        inner,
        3,
        theme,
        buf,
        layout,
    );
    let bundle_choices = app.launch_field_choice_count(spawn, LaunchField::Delivery);
    let bundle_value = if spawn.bundle_id.is_empty() && bundle_choices == Some(1) {
        "none installed"
    } else if spawn.bundle_id.is_empty() {
        "none"
    } else {
        &spawn.bundle_id
    };
    render_launch_field(
        "Delivery",
        bundle_value,
        spawn.field == LaunchField::Delivery,
        bundle_choices.is_some_and(|choices| choices > 1),
        LaunchField::Delivery,
        inner,
        4,
        theme,
        buf,
        layout,
    );
    let context_available = app.spawn_context_available(spawn);
    render_launch_field(
        "Continue from",
        match (spawn.context_mode, context_available) {
            (LaunchContextMode::None, false) => "none (no exported pack)",
            (LaunchContextMode::None, true) => "none",
            (LaunchContextMode::ContextPack, _) => "context-pack",
        },
        spawn.field == LaunchField::ContinueFrom,
        context_available,
        LaunchField::ContinueFrom,
        inner,
        5,
        theme,
        buf,
        layout,
    );
    let launch = Rect::new(inner.right().saturating_sub(8), inner.y + 7, 8.min(inner.width), 1);
    Paragraph::new("[Launch]")
        .style(Style::default().fg(theme.active_tab_text).bg(theme.accent).add_modifier(Modifier::BOLD))
        .render(launch, buf);
    push_modal_hit(layout, launch, HitTarget::SpawnLaunch);
    let cancel = Rect::new(launch.x.saturating_sub(9).max(inner.x), inner.y + 7, 8.min(inner.width), 1);
    Paragraph::new("[Cancel]")
        .style(Style::default().fg(theme.text).bg(theme.active))
        .render(cancel, buf);
    push_modal_hit(layout, cancel, HitTarget::SpawnCancel);
    render_modal_line(
        launch_field_help(spawn.field, spawn.target),
        inner,
        9,
        Style::default().fg(theme.muted).bg(theme.modal),
        buf,
    );
    render_modal_line(
        "Enter configures new Git locations; Launch always starts an interactive PTY",
        inner,
        10,
        Style::default().fg(theme.muted).bg(theme.modal),
        buf,
    );
    render_receipt(app, inner, 12, buf, theme);
}

fn launch_field_help(field: LaunchField, target: LaunchTarget) -> &'static str {
    match field {
        LaunchField::Node => "Connected machine that owns the CLI process, workspaces, Git operations, and delivery.",
        LaunchField::Workspace => "Registered project root on the selected Node where the session can start.",
        LaunchField::GitLocation if target == LaunchTarget::ExistingWorkspace => {
            "Use selected workspace: start directly in its current Git working tree."
        }
        LaunchField::GitLocation if target == LaunchTarget::NewLinkedWorktree => {
            "Configure the linked worktree; managed policy comes from the selected workspace."
        }
        LaunchField::GitLocation => "Configure a new standalone Git repository before launching in it.",
        LaunchField::Provider => "Installed CLI adapter to wrap; provider login remains outside Gate4Agent.",
        LaunchField::Delivery => {
            "Immutable skills/plugin package already installed on this Node; none installed means no delivery occurs."
        }
        LaunchField::ContinueFrom => {
            "Attach a same-Node exported ContextPack file; provider consumption is a separate explicit step."
        }
    }
}

fn render_launch_field(
    label: &str,
    value: &str,
    active: bool,
    cycles: bool,
    field: LaunchField,
    area: Rect,
    row: u16,
    theme: Theme,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
) -> u16 {
    let style = if active {
        Style::default()
            .fg(theme.active_tab_text)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text).bg(theme.modal)
    };
    let rendered_value = if cycles {
        format!("[< {value} >]")
    } else {
        format!("[ {value} ]")
    };
    let prefix = format!("{} {:<18} ", if active { ">" } else { " " }, label);
    let line = format!("{prefix}{rendered_value}");
    let line_width = (cell_width(&line) as u16).min(area.width);
    render_modal_line(
        line,
        area,
        row,
        style,
        buf,
    );
    push_modal_hit(
        layout,
        Rect::new(area.x, area.y.saturating_add(row), line_width, 1),
        HitTarget::SpawnField(field),
    );
    if cycles {
        let value_start = area.x.saturating_add(cell_width(&prefix) as u16);
        let left_x = value_start.saturating_add(1);
        let right_x = value_start
            .saturating_add(4)
            .saturating_add(cell_width(value) as u16);
        let right = area.right();
        if left_x < right {
            push_modal_hit(
                layout,
                Rect::new(left_x, area.y.saturating_add(row), 1, 1),
                HitTarget::SpawnFieldPrevious(field),
            );
        }
        if right_x < right {
            push_modal_hit(
                layout,
                Rect::new(right_x, area.y.saturating_add(row), 1, 1),
                HitTarget::SpawnFieldNext(field),
            );
        }
    }
    area.x.saturating_add(line_width)
}

fn render_inline_launch_action(
    label: &str,
    field_end: u16,
    area: Rect,
    row: u16,
    enabled: bool,
    target: HitTarget,
    theme: Theme,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
) {
    if row >= area.height || area.width == 0 {
        return;
    }
    let width = (cell_width(label) as u16).min(area.width);
    let x = field_end
        .saturating_add(1)
        .min(area.right().saturating_sub(width));
    let action = Rect::new(x, area.y + row, width, 1);
    if let Some(field_hit) = layout.hits.iter_mut().rev().find(|hit| {
        matches!(hit.target, HitTarget::SpawnField(_))
            && hit.rect.y == action.y
            && hit.rect.x < action.x
    }) {
        field_hit.rect.width = action.x.saturating_sub(field_hit.rect.x);
    }
    Paragraph::new(truncate_cells(label, width as usize))
        .style(
            Style::default()
                .fg(if enabled { theme.teal } else { theme.muted })
                .bg(theme.modal)
                .add_modifier(Modifier::BOLD),
        )
        .render(action, buf);
    if enabled {
        push_modal_hit(layout, action, target);
    }
}

fn render_history(app: &App, area: Rect, buf: &mut TerminalBuffer, theme: Theme) {
    let Some(history) = &app.history else {
        return;
    };
    let width = 96.min(area.width.saturating_sub(4));
    let height = 22.min(area.height.saturating_sub(2));
    let dialog = centered(area, width, height);
    fill_rect(dialog, theme.modal, buf);
    Block::bordered()
        .title(" native session history ")
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.modal))
        .render(dialog, buf);
    if dialog.width < 3 || dialog.height < 3 {
        return;
    }
    let inner = Rect::new(dialog.x + 1, dialog.y + 1, dialog.width - 2, dialog.height - 2);
    render_modal_line(
        format!(
            "source provider={} address={}/{} #{}:{}",
            history.source_provider,
            history.source.node_id,
            history.source.workspace_id,
            history.source.instance_id,
            history.source.generation,
        ),
        inner,
        0,
        Style::default().fg(theme.text).bg(theme.modal).add_modifier(Modifier::BOLD),
        buf,
    );
    render_modal_line(
        format!("source workspace={}", host_path_display(&history.source_workspace_root)),
        inner,
        1,
        Style::default().fg(theme.dim).bg(theme.modal),
        buf,
    );
    render_modal_line(
        format!("candidates ({})", history.candidates.len()),
        inner,
        2,
        Style::default().fg(theme.teal).bg(theme.modal).add_modifier(Modifier::BOLD),
        buf,
    );
    let candidate_capacity = 5_usize;
    let selected = history.selected.min(history.candidates.len().saturating_sub(1));
    let start = selected.saturating_sub(candidate_capacity.saturating_sub(1));
    for (visible, (index, candidate)) in history
        .candidates
        .iter()
        .enumerate()
        .skip(start)
        .take(candidate_capacity)
        .enumerate()
    {
        let active = index == selected;
        let timestamp = candidate
            .modified_at_unix_ms
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let style = if active {
            Style::default().fg(theme.active_tab_text).bg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text).bg(theme.modal)
        };
        render_modal_line(
            format!(
                "{} id={} | hint={} | modified_unix_ms={timestamp}",
                if active { ">" } else { " " },
                candidate.id,
                candidate.session_id_hint,
            ),
            inner,
            3 + visible as u16,
            style,
            buf,
        );
    }
    let loaded = history.loaded.as_ref().map(|loaded| {
        format!(
            "loaded native_session_id={} message_count={} completed_turn_count={}",
            loaded.session_id,
            loaded.message_count,
            loaded.completed_turn_count.map(|count| count.to_string()).unwrap_or_else(|| "unknown".to_owned()),
        )
    }).unwrap_or_else(|| "loaded none".to_owned());
    render_modal_line(loaded, inner, 8, Style::default().fg(theme.green).bg(theme.modal), buf);
    if let Some(context) = history.context.as_ref() {
        render_modal_line(format!("exported context_id={}", context.id), inner, 9, Style::default().fg(theme.teal).bg(theme.modal), buf);
        render_modal_line(format!("exported digest={}", context.digest), inner, 10, Style::default().fg(theme.dim).bg(theme.modal), buf);
        render_modal_line(
            format!(
                "exported message_count={}/{} truncated={}",
                context.retained_message_count, context.source_message_count, context.truncated
            ),
            inner,
            11,
            Style::default().fg(theme.dim).bg(theme.modal),
            buf,
        );
    } else {
        render_modal_line("exported context none", inner, 9, Style::default().fg(theme.muted).bg(theme.modal), buf);
    }
    if let Some(pending) = history.pending_label.as_ref() {
        render_modal_line(format!("pending {pending}"), inner, 12, Style::default().fg(theme.yellow).bg(theme.modal), buf);
    }
    render_modal_line(
        "Enter load | x export | f forget exported context | Esc close",
        inner,
        13,
        Style::default().fg(theme.muted).bg(theme.modal),
        buf,
    );
    render_receipt(app, inner, 15, buf, theme);
}

fn render_receipt(app: &App, area: Rect, start_row: u16, buf: &mut TerminalBuffer, theme: Theme) {
    let receipt = resolved_receipt(app);
    render_modal_line(
        if receipt.is_some() { "last resolved launch" } else { "last resolved launch none" },
        area,
        start_row.saturating_sub(1),
        Style::default().fg(theme.teal).bg(theme.modal).add_modifier(Modifier::BOLD),
        buf,
    );
    let Some((receipt, managed_worktree)) = receipt else {
        return;
    };
    render_modal_line(
        format!(
            "provider={} mode={} workspace={} worktree={}",
            receipt.provider,
            session_mode_label(receipt.mode),
            receipt.target.workspace_id,
            receipt.target.worktree_id.as_ref().map(|id| id.as_str()).unwrap_or("current"),
        ),
        area,
        start_row,
        Style::default().fg(theme.text).bg(theme.modal),
        buf,
    );
    let environment = receipt.environment_profile.as_ref().map(|profile| {
        format!(" env={}@{}", profile.profile_id, profile.profile_revision)
    }).unwrap_or_default();
    let managed = managed_worktree.map(|lease| {
        let latest = app
            .last_managed_worktree_lease
            .as_ref()
            .filter(|latest| latest.lease_id == lease.lease_id)
            .unwrap_or(lease);
        format!(" managed={}@{}", latest.profile_id, latest.profile_revision)
    }).unwrap_or_default();
    render_modal_line(
        format!("profile={} revision={}{}{}", receipt.profile_id, receipt.profile_revision, environment, managed),
        area,
        start_row + 1,
        Style::default().fg(theme.dim).bg(theme.modal),
        buf,
    );
    if let Some(initial) = managed_worktree {
        let latest = app
            .last_managed_worktree_lease
            .as_ref()
            .filter(|lease| lease.lease_id == initial.lease_id)
            .unwrap_or(initial);
        let removed = app
            .last_managed_worktree_removed
            .as_deref()
            .is_some_and(|lease_id| lease_id == initial.lease_id.as_str());
        let failure = latest
            .cleanup_failure
            .map(managed_worktree_cleanup_failure_label)
            .unwrap_or("none");
        render_modal_line(
            format!(
                "worktree state={} sessions={} records={} failure={}",
                if removed { "removed" } else { managed_worktree_state_label(latest.state) },
                latest.active_session_count,
                latest.managed_record_count,
                failure,
            ),
            area,
            start_row + 2,
            Style::default().fg(theme.dim).bg(theme.modal),
            buf,
        );
    } else {
        render_modal_line(
            "worktree=none",
            area,
            start_row + 2,
            Style::default().fg(theme.dim).bg(theme.modal),
            buf,
        );
    }
    let bundle = receipt.bundle.as_ref().map(|bundle| {
        format!("bundle revision={} digest={}", bundle.revision, compact_digest(bundle.digest.as_str()))
    }).unwrap_or_else(|| "bundle=none".to_owned());
    render_modal_line(bundle, area, start_row + 3, Style::default().fg(theme.dim).bg(theme.modal), buf);
    let context = receipt.context.as_ref().map(|context| {
        format!(
            "ctx src={}/{}#{}:{}/{} digest={} count={}/{} cut={}",
            context.lineage.source_node_id,
            context.lineage.source_session.workspace_id,
            context.lineage.source_session.session.instance_id.0,
            context.lineage.source_session.session.generation.0,
            context.lineage.source_provider,
            compact_digest_hash(context.digest.as_str()),
            context.retained_message_count,
            context.source_message_count,
            context.truncated,
        )
    }).unwrap_or_else(|| "context=none".to_owned());
    render_modal_line(context, area, start_row + 4, Style::default().fg(theme.dim).bg(theme.modal), buf);
}

fn resolved_receipt(app: &App) -> Option<(&ResolvedSpawnReceipt, Option<&ManagedWorktreeLeaseSnapshot>)> {
    app.last_managed_spawn_receipt
        .as_ref()
        .map(|receipt| (&receipt.spawn, Some(&receipt.lease)))
        .or_else(|| app.last_spawn_receipt.as_ref().map(|receipt| (receipt, None)))
}

fn session_mode_label(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::Pty => "pty",
        SessionMode::Inline => "inline",
    }
}

fn managed_worktree_state_label(state: ManagedWorktreeLeaseState) -> &'static str {
    match state {
        ManagedWorktreeLeaseState::Allocating => "allocating",
        ManagedWorktreeLeaseState::Ready => "ready",
        ManagedWorktreeLeaseState::InUse => "in-use",
        ManagedWorktreeLeaseState::Retained => "retained",
        ManagedWorktreeLeaseState::CleanupBlocked => "cleanup-blocked",
        ManagedWorktreeLeaseState::RecoveryRequired => "recovery-required",
        ManagedWorktreeLeaseState::Removed => "removed",
    }
}

fn managed_worktree_cleanup_failure_label(failure: ManagedWorktreeCleanupFailure) -> &'static str {
    match failure {
        ManagedWorktreeCleanupFailure::Busy => "busy",
        ManagedWorktreeCleanupFailure::Dirty => "dirty",
        ManagedWorktreeCleanupFailure::Locked => "locked",
        ManagedWorktreeCleanupFailure::Prunable => "prunable",
        ManagedWorktreeCleanupFailure::OwnershipConflict => "ownership-conflict",
        ManagedWorktreeCleanupFailure::Backend => "backend",
    }
}

fn compact_digest(digest: &str) -> String {
    let keep = "sha256:".len() + 12;
    if digest.len() <= keep {
        digest.to_owned()
    } else {
        format!("{}...", &digest[..keep])
    }
}

fn compact_digest_hash(digest: &str) -> &str {
    digest
        .strip_prefix("sha256:")
        .unwrap_or(digest)
        .get(..8)
        .unwrap_or(digest)
}

fn render_modal_line(
    text: impl AsRef<str>,
    area: Rect,
    row: u16,
    style: Style,
    buf: &mut TerminalBuffer,
) {
    if row >= area.height {
        return;
    }
    Paragraph::new(truncate_cells(text.as_ref(), area.width as usize))
        .style(style)
        .render(Rect::new(area.x, area.y + row, area.width, 1), buf);
}

fn modal_row(area: Rect, row: u16) -> Rect {
    if row >= area.height {
        Rect::default()
    } else {
        Rect::new(area.x, area.y + row, area.width, 1)
    }
}

fn push_modal_hit(layout: &mut LayoutRects, rect: Rect, target: HitTarget) {
    if rect.width > 0 && rect.height > 0 {
        layout.hits.push(HitRegion { rect, target });
    }
}

fn render_add_space(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let Some(dialog) = &app.add_space else {
        return;
    };
    let width = 72.min(area.width.saturating_sub(4));
    let height = 10.min(area.height.saturating_sub(2));
    let modal = positioned_modal(area, width, height, app.add_space_modal_position);
    layout.add_space_modal = modal;
    fill_rect(modal, theme.modal, buf);
    Block::bordered()
        .title(" add workspace ")
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.modal))
        .render(modal, buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(modal.x, modal.y, modal.width, 1),
        target: HitTarget::AddSpaceDrag,
    });
    if modal.width < 3 || modal.height < 3 {
        return;
    }
    let inner = Rect::new(modal.x + 1, modal.y + 1, modal.width - 2, modal.height - 2);
    render_modal_line(
        format!("  {:<12} {}", "node", dialog.node_id),
        inner,
        0,
        Style::default().fg(theme.muted).bg(theme.modal),
        buf,
    );
    render_add_space_field(
        "workspace ID",
        &dialog.workspace_id,
        dialog.field == AddSpaceField::WorkspaceId,
        AddSpaceField::WorkspaceId,
        inner,
        1,
        theme,
        buf,
        layout,
    );
    render_add_space_field(
        "root path",
        &dialog.root,
        dialog.field == AddSpaceField::Root,
        AddSpaceField::Root,
        inner,
        2,
        theme,
        buf,
        layout,
    );
    let browse = Rect::new(inner.right().saturating_sub(10), inner.y + 2, 10.min(inner.width), 1);
    Paragraph::new("[Browse…]")
        .style(Style::default().fg(theme.teal).bg(theme.modal).add_modifier(Modifier::BOLD))
        .render(browse, buf);
    push_modal_hit(layout, browse, HitTarget::AddSpaceBrowse);
    let register = Rect::new(inner.right().saturating_sub(10), inner.y + 4, 10.min(inner.width), 1);
    Paragraph::new("[Register]")
        .style(Style::default().fg(theme.active_tab_text).bg(theme.accent).add_modifier(Modifier::BOLD))
        .render(register, buf);
    push_modal_hit(layout, register, HitTarget::AddSpaceRegister);
    let cancel = Rect::new(register.x.saturating_sub(9).max(inner.x), inner.y + 4, 8.min(inner.width), 1);
    Paragraph::new("[Cancel]")
        .style(Style::default().fg(theme.text).bg(theme.active))
        .render(cancel, buf);
    push_modal_hit(layout, cancel, HitTarget::AddSpaceCancel);
    render_modal_line(
        "Click a field, then type or paste | drag title",
        inner,
        6,
        Style::default().fg(theme.muted).bg(theme.modal),
        buf,
    );
}

fn render_add_space_field(
    label: &str,
    value: &str,
    active: bool,
    field: AddSpaceField,
    area: Rect,
    row: u16,
    theme: Theme,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
) {
    let style = if active {
        Style::default()
            .fg(theme.active_tab_text)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text).bg(theme.modal)
    };
    render_modal_line(
        format!("{} {:<12} [ {} ]", if active { ">" } else { " " }, label, value),
        area,
        row,
        style,
        buf,
    );
    push_modal_hit(layout, modal_row(area, row), HitTarget::AddSpaceField(field));
}

fn render_folder_browser(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let Some(browser) = &app.folder_browser else {
        return;
    };
    let width = 92.min(area.width.saturating_sub(2));
    let height = 26.min(area.height.saturating_sub(2));
    let modal = positioned_modal(area, width, height, app.folder_browser_modal_position);
    layout.folder_browser_modal = modal;
    fill_rect(modal, theme.modal, buf);
    Block::bordered()
        .title(" browse directories on node ")
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.modal))
        .render(modal, buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(modal.x, modal.y, modal.width, 1),
        target: HitTarget::FolderBrowserDrag,
    });
    if modal.width < 3 || modal.height < 3 {
        return;
    }
    let inner = Rect::new(modal.x + 1, modal.y + 1, modal.width - 2, modal.height - 2);
    let directory = browser
        .directory
        .as_ref()
        .map(host_path_display)
        .unwrap_or_else(|| "computer roots".to_owned());
    render_modal_line(
        format!("node={} | {}", browser.node_id, directory),
        inner,
        0,
        Style::default().fg(theme.text).bg(theme.modal).add_modifier(Modifier::BOLD),
        buf,
    );
    let parent = Rect::new(inner.x, inner.y + 1, 12.min(inner.width), 1);
    Paragraph::new("[↑ Parent]")
        .style(Style::default().fg(theme.teal).bg(theme.modal))
        .render(parent, buf);
    push_modal_hit(layout, parent, HitTarget::FolderBrowserParent);
    let filter_active = browser.field == FolderBrowserField::Filter;
    let filter = Rect::new(inner.x, inner.y + 2, inner.width, 1);
    Paragraph::new(format!(
        "{} filter [ {} ]",
        if filter_active { ">" } else { " " },
        browser.filter,
    ))
    .style(if filter_active {
        Style::default()
            .fg(theme.active_tab_text)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text).bg(theme.modal)
    })
    .render(filter, buf);
    push_modal_hit(layout, filter, HitTarget::FolderBrowserFilter);

    let visible = app.visible_host_directory_indices();
    let list_capacity = inner.height.saturating_sub(8) as usize;
    let selected = browser.selected.min(visible.len().saturating_sub(1));
    let start = selected.saturating_sub(list_capacity.saturating_sub(1));
    for (row, entry_index) in visible
        .iter()
        .copied()
        .skip(start)
        .take(list_capacity)
        .enumerate()
    {
        let Some(entry) = browser.entries.get(entry_index) else {
            continue;
        };
        let active = start + row == selected && browser.field == FolderBrowserField::Entries;
        let marker = if entry.is_link { "↪" } else { "▸" };
        let line = Rect::new(inner.x, inner.y + 3 + row as u16, inner.width, 1);
        Paragraph::new(format!(
            "{} {} {}",
            if active { ">" } else { " " },
            marker,
            entry.display_name,
        ))
        .style(if active {
            Style::default()
                .fg(theme.active_tab_text)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text).bg(theme.modal)
        })
        .render(line, buf);
        push_modal_hit(layout, line, HitTarget::FolderBrowserEntry(entry_index));
    }

    let action_row = inner.bottom().saturating_sub(3);
    let can_load_more = browser.next_after.is_some()
        && browser.entries.len() < MAX_BROWSER_LOADED_ENTRIES
        && !browser.pending;
    let load_more = Rect::new(inner.x, action_row, 13.min(inner.width), 1);
    Paragraph::new(if can_load_more { "[Load more]" } else { " Load more " })
        .style(Style::default().fg(if can_load_more { theme.teal } else { theme.muted }).bg(theme.modal))
        .render(load_more, buf);
    if can_load_more {
        push_modal_hit(layout, load_more, HitTarget::FolderBrowserLoadMore);
    }
    let use_folder = Rect::new(inner.right().saturating_sub(17), action_row, 17.min(inner.width), 1);
    Paragraph::new("[Use this folder]")
        .style(Style::default().fg(if browser.directory.is_some() { theme.active_tab_text } else { theme.muted }).bg(if browser.directory.is_some() { theme.accent } else { theme.modal }).add_modifier(Modifier::BOLD))
        .render(use_folder, buf);
    if browser.directory.is_some() {
        push_modal_hit(layout, use_folder, HitTarget::FolderBrowserUse);
    }
    let cancel = Rect::new(use_folder.x.saturating_sub(9).max(inner.x), action_row, 8.min(inner.width), 1);
    Paragraph::new("[Cancel]")
        .style(Style::default().fg(theme.text).bg(theme.active))
        .render(cancel, buf);
    push_modal_hit(layout, cancel, HitTarget::FolderBrowserCancel);

    let status = if browser.pending {
        if browser.append_pending { "loading next page…".to_owned() } else { "loading directory…".to_owned() }
    } else if let Some(error) = browser.error.as_ref() {
        format!("error: {error}")
    } else if browser.entries.len() == MAX_BROWSER_LOADED_ENTRIES
        && browser.next_after.is_some()
    {
        format!("{} directories loaded (client limit reached)", visible.len())
    } else {
        format!(
            "{} director{} loaded{}",
            visible.len(),
            if visible.len() == 1 { "y" } else { "ies" },
            if browser.incomplete { " (incomplete)" } else { "" },
        )
    };
    render_modal_line(status, inner, inner.height.saturating_sub(2), Style::default().fg(theme.dim).bg(theme.modal), buf);
    render_modal_line(
        "Enter open | ←/Backspace parent | u use | m more | Tab filter | drag title",
        inner,
        inner.height.saturating_sub(1),
        Style::default().fg(theme.muted).bg(theme.modal),
        buf,
    );
}

fn render_create_worktree(
    app: &App,
    area: Rect,
    buf: &mut TerminalBuffer,
    layout: &mut LayoutRects,
    theme: Theme,
) {
    let Some(dialog) = &app.create_worktree else {
        return;
    };
    let width = 72.min(area.width.saturating_sub(4));
    let height = 14.min(area.height.saturating_sub(2));
    let modal = positioned_modal(area, width, height, app.create_worktree_modal_position);
    layout.create_worktree_modal = modal;
    fill_rect(modal, theme.modal, buf);
    let prefix = |field| if dialog.field == field { ">" } else { " " };
    let (lines, editable_fields, primary_label, primary_enabled, detail) = match dialog.kind {
        GitLocationDialogKind::ManualLinked => (
            vec![
                Line::styled(format!("source  {} / {}", dialog.node_id, dialog.source_workspace_id), Style::default().fg(theme.muted)),
                Line::styled(format!("{} worktree id  {}", prefix(CreateWorktreeField::WorkspaceId), dialog.workspace_id), Style::default().fg(theme.text)),
                Line::styled(format!("{} root         {}", prefix(CreateWorktreeField::TargetRoot), dialog.target_root), Style::default().fg(theme.text)),
                Line::styled(format!("{} branch       {}", prefix(CreateWorktreeField::Branch), dialog.branch), Style::default().fg(theme.text)),
                Line::styled(format!("{} base         {}", prefix(CreateWorktreeField::Base), if dialog.base.is_empty() { "(HEAD)" } else { &dialog.base }), Style::default().fg(theme.text)),
                Line::styled("Manual policy: Node creates and registers this linked worktree.", Style::default().fg(theme.muted)),
            ],
            vec![(1, CreateWorktreeField::WorkspaceId), (2, CreateWorktreeField::TargetRoot), (3, CreateWorktreeField::Branch), (4, CreateWorktreeField::Base)],
            "[Create & register]",
            true,
            "Tab field | Enter create and return ready to Launch | Esc cancel",
        ),
        GitLocationDialogKind::Standalone => (
            vec![
                Line::styled(format!("node  {}", dialog.node_id), Style::default().fg(theme.muted)),
                Line::styled(format!("{} workspace id    {}", prefix(CreateWorktreeField::WorkspaceId), dialog.workspace_id), Style::default().fg(theme.text)),
                Line::styled(format!("{} root            {}", prefix(CreateWorktreeField::TargetRoot), dialog.target_root), Style::default().fg(theme.text)),
                Line::styled(format!("{} initial branch  {}", prefix(CreateWorktreeField::Branch), if dialog.branch.is_empty() { "(default)" } else { &dialog.branch }), Style::default().fg(theme.text)),
                Line::styled("Creates a new standalone Git repository outside the source workspace.", Style::default().fg(theme.muted)),
            ],
            vec![(1, CreateWorktreeField::WorkspaceId), (2, CreateWorktreeField::TargetRoot), (3, CreateWorktreeField::Branch)],
            "[Create repository]",
            true,
            "Tab field | Enter create and return ready to Launch | Esc cancel",
        ),
        GitLocationDialogKind::ManagedLinked => {
            let policy = app.spawn.as_ref().and_then(|spawn| app.selected_managed_worktree_profile(spawn));
            let summary = policy.as_ref().map(|profile| format!("profile    {}@{}", profile.id, profile.revision)).unwrap_or_else(|| "profile    none advertised".to_owned());
            let retention = policy.as_ref().map(|profile| match profile.retention {
                ManagedWorktreeRetention::RemoveWhenReleased => "remove when released",
                ManagedWorktreeRetention::Retain => "retain after session",
            }).unwrap_or("unavailable");
            (
                vec![
                    Line::styled(format!("source     {} / {}", dialog.node_id, dialog.source_workspace_id), Style::default().fg(theme.muted)),
                    Line::styled("mode       Managed", Style::default().fg(theme.text)),
                    Line::styled(summary, Style::default().fg(theme.text)),
                    Line::styled(format!("retention  {retention}"), Style::default().fg(theme.text)),
                    Line::styled("The advertised Node policy is read-only in Launch.", Style::default().fg(theme.muted)),
                ],
                Vec::new(),
                "[Launch]",
                policy.is_some(),
                "Enter confirms the advertised policy and launches an interactive PTY",
            )
        }
        GitLocationDialogKind::LinkedDisabled => (
            vec![
                Line::styled(format!("source  {} / {}", dialog.node_id, dialog.source_workspace_id), Style::default().fg(theme.muted)),
                Line::styled("mode     Off", Style::default().fg(theme.text)),
                Line::styled("Disabled: this workspace explicitly disallows linked worktrees.", Style::default().fg(theme.red)),
                Line::styled("Choose Existing workspace or New standalone repository.", Style::default().fg(theme.muted)),
            ],
            Vec::new(),
            "[Disabled]",
            false,
            "Esc returns to Git location",
        ),
    };
    Paragraph::new(Text::from_lines(lines))
    .block(
        Block::bordered()
            .title(" configure Git location ")
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.modal)),
    )
    .style(Style::default().fg(theme.text).bg(theme.modal))
    .render(modal, buf);
    layout.hits.push(HitRegion {
        rect: Rect::new(modal.x, modal.y, modal.width, 1),
        target: HitTarget::CreateWorktreeDrag,
    });
    if modal.width < 3 || modal.height < 3 {
        return;
    }
    let inner = Rect::new(modal.x + 1, modal.y + 1, modal.width - 2, modal.height - 2);
    for (row, field) in editable_fields {
        push_modal_hit(layout, modal_row(inner, row), HitTarget::CreateWorktreeField(field));
    }
    if let Some(preview) = match dialog.kind {
        GitLocationDialogKind::ManualLinked => Some(format!(
            "preview: {} -> {} | branch {} @ {}",
            dialog.source_workspace_id,
            dialog.workspace_id,
            dialog.branch,
            if dialog.base.is_empty() { "HEAD" } else { &dialog.base },
        )),
        GitLocationDialogKind::Standalone => Some(format!(
            "preview: repository {} at {} | initial branch {}",
            dialog.workspace_id,
            dialog.target_root,
            if dialog.branch.is_empty() { "provider default" } else { &dialog.branch },
        )),
        GitLocationDialogKind::ManagedLinked | GitLocationDialogKind::LinkedDisabled => None,
    } {
        render_modal_line(
            preview,
            inner,
            6,
            Style::default().fg(theme.dim).bg(theme.modal),
            buf,
        );
    }
    let primary_width = (cell_width(primary_label) as u16).min(inner.width);
    let create = Rect::new(inner.right().saturating_sub(primary_width), inner.y + 8, primary_width, 1);
    Paragraph::new(primary_label)
        .style(if primary_enabled {
            Style::default().fg(theme.active_tab_text).bg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted).bg(theme.modal)
        })
        .render(create, buf);
    if primary_enabled {
        push_modal_hit(layout, create, HitTarget::CreateWorktreeCreate);
    }
    let cancel = Rect::new(create.x.saturating_sub(9).max(inner.x), inner.y + 8, 8.min(inner.width), 1);
    Paragraph::new("[Cancel]")
        .style(Style::default().fg(theme.text).bg(theme.active))
        .render(cancel, buf);
    push_modal_hit(layout, cancel, HitTarget::CreateWorktreeCancel);
    render_modal_line(
        detail,
        inner,
        10,
        Style::default().fg(theme.muted).bg(theme.modal),
        buf,
    );
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
        Line::styled(truncate_cells(&host_path_display(&dialog.target_root), width.saturating_sub(4) as usize), Style::default().fg(theme.dim)),
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
        Line::styled(
            if dialog.local_alias {
                "Enter save local alias (empty clears) | Esc cancel"
            } else {
                "Enter rename provider record | Esc cancel"
            },
            Style::default().fg(theme.muted),
        ),
    ]))
    .block(
        Block::bordered()
            .title(if dialog.local_alias {
                " local alias "
            } else {
                " rename provider record "
            })
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.modal)),
    )
    .style(Style::default().fg(theme.text).bg(theme.modal))
    .render(modal, buf);
}

fn render_task_id(app: &App, area: Rect, buf: &mut TerminalBuffer, theme: Theme) {
    let Some(dialog) = &app.task_id_dialog else {
        return;
    };
    let width = 62.min(area.width.saturating_sub(4));
    let height = 7.min(area.height.saturating_sub(2));
    let modal = centered(area, width, height);
    fill_rect(modal, theme.modal, buf);
    Paragraph::new(Text::from_lines(vec![
        Line::styled(
            format!("{} / {} · revision {}", dialog.node_id, dialog.record_id, dialog.expected_revision),
            Style::default().fg(theme.muted),
        ),
        Line::styled(
            format!("> {}", dialog.value),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            "Exact task- plus 24 lowercase hex · Enter assign · Esc cancel",
            Style::default().fg(theme.muted),
        ),
    ]))
    .block(
        Block::bordered()
            .title(" assign task ID ")
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
        ControlSection::Agents | ControlSection::Workspaces => {
            render_roster(app, content, buf, layout, content_theme)
        }
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
                        .map(|entry| cell_width(&repository_path_display(&entry.relative_path)) + 8)
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
                        .map(|entry| {
                            let current = repository_path_display(&entry.path);
                            let path_width = entry.previous_path.as_ref().map_or_else(
                                || cell_width(&current),
                                |previous| {
                                    cell_width(&repository_path_display(previous))
                                        + cell_width(" -> ")
                                        + cell_width(&current)
                                },
                            );
                            path_width + 5
                        })
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
                        return cell_width(&record.short_title())
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
            let native_rows = app.existing_session.as_ref().map_or(0, |_| {
                app.native_session_tree_items().len().saturating_add(5)
            });
            (
                width,
                rows.len()
                    .saturating_mul(2)
                    .saturating_add(1)
                    .saturating_add(native_rows),
            )
        }
        ControlSection::Workspaces => {
            let rows = app.space_rows();
            let width = rows
                .iter()
                .map(|(node_index, workspace_index)| {
                    let node = &app.nodes[*node_index];
                    let workspace = &node.workspaces[*workspace_index];
                    cell_width(&workspace.label)
                        .max(cell_width(&host_path_display(&workspace.canonical_root)) + cell_width(&node.node_id) + 3)
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
        tab,
        current_column,
        current_row,
        moved: true,
        ..
    }) = &app.drag_state
    else {
        return;
    };
    if let Some(target) = layout
        .surface_panes
        .iter()
        .find(|pane| pane.frame.contains(*current_column, *current_row))
    {
        let zone = surface_drop_zone(target.frame, *current_column, *current_row);
        draw_surface_compass(target.frame, zone, buf, theme);
    }

    let title = app.surface_tab_title(tab);
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

fn draw_surface_compass(
    frame: Rect,
    active: SurfaceDropZone,
    buf: &mut TerminalBuffer,
    theme: Theme,
) {
    draw_outline(frame, theme.accent, theme.surface, buf);
    let active_rect = surface_zone_rect(frame, active);
    for row in active_rect.y..active_rect.bottom() {
        for column in active_rect.x..active_rect.right() {
            let cell = buf.get_mut(column, row);
            cell.style = cell
                .style
                .fg(theme.active_tab_text)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD);
        }
    }
    let center_x = frame.x.saturating_add(frame.width / 2);
    let center_y = frame.y.saturating_add(frame.height / 2);
    for (zone, symbol, column, row) in [
        (SurfaceDropZone::Top, "^", center_x, frame.y.saturating_add(1)),
        (
            SurfaceDropZone::Bottom,
            "v",
            center_x,
            frame.bottom().saturating_sub(2),
        ),
        (SurfaceDropZone::Left, "<", frame.x.saturating_add(1), center_y),
        (
            SurfaceDropZone::Right,
            ">",
            frame.right().saturating_sub(2),
            center_y,
        ),
        (SurfaceDropZone::Center, "+", center_x, center_y),
    ] {
        if frame.contains(column, row) {
            let cell = buf.get_mut(column, row);
            cell.symbol = symbol.into();
            cell.style = Style::default()
                .fg(if zone == active { theme.active_tab_text } else { theme.accent })
                .bg(if zone == active { theme.accent } else { theme.surface })
                .add_modifier(Modifier::BOLD);
        }
    }
}

fn surface_zone_rect(frame: Rect, zone: SurfaceDropZone) -> Rect {
    let edge_width = (frame.width / 4).max(1);
    let edge_height = (frame.height / 4).max(1);
    match zone {
        SurfaceDropZone::Top => Rect::new(frame.x, frame.y, frame.width, edge_height),
        SurfaceDropZone::Bottom => Rect::new(
            frame.x,
            frame.bottom().saturating_sub(edge_height),
            frame.width,
            edge_height,
        ),
        SurfaceDropZone::Left => Rect::new(frame.x, frame.y, edge_width, frame.height),
        SurfaceDropZone::Right => Rect::new(
            frame.right().saturating_sub(edge_width),
            frame.y,
            edge_width,
            frame.height,
        ),
        SurfaceDropZone::Center => Rect::new(
            frame.x.saturating_add(edge_width),
            frame.y.saturating_add(edge_height),
            frame.width.saturating_sub(edge_width.saturating_mul(2)),
            frame.height.saturating_sub(edge_height.saturating_mul(2)),
        ),
    }
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
    relative_path: &gate4agent_node_protocol::RepositoryPath,
    kind: WorkspaceEntryKind,
    git: &GitSnapshot,
) -> Option<bool> {
    git.status
        .iter()
        .filter(|entry| {
            match kind {
                WorkspaceEntryKind::Directory => {
                    entry.path == *relative_path
                        || entry.path.is_descendant_of(relative_path)
                }
                WorkspaceEntryKind::File => entry.path == *relative_path,
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

fn wrap_preview_text(value: &str, max_cells: usize) -> Vec<String> {
    let max_cells = max_cells.max(1);
    let display_safe = value
        .chars()
        .map(|character| {
            if matches!(
                character as u32,
                0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
            ) {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect::<String>();
    let mut lines = Vec::new();
    let mut fenced = false;
    for source_line in display_safe.split('\n') {
        if source_line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let code_line = fenced
            || source_line
                .chars()
                .next()
                .is_some_and(char::is_whitespace);
        let mut remaining = source_line;
        while !remaining.is_empty() {
            let hard_prefix = take_prefix_cells(remaining, max_cells);
            let prefix = if !code_line && hard_prefix.len() < remaining.len() {
                if remaining[hard_prefix.len()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
                {
                    hard_prefix.trim_end()
                } else {
                    hard_prefix
                        .char_indices()
                        .rev()
                        .find_map(|(index, character)| {
                            (index > 0 && character.is_whitespace())
                                .then_some(hard_prefix[..index].trim_end())
                        })
                        .filter(|prefix| !prefix.is_empty())
                        .unwrap_or(hard_prefix.as_str())
                }
            } else {
                hard_prefix.as_str()
            };
            if prefix.is_empty() {
                break;
            }
            remaining = &remaining[prefix.len()..];
            if !code_line {
                remaining = remaining.trim_start_matches(char::is_whitespace);
            }
            lines.push(prefix.to_owned());
        }
        if source_line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
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
    use gate4agent_harness_client::{
        HarnessExecutionModeV1, HarnessRevision, HarnessRunId, HarnessRunLifecycleV1,
        HarnessTaskId, HarnessTaskStateV1, RedactedBindingStateV1,
        RedactedRunIntentV1, RedactedRunV1, RedactedTaskV1,
        RedactedWorktreeIntentV1, TaskCreatorCategoryV1,
    };
    use gate4agent_node_protocol::{
        AgentProgressCurrentV1, AgentProgressUsageV1, AgentProgressV1,
        ContextPackLineageReceipt, GitCommitSummary, GitStatusEntry, GitWorktreeSnapshot,
        HistoryCandidateSummary, HostDirectoryEntry, ManagedSessionState, ManagedWorktreeLeaseId,
        ManagedWorktreeProfileSummary, ManagedWorktreeRetention, ManagedWorktreeSpawnReceipt,
        NodeId, NodeIncarnationId,
        ResolvedBundleReceipt, ResolvedContextPackReceipt, ResolvedSpawnReceipt,
        SessionAddress as NodeSessionAddress, SessionKey, SessionMode, SpawnBundleDigest,
        SpawnBundleId, SpawnBundleRevision, SpawnContextDigest, SpawnContextId, SpawnDeadlineMs,
        SpawnFieldProvenance, SpawnIdempotencyKey, SpawnProfileId, SpawnProfileRevision,
        SpawnPromptMetadata, SpawnRequiredCapabilities, SpawnResolutionProvenance, SpawnTarget,
        WorkspaceEntry, WorkspaceId, WorktreeProfileId, WorktreeProfileInventory,
        WorktreeProfileRevision,
        NODE_INCARNATION_ID_BYTES,
    };
    use gate4agent_types::{
        AgentId, AgentInstanceId, ProviderActivity, SessionGeneration,
        TerminalMouseProtocolEncoding, TerminalSize,
    };
    use crate::app::{
        AgentMenuAction, AgentMenuState, CreateWorkspaceEntryDialog, CreateWorktreeDialog,
        DragSource, FolderBrowserDialog, HistoryDialog, LoadedHistoryView, ManagedSessionView,
        NativeSessionMenuAction, NativeSessionMenuState, NodeView, Provider, ProviderInventory,
        SessionAddress, SessionView, SpawnDialog, WorkspaceView,
    };

    fn host_path(value: impl Into<String>) -> gate4agent_node_protocol::OpaqueHostPath {
        gate4agent_node_protocol::OpaqueHostPath::utf8(value.into()).unwrap()
    }

    fn repository_path(value: impl Into<String>) -> gate4agent_node_protocol::RepositoryPath {
        gate4agent_node_protocol::RepositoryPath::utf8(value.into()).unwrap()
    }

    fn provider(value: &str) -> Provider {
        AgentId::new(value).unwrap()
    }

    fn buffer_text(buf: &TerminalBuffer) -> String {
        (0..buf.height())
            .map(|row| {
                (0..buf.width())
                    .map(|column| buf.get(column, row).symbol.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn harness_kanban_renders_exact_state_columns_and_stale_snapshot_banner() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.harness_kanban.enabled = true;
        app.agent_board_mode = crate::app::AgentBoardMode::HarnessKanban;
        app.surface.open_in_focused(SurfaceTab::AgentBoard);
        let task = RedactedTaskV1 {
            task_id: HarnessTaskId::new("htask_111111111111111111111111").unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            title: "authoritative task".to_owned(),
            body: "body".to_owned(),
            creator: TaskCreatorCategoryV1::User,
            parent_task_id: None,
            dependency_ids: Vec::new(),
            state: HarnessTaskStateV1::Backlog,
            run_ids: Vec::new(),
            references_redacted: false,
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        app.begin_harness_refresh(1);
        app.apply_harness_snapshot(1, vec![task.clone()], Vec::new());
        app.begin_harness_refresh(2);
        app.fail_harness_refresh(2, "host restarting".to_owned());

        let mut buf = TerminalBuffer::new(260, 28);
        let layout = render(&app, &mut buf);
        let text = buffer_text(&buf);
        for label in ["Backlog", "Ready", "Running", "Waiting", "Review", "Done", "Failed", "Cancelled"] {
            assert!(text.contains(label), "missing Harness column {label}");
        }
        assert!(text.contains("STALE | last complete Harness snapshot retained"));
        for label in [
            "[Tasks]",
            "[Runtime]",
            "[New task]",
            "[Refresh]",
            "[Run next Ready]",
            "[Move to Ready]",
        ] {
            assert!(text.contains(label), "missing mouse-first Harness control {label}: {text}");
        }
        assert_eq!(text.matches("[Tasks]").count(), 1, "{text}");
        assert_eq!(text.matches("[Runtime]").count(), 1, "{text}");
        let focused_pane = layout.surface_panes.iter()
            .find(|pane| pane.pane_id == app.surface.focused)
            .unwrap();
        assert_eq!(focused_pane.viewport.y, focused_pane.header.bottom());
        assert!(!text.contains("[< columns]"), "{text}");
        assert!(!text.contains("[columns >]"), "{text}");
        assert!(layout.hits.iter().any(|hit| {
            hit.target == HitTarget::HarnessTaskCard(task.task_id.clone())
        }));
        for mode in [
            crate::app::AgentBoardMode::HarnessKanban,
            crate::app::AgentBoardMode::SessionMonitoring,
        ] {
            assert!(layout.hits.iter().any(|hit| {
                hit.target == HitTarget::HarnessBoardMode(mode)
            }), "missing mode-tab hit {mode:?}");
        }
        assert!(!layout.hits.iter().any(|hit| {
            matches!(hit.target, HitTarget::AgentBoardCardRun(_))
        }));

        app.harness_kanban.monitor = Some(crate::app::HarnessRunMonitorView {
            run: RedactedRunV1 {
                run_id: HarnessRunId::new("hrun_222222222222222222222222").unwrap(),
                revision: HarnessRevision::new(1).unwrap(),
                parent_run_id: None,
                task_id: Some(task.task_id),
                operation_id: None,
                intent: RedactedRunIntentV1 {
                    mode: HarnessExecutionModeV1::Pty,
                    worktree: RedactedWorktreeIntentV1::Existing,
                    has_delivery_bundle: false,
                    has_continuation: false,
                },
                lifecycle: HarnessRunLifecycleV1::Running,
                binding: RedactedBindingStateV1::ManagedActive,
                result_disposition: None,
                failure_category: None,
                references_redacted: false,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
            },
            monitor: None,
            timeline: Vec::new(),
            loading: true,
            stale_reason: None,
        });
        let mut details_buf = TerminalBuffer::new(260, 28);
        render(&app, &mut details_buf);
        let details_text = buffer_text(&details_buf);
        assert!(details_text.contains("Harness run details"), "{details_text}");
        assert!(details_text.contains("loading authoritative run details"), "{details_text}");
        assert!(!details_text.contains("Harness monitor"), "{details_text}");
        assert_eq!(app.surface.active_tab(), Some(&SurfaceTab::AgentBoard));
        let tabs = app.surface.all_tabs();
        assert_eq!(tabs.len(), 2);
        assert!(matches!(tabs.first(), Some(SurfaceTab::Pty(_))));
        assert!(matches!(tabs.last(), Some(SurfaceTab::AgentBoard)));
    }

    #[test]
    fn harness_kanban_empty_loading_and_error_keep_columns_and_mouse_actions_visible() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.harness_kanban.enabled = true;
        app.agent_board_mode = crate::app::AgentBoardMode::HarnessKanban;

        let mut empty = TerminalBuffer::new(72, 14);
        let mut empty_layout = LayoutRects::default();
        render_harness_kanban(
            &app,
            Rect::new(0, 0, 72, 14),
            &mut empty,
            &mut empty_layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        let empty_text = buffer_text(&empty);
        assert!(empty_text.contains("Backlog (0)"), "{empty_text}");
        assert!(empty_text.contains("Ready (0)"), "{empty_text}");
        assert!(!empty_text.contains("[< columns]"), "{empty_text}");
        assert!(!empty_text.contains("[columns >]"), "{empty_text}");
        assert_eq!(empty.get(0, 0).symbol, "┌");
        assert_eq!(empty.get(71, 0).symbol, "┐");
        assert_eq!(empty.get(0, 13).symbol, "└");
        assert_eq!(empty.get(71, 13).symbol, "┘");
        assert_eq!(empty.get(24, 0).symbol, "─", "table has one outer top border");
        assert_eq!(empty.get(0, 2).symbol, "├");
        assert_eq!(empty.get(1, 2).symbol, "─");
        assert_eq!(empty.get(24, 3).symbol, "│");
        assert_eq!(empty.get(24, 4).symbol, "┼");
        assert_eq!(empty.get(24, 5).symbol, "│");
        assert_eq!(empty.get(24, 12).symbol, "│");
        assert_eq!(empty.get(24, 13).symbol, "┴");
        assert_eq!(empty.get(48, 12).symbol, "│");
        assert!(!empty_layout.hits.iter().any(|hit| {
            matches!(hit.target, HitTarget::HarnessColumnScroll(_))
        }));

        app.begin_harness_refresh(41);

        let mut loading = TerminalBuffer::new(72, 14);
        let mut loading_layout = LayoutRects::default();
        render_harness_kanban(
            &app,
            Rect::new(0, 0, 72, 14),
            &mut loading,
            &mut loading_layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        let loading_text = buffer_text(&loading);
        for label in [
            "[New task]",
            "[Refresh]",
            "[Run next Ready]",
            "Backlog (0)",
            "Ready (0)",
        ] {
            assert!(loading_text.contains(label), "missing narrow Harness UI label {label}: {loading_text}");
        }
        assert!(loading_text.contains("Refreshing authoritative Harness snapshot..."));
        for target in [
            HitTarget::HarnessTaskCreate,
            HitTarget::HarnessTaskRefresh,
            HitTarget::HarnessScheduleNext,
        ] {
            assert!(loading_layout.hits.iter().any(|hit| hit.target == target), "missing hit {target:?}");
        }
        assert!(!loading_layout.hits.iter().any(|hit| {
            matches!(hit.target, HitTarget::HarnessColumnScroll(_))
        }));

        app.fail_harness_refresh(41, "authoritative host unavailable".to_owned());
        let mut failed = TerminalBuffer::new(72, 14);
        let mut failed_layout = LayoutRects::default();
        render_harness_kanban(
            &app,
            Rect::new(0, 0, 72, 14),
            &mut failed,
            &mut failed_layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        let failed_text = buffer_text(&failed);
        assert!(failed_text.contains("STALE | last complete Harness snapshot retained"), "{failed_text}");
        assert!(failed_text.contains("Backlog (0)"), "{failed_text}");
        assert!(failed_text.contains("Ready (0)"), "{failed_text}");
    }

    #[test]
    fn harness_kanban_header_chevrons_are_hover_only_exact_and_bounded() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.harness_kanban.enabled = true;
        app.agent_board_mode = crate::app::AgentBoardMode::HarnessKanban;

        let render_board = |app: &App| {
            let mut buffer = TerminalBuffer::new(72, 10);
            let mut layout = LayoutRects::default();
            render_harness_kanban(
                app,
                Rect::new(0, 0, 72, 10),
                &mut buffer,
                &mut layout,
                Theme::for_mode(PtyColorMode::Inherited),
            );
            (buffer_text(&buffer), layout)
        };

        let (first, layout) = render_board(&app);
        assert!(first.contains("Backlog (0)"), "{first}");
        assert!(first.contains("Ready (0)"), "{first}");
        assert!(!first.contains('‹'), "{first}");
        assert!(!first.contains('›'), "{first}");
        assert!(!layout.hits.iter().any(|hit| {
            matches!(hit.target, HitTarget::HarnessColumnScroll(_))
        }));

        assert_eq!(app.hover(71, 5), crate::app::AppAction::None);
        let (_, layout) = render_board(&app);
        assert!(!layout.hits.iter().any(|hit| {
            matches!(hit.target, HitTarget::HarnessColumnScroll(_))
        }), "body/side hover must not create navigation hits");

        assert_eq!(app.hover(40, 1), crate::app::AppAction::None);
        let (right_text, layout) = render_board(&app);
        assert!(!right_text.contains('‹'), "left is unavailable at the start: {right_text}");
        assert!(right_text.contains('›'), "{right_text}");
        let right = layout.hits.iter()
            .find(|hit| hit.target == HitTarget::HarnessColumnScroll(1))
            .unwrap().rect;
        assert_eq!(right, Rect::new(70, 1, 1, 1));
        app.layout = layout;
        assert_eq!(app.click(right.x, right.y), crate::app::AppAction::None);
        assert_eq!(app.harness_kanban.column_scroll, 1);

        assert_eq!(app.hover(40, 1), crate::app::AppAction::None);
        let (second, layout) = render_board(&app);
        assert!(second.contains('‹'), "{second}");
        assert!(second.contains('›'), "{second}");
        assert!(!second.contains("Backlog (0)"), "{second}");
        assert!(second.contains("Ready (0)"), "{second}");
        assert!(second.contains("Waiting (0)"), "{second}");
        let left = layout.hits.iter()
            .find(|hit| hit.target == HitTarget::HarnessColumnScroll(0))
            .unwrap().rect;
        app.layout = layout;
        assert_eq!(left, Rect::new(1, 1, 1, 1));
        assert_eq!(app.click(left.x, left.y), crate::app::AppAction::None);
        assert_eq!(app.harness_kanban.column_scroll, 0);

        let (third, layout) = render_board(&app);
        assert!(third.contains("Backlog (0)"), "{third}");
        assert!(third.contains("Ready (0)"), "{third}");
        assert!(!layout.hits.iter().any(|hit| {
            hit.target == HitTarget::HarnessColumnScroll(0)
        }), "left must not have a hit at the start");
        assert!(layout.hits.iter().any(|hit| {
            hit.target == HitTarget::HarnessColumnScroll(1)
        }), "right must remain available at the start");
        assert!(!third.contains('‹'), "left must be unavailable at the start: {third}");

        app.harness_kanban.column_scroll = 1;
        assert_eq!(app.hover(40, 1), crate::app::AppAction::None);
        let (_, layout) = render_board(&app);
        assert!(layout.hits.iter().any(|hit| {
            hit.target == HitTarget::HarnessColumnScroll(0)
        }));
        assert_eq!(app.hover(40, 5), crate::app::AppAction::None);
        let (_, layout) = render_board(&app);
        assert!(!layout.hits.iter().any(|hit| {
            matches!(hit.target, HitTarget::HarnessColumnScroll(_))
        }), "leaving the board must hide both edge overlays");

        app.harness_kanban.column_scroll = 5;
        assert_eq!(app.hover(40, 1), crate::app::AppAction::None);
        let (end, layout) = render_board(&app);
        assert!(end.contains('‹'), "{end}");
        assert!(!end.contains('›'), "right must be unavailable at the end: {end}");
        assert!(layout.hits.iter().any(|hit| {
            hit.target == HitTarget::HarnessColumnScroll(4)
        }));
        assert!(!layout.hits.iter().any(|hit| {
            hit.target == HitTarget::HarnessColumnScroll(6)
        }));
    }

    #[test]
    fn harness_composer_modal_has_unicode_frame_and_contained_mouse_targets() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.harness_kanban.enabled = true;
        app.agent_board_mode = crate::app::AgentBoardMode::HarnessKanban;
        app.harness_kanban.composer = Some(crate::app::HarnessTaskComposer::default());
        let mut buffer = TerminalBuffer::new(72, 14);
        let mut layout = LayoutRects::default();
        render_harness_kanban(
            &app,
            Rect::new(0, 0, 72, 14),
            &mut buffer,
            &mut layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        let text = buffer_text(&buffer);
        assert!(text.contains("Title:"), "{text}");
        assert!(text.contains("Body:"), "{text}");
        assert!(text.contains("[Create in Backlog]"), "{text}");
        assert!(text.contains("[Cancel]"), "{text}");
        let modal = Rect::new(0, 3, 72, 8);
        assert_eq!(buffer.get(modal.x, modal.y).symbol, "┌");
        assert_eq!(buffer.get(modal.right() - 1, modal.y).symbol, "┐");
        assert_eq!(buffer.get(modal.x, modal.bottom() - 1).symbol, "└");
        assert_eq!(buffer.get(modal.right() - 1, modal.bottom() - 1).symbol, "┘");
        assert_eq!(buffer.get(modal.x, modal.y + 2).symbol, "├");
        assert_eq!(buffer.get(modal.x + 1, modal.y + 2).symbol, "─");
        assert_eq!(buffer.get(modal.right() - 1, modal.y + 2).symbol, "┤");
        assert_eq!(buffer.get(24, modal.y + 5).symbol, " ", "table separator leaked through modal");
        for target in [
            HitTarget::HarnessComposerField(crate::app::HarnessTaskComposerField::Title),
            HitTarget::HarnessComposerField(crate::app::HarnessTaskComposerField::Body),
            HitTarget::HarnessComposerCreate,
            HitTarget::HarnessComposerCancel,
        ] {
            let hit = layout.hits.iter().find(|hit| hit.target == target)
                .unwrap_or_else(|| panic!("missing hit {target:?}"));
            assert!(hit.rect.x > modal.x, "{target:?}: {:?}", hit.rect);
            assert!(hit.rect.right() < modal.right(), "{target:?}: {:?}", hit.rect);
            assert!(hit.rect.y > modal.y, "{target:?}: {:?}", hit.rect);
            assert!(hit.rect.bottom() < modal.bottom(), "{target:?}: {:?}", hit.rect);
        }
    }

    fn context_receipt() -> ResolvedContextPackReceipt {
        ResolvedContextPackReceipt {
            id: SpawnContextId::new("context-a").unwrap(),
            digest: SpawnContextDigest::new(format!("sha256:{}", "b".repeat(64))).unwrap(),
            lineage: ContextPackLineageReceipt {
                source_node_id: NodeId::new("node-source").unwrap(),
                source_session: NodeSessionAddress {
                    workspace_id: WorkspaceId::new("workspace-source").unwrap(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(7),
                        generation: SessionGeneration(3),
                    },
                },
                source_provider: provider("codex"),
            },
            source_message_count: 9,
            retained_message_count: 7,
            byte_len: 512,
            truncated: true,
        }
    }

    fn spawn_receipt() -> ResolvedSpawnReceipt {
        let bundle = ResolvedBundleReceipt {
            id: SpawnBundleId::new("bundle-a").unwrap(),
            revision: SpawnBundleRevision::new("bundle-r3").unwrap(),
            digest: SpawnBundleDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
        };
        let context = context_receipt();
        ResolvedSpawnReceipt {
            incarnation_id: NodeIncarnationId::from_bytes([9; NODE_INCARNATION_ID_BYTES]),
            session: NodeSessionAddress {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(11),
                    generation: SessionGeneration(2),
                },
            },
            target: SpawnTarget {
                node_id: NodeId::new("node-a").unwrap(),
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                worktree_id: None,
            },
            profile_id: SpawnProfileId::new("review").unwrap(),
            profile_revision: SpawnProfileRevision::new("review-r4").unwrap(),
            provider: provider("codex"),
            mode: SessionMode::Inline,
            terminal_size: TerminalSize { rows: 24, columns: 100 },
            prompt: SpawnPromptMetadata { present: true, byte_len: 18 },
            bundle_id: Some(bundle.id.clone()),
            bundle: Some(bundle),
            context_id: Some(context.id.clone()),
            context: Some(context),
            environment_profile: None,
            deadline_ms: SpawnDeadlineMs::new(5_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("launch-render-test").unwrap(),
            required_capabilities: SpawnRequiredCapabilities::default(),
            provenance: SpawnResolutionProvenance {
                provider: SpawnFieldProvenance::Profile,
                mode: SpawnFieldProvenance::Profile,
                terminal_size: SpawnFieldProvenance::Profile,
                prompt: SpawnFieldProvenance::Profile,
                bundle_id: SpawnFieldProvenance::Profile,
                context_id: SpawnFieldProvenance::Profile,
                environment_profile_id: SpawnFieldProvenance::Profile,
            },
            harness_mcp_proxy: None,
        }
    }

    fn managed_lease(state: ManagedWorktreeLeaseState) -> ManagedWorktreeLeaseSnapshot {
        ManagedWorktreeLeaseSnapshot {
            lease_id: ManagedWorktreeLeaseId::new("lease-render").unwrap(),
            source_workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            workspace_id: WorkspaceId::new("workspace-managed").unwrap(),
            profile_id: WorktreeProfileId::new("isolated").unwrap(),
            profile_revision: WorktreeProfileRevision::new("isolated-r2").unwrap(),
            retention: ManagedWorktreeRetention::RemoveWhenReleased,
            state,
            active_session_count: 0,
            managed_record_count: 0,
            cleanup_failure: Some(ManagedWorktreeCleanupFailure::Dirty),
            created_at_unix_ms: 1,
            updated_at_unix_ms: 2,
        }
    }

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
            incarnation_id: None,
            endpoint: "pipe".to_owned(),
            relay_route: gate4agent_c2_protocol::C2RelayRoute::Unknown,
            connection: ConnectionState::Connected,
            controller_owned: true,
            event_sequence: 1,
            session_records: Vec::new(),
            launch_inventory: None,
            providers: vec![ProviderInventory { provider: provider("kimi"), enabled: true }],
            workspaces: vec![WorkspaceView {
                workspace_id: "workspace-a".to_owned(),
                label: "nemo".to_owned(),
                canonical_root: host_path(r"C:\work\nemo"),
                providers: vec![ProviderInventory { provider: provider("kimi"), enabled: true }],
                worktree_service_mode: Some(gate4agent_node_protocol::WorktreeServiceMode::Manual),
                managed_worktree_profiles: None,
                sessions: vec![SessionView {
                    address: address.clone(),
                    provider: provider("kimi"),
                    status: "running".to_owned(),
                    running: true,
                    stoppable: true,
                    removable: false,
                    restartable: false,
                    attention: false,
                    has_provider_session_identity: true,
                    progress: None,
                    terminal_formatted: b"\x1b[38;2;80;160;255;48;2;0;51;102mK".to_vec(),
                    terminal_scrollback: Vec::new(),
                    terminal_alternate_screen: false,
                    terminal_mouse_protocol_enabled: false,
                    terminal_mouse_protocol_encoding: TerminalMouseProtocolEncoding::Default,
                    terminal_cursor: Some((0, 1)),
                }],
            }],
        });
        app.surface.open_in_focused(SurfaceTab::Pty(address));
        app
    }

    fn active_pty_address(app: &App) -> SessionAddress {
        app.surface
            .active_tab()
            .and_then(SurfaceTab::pty_address)
            .expect("fixture has an active PTY tab")
            .clone()
    }

    fn test_agent_progress() -> AgentProgressV1 {
        AgentProgressV1 {
            provider_sequence: 9,
            activity: ProviderActivity::Working,
            completed_turns: 4,
            usage: Some(AgentProgressUsageV1 {
                input_tokens: 12,
                output_tokens: 6,
                cache_read_tokens: 3,
                cache_write_tokens: 2,
                reasoning_tokens: 7,
            }),
            current: AgentProgressCurrentV1::Working,
            active_tool_labels: vec!["shell".to_owned()],
            active_tool_count: 1,
            attention: None,
            subagent_count: 2,
            last_event_kind: None,
            gap_count: 0,
            stale: false,
            truncated: false,
        }
    }

    fn exact_context_snapshot(context_window: Option<u64>) -> ContextOccupancySnapshot {
        ContextOccupancySnapshot {
            uncached_input_tokens: 10,
            output_tokens: 10,
            cache_read_tokens: 20,
            cache_write_tokens: 10,
            reasoning_tokens: None,
            unattributed_tokens: 10,
            used_tokens: 60,
            context_window,
            evidence: gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider,
            provenance: ContextOccupancyProvenance::ExactCurrentWindow,
        }
    }

    #[test]
    fn exact_context_bar_has_bounded_segment_widths_colors_hits_and_tooltip() {
        let theme = Theme::for_mode(PtyColorMode::Inherited);
        let snapshot = exact_context_snapshot(Some(100));
        let segments = context_usage_bar_segments(snapshot, 100, 50, theme);
        assert_eq!(
            segments.iter().map(|segment| segment.width).collect::<Vec<_>>(),
            vec![5, 10, 5, 5, 5, 20],
        );
        assert_eq!(
            segments.iter().map(|segment| segment.color).collect::<Vec<_>>(),
            vec![theme.accent, theme.teal, theme.yellow, theme.green, theme.red, theme.border],
        );

        let mut buffer = TerminalBuffer::new(120, 8);
        let mut layout = LayoutRects::default();
        render_context_usage_bar(
            snapshot,
            100,
            Rect::new(0, 2, 50, 1),
            &mut buffer,
            &mut layout,
            theme,
        );
        assert_eq!(layout.hits.len(), 6);
        assert_eq!(
            layout.hits.iter().map(|hit| hit.rect.width).collect::<Vec<_>>(),
            vec![5, 10, 5, 5, 5, 20],
        );
        for (region, segment) in layout.hits.iter().zip(segments.iter()) {
            assert_eq!(buffer.get(region.rect.x, region.rect.y).style.bg, segment.color);
            assert!(matches!(
                &region.target,
                HitTarget::ContextUsageSegment(hit) if *hit == segment.hit
            ));
        }

        let cache_read = layout.hits[1].clone();
        let HitTarget::ContextUsageSegment(hit) = cache_read.target else {
            panic!("expected context segment hit");
        };
        render_context_usage_tooltip(
            Some(ContextUsageHover {
                column: cache_read.rect.x,
                row: cache_read.rect.y,
                hit,
            }),
            Rect::new(0, 0, 120, 8),
            &mut buffer,
            &layout,
            theme,
        );
        let text = buffer_text(&buffer);
        assert!(text.contains("Cache read: 20 tokens | share 20/100 (20.00%)"));
        assert!(text.contains("Current / Live | source structured provider"));
        assert!(text.contains("Formula: used = uncached input + cache read"));
        assert!(!text.contains("prompt"));
        assert!(!text.contains("transcript"));
        assert!(!text.contains("path"));
        assert!(!text.contains("tool"));
    }

    #[test]
    fn exact_context_bar_clamps_over_capacity_and_unknown_window_is_unavailable() {
        let theme = Theme::for_mode(PtyColorMode::Inherited);
        let mut over = exact_context_snapshot(Some(20));
        over.used_tokens = 60;
        let segments = context_usage_bar_segments(over, 20, 40, theme);
        assert_eq!(
            segments.iter().map(|segment| segment.width as usize).sum::<usize>(),
            40,
        );
        assert_eq!(
            exact_context_window(exact_context_snapshot(None)),
            Err("context window was not reported by this source"),
        );
        assert_eq!(
            exact_context_window(ContextOccupancySnapshot {
                context_window: Some(0),
                ..exact_context_snapshot(Some(100))
            }),
            Err("reported context window is zero"),
        );
    }

    fn agent_board_render_fixture() -> App {
        let template = fixture(PtyColorMode::Inherited).nodes.remove(0);
        let mut app = App::default();
        let specs = [
            ("attention", ManagedSessionState::Live, ConnectionState::Connected, Some(AgentProgressCurrentV1::WaitingForInput), false, 0),
            ("working", ManagedSessionState::Live, ConnectionState::Connected, Some(AgentProgressCurrentV1::Working), true, 0),
            ("idle", ManagedSessionState::Live, ConnectionState::Connected, Some(AgentProgressCurrentV1::Idle), false, 0),
            ("dormant", ManagedSessionState::Dormant, ConnectionState::Connected, None, false, 0),
            ("stale", ManagedSessionState::Live, ConnectionState::Connected, Some(AgentProgressCurrentV1::Working), false, 0),
            ("gap", ManagedSessionState::Live, ConnectionState::Connected, Some(AgentProgressCurrentV1::Working), false, 3),
            ("offline", ManagedSessionState::Live, ConnectionState::Disconnected("PRIVATE CONNECTION ERROR".to_owned()), Some(AgentProgressCurrentV1::Working), false, 0),
        ];
        for (index, (name, state, connection, current, truncated, gap_count)) in specs.into_iter().enumerate() {
            let mut node = template.clone();
            node.node_id = format!("node-{name}");
            node.connection = connection;
            node.workspaces[0].workspace_id = format!("workspace-{name}");
            let address = if state == ManagedSessionState::Dormant {
                node.workspaces[0].sessions.clear();
                None
            } else {
                let workspace_id = node.workspaces[0].workspace_id.clone();
                let session = &mut node.workspaces[0].sessions[0];
                session.address.node_id = node.node_id.clone();
                session.address.workspace_id = workspace_id;
                session.address.instance_id = index as u64 + 1;
                session.attention = false;
                session.progress = current.map(|current| {
                    let mut progress = test_agent_progress();
                    progress.current = current;
                    progress.activity = match current {
                        AgentProgressCurrentV1::Idle => ProviderActivity::Idle,
                        AgentProgressCurrentV1::Working => ProviderActivity::Working,
                        AgentProgressCurrentV1::WaitingForInput => ProviderActivity::WaitingForInput,
                        AgentProgressCurrentV1::Blocked => ProviderActivity::Blocked,
                    };
                    progress.stale = name == "stale";
                    progress.gap_count = gap_count;
                    progress.truncated = truncated;
                    progress
                });
                Some(session.address.clone())
            };
            node.session_records = vec![ManagedSessionView {
                node_id: node.node_id.clone(),
                record_id: format!("record-{name}"),
                display_name: format!("{name} agent"),
                provider: provider("codex"),
                mode: SessionMode::Pty,
                state,
                workspace_id: node.workspaces[0].workspace_id.clone(),
                canonical_root: None,
                has_provider_session_identity: true,
                bundle: None,
                context_id: None,
                context: None,
                task_binding: None,
                active_session: address,
                last_error: Some("PRIVATE RAW ERROR".to_owned()),
            }];
            app.nodes.push(node);
        }
        app.agent_board.selected = app.agent_rows().first().cloned();
        app
    }

    #[test]
    fn agent_board_render_responsive_columns_text_and_hits_are_clipped() {
        let app = agent_board_render_fixture();
        for (width, expected_headers) in [(23, 1), (72, 3), (144, 6)] {
            let mut buffer = TerminalBuffer::new(width, 36);
            let mut layout = LayoutRects::default();
            let area = Rect::new(0, 0, width, 36);
            render_agent_board(
                &app,
                area,
                &mut buffer,
                &mut layout,
                Theme::for_mode(PtyColorMode::Inherited),
            );
            let text = buffer_text(&buffer);
            let headers = AgentBoardColumn::ALL
                .iter()
                .filter(|column| text.contains(column.label()))
                .count();
            assert_eq!(headers, expected_headers, "{width}: {text}");
            assert!(layout.hits.iter().filter(|hit| matches!(
                hit.target,
                HitTarget::AgentBoardCard(_)
                    | HitTarget::AgentBoardCardOpen(_)
                    | HitTarget::AgentBoardCardRun(_)
                    | HitTarget::AgentBoardCardProgress(_)
            )).all(|hit| {
                hit.rect.x >= area.x
                    && hit.rect.y >= area.y
                    && hit.rect.right() <= area.right()
                    && hit.rect.bottom() <= area.bottom()
            }));
        }

        let mut buffer = TerminalBuffer::new(144, 36);
        let mut layout = LayoutRects::default();
        render_agent_board(
            &app,
            Rect::new(0, 0, 144, 36),
            &mut buffer,
            &mut layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        let text = buffer_text(&buffer);
        assert!(text.contains("stale"), "{text}");
        assert!(text.contains("event gap 3"), "{text}");
        assert!(text.contains("status: offline"), "{text}");
        assert!(text.contains("partial"), "{text}");
        assert!(text.contains("[workspace]"), "{text}");
        assert!(text.contains("[monitor]"), "{text}");
        assert!(text.contains("No sessions observed") || AgentBoardColumn::ALL.iter().all(|column| {
            app.agent_board_cards().iter().any(|card| card.column == *column)
        }));
        assert!(!text.contains("PRIVATE"), "{text}");
    }

    #[test]
    fn activity_board_task_filter_renders_exact_header_and_never_zero_unassigned() {
        let mut app = agent_board_render_fixture();
        let task_id: gate4agent_node_protocol::TaskId =
            "task-0123456789abcdef01234567".parse().unwrap();
        for node in app.nodes.iter_mut().take(2) {
            node.session_records[0].task_binding = Some(
                gate4agent_node_protocol::SessionTaskBindingV1 {
                    revision: 1,
                    task_id: Some(task_id.clone()),
                    changed_at_unix_ms: 1,
                },
            );
        }
        app.agent_board.task_filter = Some(task_id.clone());
        let mut buffer = TerminalBuffer::new(120, 30);
        let mut layout = LayoutRects::default();
        render_agent_board(
            &app,
            Rect::new(0, 0, 120, 30),
            &mut buffer,
            &mut layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        let text = buffer_text(&buffer);
        assert!(text.contains(&format!("Task {task_id} · 2 observed runs")), "{text}");
        assert!(!text.contains("idle agent"), "{text}");

        for node_index in 0..2 {
            let mut authoritative = app.nodes[node_index].session_records[0].clone();
            authoritative.task_binding = None;
            app.upsert_managed_session(authoritative);
        }
        assert!(app.agent_board.task_filter.is_none());
        let mut buffer = TerminalBuffer::new(120, 30);
        let mut layout = LayoutRects::default();
        render_agent_board(
            &app,
            Rect::new(0, 0, 120, 30),
            &mut buffer,
            &mut layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        assert!(!buffer_text(&buffer).contains("0 observed runs"));
    }

    #[test]
    fn agent_board_render_all_empty_is_explicit() {
        let app = App::default();
        let mut buffer = TerminalBuffer::new(72, 12);
        let mut layout = LayoutRects::default();
        render_agent_board(
            &app,
            Rect::new(0, 0, 72, 12),
            &mut buffer,
            &mut layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        let text = buffer_text(&buffer);
        assert!(text.contains("No managed or live agents"), "{text}");
        assert!(text.contains("Provider history is r"), "{text}");
        assert!(layout.hits.is_empty());
    }

    #[test]
    fn agent_board_progress_summary_is_fail_closed_for_nonfresh_cards() {
        let mut app = agent_board_render_fixture();
        let retained = app
            .nodes
            .iter()
            .find(|node| node.node_id == "node-working")
            .unwrap()
            .workspaces[0]
            .sessions[0]
            .clone();
        let dormant = app
            .nodes
            .iter_mut()
            .find(|node| node.node_id == "node-dormant")
            .unwrap();
        let mut retained = retained;
        retained.address.node_id = dormant.node_id.clone();
        retained.address.workspace_id = dormant.workspaces[0].workspace_id.clone();
        retained.progress = Some(test_agent_progress());
        dormant.session_records[0].active_session = Some(retained.address.clone());
        dormant.workspaces[0].sessions.push(retained);

        for (record_id, should_show_progress) in [
            ("record-dormant", false),
            ("record-stale", false),
            ("record-gap", false),
            ("record-offline", false),
            ("record-working", true),
        ] {
            let key = AgentRowKey::Managed {
                node_id: format!("node-{}", record_id.trim_start_matches("record-")),
                record_id: record_id.to_owned(),
            };
            let card = app
                .agent_board_cards()
                .into_iter()
                .find(|card| card.key == key)
                .unwrap();
            let mut buffer = TerminalBuffer::new(48, 5);
            let mut layout = LayoutRects::default();
            render_agent_board_card(
                &app,
                &card,
                Rect::new(0, 0, 48, 5),
                &mut buffer,
                &mut layout,
                Theme::for_mode(PtyColorMode::Inherited),
            );
            let text = buffer_text(&buffer);
            assert_eq!(text.contains("turns 4 | tools 1"), should_show_progress, "{record_id}: {text}");
            assert_eq!(text.contains("tokens 12/6"), should_show_progress, "{record_id}: {text}");
        }
    }

    fn empty_existing_session_dialog() -> crate::app::ExistingSessionDialog {
        crate::app::ExistingSessionDialog {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            mode: ExistingSessionMode::Catalog,
            catalog: NativeSessionCatalogState::Ready,
            rows: Vec::new(),
            selected: 0,
            tree_cursor: 0,
            scroll: 0,
            display_name: String::new(),
            session_id: String::new(),
            field: ExistingSessionField::Sessions,
            operation: None,
            operation_error: None,
            request_token: 1,
            pending_routes: std::collections::BTreeSet::new(),
            route_pages: std::collections::BTreeMap::new(),
            catalog_failure: None,
            preview: None,
            preview_scroll: 0,
            preview_request_token: 0,
            preview_record_id: None,
            preview_selection_id: None,
            ask_after_resume: String::new(),
        }
    }

    #[test]
    fn native_history_provider_summary_is_exact_and_does_not_expose_private_paths() {
        let mut app = fixture(PtyColorMode::Inherited);
        let claude_route = crate::app::NativeSessionCatalogRoute::workspace(
            "workspace-a".to_owned(), provider("claude"),
        );
        let codex_route = crate::app::NativeSessionCatalogRoute::workspace(
            "workspace-a".to_owned(), provider("codex"),
        );
        let grok_route = crate::app::NativeSessionCatalogRoute::workspace(
            "workspace-a".to_owned(), provider("grok"),
        );
        let route_page = |recent_remaining_count, older_remaining_count| {
            crate::app::NativeSessionRoutePageState {
                catalog_revision: 7,
                recent_cutoff_unix_ms: 8,
                recent_remaining_count,
                older_remaining_count,
                recent_next_after_selection_id: None,
                older_next_after_selection_id: None,
                recent_has_more: recent_remaining_count > 0,
                older_has_more: older_remaining_count > 0,
                pending: None,
            }
        };
        let private_path = r"C:\private\PRIVATE_PATH_SENTINEL\session.jsonl";
        let mut dialog = empty_existing_session_dialog();
        dialog.rows.push(crate::app::NativeSessionCatalogRowView {
            node_id: "node-a".to_owned(),
            route: grok_route.clone(),
            catalog_revision: 7,
            recent_cutoff_unix_ms: 8,
            selection_id: "grok-selection".to_owned(),
            title: Some(private_path.to_owned()),
            modified_at: None,
            model: None,
            message_count: None,
            completed_turn_count: None,
            external_group: None,
            record_id: None,
        });
        dialog.route_pages.insert(claude_route, route_page(0, 0));
        dialog.route_pages.insert(codex_route, route_page(3, 0));
        dialog.route_pages.insert(grok_route.clone(), route_page(6, 5));
        app.collapsed_native_providers.insert((
            "node-a".to_owned(),
            NativeSessionGroupKey::Workspace("workspace-a".to_owned()),
            grok_route.provider,
        ));
        app.existing_session = Some(dialog);
        let mut buffer = TerminalBuffer::new(80, 16);
        let mut layout = LayoutRects::default();
        render_native_session_list(
            &app,
            Rect::new(0, 0, 80, 16),
            &mut buffer,
            &mut layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );

        let text = buffer_text(&buffer);
        assert!(text.contains("providers claude:0 codex:3 grok:12"), "{text}");
        assert!(!text.contains(private_path), "{text}");
        assert!(!text.contains("PRIVATE_PATH_SENTINEL"), "{text}");
    }

    #[test]
    fn agent_progress_card_collapses_expands_and_clips_on_both_agent_surfaces() {
        let mut app = fixture(PtyColorMode::Inherited);
        let address = active_pty_address(&app);
        app.nodes[0].workspaces[0].sessions[0].progress = Some(test_agent_progress());
        let key = AgentRowKey::Legacy(address.clone());
        let mut buf = TerminalBuffer::new(100, 30);

        let collapsed = render(&app, &mut buf);
        let collapsed_text = buffer_text(&buf);
        assert!(collapsed_text.contains("[d+]"), "{collapsed_text}");
        assert!(!collapsed_text.contains("working | fresh"), "{collapsed_text}");
        let toggle = collapsed.hits.iter().find(|hit| {
            hit.target == HitTarget::AgentProgressToggle(key.clone())
        }).cloned().expect("collapsed agent card exposes progress toggle");
        app.layout = collapsed;
        let tabs_before = app.surface.all_tabs().len();
        assert_eq!(
            app.click(toggle.rect.x, toggle.rect.y),
            crate::app::AppAction::None,
        );
        assert!(app.agent_progress_expanded(&key));
        assert_eq!(app.agent_run_lens_key(), None);
        assert_eq!(app.surface.all_tabs().len(), tabs_before);

        let _ = render(&app, &mut buf);
        let expanded_text = buffer_text(&buf);
        assert!(expanded_text.contains("[d-]"), "{expanded_text}");
        assert!(expanded_text.contains("working | fresh"), "{expanded_text}");
        assert!(expanded_text.contains("tools 1 [shell]"), "{expanded_text}");
        assert!(expanded_text.contains("turns 4"), "{expanded_text}");

        app.existing_session = Some(empty_existing_session_dialog());
        let unified = render(&app, &mut buf);
        let unified_text = buffer_text(&buf);
        assert!(unified_text.contains("[details -]"), "{unified_text}");
        assert!(unified_text.contains("working | fresh"), "{unified_text}");
        assert!(unified.hits.iter().any(|hit| {
            hit.target == HitTarget::AgentProgressToggle(key.clone())
        }));

        let mut clipped_buf = TerminalBuffer::new(60, 14);
        let clipped = render(&app, &mut clipped_buf);
        assert!(clipped.hits.iter().filter(|hit| matches!(
            hit.target,
            HitTarget::Agent(_) | HitTarget::AgentRun(_) | HitTarget::AgentProgressToggle(_)
        )).all(|hit| {
            hit.rect.x >= clipped.agents.x
                && hit.rect.y >= clipped.agents.y
                && hit.rect.right() <= clipped.agents.right()
                && hit.rect.bottom() <= clipped.agents.bottom()
        }));
    }

    #[test]
    fn dormant_managed_agent_never_reuses_live_progress_after_resume() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.nodes[0].workspaces[0].sessions[0].progress = Some(test_agent_progress());
        let key = AgentRowKey::Managed {
            node_id: "node-a".to_owned(),
            record_id: "record-dormant".to_owned(),
        };
        app.nodes[0].session_records.push(ManagedSessionView {
            node_id: "node-a".to_owned(),
            record_id: "record-dormant".to_owned(),
            display_name: "Dormant".to_owned(),
            provider: provider("kimi"),
            mode: SessionMode::Pty,
            state: ManagedSessionState::Dormant,
            workspace_id: "workspace-a".to_owned(),
            canonical_root: None,
            has_provider_session_identity: true,
            bundle: None,
            context_id: None,
            context: None,
            task_binding: None,
            active_session: None,
            last_error: None,
        });

        assert_eq!(
            agent_progress_lines(&app, &key)[0],
            "unavailable until resume",
        );
    }

    #[test]
    fn managed_agent_local_pin_alias_and_order_handle_render_narrow_with_exact_hits() {
        let mut app = fixture(PtyColorMode::Inherited);
        let key = AgentRowKey::Managed {
            node_id: "node-a".to_owned(),
            record_id: "record-local-render".to_owned(),
        };
        app.nodes[0].session_records.push(ManagedSessionView {
            node_id: "node-a".to_owned(),
            record_id: "record-local-render".to_owned(),
            display_name: "Authoritative title".to_owned(),
            provider: provider("codex"),
            mode: SessionMode::Pty,
            state: ManagedSessionState::Dormant,
            workspace_id: "workspace-a".to_owned(),
            canonical_root: None,
            has_provider_session_identity: true,
            bundle: None,
            context_id: None,
            context: None,
            task_binding: None,
            active_session: None,
            last_error: None,
        });
        app.managed_agent_preferences.insert(
            ("node-a".to_owned(), "record-local-render".to_owned()),
            crate::app::ManagedAgentPreference {
                node_id: "node-a".to_owned(),
                record_id: "record-local-render".to_owned(),
                pinned: true,
                alias: Some("Local alias".to_owned()),
                order: Some(0),
            },
        );
        app.selected_agent = app.agent_rows().iter().position(|row| row == &key).unwrap();
        let mut buffer = TerminalBuffer::new(56, 16);
        let layout = render(&app, &mut buffer);
        let text = buffer_text(&buffer);
        assert!(text.contains("Local"), "{text}");
        assert!(text.contains("Authoritat"), "{text}");
        assert!(text.contains("P"), "{text}");
        let handles = layout.hits.iter().filter(|hit| {
            hit.target == HitTarget::AgentOrderHandle(key.clone())
        }).collect::<Vec<_>>();
        assert_eq!(handles.len(), 1);
        assert!(handles[0].rect.width > 0);
        assert!(handles[0].rect.right() <= layout.agents.right());
        assert!(!layout.hits.iter().any(|hit| {
            matches!(&hit.target, HitTarget::AgentOrderHandle(AgentRowKey::Legacy(_)))
        }));
    }

    #[test]
    fn minimum_sidebar_exposes_only_nonoverlapping_agent_actions() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.sidebar_width = 18;
        let key = AgentRowKey::Legacy(active_pty_address(&app));
        let mut buffer = TerminalBuffer::new(80, 18);
        let layout = render(&app, &mut buffer);
        let actions = layout
            .hits
            .iter()
            .filter(|hit| hit.target == HitTarget::AgentMore(0))
            .collect::<Vec<_>>();
        assert_eq!(actions.len(), 1);
        assert!(actions[0].rect.right() <= layout.agents.right());
        assert!(!layout.hits.iter().any(|hit| {
            hit.target == HitTarget::AgentRun(key.clone())
                || hit.target == HitTarget::AgentProgressToggle(key.clone())
        }));
        assert!(buffer_text(&buffer).contains("[act]"));
    }

    fn inspection() -> WorkspaceInspection {
        WorkspaceInspection {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            entries: vec![
                WorkspaceEntry {
                    relative_path: repository_path("src"),
                    kind: WorkspaceEntryKind::Directory,
                },
                WorkspaceEntry {
                    relative_path: repository_path("src/main.rs"),
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
                    path: repository_path("src/main.rs"),
                    previous_path: None,
                }],
                recent_commits: vec![GitCommitSummary {
                    id: "abcdef0".to_owned(),
                    summary: "ship workspace controls".to_owned(),
                }],
                worktrees: Vec::new(),
                managed_worktree: None,
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
        assert!(!workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::RosterMode(RosterMode::NativeSessions)));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::RosterMode(RosterMode::Workspaces)));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::AddTab));
        assert!(workspace_layout.hits.iter().any(|hit| hit.target == HitTarget::Settings));

        app.roster_mode = RosterMode::Agents;
        let agent_layout = render(&app, &mut buf);
        assert!(agent_layout.hits.iter().any(|hit| hit.target == HitTarget::AddAgent));
        assert!(agent_layout.hits.iter().all(|hit| !matches!(hit.target, HitTarget::Viewport) || hit.rect == agent_layout.viewport));
    }

    #[test]
    fn agent_run_controls_use_stable_keys_in_roster_tree_and_inspector_header() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.roster_mode = RosterMode::Agents;
        let expected = AgentRowKey::Legacy(active_pty_address(&app));
        let mut buf = TerminalBuffer::new(100, 32);
        let layout = render(&app, &mut buf);
        let run = layout.hits.iter().find(|hit| {
            hit.target == HitTarget::AgentRun(expected.clone())
        }).cloned().expect("agent roster exposes stable run target");
        app.layout = layout;
        let action = app.click(run.rect.x, run.rect.y);
        assert!(matches!(action, crate::app::AppAction::InspectWorkspace { .. }));

        let lens_layout = render(&app, &mut buf);
        assert!(lens_layout.hits.iter().any(|hit| hit.target == HitTarget::AgentRunAll));
        assert!(lens_layout.hits.iter().any(|hit| {
            hit.target == HitTarget::AgentRun(expected.clone())
        }));
        let all = lens_layout.hits.iter().find(|hit| hit.target == HitTarget::AgentRunAll).unwrap();
        assert!(!lens_layout.hits.iter().any(|hit| {
            matches!(hit.target, HitTarget::NewFile | HitTarget::NewDirectory)
                && hit.rect.contains(all.rect.x, all.rect.y)
        }));

        let mut unified = fixture(PtyColorMode::Inherited);
        unified.focus = Focus::Agents;
        let _ = unified.reduce(crate::app::UiKey::Char('a'));
        let tree = render(&unified, &mut buf);
        assert!(tree.hits.iter().any(|hit| {
            hit.target == HitTarget::AgentRun(AgentRowKey::Legacy(active_pty_address(&unified)))
        }));
    }

    #[test]
    fn roster_exposes_only_agents_and_workspaces_controls_across_layouts() {
        let mut app = fixture(PtyColorMode::Inherited);
        let mut buf = TerminalBuffer::new(100, 24);

        let split = render(&app, &mut buf);
        assert!([RosterMode::Agents, RosterMode::Workspaces]
            .iter()
            .all(|mode| split.hits.iter().any(|hit| hit.target == HitTarget::RosterMode(*mode))));
        assert!(!split.hits.iter().any(|hit| {
            hit.target == HitTarget::RosterMode(RosterMode::NativeSessions)
        }));

        app.sidebar_presentation = SidebarPresentation::Activity;
        app.control_section = ControlSection::Agents;
        let activity = render(&app, &mut buf);
        assert!([RosterMode::Agents, RosterMode::Workspaces]
            .iter()
            .all(|mode| activity.hits.iter().any(|hit| hit.target == HitTarget::RosterMode(*mode))));
        assert!(!activity.hits.iter().any(|hit| {
            hit.target == HitTarget::RosterMode(RosterMode::NativeSessions)
        }));

        app.menu_placement = MenuPlacement::Modal;
        app.focus = Focus::Settings;
        let modal = render(&app, &mut buf);
        assert!([RosterMode::Agents, RosterMode::Workspaces]
            .iter()
            .all(|mode| modal.hits.iter().any(|hit| hit.target == HitTarget::RosterMode(*mode))));
        assert!(!modal.hits.iter().any(|hit| {
            hit.target == HitTarget::RosterMode(RosterMode::NativeSessions)
        }));
    }

    #[test]
    fn agents_surface_renders_native_tree_and_hides_empty_workspace() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.nodes[0].session_records.clear();
        let mut workspace_b = app.nodes[0].workspaces[0].clone();
        workspace_b.workspace_id = "workspace-b".to_owned();
        workspace_b.label = "second".to_owned();
        workspace_b.sessions.clear();
        let mut workspace_empty = workspace_b.clone();
        workspace_empty.workspace_id = "workspace-empty".to_owned();
        workspace_empty.label = "empty".to_owned();
        workspace_empty.providers.clear();
        app.nodes[0].workspaces.extend([workspace_b, workspace_empty]);
        app.roster_mode = RosterMode::Agents;
        let mut buf = TerminalBuffer::new(100, 70);
        app.existing_session = Some(crate::app::ExistingSessionDialog {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            mode: ExistingSessionMode::Catalog,
            catalog: NativeSessionCatalogState::Ready,
            rows: vec![
                crate::app::NativeSessionCatalogRowView {
                    node_id: "node-a".to_owned(),
                    route: crate::app::NativeSessionCatalogRoute::workspace(
                        "workspace-a".to_owned(), provider("codex")
                    ),
                    catalog_revision: 7,
                    recent_cutoff_unix_ms: 10,
                    selection_id: "hist_native_7".to_owned(),
                    title: Some("Recovered Codex".to_owned()),
                    modified_at: Some("1765432100000".to_owned()),
                    model: Some("gpt-5".to_owned()),
                    message_count: Some(9),
                    completed_turn_count: Some(3),
                    external_group: None,
                    record_id: None,
                },
                crate::app::NativeSessionCatalogRowView {
                    node_id: "node-a".to_owned(),
                    route: crate::app::NativeSessionCatalogRoute::workspace(
                        "workspace-b".to_owned(), provider("kimi")
                    ),
                    catalog_revision: 7,
                    recent_cutoff_unix_ms: 10,
                    selection_id: "hist_native_8".to_owned(),
                    title: Some("Recovered Kimi".to_owned()),
                    modified_at: Some("1765432100001".to_owned()),
                    model: None,
                    message_count: Some(4),
                    completed_turn_count: Some(1),
                    external_group: None,
                    record_id: None,
                },
            ],
            selected: 0,
            tree_cursor: 0,
            scroll: 0,
            display_name: "Recovered Codex".to_owned(),
            session_id: "native-session-7".to_owned(),
            field: ExistingSessionField::Sessions,
            operation: None,
            operation_error: None,
            request_token: 1,
            pending_routes: std::collections::BTreeSet::new(),
            route_pages: std::collections::BTreeMap::new(),
            catalog_failure: None,
            preview: None,
            preview_scroll: 0,
            preview_request_token: 0,
            preview_record_id: None,
            preview_selection_id: None,
            ask_after_resume: String::new(),
        });
        app.existing_session.as_mut().unwrap().route_pages.insert(
            crate::app::NativeSessionCatalogRoute::workspace(
                "workspace-a".to_owned(), provider("codex")
            ),
            crate::app::NativeSessionRoutePageState {
                catalog_revision: 7,
                recent_cutoff_unix_ms: 10,
                recent_remaining_count: 0,
                older_remaining_count: 3,
                recent_next_after_selection_id: None,
                older_next_after_selection_id: None,
                recent_has_more: false,
                older_has_more: true,
                pending: None,
            },
        );
        let roster = render(&app, &mut buf);
        assert!(app.nodes[0].session_records.is_empty());
        assert!(!roster.hits.iter().any(|hit| {
            hit.target == HitTarget::RosterMode(RosterMode::NativeSessions)
        }));
        assert!(roster.hits.iter().any(|hit| hit.target == HitTarget::ExistingSessionRow(0)));
        assert!(roster.hits.iter().any(|hit| hit.target == HitTarget::NativeSessionsOpen));
        assert!(roster.hits.iter().any(|hit| matches!(&hit.target,
            HitTarget::NativeSessionsLoadMore(
                route,
                gate4agent_types::NativeSessionCatalogWindow::Older,
            ) if route.workspace_id.as_deref() == Some("workspace-a")
                && route.provider == provider("codex")
        )));
        let roster_text = buffer_text(&buf);
        assert!(
            roster_text.contains("managed/live agent(s)"),
            "{roster_text}",
        );
        assert!(roster.hits.iter().any(|hit| {
            hit.target == HitTarget::NativeSessionMore(0)
        }));
        assert!(roster.hits.iter().any(|hit| {
            hit.target == HitTarget::AgentBoardOpen
        }));
        assert!(roster_text.contains("[B Board]"), "{roster_text}");
        assert!(roster_text.contains("workspace-a"));
        assert!(roster_text.contains("workspace-b"));
        assert!(!roster_text.contains("(0)"));
        assert!(roster_text.contains("[codex] (1)"));
        assert!(roster_text.contains("Recove"));
        assert!(roster_text.contains("Show 3 older"));
        assert!(roster_text.contains("[kimi] (1)"));
        assert!(roster_text.contains("Recove"));
        assert!(!roster_text.contains("Provider <"));
        assert!(!roster_text.contains(&["+", " existing"].concat()));
        assert!(!roster_text.contains("Existing sessions"));

        let dialog = app.existing_session.as_mut().unwrap();
        dialog.provider = provider("qwen-code");
        dialog.rows[0].route.provider = provider("qwen-code");
        dialog.preview = Some(NativeSessionPreviewState::Ready(
            crate::app::NativeSessionPreviewView {
                title: Some("Recovered Qwen".to_owned()),
                modified_at: None,
                model: None,
                message_count: 0,
                message_count_exact: false,
                completed_turn_count: None,
                total_tokens: None,
                truncated: true,
                messages: vec![crate::app::NativeSessionPreviewMessageView {
                    role: "assistant".to_owned(),
                    text: "latest bounded answer".to_owned(),
                }],
            },
        ));
        app.focus = Focus::ExistingSession;
        let qwen_modal = render(&app, &mut buf);
        assert!(!qwen_modal.hits.iter().any(|hit| {
            hit.target == HitTarget::ExistingSessionNativeResume
        }));
        assert!(buffer_text(&buf).contains(
            "qwen-code history and preview are available; native resume is not supported"
        ));
        assert!(buffer_text(&buf).contains("showing latest 1"));
        assert!(!buffer_text(&buf).contains("of 0"));

        let key = crate::app::PreviewTabKey::NativeSelection {
            node_id: "node-a".to_owned(),
            route: crate::app::NativeSessionCatalogRoute::workspace(
                "workspace-a".to_owned(), provider("codex")
            ),
            catalog_revision: 7,
            recent_cutoff_unix_ms: 10,
            selection_id: "hist_native_7".to_owned(),
        };
        app.preview_tabs.insert(key.clone(), crate::app::PreviewTabView {
            title: "Recovered Codex".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            preview: NativeSessionPreviewState::Ready(crate::app::NativeSessionPreviewView {
            title: Some("Recovered Codex".to_owned()),
            modified_at: Some("1765432100000".to_owned()),
            model: Some("gpt-5".to_owned()),
            message_count: 1,
            message_count_exact: true,
            completed_turn_count: Some(1),
            total_tokens: None,
            truncated: false,
            messages: vec![crate::app::NativeSessionPreviewMessageView {
                role: "user".to_owned(),
                text: "review this bounded change".to_owned(),
            }],
            }),
            phase: PreviewTabPhase::Hydrated,
            reconnect_error: None,
            scroll: 0,
            request_token: 7,
            resume_available: true,
            record_id: None,
        });
        app.surface.open_in_focused(SurfaceTab::Preview(key));
        app.focus = Focus::Viewport;
        let pane = render(&app, &mut buf);
        assert_eq!(pane.existing_session_modal, Rect::default());
        let text = buffer_text(&buf);
        assert!(text.contains("Recovered Codex"));
        assert!(text.contains("review this bounded change"));
        assert!(!text.contains("1765432100000"));
        assert!(!text.contains("gpt-5"));
        assert!(!text.contains("USER:"));
        assert!(!text.contains("ASSISTANT:"));
        assert!(text.contains("read-only"));
        assert!(!text.contains("workspace="));
        assert!(!text.contains("transcript_path"));
        assert!(!text.contains("raw_messages"));

        app.focus = Focus::ExistingSession;
        app.existing_session.as_mut().unwrap().mode = ExistingSessionMode::AdvancedImport;
        app.existing_session.as_mut().unwrap().field = ExistingSessionField::SessionId;
        let advanced = render(&app, &mut buf);
        assert!(advanced.hits.iter().any(|hit| hit.target == HitTarget::ExistingSessionImport));
        assert!(advanced.hits.iter().any(|hit| hit.target == HitTarget::ExistingSessionBackToCatalog));
    }

    #[test]
    fn preview_surface_renders_loading_empty_unavailable_and_error_states() {
        let mut app = fixture(PtyColorMode::Inherited);
        let key = crate::app::PreviewTabKey::ManagedRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-history".to_owned(),
        };
        app.preview_tabs.insert(key.clone(), crate::app::PreviewTabView {
            title: "Dormant history".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            preview: NativeSessionPreviewState::Loading,
            phase: PreviewTabPhase::Hydrated,
            reconnect_error: None,
            scroll: 0,
            request_token: 11,
            resume_available: true,
            record_id: Some("record-history".to_owned()),
        });
        app.surface.open_in_focused(SurfaceTab::Preview(key.clone()));
        app.focus = Focus::Viewport;

        let mut loading = TerminalBuffer::new(100, 24);
        let loading_layout = render(&app, &mut loading);
        assert_eq!(loading_layout.existing_session_modal, Rect::default());
        assert!(buffer_text(&loading).contains("Loading session"));
        assert!(buffer_text(&loading).contains("[Resume session]"));

        app.preview_tabs.get_mut(&key).unwrap().preview = NativeSessionPreviewState::Empty(
            crate::app::NativeSessionPreviewView {
                title: Some("Dormant history".to_owned()),
                modified_at: None,
                model: None,
                message_count: 0,
                message_count_exact: true,
                completed_turn_count: None,
                total_tokens: None,
                truncated: false,
                messages: Vec::new(),
            },
        );
        let mut empty = TerminalBuffer::new(100, 24);
        render(&app, &mut empty);
        assert!(buffer_text(&empty).contains("No conversation turns yet"));

        app.preview_tabs.get_mut(&key).unwrap().preview =
            NativeSessionPreviewState::Unavailable("adapter capability absent".to_owned());
        let mut unavailable = TerminalBuffer::new(100, 24);
        render(&app, &mut unavailable);
        assert!(buffer_text(&unavailable).contains("Session unavailable: adapter capability absent"));
        assert!(buffer_text(&unavailable).contains("[Resume session]"));

        app.preview_tabs.get_mut(&key).unwrap().preview =
            NativeSessionPreviewState::Error("bounded preview rejected".to_owned());
        let mut error = TerminalBuffer::new(100, 24);
        render(&app, &mut error);
        assert!(buffer_text(&error).contains("Session failed: bounded preview rejected"));
        assert!(buffer_text(&error).contains("[Resume session]"));
    }

    #[test]
    fn native_agent_chat_rejects_unknown_transcript_roles_fail_closed() {
        let tab = crate::app::PreviewTabView {
            title: "Semantic session".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            preview: NativeSessionPreviewState::Ready(crate::app::NativeSessionPreviewView {
                title: None,
                modified_at: None,
                model: None,
                message_count: 5,
                message_count_exact: true,
                completed_turn_count: Some(1),
                total_tokens: None,
                truncated: false,
                messages: vec![
                    crate::app::NativeSessionPreviewMessageView {
                        role: "user".to_owned(),
                        text: "inspect this".to_owned(),
                    },
                    crate::app::NativeSessionPreviewMessageView {
                        role: "assistant".to_owned(),
                        text: "checking".to_owned(),
                    },
                    crate::app::NativeSessionPreviewMessageView {
                        role: "thinking".to_owned(),
                        text: "bounded reasoning".to_owned(),
                    },
                    crate::app::NativeSessionPreviewMessageView {
                        role: "tool:rg".to_owned(),
                        text: "one match".to_owned(),
                    },
                    crate::app::NativeSessionPreviewMessageView {
                        role: "error".to_owned(),
                        text: "failed turn".to_owned(),
                    },
                ],
            }),
            phase: PreviewTabPhase::Resuming,
            reconnect_error: None,
            scroll: 0,
            request_token: 1,
            resume_available: true,
            record_id: Some("record-semantic".to_owned()),
        };

        let lines = native_agent_chat_lines(&tab, 80, '⠙');
        assert!(lines.iter().any(|line| line.kind == NativeAgentChatLineKind::User));
        assert!(lines.iter().any(|line| line.kind == NativeAgentChatLineKind::Assistant));
        assert!(!lines.iter().any(|line| line.text.contains("bounded reasoning")));
        assert!(!lines.iter().any(|line| line.text.contains("one match")));
        assert!(!lines.iter().any(|line| line.text.contains("failed turn")));
        assert!(lines.iter().any(|line| {
            line.kind == NativeAgentChatLineKind::Status
                && line.text.contains("Reconnecting session")
        }));
    }

    #[test]
    fn provider_read_only_skins_are_static_private_and_resume_fail_closed() {
        let cases = [
            ("claude", "CLAUDE", true),
            ("codex", "CODEX", true),
            ("kimi", "KIMI", true),
            ("grok", "GROK", true),
            ("qwen-code", "QWEN", false),
            ("other-provider", "AGENT", false),
        ];

        for (provider_id, assistant, can_resume) in cases {
            let tab = crate::app::PreviewTabView {
                title: "Bounded history".to_owned(),
                workspace_id: "WORKSPACE_PRIVATE".to_owned(),
                provider: provider(provider_id),
                preview: NativeSessionPreviewState::Ready(
                    crate::app::NativeSessionPreviewView {
                        title: Some("Bounded history".to_owned()),
                        modified_at: Some("TIMESTAMP_PRIVATE".to_owned()),
                        model: Some("MODEL_PRIVATE".to_owned()),
                        message_count: 2,
                        message_count_exact: true,
                        completed_turn_count: Some(1),
                        total_tokens: None,
                        truncated: false,
                        messages: vec![
                            crate::app::NativeSessionPreviewMessageView {
                                role: "user".to_owned(),
                                text: "bounded request".to_owned(),
                            },
                            crate::app::NativeSessionPreviewMessageView {
                                role: "assistant".to_owned(),
                                text: "bounded answer".to_owned(),
                            },
                        ],
                    },
                ),
                phase: PreviewTabPhase::Hydrated,
                reconnect_error: None,
                scroll: 0,
                request_token: 1,
                resume_available: true,
                record_id: Some("RECORD_PRIVATE".to_owned()),
            };
            let mut buffer = TerminalBuffer::new(80, 9);
            let mut layout = LayoutRects::default();
            render_preview_tab(
                &tab,
                PaneId(41),
                Rect::new(0, 0, 80, 9),
                &mut buffer,
                &mut layout,
                Theme::for_mode(PtyColorMode::Inherited),
                '⠙',
            );
            let text = buffer_text(&buffer);
            assert!(text.contains(assistant), "{provider_id}: {text}");
            assert!(text.contains("bounded answer"), "{provider_id}: {text}");
            assert!(text.contains("bounded request"), "{provider_id}: {text}");
            assert!(text.contains("read-only"), "{provider_id}: {text}");
            assert!(text.contains("2 messages | 1 completed turns"), "{provider_id}: {text}");
            assert!(!text.contains("MODEL_PRIVATE"), "{provider_id}: {text}");
            assert!(!text.contains("TIMESTAMP_PRIVATE"), "{provider_id}: {text}");
            assert!(!text.contains("WORKSPACE_PRIVATE"), "{provider_id}: {text}");
            assert!(!text.contains("RECORD_PRIVATE"), "{provider_id}: {text}");
            assert!(!text.contains("live PTY"), "{provider_id}: {text}");
            assert_eq!(
                layout.hits.iter().filter(|hit| {
                    hit.target == HitTarget::PreviewResume(PaneId(41))
                }).count(),
                usize::from(can_resume),
                "{provider_id}",
            );
            assert_eq!(text.contains("[Resume session]"), can_resume, "{provider_id}");
        }

        assert_eq!(
            ProviderPreviewSkin::for_provider("qwen"),
            ProviderPreviewSkin::Qwen,
        );
    }

    #[test]
    fn read_only_transcript_reports_bounded_history_and_wraps_words_and_code() {
        let mut tab = crate::app::PreviewTabView {
            title: "Bounded".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            preview: NativeSessionPreviewState::Ready(crate::app::NativeSessionPreviewView {
                title: None,
                modified_at: None,
                model: None,
                message_count: 42,
                message_count_exact: true,
                completed_turn_count: Some(21),
                total_tokens: None,
                truncated: true,
                messages: vec![crate::app::NativeSessionPreviewMessageView {
                    role: "assistant".to_owned(),
                    text: "alpha beta gamma".to_owned(),
                }],
            }),
            phase: PreviewTabPhase::Hydrated,
            reconnect_error: None,
            scroll: 0,
            request_token: 1,
            resume_available: true,
            record_id: Some("record-a".to_owned()),
        };
        assert_eq!(
            preview_history_summary(&tab),
            "latest 1 of 42 messages | 21 completed turns | total tokens unknown",
        );
        if let NativeSessionPreviewState::Ready(preview) = &mut tab.preview {
            preview.message_count_exact = false;
        }
        assert_eq!(
            preview_history_summary(&tab),
            "latest 1 messages; total incomplete | 21 completed turns | total tokens unknown",
        );
        assert_eq!(wrap_preview_text("alpha beta gamma", 10), vec!["alpha beta", "gamma"]);
        assert_eq!(wrap_preview_text("safe\u{202e}spoof", 20), vec!["safe�spoof"]);
        assert_eq!(
            wrap_preview_text("```\nlet value = 123456;\n```", 8),
            vec!["```", "let valu", "e = 1234", "56;", "```"],
        );
    }

    #[test]
    fn read_only_preview_tokens_render_unknown_zero_and_count_without_private_metadata() {
        let mut tab = crate::app::PreviewTabView {
            title: "PRIVATE_TITLE".to_owned(),
            workspace_id: "PRIVATE_WORKSPACE".to_owned(),
            provider: provider("codex"),
            preview: NativeSessionPreviewState::Ready(crate::app::NativeSessionPreviewView {
                title: Some("PRIVATE_PREVIEW_TITLE".to_owned()),
                modified_at: Some("PRIVATE_TIMESTAMP".to_owned()),
                model: Some("PRIVATE_MODEL".to_owned()),
                message_count: 1,
                message_count_exact: true,
                completed_turn_count: Some(1),
                total_tokens: None,
                truncated: false,
                messages: vec![crate::app::NativeSessionPreviewMessageView {
                    role: "assistant".to_owned(),
                    text: "bounded answer".to_owned(),
                }],
            }),
            phase: PreviewTabPhase::Hydrated,
            reconnect_error: None,
            scroll: 0,
            request_token: 1,
            resume_available: true,
            record_id: Some("PRIVATE_RECORD".to_owned()),
        };

        for (total_tokens, expected) in [
            (None, "unknown"),
            (Some(0), "0"),
            (Some(4_321), "4321"),
        ] {
            let NativeSessionPreviewState::Ready(preview) = &mut tab.preview else {
                unreachable!("test preview stays ready");
            };
            preview.total_tokens = total_tokens;

            assert_eq!(
                preview_history_summary(&tab),
                format!("1 messages | 1 completed turns | total tokens {expected}"),
            );

            let mut buffer = TerminalBuffer::new(200, 6);
            let mut layout = LayoutRects::default();
            render_preview_tab(
                &tab,
                PaneId(88),
                Rect::new(0, 0, 200, 6),
                &mut buffer,
                &mut layout,
                Theme::for_mode(PtyColorMode::Inherited),
                '⠙',
            );
            let text = buffer_text(&buffer);
            assert!(text.contains(&format!("total tokens {expected}")), "{text}");
            for private in [
                "PRIVATE_TITLE",
                "PRIVATE_PREVIEW_TITLE",
                "PRIVATE_TIMESTAMP",
                "PRIVATE_MODEL",
                "PRIVATE_WORKSPACE",
                "PRIVATE_RECORD",
            ] {
                assert!(!text.contains(private), "{private}: {text}");
            }
        }
    }

    #[test]
    fn provider_read_only_skin_keeps_narrow_resume_hit_inside_exact_pane() {
        let tab = crate::app::PreviewTabView {
            title: "Narrow".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            preview: NativeSessionPreviewState::Ready(crate::app::NativeSessionPreviewView {
                title: None,
                modified_at: None,
                model: None,
                message_count: 1,
                message_count_exact: true,
                completed_turn_count: Some(1),
                total_tokens: None,
                truncated: false,
                messages: vec![crate::app::NativeSessionPreviewMessageView {
                    role: "assistant".to_owned(),
                    text: "narrow bounded response".to_owned(),
                }],
            }),
            phase: PreviewTabPhase::Hydrated,
            reconnect_error: None,
            scroll: 0,
            request_token: 1,
            resume_available: true,
            record_id: Some("record-narrow".to_owned()),
        };
        let area = Rect::new(3, 2, 8, 4);
        let mut buffer = TerminalBuffer::new(16, 8);
        let mut layout = LayoutRects::default();
        render_preview_tab(
            &tab,
            PaneId(73),
            area,
            &mut buffer,
            &mut layout,
            Theme::for_mode(PtyColorMode::Inherited),
            '⠙',
        );

        let hits = layout.hits.iter().filter(|hit| {
            hit.target == HitTarget::PreviewResume(PaneId(73))
        }).collect::<Vec<_>>();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].rect.width > 0);
        assert_eq!(hits[0].rect.y, area.bottom() - 1);
        assert!(hits[0].rect.x >= area.x);
        assert!(hits[0].rect.right() <= area.right());
        let text = buffer_text(&buffer);
        assert!(text.contains("read-o"), "{text:?}");
    }

    #[test]
    fn managed_and_native_preview_keys_share_qwen_skin_and_no_resume_policy() {
        let tab = crate::app::PreviewTabView {
            title: "Qwen archive".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("qwen-code"),
            preview: NativeSessionPreviewState::Ready(crate::app::NativeSessionPreviewView {
                title: Some("Qwen archive".to_owned()),
                modified_at: None,
                model: None,
                message_count: 1,
                message_count_exact: true,
                completed_turn_count: Some(1),
                total_tokens: None,
                truncated: false,
                messages: vec![crate::app::NativeSessionPreviewMessageView {
                    role: "assistant".to_owned(),
                    text: "same bounded history".to_owned(),
                }],
            }),
            phase: PreviewTabPhase::Hydrated,
            reconnect_error: None,
            scroll: 0,
            request_token: 1,
            resume_available: true,
            record_id: Some("record-qwen".to_owned()),
        };
        let managed_key = crate::app::PreviewTabKey::ManagedRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-qwen".to_owned(),
        };
        let native_key = crate::app::PreviewTabKey::NativeSelection {
            node_id: "node-a".to_owned(),
            route: crate::app::NativeSessionCatalogRoute::workspace(
                "workspace-a".to_owned(),
                provider("qwen-code"),
            ),
            catalog_revision: 7,
            recent_cutoff_unix_ms: 10,
            selection_id: "native-qwen".to_owned(),
        };
        let render_key = |key: crate::app::PreviewTabKey| {
            let mut app = fixture(PtyColorMode::Inherited);
            app.preview_tabs.insert(key.clone(), tab.clone());
            app.surface.open_in_focused(SurfaceTab::Preview(key));
            app.focus = Focus::Viewport;
            let mut buffer = TerminalBuffer::new(86, 18);
            let layout = render(&app, &mut buffer);
            (buffer_text(&buffer), layout)
        };

        let (managed_text, managed_layout) = render_key(managed_key);
        let (native_text, native_layout) = render_key(native_key);
        assert_eq!(managed_text, native_text);
        assert!(managed_text.contains("read-only"));
        assert!(managed_text.contains("QWEN"));
        assert!(managed_text.contains("same bounded history"));
        assert!(!managed_layout.hits.iter().any(|hit| {
            matches!(hit.target, HitTarget::PreviewResume(_))
        }));
        assert!(!native_layout.hits.iter().any(|hit| {
            matches!(hit.target, HitTarget::PreviewResume(_))
        }));
        assert!(!managed_text.contains("[Resume]"));
        assert!(!managed_text.contains("[Resume session]"));
    }

    #[test]
    fn hydrated_preview_starts_at_tail_with_latest_assistant_visible() {
        let mut app = fixture(PtyColorMode::Inherited);
        let key = crate::app::PreviewTabKey::ManagedRecord {
            node_id: "node-a".to_owned(),
            record_id: "tail-preview".to_owned(),
        };
        app.preview_tabs.insert(key.clone(), crate::app::PreviewTabView {
            title: "Tail preview".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            preview: NativeSessionPreviewState::Ready(crate::app::NativeSessionPreviewView {
                title: None,
                modified_at: None,
                model: None,
                message_count: 2,
                message_count_exact: false,
                completed_turn_count: Some(1),
                total_tokens: None,
                truncated: true,
                messages: vec![
                    crate::app::NativeSessionPreviewMessageView {
                        role: "user".to_owned(),
                        text: format!("EARLIEST {}", "long prompt ".repeat(120)),
                    },
                    crate::app::NativeSessionPreviewMessageView {
                        role: "assistant".to_owned(),
                        text: "LATEST_ASSISTANT_VISIBLE".to_owned(),
                    },
                ],
            }),
            phase: PreviewTabPhase::Hydrated,
            reconnect_error: None,
            scroll: usize::MAX,
            request_token: 1,
            resume_available: true,
            record_id: Some("tail-preview".to_owned()),
        });
        app.surface.open_in_focused(SurfaceTab::Preview(key));
        app.focus = Focus::Viewport;

        let mut buffer = TerminalBuffer::new(72, 16);
        render(&app, &mut buffer);
        let text = buffer_text(&buffer);
        assert!(text.contains("LATEST_ASSISTANT_VISIBLE"), "{text}");
        assert!(!text.contains("EARLIEST"));
    }

    #[test]
    fn hydrated_preview_resume_chip_dispatches_exact_correlated_resume() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.nodes[0].session_records.push(crate::app::ManagedSessionView {
            node_id: "node-a".to_owned(),
            record_id: "record-chip".to_owned(),
            display_name: "Chip session".to_owned(),
            provider: provider("codex"),
            mode: SessionMode::Pty,
            state: gate4agent_node_protocol::ManagedSessionState::Dormant,
            workspace_id: "workspace-a".to_owned(),
            canonical_root: None,
            has_provider_session_identity: true,
            bundle: None,
            context_id: None,
            context: None,
            task_binding: None,
            active_session: None,
            last_error: None,
        });
        let key = crate::app::PreviewTabKey::ManagedRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-chip".to_owned(),
        };
        app.preview_tabs.insert(key.clone(), crate::app::PreviewTabView {
            title: "Chip session".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            preview: NativeSessionPreviewState::Ready(crate::app::NativeSessionPreviewView {
                title: Some("Chip session".to_owned()),
                modified_at: None,
                model: None,
                message_count: 1,
                message_count_exact: true,
                completed_turn_count: Some(1),
                total_tokens: None,
                truncated: false,
                messages: vec![crate::app::NativeSessionPreviewMessageView {
                    role: "assistant".to_owned(),
                    text: "ready".to_owned(),
                }],
            }),
            phase: PreviewTabPhase::Hydrated,
            reconnect_error: None,
            scroll: 0,
            request_token: 1,
            resume_available: true,
            record_id: Some("record-chip".to_owned()),
        });
        app.surface.open_in_focused(SurfaceTab::Preview(key.clone()));
        app.focus = Focus::Viewport;
        let mut buffer = TerminalBuffer::new(90, 18);
        let layout = render(&app, &mut buffer);
        assert_eq!(
            layout
                .hits
                .iter()
                .filter(|hit| matches!(hit.target, HitTarget::PreviewResume(_)))
                .count(),
            1,
        );
        let hit = layout
            .hits
            .iter()
            .find(|hit| matches!(hit.target, HitTarget::PreviewResume(_)))
            .cloned()
            .expect("hydrated preview must expose Resume session chip");
        assert!(buffer_text(&buffer).contains("[Resume session]"));
        app.layout = layout;

        let operation_token = match app.click(hit.rect.x, hit.rect.y) {
            crate::app::AppAction::ResumeSessionRecord { operation_token, .. } => operation_token,
            action => panic!("expected chip resume dispatch, got {action:?}"),
        };
        assert_ne!(operation_token, 0);
        assert_eq!(
            app.preview_tabs.get(&key).map(|tab| tab.phase),
            Some(PreviewTabPhase::Resuming),
        );
    }

    #[test]
    fn reconnect_failure_remains_visible_below_long_hydrated_transcript() {
        let mut app = fixture(PtyColorMode::Inherited);
        let key = crate::app::PreviewTabKey::ManagedRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-error".to_owned(),
        };
        app.preview_tabs.insert(key.clone(), crate::app::PreviewTabView {
            title: "Failed reconnect".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            preview: NativeSessionPreviewState::Ready(crate::app::NativeSessionPreviewView {
                title: None,
                modified_at: None,
                model: None,
                message_count: 30,
                message_count_exact: true,
                completed_turn_count: Some(15),
                total_tokens: None,
                truncated: false,
                messages: (0..30)
                    .map(|index| crate::app::NativeSessionPreviewMessageView {
                        role: "assistant".to_owned(),
                        text: format!("historical turn {index}"),
                    })
                    .collect(),
            }),
            phase: PreviewTabPhase::Hydrated,
            reconnect_error: Some("Reconnect failed: provider denied resume".to_owned()),
            scroll: usize::MAX,
            request_token: 1,
            resume_available: true,
            record_id: Some("record-error".to_owned()),
        });
        app.surface.open_in_focused(SurfaceTab::Preview(key));
        let mut buffer = TerminalBuffer::new(70, 18);
        render(&app, &mut buffer);
        let text = buffer_text(&buffer);
        assert!(text.contains("Reconnect"), "{text:?}");
        assert!(text.contains("provider denied"), "{text:?}");
        assert!(text.contains("[Resume session]"));
    }

    #[test]
    fn workspace_window_uses_scroll_offset_without_moving_selection() {
        let mut app = fixture(PtyColorMode::Inherited);
        let template = app.nodes[0].workspaces[0].clone();
        for index in 1..8 {
            let mut workspace = template.clone();
            workspace.workspace_id = format!("workspace-{index}");
            workspace.label = format!("space-{index}");
            workspace.canonical_root = host_path(format!(r"C:\work\space-{index}"));
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
        let address = active_pty_address(&app);
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
        app.surface.focused_pane_mut().tabs.clear();
        let mut buf = TerminalBuffer::new(100, 24);
        let layout = render(&app, &mut buf);
        assert_eq!(buf.get(layout.viewport.x, layout.viewport.y).symbol, " ");
    }

    #[test]
    fn inherited_modals_are_opaque_over_provider_output() {
        let mut app = fixture(PtyColorMode::Inherited);
        app.focus = Focus::AddSpace;
        app.add_space_modal_position = Some((7, 4));
        app.add_space = Some(crate::app::AddSpaceDialog {
            node_id: "node-a".to_owned(),
            workspace_id: "scratch".to_owned(),
            root: r"C:\work\scratch".to_owned(),
            original_root: None,
            root_edited: true,
            field: AddSpaceField::WorkspaceId,
        });
        let mut buf = TerminalBuffer::new(100, 24);

        let layout = render(&app, &mut buf);

        let modal = Rect::new(7, 4, 72, 10);
        assert_eq!(layout.add_space_modal, modal);
        let blank_interior = buf.get(modal.x + 2, modal.y + 6);
        assert_eq!(blank_interior.symbol, " ");
        assert_eq!(blank_interior.style.bg, Color::Black);
        for target in [
            HitTarget::AddSpaceDrag,
            HitTarget::AddSpaceField(AddSpaceField::WorkspaceId),
            HitTarget::AddSpaceField(AddSpaceField::Root),
            HitTarget::AddSpaceCancel,
            HitTarget::AddSpaceRegister,
        ] {
            assert!(layout.hits.iter().any(|hit| hit.target == target));
        }
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
        app.roster_mode = RosterMode::Workspaces;
        let workspace_layout = render(&app, &mut buf);
        assert!([RosterMode::Agents, RosterMode::Workspaces]
            .iter()
            .all(|mode| workspace_layout.hits.iter().any(|hit| {
            hit.target == HitTarget::RosterMode(*mode)
        })));
        assert!(!workspace_layout.hits.iter().any(|hit| {
            hit.target == HitTarget::RosterMode(RosterMode::NativeSessions)
        }));
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
    fn workspace_files_render_create_actions_and_entry_dialog() {
        let mut app = fixture(PtyColorMode::GateOverride);
        app.apply_workspace_inspection("node-a".to_owned(), inspection());
        app.sidebar_mode = SidebarMode::Files;
        let mut files = TerminalBuffer::new(100, 24);

        let files_layout = render(&app, &mut files);

        assert!(files_layout.hits.iter().any(|hit| hit.target == HitTarget::NewFile));
        assert!(files_layout
            .hits
            .iter()
            .any(|hit| hit.target == HitTarget::NewDirectory));
        let files_text = buffer_text(&files);
        assert!(files_text.contains("[+.]") && files_text.contains("[+>]"), "{files_text}");
        assert!(!files_text.contains("+file") && !files_text.contains("+dir"), "{files_text}");

        app.create_workspace_entry = Some(CreateWorkspaceEntryDialog {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            kind: WorkspaceEntryKind::File,
            path: "notes/new.md".to_owned(),
            pending: false,
            token: 0,
            error: None,
        });
        app.focus = Focus::CreateWorkspaceEntry;
        let mut narrow = TerminalBuffer::new(48, 10);
        let mut modal_layout = LayoutRects::default();

        render_create_workspace_entry(
            &app,
            Rect::new(0, 0, 48, 10),
            &mut narrow,
            &mut modal_layout,
            Theme::for_mode(app.color_mode),
        );

        for target in [
            HitTarget::CreateWorkspaceEntrySubmit,
            HitTarget::CreateWorkspaceEntryCancel,
        ] {
            let hit = modal_layout
                .hits
                .iter()
                .find(|hit| hit.target == target)
                .expect("create dialog action hit");
            assert!(hit.rect.right() <= narrow.width());
            assert!(hit.rect.bottom() <= narrow.height());
        }
        let text = buffer_text(&narrow);
        assert!(text.contains("notes/new.md"));
        assert!(text.contains("target"));
        assert!(text.contains("No overwrite"));
    }

    #[test]
    fn git_worktree_rows_expose_create_open_register_remove_and_shift_detail_hits() {
        let mut app = fixture(PtyColorMode::GateOverride);
        let mut snapshot = inspection();
        snapshot.git.worktrees = vec![
            GitWorktreeSnapshot {
                path: host_path(r"C:\work\main"),
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
                path: host_path(r"C:\work\feature"),
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
            repository_path("src"),
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
    fn dirty_matching_uses_physical_bytes_and_preserves_backslashes() {
        let mut git = inspection().git;
        git.status[0].path = repository_path(r"src\main.rs");
        assert_eq!(
            workspace_entry_dirty(
                &repository_path("src/main.rs"),
                WorkspaceEntryKind::File,
                &git,
            ),
            None,
        );

        git.status[0].path = gate4agent_node_protocol::RepositoryPath::unix_bytes(
            b"src/main.rs".to_vec(),
        ).unwrap();
        assert_eq!(
            workspace_entry_dirty(
                &repository_path("src/main.rs"),
                WorkspaceEntryKind::File,
                &git,
            ),
            Some(false),
        );
    }

    #[test]
    fn git_rename_row_renders_sanitized_previous_and_current_repository_paths() {
        let mut app = fixture(PtyColorMode::GateOverride);
        let mut snapshot = inspection();
        snapshot.git.status[0].index_status = "R".to_owned();
        snapshot.git.status[0].previous_path = Some(repository_path("old/\n.rs"));
        snapshot.git.status[0].path = repository_path("new/\u{1b}.rs");
        app.apply_workspace_inspection("node-a".to_owned(), snapshot);
        app.sidebar_mode = SidebarMode::Git;
        let mut buf = TerminalBuffer::new(80, 4);
        let mut layout = LayoutRects::default();
        let inspection = app.selected_workspace_inspection().unwrap();

        render_git_snapshot(
            &app,
            inspection,
            Rect::new(0, 0, 80, 4),
            &mut buf,
            &mut layout,
            Theme::for_mode(app.color_mode),
        );
        let hit = layout.hits.iter()
            .find(|hit| hit.target == HitTarget::SidebarItem(0))
            .expect("git rename row");
        let row = (hit.rect.x..hit.rect.right())
            .map(|x| buf.get(x, hit.rect.y).symbol.as_str())
            .collect::<String>();
        assert!(row.contains(r"old/\n.rs -> new/\u{1b}.rs"), "{row:?}");
    }

    #[cfg(any())]
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
    fn session_drag_renders_pointer_ghost_and_five_zone_compass() {
        let mut app = fixture(PtyColorMode::GateOverride);
        let tab = app.surface.focused_pane().tabs[0].clone();
        let mut buf = TerminalBuffer::new(120, 32);
        let base = render(&app, &mut buf);
        let target = base.surface_panes[0].frame;
        let current_column = target.right().saturating_sub(2);
        let current_row = target.y + target.height / 2;
        app.drag_state = Some(DragState::SessionChip {
            source: DragSource::Tab(PaneId(0), 0),
            tab,
            start_column: 1,
            start_row: 1,
            current_column,
            current_row,
            moved: true,
        });

        render(&app, &mut buf);

        assert_eq!(buf.get(target.x, target.y).symbol, "┌");
        assert_eq!(buf.get(target.x, target.y).style.fg, MAUVE);
        assert_eq!(
            surface_drop_zone(target, current_column, current_row),
            SurfaceDropZone::Right,
        );
        assert_eq!(
            buf.get(target.right().saturating_sub(3), current_row.saturating_add(2))
                .style
                .bg,
            MAUVE,
        );
    }

    #[cfg(any())]
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
    fn launch_product_ux_rows_and_hits_hide_internal_fields_and_do_not_overlap() {
        let mut app = fixture(PtyColorMode::GateOverride);
        app.focus = Focus::Spawn;
        app.spawn_modal_position = Some((6, 3));
        app.spawn = Some(SpawnDialog {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            target: LaunchTarget::NewLinkedWorktree,
            profile_id: "review".to_owned(),
            worktree_profile_id: "isolated".to_owned(),
            bundle_id: "bundle-a".to_owned(),
            context_mode: LaunchContextMode::ContextPack,
            field: LaunchField::ContinueFrom,
        });
        app.history = Some(HistoryDialog {
            source: SessionAddress {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                instance_id: 7,
                generation: 2,
            },
            source_provider: provider("codex"),
            source_workspace_root: host_path(r"C:\work\nemo"),
            candidates: Vec::new(),
            selected: 0,
            loaded: None,
            context: Some(context_receipt()),
            pending_label: None,
        });
        let initial_lease = managed_lease(ManagedWorktreeLeaseState::InUse);
        app.last_managed_spawn_receipt = Some(ManagedWorktreeSpawnReceipt {
            spawn: spawn_receipt(),
            lease: initial_lease,
        });
        app.last_managed_worktree_lease = Some(managed_lease(
            ManagedWorktreeLeaseState::CleanupBlocked,
        ));
        let mut buf = TerminalBuffer::new(110, 28);

        let layout = render(&app, &mut buf);

        let text = buffer_text(&buf);
        assert_eq!(layout.spawn_modal, Rect::new(6, 3, 86, 19));
        assert!(text.contains("session lab / launch"));
        assert!(text.contains("Node"));
        assert!(text.contains("[ node-a | Unknown ]"));
        assert!(text.contains("Workspace"));
        assert!(text.contains("[Browse\u{2026}]"));
        assert!(text.contains("Git location"));
        assert!(text.contains("New linked worktree"));
        assert!(text.contains("[Configure\u{2026}]"));
        assert!(text.contains("Provider"));
        assert!(text.contains("Delivery"));
        assert!(text.contains("Continue from"));
        assert!(!text.contains("session mode"));
        assert!(!text.contains("CLI launch profile"));
        assert!(!text.contains("worktree policy"));
        assert!(!text.contains("[+ Worktree\u{2026}]"));
        assert!(text.contains("[Cancel]"));
        assert!(text.contains("[Launch]"));
        assert!(!text.contains("prompt"));
        for field in [
            LaunchField::Node,
            LaunchField::Workspace,
            LaunchField::GitLocation,
            LaunchField::Provider,
            LaunchField::Delivery,
            LaunchField::ContinueFrom,
        ] {
            assert!(layout
                .hits
                .iter()
                .any(|hit| hit.target == HitTarget::SpawnField(field)));
        }
        for target in [
            HitTarget::SpawnDrag,
            HitTarget::SpawnRegisterWorkspace,
            HitTarget::SpawnConfigureGitLocation,
            HitTarget::SpawnCancel,
            HitTarget::SpawnLaunch,
        ] {
            assert!(layout.hits.iter().any(|hit| hit.target == target));
        }
        let workspace_row = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::SpawnField(LaunchField::Workspace))
            .unwrap()
            .rect;
        let browse = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::SpawnRegisterWorkspace)
            .unwrap()
            .rect;
        assert_eq!(browse.y, workspace_row.y);
        assert!(workspace_row.right() <= browse.x);
        let target_row = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::SpawnField(LaunchField::GitLocation))
            .unwrap()
            .rect;
        let worktree = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::SpawnConfigureGitLocation)
            .unwrap()
            .rect;
        assert_eq!(worktree.y, target_row.y);
        assert!(target_row.right() <= worktree.x);
        assert!(text.contains("profile=review revision=review-r4"));
        assert!(text.contains("worktree state=cleanup-blocked sessions=0 records=0 failure=dirty"));
        assert!(text.contains("bundle revision=bundle-r3 digest=sha256:aaaaaaaaaaaa..."));
        assert!(text.contains("ctx src=node-source/workspace-source#7:3/codex"));
        assert!(text.contains("digest=bbbbbbbb count=7/9 cut=true"));
    }

    #[test]
    fn minimum_supported_terminal_keeps_launch_buttons_inside_the_modal() {
        let mut app = fixture(PtyColorMode::GateOverride);
        app.focus = Focus::Spawn;
        app.spawn = Some(SpawnDialog {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            target: LaunchTarget::NewLinkedWorktree,
            profile_id: "default".to_owned(),
            worktree_profile_id: String::new(),
            bundle_id: String::new(),
            context_mode: LaunchContextMode::None,
            field: LaunchField::Workspace,
        });
        let mut buf = TerminalBuffer::new(56, 14);

        let layout = render(&app, &mut buf);

        assert_eq!(layout.spawn_modal, Rect::new(0, 0, 56, 14));
        for target in [
            HitTarget::SpawnRegisterWorkspace,
            HitTarget::SpawnConfigureGitLocation,
            HitTarget::SpawnCancel,
            HitTarget::SpawnLaunch,
        ] {
            let hit = layout.hits.iter().find(|hit| hit.target == target).unwrap();
            assert!(layout.spawn_modal.contains(hit.rect.x, hit.rect.y));
            assert!(layout.spawn_modal.contains(
                hit.rect.right().saturating_sub(1),
                hit.rect.bottom().saturating_sub(1),
            ));
        }
        let browse = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::SpawnRegisterWorkspace)
            .unwrap()
            .rect;
        let workspace_row = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::SpawnField(LaunchField::Workspace))
            .unwrap()
            .rect;
        assert_eq!(browse.y, workspace_row.y);
        let manual = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::SpawnConfigureGitLocation)
            .unwrap()
            .rect;
        let target_row = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::SpawnField(LaunchField::GitLocation))
            .unwrap()
            .rect;
        assert_eq!(manual.y, target_row.y);
    }

    #[test]
    fn managed_and_off_git_policy_are_visible_only_inside_configuration_modal() {
        let mut app = fixture(PtyColorMode::GateOverride);
        app.focus = Focus::Spawn;
        app.nodes[0].workspaces[0].worktree_service_mode =
            Some(gate4agent_node_protocol::WorktreeServiceMode::Managed);
        app.spawn = Some(SpawnDialog {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: provider("codex"),
            target: LaunchTarget::NewLinkedWorktree,
            profile_id: "default".to_owned(),
            worktree_profile_id: String::new(),
            bundle_id: String::new(),
            context_mode: LaunchContextMode::None,
            field: LaunchField::GitLocation,
        });
        let mut buf = TerminalBuffer::new(100, 24);

        let layout = render(&app, &mut buf);

        assert!(buffer_text(&buf).contains("[Configure\u{2026}]"));
        assert!(layout
            .hits
            .iter()
            .any(|hit| hit.target == HitTarget::SpawnConfigureGitLocation));

        app.nodes[0].workspaces[0].managed_worktree_profiles = Some(WorktreeProfileInventory {
            profiles: vec![ManagedWorktreeProfileSummary {
                id: WorktreeProfileId::new("isolated").unwrap(),
                revision: WorktreeProfileRevision::new("isolated-r2").unwrap(),
                retention: ManagedWorktreeRetention::RemoveWhenReleased,
            }],
        });
        app.spawn.as_mut().unwrap().worktree_profile_id = "isolated".to_owned();
        app.focus = Focus::CreateWorktree;
        app.create_worktree = Some(CreateWorktreeDialog {
            node_id: "node-a".to_owned(),
            source_workspace_id: "workspace-a".to_owned(),
            workspace_id: String::new(),
            target_root: String::new(),
            branch: String::new(),
            base: String::new(),
            field: CreateWorktreeField::WorkspaceId,
            return_to_spawn: true,
            kind: GitLocationDialogKind::ManagedLinked,
        });
        let mut managed_buf = TerminalBuffer::new(100, 24);
        let managed_layout = render(&app, &mut managed_buf);
        let managed_text = buffer_text(&managed_buf);
        assert!(managed_text.contains("mode       Managed"));
        assert!(managed_text.contains("profile    isolated@isolated-r2"));
        assert!(managed_text.contains("retention  remove when released"));
        assert!(managed_text.contains("[Launch]"));
        assert!(managed_layout.hits.iter().all(|hit| !matches!(hit.target, HitTarget::CreateWorktreeField(_))));

        app.create_worktree.as_mut().unwrap().kind = GitLocationDialogKind::LinkedDisabled;
        let mut off_buf = TerminalBuffer::new(100, 24);
        let off_layout = render(&app, &mut off_buf);
        let off_text = buffer_text(&off_buf);
        assert!(off_text.contains("mode     Off"));
        assert!(off_text.contains("Disabled: this workspace explicitly disallows linked worktrees."));
        assert!(off_layout.hits.iter().all(|hit| hit.target != HitTarget::CreateWorktreeCreate));
    }

    #[test]
    fn history_modal_renders_bounded_metadata_without_history_messages() {
        let mut app = fixture(PtyColorMode::GateOverride);
        app.nodes[0].workspaces[0].sessions[0].terminal_formatted =
            b"SECRET_HISTORY_MESSAGE".to_vec();
        app.nodes[0].workspaces[0].sessions[0].terminal_scrollback =
            vec![b"SECRET_HISTORY_MESSAGE".to_vec()];
        app.history = Some(HistoryDialog {
            source: SessionAddress {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                instance_id: 11,
                generation: 2,
            },
            source_provider: provider("codex"),
            source_workspace_root: host_path(r"C:\work\nemo"),
            candidates: vec![HistoryCandidateSummary {
                id: "candidate-17".to_owned(),
                session_id_hint: "native-session-hint".to_owned(),
                modified_at_unix_ms: Some(1_765_432_100_000),
            }],
            selected: 0,
            loaded: Some(LoadedHistoryView {
                session_id: "native-session-17".to_owned(),
                message_count: 12,
                completed_turn_count: Some(5),
            }),
            context: Some(context_receipt()),
            pending_label: None,
        });
        app.last_spawn_receipt = Some(spawn_receipt());
        let mut buf = TerminalBuffer::new(120, 28);

        render_history(&app, Rect::new(0, 0, 120, 28), &mut buf, Theme::for_mode(app.color_mode));

        let text = buffer_text(&buf);
        assert!(text.contains("source provider=codex address=node-a/workspace-a #11:2"));
        assert!(text.contains(r"source workspace=C:\work\nemo"));
        assert!(text.contains("id=candidate-17 | hint=native-session-hint | modified_unix_ms=1765432100000"));
        assert!(text.contains("loaded native_session_id=native-session-17 message_count=12 completed_turn_count=5"));
        assert!(text.contains("exported context_id=context-a"));
        assert!(text.contains("exported digest=sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
        assert!(text.contains("exported message_count=7/9 truncated=true"));
        assert!(text.contains("Enter load | x export | f forget exported context | Esc close"));
        assert!(!text.contains("SECRET_HISTORY_MESSAGE"));
    }

    #[test]
    fn managed_roster_secondary_line_appends_bundle_and_context_tags() {
        let mut app = fixture(PtyColorMode::GateOverride);
        let receipt = spawn_receipt();
        app.nodes[0].session_records.push(ManagedSessionView {
            node_id: "node-a".to_owned(),
            record_id: "record-tags".to_owned(),
            display_name: "tagged session".to_owned(),
            provider: provider("codex"),
            mode: SessionMode::Inline,
            state: ManagedSessionState::Dormant,
            workspace_id: "workspace-a".to_owned(),
            canonical_root: Some(host_path(r"C:\work\nemo")),
            has_provider_session_identity: true,
            bundle: receipt.bundle,
            context_id: receipt.context_id,
            context: receipt.context,
            task_binding: None,
            active_session: None,
            last_error: None,
        });
        let mut buf = TerminalBuffer::new(120, 6);
        let mut layout = LayoutRects::default();

        render_agent_list(
            &app,
            Rect::new(0, 0, 120, 6),
            &mut buf,
            &mut layout,
            Theme::for_mode(app.color_mode),
        );

        let text = buffer_text(&buf);
        assert!(text.contains("b:bundle-a@bundle-r3"));
        assert!(text.contains("c:context-a"));
    }

    #[test]
    fn agent_row_more_button_and_context_menu_render_with_clamped_action_hits() {
        let mut app = fixture(PtyColorMode::GateOverride);
        let key = AgentRowKey::Legacy(active_pty_address(&app));
        app.agent_menu = Some(AgentMenuState {
            key,
            anchor_column: 99,
            anchor_row: 23,
            selected: 0,
        });
        let mut buf = TerminalBuffer::new(100, 24);

        let layout = render(&app, &mut buf);
        assert!(layout
            .hits
            .iter()
            .any(|hit| matches!(hit.target, HitTarget::AgentMore(_))));
        let action_hits = layout
            .hits
            .iter()
            .filter(|hit| matches!(hit.target, HitTarget::AgentMenuAction(_)))
            .collect::<Vec<_>>();
        assert_eq!(action_hits.len(), AgentMenuAction::ALL.len());
        assert!(action_hits
            .iter()
            .all(|hit| hit.rect.right() <= 100 && hit.rect.bottom() <= 24));
        let text = buffer_text(&buf);
        assert!(text.contains("agent actions"));
        assert!(
            text.contains("Rename managed record") || text.contains("Rename..."),
            "{text}",
        );

        app.agent_menu.as_mut().unwrap().selected = AgentMenuAction::ALL.len() - 1;
        let mut narrow = TerminalBuffer::new(56, 14);
        let narrow_layout = render(&app, &mut narrow);
        let narrow_text = buffer_text(&narrow);
        assert!(narrow_text.contains("Forget record"), "{narrow_text}");
        assert!(narrow_layout.hits.iter().any(|hit| {
            hit.target == HitTarget::AgentMenuAction(AgentMenuAction::Forget)
        }));
    }

    #[test]
    fn native_history_row_renders_clamped_context_menu_with_truthful_actions() {
        let mut app = fixture(PtyColorMode::GateOverride);
        let route = crate::app::NativeSessionCatalogRoute::workspace(
            "workspace-a".to_owned(),
            provider("codex"),
        );
        let key = crate::app::PreviewTabKey::NativeSelection {
            node_id: "node-a".to_owned(),
            route: route.clone(),
            catalog_revision: 7,
            recent_cutoff_unix_ms: 10,
            selection_id: "native-history-1".to_owned(),
        };
        let mut dialog = empty_existing_session_dialog();
        dialog.rows.push(crate::app::NativeSessionCatalogRowView {
            node_id: "node-a".to_owned(),
            route,
            catalog_revision: 7,
            recent_cutoff_unix_ms: 10,
            selection_id: "native-history-1".to_owned(),
            title: Some("Prior Codex session".to_owned()),
            modified_at: None,
            model: None,
            message_count: Some(12),
            completed_turn_count: Some(4),
            external_group: None,
            record_id: None,
        });
        app.existing_session = Some(dialog);
        app.native_session_menu = Some(NativeSessionMenuState {
            key,
            anchor_column: 99,
            anchor_row: 23,
            selected: 0,
        });
        let mut buf = TerminalBuffer::new(80, 16);

        let layout = render(&app, &mut buf);
        let action_hits = layout
            .hits
            .iter()
            .filter(|hit| matches!(hit.target, HitTarget::NativeSessionMenuAction(_)))
            .collect::<Vec<_>>();
        assert_eq!(action_hits.len(), NativeSessionMenuAction::ALL.len());
        assert!(action_hits
            .iter()
            .all(|hit| hit.rect.right() <= 80 && hit.rect.bottom() <= 16));
        let text = buffer_text(&buf);
        assert!(text.contains("provider history actions"), "{text}");
        assert!(text.contains("Open transcript + metrics"), "{text}");
        assert!(text.contains("Open Agent Board"), "{text}");
        assert!(text.contains("Rename managed record"), "{text}");
        assert!(text.contains("hydrate the transcript"), "{text}");
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
        let live_address = active_pty_address(&app);
        for (name, state) in states {
            app.nodes[0].session_records.push(ManagedSessionView {
                node_id: "node-a".to_owned(),
                record_id: format!("record-{name}"),
                display_name: format!("{name} session"),
                provider: provider("codex"),
                mode: SessionMode::Pty,
                state,
                workspace_id: "workspace-a".to_owned(),
                canonical_root: Some(host_path(r"C:\work\nemo")),
                has_provider_session_identity: state != ManagedSessionState::IdentityPending,
                bundle: None,
                context_id: None,
                context: None,
                task_binding: None,
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
        assert!([ControlSection::Files, ControlSection::Git].iter().all(|section| expanded_layout
            .hits
            .iter()
            .any(|hit| hit.target == HitTarget::ActivitySection(*section))));
        assert!([RosterMode::Agents, RosterMode::Workspaces]
            .iter()
            .all(|mode| expanded_layout.hits.iter().any(|hit| {
            hit.target == HitTarget::RosterMode(*mode)
        })));
        assert!(!expanded_layout.hits.iter().any(|hit| {
            hit.target == HitTarget::RosterMode(RosterMode::NativeSessions)
        }));

        app.roster_mode = RosterMode::NativeSessions;
        let mut native_selected = TerminalBuffer::new(100, 24);
        let native_layout = render(&app, &mut native_selected);
        let agents_hit = native_layout.hits.iter().find(|hit| {
            hit.target == HitTarget::RosterMode(RosterMode::Agents)
        }).unwrap();
        let accent = Theme::for_mode(app.color_mode).accent;
        assert_eq!(native_selected.get(agents_hit.rect.x, agents_hit.rect.y).style.bg, accent);
        assert!(!native_layout.hits.iter().any(|hit| {
            hit.target == HitTarget::RosterMode(RosterMode::NativeSessions)
        }));

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

    #[test]
    fn folder_browser_renders_node_navigation_filter_and_mouse_actions() {
        let mut app = fixture(PtyColorMode::GateOverride);
        app.focus = Focus::FolderBrowser;
        app.folder_browser = Some(FolderBrowserDialog {
            node_id: "node-a".to_owned(),
            directory: Some(host_path(r"C:\work")),
            parent: Some(host_path(r"C:\")),
            entries: vec![HostDirectoryEntry {
                path: host_path(r"C:\work\nemo"),
                display_name: "nemo".to_owned(),
                is_link: false,
            }],
            next_after: Some(host_path(r"C:\work\nemo")),
            incomplete: false,
            selected: 0,
            scroll: 0,
            filter: "nem".to_owned(),
            field: FolderBrowserField::Entries,
            pending: false,
            append_pending: false,
            request_token: 7,
            error: None,
        });
        let mut buffer = TerminalBuffer::new(110, 30);

        let layout = render(&app, &mut buffer);
        let text = buffer_text(&buffer);

        assert!(text.contains("browse directories on node"));
        assert!(text.contains("node=node-a"));
        assert!(text.contains("nemo"));
        for target in [
            HitTarget::FolderBrowserDrag,
            HitTarget::FolderBrowserParent,
            HitTarget::FolderBrowserFilter,
            HitTarget::FolderBrowserEntry(0),
            HitTarget::FolderBrowserLoadMore,
            HitTarget::FolderBrowserUse,
            HitTarget::FolderBrowserCancel,
        ] {
            assert!(layout.hits.iter().any(|hit| hit.target == target));
        }
    }

    #[test]
    fn manual_worktree_modal_renders_mouse_fields_drag_and_create_action() {
        let mut app = fixture(PtyColorMode::GateOverride);
        app.focus = Focus::CreateWorktree;
        app.create_worktree = Some(CreateWorktreeDialog {
            node_id: "node-a".to_owned(),
            source_workspace_id: "workspace-a".to_owned(),
            workspace_id: "workspace-a-worktree".to_owned(),
            target_root: r"C:\work\workspace-a-worktree".to_owned(),
            branch: "workspace-a-worktree".to_owned(),
            base: String::new(),
            field: CreateWorktreeField::TargetRoot,
            return_to_spawn: true,
            kind: GitLocationDialogKind::ManualLinked,
        });
        let mut buffer = TerminalBuffer::new(100, 26);

        let layout = render(&app, &mut buffer);
        let text = buffer_text(&buffer);

        assert!(text.contains("configure Git location"));
        assert!(text.contains("preview: workspace-a -> workspace-a-worktree"));
        assert!(text.contains("[Create & register]"));
        for field in [
            CreateWorktreeField::WorkspaceId,
            CreateWorktreeField::TargetRoot,
            CreateWorktreeField::Branch,
            CreateWorktreeField::Base,
        ] {
            assert!(layout
                .hits
                .iter()
                .any(|hit| hit.target == HitTarget::CreateWorktreeField(field)));
        }
        for target in [
            HitTarget::CreateWorktreeDrag,
            HitTarget::CreateWorktreeCancel,
            HitTarget::CreateWorktreeCreate,
        ] {
            assert!(layout.hits.iter().any(|hit| hit.target == target));
        }
    }

    #[test]
    fn workspace_file_renders_syntax_and_selection_in_view_and_edit_modes() {
        let mut editor = crate::text_editor::TextEditor::from_text(
            "pub let value = \"text\"; // note".to_owned(),
        )
        .expect("text editor");
        editor
            .start_selection(crate::text_editor::CursorPosition { line: 0, column: 8 })
            .expect("selection start");
        editor
            .update_selection(crate::text_editor::CursorPosition { line: 0, column: 13 })
            .expect("selection end");
        let theme = Theme::for_mode(PtyColorMode::GateOverride);

        for edit_mode in [false, true] {
            let tab = WorkspaceFileTabView {
                editor: editor.clone(),
                state: WorkspaceFileState::Ready,
                edit_mode,
                request_token: 1,
                inline_history: None,
            };
            let mut buffer = TerminalBuffer::new(120, 3);

            render_workspace_file_tab_rich(
                &tab,
                "src/lib.rs",
                Rect::new(0, 0, 120, 3),
                &mut buffer,
                theme,
            );

            let content_x = 4;
            assert_eq!(buffer.get(content_x, 0).symbol, "p");
            assert_eq!(buffer.get(content_x, 0).style.fg, theme.accent);
            assert!(buffer.get(content_x, 0).style.modifiers.contains(Modifier::BOLD));
            assert_eq!(buffer.get(content_x + 8, 0).symbol, "v");
            assert_eq!(buffer.get(content_x + 8, 0).style.bg, theme.accent);
            assert_eq!(buffer.get(content_x + 8, 0).style.fg, theme.active_tab_text);
            let footer = (0..120)
                .map(|column| buffer.get(column, 2).symbol.as_str())
                .collect::<String>();
            assert!(footer.contains("click/drag select"), "{footer:?}");
            assert!(footer.contains("Ctrl+A/C/X/V"), "{footer:?}");
        }
    }

    #[test]
    fn file_selection_predicate_reuses_one_precomputed_utf8_byte_range() {
        let mut editor = crate::text_editor::TextEditor::from_text("zero\nЖ🙂tail\nlast".to_owned())
            .expect("editor");
        editor
            .start_selection(crate::text_editor::CursorPosition { line: 1, column: 1 })
            .expect("selection start");
        editor
            .update_selection(crate::text_editor::CursorPosition { line: 2, column: 1 })
            .expect("selection end");

        let selection = editor.selection_range().expect("byte range");
        let second_line_start = editor.line_byte_start(1).expect("second line start");
        let third_line_start = editor.line_byte_start(2).expect("third line start");

        assert!(!file_byte_is_selected(Some(&selection), second_line_start));
        assert!(file_byte_is_selected(Some(&selection), second_line_start + "Ж".len()));
        assert!(file_byte_is_selected(Some(&selection), third_line_start));
        assert!(!file_byte_is_selected(Some(&selection), third_line_start + 1));
        assert!(!file_byte_is_selected(None, second_line_start + "Ж".len()));
    }

    #[test]
    fn file_surface_separates_global_controls_pane_title_actions_and_scrollbar() {
        let mut app = fixture(PtyColorMode::GateOverride);
        let key = crate::app::WorkspaceFileTabKey {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: repository_path("src/lib.rs"),
        };
        let text = (0..60)
            .map(|line| format!("pub const LINE_{line}: usize = {line};"))
            .collect::<Vec<_>>()
            .join("\n");
        app.file_tabs.insert(
            key.clone(),
            WorkspaceFileTabView {
                editor: crate::text_editor::TextEditor::from_text(text).unwrap(),
                state: WorkspaceFileState::Ready,
                edit_mode: false,
                request_token: 1,
                inline_history: None,
            },
        );
        app.surface.open_in_focused(SurfaceTab::File(key.clone()));
        let pane_id = app.surface.focused;
        let mut buffer = TerminalBuffer::new(120, 24);

        let layout = render(&app, &mut buffer);
        let top = (layout.tabs.x..layout.tabs.right())
            .map(|column| buffer.get(column, layout.tabs.y).symbol.as_str())
            .collect::<String>();
        let header = layout.surface_panes[0].header;
        let header_text = (header.x..header.right())
            .map(|column| buffer.get(column, header.y).symbol.as_str())
            .collect::<String>();
        let actions_text = (header.x..header.right())
            .map(|column| buffer.get(column, header.y + 1).symbol.as_str())
            .collect::<String>();

        assert!(top.starts_with(" +  [#] "), "{top:?}");
        assert!(top.ends_with(" [S] "), "{top:?}");
        assert!(!top.contains(" file "), "{top:?}");
        assert!(header_text.contains(" lib.rs "), "{header_text:?}");
        assert!(!header_text.contains(" file "), "{header_text:?}");
        assert!(actions_text.contains("[History]"), "{actions_text:?}");
        assert!(actions_text.contains("[Changes]"), "{actions_text:?}");
        assert!(actions_text.contains("[Edit]"), "{actions_text:?}");
        assert!(actions_text.contains("[Save]"), "{actions_text:?}");
        for target in [
            HitTarget::FileHistory(pane_id),
            HitTarget::FileChanges(pane_id),
            HitTarget::FileEdit(pane_id),
            HitTarget::FileSave(pane_id),
            HitTarget::FileScrollbar(pane_id),
        ] {
            assert!(layout.hits.iter().any(|hit| hit.target == target), "{target:?}");
        }
        let scrollbar = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::FileScrollbar(pane_id))
            .unwrap()
            .rect;
        assert_eq!(scrollbar.x, layout.surface_panes[0].viewport.right() - 1);
        assert_eq!(buffer.get(scrollbar.x, scrollbar.y).symbol, "#");
        let history_hit = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::FileHistory(pane_id))
            .unwrap()
            .rect;
        let changes_hit = layout
            .hits
            .iter()
            .find(|hit| hit.target == HitTarget::FileChanges(pane_id))
            .unwrap()
            .rect;
        let mut changes_app = app.clone();
        changes_app.layout = layout.clone();
        let crate::app::AppAction::ReadGitHistory {
            path: Some(changes_path),
            ..
        } = changes_app.click(changes_hit.x, changes_hit.y)
        else {
            panic!("Changes hit must win over the pane header");
        };
        assert_eq!(changes_path.as_utf8(), Some("src/lib.rs"));
        assert!(matches!(
            changes_app.file_tabs[&key]
                .inline_history
                .as_ref()
                .and_then(|history| history.pending_diff.as_ref()),
            Some(crate::app::WorkspaceGitDiffTarget::Working { path: Some(ref path) })
                if path.as_utf8() == Some("src/lib.rs")
        ));
        assert!(changes_app.git_tabs.is_empty());

        app.layout = layout.clone();
        let crate::app::AppAction::ReadGitHistory {
            path: Some(history_path),
            ..
        } = app.click(history_hit.x, history_hit.y)
        else {
            panic!("History hit must win over the pane header");
        };
        assert_eq!(history_path.as_utf8(), Some("src/lib.rs"));
        let history_layout = render(&app, &mut buffer);
        let history_header = history_layout.surface_panes[0].header;
        let history_header_text = (history_header.x..history_header.right())
            .map(|column| buffer.get(column, history_header.y).symbol.as_str())
            .collect::<String>();
        let history_actions_text = (history_header.x..history_header.right())
            .map(|column| buffer.get(column, history_header.y + 1).symbol.as_str())
            .collect::<String>();
        assert!(history_header_text.contains(" lib.rs "), "{history_header_text:?}");
        assert!(!history_header_text.contains("commits"), "{history_header_text:?}");
        assert!(history_actions_text.contains("[Source]"), "{history_actions_text:?}");
        assert!(history_actions_text.contains("commits | Up/Down"), "{history_actions_text:?}");
        assert!(buffer_text(&buffer).contains("loading file history"));
        assert!(app.git_tabs.is_empty());
    }

    #[test]
    fn empty_surface_has_only_global_controls_and_no_empty_label() {
        let mut app = App::default();
        app.menu_placement = MenuPlacement::Modal;
        let mut buffer = TerminalBuffer::new(80, 18);

        let layout = render(&app, &mut buffer);
        let global = (layout.tabs.x..layout.tabs.right())
            .map(|column| buffer.get(column, layout.tabs.y).symbol.as_str())
            .collect::<String>();
        let pane = layout.surface_panes[0].header;
        let pane_chrome = (pane.y..pane.y.saturating_add(2))
            .flat_map(|row| {
                (pane.x..pane.right())
                    .map(|column| buffer.get(column, row).symbol.as_str())
                    .collect::<Vec<_>>()
            })
            .collect::<String>();

        assert!(global.starts_with(" +  [#] "), "{global:?}");
        assert!(global.ends_with(" [S] "), "{global:?}");
        assert!(!global.to_ascii_lowercase().contains("empty"), "{global:?}");
        assert!(!pane_chrome.to_ascii_lowercase().contains("empty"), "{pane_chrome:?}");
        assert!(pane_chrome.trim().is_empty(), "{pane_chrome:?}");
    }

    #[test]
    fn file_and_commit_panes_keep_independent_headers_and_ascii_chrome() {
        let mut app = App::default();
        app.menu_placement = MenuPlacement::Modal;
        let file_key = crate::app::WorkspaceFileTabKey {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: repository_path("src/alpha.rs"),
        };
        app.file_tabs.insert(
            file_key.clone(),
            WorkspaceFileTabView {
                editor: crate::text_editor::TextEditor::from_text("fn alpha() {}".to_owned()).unwrap(),
                state: WorkspaceFileState::Ready,
                edit_mode: false,
                request_token: 1,
                inline_history: None,
            },
        );
        let git_key = crate::app::WorkspaceGitTabKey {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            history_path: Some(repository_path("src/beta.rs")),
        };
        app.git_tabs.insert(
            git_key.clone(),
            WorkspaceGitTabView {
                state: WorkspaceGitState::Ready,
                mode: WorkspaceGitPaneMode::List,
                history_path: Some(repository_path("src/beta.rs")),
                commits: Vec::new(),
                selected: 0,
                list_scroll: 0,
                detail_scroll: 0,
                next_before: None,
                has_more: false,
                diff: None,
                diff_error: None,
                pending_diff: None,
                history_token: 1,
                diff_token: 0,
            },
        );
        app.surface.open_in_focused(SurfaceTab::File(file_key));
        app.surface
            .drop_tab(SurfaceTab::Git(git_key), PaneId(0), SurfaceDropZone::Right)
            .unwrap();
        let mut buffer = TerminalBuffer::new(100, 22);

        let layout = render(&app, &mut buffer);
        assert_eq!(layout.surface_panes.len(), 2);
        let left = layout.surface_panes.iter().min_by_key(|pane| pane.frame.x).unwrap();
        let right = layout.surface_panes.iter().max_by_key(|pane| pane.frame.x).unwrap();
        let header_text = |pane: &SurfacePaneLayout| {
            (pane.header.x..pane.header.right())
                .map(|column| buffer.get(column, pane.header.y).symbol.as_str())
                .collect::<String>()
        };
        let action_text = |pane: &SurfacePaneLayout| {
            (pane.header.x..pane.header.right())
                .map(|column| buffer.get(column, pane.header.y + 1).symbol.as_str())
                .collect::<String>()
        };
        let left_header = header_text(left);
        let right_header = header_text(right);

        assert!(left_header.contains("alpha.rs"), "{left_header:?}");
        assert!(!left_header.contains("beta.rs"), "{left_header:?}");
        assert!(right_header.contains("beta.rs commits"), "{right_header:?}");
        assert!(!right_header.contains("alpha.rs"), "{right_header:?}");
        assert!(!left_header.contains(" file "), "{left_header:?}");
        assert!(!right_header.contains(" git "), "{right_header:?}");
        assert!(action_text(left).contains("[History]"));
        assert!(action_text(right).contains("commits | Enter diff"));
        let divider_x = left.frame.right();
        assert_eq!(buffer.get(divider_x, left.frame.y).symbol, "|");
        for text in [left_header, right_header, action_text(left), action_text(right)] {
            assert!(text.is_ascii(), "{text:?}");
        }
        for row in left.frame.y..left.frame.bottom() {
            assert!(buffer.get(divider_x, row).symbol.is_ascii());
        }
    }

    #[test]
    fn git_diff_target_label_names_scope_and_file() {
        let working = crate::app::WorkspaceGitDiffTarget::Working {
            path: Some(repository_path("src/render.rs")),
        };
        let commit = crate::app::WorkspaceGitDiffTarget::Commit {
            revision: "0123456789abcdef".to_owned(),
            path: Some(repository_path("src/render.rs")),
        };

        assert_eq!(git_diff_target_label(&working), "working tree | src/render.rs");
        assert_eq!(
            git_diff_target_label(&commit),
            "commit 0123456789ab | src/render.rs"
        );
    }

    #[test]
    fn git_diff_stats_ignore_unified_headers_and_count_changed_lines() {
        let stats = git_diff_stats(
            "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1,2 @@\n-old\n+new\n+more\ndiff --git a/src/b.rs b/src/b.rs\n--- a/src/b.rs\n+++ b/src/b.rs\n-gone\n",
        );
        assert_eq!(stats, GitDiffStats { files: 2, added: 2, deleted: 2 });
    }

    fn git_commit_view(id: char, subject: &str) -> crate::app::GitCommitView {
        crate::app::GitCommitView {
            id: id.to_string().repeat(40),
            parents: Vec::new(),
            subject: subject.to_owned(),
            author_name: "Author".to_owned(),
            author_email: "author@example.invalid".to_owned(),
            authored_at: "2026-08-12T00:00:00Z".to_owned(),
            committer_name: "Committer".to_owned(),
            committer_email: "committer@example.invalid".to_owned(),
            committed_at: "2026-08-12T00:00:01Z".to_owned(),
            signature_status: "Good".to_owned(),
            signer: None,
        }
    }

    #[test]
    fn narrow_inline_file_history_list_renders_clickable_commit_rows() {
        let tab = WorkspaceGitTabView {
            state: WorkspaceGitState::Ready,
            mode: WorkspaceGitPaneMode::List,
            history_path: Some(repository_path("src/history.rs")),
            commits: vec![
                git_commit_view('a', "first file commit"),
                git_commit_view('b', "second file commit"),
            ],
            selected: 1,
            list_scroll: 0,
            detail_scroll: 0,
            next_before: None,
            has_more: false,
            diff: None,
            diff_error: None,
            pending_diff: None,
            history_token: 1,
            diff_token: 0,
        };
        let pane_id = PaneId(7);
        let mut buffer = TerminalBuffer::new(31, 4);
        let mut layout = LayoutRects::default();

        render_workspace_file_history(
            &tab,
            pane_id,
            Rect::new(0, 0, 31, 4),
            &mut buffer,
            &mut layout,
            Theme::for_mode(PtyColorMode::GateOverride),
            '*',
        );

        let text = buffer_text(&buffer);
        assert!(text.contains("aaaaaaaa first file commit"), "{text}");
        assert!(text.contains("bbbbbbbb second file commit"), "{text}");
        assert!(layout.hits.iter().any(|hit| {
            hit.target == HitTarget::FileHistoryCommit(pane_id, 0)
        }));
        assert!(layout.hits.iter().any(|hit| {
            hit.target == HitTarget::FileHistoryCommit(pane_id, 1)
        }));
    }

    #[test]
    fn inline_file_commit_detail_uses_toolbar_navigation_and_colored_diff_rows() {
        let mut app = App::default();
        app.menu_placement = MenuPlacement::Modal;
        let key = crate::app::WorkspaceFileTabKey {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            path: repository_path("src/history.rs"),
        };
        let revision = "b".repeat(40);
        let history = WorkspaceGitTabView {
            state: WorkspaceGitState::Ready,
            mode: WorkspaceGitPaneMode::Detail,
            history_path: Some(key.path.clone()),
            commits: vec![
                git_commit_view('a', "before"),
                git_commit_view('b', "highlight inline diff"),
                git_commit_view('c', "after"),
            ],
            selected: 1,
            list_scroll: 0,
            detail_scroll: 0,
            next_before: None,
            has_more: false,
            diff: Some(crate::app::WorkspaceGitDiffView {
                target: crate::app::WorkspaceGitDiffTarget::Commit {
                    revision,
                    path: Some(key.path.clone()),
                },
                text: "diff --git a/src/history.rs b/src/history.rs\nindex 1111111..2222222 100644\n--- a/src/history.rs\n+++ b/src/history.rs\n@@ -1 +1 @@\n-old line\n+new line\n".to_owned(),
                byte_len: 141,
                truncated: false,
            }),
            diff_error: None,
            pending_diff: None,
            history_token: 1,
            diff_token: 2,
        };
        app.file_tabs.insert(
            key.clone(),
            WorkspaceFileTabView {
                editor: crate::text_editor::TextEditor::from_text("current source".to_owned())
                    .unwrap(),
                state: WorkspaceFileState::Ready,
                edit_mode: false,
                request_token: 1,
                inline_history: Some(history),
            },
        );
        app.surface.open_in_focused(SurfaceTab::File(key));
        let pane_id = app.surface.focused;
        let theme = Theme::for_mode(PtyColorMode::GateOverride);
        let mut buffer = TerminalBuffer::new(70, 28);

        let layout = render(&app, &mut buffer);
        let text = buffer_text(&buffer);
        for target in [
            HitTarget::FileSource(pane_id),
            HitTarget::FileHistoryBack(pane_id),
            HitTarget::FileHistoryPrevious(pane_id),
            HitTarget::FileHistoryNext(pane_id),
        ] {
            assert!(layout.hits.iter().any(|hit| hit.target == target), "{target:?}");
        }
        assert!(text.contains("[Source] [Commits] < >"), "{text}");
        assert!(text.contains("Summary: 1 file | +1 -1 | 141 bytes"), "{text}");
        assert!(text.contains("@@ -1 +1 @@"), "{text}");
        let row_with = |needle: &str| {
            (0..buffer.height()).find(|row| {
                (0..buffer.width())
                    .map(|column| buffer.get(column, *row).symbol.as_str())
                    .collect::<String>()
                    .contains(needle)
            })
        };
        let deleted_row = row_with("-old line").expect("deleted row");
        let added_row = row_with("+new line").expect("added row");
        let hunk_row = row_with("@@ -1 +1 @@").expect("hunk row");
        let meta_row = row_with("diff --git").expect("diff metadata row");
        assert_eq!(buffer.get(0, deleted_row).style.bg, theme.diff_deleted);
        assert_eq!(buffer.get(0, added_row).style.bg, theme.diff_added);
        assert_eq!(buffer.get(0, hunk_row).style.bg, theme.diff_hunk);
        assert_eq!(buffer.get(0, meta_row).style.bg, theme.diff_meta);
        assert!(!text.contains("current source"), "{text}");
    }

    #[test]
    fn narrow_git_commit_detail_shows_metadata_summary_and_patch() {
        let mut app = App::default();
        app.menu_placement = MenuPlacement::Modal;
        let key = crate::app::WorkspaceGitTabKey {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            history_path: Some(repository_path("src/history.rs")),
        };
        let revision = "a".repeat(40);
        app.git_tabs.insert(
            key.clone(),
            WorkspaceGitTabView {
                state: WorkspaceGitState::Ready,
                mode: WorkspaceGitPaneMode::Detail,
                history_path: key.history_path.clone(),
                commits: vec![crate::app::GitCommitView {
                    id: revision.clone(),
                    parents: vec!["b".repeat(40)],
                    subject: "show exact diff".to_owned(),
                    author_name: "Author".to_owned(),
                    author_email: "author@example.invalid".to_owned(),
                    authored_at: "2026-08-12T00:00:00Z".to_owned(),
                    committer_name: "Committer".to_owned(),
                    committer_email: "committer@example.invalid".to_owned(),
                    committed_at: "2026-08-12T00:00:01Z".to_owned(),
                    signature_status: "Good".to_owned(),
                    signer: Some("Signer".to_owned()),
                }],
                selected: 0,
                list_scroll: 0,
                detail_scroll: 0,
                next_before: None,
                has_more: false,
                diff: Some(crate::app::WorkspaceGitDiffView {
                    target: crate::app::WorkspaceGitDiffTarget::Commit {
                        revision,
                        path: key.history_path.clone(),
                    },
                    text: "commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\n    extended commit body\n\ndiff --git a/src/history.rs b/src/history.rs\n-old\n+new\n+more\n".to_owned(),
                    byte_len: 64,
                    truncated: false,
                }),
                diff_error: None,
                pending_diff: None,
                history_token: 1,
                diff_token: 2,
            },
        );
        app.surface.open_in_focused(SurfaceTab::Git(key));
        let mut buffer = TerminalBuffer::new(58, 28);

        let layout = render(&app, &mut buffer);
        let text = buffer_text(&buffer);
        assert!(layout.hits.iter().any(|hit| hit.target == HitTarget::GitBack(PaneId(0))));
        assert!(text.contains("Commit: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "{text}");
        assert!(text.contains("Message: show exact diff"), "{text}");
        assert!(text.contains("Parents: bbbbbbbbbbbb"), "{text}");
        assert!(text.contains("1 file | +2 -1 | 64 bytes"), "{text}");
        assert!(text.contains("extended commit body"), "{text}");
        assert!(text.contains("-old"), "{text}");
        assert!(text.contains("+new"), "{text}");
    }

    fn add_surface_file_tabs(app: &mut App, count: usize) {
        for index in 0..count {
            app.surface.open_in_focused(SurfaceTab::File(
                crate::app::WorkspaceFileTabKey {
                    node_id: "node-a".to_owned(),
                    workspace_id: "workspace-a".to_owned(),
                    path: repository_path(format!("src/{index}.rs")),
                },
            ));
        }
    }

    #[test]
    fn surface_divider_drag_resizes_two_by_one_frames() {
        let mut app = App::default();
        app.menu_placement = MenuPlacement::Modal;
        add_surface_file_tabs(&mut app, 2);
        app.surface.apply_layout_preset(LayoutPreset::TwoByOne).unwrap();
        let mut buffer = TerminalBuffer::new(100, 24);
        let layout = render(&app, &mut buffer);
        let before = layout.surface_panes.iter().map(|pane| pane.frame.width).collect::<Vec<_>>();
        let divider = layout
            .hits
            .iter()
            .find(|hit| matches!(
                &hit.target,
                HitTarget::SurfaceDivider { path, axis: SplitAxis::Horizontal, .. }
                    if path.0.is_empty()
            ))
            .cloned()
            .unwrap();
        app.layout = layout;

        app.click(divider.rect.x, divider.rect.y);
        app.drag(25, divider.rect.y);
        app.drop_at(25, divider.rect.y);

        let resized = render(&app, &mut buffer);
        let after = resized.surface_panes.iter().map(|pane| pane.frame.width).collect::<Vec<_>>();
        assert_ne!(after, before);
        assert!(after[0] < before[0], "before={before:?} after={after:?}");
        assert!(after.iter().all(|width| *width >= 3), "{after:?}");
    }

    #[test]
    fn surface_divider_drag_resizes_one_by_two_frames() {
        let mut app = App::default();
        app.menu_placement = MenuPlacement::Modal;
        add_surface_file_tabs(&mut app, 2);
        app.surface.apply_layout_preset(LayoutPreset::OneByTwo).unwrap();
        let mut buffer = TerminalBuffer::new(100, 30);
        let layout = render(&app, &mut buffer);
        let before = layout.surface_panes.iter().map(|pane| pane.frame.height).collect::<Vec<_>>();
        let divider = layout
            .hits
            .iter()
            .find(|hit| matches!(
                &hit.target,
                HitTarget::SurfaceDivider { path, axis: SplitAxis::Vertical, .. }
                    if path.0.is_empty()
            ))
            .cloned()
            .unwrap();
        app.layout = layout;

        app.click(divider.rect.x, divider.rect.y);
        app.drag(divider.rect.x, 8);
        app.drop_at(divider.rect.x, 8);

        let resized = render(&app, &mut buffer);
        let after = resized.surface_panes.iter().map(|pane| pane.frame.height).collect::<Vec<_>>();
        assert_ne!(after, before);
        assert!(after[0] < before[0], "before={before:?} after={after:?}");
        assert!(after.iter().all(|height| *height >= 3), "{after:?}");
    }

    #[test]
    fn nested_surface_divider_drag_updates_only_target_split() {
        let mut app = App::default();
        app.menu_placement = MenuPlacement::Modal;
        add_surface_file_tabs(&mut app, 3);
        app.surface.apply_layout_preset(LayoutPreset::TwoPlusOne).unwrap();
        let mut buffer = TerminalBuffer::new(100, 30);
        let layout = render(&app, &mut buffer);
        let divider = layout
            .hits
            .iter()
            .find(|hit| matches!(
                &hit.target,
                HitTarget::SurfaceDivider { path, axis: SplitAxis::Vertical, .. }
                    if path.0 == vec![PaneBranch::First]
            ))
            .cloned()
            .unwrap();
        let (root_before, nested_before) = match &app.surface.root {
            PaneNode::Split { ratio_bps, first, .. } => match first.as_ref() {
                PaneNode::Split { ratio_bps: nested, .. } => (*ratio_bps, *nested),
                _ => panic!("expected nested split"),
            },
            _ => panic!("expected root split"),
        };
        app.layout = layout;

        app.click(divider.rect.x, divider.rect.y);
        app.drag(divider.rect.x, 6);
        app.drop_at(divider.rect.x, 6);

        let (root_after, nested_after) = match &app.surface.root {
            PaneNode::Split { ratio_bps, first, .. } => match first.as_ref() {
                PaneNode::Split { ratio_bps: nested, .. } => (*ratio_bps, *nested),
                _ => panic!("expected nested split"),
            },
            _ => panic!("expected root split"),
        };
        assert_eq!(root_after, root_before);
        assert_ne!(nested_after, nested_before);
    }

    #[test]
    fn surface_divider_drag_ignores_outside_clicks_and_open_modal() {
        let mut app = App::default();
        app.menu_placement = MenuPlacement::Modal;
        add_surface_file_tabs(&mut app, 2);
        app.surface.apply_layout_preset(LayoutPreset::TwoByOne).unwrap();
        let mut buffer = TerminalBuffer::new(100, 24);
        let layout = render(&app, &mut buffer);
        let divider = layout
            .hits
            .iter()
            .find(|hit| matches!(hit.target, HitTarget::SurfaceDivider { .. }))
            .cloned()
            .unwrap();
        let original = app.surface.root.clone();
        app.layout = layout;

        app.click(0, 0);
        app.drag(20, 10);
        app.drop_at(20, 10);
        assert_eq!(app.surface.root, original);

        app.focus = Focus::Settings;
        app.click(divider.rect.x, divider.rect.y);
        app.drag(20, divider.rect.y);
        app.drop_at(20, divider.rect.y);
        assert_eq!(app.surface.root, original);
        assert!(!matches!(app.drag_state, Some(DragState::SurfaceDivider { .. })));
    }

    fn session_monitor_render_fixture(events: bool, detail: bool) -> App {
        let mut app = fixture(PtyColorMode::Inherited);
        let address = active_pty_address(&app);
        let incarnation_id = NodeIncarnationId::from_bytes([41; NODE_INCARNATION_ID_BYTES]);
        app.nodes[0].incarnation_id = Some(incarnation_id);
        app.set_session_monitor_support(
            address.node_id.clone(),
            incarnation_id,
            crate::app::SessionMonitorSupport {
                known: true,
                events,
                managed_target: events,
                workflow_detail: detail,
            },
        );
        app.open_session_monitor(AgentRowKey::Legacy(address));
        app
    }

    fn managed_dormant_monitor_fixture(managed_target: bool) -> App {
        let mut app = session_monitor_render_fixture(true, true);
        let incarnation = app.nodes[0].incarnation_id.unwrap();
        app.set_session_monitor_support(
            "node-a".to_owned(),
            incarnation,
            crate::app::SessionMonitorSupport {
                known: true,
                events: true,
                managed_target,
                workflow_detail: true,
            },
        );
        app.upsert_managed_session(crate::app::ManagedSessionView {
            node_id: "node-a".to_owned(),
            record_id: "record-dormant".to_owned(),
            display_name: "Dormant agent".to_owned(),
            provider: gate4agent_types::AgentId::new("codex").unwrap(),
            mode: SessionMode::Pty,
            state: gate4agent_node_protocol::ManagedSessionState::Dormant,
            workspace_id: "workspace-a".to_owned(),
            canonical_root: None,
            has_provider_session_identity: true,
            bundle: None,
            context_id: None,
            context: None,
            task_binding: None,
            active_session: None,
            last_error: None,
        });
        assert!(app.apply_managed_record_inventory("node-a", incarnation, true));
        app.open_session_monitor(AgentRowKey::Managed {
            node_id: "node-a".to_owned(),
            record_id: "record-dormant".to_owned(),
        });
        app
    }

    fn render_focused_monitor(app: &App, width: u16, height: u16) -> String {
        let mut buffer = TerminalBuffer::new(width, height);
        let mut layout = LayoutRects::default();
        let monitor = app.focused_session_monitor().unwrap();
        render_session_monitor(
            app,
            monitor,
            Rect::new(0, 0, width, height),
            &mut buffer,
            &mut layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        buffer_text(&buffer)
    }

    #[test]
    fn node_routes_render_exact_c2_fact_in_existing_launch_and_monitor_surfaces() {
        for (route, label) in [
            (C2RelayRoute::LocalIpc, "Local IPC"),
            (C2RelayRoute::SshForwardedLoopback, "SSH forwarded"),
            (C2RelayRoute::Unknown, "Unknown"),
        ] {
            let mut app = fixture(PtyColorMode::Inherited);
            app.nodes[0].relay_route = route;
            app.focus = Focus::Spawn;
            app.spawn = Some(SpawnDialog {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
                provider: provider("kimi"),
                target: LaunchTarget::ExistingWorkspace,
                profile_id: "default".to_owned(),
                worktree_profile_id: String::new(),
                bundle_id: String::new(),
                context_mode: LaunchContextMode::None,
                field: LaunchField::Node,
            });
            let mut buffer = TerminalBuffer::new(110, 28);
            render(&app, &mut buffer);
            let text = buffer_text(&buffer);
            assert!(text.contains(&format!("node-a | {label}")), "missing route label {label}");
        }

        let mut monitor = session_monitor_render_fixture(false, false);
        monitor.nodes[0].relay_route = C2RelayRoute::SshForwardedLoopback;
        assert!(render_focused_monitor(&monitor, 100, 18)
            .contains("Node route: SSH forwarded"));
    }

    fn render_monitor_observation(
        source_sequence: u64,
        evidence: gate4agent_node_protocol::ObservationEvidenceV1,
        kind: gate4agent_node_protocol::ObservationKindV1,
    ) -> gate4agent_node_protocol::ObservationV1 {
        gate4agent_node_protocol::ObservationV1 {
            source_sequence,
            observed_at_unix_ms: Some(10_000 + source_sequence),
            evidence,
            kind,
            truncated: false,
        }
    }

    fn apply_render_monitor_observation(
        app: &mut App,
        node_sequence: u64,
        observation: gate4agent_node_protocol::ObservationV1,
    ) {
        let key = match app.surface.active_tab().cloned().unwrap() {
            SurfaceTab::SessionMonitor(key) => key,
            other => panic!("expected Session Monitor, got {other:?}"),
        };
        let SessionMonitorTarget::Runtime { address, incarnation } = key.target else {
            panic!("expected runtime Session Monitor")
        };
        assert!(app.apply_session_observation(
            address.node_id.clone(),
            incarnation,
            node_sequence,
            address,
            observation,
        ));
    }

    #[test]
    fn session_monitor_existing_render_parity_uses_engine_projection() {
        let unavailable = session_monitor_render_fixture(false, false);
        let text = render_focused_monitor(&unavailable, 72, 12);
        assert!(text.contains("Observation events unavailable: capability not negotiated"), "{text}");

        let waiting = session_monitor_render_fixture(true, true);
        let text = render_focused_monitor(&waiting, 72, 12);
        assert!(text.contains("Waiting for observation events"), "{text}");

        let mut hint = session_monitor_render_fixture(true, true);
        apply_render_monitor_observation(
            &mut hint,
            1,
            render_monitor_observation(
                1,
                gate4agent_node_protocol::ObservationEvidenceV1::PtyHint,
                gate4agent_node_protocol::ObservationKindV1::Working,
            ),
        );
        let text = render_focused_monitor(&hint, 72, 12);
        assert!(text.contains("PTY hint is non-authoritative"), "{text}");
        assert!(!text.contains("Current: working"), "{text}");
    }

    #[test]
    fn session_monitor_renders_truthful_persistence_unavailable_status() {
        let mut app = session_monitor_render_fixture(true, true);
        app.mark_observation_persistence_unavailable("SQLite commit failed".to_owned());
        let text = render_focused_monitor(&app, 88, 12);
        assert!(
            text.contains("Persistence unavailable: SQLite commit failed"),
            "{text}",
        );
        assert!(!text.contains("Persistence: committed revision"), "{text}");
    }

    #[test]
    fn render_managed_dormant_monitor_without_runtime_or_task_inference() {
        let app = managed_dormant_monitor_fixture(true);

        let text = render_focused_monitor(&app, 88, 14);
        assert!(text.contains("Identity: managed node-a / @record-dormant"), "{text}");
        assert!(text.contains("Managed record: dormant | no active runtime"), "{text}");
        assert!(!text.contains("task"), "{text}");
        assert!(!text.contains("kanban"), "{text}");
    }

    #[test]
    fn managed_dormant_monitor_reports_managed_target_not_negotiated() {
        let app = managed_dormant_monitor_fixture(false);
        let text = render_focused_monitor(&app, 88, 14);
        assert!(
            text.contains("Managed target unavailable: capability not negotiated"),
            "{text}",
        );
        assert!(!text.contains("Waiting for observation events"), "{text}");
    }

    #[test]
    fn session_monitor_detail_gated_sections_never_claim_empty() {
        let mut app = session_monitor_render_fixture(true, false);
        apply_render_monitor_observation(
            &mut app,
            1,
            render_monitor_observation(
                1,
                gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider,
                gate4agent_node_protocol::ObservationKindV1::Working,
            ),
        );
        for section in [SessionMonitorSection::Workflow, SessionMonitorSection::FilesGit] {
            app.session_monitors.values_mut().next().unwrap().section = section;
            let text = render_focused_monitor(&app, 72, 12);
            assert!(text.contains("Workflow detail unavailable: capability not negotiated"), "{text}");
            assert!(!text.contains("No validated file-change paths"), "{text}");
            assert!(!text.contains("Todo snapshot not observed"), "{text}");
        }
    }

    #[test]
    fn session_monitor_source_capabilities_render_supported_unsupported_and_not_observed() {
        let mut app = session_monitor_render_fixture(true, true);
        apply_render_monitor_observation(
            &mut app,
            1,
            render_monitor_observation(
                1,
                gate4agent_node_protocol::ObservationEvidenceV1::ManagedHook,
                gate4agent_node_protocol::ObservationKindV1::SourceCapabilities {
                    source_family: gate4agent_node_protocol::ObservationSourceFamilyV1::ManagedHook,
                    source_adapter: "codex".to_owned(),
                    capabilities: gate4agent_node_protocol::ObservationCapabilitiesV1 {
                        tools: true,
                        attention: true,
                        ..gate4agent_node_protocol::ObservationCapabilitiesV1::default()
                    },
                },
            ),
        );

        app.session_monitors.values_mut().next().unwrap().section = SessionMonitorSection::Tools;
        let tools = render_focused_monitor(&app, 88, 12);
        assert!(tools.contains("Tool events: supported; no event observed"), "{tools}");
        assert!(tools.contains("Owned-process events: not supported by observed sources"), "{tools}");

        app.session_monitors.values_mut().next().unwrap().section = SessionMonitorSection::Subagents;
        let subagents = render_focused_monitor(&app, 88, 12);
        assert!(subagents.contains("Subagent events: not supported by observed sources"), "{subagents}");

        let mut waiting = session_monitor_render_fixture(true, true);
        apply_render_monitor_observation(
            &mut waiting,
            1,
            render_monitor_observation(
                1,
                gate4agent_node_protocol::ObservationEvidenceV1::PtyHint,
                gate4agent_node_protocol::ObservationKindV1::Working,
            ),
        );
        waiting.session_monitors.values_mut().next().unwrap().section = SessionMonitorSection::Usage;
        let usage = render_focused_monitor(&waiting, 88, 12);
        assert!(usage.contains("Usage events: not observed"), "{usage}");
    }

    #[test]
    fn session_monitor_context_bar_requires_exact_live_complete_fact() {
        let mut app = session_monitor_render_fixture(true, true);
        app.session_monitors.values_mut().next().unwrap().section = SessionMonitorSection::Usage;
        apply_render_monitor_observation(
            &mut app,
            1,
            render_monitor_observation(
                1,
                gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider,
                gate4agent_node_protocol::ObservationKindV1::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 20,
                    cache_write_tokens: 5,
                    reasoning_tokens: 3,
                    context_window: Some(100),
                    is_cumulative: true,
                },
            ),
        );
        let accounting = render_focused_monitor(&app, 112, 20);
        assert!(accounting.contains(
            "Context: not reported | usage accounting is not an exact current-window signal"
        ), "{accounting}");

        apply_render_monitor_observation(
            &mut app,
            2,
            render_monitor_observation(
                2,
                gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider,
                gate4agent_node_protocol::ObservationKindV1::ContextWindowUsage {
                    uncached_input_tokens: 10,
                    cache_read_tokens: 20,
                    cache_write_tokens: 5,
                    output_tokens: 15,
                    unattributed_tokens: 10,
                    used_tokens: 60,
                    capacity_tokens: 100,
                },
            ),
        );
        let mut buffer = TerminalBuffer::new(112, 20);
        let mut layout = LayoutRects::default();
        render_session_monitor(
            &app,
            app.focused_session_monitor().unwrap(),
            Rect::new(0, 0, 112, 20),
            &mut buffer,
            &mut layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        let exact = buffer_text(&buffer);
        assert!(exact.contains(
            "Persistence unavailable: durable observation store is starting | Node route: Unknown"
        ), "{exact}");
        assert!(exact.contains("Context: 60 / 100 tokens"), "{exact}");
        assert!(exact.contains("Uncached input: 10"), "{exact}");
        assert!(exact.contains("Unattributed: 10"), "{exact}");
        let segment_hits = layout.hits.iter().filter_map(|region| match &region.target {
            HitTarget::ContextUsageSegment(hit) => Some(hit.segment),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(segment_hits, vec![
            ContextUsageSegment::UncachedInput,
            ContextUsageSegment::CacheRead,
            ContextUsageSegment::CacheWrite,
            ContextUsageSegment::ProviderOutput,
            ContextUsageSegment::Unattributed,
            ContextUsageSegment::Remaining,
        ]);

        apply_render_monitor_observation(
            &mut app,
            3,
            render_monitor_observation(
                3,
                gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider,
                gate4agent_node_protocol::ObservationKindV1::ContextWindowUsage {
                    uncached_input_tokens: 50,
                    cache_read_tokens: 30,
                    cache_write_tokens: 20,
                    output_tokens: 10,
                    unattributed_tokens: 10,
                    used_tokens: 120,
                    capacity_tokens: 100,
                },
            ),
        );
        let over_capacity = render_focused_monitor(&app, 112, 20);
        assert!(over_capacity.contains(
            "Context: 120 / 100 tokens | over-capacity; exceeds reported window by 20"
        ), "{over_capacity}");

        apply_render_monitor_observation(
            &mut app,
            4,
            render_monitor_observation(
                4,
                gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider,
                gate4agent_node_protocol::ObservationKindV1::Gap { missed: 1 },
            ),
        );
        let after_gap = render_focused_monitor(&app, 112, 20);
        assert!(after_gap.contains("Context: not reported"), "{after_gap}");
    }

    #[test]
    fn session_monitor_exact_context_requires_structured_provider_evidence() {
        let mut app = session_monitor_render_fixture(true, true);
        app.session_monitors.values_mut().next().unwrap().section = SessionMonitorSection::Usage;
        apply_render_monitor_observation(
            &mut app,
            1,
            render_monitor_observation(
                1,
                gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider,
                gate4agent_node_protocol::ObservationKindV1::ContextWindowUsage {
                    uncached_input_tokens: 10,
                    cache_read_tokens: 20,
                    cache_write_tokens: 5,
                    output_tokens: 15,
                    unattributed_tokens: 10,
                    used_tokens: 60,
                    capacity_tokens: 100,
                },
            ),
        );
        let key = app.focused_session_monitor().unwrap().key.clone();
        let valid = app.session_monitor_projection(&key).unwrap().clone();
        let mut positive_buffer = TerminalBuffer::new(112, 20);
        let mut positive_layout = LayoutRects::default();
        render_session_monitor(
            &app,
            app.focused_session_monitor().unwrap(),
            Rect::new(0, 0, 112, 20),
            &mut positive_buffer,
            &mut positive_layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        assert!(positive_layout.hits.iter().any(|region| {
            matches!(&region.target, HitTarget::ContextUsageSegment(_))
        }));

        for evidence in [
            gate4agent_node_protocol::ObservationEvidenceV1::ManagedHook,
            gate4agent_node_protocol::ObservationEvidenceV1::NodeLifecycle,
            gate4agent_node_protocol::ObservationEvidenceV1::WorkspaceObservation,
            gate4agent_node_protocol::ObservationEvidenceV1::HistoryProjection,
            gate4agent_node_protocol::ObservationEvidenceV1::PtyHint,
        ] {
            let mut invalid = valid.clone();
            invalid
                .usage
                .context_occupancy
                .as_mut()
                .unwrap()
                .evidence = evidence;
            app.apply_observation_open_snapshot(vec![invalid], Vec::new());
            let mut buffer = TerminalBuffer::new(112, 20);
            let mut layout = LayoutRects::default();
            render_session_monitor(
                &app,
                app.focused_session_monitor().unwrap(),
                Rect::new(0, 0, 112, 20),
                &mut buffer,
                &mut layout,
                Theme::for_mode(PtyColorMode::Inherited),
            );
            let text = buffer_text(&buffer);
            assert!(text.contains(
                "Context: not reported | exact current-window source is not structured provider evidence"
            ), "{evidence:?}: {text}");
            assert!(!layout.hits.iter().any(|region| {
                matches!(&region.target, HitTarget::ContextUsageSegment(_))
            }), "{evidence:?}");
            app.layout = layout;
            app.context_usage_hover = None;
            assert_eq!(app.hover(1, 3), crate::app::AppAction::None);
            assert!(app.context_usage_hover.is_none(), "{evidence:?}");
        }
    }

    #[test]
    fn session_monitor_history_summary_distinguishes_unknown_tokens_from_zero() {
        let mut app = session_monitor_render_fixture(true, true);
        app.session_monitors.values_mut().next().unwrap().section = SessionMonitorSection::Usage;
        apply_render_monitor_observation(
            &mut app,
            1,
            render_monitor_observation(
                1,
                gate4agent_node_protocol::ObservationEvidenceV1::HistoryProjection,
                gate4agent_node_protocol::ObservationKindV1::SourceCapabilities {
                    source_family: gate4agent_node_protocol::ObservationSourceFamilyV1::History,
                    source_adapter: "native-history".to_owned(),
                    capabilities: gate4agent_node_protocol::ObservationCapabilitiesV1 {
                        history_summary: true,
                        ..gate4agent_node_protocol::ObservationCapabilitiesV1::default()
                    },
                },
            ),
        );
        apply_render_monitor_observation(
            &mut app,
            2,
            render_monitor_observation(
                1,
                gate4agent_node_protocol::ObservationEvidenceV1::HistoryProjection,
                gate4agent_node_protocol::ObservationKindV1::HistorySnapshot {
                    message_count: 17,
                    message_count_exact: false,
                    completed_turn_count: Some(4),
                    total_tokens: None,
                },
            ),
        );
        let unknown = render_focused_monitor(&app, 112, 18);
        assert!(
            unknown.contains(
                "History summary: messages 17 observed; total incomplete | completed turns 4 | total tokens unknown"
            ),
            "{unknown}",
        );

        apply_render_monitor_observation(
            &mut app,
            3,
            render_monitor_observation(
                2,
                gate4agent_node_protocol::ObservationEvidenceV1::HistoryProjection,
                gate4agent_node_protocol::ObservationKindV1::HistorySnapshot {
                    message_count: 17,
                    message_count_exact: true,
                    completed_turn_count: Some(4),
                    total_tokens: Some(0),
                },
            ),
        );
        let zero = render_focused_monitor(&app, 112, 18);
        assert!(
            zero.contains(
                "History summary: messages 17 | completed turns 4 | total tokens 0"
            ),
            "{zero}",
        );
    }

    #[test]
    fn session_monitor_section_hits_are_non_overlapping_and_in_bounds() {
        let app = session_monitor_render_fixture(true, true);
        let mut buffer = TerminalBuffer::new(120, 1);
        let mut layout = LayoutRects::default();
        let key = match app.surface.active_tab().unwrap() {
            SurfaceTab::SessionMonitor(key) => key,
            _ => unreachable!(),
        };
        let area = Rect::new(0, 0, 120, 1);
        render_session_monitor_toolbar(
            &app,
            PaneId(0),
            key,
            area,
            &mut buffer,
            &mut layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        let hits = layout.hits.iter().filter(|hit| matches!(
            hit.target,
            HitTarget::SessionMonitorSection(_, _)
        )).collect::<Vec<_>>();
        assert_eq!(hits.len(), SessionMonitorSection::ALL.len());
        assert!(hits.iter().all(|hit| hit.rect.x >= area.x
            && hit.rect.right() <= area.right()
            && hit.rect.y >= area.y
            && hit.rect.bottom() <= area.bottom()));
        for (index, left) in hits.iter().enumerate() {
            for right in hits.iter().skip(index + 1) {
                assert!(left.rect.right() <= right.rect.x || right.rect.right() <= left.rect.x);
            }
        }
    }

    #[test]
    fn session_monitor_narrow_layout_keeps_selected_section_reachable() {
        let mut app = session_monitor_render_fixture(true, true);
        app.session_monitors.values_mut().next().unwrap().section = SessionMonitorSection::Timeline;
        let key = match app.surface.active_tab().unwrap() {
            SurfaceTab::SessionMonitor(key) => key,
            _ => unreachable!(),
        };
        let mut buffer = TerminalBuffer::new(12, 1);
        let mut layout = LayoutRects::default();
        render_session_monitor_toolbar(
            &app,
            PaneId(0),
            key,
            Rect::new(0, 0, 12, 1),
            &mut buffer,
            &mut layout,
            Theme::for_mode(PtyColorMode::Inherited),
        );
        assert!(layout.hits.iter().any(|hit| matches!(
            hit.target,
            HitTarget::SessionMonitorSection(_, SessionMonitorSection::Timeline)
        ) && hit.rect.right() <= 12));
        assert!(buffer_text(&buffer).contains("Timeline"));
    }

    #[test]
    fn session_monitor_privacy_sentinel_absent() {
        let sentinel = "PRIVATE_PROMPT_TOOL_IO_AUTH_RAW_ID";
        let mut app = session_monitor_render_fixture(true, true);
        apply_render_monitor_observation(
            &mut app,
            1,
            render_monitor_observation(
                1,
                gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider,
                gate4agent_node_protocol::ObservationKindV1::ToolStarted {
                    correlation_id: sentinel.to_owned(),
                    class: "shell".to_owned(),
                },
            ),
        );
        apply_render_monitor_observation(
            &mut app,
            2,
            render_monitor_observation(
                2,
                gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider,
                gate4agent_node_protocol::ObservationKindV1::Error { detail: sentinel.to_owned() },
            ),
        );
        apply_render_monitor_observation(
            &mut app,
            3,
            render_monitor_observation(
                3,
                gate4agent_node_protocol::ObservationEvidenceV1::StructuredProvider,
                gate4agent_node_protocol::ObservationKindV1::TodoSnapshot {
                    revision: 1,
                    items: vec![gate4agent_node_protocol::ObservationTodoItemV1 {
                        id: None,
                        text: sentinel.to_owned(),
                        state: gate4agent_node_protocol::ObservationTodoStateV1::Pending,
                    }],
                    complete: false,
                },
            ),
        );
        let mut combined = String::new();
        for section in SessionMonitorSection::ALL {
            app.session_monitors.values_mut().next().unwrap().section = section;
            combined.push_str(&render_focused_monitor(&app, 88, 20));
        }
        assert!(!combined.contains(sentinel), "{combined}");
        assert!(!combined.contains('Р'), "{combined}");
        assert!(!combined.contains("В·"), "{combined}");
        assert!(!combined.contains('�'), "{combined}");
    }
}
