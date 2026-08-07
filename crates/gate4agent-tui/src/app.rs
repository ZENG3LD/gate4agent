use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use gate4agent_node_protocol::{
    GitWorktreeSnapshot, ManagedSessionState, SessionMode, WorkspaceEntryKind, WorkspaceId,
    WorkspaceInspection, MAX_SESSION_DISPLAY_NAME_BYTES,
    MAX_NODE_IDENTIFIER_BYTES, MAX_NODE_TEXT_BYTES, MAX_WORKSPACE_ROOT_BYTES,
};
use gate4agent_types::{
    TerminalControl, TerminalMouseProtocolEncoding, TERMINAL_INPUT_MAX_BYTES,
};
use uzor_tui::Rect;

const WHEEL_SCROLL_LINES: usize = 3;
const MIN_CONTROL_MODAL_WIDTH: u16 = 36;
const MIN_CONTROL_MODAL_HEIGHT: u16 = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    Claude,
    Codex,
    Kimi,
}

impl Provider {
    pub const ALL: [Self; 3] = [Self::Claude, Self::Codex, Self::Kimi];

    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Kimi => "kimi",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl std::str::FromStr for Provider {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "kimi" => Ok(Self::Kimi),
            _ => Err(format!("unsupported agent: {value}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PtyColorMode {
    #[default]
    Inherited,
    GateOverride,
}

impl PtyColorMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Inherited => Self::GateOverride,
            Self::GateOverride => Self::Inherited,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Inherited => "inherit",
            Self::GateOverride => "gate",
        }
    }
}

impl fmt::Display for PtyColorMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl std::str::FromStr for PtyColorMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "inherit" | "inherited" => Ok(Self::Inherited),
            "gate" | "override" => Ok(Self::GateOverride),
            _ => Err(format!("unsupported style: {value}")),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Resyncing,
    Disconnected(String),
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionAddress {
    pub node_id: String,
    pub workspace_id: String,
    pub instance_id: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionView {
    pub address: SessionAddress,
    pub provider: Provider,
    pub status: String,
    pub running: bool,
    pub stoppable: bool,
    pub removable: bool,
    pub restartable: bool,
    pub attention: bool,
    pub has_provider_session_identity: bool,
    pub terminal_formatted: Vec<u8>,
    pub terminal_scrollback: Vec<Vec<u8>>,
    pub terminal_alternate_screen: bool,
    pub terminal_mouse_protocol_enabled: bool,
    pub terminal_mouse_protocol_encoding: TerminalMouseProtocolEncoding,
    pub terminal_cursor: Option<(u16, u16)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedSessionView {
    pub node_id: String,
    pub record_id: String,
    pub display_name: String,
    pub provider: Provider,
    pub mode: SessionMode,
    pub state: ManagedSessionState,
    pub workspace_id: String,
    pub canonical_root: String,
    pub has_provider_session_identity: bool,
    pub active_session: Option<SessionAddress>,
    pub last_error: Option<String>,
}

impl ManagedSessionView {
    pub fn short_title(&self) -> &str {
        &self.display_name
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AgentRowKey {
    Managed { node_id: String, record_id: String },
    Legacy(SessionAddress),
}

impl SessionView {
    pub fn short_title(&self) -> String {
        format!("{} #{}", self.provider, self.address.instance_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderInventory {
    pub provider: Provider,
    pub enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceView {
    pub workspace_id: String,
    pub label: String,
    pub canonical_root: String,
    pub providers: Vec<ProviderInventory>,
    pub sessions: Vec<SessionView>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeView {
    pub node_id: String,
    pub endpoint: String,
    pub connection: ConnectionState,
    pub controller_owned: bool,
    pub event_sequence: u64,
    pub workspaces: Vec<WorkspaceView>,
    pub session_records: Vec<ManagedSessionView>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTab {
    pub address: SessionAddress,
}

pub const MAX_GRID_PANES: usize = 4;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SurfaceMode {
    #[default]
    Tab,
    Grid,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GridPreset {
    #[default]
    Quad,
    Columns,
    Rows,
}

impl GridPreset {
    pub const ALL: [Self; 3] = [Self::Quad, Self::Columns, Self::Rows];

    pub fn id(self) -> &'static str {
        match self {
            Self::Quad => "2x2",
            Self::Columns => "1x4",
            Self::Rows => "4x1",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GridAxisKind {
    Columns,
    Rows,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GridPane {
    pub address: SessionAddress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GridState {
    pub panes: Vec<GridPane>,
    pub focused: usize,
    pub preset: GridPreset,
    pub column_cuts: [u16; 3],
    pub row_cuts: [u16; 3],
}

impl Default for GridState {
    fn default() -> Self {
        Self {
            panes: Vec::new(),
            focused: 0,
            preset: GridPreset::Quad,
            column_cuts: [2_500, 5_000, 7_500],
            row_cuts: [2_500, 5_000, 7_500],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DragSource {
    Tab(usize),
    Agent(usize),
    Pane(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    Spaces,
    Agents,
    Tabs,
    Viewport,
    Spawn,
    AddSpace,
    CreateWorktree,
    RemoveWorktree,
    RenameSession,
    ForgetSession,
    Settings,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SidebarMode {
    #[default]
    Files,
    Git,
}

impl SidebarMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Git => "git",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RosterMode {
    #[default]
    Agents,
    Workspaces,
}

impl RosterMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Workspaces => "workspaces",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MenuPlacement {
    #[default]
    Sidebar,
    Modal,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SidebarPresentation {
    #[default]
    Split,
    Activity,
}

impl SidebarPresentation {
    pub fn id(self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::Activity => "activity",
        }
    }
}

impl MenuPlacement {
    pub fn id(self) -> &'static str {
        match self {
            Self::Sidebar => "sidebar",
            Self::Modal => "modal",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ControlSection {
    #[default]
    Files,
    Git,
    Agents,
    Workspaces,
    Settings,
}

impl ControlSection {
    pub const ALL: [Self; 5] = [
        Self::Files,
        Self::Git,
        Self::Agents,
        Self::Workspaces,
        Self::Settings,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Git => "git",
            Self::Agents => "agents",
            Self::Workspaces => "workspaces",
            Self::Settings => "settings",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DragState {
    ControlModal {
        grab_column: u16,
        grab_row: u16,
        origin_x: u16,
        origin_y: u16,
    },
    ControlModalResize {
        start_column: u16,
        start_row: u16,
        origin_width: u16,
        origin_height: u16,
    },
    SidebarWidth,
    SidebarSplit,
    SessionChip {
        source: DragSource,
        address: SessionAddress,
        start_column: u16,
        start_row: u16,
        current_column: u16,
        current_row: u16,
        moved: bool,
    },
    AgentSelection {
        key: AgentRowKey,
        start_column: u16,
        start_row: u16,
        moved: bool,
    },
    GridDivider {
        axis: GridAxisKind,
        index: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnDialog {
    pub node_id: String,
    pub workspace_id: String,
    pub provider: Provider,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddSpaceField {
    WorkspaceId,
    Root,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddSpaceDialog {
    pub node_id: String,
    pub workspace_id: String,
    pub root: String,
    pub field: AddSpaceField,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreateWorktreeField {
    WorkspaceId,
    TargetRoot,
    Branch,
    Base,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateWorktreeDialog {
    pub node_id: String,
    pub source_workspace_id: String,
    pub workspace_id: String,
    pub target_root: String,
    pub branch: String,
    pub base: String,
    pub field: CreateWorktreeField,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoveWorktreeDialog {
    pub node_id: String,
    pub source_workspace_id: String,
    pub target_root: String,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenameSessionDialog {
    pub node_id: String,
    pub record_id: String,
    pub original_name: String,
    pub display_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForgetSessionDialog {
    pub node_id: String,
    pub record_id: String,
    pub display_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiKey {
    Char(char),
    Ctrl(char),
    OperatorEscape,
    TerminalBytes(Vec<u8>),
    Enter,
    Escape,
    Backspace,
    Insert,
    Delete,
    Home,
    End,
    Up,
    Down,
    Left,
    Right,
    Tab,
    BackTab,
    PageUp,
    PageDown,
    Function(u8),
    UnsupportedModifier,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppAction {
    None,
    Quit,
    Spawn {
        node_id: String,
        workspace_id: String,
        provider: Provider,
        rows: u16,
        cols: u16,
    },
    Resume {
        address: SessionAddress,
        rows: u16,
        cols: u16,
    },
    ResumeSessionRecord {
        node_id: String,
        record_id: String,
        rows: u16,
        cols: u16,
    },
    RenameSessionRecord {
        node_id: String,
        record_id: String,
        display_name: String,
    },
    ForgetSessionRecord {
        node_id: String,
        record_id: String,
    },
    Input { address: SessionAddress, text: String },
    Paste { address: SessionAddress, text: String },
    TerminalControl { address: SessionAddress, control: TerminalControl },
    TerminalBytes { address: SessionAddress, bytes: Vec<u8> },
    Resize { address: SessionAddress, rows: u16, cols: u16 },
    Stop { address: SessionAddress, force: bool },
    Remove { address: SessionAddress },
    RegisterWorkspace { node_id: String, workspace_id: String, root: String },
    UnregisterWorkspace { node_id: String, workspace_id: String },
    CreateWorktree {
        node_id: String,
        source_workspace_id: String,
        workspace_id: String,
        target_root: String,
        branch: String,
        base: Option<String>,
    },
    RemoveWorktree {
        node_id: String,
        source_workspace_id: String,
        target_root: String,
    },
    InspectWorkspace { node_id: String, workspace_id: String },
    Resync { node_id: String, after_sequence: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HitTarget {
    SidebarMode(SidebarMode),
    RosterMode(RosterMode),
    Space(usize),
    SpawnSpace(usize),
    AddSpace,
    RemoveSpace,
    RefreshWorkspace,
    CreateWorktree,
    Worktree(usize),
    RegisterWorktree(usize),
    RemoveWorktree(usize),
    SidebarItem(usize),
    Agent(usize),
    AddAgent,
    Tab(usize),
    AddTab,
    GridToggle,
    GridPreset(GridPreset),
    GridPaneHeader(usize),
    GridPaneBody(usize),
    GridDropSlot(usize),
    TabDrop,
    GridDivider(GridAxisKind, usize),
    Settings,
    ControlSection(ControlSection),
    SettingsStyle,
    SettingsPlacement,
    SettingsPresentation,
    SettingsSidebarCollapsed,
    ActivitySection(ControlSection),
    SidebarCollapse,
    ControlDrag,
    ControlResize,
    SidebarWidthDrag,
    SidebarSplitDrag,
    Viewport,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HitRegion {
    pub rect: Rect,
    pub target: HitTarget,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GridPaneLayout {
    pub pane_index: usize,
    pub frame: Rect,
    pub header: Rect,
    pub viewport: Rect,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LayoutRects {
    pub activity_rail: Rect,
    pub spaces: Rect,
    pub agents: Rect,
    pub tabs: Rect,
    pub viewport: Rect,
    pub control_content: Rect,
    pub control_modal: Rect,
    pub grid_panes: Vec<GridPaneLayout>,
    pub grid_drop: Rect,
    pub tab_drop: Rect,
    pub hits: Vec<HitRegion>,
}

#[derive(Clone, Debug)]
pub struct App {
    pub nodes: Vec<NodeView>,
    pub selected_space: usize,
    pub selected_agent: usize,
    pub tabs: Vec<SessionTab>,
    pub pending_open: Vec<SessionAddress>,
    pub pending_space: Option<(String, String)>,
    pub selected_tab: usize,
    pub surface_mode: SurfaceMode,
    pub grid: GridState,
    pub sidebar_mode: SidebarMode,
    pub roster_mode: RosterMode,
    pub files_cursor: usize,
    pub git_cursor: usize,
    pub files_scroll: usize,
    pub git_scroll: usize,
    pub agents_scroll: usize,
    pub workspaces_scroll: usize,
    pub terminal_scroll_offsets: BTreeMap<SessionAddress, usize>,
    pub collapsed_directories: BTreeSet<(String, String, String)>,
    pub menu_placement: MenuPlacement,
    pub sidebar_presentation: SidebarPresentation,
    pub sidebar_collapsed: bool,
    pub control_section: ControlSection,
    pub settings_return_focus: Focus,
    pub control_modal_position: Option<(u16, u16)>,
    pub control_modal_size: Option<(u16, u16)>,
    pub sidebar_width: u16,
    pub sidebar_split_percent: u16,
    pub drag_state: Option<DragState>,
    pub workspace_inspections: BTreeMap<(String, String), WorkspaceInspection>,
    pub inspection_pending: Option<(String, String)>,
    pub focus: Focus,
    pub spawn: Option<SpawnDialog>,
    pub add_space: Option<AddSpaceDialog>,
    pub create_worktree: Option<CreateWorktreeDialog>,
    pub remove_worktree: Option<RemoveWorktreeDialog>,
    pub rename_session: Option<RenameSessionDialog>,
    pub forget_session: Option<ForgetSessionDialog>,
    pub color_mode: PtyColorMode,
    pub notice: Option<String>,
    pub terminal_rows: u16,
    pub terminal_cols: u16,
    pub layout: LayoutRects,
    pub should_quit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            selected_space: 0,
            selected_agent: 0,
            tabs: Vec::new(),
            pending_open: Vec::new(),
            pending_space: None,
            selected_tab: 0,
            surface_mode: SurfaceMode::Tab,
            grid: GridState::default(),
            sidebar_mode: SidebarMode::Files,
            roster_mode: RosterMode::Agents,
            files_cursor: 0,
            git_cursor: 0,
            files_scroll: 0,
            git_scroll: 0,
            agents_scroll: 0,
            workspaces_scroll: 0,
            terminal_scroll_offsets: BTreeMap::new(),
            collapsed_directories: BTreeSet::new(),
            menu_placement: MenuPlacement::Sidebar,
            sidebar_presentation: SidebarPresentation::Split,
            sidebar_collapsed: false,
            control_section: ControlSection::Files,
            settings_return_focus: Focus::Tabs,
            control_modal_position: None,
            control_modal_size: None,
            sidebar_width: 26,
            sidebar_split_percent: 50,
            drag_state: None,
            workspace_inspections: BTreeMap::new(),
            inspection_pending: None,
            focus: Focus::Spaces,
            spawn: None,
            add_space: None,
            create_worktree: None,
            remove_worktree: None,
            rename_session: None,
            forget_session: None,
            color_mode: PtyColorMode::Inherited,
            notice: None,
            terminal_rows: 24,
            terminal_cols: 80,
            layout: LayoutRects::default(),
            should_quit: false,
        }
    }
}

impl App {
    pub fn space_rows(&self) -> Vec<(usize, usize)> {
        let mut rows = Vec::new();
        for (node_index, node) in self.nodes.iter().enumerate() {
            for workspace_index in 0..node.workspaces.len() {
                rows.push((node_index, workspace_index));
            }
        }
        rows
    }

    pub fn selected_workspace(&self) -> Option<(&NodeView, &WorkspaceView)> {
        let (node_index, workspace_index) = *self.space_rows().get(self.selected_space)?;
        let node = self.nodes.get(node_index)?;
        Some((node, node.workspaces.get(workspace_index)?))
    }

    pub fn selected_workspace_inspection(&self) -> Option<&WorkspaceInspection> {
        let (node, workspace) = self.selected_workspace()?;
        self.workspace_inspections
            .get(&(node.node_id.clone(), workspace.workspace_id.clone()))
    }

    pub fn selected_workspace_route(&self) -> Option<(String, String)> {
        let (node, workspace) = self.selected_workspace()?;
        Some((node.node_id.clone(), workspace.workspace_id.clone()))
    }

    pub fn visible_workspace_entry_indices(&self) -> Vec<usize> {
        let Some((node_id, workspace_id)) = self.selected_workspace_route() else {
            return Vec::new();
        };
        let Some(inspection) = self.selected_workspace_inspection() else {
            return Vec::new();
        };
        inspection
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                (!self.entry_has_collapsed_ancestor(
                    &node_id,
                    &workspace_id,
                    &entry.relative_path,
                ))
                .then_some(index)
            })
            .collect()
    }

    pub fn directory_is_collapsed(&self, relative_path: &str) -> bool {
        self.selected_workspace_route().is_some_and(|(node_id, workspace_id)| {
            self.collapsed_directories.contains(&(
                node_id,
                workspace_id,
                relative_path.to_owned(),
            ))
        })
    }

    fn entry_has_collapsed_ancestor(
        &self,
        node_id: &str,
        workspace_id: &str,
        relative_path: &str,
    ) -> bool {
        let components = relative_path.split('/').collect::<Vec<_>>();
        let mut ancestor = String::new();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            if !ancestor.is_empty() {
                ancestor.push('/');
            }
            ancestor.push_str(component);
            if self.collapsed_directories.contains(&(
                node_id.to_owned(),
                workspace_id.to_owned(),
                ancestor.clone(),
            )) {
                return true;
            }
        }
        false
    }

    pub fn workspace_inspection_pending(&self) -> bool {
        let Some((node, workspace)) = self.selected_workspace() else {
            return false;
        };
        self.inspection_pending
            .as_ref()
            .is_some_and(|(node_id, workspace_id)| {
                *node_id == node.node_id && *workspace_id == workspace.workspace_id
            })
    }

    pub fn workspace_inspection_visible(&self) -> bool {
        self.menu_placement == MenuPlacement::Sidebar
            || (self.menu_placement == MenuPlacement::Modal
                && self.focus == Focus::Settings
                && matches!(self.control_section, ControlSection::Files | ControlSection::Git))
    }

    pub fn apply_workspace_inspection(
        &mut self,
        node_id: String,
        inspection: WorkspaceInspection,
    ) {
        let workspace_id = inspection.workspace_id.to_string();
        self.workspace_inspections
            .insert((node_id.clone(), workspace_id.clone()), inspection);
        self.files_cursor = self
            .files_cursor
            .min(self.visible_workspace_entry_indices().len().saturating_sub(1));
        self.git_cursor = self.git_cursor.min(self.git_item_count().saturating_sub(1));
        self.files_scroll = self
            .files_scroll
            .min(self.visible_workspace_entry_indices().len().saturating_sub(1));
        self.git_scroll = self.git_scroll.min(self.git_item_count().saturating_sub(1));
        if self
            .inspection_pending
            .as_ref()
            .is_some_and(|pending| *pending == (node_id, workspace_id))
        {
            self.inspection_pending = None;
        }
    }

    pub fn fail_workspace_inspection(
        &mut self,
        node_id: String,
        workspace_id: String,
        message: String,
    ) {
        if self
            .inspection_pending
            .as_ref()
            .is_some_and(|pending| pending == &(node_id.clone(), workspace_id.clone()))
        {
            self.inspection_pending = None;
        }
        self.notice = Some(format!("{node_id}/{workspace_id}: {message}"));
    }

    pub fn selected_agent_session(&self) -> Option<&SessionView> {
        let key = self.agent_rows().get(self.selected_agent)?.clone();
        let address = self.agent_row_active_address(&key)?;
        self.find_session(&address)
    }

    pub fn selected_managed_session(&self) -> Option<&ManagedSessionView> {
        let key = self.agent_rows().get(self.selected_agent)?.clone();
        self.find_managed_session(&key)
    }

    pub fn selected_tab_session(&self) -> Option<&SessionView> {
        self.find_session(&self.tabs.get(self.selected_tab)?.address)
    }

    pub fn focused_address(&self) -> Option<&SessionAddress> {
        match self.surface_mode {
            SurfaceMode::Tab => self.tabs.get(self.selected_tab).map(|tab| &tab.address),
            SurfaceMode::Grid => self.grid.panes.get(self.grid.focused).map(|pane| &pane.address),
        }
    }

    pub fn focused_session(&self) -> Option<&SessionView> {
        self.find_session(self.focused_address()?)
    }

    pub fn focused_terminal_rect(&self) -> Rect {
        match self.surface_mode {
            SurfaceMode::Tab => self.layout.viewport,
            SurfaceMode::Grid => self
                .layout
                .grid_panes
                .iter()
                .find(|pane| pane.pane_index == self.grid.focused)
                .map(|pane| pane.viewport)
                .unwrap_or(self.layout.viewport),
        }
    }

    pub fn desired_terminal_sizes(&self) -> Vec<(SessionAddress, u16, u16)> {
        match self.surface_mode {
            SurfaceMode::Tab => self
                .focused_session()
                .filter(|session| session.running)
                .map(|session| {
                    let rect = self.focused_terminal_rect();
                    (session.address.clone(), rect.height.max(1), rect.width.max(1))
                })
                .into_iter()
                .collect(),
            SurfaceMode::Grid => self
                .grid
                .panes
                .iter()
                .enumerate()
                .filter_map(|(pane_index, pane)| {
                    if !self.find_session(&pane.address).is_some_and(|session| session.running) {
                        return None;
                    }
                    let rect = self
                        .layout
                        .grid_panes
                        .iter()
                        .find(|layout| layout.pane_index == pane_index)?
                        .viewport;
                    Some((pane.address.clone(), rect.height.max(1), rect.width.max(1)))
                })
                .collect(),
        }
    }

    pub fn find_session(&self, address: &SessionAddress) -> Option<&SessionView> {
        self.nodes
            .iter()
            .find(|node| node.node_id == address.node_id)?
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == address.workspace_id)?
            .sessions
            .iter()
            .find(|session| session.address == *address)
    }

    pub fn session_title(&self, address: &SessionAddress) -> Option<String> {
        if let Some(record) = self
            .nodes
            .iter()
            .filter(|node| node.node_id == address.node_id)
            .flat_map(|node| node.session_records.iter())
            .find(|record| record.active_session.as_ref() == Some(address))
        {
            return Some(record.display_name.clone());
        }
        self.find_session(address).map(SessionView::short_title)
    }

    pub fn find_managed_session(&self, key: &AgentRowKey) -> Option<&ManagedSessionView> {
        let AgentRowKey::Managed { node_id, record_id } = key else {
            return None;
        };
        self.nodes
            .iter()
            .find(|node| node.node_id == *node_id)?
            .session_records
            .iter()
            .find(|record| record.record_id == *record_id)
    }

    pub fn agent_row_active_address(&self, key: &AgentRowKey) -> Option<SessionAddress> {
        match key {
            AgentRowKey::Managed { .. } => self.find_managed_session(key)?.active_session.clone(),
            AgentRowKey::Legacy(address) => Some(address.clone()),
        }
    }

    fn node_is_connected(&self, node_id: &str) -> bool {
        self.nodes
            .iter()
            .find(|node| node.node_id == node_id)
            .is_some_and(|node| matches!(node.connection, ConnectionState::Connected))
    }

    fn address_is_connected(&self, address: &SessionAddress) -> bool {
        self.node_is_connected(&address.node_id)
    }

    pub fn agent_addresses(&self) -> Vec<SessionAddress> {
        let mut sessions = self
            .nodes
            .iter()
            .flat_map(|node| node.workspaces.iter())
            .flat_map(|workspace| workspace.sessions.iter())
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            (
                !left.attention,
                !left.running,
                left.provider.id(),
                left.address.node_id.as_str(),
                left.address.workspace_id.as_str(),
                left.address.instance_id,
            )
                .cmp(&(
                    !right.attention,
                    !right.running,
                    right.provider.id(),
                    right.address.node_id.as_str(),
                    right.address.workspace_id.as_str(),
                    right.address.instance_id,
                ))
        });
        sessions.into_iter().map(|session| session.address.clone()).collect()
    }

    pub fn agent_rows(&self) -> Vec<AgentRowKey> {
        let managed_addresses = self
            .nodes
            .iter()
            .flat_map(|node| node.session_records.iter())
            .filter_map(|record| record.active_session.clone())
            .collect::<BTreeSet<_>>();
        let mut rows = self
            .nodes
            .iter()
            .flat_map(|node| {
                node.session_records.iter().map(|record| AgentRowKey::Managed {
                    node_id: node.node_id.clone(),
                    record_id: record.record_id.clone(),
                })
            })
            .chain(
                self.agent_addresses()
                    .into_iter()
                    .filter(|address| !managed_addresses.contains(address))
                    .map(AgentRowKey::Legacy),
            )
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| self.agent_row_sort_key(left).cmp(&self.agent_row_sort_key(right)));
        rows
    }

    fn agent_row_sort_key(&self, key: &AgentRowKey) -> (bool, u8, String, String, String) {
        if let Some(record) = self.find_managed_session(key) {
            let attention = record
                .active_session
                .as_ref()
                .and_then(|address| self.find_session(address))
                .is_some_and(|session| session.attention);
            let state = match record.state {
                ManagedSessionState::Live => 0,
                ManagedSessionState::IdentityPending => 1,
                ManagedSessionState::Dormant => 2,
                ManagedSessionState::Unavailable => 3,
            };
            return (
                !attention,
                state,
                record.provider.id().to_owned(),
                record.display_name.to_ascii_lowercase(),
                record.record_id.clone(),
            );
        }
        let AgentRowKey::Legacy(address) = key else {
            unreachable!("managed row must resolve")
        };
        let session = self.find_session(address);
        (
            !session.is_some_and(|session| session.attention),
            if session.is_some_and(|session| session.running) { 0 } else { 3 },
            session.map(|session| session.provider.id()).unwrap_or("unknown").to_owned(),
            format!("{:020}", address.instance_id),
            format!("{}:{}", address.node_id, address.workspace_id),
        )
    }

    pub fn upsert_node(&mut self, node: NodeView) {
        let previous_scrollback = self
            .terminal_scroll_offsets
            .keys()
            .filter(|address| address.node_id == node.node_id)
            .filter_map(|address| {
                self.find_session(address)
                    .map(|session| (address.clone(), session.terminal_scrollback.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        let selected_space = self.selected_workspace().map(|(node, workspace)| {
            (node.node_id.clone(), workspace.workspace_id.clone())
        });
        let selected_agent = self.agent_rows().get(self.selected_agent).cloned();
        for tab in self.tabs.iter_mut().filter(|tab| tab.address.node_id == node.node_id) {
            let Some(workspace) = node
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == tab.address.workspace_id)
            else {
                continue;
            };
            let mut generations = workspace
                .sessions
                .iter()
                .filter(|session| session.address.instance_id == tab.address.instance_id);
            let Some(rebound) = generations.next() else {
                continue;
            };
            if generations.next().is_none() {
                tab.address.generation = rebound.address.generation;
            }
        }
        for pane in self
            .grid
            .panes
            .iter_mut()
            .filter(|pane| pane.address.node_id == node.node_id)
        {
            let Some(workspace) = node
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == pane.address.workspace_id)
            else {
                continue;
            };
            let mut generations = workspace
                .sessions
                .iter()
                .filter(|session| session.address.instance_id == pane.address.instance_id);
            let Some(rebound) = generations.next() else {
                continue;
            };
            if generations.next().is_none() {
                pane.address.generation = rebound.address.generation;
            }
        }
        let selected_tab = self.tabs.get(self.selected_tab).map(|tab| tab.address.clone());
        let focused_pane = self
            .grid
            .panes
            .get(self.grid.focused)
            .map(|pane| pane.address.clone());
        if let Some(existing) = self.nodes.iter_mut().find(|item| item.node_id == node.node_id) {
            *existing = node;
        } else {
            self.nodes.push(node);
        }
        let tabs = std::mem::take(&mut self.tabs);
        self.tabs = tabs
            .into_iter()
            .filter(|tab| self.find_session(&tab.address).is_some())
            .collect();
        let panes = std::mem::take(&mut self.grid.panes);
        self.grid.panes = panes
            .into_iter()
            .filter(|pane| self.find_session(&pane.address).is_some())
            .collect();
        let grid_addresses = self
            .grid
            .panes
            .iter()
            .map(|pane| pane.address.clone())
            .collect::<BTreeSet<_>>();
        self.tabs.retain(|tab| !grid_addresses.contains(&tab.address));
        if let Some(index) = selected_tab
            .as_ref()
            .and_then(|address| self.tabs.iter().position(|tab| &tab.address == address))
        {
            self.selected_tab = index;
        } else {
            let selected_was_removed = selected_tab.is_some();
            self.selected_tab = self.selected_tab.min(self.tabs.len().saturating_sub(1));
            if selected_was_removed
                && self.surface_mode == SurfaceMode::Tab
                && self.focus == Focus::Viewport
            {
                self.focus = if self.tabs.is_empty() && self.menu_placement == MenuPlacement::Sidebar {
                    self.sidebar_focus()
                } else {
                    Focus::Tabs
                };
                self.notice = Some("active PTY disappeared; input target cleared".to_owned());
            }
        }
        if let Some(index) = focused_pane.as_ref().and_then(|address| {
            self.grid
                .panes
                .iter()
                .position(|pane| &pane.address == address)
        }) {
            self.grid.focused = index;
        } else {
            let focused_was_removed = focused_pane.is_some();
            self.grid.focused = self.grid.focused.min(self.grid.panes.len().saturating_sub(1));
            if focused_was_removed
                && self.surface_mode == SurfaceMode::Grid
                && self.focus == Focus::Viewport
            {
                if self.grid.panes.is_empty() {
                    self.surface_mode = SurfaceMode::Tab;
                    self.focus = if self.tabs.is_empty()
                        && self.menu_placement == MenuPlacement::Sidebar
                    {
                        self.sidebar_focus()
                    } else {
                        Focus::Tabs
                    };
                    self.notice = Some("active grid PTY disappeared; input target cleared".to_owned());
                } else {
                    self.notice = Some("active grid PTY disappeared; nearest pane focused".to_owned());
                }
            }
        }
        if let Some((node_id, workspace_id)) = selected_space {
            if let Some(index) = self.space_rows().iter().position(|(node_index, workspace_index)| {
                self.nodes[*node_index].node_id == node_id
                    && self.nodes[*node_index].workspaces[*workspace_index].workspace_id == workspace_id
            }) {
                self.selected_space = index;
            } else {
                self.selected_space = self.selected_space.min(self.space_rows().len().saturating_sub(1));
            }
        } else {
            self.selected_space = self.selected_space.min(self.space_rows().len().saturating_sub(1));
        }
        if let Some(key) = selected_agent {
            let key = match key {
                AgentRowKey::Managed { node_id, record_id } => {
                    AgentRowKey::Managed { node_id, record_id }
                }
                AgentRowKey::Legacy(address) => self
                    .rebound_address(&address)
                    .map(AgentRowKey::Legacy)
                    .unwrap_or(AgentRowKey::Legacy(address)),
            };
            if let Some(index) = self.agent_rows().iter().position(|candidate| *candidate == key) {
                self.selected_agent = index;
            }
        } else {
            self.selected_agent = self.selected_agent.min(self.agent_rows().len().saturating_sub(1));
        }
        self.reconcile_pending_space();
        self.reconcile_pending_open();
        let live_workspaces = self
            .nodes
            .iter()
            .flat_map(|node| {
                node.workspaces
                    .iter()
                    .map(|workspace| (node.node_id.clone(), workspace.workspace_id.clone()))
            })
            .collect::<BTreeSet<_>>();
        self.workspace_inspections.retain(|(node_id, workspace_id), _| {
            live_workspaces.contains(&(node_id.clone(), workspace_id.clone()))
        });
        self.collapsed_directories.retain(|(node_id, workspace_id, _)| {
            live_workspaces.contains(&(node_id.clone(), workspace_id.clone()))
        });
        self.files_cursor = self
            .files_cursor
            .min(self.visible_workspace_entry_indices().len().saturating_sub(1));
        self.git_cursor = self.git_cursor.min(self.git_item_count().saturating_sub(1));
        self.reconcile_terminal_scroll_offsets(&previous_scrollback);
        self.reconcile_session_drag();
    }

    fn rebound_address(&self, address: &SessionAddress) -> Option<SessionAddress> {
        if self.find_session(address).is_some() {
            return Some(address.clone());
        }
        let mut candidates = self
            .nodes
            .iter()
            .filter(|node| node.node_id == address.node_id)
            .flat_map(|node| node.workspaces.iter())
            .filter(|workspace| workspace.workspace_id == address.workspace_id)
            .flat_map(|workspace| workspace.sessions.iter())
            .filter(|session| session.address.instance_id == address.instance_id);
        let rebound = candidates.next()?.address.clone();
        candidates.next().is_none().then_some(rebound)
    }

    pub fn set_node_connection(
        &mut self,
        expected_node_id: &str,
        endpoint: &str,
        state: ConnectionState,
    ) {
        if let Some(node) = self.nodes.iter_mut().find(|node| node.node_id == expected_node_id) {
            if !endpoint.is_empty() {
                node.endpoint = endpoint.to_owned();
            }
            node.connection = state;
        } else {
            self.nodes.push(NodeView {
                node_id: expected_node_id.to_owned(),
                endpoint: endpoint.to_owned(),
                connection: state,
                controller_owned: false,
                event_sequence: 0,
                workspaces: Vec::new(),
                session_records: Vec::new(),
            });
        }
        self.reconcile_session_drag();
    }

    pub fn remove_topology_node(&mut self, node_id: &str) {
        let selected_space = self.selected_workspace_route();
        let selected_agent = self.agent_rows().get(self.selected_agent).cloned();
        let selected_tab = self.tabs.get(self.selected_tab).map(|tab| tab.address.clone());
        let focused_pane = self
            .grid
            .panes
            .get(self.grid.focused)
            .map(|pane| pane.address.clone());
        let selected_space_removed = selected_space
            .as_ref()
            .is_some_and(|(selected_node_id, _)| selected_node_id == node_id);
        let selected_agent_removed = selected_agent.as_ref().is_some_and(|key| match key {
            AgentRowKey::Managed { node_id: selected_node_id, .. } => selected_node_id == node_id,
            AgentRowKey::Legacy(address) => address.node_id == node_id,
        });
        let selected_tab_removed = selected_tab
            .as_ref()
            .is_some_and(|address| address.node_id == node_id);
        let focused_pane_removed = focused_pane
            .as_ref()
            .is_some_and(|address| address.node_id == node_id);

        self.nodes.retain(|node| node.node_id != node_id);
        self.tabs.retain(|tab| tab.address.node_id != node_id);
        self.grid.panes.retain(|pane| pane.address.node_id != node_id);
        self.pending_open.retain(|address| address.node_id != node_id);
        self.terminal_scroll_offsets
            .retain(|address, _| address.node_id != node_id);
        self.workspace_inspections
            .retain(|(inspection_node_id, _), _| inspection_node_id != node_id);
        self.collapsed_directories
            .retain(|(directory_node_id, _, _)| directory_node_id != node_id);
        if self.pending_space.as_ref().is_some_and(|(pending_node_id, _)| pending_node_id == node_id) {
            self.pending_space = None;
        }
        if self
            .inspection_pending
            .as_ref()
            .is_some_and(|(pending_node_id, _)| pending_node_id == node_id)
        {
            self.inspection_pending = None;
        }

        let removed_modal_focus = match self.focus {
            Focus::Spawn => self.spawn.as_ref().is_some_and(|dialog| dialog.node_id == node_id),
            Focus::AddSpace => self.add_space.as_ref().is_some_and(|dialog| dialog.node_id == node_id),
            Focus::CreateWorktree => self
                .create_worktree
                .as_ref()
                .is_some_and(|dialog| dialog.node_id == node_id),
            Focus::RemoveWorktree => self
                .remove_worktree
                .as_ref()
                .is_some_and(|dialog| dialog.node_id == node_id),
            Focus::RenameSession => self
                .rename_session
                .as_ref()
                .is_some_and(|dialog| dialog.node_id == node_id),
            Focus::ForgetSession => self
                .forget_session
                .as_ref()
                .is_some_and(|dialog| dialog.node_id == node_id),
            _ => false,
        };
        if self.spawn.as_ref().is_some_and(|dialog| dialog.node_id == node_id) {
            self.spawn = None;
        }
        if self.add_space.as_ref().is_some_and(|dialog| dialog.node_id == node_id) {
            self.add_space = None;
        }
        if self
            .create_worktree
            .as_ref()
            .is_some_and(|dialog| dialog.node_id == node_id)
        {
            self.create_worktree = None;
        }
        if self
            .remove_worktree
            .as_ref()
            .is_some_and(|dialog| dialog.node_id == node_id)
        {
            self.remove_worktree = None;
        }
        if self
            .rename_session
            .as_ref()
            .is_some_and(|dialog| dialog.node_id == node_id)
        {
            self.rename_session = None;
        }
        if self
            .forget_session
            .as_ref()
            .is_some_and(|dialog| dialog.node_id == node_id)
        {
            self.forget_session = None;
        }

        self.selected_tab = selected_tab
            .as_ref()
            .filter(|address| address.node_id != node_id)
            .and_then(|address| self.tabs.iter().position(|tab| &tab.address == address))
            .unwrap_or_else(|| self.selected_tab.min(self.tabs.len().saturating_sub(1)));
        self.grid.focused = focused_pane
            .as_ref()
            .filter(|address| address.node_id != node_id)
            .and_then(|address| self.grid.panes.iter().position(|pane| &pane.address == address))
            .unwrap_or_else(|| self.grid.focused.min(self.grid.panes.len().saturating_sub(1)));
        self.selected_space = selected_space
            .as_ref()
            .filter(|(selected_node_id, _)| selected_node_id != node_id)
            .and_then(|(selected_node_id, workspace_id)| {
                self.space_rows().iter().position(|(node_index, workspace_index)| {
                    self.nodes[*node_index].node_id == *selected_node_id
                        && self.nodes[*node_index].workspaces[*workspace_index].workspace_id
                            == *workspace_id
                })
            })
            .unwrap_or_else(|| self.selected_space.min(self.space_rows().len().saturating_sub(1)));
        self.selected_agent = selected_agent
            .as_ref()
            .filter(|key| match key {
                AgentRowKey::Managed { node_id: selected_node_id, .. } => selected_node_id != node_id,
                AgentRowKey::Legacy(address) => address.node_id != node_id,
            })
            .and_then(|key| self.agent_rows().iter().position(|candidate| candidate == key))
            .unwrap_or_else(|| self.selected_agent.min(self.agent_rows().len().saturating_sub(1)));

        if selected_space_removed {
            self.files_cursor = 0;
            self.git_cursor = 0;
            self.files_scroll = 0;
            self.git_scroll = 0;
            self.workspaces_scroll = 0;
        } else {
            self.files_cursor = self
                .files_cursor
                .min(self.visible_workspace_entry_indices().len().saturating_sub(1));
            self.git_cursor = self.git_cursor.min(self.git_item_count().saturating_sub(1));
        }
        if selected_agent_removed {
            self.agents_scroll = 0;
        }

        self.reconcile_session_drag();
        let active_target_removed = match self.surface_mode {
            SurfaceMode::Tab => selected_tab_removed,
            SurfaceMode::Grid => focused_pane_removed,
        };
        if self.focus == Focus::Viewport && active_target_removed {
            if self.surface_mode == SurfaceMode::Grid && !self.grid.panes.is_empty() {
                self.notice = Some(format!(
                    "{node_id} removed from C2 topology; nearest grid pane focused"
                ));
            } else {
                self.surface_mode = SurfaceMode::Tab;
                self.focus = if self.tabs.is_empty() && self.menu_placement == MenuPlacement::Sidebar {
                    self.sidebar_focus()
                } else {
                    Focus::Tabs
                };
                self.notice = Some(format!(
                    "{node_id} removed from C2 topology; active PTY input target cleared"
                ));
            }
        } else if removed_modal_focus {
            self.surface_mode = SurfaceMode::Tab;
            self.focus = if self.tabs.is_empty() && self.menu_placement == MenuPlacement::Sidebar {
                self.sidebar_focus()
            } else {
                Focus::Tabs
            };
            self.notice = Some(format!("{node_id} removed from C2 topology; dialog closed"));
        }
    }

    pub fn upsert_managed_session(&mut self, record: ManagedSessionView) {
        let key = AgentRowKey::Managed {
            node_id: record.node_id.clone(),
            record_id: record.record_id.clone(),
        };
        let selected = self.agent_rows().get(self.selected_agent).cloned();
        let Some(node) = self.nodes.iter_mut().find(|node| node.node_id == record.node_id) else {
            return;
        };
        if let Some(existing) = node
            .session_records
            .iter_mut()
            .find(|existing| existing.record_id == record.record_id)
        {
            *existing = record;
        } else {
            node.session_records.push(record);
        }
        if let Some(selected) = selected {
            if let Some(index) = self.agent_rows().iter().position(|candidate| *candidate == selected) {
                self.selected_agent = index;
            }
        } else if let Some(index) = self.agent_rows().iter().position(|candidate| *candidate == key) {
            self.selected_agent = index;
        }
    }

    pub fn remove_managed_session(&mut self, node_id: &str, record_id: &str) {
        let selected = self.agent_rows().get(self.selected_agent).cloned();
        if let Some(node) = self.nodes.iter_mut().find(|node| node.node_id == node_id) {
            node.session_records.retain(|record| record.record_id != record_id);
        }
        let removed = AgentRowKey::Managed {
            node_id: node_id.to_owned(),
            record_id: record_id.to_owned(),
        };
        if selected.as_ref() != Some(&removed) {
            if let Some(index) = selected
                .as_ref()
                .and_then(|selected| self.agent_rows().iter().position(|candidate| candidate == selected))
            {
                self.selected_agent = index;
                return;
            }
        }
        self.selected_agent = self.selected_agent.min(self.agent_rows().len().saturating_sub(1));
    }

    pub fn request_open(&mut self, address: SessionAddress) {
        if self.find_session(&address).is_some() {
            self.open_address(address);
        } else if !self.pending_open.contains(&address) {
            self.pending_open.push(address);
        }
    }

    pub fn request_space_selection(&mut self, node_id: String, workspace_id: String) {
        self.pending_space = Some((node_id, workspace_id));
        self.reconcile_pending_space();
    }

    pub fn open_selected_agent(&mut self) {
        let key = self.agent_rows().get(self.selected_agent).cloned();
        if let Some(address) = key.as_ref().and_then(|key| self.agent_row_active_address(key)) {
            self.open_address(address);
        } else if let Some(record) = key.as_ref().and_then(|key| self.find_managed_session(key)) {
            self.notice = Some(format!(
                "{} is {}; press r to resume",
                record.display_name,
                managed_state_label(record.state)
            ));
        }
    }

    pub fn open_address(&mut self, address: SessionAddress) {
        if self.find_session(&address).is_none() {
            return;
        }
        if !self.address_is_connected(&address) {
            self.notice = Some(format!("{} is disconnected; PTY cannot be opened", address.node_id));
            return;
        }
        if let Some(space_index) = self.space_rows().iter().position(|(node_index, workspace_index)| {
            let node = &self.nodes[*node_index];
            node.node_id == address.node_id
                && node.workspaces[*workspace_index].workspace_id == address.workspace_id
        }) {
            self.select_workspace_without_inspection(space_index);
        }
        if let Some(agent_index) = self
            .agent_rows()
            .iter()
            .position(|candidate| self.agent_row_active_address(candidate).as_ref() == Some(&address))
        {
            self.selected_agent = agent_index;
        }
        if let Some(index) = self
            .grid
            .panes
            .iter()
            .position(|pane| pane.address == address)
        {
            self.grid.focused = index;
            self.surface_mode = SurfaceMode::Grid;
            self.focus = Focus::Viewport;
            return;
        }
        if let Some(index) = self.tabs.iter().position(|tab| tab.address == address) {
            self.selected_tab = index;
        } else {
            self.tabs.push(SessionTab { address });
            self.selected_tab = self.tabs.len() - 1;
        }
        self.surface_mode = SurfaceMode::Tab;
        self.focus = Focus::Viewport;
    }

    pub fn focus_grid_pane(&mut self, index: usize) {
        if index >= self.grid.panes.len() {
            return;
        }
        self.grid.focused = index;
        self.surface_mode = SurfaceMode::Grid;
        self.focus = Focus::Viewport;
    }

    pub fn set_grid_preset(&mut self, preset: GridPreset) {
        self.grid.preset = preset;
        if !self.grid.panes.is_empty() {
            self.surface_mode = SurfaceMode::Grid;
            self.focus = Focus::Viewport;
        }
    }

    pub fn move_address_to_grid(&mut self, address: SessionAddress, slot: Option<usize>) -> bool {
        if self.find_session(&address).is_none() || !self.address_is_connected(&address) {
            return false;
        }
        if let Some(existing) = self
            .grid
            .panes
            .iter()
            .position(|pane| pane.address == address)
        {
            let pane = self.grid.panes.remove(existing);
            let target = slot
                .unwrap_or(existing)
                .min(self.grid.panes.len());
            self.grid.panes.insert(target, pane);
            self.grid.focused = target;
            self.surface_mode = SurfaceMode::Grid;
            self.focus = Focus::Viewport;
            return true;
        }
        if self.grid.panes.len() >= MAX_GRID_PANES {
            self.notice = Some("grid is full; detach a pane before adding another PTY".to_owned());
            return false;
        }
        self.remove_tab_address(&address);
        let target = slot.unwrap_or(self.grid.panes.len()).min(self.grid.panes.len());
        self.grid.panes.insert(target, GridPane { address });
        self.grid.focused = target;
        self.surface_mode = SurfaceMode::Grid;
        self.focus = Focus::Viewport;
        true
    }

    pub fn move_address_to_tabs(&mut self, address: SessionAddress) -> bool {
        if self.find_session(&address).is_none() || !self.address_is_connected(&address) {
            return false;
        }
        if let Some(index) = self
            .grid
            .panes
            .iter()
            .position(|pane| pane.address == address)
        {
            self.grid.panes.remove(index);
            self.grid.focused = self.grid.focused.min(self.grid.panes.len().saturating_sub(1));
        }
        if let Some(index) = self.tabs.iter().position(|tab| tab.address == address) {
            self.selected_tab = index;
        } else {
            self.tabs.push(SessionTab { address });
            self.selected_tab = self.tabs.len() - 1;
        }
        self.surface_mode = SurfaceMode::Tab;
        self.focus = Focus::Viewport;
        true
    }

    fn remove_tab_address(&mut self, address: &SessionAddress) {
        let selected = self.tabs.get(self.selected_tab).map(|tab| tab.address.clone());
        self.tabs.retain(|tab| &tab.address != address);
        if let Some(index) = selected
            .as_ref()
            .and_then(|selected| self.tabs.iter().position(|tab| &tab.address == selected))
        {
            self.selected_tab = index;
        } else {
            self.selected_tab = self.selected_tab.min(self.tabs.len().saturating_sub(1));
        }
    }

    fn reconcile_pending_open(&mut self) {
        let ready = self
            .pending_open
            .iter()
            .filter(|address| self.find_session(address).is_some())
            .cloned()
            .collect::<Vec<_>>();
        self.pending_open.retain(|address| !ready.contains(address));
        for address in ready {
            self.open_address(address);
        }
    }

    fn reconcile_pending_space(&mut self) {
        let Some((node_id, workspace_id)) = self.pending_space.clone() else {
            return;
        };
        let target = self.space_rows().iter().position(|(node_index, workspace_index)| {
            self.nodes[*node_index].node_id == node_id
                && self.nodes[*node_index].workspaces[*workspace_index].workspace_id == workspace_id
        });
        if let Some(index) = target {
            self.select_workspace_without_inspection(index);
            self.pending_space = None;
        }
    }

    fn reconcile_session_drag(&mut self) {
        let Some(drag) = self.drag_state.take() else {
            return;
        };
        self.drag_state = match drag {
            DragState::SessionChip {
                source,
                address,
                start_column,
                start_row,
                current_column,
                current_row,
                moved,
            } => self.rebound_address(&address).map(|address| DragState::SessionChip {
                source,
                address,
                start_column,
                start_row,
                current_column,
                current_row,
                moved,
            }),
            DragState::AgentSelection {
                key,
                start_column,
                start_row,
                moved,
            } => self
                .agent_rows()
                .contains(&key)
                .then_some(DragState::AgentSelection {
                    key,
                    start_column,
                    start_row,
                    moved,
                }),
            other => Some(other),
        };
    }

    fn reconcile_terminal_scroll_offsets(
        &mut self,
        previous_scrollback: &BTreeMap<SessionAddress, Vec<Vec<u8>>>,
    ) {
        let previous = std::mem::take(&mut self.terminal_scroll_offsets);
        for (address, offset) in previous {
            let Some(rebound) = self.rebound_address(&address) else {
                continue;
            };
            let Some(session) = self.find_session(&rebound) else {
                continue;
            };
            let maximum = session.terminal_scrollback.len();
            let advance = previous_scrollback
                .get(&address)
                .map(|previous| scrollback_advance(previous, &session.terminal_scrollback))
                .unwrap_or(0);
            let offset = offset.saturating_add(advance).min(maximum);
            if offset > 0 {
                self.terminal_scroll_offsets.insert(rebound, offset);
            }
        }
    }

    pub fn close_selected_tab(&mut self) {
        if self.surface_mode == SurfaceMode::Grid {
            if self.grid.panes.is_empty() {
                return;
            }
            self.grid.panes.remove(self.grid.focused);
            self.grid.focused = self.grid.focused.min(self.grid.panes.len().saturating_sub(1));
            if self.grid.panes.is_empty() {
                self.surface_mode = SurfaceMode::Tab;
            }
            self.focus = if self.surface_mode == SurfaceMode::Tab
                && self.tabs.is_empty()
                && self.menu_placement == MenuPlacement::Sidebar
            {
                self.sidebar_focus()
            } else {
                Focus::Tabs
            };
            self.notice = Some("grid pane detached; headless session continues".to_owned());
            return;
        }
        if self.tabs.is_empty() {
            return;
        }
        self.tabs.remove(self.selected_tab);
        self.selected_tab = self.selected_tab.min(self.tabs.len().saturating_sub(1));
        self.focus = if self.tabs.is_empty() && self.menu_placement == MenuPlacement::Sidebar {
            self.sidebar_focus()
        } else {
            Focus::Tabs
        };
        self.notice = Some("tab detached; headless session continues".to_owned());
    }

    pub fn set_terminal_size(&mut self, cols: u16, rows: u16) -> AppAction {
        self.terminal_cols = cols;
        self.terminal_rows = rows;
        let Some(address) = self.focused_session().map(|session| session.address.clone()) else {
            return AppAction::None;
        };
        let rect = self.focused_terminal_rect();
        AppAction::Resize {
            address,
            rows: rect.height.max(1),
            cols: rect.width.max(1),
        }
    }

    pub fn content_rows(&self) -> u16 {
        self.layout.viewport.height.max(1)
    }

    pub fn content_cols(&self) -> u16 {
        self.layout.viewport.width.max(1)
    }

    pub fn reduce(&mut self, key: UiKey) -> AppAction {
        if key == UiKey::UnsupportedModifier {
            self.notice = Some("unsupported key modifier; no input sent".to_owned());
            return AppAction::None;
        }
        if self.focus == Focus::Spawn {
            return self.reduce_spawn(key);
        }
        if self.focus == Focus::AddSpace {
            return self.reduce_add_space(key);
        }
        if self.focus == Focus::CreateWorktree {
            return self.reduce_create_worktree(key);
        }
        if self.focus == Focus::RemoveWorktree {
            return self.reduce_remove_worktree(key);
        }
        if self.focus == Focus::RenameSession {
            return self.reduce_rename_session(key);
        }
        if self.focus == Focus::ForgetSession {
            return self.reduce_forget_session(key);
        }
        if self.focus == Focus::Settings {
            return self.reduce_settings(key);
        }
        if self.focus == Focus::Viewport {
            return self.reduce_viewport(key);
        }
        match key {
            UiKey::Ctrl('q') => {
                self.should_quit = true;
                AppAction::Quit
            }
            UiKey::Ctrl('n') => self.begin_spawn(),
            UiKey::Ctrl('t') => {
                self.cycle_color_mode();
                AppAction::None
            }
            UiKey::Ctrl('w') => {
                self.close_selected_tab();
                AppAction::None
            }
            UiKey::Tab => {
                self.focus = self.next_focus();
                AppAction::None
            }
            UiKey::BackTab => {
                self.focus = self.previous_focus();
                AppAction::None
            }
            _ => match self.focus {
                Focus::Spaces => self.reduce_spaces(key),
                Focus::Agents => self.reduce_agents(key),
                Focus::Tabs => self.reduce_tabs(key),
                Focus::Viewport
                | Focus::Spawn
                | Focus::AddSpace
                | Focus::CreateWorktree
                | Focus::RemoveWorktree
                | Focus::RenameSession
                | Focus::ForgetSession
                | Focus::Settings => {
                    AppAction::None
                }
            },
        }
    }

    pub fn click(&mut self, column: u16, row: u16) -> AppAction {
        if matches!(
            self.focus,
            Focus::Spawn
                | Focus::AddSpace
                | Focus::CreateWorktree
                | Focus::RemoveWorktree
                | Focus::RenameSession
                | Focus::ForgetSession
        ) {
            return AppAction::None;
        }
        let target = self
            .layout
            .hits
            .iter()
            .rev()
            .find(|hit| hit.rect.contains(column, row))
            .map(|hit| hit.target.clone());
        if self.focus == Focus::Settings {
            match target {
                Some(HitTarget::ControlResize) => {
                    self.begin_control_resize(column, row);
                }
                Some(HitTarget::ControlDrag) => {
                    self.begin_control_drag(column, row);
                }
                Some(HitTarget::ControlSection(section)) => {
                    return self.select_control_section(section);
                }
                Some(HitTarget::SettingsStyle) => self.cycle_color_mode(),
                Some(HitTarget::SettingsPlacement) => self.toggle_menu_placement(),
                Some(HitTarget::SettingsPresentation) => self.toggle_sidebar_presentation(),
                Some(HitTarget::SettingsSidebarCollapsed) => self.toggle_sidebar_collapsed(),
                Some(HitTarget::SidebarItem(index)) => {
                    let mode = match self.control_section {
                        ControlSection::Files => SidebarMode::Files,
                        ControlSection::Git => SidebarMode::Git,
                        ControlSection::Agents
                        | ControlSection::Workspaces
                        | ControlSection::Settings => return AppAction::None,
                    };
                    return self.select_inspector_item(mode, index);
                }
                Some(HitTarget::Agent(index)) => {
                    self.select_or_begin_agent_drag(index, column, row);
                }
                Some(HitTarget::Space(index)) => {
                    self.select_workspace_without_inspection(index);
                    return self.inspect_selected_workspace();
                }
                Some(HitTarget::SpawnSpace(index)) => {
                    self.select_workspace_without_inspection(index);
                    return self.begin_spawn();
                }
                Some(HitTarget::AddSpace) => return self.begin_add_space(),
                Some(HitTarget::RemoveSpace) => return self.remove_selected_space(),
                Some(HitTarget::RefreshWorkspace) => return self.inspect_selected_workspace(),
                Some(HitTarget::CreateWorktree) => return self.begin_create_worktree(),
                Some(HitTarget::Worktree(index)) => return self.activate_worktree(index),
                Some(HitTarget::RegisterWorktree(index)) => {
                    return self.begin_register_worktree(index);
                }
                Some(HitTarget::RemoveWorktree(index)) => {
                    return self.begin_remove_worktree(index);
                }
                Some(HitTarget::AddAgent) => return self.begin_spawn(),
                Some(HitTarget::Settings) => self.close_settings(),
                _ => {}
            }
            return AppAction::None;
        }
        match target {
            Some(HitTarget::SidebarMode(mode)) => {
                return self.select_sidebar_mode(mode);
            }
            Some(HitTarget::ActivitySection(section)) => {
                return self.activate_activity_section(section);
            }
            Some(HitTarget::SidebarCollapse) => self.toggle_sidebar_collapsed(),
            Some(HitTarget::SidebarWidthDrag) => self.drag_state = Some(DragState::SidebarWidth),
            Some(HitTarget::SidebarSplitDrag) => self.drag_state = Some(DragState::SidebarSplit),
            Some(HitTarget::RosterMode(mode)) => {
                self.roster_mode = mode;
                self.control_section = match mode {
                    RosterMode::Agents => ControlSection::Agents,
                    RosterMode::Workspaces => ControlSection::Workspaces,
                };
                self.focus = Focus::Agents;
            }
            Some(HitTarget::Space(index)) => {
                self.control_section = ControlSection::Workspaces;
                return self.select_workspace(index);
            }
            Some(HitTarget::SpawnSpace(index)) => {
                self.control_section = ControlSection::Workspaces;
                self.select_workspace_without_inspection(index);
                return self.begin_spawn();
            }
            Some(HitTarget::AddSpace) => return self.begin_add_space(),
            Some(HitTarget::RemoveSpace) => return self.remove_selected_space(),
            Some(HitTarget::RefreshWorkspace) => return self.inspect_selected_workspace(),
            Some(HitTarget::CreateWorktree) => return self.begin_create_worktree(),
            Some(HitTarget::Worktree(index)) => return self.activate_worktree(index),
            Some(HitTarget::RegisterWorktree(index)) => {
                return self.begin_register_worktree(index);
            }
            Some(HitTarget::RemoveWorktree(index)) => return self.begin_remove_worktree(index),
            Some(HitTarget::SidebarItem(index)) => {
                self.control_section = match self.sidebar_mode {
                    SidebarMode::Files => ControlSection::Files,
                    SidebarMode::Git => ControlSection::Git,
                };
                self.focus = Focus::Spaces;
                return self.select_inspector_item(self.sidebar_mode, index);
            }
            Some(HitTarget::Agent(index)) => {
                self.control_section = ControlSection::Agents;
                self.select_or_begin_agent_drag(index, column, row);
            }
            Some(HitTarget::AddAgent) => return self.begin_spawn(),
            Some(HitTarget::Tab(index)) => {
                if let Some(address) = self.tabs.get(index).map(|tab| tab.address.clone()) {
                    self.begin_session_drag(DragSource::Tab(index), address, column, row);
                }
            }
            Some(HitTarget::AddTab) => return self.begin_spawn(),
            Some(HitTarget::GridToggle) => {
                if self.grid.panes.is_empty() {
                    self.notice = Some("drag an agent or tab onto grid to create its first pane".to_owned());
                } else {
                    self.surface_mode = SurfaceMode::Grid;
                    self.focus = Focus::Viewport;
                }
            }
            Some(HitTarget::GridPreset(preset)) => self.set_grid_preset(preset),
            Some(HitTarget::GridPaneHeader(index)) => {
                if let Some(address) = self.grid.panes.get(index).map(|pane| pane.address.clone()) {
                    self.begin_session_drag(DragSource::Pane(index), address, column, row);
                }
            }
            Some(HitTarget::GridPaneBody(index)) => self.focus_grid_pane(index),
            Some(HitTarget::GridDropSlot(index)) => {
                if index < self.grid.panes.len() {
                    self.focus_grid_pane(index);
                }
            }
            Some(HitTarget::GridDivider(axis, index)) => {
                self.drag_state = Some(DragState::GridDivider { axis, index });
            }
            Some(HitTarget::TabDrop) => {}
            Some(HitTarget::Settings) => self.begin_settings(),
            Some(
                HitTarget::ControlSection(_)
                | HitTarget::SettingsStyle
                | HitTarget::SettingsPlacement
                | HitTarget::SettingsPresentation
                | HitTarget::SettingsSidebarCollapsed
                | HitTarget::ControlDrag
                | HitTarget::ControlResize,
            ) => {}
            Some(HitTarget::Viewport) => self.focus = Focus::Viewport,
            None => {}
        }
        AppAction::None
    }

    pub fn scroll(&mut self, column: u16, row: u16, up: bool) -> AppAction {
        if matches!(
            self.focus,
            Focus::Spawn
                | Focus::AddSpace
                | Focus::CreateWorktree
                | Focus::RemoveWorktree
                | Focus::RenameSession
                | Focus::ForgetSession
        ) {
            return AppAction::None;
        }
        if self.focus == Focus::Settings {
            return if self.layout.control_modal.contains(column, row)
                || self.layout.control_content.contains(column, row)
            {
                self.wheel_control(up)
            } else {
                AppAction::None
            };
        }
        if let Some((address, viewport)) = self.terminal_target_at(column, row) {
            return self.scroll_terminal(address, viewport, column, row, up);
        }
        if self.layout.spaces.contains(column, row) {
            let reserved_rows = match self.sidebar_mode {
                SidebarMode::Files => 2,
                SidebarMode::Git => 3,
            };
            self.scroll_inspector(
                self.sidebar_mode,
                up,
                self.layout.spaces.height.saturating_sub(reserved_rows),
            );
            return AppAction::None;
        }
        if self.layout.agents.contains(column, row) {
            self.scroll_roster(self.roster_mode, up, self.layout.agents.height.saturating_sub(3));
            return AppAction::None;
        }
        AppAction::None
    }

    pub fn drag(&mut self, column: u16, row: u16) -> AppAction {
        let Some(drag) = self.drag_state.clone() else {
            return AppAction::None;
        };
        match drag {
            DragState::ControlModal {
                grab_column,
                grab_row,
                origin_x,
                origin_y,
            } => {
                let x = i32::from(origin_x) + i32::from(column) - i32::from(grab_column);
                let y = i32::from(origin_y) + i32::from(row) - i32::from(grab_row);
                let max_x = self.terminal_cols.saturating_sub(self.layout.control_modal.width);
                let max_y = self.terminal_rows.saturating_sub(self.layout.control_modal.height);
                self.control_modal_position = Some((
                    x.clamp(0, i32::from(max_x)) as u16,
                    y.clamp(0, i32::from(max_y)) as u16,
                ));
            }
            DragState::ControlModalResize {
                start_column,
                start_row,
                origin_width,
                origin_height,
            } => {
                let width = i32::from(origin_width) + i32::from(column) - i32::from(start_column);
                let height = i32::from(origin_height) + i32::from(row) - i32::from(start_row);
                let max_width = self
                    .control_modal_position
                    .map(|(x, _)| self.terminal_cols.saturating_sub(x))
                    .unwrap_or(self.terminal_cols)
                    .max(1);
                let max_height = self
                    .control_modal_position
                    .map(|(_, y)| self.terminal_rows.saturating_sub(y))
                    .unwrap_or(self.terminal_rows)
                    .max(1);
                self.control_modal_size = Some((
                    width.clamp(
                        i32::from(MIN_CONTROL_MODAL_WIDTH.min(max_width)),
                        i32::from(max_width),
                    ) as u16,
                    height.clamp(
                        i32::from(MIN_CONTROL_MODAL_HEIGHT.min(max_height)),
                        i32::from(max_height),
                    ) as u16,
                ));
            }
            DragState::SidebarWidth => {
                let maximum = self.terminal_cols.saturating_sub(24).min(60).max(18);
                let content_column = if self.sidebar_presentation == SidebarPresentation::Activity {
                    column.saturating_sub(self.layout.activity_rail.width)
                } else {
                    column
                };
                self.sidebar_width = content_column.saturating_add(1).clamp(18, maximum);
            }
            DragState::SidebarSplit => {
                let height = self.terminal_rows.max(1);
                self.sidebar_split_percent =
                    (u32::from(row) * 100 / u32::from(height)).clamp(25, 75) as u16;
            }
            DragState::SessionChip {
                start_column,
                start_row,
                ..
            } => {
                if let Some(DragState::SessionChip {
                    current_column,
                    current_row,
                    moved,
                    ..
                }) = self.drag_state.as_mut()
                {
                    *current_column = column;
                    *current_row = row;
                    if column != start_column || row != start_row {
                        *moved = true;
                    }
                }
            }
            DragState::AgentSelection {
                start_column,
                start_row,
                ..
            } => {
                if let Some(DragState::AgentSelection { moved, .. }) = self.drag_state.as_mut() {
                    if column != start_column || row != start_row {
                        *moved = true;
                    }
                }
            }
            DragState::GridDivider { axis, index } => {
                self.update_grid_divider(axis, index, column, row);
            }
        }
        AppAction::None
    }

    pub fn drop_at(&mut self, column: u16, row: u16) -> AppAction {
        let Some(drag) = self.drag_state.take() else {
            return AppAction::None;
        };
        let (address, moved) = match drag {
            DragState::SessionChip { address, moved, .. } => (address, moved),
            DragState::AgentSelection { key, moved, .. } => {
                if !moved {
                    let Some(index) = self
                        .agent_rows()
                        .iter()
                        .position(|candidate| candidate == &key)
                    else {
                        return AppAction::None;
                    };
                    self.selected_agent = index;
                    if self.focus != Focus::Settings {
                        self.focus = Focus::Agents;
                    }
                    self.open_selected_agent();
                }
                return AppAction::None;
            }
            _ => return AppAction::None,
        };
        if !moved {
            self.open_address(address);
            return AppAction::None;
        }
        let target = self
            .layout
            .hits
            .iter()
            .rev()
            .find(|hit| hit.rect.contains(column, row))
            .map(|hit| hit.target.clone());
        match target {
            Some(HitTarget::GridDropSlot(index))
            | Some(HitTarget::GridPaneHeader(index))
            | Some(HitTarget::GridPaneBody(index)) => {
                self.move_address_to_grid(address, Some(index));
            }
            Some(HitTarget::GridToggle) | Some(HitTarget::GridPreset(_)) => {
                self.move_address_to_grid(address, None);
            }
            Some(HitTarget::TabDrop)
            | Some(HitTarget::Tab(_))
            | Some(HitTarget::AddTab) => {
                self.move_address_to_tabs(address);
            }
            _ if self.layout.grid_drop.contains(column, row) => {
                self.move_address_to_grid(address, None);
            }
            _ if self.layout.tab_drop.contains(column, row) => {
                self.move_address_to_tabs(address);
            }
            _ => {}
        }
        AppAction::None
    }

    pub fn end_drag(&mut self) {
        self.drag_state = None;
    }

    fn begin_control_drag(&mut self, column: u16, row: u16) {
        let modal = self.layout.control_modal;
        if modal.width == 0 || modal.height == 0 {
            return;
        }
        self.control_modal_position = Some((modal.x, modal.y));
        self.drag_state = Some(DragState::ControlModal {
            grab_column: column,
            grab_row: row,
            origin_x: modal.x,
            origin_y: modal.y,
        });
    }

    fn begin_control_resize(&mut self, column: u16, row: u16) {
        let modal = self.layout.control_modal;
        if modal.width == 0 || modal.height == 0 {
            return;
        }
        self.control_modal_position = Some((modal.x, modal.y));
        self.control_modal_size = Some((modal.width, modal.height));
        self.drag_state = Some(DragState::ControlModalResize {
            start_column: column,
            start_row: row,
            origin_width: modal.width,
            origin_height: modal.height,
        });
    }

    fn begin_session_drag(
        &mut self,
        source: DragSource,
        address: SessionAddress,
        column: u16,
        row: u16,
    ) {
        self.drag_state = Some(DragState::SessionChip {
            source,
            address,
            start_column: column,
            start_row: row,
            current_column: column,
            current_row: row,
            moved: false,
        });
    }

    fn select_or_begin_agent_drag(&mut self, index: usize, column: u16, row: u16) {
        let Some(key) = self.agent_rows().get(index).cloned() else {
            return;
        };
        if let Some(address) = self.agent_row_active_address(&key) {
            self.begin_session_drag(DragSource::Agent(index), address, column, row);
        } else {
            self.drag_state = Some(DragState::AgentSelection {
                key,
                start_column: column,
                start_row: row,
                moved: false,
            });
        }
    }

    fn update_grid_divider(
        &mut self,
        axis: GridAxisKind,
        index: usize,
        column: u16,
        row: u16,
    ) {
        if index >= 3 {
            return;
        }
        let area = self.layout.viewport;
        let (offset, extent) = match axis {
            GridAxisKind::Columns => (column.saturating_sub(area.x), area.width),
            GridAxisKind::Rows => (row.saturating_sub(area.y), area.height),
        };
        if extent == 0 {
            return;
        }
        let candidate = (u32::from(offset.min(extent)) * 10_000 / u32::from(extent)) as u16;
        let cuts = match axis {
            GridAxisKind::Columns => &mut self.grid.column_cuts,
            GridAxisKind::Rows => &mut self.grid.row_cuts,
        };
        let (minimum, maximum) = if self.grid.preset == GridPreset::Quad {
            if index != 1 {
                return;
            }
            (1_000, 9_000)
        } else {
            let minimum = if index == 0 {
                1_000
            } else {
                cuts[index - 1].saturating_add(1_000)
            };
            let maximum = if index + 1 == cuts.len() {
                9_000
            } else {
                cuts[index + 1].saturating_sub(1_000)
            };
            (minimum, maximum)
        };
        cuts[index] = candidate.clamp(minimum, maximum);
    }

    fn select_workspace(&mut self, index: usize) -> AppAction {
        if index >= self.space_rows().len() {
            return AppAction::None;
        }
        self.select_workspace_without_inspection(index);
        self.focus = Focus::Agents;
        self.inspect_selected_workspace()
    }

    fn select_workspace_without_inspection(&mut self, index: usize) {
        if index == self.selected_space || index >= self.space_rows().len() {
            return;
        }
        self.selected_space = index;
        self.files_cursor = 0;
        self.git_cursor = 0;
        self.files_scroll = 0;
        self.git_scroll = 0;
    }

    fn select_inspector_item(&mut self, mode: SidebarMode, index: usize) -> AppAction {
        match mode {
            SidebarMode::Files => {
                let visible = self.visible_workspace_entry_indices();
                let Some(cursor) = visible.iter().position(|entry_index| *entry_index == index) else {
                    return AppAction::None;
                };
                self.files_cursor = cursor;
                let entry = self
                    .selected_workspace_inspection()
                    .and_then(|inspection| inspection.entries.get(index))
                    .map(|entry| (entry.kind, entry.relative_path.clone()));
                let Some((WorkspaceEntryKind::Directory, relative_path)) = entry else {
                    return AppAction::None;
                };
                let Some((node_id, workspace_id)) = self.selected_workspace_route() else {
                    return AppAction::None;
                };
                let key = (node_id, workspace_id, relative_path);
                if !self.collapsed_directories.remove(&key) {
                    self.collapsed_directories.insert(key);
                }
                self.files_cursor = self
                    .files_cursor
                    .min(self.visible_workspace_entry_indices().len().saturating_sub(1));
                self.files_scroll = self
                    .files_scroll
                    .min(self.visible_workspace_entry_indices().len().saturating_sub(1));
            }
            SidebarMode::Git => self.git_cursor = index.min(self.git_item_count().saturating_sub(1)),
        }
        AppAction::None
    }

    pub fn paste(&mut self, text: String) -> AppAction {
        if self.focus == Focus::AddSpace {
            return self.paste_add_space(text);
        }
        if self.focus == Focus::CreateWorktree {
            return self.paste_create_worktree(text);
        }
        if self.focus == Focus::RenameSession {
            return self.paste_rename_session(text);
        }
        if self.focus != Focus::Viewport {
            return AppAction::None;
        }
        if !self.focused_session().is_some_and(|session| session.running) {
            self.notice = Some("stopped PTY is read-only; use r from agents to restart".to_owned());
            return AppAction::None;
        }
        if text.len() > TERMINAL_INPUT_MAX_BYTES {
            self.notice = Some("PTY paste exceeds terminal input limit; nothing sent".to_owned());
            return AppAction::None;
        }
        self.for_active(|address| AppAction::Paste { address, text })
    }

    fn begin_spawn(&mut self) -> AppAction {
        let Some((node, workspace)) = self.selected_workspace() else {
            self.notice = Some("select a workspace before spawning".to_owned());
            return AppAction::None;
        };
        if !matches!(node.connection, ConnectionState::Connected) {
            self.notice = Some(format!("{} is disconnected; spawn unavailable", node.node_id));
            return AppAction::None;
        }
        let Some(provider) = workspace
            .providers
            .iter()
            .find(|inventory| inventory.enabled)
            .map(|inventory| inventory.provider)
        else {
            self.notice = Some("selected workspace has no enabled provider".to_owned());
            return AppAction::None;
        };
        self.spawn = Some(SpawnDialog {
            node_id: node.node_id.clone(),
            workspace_id: workspace.workspace_id.clone(),
            provider,
        });
        self.focus = Focus::Spawn;
        AppAction::None
    }

    fn begin_add_space(&mut self) -> AppAction {
        let node_id = self
            .selected_workspace()
            .filter(|(node, _)| matches!(node.connection, ConnectionState::Connected))
            .map(|(node, _)| node.node_id.clone())
            .or_else(|| self.nodes.iter().find(|node| matches!(node.connection, ConnectionState::Connected)).map(|node| node.node_id.clone()));
        let Some(node_id) = node_id else {
            self.notice = Some("no connected node available for a new space".to_owned());
            return AppAction::None;
        };
        self.add_space = Some(AddSpaceDialog {
            node_id,
            workspace_id: String::new(),
            root: String::new(),
            field: AddSpaceField::WorkspaceId,
        });
        self.focus = Focus::AddSpace;
        AppAction::None
    }

    fn begin_create_worktree(&mut self) -> AppAction {
        let Some((node, workspace)) = self.selected_workspace() else {
            self.notice = Some("select a workspace before creating a worktree".to_owned());
            return AppAction::None;
        };
        if !matches!(node.connection, ConnectionState::Connected) {
            self.notice = Some(format!("{} is disconnected; worktree creation unavailable", node.node_id));
            return AppAction::None;
        }
        let workspace_id = next_worktree_id(&workspace.workspace_id);
        self.create_worktree = Some(CreateWorktreeDialog {
            node_id: node.node_id.clone(),
            source_workspace_id: workspace.workspace_id.clone(),
            target_root: format!("{}-{workspace_id}", workspace.canonical_root.trim_end_matches(['\\', '/'])),
            branch: workspace_id.clone(),
            workspace_id,
            base: String::new(),
            field: CreateWorktreeField::WorkspaceId,
        });
        self.focus = Focus::CreateWorktree;
        AppAction::None
    }

    fn selected_git_worktree(&self) -> Option<(usize, &GitWorktreeSnapshot)> {
        let inspection = self.selected_workspace_inspection()?;
        let index = self.git_cursor;
        inspection.git.worktrees.get(index).map(|worktree| (index, worktree))
    }

    fn activate_worktree(&mut self, index: usize) -> AppAction {
        self.git_cursor = index.min(self.git_item_count().saturating_sub(1));
        let Some((node_id, workspace_id)) = self
            .selected_workspace()
            .and_then(|(node, _)| {
                self.selected_workspace_inspection()?
                    .git
                    .worktrees
                    .get(index)?
                    .workspace_id
                    .as_ref()
                    .map(|workspace_id| (node.node_id.clone(), workspace_id.to_string()))
            })
        else {
            return self.begin_register_worktree(index);
        };
        self.request_space_selection(node_id, workspace_id);
        self.inspect_selected_workspace()
    }

    fn begin_register_worktree(&mut self, index: usize) -> AppAction {
        let Some((node_id, worktree)) = self.selected_workspace().and_then(|(node, _)| {
            self.selected_workspace_inspection()?
                .git
                .worktrees
                .get(index)
                .cloned()
                .map(|worktree| (node.node_id.clone(), worktree))
        }) else {
            return AppAction::None;
        };
        if worktree.workspace_id.is_some() {
            return self.activate_worktree(index);
        }
        self.add_space = Some(AddSpaceDialog {
            node_id,
            workspace_id: suggested_workspace_id(&worktree),
            root: worktree.path,
            field: AddSpaceField::WorkspaceId,
        });
        self.focus = Focus::AddSpace;
        AppAction::None
    }

    fn begin_remove_worktree(&mut self, index: usize) -> AppAction {
        let Some((node_id, source_workspace_id, worktree)) = self
            .selected_workspace()
            .and_then(|(node, workspace)| {
                self.selected_workspace_inspection()?
                    .git
                    .worktrees
                    .get(index)
                    .cloned()
                    .map(|worktree| {
                        (node.node_id.clone(), workspace.workspace_id.clone(), worktree)
                    })
            })
        else {
            return AppAction::None;
        };
        if !worktree_can_be_removed(&worktree) {
            self.notice = Some("main, bare, locked, or prunable worktrees cannot be removed here".to_owned());
            return AppAction::None;
        }
        self.remove_worktree = Some(RemoveWorktreeDialog {
            node_id,
            source_workspace_id,
            target_root: worktree.path,
            branch: worktree.branch,
        });
        self.focus = Focus::RemoveWorktree;
        AppAction::None
    }

    fn begin_rename_selected_agent(&mut self) -> AppAction {
        let Some(record) = self.selected_managed_session() else {
            self.notice = Some("rename requires a node-owned session record".to_owned());
            return AppAction::None;
        };
        self.rename_session = Some(RenameSessionDialog {
            node_id: record.node_id.clone(),
            record_id: record.record_id.clone(),
            original_name: record.display_name.clone(),
            display_name: record.display_name.clone(),
        });
        self.focus = Focus::RenameSession;
        AppAction::None
    }

    fn begin_forget_selected_agent(&mut self) -> AppAction {
        let Some(record) = self.selected_managed_session() else {
            self.notice = Some("forget requires a node-owned session record".to_owned());
            return AppAction::None;
        };
        if record.active_session.is_some() || matches!(record.state, ManagedSessionState::Live) {
            self.notice = Some("stop the live PTY before forgetting its session record".to_owned());
            return AppAction::None;
        }
        self.forget_session = Some(ForgetSessionDialog {
            node_id: record.node_id.clone(),
            record_id: record.record_id.clone(),
            display_name: record.display_name.clone(),
        });
        self.focus = Focus::ForgetSession;
        AppAction::None
    }

    fn connected_node_ids(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|node| matches!(node.connection, ConnectionState::Connected))
            .map(|node| node.node_id.clone())
            .collect()
    }

    fn select_sidebar_mode(&mut self, mode: SidebarMode) -> AppAction {
        self.sidebar_mode = mode;
        self.control_section = match mode {
            SidebarMode::Files => ControlSection::Files,
            SidebarMode::Git => ControlSection::Git,
        };
        self.focus = Focus::Spaces;
        self.inspect_selected_workspace()
    }

    fn select_control_section(&mut self, section: ControlSection) -> AppAction {
        self.control_section = section;
        self.focus = Focus::Settings;
        match section {
            ControlSection::Files | ControlSection::Git => self.inspect_selected_workspace(),
            ControlSection::Agents
            | ControlSection::Workspaces
            | ControlSection::Settings => AppAction::None,
        }
    }

    fn activate_activity_section(&mut self, section: ControlSection) -> AppAction {
        if section == ControlSection::Settings {
            self.begin_settings();
            return AppAction::None;
        }
        if self.control_section == section && !self.sidebar_collapsed {
            self.sidebar_collapsed = true;
            return AppAction::None;
        }
        self.control_section = section;
        self.sidebar_collapsed = false;
        match section {
            ControlSection::Files => {
                self.sidebar_mode = SidebarMode::Files;
                self.focus = Focus::Spaces;
                self.inspect_selected_workspace()
            }
            ControlSection::Git => {
                self.sidebar_mode = SidebarMode::Git;
                self.focus = Focus::Spaces;
                self.inspect_selected_workspace()
            }
            ControlSection::Agents => {
                self.roster_mode = RosterMode::Agents;
                self.focus = Focus::Agents;
                AppAction::None
            }
            ControlSection::Workspaces => {
                self.roster_mode = RosterMode::Workspaces;
                self.focus = Focus::Agents;
                AppAction::None
            }
            ControlSection::Settings => AppAction::None,
        }
    }

    fn wheel_control(&mut self, up: bool) -> AppAction {
        match self.control_section {
            ControlSection::Files => self.scroll_inspector(
                SidebarMode::Files,
                up,
                self.layout.control_content.height.saturating_sub(1),
            ),
            ControlSection::Git => self.scroll_inspector(
                SidebarMode::Git,
                up,
                self.layout.control_content.height.saturating_sub(2),
            ),
            ControlSection::Agents => self.scroll_roster(
                RosterMode::Agents,
                up,
                self.layout.control_content.height.saturating_sub(1),
            ),
            ControlSection::Workspaces => self.scroll_roster(
                RosterMode::Workspaces,
                up,
                self.layout.control_content.height.saturating_sub(1),
            ),
            ControlSection::Settings => {}
        }
        AppAction::None
    }

    fn move_control_selection(&mut self, up: bool) -> AppAction {
        match self.control_section {
            ControlSection::Files => {
                self.files_cursor = moved_index(
                    self.files_cursor,
                    self.visible_workspace_entry_indices().len(),
                    up,
                );
            }
            ControlSection::Git => {
                self.git_cursor = moved_index(self.git_cursor, self.git_item_count(), up);
            }
            ControlSection::Agents => {
                self.selected_agent = moved_index(
                    self.selected_agent,
                    self.agent_rows().len(),
                    up,
                );
            }
            ControlSection::Workspaces => {
                let next = moved_index(self.selected_space, self.space_rows().len(), up);
                if next != self.selected_space {
                    self.select_workspace_without_inspection(next);
                    self.reveal_roster_selection(
                        RosterMode::Workspaces,
                        self.layout.control_content.height.saturating_sub(1),
                    );
                    return self.inspect_selected_workspace();
                }
            }
            ControlSection::Settings => {}
        }
        match self.control_section {
            ControlSection::Files => self.reveal_inspector_selection(
                SidebarMode::Files,
                self.layout.control_content.height.saturating_sub(1),
            ),
            ControlSection::Git => self.reveal_inspector_selection(
                SidebarMode::Git,
                self.layout.control_content.height.saturating_sub(2),
            ),
            ControlSection::Agents => self.reveal_roster_selection(
                RosterMode::Agents,
                self.layout.control_content.height.saturating_sub(1),
            ),
            ControlSection::Workspaces => self.reveal_roster_selection(
                RosterMode::Workspaces,
                self.layout.control_content.height.saturating_sub(1),
            ),
            ControlSection::Settings => {}
        }
        AppAction::None
    }

    fn terminal_target_at(&self, column: u16, row: u16) -> Option<(SessionAddress, Rect)> {
        match self.surface_mode {
            SurfaceMode::Tab if self.layout.viewport.contains(column, row) => {
                self.focused_address()
                    .cloned()
                    .map(|address| (address, self.layout.viewport))
            }
            SurfaceMode::Grid => self
                .layout
                .grid_panes
                .iter()
                .find(|pane| pane.viewport.contains(column, row))
                .and_then(|layout| {
                    self.grid
                        .panes
                        .get(layout.pane_index)
                        .map(|pane| (pane.address.clone(), layout.viewport))
                }),
            SurfaceMode::Tab => None,
        }
    }

    fn scroll_terminal(
        &mut self,
        address: SessionAddress,
        viewport: Rect,
        column: u16,
        row: u16,
        up: bool,
    ) -> AppAction {
        let Some(session) = self.find_session(&address) else {
            return AppAction::None;
        };
        let maximum = session.terminal_scrollback.len();
        let current = self.terminal_scroll_offset(&address).min(maximum);
        if current > 0 {
            let next = if up {
                current.saturating_add(WHEEL_SCROLL_LINES).min(maximum)
            } else {
                current.saturating_sub(WHEEL_SCROLL_LINES)
            };
            if next == 0 {
                self.terminal_scroll_offsets.remove(&address);
            } else {
                self.terminal_scroll_offsets.insert(address, next);
            }
            return AppAction::None;
        }
        if session.running && session.terminal_mouse_protocol_enabled {
            let x = column
                .saturating_sub(viewport.x)
                .saturating_add(1)
                .min(viewport.width.max(1));
            let y = row
                .saturating_sub(viewport.y)
                .saturating_add(1)
                .min(viewport.height.max(1));
            return AppAction::TerminalBytes {
                address,
                bytes: terminal_mouse_wheel_bytes(
                    session.terminal_mouse_protocol_encoding,
                    x,
                    y,
                    up,
                ),
            };
        }
        if session.terminal_alternate_screen || maximum == 0 {
            return AppAction::None;
        }
        let next = if up {
            WHEEL_SCROLL_LINES.min(maximum)
        } else {
            0
        };
        if next > 0 {
            self.terminal_scroll_offsets.insert(address, next);
        }
        AppAction::None
    }

    fn scroll_inspector(&mut self, mode: SidebarMode, up: bool, visible_rows: u16) {
        let count = match mode {
            SidebarMode::Files => self.visible_workspace_entry_indices().len(),
            SidebarMode::Git => self.git_item_count(),
        };
        let maximum = count.saturating_sub(visible_rows as usize);
        let offset = match mode {
            SidebarMode::Files => &mut self.files_scroll,
            SidebarMode::Git => &mut self.git_scroll,
        };
        *offset = wheel_scroll_start(*offset, maximum, up);
    }

    fn scroll_roster(&mut self, mode: RosterMode, up: bool, visible_rows: u16) {
        let capacity = visible_rows as usize / 2;
        let count = match mode {
            RosterMode::Agents => self.agent_rows().len(),
            RosterMode::Workspaces => self.space_rows().len(),
        };
        let maximum = count.saturating_sub(capacity);
        let offset = match mode {
            RosterMode::Agents => &mut self.agents_scroll,
            RosterMode::Workspaces => &mut self.workspaces_scroll,
        };
        *offset = wheel_scroll_start(*offset, maximum, up);
    }

    fn reveal_inspector_selection(&mut self, mode: SidebarMode, visible_rows: u16) {
        let (selected, count) = match mode {
            SidebarMode::Files => (
                self.files_cursor,
                self.visible_workspace_entry_indices().len(),
            ),
            SidebarMode::Git => (self.git_cursor, self.git_item_count()),
        };
        let offset = match mode {
            SidebarMode::Files => &mut self.files_scroll,
            SidebarMode::Git => &mut self.git_scroll,
        };
        *offset = visible_selection_start(*offset, selected, count, visible_rows as usize);
    }

    fn reveal_roster_selection(&mut self, mode: RosterMode, visible_rows: u16) {
        let capacity = visible_rows as usize / 2;
        let (selected, count) = match mode {
            RosterMode::Agents => (self.selected_agent, self.agent_rows().len()),
            RosterMode::Workspaces => (self.selected_space, self.space_rows().len()),
        };
        let offset = match mode {
            RosterMode::Agents => &mut self.agents_scroll,
            RosterMode::Workspaces => &mut self.workspaces_scroll,
        };
        *offset = visible_selection_start(*offset, selected, count, capacity);
    }

    pub fn terminal_scroll_offset(&self, address: &SessionAddress) -> usize {
        self.terminal_scroll_offsets.get(address).copied().unwrap_or(0)
    }

    pub fn inspect_selected_workspace(&mut self) -> AppAction {
        let Some((node, workspace)) = self.selected_workspace() else {
            self.notice = Some("select a workspace before inspection".to_owned());
            return AppAction::None;
        };
        if !matches!(node.connection, ConnectionState::Connected) {
            self.notice = Some(format!(
                "{} is disconnected; workspace inspection unavailable",
                node.node_id
            ));
            return AppAction::None;
        }
        let node_id = node.node_id.clone();
        let workspace_id = workspace.workspace_id.clone();
        self.inspection_pending = Some((node_id.clone(), workspace_id.clone()));
        AppAction::InspectWorkspace {
            node_id,
            workspace_id,
        }
    }

    fn spawn_routes(&self) -> Vec<(String, String, Vec<Provider>)> {
        self.nodes
            .iter()
            .filter(|node| matches!(node.connection, ConnectionState::Connected))
            .flat_map(|node| {
                node.workspaces.iter().filter_map(|workspace| {
                    let providers = workspace
                        .providers
                        .iter()
                        .filter(|inventory| inventory.enabled)
                        .map(|inventory| inventory.provider)
                        .collect::<Vec<_>>();
                    (!providers.is_empty()).then(|| {
                        (
                            node.node_id.clone(),
                            workspace.workspace_id.clone(),
                            providers,
                        )
                    })
                })
            })
            .collect()
    }

    fn cycle_spawn_workspace(&self, spawn: &mut SpawnDialog, forward: bool) {
        let routes = self.spawn_routes();
        if routes.is_empty() {
            return;
        }
        let current = routes
            .iter()
            .position(|(node_id, workspace_id, _)| {
                *node_id == spawn.node_id && *workspace_id == spawn.workspace_id
            })
            .unwrap_or(0);
        let index = if forward {
            (current + 1) % routes.len()
        } else if current == 0 {
            routes.len() - 1
        } else {
            current - 1
        };
        let (node_id, workspace_id, providers) = &routes[index];
        spawn.node_id.clone_from(node_id);
        spawn.workspace_id.clone_from(workspace_id);
        if !providers.contains(&spawn.provider) {
            spawn.provider = providers[0];
        }
    }

    fn remove_selected_space(&mut self) -> AppAction {
        let Some((node, workspace)) = self.selected_workspace() else {
            return AppAction::None;
        };
        if !matches!(node.connection, ConnectionState::Connected) {
            self.notice = Some(format!(
                "{} is disconnected; space removal unavailable",
                node.node_id
            ));
            return AppAction::None;
        }
        AppAction::UnregisterWorkspace {
            node_id: node.node_id.clone(),
            workspace_id: workspace.workspace_id.clone(),
        }
    }

    fn cycle_add_space_node(&self, current: &str, forward: bool) -> String {
        let nodes = self.connected_node_ids();
        if nodes.is_empty() {
            return current.to_owned();
        }
        let current = nodes.iter().position(|node_id| node_id == current).unwrap_or(0);
        let index = if forward {
            (current + 1) % nodes.len()
        } else if current == 0 {
            nodes.len() - 1
        } else {
            current - 1
        };
        nodes[index].clone()
    }

    fn reduce_spaces(&mut self, key: UiKey) -> AppAction {
        self.control_section = match self.sidebar_mode {
            SidebarMode::Files => ControlSection::Files,
            SidebarMode::Git => ControlSection::Git,
        };
        let count = self.sidebar_item_count();
        let navigated = matches!(&key, UiKey::Up | UiKey::Down | UiKey::Home | UiKey::End);
        match key {
            UiKey::Up => match self.sidebar_mode {
                SidebarMode::Files => self.files_cursor = self.files_cursor.saturating_sub(1),
                SidebarMode::Git => self.git_cursor = self.git_cursor.saturating_sub(1),
            },
            UiKey::Down if count > 0 => match self.sidebar_mode {
                SidebarMode::Files => self.files_cursor = (self.files_cursor + 1).min(count - 1),
                SidebarMode::Git => self.git_cursor = (self.git_cursor + 1).min(count - 1),
            },
            UiKey::Home => match self.sidebar_mode {
                SidebarMode::Files => self.files_cursor = 0,
                SidebarMode::Git => self.git_cursor = 0,
            },
            UiKey::End if count > 0 => match self.sidebar_mode {
                SidebarMode::Files => self.files_cursor = count - 1,
                SidebarMode::Git => self.git_cursor = count - 1,
            },
            UiKey::Enter if self.sidebar_mode == SidebarMode::Files => {
                let Some(entry_index) = self
                    .visible_workspace_entry_indices()
                    .get(self.files_cursor)
                    .copied()
                else {
                    return AppAction::None;
                };
                return self.select_inspector_item(SidebarMode::Files, entry_index);
            }
            UiKey::Enter if self.sidebar_mode == SidebarMode::Git => {
                return self.activate_selected_git_item();
            }
            UiKey::Char('+') | UiKey::Insert if self.sidebar_mode == SidebarMode::Git => {
                return self.begin_create_worktree();
            }
            UiKey::Delete if self.sidebar_mode == SidebarMode::Git => {
                return self.remove_selected_git_worktree();
            }
            UiKey::Char('r') => return self.inspect_selected_workspace(),
            UiKey::Char('f') => return self.select_sidebar_mode(SidebarMode::Files),
            UiKey::Char('g') => return self.select_sidebar_mode(SidebarMode::Git),
            _ => {}
        }
        if navigated {
            let reserved_rows = match self.sidebar_mode {
                SidebarMode::Files => 2,
                SidebarMode::Git => 3,
            };
            self.reveal_inspector_selection(
                self.sidebar_mode,
                self.layout.spaces.height.saturating_sub(reserved_rows),
            );
        }
        AppAction::None
    }

    fn sidebar_item_count(&self) -> usize {
        match self.sidebar_mode {
            SidebarMode::Files => self.visible_workspace_entry_indices().len(),
            SidebarMode::Git => self.git_item_count(),
        }
    }

    fn git_item_count(&self) -> usize {
        let Some(inspection) = self.selected_workspace_inspection() else {
            return 0;
        };
        let details = if inspection.git.status.is_empty() {
            inspection.git.recent_commits.len()
        } else {
            inspection.git.status.len()
        };
        inspection.git.worktrees.len().saturating_add(details)
    }

    fn activate_selected_git_item(&mut self) -> AppAction {
        let worktree_count = self
            .selected_workspace_inspection()
            .map(|inspection| inspection.git.worktrees.len())
            .unwrap_or(0);
        if self.git_cursor < worktree_count {
            self.activate_worktree(self.git_cursor)
        } else {
            AppAction::None
        }
    }

    fn remove_selected_git_worktree(&mut self) -> AppAction {
        let Some((index, _)) = self.selected_git_worktree() else {
            self.notice = Some("select a removable worktree".to_owned());
            return AppAction::None;
        };
        self.begin_remove_worktree(index)
    }

    fn reduce_agents(&mut self, key: UiKey) -> AppAction {
        match key {
            UiKey::Char('a') => {
                self.roster_mode = RosterMode::Agents;
                self.control_section = ControlSection::Agents;
                return AppAction::None;
            }
            UiKey::Char('w') => {
                self.roster_mode = RosterMode::Workspaces;
                self.control_section = ControlSection::Workspaces;
                return AppAction::None;
            }
            _ => {}
        }
        self.control_section = match self.roster_mode {
            RosterMode::Agents => ControlSection::Agents,
            RosterMode::Workspaces => ControlSection::Workspaces,
        };
        if self.roster_mode == RosterMode::Workspaces {
            let count = self.space_rows().len();
            let navigated = matches!(&key, UiKey::Up | UiKey::Down | UiKey::Home | UiKey::End);
            let action = match key {
                UiKey::Up if self.selected_space > 0 => {
                    self.select_workspace(self.selected_space - 1)
                }
                UiKey::Down if self.selected_space + 1 < count => {
                    self.select_workspace(self.selected_space + 1)
                }
                UiKey::Home if count > 0 => self.select_workspace(0),
                UiKey::End if count > 0 => self.select_workspace(count - 1),
                UiKey::Enter => {
                    self.focus = Focus::Spaces;
                    self.inspect_selected_workspace()
                }
                UiKey::Char('+') | UiKey::Insert => self.begin_add_space(),
                UiKey::Delete => self.remove_selected_space(),
                UiKey::Char('r') => self.inspect_selected_workspace(),
                _ => AppAction::None,
            };
            if navigated {
                self.reveal_roster_selection(
                    RosterMode::Workspaces,
                    self.layout.agents.height.saturating_sub(3),
                );
            }
            return action;
        }
        let count = self.agent_rows().len();
        let navigated = matches!(&key, UiKey::Up | UiKey::Down | UiKey::Home | UiKey::End);
        match key {
            UiKey::Up => self.selected_agent = self.selected_agent.saturating_sub(1),
            UiKey::Down if count > 0 => self.selected_agent = (self.selected_agent + 1).min(count - 1),
            UiKey::Enter => self.open_selected_agent(),
            UiKey::Delete => {
                let Some(key) = self.agent_rows().get(self.selected_agent).cloned() else {
                    self.notice = Some("no managed session selected".to_owned());
                    return AppAction::None;
                };
                if let Some(address) = self.agent_row_active_address(&key) {
                    if !self.address_is_connected(&address) {
                        self.notice = Some(format!("{} is disconnected; lifecycle action unavailable", address.node_id));
                        return AppAction::None;
                    }
                    let Some(session) = self.find_session(&address) else {
                        return AppAction::None;
                    };
                    if session.stoppable {
                        return AppAction::Stop { address: session.address.clone(), force: false };
                    }
                    if session.removable && self.find_managed_session(&key).is_none() {
                        return AppAction::Remove { address: session.address.clone() };
                    }
                }
                return self.begin_forget_selected_agent();
            }
            UiKey::Char('+') | UiKey::Insert => return self.begin_spawn(),
            UiKey::Char('r') => return self.restart_selected_agent(),
            UiKey::Char('n') => return self.begin_rename_selected_agent(),
            _ => {}
        }
        if navigated {
            self.reveal_roster_selection(
                RosterMode::Agents,
                self.layout.agents.height.saturating_sub(3),
            );
        }
        AppAction::None
    }

    fn restart_selected_agent(&mut self) -> AppAction {
        let Some(key) = self.agent_rows().get(self.selected_agent).cloned() else {
            self.notice = Some("no managed session selected".to_owned());
            return AppAction::None;
        };
        if let Some(record) = self.find_managed_session(&key) {
            let node_id = record.node_id.clone();
            let record_id = record.record_id.clone();
            let display_name = record.display_name.clone();
            let state = record.state;
            let resumable = record.has_provider_session_identity;
            let active = record.active_session.clone();
            if !self.node_is_connected(&node_id) {
                self.notice = Some(format!("{node_id} is disconnected; resume unavailable"));
                return AppAction::None;
            }
            if state == ManagedSessionState::Live || active.is_some() {
                self.notice = Some(format!("{display_name} is already live"));
                return AppAction::None;
            }
            if state != ManagedSessionState::Dormant || !resumable {
                self.notice = Some(format!(
                    "{display_name} cannot resume: {}",
                    managed_state_label(state)
                ));
                return AppAction::None;
            }
            return AppAction::ResumeSessionRecord {
                node_id,
                record_id,
                rows: self.content_rows(),
                cols: self.content_cols(),
            };
        }
        let Some(address) = self.agent_row_active_address(&key) else {
            return AppAction::None;
        };
        if !self.address_is_connected(&address) {
            self.notice = Some(format!("{} is disconnected; restart unavailable", address.node_id));
            return AppAction::None;
        }
        let Some(session) = self.find_session(&address) else {
            return AppAction::None;
        };
        if session.running {
            self.notice = Some("selected PTY is already running".to_owned());
            return AppAction::None;
        }
        if !session.restartable || !session.has_provider_session_identity {
            self.notice = Some("restart unavailable; provider session identity was not observed".to_owned());
            return AppAction::None;
        }
        AppAction::Resume {
            address: session.address.clone(),
            rows: self.content_rows(),
            cols: self.content_cols(),
        }
    }

    fn reduce_tabs(&mut self, key: UiKey) -> AppAction {
        if self.surface_mode == SurfaceMode::Grid {
            if self.grid.panes.is_empty() {
                return AppAction::None;
            }
            let current = self.grid.focused.min(self.grid.panes.len() - 1);
            let candidate = match (self.grid.preset, key) {
                (GridPreset::Quad, UiKey::Left) if current % 2 == 1 => Some(current - 1),
                (GridPreset::Quad, UiKey::Right) if current % 2 == 0 => Some(current + 1),
                (GridPreset::Quad, UiKey::Up) if current >= 2 => Some(current - 2),
                (GridPreset::Quad, UiKey::Down) => Some(current + 2),
                (GridPreset::Columns, UiKey::Left) if current > 0 => Some(current - 1),
                (GridPreset::Columns, UiKey::Right) => Some(current + 1),
                (GridPreset::Rows, UiKey::Up) if current > 0 => Some(current - 1),
                (GridPreset::Rows, UiKey::Down) => Some(current + 1),
                (_, UiKey::Enter) => {
                    self.focus = Focus::Viewport;
                    None
                }
                _ => None,
            };
            if let Some(candidate) = candidate {
                if candidate < self.grid.panes.len() {
                    self.grid.focused = candidate;
                }
            }
            return AppAction::None;
        }
        match key {
            UiKey::Left if !self.tabs.is_empty() => self.selected_tab = self.selected_tab.saturating_sub(1),
            UiKey::Right if !self.tabs.is_empty() => {
                self.selected_tab = (self.selected_tab + 1).min(self.tabs.len() - 1)
            }
            UiKey::Enter => self.focus = Focus::Viewport,
            _ => {}
        }
        AppAction::None
    }

    fn reduce_viewport(&mut self, key: UiKey) -> AppAction {
        if key == UiKey::OperatorEscape {
            self.focus = Focus::Tabs;
            return AppAction::None;
        }
        if !self.focused_session().is_some_and(|session| session.running) {
            self.notice = Some("stopped PTY is read-only; use r from agents to restart".to_owned());
            return AppAction::None;
        }
        match key {
            UiKey::Ctrl(ch) => match control_for_ctrl(ch) {
                Some(control) => self.terminal_control(control),
                None => AppAction::None,
            },
            UiKey::OperatorEscape => AppAction::None,
            UiKey::TerminalBytes(bytes) => self.for_active(|address| AppAction::TerminalBytes {
                address,
                bytes,
            }),
            UiKey::Char(ch) => self.for_active(|address| AppAction::Input {
                address,
                text: ch.to_string(),
            }),
            UiKey::Escape => self.terminal_control(TerminalControl::Escape),
            UiKey::Enter => self.terminal_control(TerminalControl::Enter),
            UiKey::Backspace => self.terminal_control(TerminalControl::Backspace),
            UiKey::Insert => self.terminal_control(TerminalControl::Insert),
            UiKey::Delete => self.terminal_control(TerminalControl::Delete),
            UiKey::Home => self.terminal_control(TerminalControl::Home),
            UiKey::End => self.terminal_control(TerminalControl::End),
            UiKey::PageUp => self.terminal_control(TerminalControl::PageUp),
            UiKey::PageDown => self.terminal_control(TerminalControl::PageDown),
            UiKey::Function(number) => self.terminal_control(function_control(number)),
            UiKey::Tab => self.terminal_control(TerminalControl::Tab),
            UiKey::BackTab => self.terminal_control(TerminalControl::BackTab),
            UiKey::Up => self.terminal_control(TerminalControl::ArrowUp),
            UiKey::Down => self.terminal_control(TerminalControl::ArrowDown),
            UiKey::Right => self.terminal_control(TerminalControl::ArrowRight),
            UiKey::Left => self.terminal_control(TerminalControl::ArrowLeft),
            UiKey::UnsupportedModifier => AppAction::None,
        }
    }

    fn reduce_spawn(&mut self, key: UiKey) -> AppAction {
        let Some(mut spawn) = self.spawn.take() else {
            self.focus = Focus::Agents;
            return AppAction::None;
        };
        let enabled = self
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
            .unwrap_or_default();
        match key {
            UiKey::Escape => {
                self.focus = Focus::Agents;
                return AppAction::None;
            }
            UiKey::Up => self.cycle_spawn_workspace(&mut spawn, false),
            UiKey::Down => self.cycle_spawn_workspace(&mut spawn, true),
            UiKey::Left => spawn.provider = cycle_provider(&enabled, spawn.provider, false),
            UiKey::Right | UiKey::Tab => {
                spawn.provider = cycle_provider(&enabled, spawn.provider, true)
            }
            UiKey::Enter => {
                self.focus = Focus::Agents;
                return AppAction::Spawn {
                    node_id: spawn.node_id,
                    workspace_id: spawn.workspace_id,
                    provider: spawn.provider,
                    rows: self.content_rows(),
                    cols: self.content_cols(),
                };
            }
            _ => {}
        }
        self.spawn = Some(spawn);
        AppAction::None
    }

    fn reduce_add_space(&mut self, key: UiKey) -> AppAction {
        let Some(mut dialog) = self.add_space.take() else {
            self.focus = Focus::Agents;
            self.roster_mode = RosterMode::Workspaces;
            return AppAction::None;
        };
        match key {
            UiKey::Escape => {
                self.focus = Focus::Agents;
                self.roster_mode = RosterMode::Workspaces;
                return AppAction::None;
            }
            UiKey::Tab | UiKey::BackTab => {
                dialog.field = match dialog.field {
                    AddSpaceField::WorkspaceId => AddSpaceField::Root,
                    AddSpaceField::Root => AddSpaceField::WorkspaceId,
                };
            }
            UiKey::Up => dialog.node_id = self.cycle_add_space_node(&dialog.node_id, false),
            UiKey::Down => dialog.node_id = self.cycle_add_space_node(&dialog.node_id, true),
            UiKey::Backspace => match dialog.field {
                AddSpaceField::WorkspaceId => { dialog.workspace_id.pop(); }
                AddSpaceField::Root => { dialog.root.pop(); }
            },
            UiKey::Char(ch) => {
                let result = append_modal_char(&mut dialog, ch);
                if let Err(message) = result {
                    self.notice = Some(message);
                }
            }
            UiKey::Enter => {
                if dialog.workspace_id.trim().is_empty() || dialog.root.trim().is_empty() {
                    self.notice = Some("workspace ID and absolute root are required".to_owned());
                } else if let Err(error) = WorkspaceId::new(dialog.workspace_id.clone()) {
                    self.notice = Some(format!("invalid workspace ID: {error}"));
                } else if !is_absolute_root(&dialog.root) {
                    self.notice = Some("workspace root must be absolute".to_owned());
                } else {
                    self.focus = Focus::Agents;
                    self.roster_mode = RosterMode::Workspaces;
                    return AppAction::RegisterWorkspace {
                        node_id: dialog.node_id,
                        workspace_id: dialog.workspace_id,
                        root: dialog.root,
                    };
                }
            }
            _ => {}
        }
        self.add_space = Some(dialog);
        AppAction::None
    }

    fn paste_add_space(&mut self, text: String) -> AppAction {
        if let Some(dialog) = self.add_space.as_mut() {
            if let Err(message) = append_modal_paste(dialog, &text) {
                self.notice = Some(message);
            }
        }
        AppAction::None
    }

    fn reduce_create_worktree(&mut self, key: UiKey) -> AppAction {
        let Some(mut dialog) = self.create_worktree.take() else {
            self.focus = Focus::Spaces;
            return AppAction::None;
        };
        match key {
            UiKey::Escape => {
                self.focus = Focus::Spaces;
                return AppAction::None;
            }
            UiKey::Tab => dialog.field = next_create_worktree_field(dialog.field, true),
            UiKey::BackTab => dialog.field = next_create_worktree_field(dialog.field, false),
            UiKey::Up => dialog.field = next_create_worktree_field(dialog.field, false),
            UiKey::Down => dialog.field = next_create_worktree_field(dialog.field, true),
            UiKey::Backspace => {
                create_worktree_field_mut(&mut dialog).pop();
            }
            UiKey::Char(ch) => {
                if let Err(message) = append_create_worktree_char(&mut dialog, ch) {
                    self.notice = Some(message);
                }
            }
            UiKey::Enter => {
                if dialog.workspace_id.trim().is_empty()
                    || dialog.target_root.trim().is_empty()
                    || dialog.branch.trim().is_empty()
                {
                    self.notice = Some("workspace ID, absolute target root, and branch are required".to_owned());
                } else if let Err(error) = WorkspaceId::new(dialog.workspace_id.clone()) {
                    self.notice = Some(format!("invalid workspace ID: {error}"));
                } else if !is_absolute_root(&dialog.target_root) {
                    self.notice = Some("worktree target root must be absolute".to_owned());
                } else {
                    self.focus = Focus::Spaces;
                    return AppAction::CreateWorktree {
                        node_id: dialog.node_id,
                        source_workspace_id: dialog.source_workspace_id,
                        workspace_id: dialog.workspace_id,
                        target_root: dialog.target_root,
                        branch: dialog.branch,
                        base: (!dialog.base.trim().is_empty()).then_some(dialog.base),
                    };
                }
            }
            _ => {}
        }
        self.create_worktree = Some(dialog);
        AppAction::None
    }

    fn paste_create_worktree(&mut self, text: String) -> AppAction {
        let Some(dialog) = self.create_worktree.as_mut() else {
            return AppAction::None;
        };
        if text.contains(['\r', '\n', '\0']) {
            self.notice = Some("worktree field cannot contain control characters".to_owned());
            return AppAction::None;
        }
        let field = dialog.field;
        let current = create_worktree_field_mut(dialog);
        let limit = match field {
            CreateWorktreeField::WorkspaceId => MAX_NODE_IDENTIFIER_BYTES,
            CreateWorktreeField::TargetRoot => MAX_WORKSPACE_ROOT_BYTES,
            CreateWorktreeField::Branch | CreateWorktreeField::Base => MAX_NODE_TEXT_BYTES,
        };
        if current.len().saturating_add(text.len()) > limit {
            self.notice = Some("worktree field exceeds protocol limit".to_owned());
            return AppAction::None;
        }
        if field == CreateWorktreeField::WorkspaceId
            && text.chars().any(|ch| !workspace_id_char(ch))
        {
            self.notice = Some("workspace ID contains unsupported characters".to_owned());
            return AppAction::None;
        }
        current.push_str(&text);
        AppAction::None
    }

    fn reduce_remove_worktree(&mut self, key: UiKey) -> AppAction {
        let Some(dialog) = self.remove_worktree.take() else {
            self.focus = Focus::Spaces;
            return AppAction::None;
        };
        match key {
            UiKey::Char('y') | UiKey::Char('Y') | UiKey::Enter => {
                self.focus = Focus::Spaces;
                AppAction::RemoveWorktree {
                    node_id: dialog.node_id,
                    source_workspace_id: dialog.source_workspace_id,
                    target_root: dialog.target_root,
                }
            }
            UiKey::Escape | UiKey::Char('n') | UiKey::Char('N') => {
                self.focus = Focus::Spaces;
                AppAction::None
            }
            _ => {
                self.remove_worktree = Some(dialog);
                AppAction::None
            }
        }
    }

    fn reduce_rename_session(&mut self, key: UiKey) -> AppAction {
        let Some(mut dialog) = self.rename_session.take() else {
            self.focus = Focus::Agents;
            return AppAction::None;
        };
        match key {
            UiKey::Escape => {
                self.focus = Focus::Agents;
                AppAction::None
            }
            UiKey::Backspace => {
                dialog.display_name.pop();
                self.rename_session = Some(dialog);
                AppAction::None
            }
            UiKey::Char(ch) => {
                if let Err(message) = append_session_name(&mut dialog.display_name, ch.to_string()) {
                    self.notice = Some(message);
                }
                self.rename_session = Some(dialog);
                AppAction::None
            }
            UiKey::Enter => {
                let name = dialog.display_name.trim().to_owned();
                if let Err(message) = validate_session_name(&name) {
                    self.notice = Some(message);
                    self.rename_session = Some(dialog);
                    return AppAction::None;
                }
                self.focus = Focus::Agents;
                if name == dialog.original_name {
                    return AppAction::None;
                }
                AppAction::RenameSessionRecord {
                    node_id: dialog.node_id,
                    record_id: dialog.record_id,
                    display_name: name,
                }
            }
            _ => {
                self.rename_session = Some(dialog);
                AppAction::None
            }
        }
    }

    fn paste_rename_session(&mut self, text: String) -> AppAction {
        let Some(dialog) = self.rename_session.as_mut() else {
            return AppAction::None;
        };
        if let Err(message) = append_session_name(&mut dialog.display_name, text) {
            self.notice = Some(message);
        }
        AppAction::None
    }

    fn reduce_forget_session(&mut self, key: UiKey) -> AppAction {
        let Some(dialog) = self.forget_session.take() else {
            self.focus = Focus::Agents;
            return AppAction::None;
        };
        match key {
            UiKey::Char('y') | UiKey::Char('Y') | UiKey::Enter => {
                self.focus = Focus::Agents;
                AppAction::ForgetSessionRecord {
                    node_id: dialog.node_id,
                    record_id: dialog.record_id,
                }
            }
            UiKey::Escape | UiKey::Char('n') | UiKey::Char('N') => {
                self.focus = Focus::Agents;
                AppAction::None
            }
            _ => {
                self.forget_session = Some(dialog);
                AppAction::None
            }
        }
    }

    fn cycle_color_mode(&mut self) {
        self.color_mode = self.color_mode.cycle();
        self.notice = Some(format!("terminal style: {}", self.color_mode));
    }

    fn begin_settings(&mut self) {
        self.settings_return_focus = self.focus;
        self.focus = Focus::Settings;
    }

    fn close_settings(&mut self) {
        self.focus = if self.menu_placement == MenuPlacement::Modal
            && matches!(self.settings_return_focus, Focus::Spaces | Focus::Agents)
        {
            Focus::Tabs
        } else {
            self.settings_return_focus
        };
    }

    fn toggle_menu_placement(&mut self) {
        self.menu_placement = match self.menu_placement {
            MenuPlacement::Sidebar => MenuPlacement::Modal,
            MenuPlacement::Modal => MenuPlacement::Sidebar,
        };
        if self.menu_placement == MenuPlacement::Sidebar
            && self.sidebar_presentation == SidebarPresentation::Activity
        {
            self.synchronize_activity_section();
        }
    }

    fn toggle_sidebar_presentation(&mut self) {
        self.sidebar_presentation = match self.sidebar_presentation {
            SidebarPresentation::Split => SidebarPresentation::Activity,
            SidebarPresentation::Activity => SidebarPresentation::Split,
        };
        if self.sidebar_presentation == SidebarPresentation::Activity {
            self.synchronize_activity_section();
        }
        self.sidebar_collapsed = false;
    }

    fn synchronize_activity_section(&mut self) {
        if self.control_section == ControlSection::Settings {
            self.control_section = ControlSection::Files;
        }
        match self.control_section {
            ControlSection::Files => self.sidebar_mode = SidebarMode::Files,
            ControlSection::Git => self.sidebar_mode = SidebarMode::Git,
            ControlSection::Agents => self.roster_mode = RosterMode::Agents,
            ControlSection::Workspaces => self.roster_mode = RosterMode::Workspaces,
            ControlSection::Settings => unreachable!("settings is normalized above"),
        }
    }

    fn toggle_sidebar_collapsed(&mut self) {
        if self.menu_placement == MenuPlacement::Modal
            || self.sidebar_presentation != SidebarPresentation::Activity
        {
            return;
        }
        self.sidebar_collapsed = !self.sidebar_collapsed;
    }

    fn reduce_settings(&mut self, key: UiKey) -> AppAction {
        if self.menu_placement == MenuPlacement::Sidebar {
            match key {
                UiKey::Escape => self.close_settings(),
                UiKey::Char('s') | UiKey::Enter => self.cycle_color_mode(),
                UiKey::Char('m') | UiKey::Left | UiKey::Right => {
                    self.toggle_menu_placement()
                }
                UiKey::Char('p') => self.toggle_sidebar_presentation(),
                UiKey::Char('b') => self.toggle_sidebar_collapsed(),
                _ => {}
            }
            return AppAction::None;
        }
        match key {
            UiKey::Escape => self.close_settings(),
            UiKey::Left | UiKey::BackTab => {
                return self.select_control_section(cycle_control_section(
                    self.control_section,
                    false,
                ));
            }
            UiKey::Right | UiKey::Tab => {
                return self.select_control_section(cycle_control_section(
                    self.control_section,
                    true,
                ));
            }
            UiKey::Up => return self.move_control_selection(true),
            UiKey::Down => return self.move_control_selection(false),
            UiKey::Enter => match self.control_section {
                ControlSection::Files => {
                    let Some(entry_index) = self
                        .visible_workspace_entry_indices()
                        .get(self.files_cursor)
                        .copied()
                    else {
                        return AppAction::None;
                    };
                    return self.select_inspector_item(SidebarMode::Files, entry_index);
                }
                ControlSection::Git => return self.activate_selected_git_item(),
                ControlSection::Agents => self.open_selected_agent(),
                ControlSection::Workspaces => return self.inspect_selected_workspace(),
                ControlSection::Settings => {}
            },
            UiKey::Char('+') | UiKey::Insert => match self.control_section {
                ControlSection::Agents => return self.begin_spawn(),
                ControlSection::Workspaces => return self.begin_add_space(),
                ControlSection::Git => return self.begin_create_worktree(),
                ControlSection::Files | ControlSection::Settings => {}
            },
            UiKey::Delete => match self.control_section {
                ControlSection::Agents => return self.reduce_agents(UiKey::Delete),
                ControlSection::Workspaces => return self.remove_selected_space(),
                ControlSection::Git => return self.remove_selected_git_worktree(),
                ControlSection::Files | ControlSection::Settings => {}
            },
            UiKey::Char('r') => match self.control_section {
                ControlSection::Files | ControlSection::Git | ControlSection::Workspaces => {}
                ControlSection::Agents => return self.restart_selected_agent(),
                ControlSection::Settings => {}
            },
            UiKey::Char('s') => self.cycle_color_mode(),
            UiKey::Char('m') => self.toggle_menu_placement(),
            UiKey::Char('p') if self.control_section == ControlSection::Settings => {
                self.toggle_sidebar_presentation()
            }
            UiKey::Char('b') if self.control_section == ControlSection::Settings => {
                self.toggle_sidebar_collapsed()
            }
            UiKey::Char('n') if self.control_section == ControlSection::Agents => {
                return self.begin_rename_selected_agent();
            }
            UiKey::Home => match self.control_section {
                ControlSection::Files => self.files_cursor = 0,
                ControlSection::Git => self.git_cursor = 0,
                ControlSection::Agents => self.selected_agent = 0,
                ControlSection::Workspaces => self.selected_space = 0,
                ControlSection::Settings => {}
            },
            UiKey::End => match self.control_section {
                ControlSection::Files => {
                    self.files_cursor = self.visible_workspace_entry_indices().len().saturating_sub(1)
                }
                ControlSection::Git => self.git_cursor = self.git_item_count().saturating_sub(1),
                ControlSection::Agents => {
                    self.selected_agent = self.agent_rows().len().saturating_sub(1)
                }
                ControlSection::Workspaces => {
                    self.selected_space = self.space_rows().len().saturating_sub(1)
                }
                ControlSection::Settings => {}
            }
            _ => {}
        }
        AppAction::None
    }

    fn next_focus(&self) -> Focus {
        if self.menu_placement == MenuPlacement::Modal {
            return match self.focus {
                Focus::Tabs => Focus::Viewport,
                Focus::Viewport => Focus::Tabs,
                Focus::Spaces | Focus::Agents => Focus::Tabs,
                Focus::Spawn
                | Focus::AddSpace
                | Focus::CreateWorktree
                | Focus::RemoveWorktree
                | Focus::RenameSession
                | Focus::ForgetSession
                | Focus::Settings => self.focus,
            };
        }
        if self.sidebar_presentation == SidebarPresentation::Activity {
            let sidebar = self.sidebar_focus();
            return match self.focus {
                Focus::Tabs => Focus::Viewport,
                Focus::Viewport => sidebar,
                Focus::Spaces | Focus::Agents => {
                    if self.focus == sidebar { Focus::Tabs } else { sidebar }
                }
                Focus::Spawn
                | Focus::AddSpace
                | Focus::CreateWorktree
                | Focus::RemoveWorktree
                | Focus::RenameSession
                | Focus::ForgetSession
                | Focus::Settings => self.focus,
            };
        }
        match self.focus {
            Focus::Spaces => Focus::Agents,
            Focus::Agents => Focus::Tabs,
            Focus::Tabs => Focus::Viewport,
            Focus::Viewport => Focus::Spaces,
            Focus::Spawn
            | Focus::AddSpace
            | Focus::CreateWorktree
            | Focus::RemoveWorktree
            | Focus::RenameSession
            | Focus::ForgetSession
            | Focus::Settings => self.focus,
        }
    }

    fn previous_focus(&self) -> Focus {
        if self.menu_placement == MenuPlacement::Modal {
            return match self.focus {
                Focus::Tabs => Focus::Viewport,
                Focus::Viewport => Focus::Tabs,
                Focus::Spaces | Focus::Agents => Focus::Tabs,
                Focus::Spawn
                | Focus::AddSpace
                | Focus::CreateWorktree
                | Focus::RemoveWorktree
                | Focus::RenameSession
                | Focus::ForgetSession
                | Focus::Settings => self.focus,
            };
        }
        if self.sidebar_presentation == SidebarPresentation::Activity {
            let sidebar = self.sidebar_focus();
            return match self.focus {
                Focus::Tabs => sidebar,
                Focus::Viewport => Focus::Tabs,
                Focus::Spaces | Focus::Agents => {
                    if self.focus == sidebar { Focus::Viewport } else { sidebar }
                }
                Focus::Spawn
                | Focus::AddSpace
                | Focus::CreateWorktree
                | Focus::RemoveWorktree
                | Focus::RenameSession
                | Focus::ForgetSession
                | Focus::Settings => self.focus,
            };
        }
        match self.focus {
            Focus::Spaces => Focus::Viewport,
            Focus::Agents => Focus::Spaces,
            Focus::Tabs => Focus::Agents,
            Focus::Viewport => Focus::Tabs,
            Focus::Spawn
            | Focus::AddSpace
            | Focus::CreateWorktree
            | Focus::RemoveWorktree
            | Focus::RenameSession
            | Focus::ForgetSession
            | Focus::Settings => self.focus,
        }
    }

    fn sidebar_focus(&self) -> Focus {
        if self.sidebar_presentation != SidebarPresentation::Activity {
            return Focus::Agents;
        }
        match self.control_section {
            ControlSection::Files | ControlSection::Git => Focus::Spaces,
            ControlSection::Agents | ControlSection::Workspaces => Focus::Agents,
            ControlSection::Settings => Focus::Tabs,
        }
    }

    fn for_active(&mut self, make: impl FnOnce(SessionAddress) -> AppAction) -> AppAction {
        let Some(address) = self.focused_session().map(|session| session.address.clone()) else {
            self.notice = Some("no PTY selected".to_owned());
            return AppAction::None;
        };
        self.terminal_scroll_offsets.remove(&address);
        make(address)
    }

    fn terminal_control(&mut self, control: TerminalControl) -> AppAction {
        self.for_active(|address| AppAction::TerminalControl { address, control })
    }
}

fn is_absolute_root(root: &str) -> bool {
    let bytes = root.as_bytes();
    root.starts_with("\\\\")
        || root.starts_with('/')
        || (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/'))
}

fn next_worktree_id(source: &str) -> String {
    let suffix = "-worktree";
    let keep = MAX_NODE_IDENTIFIER_BYTES.saturating_sub(suffix.len());
    format!("{}{suffix}", &source[..source.len().min(keep)])
}

fn workspace_id_char(ch: char) -> bool {
    ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_')
}

fn suggested_workspace_id(worktree: &GitWorktreeSnapshot) -> String {
    let source = worktree
        .branch
        .as_deref()
        .or_else(|| (!worktree.head.is_empty()).then_some(worktree.head.as_str()))
        .unwrap_or("worktree");
    let mut id = source
        .chars()
        .map(|ch| {
            let ch = ch.to_ascii_lowercase();
            if workspace_id_char(ch) { ch } else { '-' }
        })
        .take(MAX_NODE_IDENTIFIER_BYTES)
        .collect::<String>();
    while id.starts_with('-') || id.starts_with('_') {
        id.remove(0);
    }
    if id.is_empty() {
        "worktree".to_owned()
    } else {
        id
    }
}

fn worktree_can_be_removed(worktree: &GitWorktreeSnapshot) -> bool {
    !worktree.is_main && !worktree.is_bare && !worktree.locked && !worktree.prunable
}

fn next_create_worktree_field(
    current: CreateWorktreeField,
    forward: bool,
) -> CreateWorktreeField {
    let fields = [
        CreateWorktreeField::WorkspaceId,
        CreateWorktreeField::TargetRoot,
        CreateWorktreeField::Branch,
        CreateWorktreeField::Base,
    ];
    let index = fields.iter().position(|field| *field == current).unwrap_or(0);
    let next = if forward {
        (index + 1) % fields.len()
    } else if index == 0 {
        fields.len() - 1
    } else {
        index - 1
    };
    fields[next]
}

fn create_worktree_field_mut(dialog: &mut CreateWorktreeDialog) -> &mut String {
    match dialog.field {
        CreateWorktreeField::WorkspaceId => &mut dialog.workspace_id,
        CreateWorktreeField::TargetRoot => &mut dialog.target_root,
        CreateWorktreeField::Branch => &mut dialog.branch,
        CreateWorktreeField::Base => &mut dialog.base,
    }
}

fn append_create_worktree_char(
    dialog: &mut CreateWorktreeDialog,
    ch: char,
) -> Result<(), String> {
    if ch.is_control() {
        return Err("worktree field cannot contain control characters".to_owned());
    }
    let field = dialog.field;
    if field == CreateWorktreeField::WorkspaceId && !workspace_id_char(ch) {
        return Err("workspace ID contains unsupported characters".to_owned());
    }
    let limit = match field {
        CreateWorktreeField::WorkspaceId => MAX_NODE_IDENTIFIER_BYTES,
        CreateWorktreeField::TargetRoot => MAX_WORKSPACE_ROOT_BYTES,
        CreateWorktreeField::Branch | CreateWorktreeField::Base => MAX_NODE_TEXT_BYTES,
    };
    let target = create_worktree_field_mut(dialog);
    if target.len().saturating_add(ch.len_utf8()) > limit {
        return Err("worktree field exceeds protocol limit".to_owned());
    }
    target.push(ch);
    Ok(())
}

fn wheel_scroll_start(current: usize, maximum: usize, up: bool) -> usize {
    if up {
        current.saturating_sub(WHEEL_SCROLL_LINES)
    } else {
        current.saturating_add(WHEEL_SCROLL_LINES).min(maximum)
    }
}

fn visible_selection_start(
    current: usize,
    selected: usize,
    count: usize,
    capacity: usize,
) -> usize {
    if count == 0 || capacity == 0 {
        return 0;
    }
    let maximum = count.saturating_sub(capacity);
    let current = current.min(maximum);
    if selected < current {
        selected
    } else if selected >= current.saturating_add(capacity) {
        selected.saturating_add(1).saturating_sub(capacity).min(maximum)
    } else {
        current
    }
}

fn moved_index(current: usize, count: usize, up: bool) -> usize {
    if count == 0 {
        0
    } else if up {
        current.saturating_sub(1)
    } else {
        current.saturating_add(1).min(count - 1)
    }
}

fn scrollback_advance(previous: &[Vec<u8>], current: &[Vec<u8>]) -> usize {
    let maximum_overlap = previous.len().min(current.len());
    for overlap in (1..=maximum_overlap).rev() {
        if previous[previous.len() - overlap..] == current[..overlap] {
            return current.len().saturating_sub(overlap);
        }
    }
    0
}

fn terminal_mouse_wheel_bytes(
    encoding: TerminalMouseProtocolEncoding,
    column: u16,
    row: u16,
    up: bool,
) -> Vec<u8> {
    let button = if up { 64_u32 } else { 65_u32 };
    match encoding {
        TerminalMouseProtocolEncoding::Sgr => {
            format!("\x1b[<{button};{column};{row}M").into_bytes()
        }
        TerminalMouseProtocolEncoding::Default => vec![
            0x1b,
            b'[',
            b'M',
            (button + 32) as u8,
            u8::try_from(u32::from(column.min(223)) + 32).unwrap_or(u8::MAX),
            u8::try_from(u32::from(row.min(223)) + 32).unwrap_or(u8::MAX),
        ],
        TerminalMouseProtocolEncoding::Utf8 => {
            let mut bytes = b"\x1b[M".to_vec();
            for value in [button + 32, u32::from(column) + 32, u32::from(row) + 32] {
                let character = char::from_u32(value).unwrap_or('\u{fffd}');
                let mut encoded = [0_u8; 4];
                bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            }
            bytes
        }
    }
}

fn cycle_control_section(current: ControlSection, forward: bool) -> ControlSection {
    let current = ControlSection::ALL
        .iter()
        .position(|section| *section == current)
        .unwrap_or(0);
    let next = if forward {
        (current + 1) % ControlSection::ALL.len()
    } else if current == 0 {
        ControlSection::ALL.len() - 1
    } else {
        current - 1
    };
    ControlSection::ALL[next]
}

fn append_modal_char(dialog: &mut AddSpaceDialog, ch: char) -> Result<(), String> {
    if ch.is_control() {
        return Err("control characters are not allowed in space fields".to_owned());
    }
    match dialog.field {
        AddSpaceField::WorkspaceId => {
            let mut candidate = dialog.workspace_id.clone();
            candidate.push(ch);
            if !workspace_id_prefix_valid(&candidate) {
                return Err(format!(
                    "workspace ID uses lowercase letters, digits, '-' or '_' and is limited to {MAX_NODE_IDENTIFIER_BYTES} bytes"
                ));
            }
            dialog.workspace_id = candidate;
        }
        AddSpaceField::Root => {
            if dialog.root.len().saturating_add(ch.len_utf8()) > MAX_WORKSPACE_ROOT_BYTES {
                return Err(format!("workspace root exceeds the {MAX_WORKSPACE_ROOT_BYTES}-byte limit"));
            }
            dialog.root.push(ch);
        }
    }
    Ok(())
}

fn append_modal_paste(dialog: &mut AddSpaceDialog, text: &str) -> Result<(), String> {
    if text.chars().any(char::is_control) {
        return Err("control characters are not allowed in space fields".to_owned());
    }
    match dialog.field {
        AddSpaceField::WorkspaceId => {
            let candidate = format!("{}{text}", dialog.workspace_id);
            WorkspaceId::new(candidate.clone()).map_err(|error| format!("invalid workspace ID: {error}"))?;
            dialog.workspace_id = candidate;
        }
        AddSpaceField::Root => {
            if dialog.root.len().saturating_add(text.len()) > MAX_WORKSPACE_ROOT_BYTES {
                return Err(format!("workspace root exceeds the {MAX_WORKSPACE_ROOT_BYTES}-byte limit"));
            }
            dialog.root.push_str(text);
        }
    }
    Ok(())
}

fn workspace_id_prefix_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NODE_IDENTIFIER_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        && !matches!(value.as_bytes().first(), Some(b'-' | b'_'))
}

fn control_for_ctrl(ch: char) -> Option<TerminalControl> {
    match ch.to_ascii_lowercase() {
        'a' => Some(TerminalControl::ControlA),
        'b' => Some(TerminalControl::ControlB),
        'c' => Some(TerminalControl::Interrupt),
        'd' => Some(TerminalControl::EndOfFile),
        'e' => Some(TerminalControl::ControlE),
        'f' => Some(TerminalControl::ControlF),
        'g' => Some(TerminalControl::ControlG),
        'h' => Some(TerminalControl::ControlH),
        'i' => Some(TerminalControl::ControlI),
        'j' => Some(TerminalControl::ControlJ),
        'k' => Some(TerminalControl::ControlK),
        'l' => Some(TerminalControl::ControlL),
        'm' => Some(TerminalControl::ControlM),
        'n' => Some(TerminalControl::ControlN),
        'o' => Some(TerminalControl::ControlO),
        'p' => Some(TerminalControl::ControlP),
        'q' => Some(TerminalControl::ControlQ),
        'r' => Some(TerminalControl::ControlR),
        's' => Some(TerminalControl::ControlS),
        't' => Some(TerminalControl::ControlT),
        'u' => Some(TerminalControl::ControlU),
        'v' => Some(TerminalControl::ControlV),
        'w' => Some(TerminalControl::ControlW),
        'x' => Some(TerminalControl::ControlX),
        'y' => Some(TerminalControl::ControlY),
        'z' => Some(TerminalControl::ControlZ),
        _ => None,
    }
}

fn function_control(number: u8) -> TerminalControl {
    match number {
        1 => TerminalControl::Function1,
        2 => TerminalControl::Function2,
        3 => TerminalControl::Function3,
        4 => TerminalControl::Function4,
        5 => TerminalControl::Function5,
        6 => TerminalControl::Function6,
        7 => TerminalControl::Function7,
        8 => TerminalControl::Function8,
        9 => TerminalControl::Function9,
        10 => TerminalControl::Function10,
        11 => TerminalControl::Function11,
        _ => TerminalControl::Function12,
    }
}

fn cycle_provider(enabled: &[Provider], current: Provider, forward: bool) -> Provider {
    if enabled.is_empty() {
        return current;
    }
    let current = enabled.iter().position(|provider| *provider == current).unwrap_or(0);
    let index = if forward {
        (current + 1) % enabled.len()
    } else if current == 0 {
        enabled.len() - 1
    } else {
        current - 1
    };
    enabled[index]
}

pub fn managed_state_label(state: ManagedSessionState) -> &'static str {
    match state {
        ManagedSessionState::IdentityPending => "identity pending",
        ManagedSessionState::Live => "live",
        ManagedSessionState::Dormant => "dormant",
        ManagedSessionState::Unavailable => "unavailable",
    }
}

fn append_session_name(target: &mut String, text: String) -> Result<(), String> {
    if text.chars().any(char::is_control) {
        return Err("session name cannot contain control characters".to_owned());
    }
    if target.len().saturating_add(text.len()) > MAX_SESSION_DISPLAY_NAME_BYTES {
        return Err("session name exceeds protocol limit".to_owned());
    }
    target.push_str(&text);
    Ok(())
}

fn validate_session_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("session name cannot be empty".to_owned());
    }
    if name.len() > MAX_SESSION_DISPLAY_NAME_BYTES || name.chars().any(char::is_control) {
        return Err("session name is invalid".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_node_protocol::{
        GitSnapshot, GitStatusEntry, GitWorktreeSnapshot, WorkspaceEntry,
    };

    fn fixture() -> App {
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 2,
        };
        let mut app = App::default();
        app.nodes.push(NodeView {
            node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            connection: ConnectionState::Connected,
            controller_owned: true,
            event_sequence: 10,
            session_records: Vec::new(),
            workspaces: vec![WorkspaceView {
                workspace_id: "workspace-a".to_owned(),
                label: "nemo".to_owned(),
                canonical_root: r"C:\work\nemo".to_owned(),
                providers: Provider::ALL
                    .into_iter()
                    .map(|provider| ProviderInventory { provider, enabled: true })
                    .collect(),
                sessions: vec![SessionView {
                    address: address.clone(),
                    provider: Provider::Codex,
                    status: "running".to_owned(),
                    running: true,
                    stoppable: true,
                    removable: false,
                    restartable: false,
                    attention: false,
                    has_provider_session_identity: true,
                    terminal_formatted: b"codex".to_vec(),
                    terminal_scrollback: Vec::new(),
                    terminal_alternate_screen: false,
                    terminal_mouse_protocol_enabled: false,
                    terminal_mouse_protocol_encoding: TerminalMouseProtocolEncoding::Default,
                    terminal_cursor: Some((0, 5)),
                }],
            }],
        });
        app.tabs.push(SessionTab { address });
        app.layout.viewport = Rect::new(26, 1, 74, 23);
        app
    }

    fn add_session(app: &mut App, instance_id: u64, provider: Provider) -> SessionAddress {
        let mut session = app.nodes[0].workspaces[0].sessions[0].clone();
        session.address.instance_id = instance_id;
        session.provider = provider;
        let address = session.address.clone();
        app.nodes[0].workspaces[0].sessions.push(session);
        address
    }

    fn add_managed_record(
        app: &mut App,
        record_id: &str,
        display_name: &str,
        state: ManagedSessionState,
        active_session: Option<SessionAddress>,
        has_identity: bool,
    ) -> AgentRowKey {
        app.nodes[0].session_records.push(ManagedSessionView {
            node_id: "node-a".to_owned(),
            record_id: record_id.to_owned(),
            display_name: display_name.to_owned(),
            provider: Provider::Codex,
            mode: SessionMode::Pty,
            state,
            workspace_id: "workspace-a".to_owned(),
            canonical_root: r"C:\work\nemo".to_owned(),
            has_provider_session_identity: has_identity,
            active_session,
            last_error: None,
        });
        AgentRowKey::Managed {
            node_id: "node-a".to_owned(),
            record_id: record_id.to_owned(),
        }
    }

    fn workspace_inspection() -> WorkspaceInspection {
        WorkspaceInspection {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            entries: vec![
                WorkspaceEntry {
                    relative_path: "src".to_owned(),
                    kind: WorkspaceEntryKind::Directory,
                },
                WorkspaceEntry {
                    relative_path: "src/bin".to_owned(),
                    kind: WorkspaceEntryKind::Directory,
                },
                WorkspaceEntry {
                    relative_path: "src/bin/main.rs".to_owned(),
                    kind: WorkspaceEntryKind::File,
                },
                WorkspaceEntry {
                    relative_path: "Cargo.toml".to_owned(),
                    kind: WorkspaceEntryKind::File,
                },
            ],
            tree_truncated: false,
            git: GitSnapshot {
                is_repository: true,
                branch: Some("main".to_owned()),
                status: vec![
                    GitStatusEntry {
                        index_status: " ".to_owned(),
                        worktree_status: "M".to_owned(),
                        path: "src/bin/main.rs".to_owned(),
                    },
                    GitStatusEntry {
                        index_status: "?".to_owned(),
                        worktree_status: "?".to_owned(),
                        path: "notes.txt".to_owned(),
                    },
                ],
                recent_commits: Vec::new(),
                worktrees: Vec::new(),
                truncated: false,
                diagnostic: None,
            },
        }
    }

    fn worktree(path: &str, branch: &str, workspace_id: Option<&str>) -> GitWorktreeSnapshot {
        GitWorktreeSnapshot {
            path: path.to_owned(),
            head: "0123456789abcdef".to_owned(),
            branch: Some(branch.to_owned()),
            is_bare: false,
            is_main: false,
            locked: false,
            lock_reason: None,
            prunable: false,
            prunable_reason: None,
            workspace_id: workspace_id.map(|id| WorkspaceId::new(id).unwrap()),
        }
    }

    #[test]
    fn non_viewport_typing_never_opens_or_sends_common_input() {
        let mut app = fixture();
        for focus in [Focus::Spaces, Focus::Agents, Focus::Tabs] {
            app.focus = focus;
            assert_eq!(app.reduce(UiKey::Char('x')), AppAction::None);
            assert_eq!(app.paste("paste".to_owned()), AppAction::None);
            assert_eq!(app.focus, focus);
        }
    }

    #[test]
    fn activity_focus_cycle_visits_only_the_selected_sidebar_surface() {
        let mut app = fixture();
        app.sidebar_presentation = SidebarPresentation::Activity;
        app.control_section = ControlSection::Files;
        app.focus = Focus::Spaces;
        assert_eq!(app.next_focus(), Focus::Tabs);
        app.focus = Focus::Tabs;
        assert_eq!(app.next_focus(), Focus::Viewport);
        app.focus = Focus::Viewport;
        assert_eq!(app.next_focus(), Focus::Spaces);
        assert_eq!(app.previous_focus(), Focus::Tabs);

        app.control_section = ControlSection::Agents;
        app.focus = Focus::Agents;
        assert_eq!(app.next_focus(), Focus::Tabs);
        app.focus = Focus::Tabs;
        assert_eq!(app.previous_focus(), Focus::Agents);
    }

    #[test]
    fn tab_click_focuses_exact_pty_and_typing_is_direct() {
        let mut app = fixture();
        app.focus = Focus::Agents;
        app.layout.hits.push(HitRegion { rect: Rect::new(26, 0, 8, 1), target: HitTarget::Tab(0) });
        assert_eq!(app.click(27, 0), AppAction::None);
        assert_eq!(app.focus, Focus::Agents);
        assert_eq!(app.drop_at(27, 0), AppAction::None);
        assert_eq!(app.focus, Focus::Viewport);
        assert!(matches!(app.reduce(UiKey::Char('x')), AppAction::Input { text, .. } if text == "x"));
    }

    #[test]
    fn modal_focus_blocks_click_through_to_base_layout() {
        let mut app = fixture();
        app.focus = Focus::Agents;
        app.roster_mode = RosterMode::Workspaces;
        app.reduce(UiKey::Char('+'));
        app.layout.hits.push(HitRegion {
            rect: Rect::new(26, 0, 3, 1),
            target: HitTarget::AddTab,
        });

        assert_eq!(app.click(27, 0), AppAction::None);
        assert_eq!(app.focus, Focus::AddSpace);
        assert!(app.add_space.is_some());
        assert!(app.spawn.is_none());
    }

    #[test]
    fn ctrl_g_reaches_provider_and_explicit_operator_escape_leaves_viewport() {
        let mut app = fixture();
        app.focus = Focus::Viewport;
        assert!(matches!(
            app.reduce(UiKey::Ctrl('g')),
            AppAction::TerminalControl { control: TerminalControl::ControlG, .. }
        ));
        assert_eq!(app.focus, Focus::Viewport);
        assert_eq!(app.reduce(UiKey::OperatorEscape), AppAction::None);
        assert_eq!(app.focus, Focus::Tabs);
        app.focus = Focus::Viewport;
        assert!(matches!(app.reduce(UiKey::Ctrl('t')), AppAction::TerminalControl { control: TerminalControl::ControlT, .. }));
    }

    #[test]
    fn alt_provider_shortcut_is_forwarded_as_exact_terminal_bytes() {
        let mut app = fixture();
        app.focus = Focus::Viewport;
        assert!(matches!(
            app.reduce(UiKey::TerminalBytes(b"\x1bp".to_vec())),
            AppAction::TerminalBytes { bytes, .. } if bytes == b"\x1bp"
        ));
    }

    #[test]
    fn agents_stop_running_remove_stopped_and_restart_by_identity() {
        let mut app = fixture();
        app.focus = Focus::Agents;
        assert!(matches!(app.reduce(UiKey::Delete), AppAction::Stop { force: false, .. }));
        let session = &mut app.nodes[0].workspaces[0].sessions[0];
        session.running = false;
        session.stoppable = false;
        session.removable = true;
        session.restartable = true;
        assert!(matches!(app.reduce(UiKey::Delete), AppAction::Remove { .. }));
        assert!(matches!(app.reduce(UiKey::Char('r')), AppAction::Resume { rows: 23, cols: 74, .. }));
    }

    #[test]
    fn spawn_is_provider_only_and_uses_selected_route() {
        let mut app = fixture();
        app.focus = Focus::Agents;
        assert_eq!(app.reduce(UiKey::Ctrl('n')), AppAction::None);
        assert_eq!(app.focus, Focus::Spawn);
        assert!(matches!(app.reduce(UiKey::Enter), AppAction::Spawn { node_id, workspace_id, provider: Provider::Claude, .. } if node_id == "node-a" && workspace_id == "workspace-a"));
    }

    #[test]
    fn spawn_control_targets_clicked_workspace_and_modal_can_change_route() {
        let mut app = fixture();
        let mut second = app.nodes[0].workspaces[0].clone();
        second.workspace_id = "workspace-b".to_owned();
        second.label = "other".to_owned();
        second.canonical_root = r"C:\work\other".to_owned();
        second.sessions.clear();
        second.providers = vec![ProviderInventory {
            provider: Provider::Kimi,
            enabled: true,
        }];
        app.nodes[0].workspaces.push(second);
        app.layout.hits.push(HitRegion {
            rect: Rect::new(18, 3, 7, 1),
            target: HitTarget::SpawnSpace(1),
        });

        assert_eq!(app.click(20, 3), AppAction::None);
        assert_eq!(app.focus, Focus::Spawn);
        assert_eq!(app.spawn.as_ref().unwrap().workspace_id, "workspace-b");
        assert_eq!(app.spawn.as_ref().unwrap().provider, Provider::Kimi);

        app.reduce(UiKey::Up);
        assert_eq!(app.spawn.as_ref().unwrap().workspace_id, "workspace-a");
        app.reduce(UiKey::Down);
        assert!(matches!(
            app.reduce(UiKey::Enter),
            AppAction::Spawn { workspace_id, provider: Provider::Kimi, .. }
                if workspace_id == "workspace-b"
        ));
    }

    #[test]
    fn files_and_git_modes_request_selected_workspace_without_touching_pty() {
        let mut app = fixture();
        app.focus = Focus::Spaces;

        assert!(matches!(
            app.reduce(UiKey::Char('f')),
            AppAction::InspectWorkspace { node_id, workspace_id }
                if node_id == "node-a" && workspace_id == "workspace-a"
        ));
        assert_eq!(app.sidebar_mode, SidebarMode::Files);
        assert!(app.workspace_inspection_pending());
        assert!(matches!(
            app.reduce(UiKey::Char('g')),
            AppAction::InspectWorkspace { node_id, workspace_id }
                if node_id == "node-a" && workspace_id == "workspace-a"
        ));
        assert_eq!(app.sidebar_mode, SidebarMode::Git);
        assert_eq!(app.focus, Focus::Spaces);
    }

    #[test]
    fn mouse_wheel_scrolls_each_hovered_sidebar_list_independently() {
        let mut app = fixture();
        app.apply_workspace_inspection("node-a".to_owned(), workspace_inspection());
        let mut second_workspace = app.nodes[0].workspaces[0].clone();
        second_workspace.workspace_id = "workspace-b".to_owned();
        second_workspace.label = "other".to_owned();
        second_workspace.sessions.clear();
        app.nodes[0].workspaces.push(second_workspace);
        let mut second_agent = app.nodes[0].workspaces[0].sessions[0].clone();
        second_agent.address.instance_id = 8;
        second_agent.provider = Provider::Claude;
        app.nodes[0].workspaces[0].sessions.push(second_agent);
        app.layout.spaces = Rect::new(0, 0, 25, 4);
        app.layout.agents = Rect::new(0, 4, 25, 5);

        app.sidebar_mode = SidebarMode::Files;
        assert_eq!(app.scroll(2, 2, false), AppAction::None);
        assert_eq!(app.files_scroll, 2);
        assert_eq!((app.files_cursor, app.git_cursor), (0, 0));

        app.sidebar_mode = SidebarMode::Git;
        assert_eq!(app.scroll(2, 2, false), AppAction::None);
        assert_eq!(app.git_scroll, 1);
        assert_eq!((app.files_cursor, app.git_cursor), (0, 0));

        app.roster_mode = RosterMode::Agents;
        assert_eq!(app.scroll(2, 6, false), AppAction::None);
        assert_eq!(app.agents_scroll, 1);
        assert_eq!((app.selected_agent, app.selected_space), (0, 0));

        app.roster_mode = RosterMode::Workspaces;
        assert_eq!(app.scroll(2, 6, false), AppAction::None);
        assert_eq!(app.workspaces_scroll, 1);
        assert_eq!((app.selected_agent, app.selected_space), (0, 0));
    }

    #[test]
    fn clicking_directory_collapses_and_expands_only_its_descendants() {
        let mut app = fixture();
        app.apply_workspace_inspection("node-a".to_owned(), workspace_inspection());
        app.layout.hits.push(HitRegion {
            rect: Rect::new(0, 1, 25, 1),
            target: HitTarget::SidebarItem(0),
        });

        assert_eq!(app.visible_workspace_entry_indices(), vec![0, 1, 2, 3]);
        assert_eq!(app.click(2, 1), AppAction::None);
        assert!(app.directory_is_collapsed("src"));
        assert_eq!(app.visible_workspace_entry_indices(), vec![0, 3]);
        assert_eq!(app.git_cursor, 0);

        assert_eq!(app.click(2, 1), AppAction::None);
        assert!(!app.directory_is_collapsed("src"));
        assert_eq!(app.visible_workspace_entry_indices(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn control_modal_reuses_file_git_cursors_and_directory_actions() {
        let mut app = fixture();
        app.apply_workspace_inspection("node-a".to_owned(), workspace_inspection());
        app.focus = Focus::Settings;
        app.menu_placement = MenuPlacement::Modal;
        app.control_section = ControlSection::Files;
        app.sidebar_mode = SidebarMode::Files;
        app.layout.control_content = Rect::new(10, 4, 60, 3);

        assert_eq!(app.scroll(20, 5, false), AppAction::None);
        assert_eq!(app.files_scroll, 2);
        assert_eq!(app.files_cursor, 0);
        assert_eq!(app.focus, Focus::Settings);
        app.layout.hits.push(HitRegion {
            rect: Rect::new(12, 6, 40, 1),
            target: HitTarget::SidebarItem(0),
        });
        assert_eq!(app.click(20, 6), AppAction::None);
        assert!(app.directory_is_collapsed("src"));
        assert_eq!(app.focus, Focus::Settings);

        assert!(matches!(
            app.select_control_section(ControlSection::Git),
            AppAction::InspectWorkspace { .. }
        ));
        assert_eq!(app.scroll(20, 5, false), AppAction::None);
        assert_eq!(app.git_scroll, 1);
        assert_eq!(app.git_cursor, 0);
        assert_eq!(app.files_cursor, 0);
        assert_eq!(app.focus, Focus::Settings);
    }

    #[test]
    fn add_space_modal_collects_fields_without_optimistic_insert() {
        let mut app = fixture();
        app.focus = Focus::Agents;
        app.roster_mode = RosterMode::Workspaces;
        assert_eq!(app.reduce(UiKey::Char('+')), AppAction::None);
        for ch in "scratch".chars() { app.reduce(UiKey::Char(ch)); }
        app.reduce(UiKey::Tab);
        assert_eq!(app.paste(r"C:\tmp\scratch".to_owned()), AppAction::None);
        let action = app.reduce(UiKey::Enter);
        assert!(matches!(action, AppAction::RegisterWorkspace { node_id, workspace_id, root } if node_id == "node-a" && workspace_id == "scratch" && root == r"C:\tmp\scratch"));
        assert_eq!(app.space_rows().len(), 1);
    }

    #[test]
    fn create_worktree_modal_is_prefilled_validated_and_projects_exact_action() {
        let mut app = fixture();
        assert_eq!(app.begin_create_worktree(), AppAction::None);
        let dialog = app.create_worktree.as_ref().unwrap();
        assert_eq!(dialog.source_workspace_id, "workspace-a");
        assert_eq!(dialog.workspace_id, "workspace-a-worktree");
        assert!(is_absolute_root(&dialog.target_root));
        assert_eq!(dialog.branch, "workspace-a-worktree");

        app.create_worktree.as_mut().unwrap().workspace_id = "Bad.Id".to_owned();
        assert_eq!(app.reduce(UiKey::Enter), AppAction::None);
        assert!(app.notice.as_deref().unwrap().contains("invalid workspace ID"));
        assert_eq!(app.focus, Focus::CreateWorktree);

        let dialog = app.create_worktree.as_mut().unwrap();
        dialog.workspace_id = "feature-a".to_owned();
        dialog.target_root = r"C:\work\feature-a".to_owned();
        dialog.branch = "feature/a".to_owned();
        dialog.base = "origin/main".to_owned();
        assert!(matches!(
            app.reduce(UiKey::Enter),
            AppAction::CreateWorktree {
                node_id,
                source_workspace_id,
                workspace_id,
                target_root,
                branch,
                base: Some(base),
            } if node_id == "node-a"
                && source_workspace_id == "workspace-a"
                && workspace_id == "feature-a"
                && target_root == r"C:\work\feature-a"
                && branch == "feature/a"
                && base == "origin/main"
        ));
    }

    #[test]
    fn worktree_rows_open_registered_prefill_unregistered_and_confirm_safe_remove() {
        let mut app = fixture();
        let mut inspection = workspace_inspection();
        inspection.git.worktrees = vec![
            worktree(r"C:\work\registered", "feature/registered", Some("registered")),
            worktree(r"C:\work\unregistered", "feature/unregistered", None),
        ];
        app.apply_workspace_inspection("node-a".to_owned(), inspection);

        assert_eq!(app.activate_worktree(1), AppAction::None);
        let add = app.add_space.as_ref().unwrap();
        assert_eq!(add.root, r"C:\work\unregistered");
        assert_eq!(add.workspace_id, "feature-unregistered");

        app.focus = Focus::Spaces;
        app.add_space = None;
        assert_eq!(app.begin_remove_worktree(1), AppAction::None);
        assert!(matches!(
            app.reduce(UiKey::Enter),
            AppAction::RemoveWorktree {
                source_workspace_id,
                target_root,
                ..
            } if source_workspace_id == "workspace-a" && target_root == r"C:\work\unregistered"
        ));

        let inspection = app.workspace_inspections.get_mut(&("node-a".to_owned(), "workspace-a".to_owned())).unwrap();
        inspection.git.worktrees[0].is_main = true;
        assert_eq!(app.begin_remove_worktree(0), AppAction::None);
        assert!(app.remove_worktree.is_none());
        assert!(app.notice.as_deref().unwrap().contains("cannot be removed"));
    }

    #[test]
    fn registered_space_selection_waits_for_authoritative_snapshot() {
        let mut app = fixture();
        app.request_space_selection("node-a".to_owned(), "scratch".to_owned());
        assert_eq!(app.selected_space, 0);
        let mut node = app.nodes[0].clone();
        node.workspaces.push(WorkspaceView {
            workspace_id: "scratch".to_owned(),
            label: "scratch".to_owned(),
            canonical_root: r"C:\tmp\scratch".to_owned(),
            providers: Vec::new(),
            sessions: Vec::new(),
        });
        app.upsert_node(node);
        assert_eq!(app.selected_space, 1);
        assert_eq!(app.focus, Focus::Spaces);
    }

    #[test]
    fn registered_space_selection_does_not_steal_modal_focus() {
        let mut app = fixture();
        let mut node = app.nodes[0].clone();
        node.workspaces.push(WorkspaceView {
            workspace_id: "scratch".to_owned(),
            label: "scratch".to_owned(),
            canonical_root: r"C:\tmp\scratch".to_owned(),
            providers: Vec::new(),
            sessions: Vec::new(),
        });
        app.upsert_node(node);
        app.focus = Focus::Agents;
        app.roster_mode = RosterMode::Workspaces;
        app.reduce(UiKey::Char('+'));
        assert_eq!(app.focus, Focus::AddSpace);

        app.request_space_selection("node-a".to_owned(), "scratch".to_owned());

        assert_eq!(app.selected_space, 1);
        assert_eq!(app.focus, Focus::AddSpace);
        assert!(app.add_space.is_some());
    }

    #[test]
    fn disconnected_node_retains_authoritative_inventory_and_terminal_state() {
        let mut app = fixture();
        let address = app.tabs[0].address.clone();

        app.set_node_connection(
            "node-a",
            r"\\.\pipe\node-a",
            ConnectionState::Disconnected("lost".to_owned()),
        );

        assert_eq!(app.nodes.len(), 1);
        assert_eq!(app.tabs, vec![SessionTab { address: address.clone() }]);
        assert_eq!(app.focused_session().unwrap().terminal_formatted, b"codex");
        assert!(matches!(
            app.nodes[0].connection,
            ConnectionState::Disconnected(_)
        ));
    }

    #[test]
    fn authoritative_topology_removal_purges_node_owned_state_and_keeps_survivor_usable() {
        let mut app = fixture();
        let removed_address = app.tabs[0].address.clone();
        add_managed_record(
            &mut app,
            "removed-record",
            "removed session",
            ManagedSessionState::Live,
            Some(removed_address.clone()),
            true,
        );

        let mut surviving_node = app.nodes[0].clone();
        surviving_node.node_id = "node-b".to_owned();
        surviving_node.endpoint = r"\\.\pipe\node-b".to_owned();
        surviving_node.workspaces[0].workspace_id = "workspace-b".to_owned();
        surviving_node.workspaces[0].label = "survivor".to_owned();
        surviving_node.workspaces[0].sessions[0].address.node_id = "node-b".to_owned();
        surviving_node.workspaces[0].sessions[0].address.workspace_id = "workspace-b".to_owned();
        surviving_node.workspaces[0].sessions[0].terminal_formatted = b"surviving terminal".to_vec();
        surviving_node.session_records.clear();
        let surviving_address = surviving_node.workspaces[0].sessions[0].address.clone();
        app.nodes.push(surviving_node);
        app.tabs.push(SessionTab { address: surviving_address.clone() });
        app.grid.panes = vec![
            GridPane { address: removed_address.clone() },
            GridPane { address: surviving_address.clone() },
        ];
        app.grid.focused = 0;
        app.pending_open = vec![removed_address.clone(), surviving_address.clone()];
        app.pending_space = Some(("node-a".to_owned(), "workspace-a".to_owned()));
        app.terminal_scroll_offsets.insert(removed_address.clone(), 7);
        app.terminal_scroll_offsets.insert(surviving_address.clone(), 3);
        app.apply_workspace_inspection("node-a".to_owned(), workspace_inspection());
        let mut surviving_inspection = workspace_inspection();
        surviving_inspection.workspace_id = WorkspaceId::new("workspace-b").unwrap();
        app.apply_workspace_inspection("node-b".to_owned(), surviving_inspection);
        app.inspection_pending = Some(("node-a".to_owned(), "workspace-a".to_owned()));
        app.collapsed_directories.insert((
            "node-a".to_owned(),
            "workspace-a".to_owned(),
            "src".to_owned(),
        ));
        app.collapsed_directories.insert((
            "node-b".to_owned(),
            "workspace-b".to_owned(),
            "src".to_owned(),
        ));
        app.files_cursor = 2;
        app.git_cursor = 2;
        app.files_scroll = 2;
        app.git_scroll = 2;
        app.agents_scroll = 2;
        app.workspaces_scroll = 2;
        app.selected_space = 0;
        app.selected_agent = app
            .agent_rows()
            .iter()
            .position(|row| app.agent_row_active_address(row).as_ref() == Some(&removed_address))
            .unwrap();
        app.selected_tab = 0;
        app.surface_mode = SurfaceMode::Tab;
        app.focus = Focus::Viewport;

        app.remove_topology_node("node-a");

        assert_eq!(app.nodes.len(), 1);
        assert_eq!(app.nodes[0].node_id, "node-b");
        assert_eq!(app.nodes[0].workspaces[0].sessions[0].terminal_formatted, b"surviving terminal");
        assert!(app.find_session(&removed_address).is_none());
        assert_eq!(app.tabs, vec![SessionTab { address: surviving_address.clone() }]);
        assert_eq!(app.grid.panes, vec![GridPane { address: surviving_address.clone() }]);
        assert_eq!(app.pending_open, vec![surviving_address.clone()]);
        assert!(app.pending_space.is_none());
        assert_eq!(app.terminal_scroll_offsets.len(), 1);
        assert_eq!(app.terminal_scroll_offset(&surviving_address), 3);
        assert!(!app.workspace_inspections.contains_key(&(
            "node-a".to_owned(),
            "workspace-a".to_owned()
        )));
        assert!(app.workspace_inspections.contains_key(&(
            "node-b".to_owned(),
            "workspace-b".to_owned()
        )));
        assert!(app.inspection_pending.is_none());
        assert!(!app
            .collapsed_directories
            .iter()
            .any(|(node_id, _, _)| node_id == "node-a"));
        assert!(app
            .collapsed_directories
            .iter()
            .any(|(node_id, _, _)| node_id == "node-b"));
        assert_eq!((app.files_cursor, app.git_cursor), (0, 0));
        assert_eq!(
            (app.files_scroll, app.git_scroll, app.agents_scroll, app.workspaces_scroll),
            (0, 0, 0, 0)
        );
        assert_eq!(app.selected_workspace_route(), Some(("node-b".to_owned(), "workspace-b".to_owned())));
        assert_eq!(app.selected_agent_session().unwrap().address, surviving_address);
        assert_eq!((app.selected_tab, app.grid.focused), (0, 0));
        assert_eq!((app.surface_mode, app.focus), (SurfaceMode::Tab, Focus::Tabs));

        assert_eq!(app.reduce(UiKey::Enter), AppAction::None);
        assert!(matches!(
            app.reduce(UiKey::Char('x')),
            AppAction::Input { address, text } if address == surviving_address && text == "x"
        ));
    }

    #[test]
    fn disconnected_node_rejects_operator_mutations_locally() {
        let mut app = fixture();
        app.nodes[0].connection = ConnectionState::Disconnected("lost".to_owned());

        app.focus = Focus::Spaces;
        assert_eq!(app.reduce(UiKey::Ctrl('n')), AppAction::None);
        assert_eq!(app.reduce(UiKey::Delete), AppAction::None);
        app.focus = Focus::Agents;
        assert_eq!(app.reduce(UiKey::Delete), AppAction::None);
        assert_eq!(app.reduce(UiKey::Char('r')), AppAction::None);
        app.open_selected_agent();
        assert_eq!(app.focus, Focus::Agents);
    }

    #[test]
    fn authoritative_session_removal_prunes_selected_tab_fail_closed() {
        let mut app = fixture();
        app.focus = Focus::Viewport;
        let mut node = app.nodes[0].clone();
        node.workspaces[0].sessions.clear();

        app.upsert_node(node);

        assert!(app.tabs.is_empty());
        assert_eq!(app.focus, Focus::Agents);
        assert!(app.notice.as_deref().unwrap().contains("input target cleared"));
    }

    #[test]
    fn style_cycles_only_from_operator_focus() {
        let mut app = fixture();
        assert_eq!(app.color_mode, PtyColorMode::Inherited);
        app.focus = Focus::Tabs;
        app.reduce(UiKey::Ctrl('t'));
        assert_eq!(app.color_mode, PtyColorMode::GateOverride);
        app.focus = Focus::Viewport;
        assert!(matches!(app.reduce(UiKey::Ctrl('t')), AppAction::TerminalControl { .. }));
        assert_eq!(app.color_mode, PtyColorMode::GateOverride);
    }

    #[test]
    fn control_modal_cycles_sections_style_and_menu_placement() {
        let mut app = fixture();
        app.focus = Focus::Viewport;
        app.layout.hits.push(HitRegion {
            rect: Rect::new(90, 0, 10, 1),
            target: HitTarget::Settings,
        });

        assert_eq!(app.click(95, 0), AppAction::None);
        assert_eq!(app.focus, Focus::Settings);
        assert_eq!(app.control_section, ControlSection::Files);
        assert_eq!(app.reduce(UiKey::Char('s')), AppAction::None);
        assert_eq!(app.color_mode, PtyColorMode::GateOverride);
        assert_eq!(app.reduce(UiKey::Char('m')), AppAction::None);
        assert_eq!(app.menu_placement, MenuPlacement::Modal);
        assert!(matches!(app.reduce(UiKey::Right), AppAction::InspectWorkspace { .. }));
        assert_eq!(app.control_section, ControlSection::Git);
        assert_eq!(app.reduce(UiKey::Right), AppAction::None);
        assert_eq!(app.control_section, ControlSection::Agents);
        assert_eq!(app.reduce(UiKey::Right), AppAction::None);
        assert_eq!(app.control_section, ControlSection::Workspaces);
        assert_eq!(app.reduce(UiKey::Right), AppAction::None);
        assert_eq!(app.control_section, ControlSection::Settings);
        assert!(matches!(app.reduce(UiKey::Right), AppAction::InspectWorkspace { .. }));
        assert_eq!(app.control_section, ControlSection::Files);
        assert_eq!(app.reduce(UiKey::Escape), AppAction::None);
        assert_eq!(app.focus, Focus::Viewport);
        assert_eq!(app.reduce(UiKey::OperatorEscape), AppAction::None);
        assert_eq!(app.focus, Focus::Tabs);
        assert_eq!(app.reduce(UiKey::Tab), AppAction::None);
        assert_eq!(app.focus, Focus::Viewport);
    }

    #[test]
    fn split_to_activity_preserves_operational_section_and_normalizes_settings() {
        let mut app = fixture();
        assert!(matches!(
            app.select_sidebar_mode(SidebarMode::Git),
            AppAction::InspectWorkspace { .. }
        ));
        app.begin_settings();
        app.toggle_sidebar_presentation();
        assert_eq!(app.sidebar_presentation, SidebarPresentation::Activity);
        assert_eq!(app.control_section, ControlSection::Git);
        assert_eq!(app.sidebar_mode, SidebarMode::Git);

        app.sidebar_presentation = SidebarPresentation::Split;
        app.control_section = ControlSection::Settings;
        app.toggle_sidebar_presentation();
        assert_eq!(app.control_section, ControlSection::Files);
        assert_eq!(app.sidebar_mode, SidebarMode::Files);
    }

    #[test]
    fn control_modal_click_targets_win_over_the_underlying_viewport() {
        let mut app = fixture();
        app.focus = Focus::Settings;
        app.settings_return_focus = Focus::Viewport;
        app.roster_mode = RosterMode::Workspaces;
        app.layout.hits.push(HitRegion {
            rect: Rect::new(0, 0, 100, 24),
            target: HitTarget::Viewport,
        });
        app.layout.hits.push(HitRegion {
            rect: Rect::new(30, 10, 40, 1),
            target: HitTarget::SettingsPlacement,
        });

        assert_eq!(app.click(40, 10), AppAction::None);
        assert_eq!(app.menu_placement, MenuPlacement::Modal);
        assert_eq!(app.focus, Focus::Settings);
        app.layout.hits.push(HitRegion {
            rect: Rect::new(30, 11, 40, 1),
            target: HitTarget::ControlSection(ControlSection::Agents),
        });
        assert_eq!(app.click(40, 11), AppAction::None);
        assert_eq!(app.control_section, ControlSection::Agents);
        assert_eq!(app.roster_mode, RosterMode::Workspaces);
    }

    #[test]
    fn modal_and_sidebar_splitters_drag_with_bounded_geometry() {
        let mut app = fixture();
        app.terminal_cols = 120;
        app.terminal_rows = 40;
        app.focus = Focus::Settings;
        app.layout.control_modal = Rect::new(20, 5, 60, 20);
        app.layout.hits.push(HitRegion {
            rect: Rect::new(20, 5, 60, 1),
            target: HitTarget::ControlDrag,
        });

        assert_eq!(app.click(30, 5), AppAction::None);
        assert!(matches!(app.drag_state, Some(DragState::ControlModal { .. })));
        assert_eq!(app.drag(42, 11), AppAction::None);
        assert_eq!(app.control_modal_position, Some((32, 11)));
        app.end_drag();
        assert!(app.drag_state.is_none());

        app.focus = Focus::Tabs;
        app.layout.hits.clear();
        app.layout.hits.push(HitRegion {
            rect: Rect::new(25, 0, 1, 40),
            target: HitTarget::SidebarWidthDrag,
        });
        assert_eq!(app.click(25, 10), AppAction::None);
        assert_eq!(app.drag(39, 10), AppAction::None);
        assert_eq!(app.sidebar_width, 40);
        app.end_drag();

        app.layout.hits.clear();
        app.layout.hits.push(HitRegion {
            rect: Rect::new(0, 20, 39, 1),
            target: HitTarget::SidebarSplitDrag,
        });
        assert_eq!(app.click(10, 20), AppAction::None);
        assert_eq!(app.drag(10, 28), AppAction::None);
        assert_eq!(app.sidebar_split_percent, 70);
        app.end_drag();
    }

    #[test]
    fn inspection_autorefresh_is_active_only_for_visible_file_or_git_surfaces() {
        let mut app = fixture();
        assert!(app.workspace_inspection_visible());
        app.menu_placement = MenuPlacement::Modal;
        app.focus = Focus::Viewport;
        assert!(!app.workspace_inspection_visible());
        app.focus = Focus::Settings;
        app.control_section = ControlSection::Agents;
        assert!(!app.workspace_inspection_visible());
        app.control_section = ControlSection::Files;
        assert!(app.workspace_inspection_visible());
        app.control_section = ControlSection::Git;
        assert!(app.workspace_inspection_visible());
        app.control_section = ControlSection::Settings;
        assert!(!app.workspace_inspection_visible());
    }

    #[test]
    fn spawn_accepted_autoopens_after_snapshot_materializes() {
        let mut app = fixture();
        let pending = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 9,
            generation: 1,
        };
        app.request_open(pending.clone());
        assert_eq!(app.tabs.len(), 1);
        let mut node = app.nodes[0].clone();
        let mut session = node.workspaces[0].sessions[0].clone();
        session.address = pending;
        node.workspaces[0].sessions.push(session);
        app.upsert_node(node);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.focus, Focus::Viewport);
    }

    #[test]
    fn snapshot_reorder_and_generation_rebind_preserve_agent_action_target() {
        let mut app = fixture();
        let mut other = app.nodes[0].workspaces[0].sessions[0].clone();
        other.address.instance_id = 8;
        other.provider = Provider::Claude;
        app.nodes[0].workspaces[0].sessions.push(other);
        let original = app.nodes[0].workspaces[0].sessions[0].address.clone();
        app.selected_agent = app
            .agent_addresses()
            .iter()
            .position(|address| *address == original)
            .unwrap();

        let mut replacement = app.nodes[0].clone();
        replacement.workspaces[0].sessions.reverse();
        let selected = replacement.workspaces[0]
            .sessions
            .iter_mut()
            .find(|session| session.address.instance_id == original.instance_id)
            .unwrap();
        selected.address.generation = 3;
        selected.running = false;
        selected.stoppable = false;
        selected.removable = true;
        selected.restartable = true;
        app.upsert_node(replacement);

        let rebound = app.selected_agent_session().unwrap().address.clone();
        assert_eq!(rebound.instance_id, original.instance_id);
        assert_eq!(rebound.generation, 3);
        app.focus = Focus::Agents;
        assert!(matches!(app.reduce(UiKey::Delete), AppAction::Remove { address } if address == rebound));
        assert!(matches!(app.reduce(UiKey::Char('r')), AppAction::Resume { address, .. } if address == rebound));
    }

    #[test]
    fn workspace_reorder_preserves_unregister_target_by_identity() {
        let mut app = fixture();
        let mut workspace = app.nodes[0].workspaces[0].clone();
        workspace.workspace_id = "workspace-b".to_owned();
        workspace.label = "other".to_owned();
        workspace.canonical_root = r"C:\work\other".to_owned();
        workspace.sessions.clear();
        app.nodes[0].workspaces.push(workspace);
        app.selected_space = 1;
        let mut replacement = app.nodes[0].clone();
        replacement.workspaces.reverse();
        app.upsert_node(replacement);
        app.focus = Focus::Agents;
        app.roster_mode = RosterMode::Workspaces;
        assert!(matches!(
            app.reduce(UiKey::Delete),
            AppAction::UnregisterWorkspace { node_id, workspace_id }
                if node_id == "node-a" && workspace_id == "workspace-b"
        ));
    }

    #[test]
    fn registered_and_stopping_sessions_are_stopped_never_removed() {
        let mut app = fixture();
        app.focus = Focus::Agents;
        for status in ["registered", "stopping"] {
            let session = &mut app.nodes[0].workspaces[0].sessions[0];
            session.status = status.to_owned();
            session.running = false;
            session.stoppable = true;
            session.removable = false;
            session.restartable = false;
            assert!(matches!(app.reduce(UiKey::Delete), AppAction::Stop { force: false, .. }));
            assert!(!matches!(app.reduce(UiKey::Delete), AppAction::Remove { .. }));
        }
    }

    #[test]
    fn add_space_can_route_to_connected_node_without_existing_spaces() {
        let mut app = fixture();
        app.nodes.push(NodeView {
            node_id: "node-b".to_owned(),
            endpoint: r"\\.\pipe\node-b".to_owned(),
            connection: ConnectionState::Connected,
            controller_owned: true,
            event_sequence: 1,
            session_records: Vec::new(),
            workspaces: Vec::new(),
        });
        app.focus = Focus::Agents;
        app.roster_mode = RosterMode::Workspaces;
        app.reduce(UiKey::Char('+'));
        assert_eq!(app.add_space.as_ref().unwrap().node_id, "node-a");
        app.reduce(UiKey::Down);
        assert_eq!(app.add_space.as_ref().unwrap().node_id, "node-b");
        for ch in "scratch".chars() { app.reduce(UiKey::Char(ch)); }
        app.reduce(UiKey::Tab);
        app.paste(r"C:\tmp\scratch".to_owned());
        assert!(matches!(
            app.reduce(UiKey::Enter),
            AppAction::RegisterWorkspace { node_id, workspace_id, .. }
                if node_id == "node-b" && workspace_id == "scratch"
        ));
    }

    #[test]
    fn add_space_fields_reject_invalid_or_oversized_input_atomically() {
        let mut app = fixture();
        app.focus = Focus::Agents;
        app.roster_mode = RosterMode::Workspaces;
        app.reduce(UiKey::Char('+'));
        app.reduce(UiKey::Char('A'));
        app.reduce(UiKey::Char('\n'));
        assert!(app.add_space.as_ref().unwrap().workspace_id.is_empty());
        app.paste("Bad-ID".to_owned());
        assert!(app.add_space.as_ref().unwrap().workspace_id.is_empty());
        for _ in 0..MAX_NODE_IDENTIFIER_BYTES {
            app.reduce(UiKey::Char('a'));
        }
        app.reduce(UiKey::Char('b'));
        assert_eq!(app.add_space.as_ref().unwrap().workspace_id.len(), MAX_NODE_IDENTIFIER_BYTES);
        app.reduce(UiKey::Tab);
        app.paste("x".repeat(MAX_WORKSPACE_ROOT_BYTES + 1));
        assert!(app.add_space.as_ref().unwrap().root.is_empty());
        app.paste("C:\\tmp\ninvalid".to_owned());
        assert!(app.add_space.as_ref().unwrap().root.is_empty());
    }

    #[test]
    fn session_chip_drag_moves_between_tab_and_grid_without_duplication() {
        let mut app = fixture();
        let address = app.tabs[0].address.clone();
        app.layout.hits.push(HitRegion {
            rect: Rect::new(0, 0, 8, 1),
            target: HitTarget::Tab(0),
        });
        app.layout.hits.push(HitRegion {
            rect: Rect::new(40, 2, 20, 8),
            target: HitTarget::GridDropSlot(2),
        });

        assert_eq!(app.click(2, 0), AppAction::None);
        assert_eq!(app.drag(45, 4), AppAction::None);
        assert_eq!(app.drop_at(45, 4), AppAction::None);
        assert!(app.tabs.is_empty());
        assert_eq!(app.grid.panes, vec![GridPane { address: address.clone() }]);
        assert_eq!(app.surface_mode, SurfaceMode::Grid);

        app.layout.hits.clear();
        app.layout.hits.push(HitRegion {
            rect: Rect::new(40, 2, 20, 1),
            target: HitTarget::GridPaneHeader(0),
        });
        app.layout.hits.push(HitRegion {
            rect: Rect::new(0, 0, 30, 1),
            target: HitTarget::TabDrop,
        });
        assert_eq!(app.click(45, 2), AppAction::None);
        assert_eq!(app.drag(12, 0), AppAction::None);
        assert_eq!(app.drop_at(12, 0), AppAction::None);
        assert!(app.grid.panes.is_empty());
        assert_eq!(app.tabs, vec![SessionTab { address }]);
        assert_eq!(app.surface_mode, SurfaceMode::Tab);
    }

    #[test]
    fn agent_chip_activates_only_on_release_and_drag_preserves_the_active_surface() {
        let mut click_app = fixture();
        let first = click_app.tabs[0].address.clone();
        let second = add_session(&mut click_app, 8, Provider::Claude);
        let addresses = click_app.agent_addresses();
        let first_index = addresses.iter().position(|address| address == &first).unwrap();
        let second_index = addresses.iter().position(|address| address == &second).unwrap();
        click_app.selected_agent = first_index;
        click_app.layout.hits.push(HitRegion {
            rect: Rect::new(0, 5, 24, 2),
            target: HitTarget::Agent(second_index),
        });

        assert_eq!(click_app.click(4, 5), AppAction::None);
        assert_eq!(click_app.focused_address(), Some(&first));
        assert_eq!(click_app.selected_agent, first_index);
        assert_eq!(click_app.drop_at(4, 5), AppAction::None);
        assert_eq!(click_app.focused_address(), Some(&second));
        assert_eq!(click_app.selected_agent, second_index);

        let mut drag_app = fixture();
        let first = drag_app.tabs[0].address.clone();
        let second = add_session(&mut drag_app, 8, Provider::Claude);
        let second_index = drag_app
            .agent_addresses()
            .iter()
            .position(|address| address == &second)
            .unwrap();
        drag_app.layout.hits.push(HitRegion {
            rect: Rect::new(0, 5, 24, 2),
            target: HitTarget::Agent(second_index),
        });
        drag_app.layout.grid_drop = Rect::new(30, 2, 30, 12);

        assert_eq!(drag_app.click(4, 5), AppAction::None);
        assert_eq!(drag_app.drag(35, 6), AppAction::None);
        assert_eq!(drag_app.focused_address(), Some(&first));
        assert_eq!(drag_app.surface_mode, SurfaceMode::Tab);
        assert_eq!(drag_app.drop_at(35, 6), AppAction::None);
        assert_eq!(drag_app.grid.panes, vec![GridPane { address: second }]);
        assert_eq!(drag_app.tabs, vec![SessionTab { address: first }]);
    }

    #[test]
    fn grid_reorders_duplicates_and_rejects_a_fifth_unique_pty() {
        let mut app = fixture();
        let first = app.tabs[0].address.clone();
        let second = add_session(&mut app, 8, Provider::Claude);
        let third = add_session(&mut app, 9, Provider::Kimi);
        let fourth = add_session(&mut app, 10, Provider::Codex);
        let fifth = add_session(&mut app, 11, Provider::Claude);

        for address in [&first, &second, &third, &fourth] {
            assert!(app.move_address_to_grid(address.clone(), None));
        }
        assert_eq!(app.grid.panes.len(), MAX_GRID_PANES);
        assert!(app.tabs.is_empty());
        assert!(app.move_address_to_grid(first.clone(), Some(3)));
        assert_eq!(app.grid.panes.len(), MAX_GRID_PANES);
        assert_eq!(app.grid.panes[3].address, first);
        assert!(!app.move_address_to_grid(fifth, None));
        assert_eq!(app.grid.panes.len(), MAX_GRID_PANES);
        assert!(app.notice.as_deref().unwrap().contains("grid is full"));
    }

    #[test]
    fn focused_grid_pane_routes_input_paste_and_terminal_geometry() {
        let mut app = fixture();
        let first = app.tabs[0].address.clone();
        let second = add_session(&mut app, 8, Provider::Claude);
        assert!(app.move_address_to_grid(first.clone(), None));
        assert!(app.move_address_to_grid(second.clone(), None));
        app.layout.grid_panes = vec![
            GridPaneLayout {
                pane_index: 0,
                frame: Rect::new(0, 1, 40, 20),
                header: Rect::new(0, 1, 40, 1),
                viewport: Rect::new(0, 2, 40, 19),
            },
            GridPaneLayout {
                pane_index: 1,
                frame: Rect::new(40, 1, 60, 20),
                header: Rect::new(40, 1, 60, 1),
                viewport: Rect::new(40, 2, 60, 19),
            },
        ];
        app.focus_grid_pane(1);

        assert_eq!(app.focused_session().unwrap().address, second);
        assert_eq!(app.focused_terminal_rect(), Rect::new(40, 2, 60, 19));
        assert!(matches!(
            app.reduce(UiKey::Char('x')),
            AppAction::Input { address, text } if address == second && text == "x"
        ));
        assert!(matches!(
            app.paste("paste".to_owned()),
            AppAction::Paste { address, text } if address == second && text == "paste"
        ));
        assert_eq!(
            app.desired_terminal_sizes(),
            vec![(first, 19, 40), (second, 19, 60)]
        );
    }

    #[test]
    fn grid_rebinds_generation_and_keeps_nearest_pane_when_focused_one_disappears() {
        let mut app = fixture();
        let first = app.tabs[0].address.clone();
        let second = add_session(&mut app, 8, Provider::Claude);
        assert!(app.move_address_to_grid(first.clone(), None));
        assert!(app.move_address_to_grid(second.clone(), None));
        app.focus_grid_pane(0);

        let mut replacement = app.nodes[0].clone();
        replacement.workspaces[0].sessions[0].address.generation = 3;
        app.upsert_node(replacement.clone());
        assert_eq!(app.grid.panes[0].address.generation, 3);
        assert!(matches!(
            app.reduce(UiKey::Char('x')),
            AppAction::Input { address, .. } if address.generation == 3
        ));

        replacement.workspaces[0]
            .sessions
            .retain(|session| session.address.instance_id == second.instance_id);
        app.upsert_node(replacement.clone());
        assert_eq!(app.surface_mode, SurfaceMode::Grid);
        assert_eq!(app.focus, Focus::Viewport);
        assert_eq!(app.grid.panes.len(), 1);
        assert_eq!(app.focused_address(), Some(&second));
        assert!(app.notice.as_deref().unwrap().contains("nearest pane focused"));

        replacement.workspaces[0].sessions.clear();
        app.upsert_node(replacement);
        assert!(app.grid.panes.is_empty());
        assert_eq!(app.surface_mode, SurfaceMode::Tab);
        assert_eq!(app.focus, Focus::Agents);
        assert!(app.notice.as_deref().unwrap().contains("input target cleared"));
    }

    #[test]
    fn grid_dividers_clamp_to_minimum_adjacent_shares() {
        let mut app = fixture();
        app.layout.viewport = Rect::new(0, 0, 100, 40);
        app.grid.preset = GridPreset::Columns;
        app.layout.hits.push(HitRegion {
            rect: Rect::new(25, 0, 1, 40),
            target: HitTarget::GridDivider(GridAxisKind::Columns, 0),
        });

        app.click(25, 10);
        app.drag(99, 10);
        assert_eq!(app.grid.column_cuts[0], 4_000);
        app.end_drag();
        app.click(25, 10);
        app.drag(0, 10);
        assert_eq!(app.grid.column_cuts[0], 1_000);
        app.end_drag();

        app.grid.preset = GridPreset::Quad;
        app.layout.hits.clear();
        app.layout.hits.push(HitRegion {
            rect: Rect::new(50, 0, 1, 40),
            target: HitTarget::GridDivider(GridAxisKind::Columns, 1),
        });
        app.click(50, 10);
        app.drag(0, 10);
        assert_eq!(app.grid.column_cuts[1], 1_000);
        app.end_drag();
        app.click(50, 10);
        app.drag(100, 10);
        assert_eq!(app.grid.column_cuts[1], 9_000);
    }

    #[test]
    fn grid_keyboard_navigation_respects_visual_preset_axes() {
        let mut app = fixture();
        let first = app.tabs[0].address.clone();
        let second = add_session(&mut app, 8, Provider::Claude);
        let third = add_session(&mut app, 9, Provider::Kimi);
        let fourth = add_session(&mut app, 10, Provider::Codex);
        for address in [first, second, third, fourth] {
            app.move_address_to_grid(address, None);
        }
        app.focus = Focus::Tabs;
        app.grid.preset = GridPreset::Quad;
        app.grid.focused = 0;
        app.reduce(UiKey::Right);
        assert_eq!(app.grid.focused, 1);
        app.reduce(UiKey::Down);
        assert_eq!(app.grid.focused, 3);
        app.reduce(UiKey::Left);
        assert_eq!(app.grid.focused, 2);
        app.reduce(UiKey::Up);
        assert_eq!(app.grid.focused, 0);

        app.grid.preset = GridPreset::Columns;
        app.reduce(UiKey::Down);
        assert_eq!(app.grid.focused, 0);
        app.reduce(UiKey::Right);
        assert_eq!(app.grid.focused, 1);

        app.grid.preset = GridPreset::Rows;
        app.reduce(UiKey::Right);
        assert_eq!(app.grid.focused, 1);
        app.reduce(UiKey::Down);
        assert_eq!(app.grid.focused, 2);
    }

    #[test]
    fn wheel_scrolls_terminal_history_without_sending_cursor_keys() {
        let mut app = fixture();
        let address = app.tabs[0].address.clone();
        app.nodes[0].workspaces[0].sessions[0].terminal_scrollback =
            (0..10).map(|line| format!("history-{line}").into_bytes()).collect();
        app.layout.viewport = Rect::new(26, 1, 74, 20);

        assert_eq!(app.scroll(30, 5, true), AppAction::None);
        assert_eq!(app.terminal_scroll_offset(&address), WHEEL_SCROLL_LINES);
        assert_eq!(app.scroll(30, 5, false), AppAction::None);
        assert_eq!(app.terminal_scroll_offset(&address), 0);
    }

    #[test]
    fn wheel_forwards_native_sgr_mouse_event_to_alternate_screen() {
        let mut app = fixture();
        let session = &mut app.nodes[0].workspaces[0].sessions[0];
        session.terminal_alternate_screen = true;
        session.terminal_mouse_protocol_enabled = true;
        session.terminal_mouse_protocol_encoding = TerminalMouseProtocolEncoding::Sgr;
        app.layout.viewport = Rect::new(26, 1, 74, 20);

        assert!(matches!(
            app.scroll(30, 5, true),
            AppAction::TerminalBytes { bytes, .. } if bytes == b"\x1b[<64;5;5M"
        ));
        assert!(matches!(
            app.scroll(30, 5, false),
            AppAction::TerminalBytes { bytes, .. } if bytes == b"\x1b[<65;5;5M"
        ));
    }

    #[test]
    fn wheel_prefers_observed_mouse_tracking_over_primary_screen_history() {
        let mut app = fixture();
        let session = &mut app.nodes[0].workspaces[0].sessions[0];
        session.terminal_scrollback = vec![b"history".to_vec()];
        session.terminal_mouse_protocol_enabled = true;
        session.terminal_mouse_protocol_encoding = TerminalMouseProtocolEncoding::Sgr;
        app.layout.viewport = Rect::new(26, 1, 74, 20);

        assert!(matches!(
            app.scroll(30, 5, true),
            AppAction::TerminalBytes { bytes, .. } if bytes == b"\x1b[<64;5;5M"
        ));
        assert!(app.terminal_scroll_offsets.is_empty());
    }

    #[test]
    fn stopped_terminal_with_retained_mouse_mode_scrolls_local_history() {
        let mut app = fixture();
        let address = app.tabs[0].address.clone();
        let session = &mut app.nodes[0].workspaces[0].sessions[0];
        session.running = false;
        session.terminal_scrollback = vec![b"history".to_vec()];
        session.terminal_mouse_protocol_enabled = true;
        session.terminal_mouse_protocol_encoding = TerminalMouseProtocolEncoding::Sgr;
        app.layout.viewport = Rect::new(26, 1, 74, 20);

        assert_eq!(app.scroll(30, 5, true), AppAction::None);
        assert_eq!(app.terminal_scroll_offset(&address), 1);
    }

    #[test]
    fn wheel_scrolls_the_hovered_grid_pane_independently() {
        let mut app = fixture();
        let first = app.tabs[0].address.clone();
        let second = add_session(&mut app, 8, Provider::Claude);
        for session in &mut app.nodes[0].workspaces[0].sessions {
            session.terminal_scrollback =
                (0..8).map(|line| format!("history-{line}").into_bytes()).collect();
        }
        app.move_address_to_grid(first.clone(), None);
        app.move_address_to_grid(second.clone(), None);
        app.surface_mode = SurfaceMode::Grid;
        app.layout.grid_panes = vec![
            GridPaneLayout {
                pane_index: 0,
                frame: Rect::new(0, 1, 50, 20),
                header: Rect::new(0, 1, 50, 1),
                viewport: Rect::new(0, 2, 50, 19),
            },
            GridPaneLayout {
                pane_index: 1,
                frame: Rect::new(51, 1, 49, 20),
                header: Rect::new(51, 1, 49, 1),
                viewport: Rect::new(51, 2, 49, 19),
            },
        ];

        assert_eq!(app.scroll(60, 5, true), AppAction::None);
        assert_eq!(app.terminal_scroll_offset(&first), 0);
        assert_eq!(app.terminal_scroll_offset(&second), WHEEL_SCROLL_LINES);
    }

    #[test]
    fn control_modal_header_wheel_scrolls_the_active_viewport() {
        let mut app = fixture();
        app.apply_workspace_inspection("node-a".to_owned(), workspace_inspection());
        app.focus = Focus::Settings;
        app.menu_placement = MenuPlacement::Modal;
        app.control_section = ControlSection::Files;
        app.layout.control_modal = Rect::new(10, 3, 60, 10);
        app.layout.control_content = Rect::new(11, 5, 58, 2);

        assert_eq!(app.scroll(20, 3, false), AppAction::None);
        assert_eq!(app.files_scroll, WHEEL_SCROLL_LINES);
        assert_eq!(app.files_cursor, 0);
    }

    #[test]
    fn modal_agent_chip_starts_the_same_drag_as_the_sidebar() {
        let mut app = fixture();
        let address = app.tabs[0].address.clone();
        app.focus = Focus::Settings;
        app.menu_placement = MenuPlacement::Modal;
        app.control_section = ControlSection::Agents;
        app.layout.hits.push(HitRegion {
            rect: Rect::new(20, 8, 24, 2),
            target: HitTarget::Agent(0),
        });

        assert_eq!(app.click(22, 8), AppAction::None);
        assert_eq!(app.focus, Focus::Settings);
        assert!(matches!(
            app.drag_state,
            Some(DragState::SessionChip {
                source: DragSource::Agent(0),
                ref address,
                start_column: 22,
                start_row: 8,
                ..
            }) if *address == app.tabs[0].address
        ));
        assert_eq!(app.tabs[0].address, address);
        assert_eq!(app.drop_at(22, 8), AppAction::None);
        assert_eq!(app.focus, Focus::Viewport);
    }

    #[test]
    fn sidebar_wheel_moves_viewport_without_moving_selection() {
        let mut app = fixture();
        app.apply_workspace_inspection("node-a".to_owned(), workspace_inspection());
        app.layout.spaces = Rect::new(0, 0, 26, 4);
        app.files_cursor = 1;

        assert_eq!(app.scroll(4, 2, false), AppAction::None);
        assert_eq!(app.files_cursor, 1);
        assert_eq!(app.files_scroll, 2);
    }

    #[test]
    fn terminal_input_returns_scrolled_view_to_live_bottom() {
        let mut app = fixture();
        let address = app.tabs[0].address.clone();
        app.focus = Focus::Viewport;
        app.terminal_scroll_offsets.insert(address, 9);

        assert!(matches!(app.reduce(UiKey::Char('x')), AppAction::Input { .. }));
        assert!(app.terminal_scroll_offsets.is_empty());
    }

    #[test]
    fn live_output_keeps_a_scrolled_terminal_anchored() {
        let mut app = fixture();
        let address = app.tabs[0].address.clone();
        app.nodes[0].workspaces[0].sessions[0].terminal_scrollback =
            ["a", "b", "c"].into_iter().map(|row| row.as_bytes().to_vec()).collect();
        app.terminal_scroll_offsets.insert(address.clone(), 1);
        let mut update = app.nodes[0].clone();
        update.workspaces[0].sessions[0].terminal_scrollback =
            ["b", "c", "d"].into_iter().map(|row| row.as_bytes().to_vec()).collect();

        app.upsert_node(update);

        assert_eq!(app.terminal_scroll_offset(&address), 2);
    }

    #[test]
    fn dormant_record_click_selects_only_on_release_and_routes_resume_rename_forget() {
        let mut app = fixture();
        let dormant = add_managed_record(
            &mut app,
            "record-dormant",
            "nightly review",
            ManagedSessionState::Dormant,
            None,
            true,
        );
        let index = app.agent_rows().iter().position(|row| row == &dormant).unwrap();
        let previous = app.selected_agent;
        app.layout.hits.push(HitRegion {
            rect: Rect::new(0, 6, 24, 2),
            target: HitTarget::Agent(index),
        });

        assert_eq!(app.click(4, 6), AppAction::None);
        assert_eq!(app.selected_agent, previous);
        assert!(matches!(app.drag_state, Some(DragState::AgentSelection { .. })));
        assert_eq!(app.drop_at(4, 6), AppAction::None);
        assert_eq!(app.selected_agent, index);
        assert_eq!(app.tabs.len(), 1);

        assert!(matches!(
            app.reduce(UiKey::Char('r')),
            AppAction::ResumeSessionRecord { ref node_id, ref record_id, .. }
                if node_id == "node-a" && record_id == "record-dormant"
        ));
        assert_eq!(app.reduce(UiKey::Char('n')), AppAction::None);
        assert_eq!(app.focus, Focus::RenameSession);
        app.rename_session.as_mut().unwrap().display_name.clear();
        app.paste("renamed session".to_owned());
        assert!(matches!(
            app.reduce(UiKey::Enter),
            AppAction::RenameSessionRecord { ref display_name, .. }
                if display_name == "renamed session"
        ));

        app.focus = Focus::Agents;
        app.selected_agent = app.agent_rows().iter().position(|row| row == &dormant).unwrap();
        assert_eq!(app.reduce(UiKey::Delete), AppAction::None);
        assert_eq!(app.focus, Focus::ForgetSession);
        assert!(matches!(
            app.reduce(UiKey::Enter),
            AppAction::ForgetSessionRecord { ref record_id, .. }
                if record_id == "record-dormant"
        ));
    }

    #[test]
    fn dormant_record_drag_does_not_activate_or_open_a_terminal() {
        let mut app = fixture();
        let dormant = add_managed_record(
            &mut app,
            "record-dormant",
            "cold session",
            ManagedSessionState::Dormant,
            None,
            true,
        );
        let index = app.agent_rows().iter().position(|row| row == &dormant).unwrap();
        let selected = app.selected_agent;
        let tabs = app.tabs.clone();
        app.layout.hits.push(HitRegion {
            rect: Rect::new(0, 6, 24, 2),
            target: HitTarget::Agent(index),
        });

        app.click(4, 6);
        app.drag(35, 10);
        app.drop_at(35, 10);

        assert_eq!(app.selected_agent, selected);
        assert_eq!(app.tabs, tabs);
    }

    #[test]
    fn dormant_record_release_tracks_identity_across_roster_reorder() {
        let mut app = fixture();
        let dormant = add_managed_record(
            &mut app,
            "record-dormant",
            "nightly review",
            ManagedSessionState::Dormant,
            None,
            true,
        );
        let original_index = app.agent_rows().iter().position(|row| row == &dormant).unwrap();
        app.layout.hits.push(HitRegion {
            rect: Rect::new(0, 6, 24, 2),
            target: HitTarget::Agent(original_index),
        });

        app.click(4, 6);
        add_managed_record(
            &mut app,
            "record-pending",
            "pending identity",
            ManagedSessionState::IdentityPending,
            None,
            false,
        );
        let reordered_index = app.agent_rows().iter().position(|row| row == &dormant).unwrap();
        assert_ne!(original_index, reordered_index);

        app.drop_at(4, 6);

        assert_eq!(app.selected_agent, reordered_index);
        assert_eq!(app.selected_managed_session().unwrap().record_id, "record-dormant");
        assert_eq!(app.tabs.len(), 1);
    }

    #[test]
    fn live_managed_display_name_is_the_terminal_surface_title() {
        let mut app = fixture();
        let address = app.tabs[0].address.clone();
        add_managed_record(
            &mut app,
            "record-live",
            "release shepherd",
            ManagedSessionState::Live,
            Some(address.clone()),
            true,
        );

        assert_eq!(app.session_title(&address).as_deref(), Some("release shepherd"));
    }

    #[test]
    fn wheel_scrolls_real_viewports_in_split_activity_and_modal_presentations() {
        let mut app = fixture();
        app.apply_workspace_inspection("node-a".to_owned(), workspace_inspection());
        for instance in 8..13 {
            add_session(&mut app, instance, Provider::Claude);
        }
        for index in 1..5 {
            let mut workspace = app.nodes[0].workspaces[0].clone();
            workspace.workspace_id = format!("workspace-{index}");
            workspace.sessions.clear();
            app.nodes[0].workspaces.push(workspace);
        }

        app.layout.spaces = Rect::new(3, 1, 24, 3);
        app.sidebar_mode = SidebarMode::Files;
        app.scroll(8, 2, false);
        assert!(app.files_scroll > 0);

        app.sidebar_presentation = SidebarPresentation::Activity;
        app.sidebar_mode = SidebarMode::Git;
        app.layout.spaces = Rect::new(3, 1, 24, 3);
        app.scroll(8, 2, false);
        assert!(app.git_scroll > 0);

        app.menu_placement = MenuPlacement::Modal;
        app.focus = Focus::Settings;
        app.control_section = ControlSection::Agents;
        app.layout.control_modal = Rect::new(10, 2, 40, 8);
        app.layout.control_content = Rect::new(11, 4, 38, 3);
        app.scroll(20, 5, false);
        assert!(app.agents_scroll > 0);

        app.control_section = ControlSection::Workspaces;
        app.scroll(20, 5, false);
        assert!(app.workspaces_scroll > 0);

        app.layout.control_content = Rect::default();
        assert_eq!(app.scroll(0, 0, false), AppAction::None);
    }
}
