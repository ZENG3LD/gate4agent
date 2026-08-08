use std::collections::BTreeMap;
use std::io::{self, stdout};
use std::time::{Duration, Instant};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
        EnableMouseCapture, Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use gate4agent_c2_client::{
    connect_local as connect_c2_local, C2ControlError, C2ControlHandle,
};
use gate4agent_c2_protocol::{
    C2ControlEvent, C2ControlEventKind, C2ManagedSessionRecord, C2NodeEvent, C2NodeResponse,
    C2NodeSnapshot, C2SessionSnapshot, C2SessionStatus, C2Topology, NodeRoute,
    NodeTransportState, C2WorkspaceInspection,
};
use gate4agent_node_protocol::{
    AgentProvider, ClientRole, ControllerState, ManagedSessionRecord, NodeEvent, NodeFailureCode,
    NodeId, NodeRequest, NodeResponse, NodeSnapshot, SessionAddress as WireSessionAddress,
    SessionKey, SessionMode, SessionRecordId, WorkspaceId, MAX_CONTROLLER_LEASE_MS,
    MAX_NODE_TEXT_BYTES,
};
use gate4agent_node_wire::{NamedPipeNodeClient, NodeClientError};
use gate4agent_types::{
    ControlEvent, ControlEventKind, ProviderActivity, SessionSnapshot, SessionStatus, TerminalSize,
    TerminalMouseProtocolEncoding, TransportKind,
};
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;
use uzor_tui::{Backend, CrosstermBackend, Screen};

use crate::app::{
    host_path_display, App, AppAction, ConnectionState, NodeView, Provider, ProviderInventory, PtyColorMode,
    ManagedSessionView, SessionAddress, SessionView, UiKey, WorkspaceView,
};
use crate::diagnostics::RuntimeDiagnostic;
use crate::preferences::{self, UiPreferences};
use crate::render;

const COMMAND_QUEUE: usize = 64;
const UPDATE_QUEUE: usize = 256;
const C2_COMMAND_ROUTE: &str = "\0c2";
const RECONNECT_DELAY: Duration = Duration::from_millis(500);
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(250);
const LEASE_RENEWAL_INTERVAL: Duration = Duration::from_secs(30);
const RAW_INPUT_COALESCE: Duration = Duration::from_millis(12);
const INPUT_COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);
const INSPECTION_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const PREFERENCES_SAVE_DEBOUNCE: Duration = Duration::from_millis(350);

fn initial_viewport_size(cols: u16, rows: u16) -> TerminalSize {
    let sidebar_width = 26.min(cols / 2);
    TerminalSize {
        rows: rows.saturating_sub(1).max(1),
        columns: cols.saturating_sub(sidebar_width).max(1),
    }
}

#[derive(Clone)]
pub struct NodeEndpoint {
    pub expected_node_id: NodeId,
    pub endpoint: String,
    pub token: String,
}

#[derive(Clone)]
pub struct C2Endpoint {
    pub endpoint: String,
    pub token: String,
}

#[derive(Clone)]
pub struct StartupRequest {
    pub node_id: NodeId,
    pub workspace_id: WorkspaceId,
    pub provider: Provider,
}

#[derive(Clone)]
pub struct RunOptions {
    pub nodes: Vec<NodeEndpoint>,
    pub c2: Option<C2Endpoint>,
    pub startup: Option<StartupRequest>,
    pub color_mode_override: Option<PtyColorMode>,
}

enum WorkerUpdate {
    State {
        node_id: String,
        endpoint: String,
        state: ConnectionState,
    },
    TopologyNodeRemoved {
        node_id: String,
    },
    Snapshot {
        expected_node_id: String,
        endpoint: String,
        snapshot: NodeSnapshot,
        controller_owned: bool,
        event_sequence: u64,
    },
    C2Snapshot {
        expected_node_id: String,
        endpoint: String,
        incarnation_id: gate4agent_c2_protocol::NodeIncarnationId,
        snapshot: C2NodeSnapshot,
        event_sequence: u64,
    },
    C2Event {
        node_id: String,
        cursor: gate4agent_c2_protocol::NodeCursor,
        event: C2EventUpdate,
    },
    OpenSession(SessionAddress),
    SelectWorkspace { node_id: String, workspace_id: String },
    WorkspaceInspected {
        node_id: String,
        inspection: gate4agent_node_protocol::WorkspaceInspection,
    },
    WorkspaceInspectionFailed {
        node_id: String,
        workspace_id: String,
        message: String,
    },
    SessionRecordUpserted(ManagedSessionView),
    SessionRecordRemoved { node_id: String, record_id: String },
    Notice(String),
}

enum C2EventUpdate {
    SessionRecordUpserted(ManagedSessionView),
    SessionRecordRemoved { record_id: String },
    Notice(String),
    ResyncRequired,
    Ignored,
}

#[derive(Default)]
struct C2ApplyState {
    watermarks: BTreeMap<String, gate4agent_c2_protocol::NodeCursor>,
}

impl C2ApplyState {
    fn accept_snapshot(
        &mut self,
        node_id: &str,
        cursor: gate4agent_c2_protocol::NodeCursor,
    ) -> bool {
        if self.watermarks.get(node_id).is_some_and(|current| {
            current.incarnation_id == cursor.incarnation_id && cursor.sequence < current.sequence
        }) {
            return false;
        }
        self.watermarks.insert(node_id.to_owned(), cursor);
        true
    }

    fn accept_event(
        &mut self,
        node_id: &str,
        cursor: gate4agent_c2_protocol::NodeCursor,
    ) -> bool {
        if self.watermarks.get(node_id).is_some_and(|current| {
            current.incarnation_id == cursor.incarnation_id && cursor.sequence <= current.sequence
        }) {
            return false;
        }
        self.watermarks.insert(node_id.to_owned(), cursor);
        true
    }
}

struct PendingRaw {
    address: SessionAddress,
    text: String,
    updated_at: Instant,
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(
            stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

pub async fn run(options: RunOptions) -> Result<(), Box<dyn std::error::Error>> {
    let RunOptions { nodes, c2, startup, color_mode_override } = options;
    if nodes.is_empty() == c2.is_none() {
        return Err("configure exactly one direct-node set or one C2 control endpoint".into());
    }
    let preferences_path = preferences::default_path();
    let loaded_preferences = preferences_path.as_deref().and_then(|path| match UiPreferences::load(path) {
        Ok(preferences) => Some(preferences),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => {
            crate::diagnostics::record_runtime(RuntimeDiagnostic::PreferencesLoadFailed);
            None
        }
    });
    let initial_preferences = loaded_preferences.clone().unwrap_or_default();
    let _guard = TerminalGuard::enter()?;
    let (cols, rows) = terminal::size()?;
    let backend = CrosstermBackend::new(stdout());
    let mut screen = Screen::new(backend, cols, rows);
    screen.backend_mut().hide_cursor()?;

    let (updates_tx, mut updates_rx) = mpsc::channel(UPDATE_QUEUE);
    let mut commands = BTreeMap::new();
    let mut inspection_commands = BTreeMap::new();
    let mut app = App::default();
    let mut c2_apply_state = C2ApplyState::default();
    initial_preferences.apply_to(&mut app);
    if let Some(color_mode) = color_mode_override {
        app.color_mode = color_mode;
    }
    app.terminal_cols = cols;
    app.terminal_rows = rows;
    if let Some(endpoint) = c2 {
        let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE);
        commands.insert(C2_COMMAND_ROUTE.to_owned(), command_tx);
        tokio::spawn(c2_worker(
            endpoint,
            startup,
            initial_viewport_size(cols, rows),
            command_rx,
            updates_tx.clone(),
        ));
    } else {
        for endpoint in nodes {
            let node_id = endpoint.expected_node_id.to_string();
            app.set_node_connection(&node_id, &endpoint.endpoint, ConnectionState::Connecting);
            let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE);
            commands.insert(node_id.clone(), command_tx);
            let (inspection_tx, inspection_rx) = watch::channel(None);
            inspection_commands.insert(node_id.clone(), inspection_tx);
            let startup = startup
                .as_ref()
                .filter(|startup| startup.node_id == endpoint.expected_node_id)
                .cloned();
            let initial_terminal_size = initial_viewport_size(cols, rows);
            tokio::spawn(inspection_worker(
                endpoint.clone(),
                inspection_rx,
                updates_tx.clone(),
            ));
            tokio::spawn(node_worker(
                endpoint,
                startup,
                initial_terminal_size,
                command_rx,
                updates_tx.clone(),
            ));
        }
    }
    drop(updates_tx);

    let mut pending_raw: Option<PendingRaw> = None;
    let mut last_terminal_sizes = BTreeMap::new();
    let mut last_notice = None;
    let mut notice_deadline = None;
    let mut auto_inspected_route = None;
    let mut next_auto_inspection = Instant::now();
    let mut preferred_color_mode = initial_preferences.color_mode;
    let mut observed_app_color_mode = app.color_mode;
    let mut observed_preferences = preferences_for_save(&app, preferred_color_mode);
    let mut persisted_preferences = loaded_preferences;
    let mut preferences_deadline = None;
    while !app.should_quit {
        while let Ok(update) = updates_rx.try_recv() {
            apply_update(&mut app, &mut c2_apply_state, update);
        }
        let selected_route = app.selected_workspace_route();
        if selected_route != auto_inspected_route {
            auto_inspected_route = selected_route.clone();
            next_auto_inspection = Instant::now();
        }
        let now = Instant::now();
        if selected_route.is_some()
            && app.workspace_inspection_visible()
            && !app.workspace_inspection_pending()
            && now >= next_auto_inspection
        {
            let action = app.inspect_selected_workspace();
            queue_action(
                &mut app,
                &commands,
                &inspection_commands,
                &mut pending_raw,
                action,
            );
            next_auto_inspection = now + INSPECTION_REFRESH_INTERVAL;
        }
        if app.notice != last_notice {
            last_notice = app.notice.clone();
            notice_deadline = app.notice.as_ref().map(|_| Instant::now() + Duration::from_secs(3));
        } else if notice_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            app.notice = None;
            last_notice = None;
            notice_deadline = None;
        }
        if app.color_mode != observed_app_color_mode {
            observed_app_color_mode = app.color_mode;
            preferred_color_mode = app.color_mode;
        }
        let current_preferences = preferences_for_save(&app, preferred_color_mode);
        if current_preferences != observed_preferences {
            observed_preferences = current_preferences;
            preferences_deadline = Some(Instant::now() + PREFERENCES_SAVE_DEBOUNCE);
        }
        if preferences_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            if persisted_preferences.as_ref() != Some(&observed_preferences) {
                if let Some(path) = preferences_path.as_deref() {
                    match observed_preferences.save(path) {
                        Ok(()) => persisted_preferences = Some(observed_preferences.clone()),
                        Err(_) => crate::diagnostics::record_runtime(
                            RuntimeDiagnostic::PreferencesSaveFailed,
                        ),
                    }
                }
            }
            preferences_deadline = None;
        }

        app.layout = render::render(&app, screen.buffer_mut());
        for action in changed_terminal_sizes(&app, &mut last_terminal_sizes) {
            queue_action(&mut app, &commands, &inspection_commands, &mut pending_raw, action);
        }
        screen.flush()?;
        sync_cursor(&app)?;

        if pending_raw
            .as_ref()
            .is_some_and(|pending| pending.updated_at.elapsed() >= RAW_INPUT_COALESCE)
        {
            flush_raw(&mut app, &commands, &mut pending_raw);
        }

        if event::poll(Duration::from_millis(20))? {
            match event::read()? {
                TerminalEvent::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(key) = map_key(key) {
                        let action = app.reduce(key);
                        queue_action(&mut app, &commands, &inspection_commands, &mut pending_raw, action);
                    }
                }
                TerminalEvent::Mouse(mouse) => {
                    let action = map_mouse(&mut app, mouse);
                    queue_action(&mut app, &commands, &inspection_commands, &mut pending_raw, action);
                }
                TerminalEvent::Paste(text) => {
                    let action = app.paste(text);
                    queue_action(&mut app, &commands, &inspection_commands, &mut pending_raw, action);
                }
                TerminalEvent::Resize(cols, rows) => {
                    screen.resize(cols, rows);
                    app.terminal_cols = cols;
                    app.terminal_rows = rows;
                }
                _ => {}
            }
        }
    }
    let final_preferences = preferences_for_save(&app, preferred_color_mode);
    if persisted_preferences.as_ref() != Some(&final_preferences) {
        if let Some(path) = preferences_path.as_deref() {
            if final_preferences.save(path).is_err() {
                crate::diagnostics::record_runtime(RuntimeDiagnostic::PreferencesSaveFailed);
            }
        }
    }
    flush_raw(&mut app, &commands, &mut pending_raw);
    screen.backend_mut().show_cursor()?;
    Ok(())
}

fn preferences_for_save(app: &App, color_mode: PtyColorMode) -> UiPreferences {
    let mut preferences = UiPreferences::from_app(app);
    preferences.color_mode = color_mode;
    preferences
}

fn queue_action(
    app: &mut App,
    commands: &BTreeMap<String, mpsc::Sender<AppAction>>,
    inspection_commands: &BTreeMap<String, watch::Sender<Option<WorkspaceId>>>,
    pending_raw: &mut Option<PendingRaw>,
    action: AppAction,
) {
    if let AppAction::Input { address, text } = action {
        if let Some(pending) = pending_raw.as_mut() {
            if pending.address == address && pending.text.len() + text.len() <= 16 * 1024 {
                pending.text.push_str(&text);
                pending.updated_at = Instant::now();
                return;
            }
        }
        flush_raw(app, commands, pending_raw);
        *pending_raw = Some(PendingRaw {
            address,
            text,
            updated_at: Instant::now(),
        });
        return;
    }
    flush_raw(app, commands, pending_raw);
    send_action(app, commands, inspection_commands, action);
}

fn flush_raw(
    app: &mut App,
    commands: &BTreeMap<String, mpsc::Sender<AppAction>>,
    pending_raw: &mut Option<PendingRaw>,
) {
    let Some(pending) = pending_raw.take() else {
        return;
    };
    send_operator_action(
        app,
        commands,
        AppAction::Input {
            address: pending.address,
            text: pending.text,
        },
    );
}

fn send_action(
    app: &mut App,
    commands: &BTreeMap<String, mpsc::Sender<AppAction>>,
    inspection_commands: &BTreeMap<String, watch::Sender<Option<WorkspaceId>>>,
    action: AppAction,
) {
    if let AppAction::InspectWorkspace { node_id, workspace_id } = &action {
        if let Some(sender) = inspection_commands.get(node_id) {
            let Ok(workspace_id_value) = WorkspaceId::new(workspace_id.clone()) else {
                app.fail_workspace_inspection(
                    node_id.clone(),
                    workspace_id.clone(),
                    "invalid workspace ID".to_owned(),
                );
                return;
            };
            sender.send_replace(Some(workspace_id_value));
            return;
        }
    }
    send_operator_action(app, commands, action);
}

fn send_operator_action(
    app: &mut App,
    commands: &BTreeMap<String, mpsc::Sender<AppAction>>,
    action: AppAction,
) {
    let Some(node_id) = action_node_id(&action).map(str::to_owned) else {
        return;
    };
    let Some(sender) = commands
        .get(&node_id)
        .or_else(|| commands.get(C2_COMMAND_ROUTE))
    else {
        app.notice = Some(format!("unknown node {node_id}"));
        return;
    };
    if sender.try_send(action).is_err() {
        app.notice = Some(format!("{node_id}: command queue busy; action cancelled"));
    }
}

fn action_node_id(action: &AppAction) -> Option<&str> {
    match action {
        AppAction::Spawn { node_id, .. }
        | AppAction::ResumeSessionRecord { node_id, .. }
        | AppAction::RenameSessionRecord { node_id, .. }
        | AppAction::ForgetSessionRecord { node_id, .. }
        | AppAction::RegisterWorkspace { node_id, .. }
        | AppAction::UnregisterWorkspace { node_id, .. }
        | AppAction::CreateWorktree { node_id, .. }
        | AppAction::RemoveWorktree { node_id, .. }
        | AppAction::InspectWorkspace { node_id, .. }
        | AppAction::Resync { node_id, .. } => Some(node_id),
        AppAction::Resume { address, .. }
        | AppAction::Input { address, .. }
        | AppAction::Paste { address, .. }
        | AppAction::TerminalControl { address, .. }
        | AppAction::TerminalBytes { address, .. }
        | AppAction::Resize { address, .. }
        | AppAction::Stop { address, .. }
        | AppAction::Remove { address } => Some(&address.node_id),
        AppAction::None | AppAction::Quit => None,
    }
}

fn sync_cursor(app: &App) -> io::Result<()> {
    if let Some((column, row)) = visible_cursor_position(app) {
        execute!(stdout(), MoveTo(column, row), Show)
    } else {
        execute!(stdout(), Hide)
    }
}

fn visible_cursor_position(app: &App) -> Option<(u16, u16)> {
    if app.focus != crate::app::Focus::Viewport {
        return None;
    }
    let area = app.focused_terminal_rect();
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let session = app.focused_session()?;
    if !session.running {
        return None;
    }
    if app.terminal_scroll_offset(&session.address) > 0 {
        return None;
    }
    let (row, column) = session.terminal_cursor?;
    Some((
        area.x + column.min(area.width - 1),
        area.y + row.min(area.height - 1),
    ))
}

fn changed_terminal_sizes(
    app: &App,
    last: &mut BTreeMap<SessionAddress, (u16, u16)>,
) -> Vec<AppAction> {
    diff_terminal_sizes(app.desired_terminal_sizes(), last)
}

fn diff_terminal_sizes(
    desired: Vec<(SessionAddress, u16, u16)>,
    last: &mut BTreeMap<SessionAddress, (u16, u16)>,
) -> Vec<AppAction> {
    let desired = desired
        .into_iter()
        .map(|(address, rows, cols)| (address, (rows, cols)))
        .collect::<BTreeMap<_, _>>();
    let actions = desired
        .iter()
        .filter(|(address, size)| last.get(*address) != Some(*size))
        .map(|(address, (rows, cols))| AppAction::Resize {
            address: address.clone(),
            rows: *rows,
            cols: *cols,
        })
        .collect();
    *last = desired;
    actions
}

fn map_key(key: KeyEvent) -> Option<UiKey> {
    if key.modifiers.intersects(
        KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META,
    ) {
        return Some(UiKey::UnsupportedModifier);
    }
    if key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT) {
        if let KeyCode::Char(ch) = key.code {
            if ch.eq_ignore_ascii_case(&'g') {
                return Some(UiKey::OperatorEscape);
            }
        }
    }
    if key.modifiers == KeyModifiers::CONTROL {
        if let KeyCode::Char(ch) = key.code {
            return Some(UiKey::Ctrl(ch.to_ascii_lowercase()));
        }
    }
    if key.modifiers.contains(KeyModifiers::ALT)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
    {
        if let KeyCode::Char(ch) = key.code {
            let mut bytes = Vec::with_capacity(5);
            bytes.push(0x1b);
            let mut encoded = [0_u8; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
            return Some(UiKey::TerminalBytes(bytes));
        }
        return Some(UiKey::UnsupportedModifier);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        || (key.modifiers.contains(KeyModifiers::SHIFT)
            && !matches!(key.code, KeyCode::Char(_) | KeyCode::Tab | KeyCode::BackTab))
    {
        return Some(UiKey::UnsupportedModifier);
    }
    match key.code {
        KeyCode::Char(ch) => Some(UiKey::Char(ch)),
        KeyCode::Enter => Some(UiKey::Enter),
        KeyCode::Esc => Some(UiKey::Escape),
        KeyCode::Backspace => Some(UiKey::Backspace),
        KeyCode::Insert => Some(UiKey::Insert),
        KeyCode::Delete => Some(UiKey::Delete),
        KeyCode::Home => Some(UiKey::Home),
        KeyCode::End => Some(UiKey::End),
        KeyCode::Up => Some(UiKey::Up),
        KeyCode::Down => Some(UiKey::Down),
        KeyCode::Left => Some(UiKey::Left),
        KeyCode::Right => Some(UiKey::Right),
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => Some(UiKey::BackTab),
        KeyCode::Tab => Some(UiKey::Tab),
        KeyCode::BackTab => Some(UiKey::BackTab),
        KeyCode::PageUp => Some(UiKey::PageUp),
        KeyCode::PageDown => Some(UiKey::PageDown),
        KeyCode::F(number) if (1..=12).contains(&number) => Some(UiKey::Function(number)),
        _ => None,
    }
}

fn map_mouse(app: &mut App, mouse: MouseEvent) -> AppAction {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => app.click(mouse.column, mouse.row),
        MouseEventKind::Drag(MouseButton::Left) => app.drag(mouse.column, mouse.row),
        MouseEventKind::Up(MouseButton::Left) => app.drop_at(mouse.column, mouse.row),
        MouseEventKind::ScrollUp => app.scroll(mouse.column, mouse.row, true),
        MouseEventKind::ScrollDown => app.scroll(mouse.column, mouse.row, false),
        _ => AppAction::None,
    }
}

async fn inspection_worker(
    endpoint: NodeEndpoint,
    mut requests: watch::Receiver<Option<WorkspaceId>>,
    updates: mpsc::Sender<WorkerUpdate>,
) {
    let node_id = endpoint.expected_node_id.to_string();
    let mut pending = None;
    loop {
        if pending.is_none() {
            if requests.changed().await.is_err() {
                return;
            }
            pending = requests.borrow_and_update().clone();
        }
        let Some(workspace_id) = pending.clone() else {
            continue;
        };
        // Observer connections also receive the node event stream. Keep each one scoped to a
        // single inspection so an idle file/git panel cannot accumulate PTY events.
        let mut node = match NamedPipeNodeClient::connect(
            &endpoint.endpoint,
            &endpoint.expected_node_id,
            ClientRole::Observer,
            &endpoint.token,
        )
        .await
        {
            Ok(node) => node,
            Err(_) => {
                sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        if node.hello().snapshot.node_id != endpoint.expected_node_id {
            send_update(
                &updates,
                WorkerUpdate::WorkspaceInspectionFailed {
                    node_id: node_id.clone(),
                    workspace_id: workspace_id.to_string(),
                    message: "node identity mismatch".to_owned(),
                },
            )
            .await;
            pending = None;
            continue;
        }
        match node
            .request(NodeRequest::InspectWorkspace {
                workspace_id: workspace_id.clone(),
            })
            .await
        {
            Ok(NodeResponse::WorkspaceInspected { inspection }) => {
                send_update(
                    &updates,
                    WorkerUpdate::WorkspaceInspected {
                        node_id: node_id.clone(),
                        inspection,
                    },
                )
                .await;
                pending = None;
            }
            Ok(_) => {
                send_update(
                    &updates,
                    WorkerUpdate::WorkspaceInspectionFailed {
                        node_id: node_id.clone(),
                        workspace_id: workspace_id.to_string(),
                        message: "unexpected inspection response".to_owned(),
                    },
                )
                .await;
                pending = None;
            }
            Err(error) if is_transport_error(&error) => {
                sleep(RECONNECT_DELAY).await;
            }
            Err(error) => {
                send_update(
                    &updates,
                    WorkerUpdate::WorkspaceInspectionFailed {
                        node_id: node_id.clone(),
                        workspace_id: workspace_id.to_string(),
                        message: safe_client_error(&error).to_owned(),
                    },
                )
                .await;
                pending = None;
            }
        }
    }
}

fn project_c2_topology(
    topology: &C2Topology,
) -> (BTreeMap<String, NodeRoute>, BTreeMap<String, String>) {
    let mut routes = BTreeMap::new();
    let mut endpoints = BTreeMap::new();
    for node in &topology.nodes {
        let node_id = node.node_id.to_string();
        endpoints.insert(node_id.clone(), node.endpoint.clone());
        if node.transport == NodeTransportState::Online {
            if let Some(expected_incarnation_id) = node.current_incarnation_id {
                routes.insert(
                    node_id,
                    NodeRoute {
                        node_id: node.node_id.clone(),
                        expected_incarnation_id,
                    },
                );
            }
        }
    }
    (routes, endpoints)
}

async fn reconcile_c2_topology(
    topology: &C2Topology,
    routes: &mut BTreeMap<String, NodeRoute>,
    endpoints: &mut BTreeMap<String, String>,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Vec<NodeRoute> {
    let (next_routes, next_endpoints) = project_c2_topology(topology);
    for node_id in endpoints.keys() {
        if !next_endpoints.contains_key(node_id) {
            send_update(
                updates,
                WorkerUpdate::TopologyNodeRemoved {
                    node_id: node_id.clone(),
                },
            )
            .await;
        }
    }
    let mut snapshots = Vec::new();
    for (node_id, endpoint) in &next_endpoints {
        let route_changed = routes.get(node_id) != next_routes.get(node_id);
        let endpoint_changed = endpoints.get(node_id) != Some(endpoint);
        if let Some(route) = next_routes.get(node_id) {
            if route_changed || endpoint_changed {
                send_update(
                    updates,
                    WorkerUpdate::State {
                        node_id: node_id.clone(),
                        endpoint: endpoint.clone(),
                        state: ConnectionState::Connecting,
                    },
                )
                .await;
                snapshots.push(route.clone());
            }
        } else if route_changed || endpoint_changed || !endpoints.contains_key(node_id) {
            send_update(
                updates,
                WorkerUpdate::State {
                    node_id: node_id.clone(),
                    endpoint: endpoint.clone(),
                    state: ConnectionState::Disconnected("node offline at C2".to_owned()),
                },
            )
            .await;
        }
    }
    *routes = next_routes;
    *endpoints = next_endpoints;
    snapshots
}

async fn dispatch_c2_startup(
    handle: &C2ControlHandle,
    routes: &BTreeMap<String, NodeRoute>,
    endpoints: &BTreeMap<String, String>,
    startup: &mut Option<StartupRequest>,
    initial_terminal_size: TerminalSize,
    updates: &mpsc::Sender<WorkerUpdate>,
) -> Result<bool, C2ControlError> {
    let Some(pending) = startup.as_ref() else {
        return Ok(false);
    };
    let Some(route) = routes.get(pending.node_id.as_str()).cloned() else {
        return Ok(false);
    };
    let request = startup.take().expect("checked pending C2 startup request");
    let action = AppAction::Spawn {
        node_id: request.node_id.to_string(),
        workspace_id: request.workspace_id.to_string(),
        provider: request.provider,
        rows: initial_terminal_size.rows,
        cols: initial_terminal_size.columns,
    };
    if let Some(node_request) = action_to_request(action) {
        c2_request_and_publish(handle, route, node_request, updates, endpoints).await?;
    }
    Ok(true)
}

async fn disconnect_c2_nodes(
    endpoints: &BTreeMap<String, String>,
    updates: &mpsc::Sender<WorkerUpdate>,
    reason: &str,
) {
    for (node_id, endpoint) in endpoints {
        send_update(
            updates,
            WorkerUpdate::State {
                node_id: node_id.clone(),
                endpoint: endpoint.clone(),
                state: ConnectionState::Disconnected(reason.to_owned()),
            },
        )
        .await;
    }
}

async fn c2_worker(
    endpoint: C2Endpoint,
    startup: Option<StartupRequest>,
    initial_terminal_size: TerminalSize,
    mut commands: mpsc::Receiver<AppAction>,
    updates: mpsc::Sender<WorkerUpdate>,
) {
    let mut startup = startup;
    let mut last_connect_failure = None;
    loop {
        let (handle, mut events) = match connect_c2_local(&endpoint.endpoint, &endpoint.token).await {
            Ok(connection) => {
                last_connect_failure = None;
                connection
            }
            Err(error) => {
                let failure = safe_c2_error(&error);
                if last_connect_failure.as_deref() != Some(failure.as_str()) {
                    crate::diagnostics::record_runtime(RuntimeDiagnostic::C2ReconnectFailed);
                    send_update(
                        &updates,
                        WorkerUpdate::Notice(format!(
                            "C2 control unavailable: {failure}; retrying"
                        )),
                    )
                    .await;
                    last_connect_failure = Some(failure);
                }
                sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        let mut topology = handle.subscribe_topology();
        let (routed_events_tx, mut routed_events_rx) = mpsc::channel(UPDATE_QUEUE);
        let event_forwarder = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if routed_events_tx.send(event).await.is_err() {
                    return;
                }
            }
        });
        let mut routes = BTreeMap::new();
        let mut node_endpoints = BTreeMap::new();
        let initial_topology = topology.borrow_and_update().clone();
        let initial_routes = reconcile_c2_topology(
            initial_topology.as_ref(),
            &mut routes,
            &mut node_endpoints,
            &updates,
        )
        .await;
        let mut transport_failed = false;
        for route in initial_routes {
            if c2_request_and_publish(
                &handle,
                route,
                NodeRequest::Snapshot,
                &updates,
                &node_endpoints,
            )
            .await.is_err() {
                crate::diagnostics::record_runtime(RuntimeDiagnostic::C2InitialSnapshotFailed);
                transport_failed = true;
                break;
            }
        }
        if transport_failed {
            event_forwarder.abort();
            disconnect_c2_nodes(&node_endpoints, &updates, "C2 control disconnected").await;
            sleep(RECONNECT_DELAY).await;
            continue;
        }
        let startup_dispatched = match dispatch_c2_startup(
            &handle,
            &routes,
            &node_endpoints,
            &mut startup,
            initial_terminal_size,
            &updates,
        )
        .await
        {
            Ok(dispatched) => dispatched,
            Err(_) => {
                crate::diagnostics::record_runtime(RuntimeDiagnostic::C2StartupFailed);
                event_forwarder.abort();
                disconnect_c2_nodes(
                    &node_endpoints,
                    &updates,
                    "C2 control disconnected",
                )
                .await;
                sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        if !startup_dispatched {
            if let Some(request) = startup.as_ref() {
                send_update(
                    &updates,
                    WorkerUpdate::Notice(format!(
                        "{}: startup waits for the node to become online at C2",
                        request.node_id
                    )),
                )
                .await;
            }
        }

        let mut snapshot_tick = tokio::time::interval(SNAPSHOT_INTERVAL);
        snapshot_tick.reset();
        let _failure = 'connected: loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { return; };
                    let Some(node_id) = action_node_id(&command).map(str::to_owned) else { continue; };
                    let Some(route) = routes.get(&node_id).cloned() else {
                        send_update(&updates, WorkerUpdate::Notice(format!("{node_id}: node offline at C2"))).await;
                        continue;
                    };
                    let requests = match command {
                        AppAction::Paste { address, text } => utf8_chunks(&text, MAX_NODE_TEXT_BYTES)
                            .into_iter()
                            .map(|chunk| NodeRequest::Paste {
                                session: wire_address(&address).expect("validated TUI session address"),
                                text: chunk.to_owned(),
                            })
                            .collect::<Vec<_>>(),
                        command => action_to_request(command).into_iter().collect(),
                    };
                    for request in requests {
                        if let Err(error) = c2_request_and_publish(
                            &handle,
                            route.clone(),
                            request,
                            &updates,
                            &node_endpoints,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                    }
                }
                event = routed_events_rx.recv() => {
                    let Some(event) = event else { break "event stream closed".to_owned(); };
                    let node_id = event.node_id.to_string();
                    if routes.get(&node_id).is_some_and(|route| {
                        route.expected_incarnation_id == event.cursor.incarnation_id
                    }) {
                        publish_c2_event(event.event, event.cursor, &updates, &node_id).await;
                    }
                }
                changed = topology.changed() => {
                    if changed.is_err() {
                        break "topology stream closed".to_owned();
                    }
                    let next = topology.borrow_and_update().clone();
                    let snapshot_routes = reconcile_c2_topology(
                        next.as_ref(),
                        &mut routes,
                        &mut node_endpoints,
                        &updates,
                    ).await;
                    for route in snapshot_routes {
                        if let Err(error) = c2_request_and_publish(
                            &handle,
                            route,
                            NodeRequest::Snapshot,
                            &updates,
                            &node_endpoints,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                    }
                    if let Err(error) = dispatch_c2_startup(
                        &handle,
                        &routes,
                        &node_endpoints,
                        &mut startup,
                        initial_terminal_size,
                        &updates,
                    ).await {
                        break 'connected safe_c2_error(&error);
                    }
                }
                _ = snapshot_tick.tick() => {
                    for route in routes.values().cloned().collect::<Vec<_>>() {
                        if let Err(error) = c2_request_and_publish(
                            &handle,
                            route,
                            NodeRequest::Snapshot,
                            &updates,
                            &node_endpoints,
                        ).await {
                            break 'connected safe_c2_error(&error);
                        }
                    }
                }
            }
        };
        event_forwarder.abort();
        crate::diagnostics::record_runtime(RuntimeDiagnostic::C2ControlDisconnected);
        disconnect_c2_nodes(&node_endpoints, &updates, "C2 control disconnected").await;
        sleep(RECONNECT_DELAY).await;
    }
}

async fn c2_request_and_publish(
    handle: &C2ControlHandle,
    route: NodeRoute,
    request: NodeRequest,
    updates: &mpsc::Sender<WorkerUpdate>,
    endpoints: &BTreeMap<String, String>,
) -> Result<(), C2ControlError> {
    let routed = handle.request(route, request).await?;
    let node_id = routed.node_id.to_string();
    let incarnation_id = routed.incarnation_id;
    match routed.response {
        Ok(response) => publish_c2_response(
            response,
            updates,
            &node_id,
            incarnation_id,
            endpoints.get(&node_id).cloned().unwrap_or_default(),
        )
        .await,
        Err(failure) => {
            send_update(
                updates,
                WorkerUpdate::Notice(format!(
                    "{node_id}: {}",
                    safe_node_failure_code(failure.code)
                )),
            )
            .await;
        }
    }
    Ok(())
}

async fn publish_c2_response(
    response: C2NodeResponse,
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
    incarnation_id: gate4agent_c2_protocol::NodeIncarnationId,
    endpoint: String,
) {
    match response {
        C2NodeResponse::Snapshot { event_sequence, snapshot, .. } => {
            send_update(
                updates,
                WorkerUpdate::C2Snapshot {
                    expected_node_id: node_id.to_owned(),
                    endpoint,
                    incarnation_id,
                    snapshot,
                    event_sequence,
                },
            )
            .await;
        }
        C2NodeResponse::Resync { event_sequence, snapshot, events } => {
            for envelope in events {
                publish_c2_event(
                    envelope.event,
                    gate4agent_c2_protocol::NodeCursor {
                        incarnation_id,
                        sequence: envelope.sequence,
                    },
                    updates,
                    node_id,
                ).await;
            }
            send_update(
                updates,
                WorkerUpdate::C2Snapshot {
                    expected_node_id: node_id.to_owned(),
                    endpoint,
                    incarnation_id,
                    snapshot,
                    event_sequence,
                },
            )
            .await;
        }
        C2NodeResponse::WorkspaceInspected { inspection } => {
            send_update(
                updates,
                WorkerUpdate::WorkspaceInspected {
                    node_id: node_id.to_owned(),
                    inspection: project_c2_workspace_inspection(inspection),
                },
            )
            .await;
        }
        C2NodeResponse::WorkspaceFileRead { .. } => {}
        C2NodeResponse::SpawnAccepted { session } => {
            send_update(updates, WorkerUpdate::OpenSession(project_wire_address(node_id, session))).await;
        }
        C2NodeResponse::SessionRecordUpdated { record } => {
            send_update(
                updates,
                WorkerUpdate::SessionRecordUpserted(project_c2_managed_session(node_id, record)),
            )
            .await;
        }
        C2NodeResponse::SessionRecordResumed { record, session } => {
            send_update(
                updates,
                WorkerUpdate::SessionRecordUpserted(project_c2_managed_session(node_id, record)),
            )
            .await;
            send_update(updates, WorkerUpdate::OpenSession(project_wire_address(node_id, session))).await;
        }
        C2NodeResponse::SessionRecordForgotten { record_id } => {
            send_update(
                updates,
                WorkerUpdate::SessionRecordRemoved {
                    node_id: node_id.to_owned(),
                    record_id: record_id.to_string(),
                },
            )
            .await;
        }
        C2NodeResponse::WorkspaceRegistered { workspace } => {
            send_update(
                updates,
                WorkerUpdate::SelectWorkspace {
                    node_id: node_id.to_owned(),
                    workspace_id: workspace.workspace_id.to_string(),
                },
            )
            .await;
        }
        C2NodeResponse::WorkspaceUnregistered { workspace_id } => {
            send_update(
                updates,
                WorkerUpdate::Notice(format!("{node_id}: space {workspace_id} unregistered")),
            )
            .await;
        }
        C2NodeResponse::WorktreeCreated { worktree, workspace } => {
            send_update(
                updates,
                WorkerUpdate::SelectWorkspace {
                    node_id: node_id.to_owned(),
                    workspace_id: workspace.workspace_id.to_string(),
                },
            )
            .await;
            send_update(
                updates,
                WorkerUpdate::Notice(format!("{node_id}: worktree {} created", host_path_display(&worktree.path))),
            )
            .await;
        }
        C2NodeResponse::WorktreeRemoved { target_root, .. } => {
            send_update(
                updates,
                WorkerUpdate::Notice(format!("{node_id}: worktree {} removed", host_path_display(&target_root))),
            )
            .await;
        }
        C2NodeResponse::Controller { .. } | C2NodeResponse::Accepted => {}
        C2NodeResponse::ShuttingDown => {
            send_update(
                updates,
                WorkerUpdate::State {
                    node_id: node_id.to_owned(),
                    endpoint,
                    state: ConnectionState::Disconnected("node shutdown".to_owned()),
                },
            )
            .await;
        }
    }
}

async fn publish_c2_event(
    event: C2NodeEvent,
    cursor: gate4agent_c2_protocol::NodeCursor,
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
) {
    let event = match event {
        C2NodeEvent::Control { address, event } => {
            if let Some(notice) = safe_c2_control_notice(node_id, &address, &event) {
                Some(C2EventUpdate::Notice(notice))
            } else {
                None
            }
        }
        C2NodeEvent::SessionRecordUpserted { record } => Some(
            C2EventUpdate::SessionRecordUpserted(project_c2_managed_session(node_id, record)),
        ),
        C2NodeEvent::SessionRecordRemoved { record_id } => Some(
            C2EventUpdate::SessionRecordRemoved { record_id: record_id.to_string() },
        ),
        C2NodeEvent::ResyncRequired { .. } => Some(C2EventUpdate::ResyncRequired),
        C2NodeEvent::ControllerChanged { .. }
        | C2NodeEvent::WorkspaceAdded { .. }
        | C2NodeEvent::WorkspaceRemoved { .. } => Some(C2EventUpdate::Ignored),
    };
    send_update(updates, WorkerUpdate::C2Event {
        node_id: node_id.to_owned(),
        cursor,
        event: event.unwrap_or(C2EventUpdate::Ignored),
    }).await;
}

fn project_wire_address(node_id: &str, address: WireSessionAddress) -> SessionAddress {
    SessionAddress {
        node_id: node_id.to_owned(),
        workspace_id: address.workspace_id.to_string(),
        instance_id: address.session.instance_id.0,
        generation: address.session.generation.0,
    }
}

fn safe_c2_error(error: &C2ControlError) -> String {
    match error {
        C2ControlError::InvalidEndpoint => "invalid C2 control endpoint".to_owned(),
        C2ControlError::InvalidToken | C2ControlError::Authentication(_) => {
            "C2 authentication unavailable".to_owned()
        }
        C2ControlError::AuthenticationTimedOut => "C2 authentication timed out".to_owned(),
        C2ControlError::Io(_) | C2ControlError::Closed => "C2 transport unavailable".to_owned(),
        C2ControlError::Frame(_) | C2ControlError::Protocol(_) => "C2 protocol invalid".to_owned(),
        C2ControlError::Relay(failure) => format!("C2 relay {:?}", failure.code),
        C2ControlError::RequestIdExhausted => "C2 request counter exhausted".to_owned(),
    }
}

async fn node_worker(
    endpoint: NodeEndpoint,
    startup: Option<StartupRequest>,
    initial_terminal_size: TerminalSize,
    mut commands: mpsc::Receiver<AppAction>,
    updates: mpsc::Sender<WorkerUpdate>,
) {
    let expected = endpoint.expected_node_id.to_string();
    let mut startup = startup;
    loop {
        send_update(
            &updates,
            WorkerUpdate::State {
                node_id: expected.clone(),
                endpoint: endpoint.endpoint.clone(),
                state: ConnectionState::Connecting,
            },
        )
        .await;
        let mut node = match NamedPipeNodeClient::connect(
            &endpoint.endpoint,
            &endpoint.expected_node_id,
            ClientRole::Operator,
            &endpoint.token,
        )
        .await
        {
            Ok(node) => node,
            Err(error) => {
                disconnected(&updates, &endpoint, safe_client_error(&error)).await;
                reject_disconnected_commands(&mut commands, &updates, &expected);
                sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        if node.hello().snapshot.node_id != endpoint.expected_node_id {
            disconnected(&updates, &endpoint, "node identity mismatch").await;
            send_update(
                &updates,
                WorkerUpdate::Notice(format!("{expected}: expected node identity was not presented")),
            )
            .await;
            sleep(RECONNECT_DELAY).await;
            continue;
        }

        let connection_id = node.hello().connection_id;
        let mut controller_owned = owns_controller(&node.hello().controller, connection_id);
        let hello = node.hello().clone();
        send_snapshot(
            &updates,
            &endpoint,
            hello.snapshot,
            controller_owned,
            hello.event_sequence,
        )
        .await;

        if let Some(request) = startup.take() {
            if ensure_controller(
                &mut node,
                &mut controller_owned,
                connection_id,
                &updates,
                &expected,
            )
            .await
            {
                let spawn = NodeRequest::Spawn {
                    workspace_id: request.workspace_id,
                    provider: map_provider(request.provider),
                    mode: SessionMode::Pty,
                    terminal_size: initial_terminal_size,
                    initial_prompt: None,
                };
                if let Err(error) = request_and_publish(
                    &mut node,
                    spawn,
                    &updates,
                    &endpoint,
                    &mut controller_owned,
                    connection_id,
                )
                .await
                {
                    send_update(
                        &updates,
                        WorkerUpdate::Notice(format!("{expected}: startup {}", safe_client_error(&error))),
                    )
                    .await;
                    if is_transport_error(&error) {
                        disconnected(&updates, &endpoint, safe_client_error(&error)).await;
                        continue;
                    }
                }
            }
        }

        let mut snapshot_tick = tokio::time::interval(SNAPSHOT_INTERVAL);
        let mut lease_tick = tokio::time::interval(LEASE_RENEWAL_INTERVAL);
        snapshot_tick.reset();
        lease_tick.reset();
        let disconnect_reason = loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else {
                        return;
                    };
                    match dispatch_worker_action(
                        &mut node,
                        command,
                        &updates,
                        &endpoint,
                        &mut controller_owned,
                        connection_id,
                    ).await {
                        Ok(()) => {}
                        Err(error) if is_transport_error(&error) => break safe_client_error(&error),
                        Err(error) => {
                            send_update(
                                &updates,
                                WorkerUpdate::Notice(format!("{expected}: {}", safe_client_error(&error))),
                            ).await;
                        }
                    }
                }
                _ = snapshot_tick.tick() => {
                    match request_and_publish(
                        &mut node,
                        NodeRequest::Snapshot,
                        &updates,
                        &endpoint,
                        &mut controller_owned,
                        connection_id,
                    ).await {
                        Ok(()) => {}
                        Err(error) if is_transport_error(&error) => break safe_client_error(&error),
                        Err(error) => {
                            send_update(
                                &updates,
                                WorkerUpdate::Notice(format!("{expected}: {}", safe_client_error(&error))),
                            ).await;
                        }
                    }
                }
                _ = lease_tick.tick() => {
                    if controller_owned && !ensure_controller(
                        &mut node,
                        &mut controller_owned,
                        connection_id,
                        &updates,
                        &expected,
                    ).await {
                        send_update(
                            &updates,
                            WorkerUpdate::Notice(format!("{expected}: controller lease lost")),
                        ).await;
                    }
                }
            }
            publish_pending_events(&mut node, &updates, &expected, &mut controller_owned, connection_id).await;
        };
        disconnected(&updates, &endpoint, disconnect_reason).await;
        reject_disconnected_commands(&mut commands, &updates, &expected);
        sleep(RECONNECT_DELAY).await;
    }
}

async fn dispatch_worker_action(
    node: &mut NamedPipeNodeClient,
    action: AppAction,
    updates: &mpsc::Sender<WorkerUpdate>,
    endpoint: &NodeEndpoint,
    controller_owned: &mut bool,
    connection_id: u64,
) -> Result<(), NodeClientError> {
    if matches!(action, AppAction::None | AppAction::Quit) {
        return Ok(());
    }
    let requires_controller = !matches!(
        action,
        AppAction::Resync { .. } | AppAction::InspectWorkspace { .. }
    );
    if requires_controller
        && !ensure_controller(
            node,
            controller_owned,
            connection_id,
            updates,
            endpoint.expected_node_id.as_str(),
        )
        .await
    {
        send_update(
            updates,
            WorkerUpdate::Notice(format!(
                "{}: controller unavailable; action cancelled",
                endpoint.expected_node_id
            )),
        )
        .await;
        return Ok(());
    }
    let action = match action {
        AppAction::Paste { address, text } => {
            let wire = wire_address(&address)
                .map_err(|message| NodeClientError::Protocol(message))?;
            for chunk in utf8_chunks(&text, MAX_NODE_TEXT_BYTES) {
                request_and_publish(
                    node,
                    NodeRequest::Paste {
                        session: wire.clone(),
                        text: chunk.to_owned(),
                    },
                    updates,
                    endpoint,
                    controller_owned,
                    connection_id,
                )
                .await?;
                await_input_completion(
                    node,
                    &address,
                    updates,
                    endpoint,
                    controller_owned,
                    connection_id,
                )
                .await?;
            }
            return Ok(());
        }
        action => action,
    };
    let wait_address = match &action {
        AppAction::Input { address, .. } => Some(address.clone()),
        _ => None,
    };
    let Some(request) = action_to_request(action) else {
        return Err(NodeClientError::Protocol("operator action is invalid".to_owned()));
    };
    request_and_publish(
        node,
        request,
        updates,
        endpoint,
        controller_owned,
        connection_id,
    )
    .await?;
    if let Some(address) = wait_address {
        await_input_completion(
            node,
            &address,
            updates,
            endpoint,
            controller_owned,
            connection_id,
        )
        .await?;
    }
    Ok(())
}

fn utf8_chunks(text: &str, max_bytes: usize) -> Vec<&str> {
    if text.is_empty() || max_bytes == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + max_bytes).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = text[start..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(offset, _)| start + offset);
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

fn action_to_request(action: AppAction) -> Option<NodeRequest> {
    match action {
        AppAction::None | AppAction::Quit => None,
        AppAction::Spawn { workspace_id, provider, rows, cols, .. } => Some(NodeRequest::Spawn {
            workspace_id: WorkspaceId::new(workspace_id).ok()?,
            provider: map_provider(provider),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize { rows, columns: cols },
            initial_prompt: None,
        }),
        AppAction::Resume { address, rows, cols } => Some(NodeRequest::Resume {
            session: wire_address(&address).ok()?,
            terminal_size: TerminalSize { rows, columns: cols },
            initial_prompt: None,
        }),
        AppAction::ResumeSessionRecord { record_id, rows, cols, .. } => {
            Some(NodeRequest::ResumeSessionRecord {
                record_id: SessionRecordId::new(record_id).ok()?,
                terminal_size: TerminalSize { rows, columns: cols },
                initial_prompt: None,
            })
        }
        AppAction::RenameSessionRecord { record_id, display_name, .. } => {
            Some(NodeRequest::RenameSessionRecord {
                record_id: SessionRecordId::new(record_id).ok()?,
                display_name,
            })
        }
        AppAction::ForgetSessionRecord { record_id, .. } => {
            Some(NodeRequest::ForgetSessionRecord {
                record_id: SessionRecordId::new(record_id).ok()?,
            })
        }
        AppAction::Input { address, text } => Some(NodeRequest::Input {
            session: wire_address(&address).ok()?,
            text,
        }),
        AppAction::Paste { address, text } => Some(NodeRequest::Paste {
            session: wire_address(&address).ok()?,
            text,
        }),
        AppAction::TerminalControl { address, control } => Some(NodeRequest::TerminalControl {
            session: wire_address(&address).ok()?,
            control,
        }),
        AppAction::TerminalBytes { address, bytes } => Some(NodeRequest::TerminalBytes {
            session: wire_address(&address).ok()?,
            bytes,
        }),
        AppAction::Resize { address, rows, cols } => Some(NodeRequest::Resize {
            session: wire_address(&address).ok()?,
            size: TerminalSize { rows, columns: cols },
        }),
        AppAction::Stop { address, force } => Some(NodeRequest::Stop {
            session: wire_address(&address).ok()?,
            force,
        }),
        AppAction::Remove { address } => Some(NodeRequest::Remove {
            session: wire_address(&address).ok()?,
        }),
        AppAction::RegisterWorkspace { workspace_id, root, .. } => {
            Some(NodeRequest::RegisterWorkspace {
                workspace_id: WorkspaceId::new(workspace_id).ok()?,
                root,
            })
        }
        AppAction::UnregisterWorkspace { workspace_id, .. } => {
            Some(NodeRequest::UnregisterWorkspace {
                workspace_id: WorkspaceId::new(workspace_id).ok()?,
            })
        }
        AppAction::CreateWorktree {
            source_workspace_id,
            workspace_id,
            target_root,
            branch,
            base,
            ..
        } => Some(NodeRequest::CreateWorktree {
            source_workspace_id: WorkspaceId::new(source_workspace_id).ok()?,
            workspace_id: WorkspaceId::new(workspace_id).ok()?,
            target_root,
            branch,
            base,
        }),
        AppAction::RemoveWorktree {
            source_workspace_id,
            target_root,
            ..
        } => Some(NodeRequest::RemoveWorktree {
            source_workspace_id: WorkspaceId::new(source_workspace_id).ok()?,
            target_root,
        }),
        AppAction::InspectWorkspace { workspace_id, .. } => {
            Some(NodeRequest::InspectWorkspace {
                workspace_id: WorkspaceId::new(workspace_id).ok()?,
            })
        }
        AppAction::Resync { after_sequence, .. } => Some(NodeRequest::Resync { after_sequence }),
    }
}

async fn ensure_controller(
    node: &mut NamedPipeNodeClient,
    controller_owned: &mut bool,
    connection_id: u64,
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
) -> bool {
    match node
        .request(NodeRequest::AcquireController { lease_ms: MAX_CONTROLLER_LEASE_MS })
        .await
    {
        Ok(NodeResponse::Controller { controller }) => {
            *controller_owned = owns_controller(&controller, connection_id);
            *controller_owned
        }
        Ok(_) => {
            *controller_owned = false;
            false
        }
        Err(error) => {
            *controller_owned = false;
            send_update(
                updates,
                WorkerUpdate::Notice(format!("{node_id}: controller {}", safe_client_error(&error))),
            )
            .await;
            false
        }
    }
}

async fn request_and_publish(
    node: &mut NamedPipeNodeClient,
    request: NodeRequest,
    updates: &mpsc::Sender<WorkerUpdate>,
    endpoint: &NodeEndpoint,
    controller_owned: &mut bool,
    connection_id: u64,
) -> Result<(), NodeClientError> {
    let source_workspace_id = match &request {
        NodeRequest::CreateWorktree { source_workspace_id, .. }
        | NodeRequest::RemoveWorktree { source_workspace_id, .. } => {
            Some(source_workspace_id.clone())
        }
        _ => None,
    };
    let mut refresh_workspace_id = source_workspace_id.clone();
    let response = node.request(request).await?;
    match response {
        NodeResponse::Snapshot { event_sequence, controller, snapshot } => {
            *controller_owned = owns_controller(&controller, connection_id);
            send_snapshot(updates, endpoint, snapshot, *controller_owned, event_sequence).await;
        }
        NodeResponse::Resync { event_sequence, snapshot, events } => {
            for envelope in events {
                publish_node_event(
                    envelope.event,
                    updates,
                    endpoint.expected_node_id.as_str(),
                    controller_owned,
                    connection_id,
                )
                .await;
            }
            send_snapshot(updates, endpoint, snapshot, *controller_owned, event_sequence).await;
        }
        NodeResponse::WorkspaceInspected { inspection } => {
            send_update(
                updates,
                WorkerUpdate::WorkspaceInspected {
                    node_id: endpoint.expected_node_id.to_string(),
                    inspection,
                },
            )
            .await;
        }
        NodeResponse::WorkspaceFileRead { .. } => {}
        NodeResponse::Controller { controller } => {
            *controller_owned = owns_controller(&controller, connection_id);
        }
        NodeResponse::SpawnAccepted { session } => {
            send_update(
                updates,
                WorkerUpdate::OpenSession(SessionAddress {
                    node_id: endpoint.expected_node_id.to_string(),
                    workspace_id: session.workspace_id.to_string(),
                    instance_id: session.session.instance_id.0,
                    generation: session.session.generation.0,
                }),
            )
            .await;
            send_update(
                updates,
                WorkerUpdate::Notice(format!(
                    "{}: spawn accepted in {} as #{}:{}",
                    endpoint.expected_node_id,
                    session.workspace_id,
                    session.session.instance_id.0,
                    session.session.generation.0
                )),
            )
            .await;
        }
        NodeResponse::SessionRecordUpdated { record } => {
            let name = record.display_name.clone();
            send_update(
                updates,
                WorkerUpdate::SessionRecordUpserted(project_managed_session(
                    endpoint.expected_node_id.as_str(),
                    record,
                )),
            )
            .await;
            send_update(
                updates,
                WorkerUpdate::Notice(format!(
                    "{}: session renamed to {name}",
                    endpoint.expected_node_id
                )),
            )
            .await;
        }
        NodeResponse::SessionRecordResumed { record, session } => {
            let name = record.display_name.clone();
            send_update(
                updates,
                WorkerUpdate::SessionRecordUpserted(project_managed_session(
                    endpoint.expected_node_id.as_str(),
                    record,
                )),
            )
            .await;
            send_update(
                updates,
                WorkerUpdate::OpenSession(SessionAddress {
                    node_id: endpoint.expected_node_id.to_string(),
                    workspace_id: session.workspace_id.to_string(),
                    instance_id: session.session.instance_id.0,
                    generation: session.session.generation.0,
                }),
            )
            .await;
            send_update(
                updates,
                WorkerUpdate::Notice(format!(
                    "{}: resumed {name}",
                    endpoint.expected_node_id
                )),
            )
            .await;
        }
        NodeResponse::SessionRecordForgotten { record_id } => {
            send_update(
                updates,
                WorkerUpdate::SessionRecordRemoved {
                    node_id: endpoint.expected_node_id.to_string(),
                    record_id: record_id.to_string(),
                },
            )
            .await;
            send_update(
                updates,
                WorkerUpdate::Notice(format!(
                    "{}: session record forgotten",
                    endpoint.expected_node_id
                )),
            )
            .await;
        }
        NodeResponse::WorkspaceRegistered { workspace } => {
            send_update(
                updates,
                WorkerUpdate::SelectWorkspace {
                    node_id: endpoint.expected_node_id.to_string(),
                    workspace_id: workspace.workspace_id.to_string(),
                },
            )
            .await;
            send_update(
                updates,
                WorkerUpdate::Notice(format!(
                    "{}: space {} registered",
                    endpoint.expected_node_id, workspace.workspace_id
                )),
            )
            .await;
        }
        NodeResponse::WorkspaceUnregistered { workspace_id } => {
            send_update(
                updates,
                WorkerUpdate::Notice(format!(
                    "{}: space {workspace_id} unregistered",
                    endpoint.expected_node_id
                )),
            )
            .await;
        }
        NodeResponse::WorktreeCreated { worktree, workspace } => {
            let response = NodeResponse::WorktreeCreated { worktree, workspace };
            let projection = project_worktree_response(
                &response,
                endpoint.expected_node_id.as_str(),
                source_workspace_id.as_ref(),
            )
            .expect("worktree response projection");
            refresh_workspace_id = projection.refresh_workspace_id;
            if let Some(workspace_id) = projection.selected_workspace_id {
                send_update(
                    updates,
                    WorkerUpdate::SelectWorkspace {
                        node_id: endpoint.expected_node_id.to_string(),
                        workspace_id,
                    },
                )
                .await;
            }
            send_update(updates, WorkerUpdate::Notice(projection.notice)).await;
        }
        NodeResponse::WorktreeRemoved { target_root, workspace_id } => {
            let response = NodeResponse::WorktreeRemoved { target_root, workspace_id };
            let projection = project_worktree_response(
                &response,
                endpoint.expected_node_id.as_str(),
                source_workspace_id.as_ref(),
            )
            .expect("worktree response projection");
            refresh_workspace_id = projection.refresh_workspace_id;
            send_update(updates, WorkerUpdate::Notice(projection.notice)).await;
        }
        NodeResponse::Accepted => {}
        NodeResponse::ShuttingDown => {
            send_update(
                updates,
                WorkerUpdate::State {
                    node_id: endpoint.expected_node_id.to_string(),
                    endpoint: endpoint.endpoint.clone(),
                    state: ConnectionState::Disconnected("node shutdown".to_owned()),
                },
            )
            .await;
        }
    }
    if let Some(workspace_id) = refresh_workspace_id {
        if let NodeResponse::WorkspaceInspected { inspection } = node
            .request(NodeRequest::InspectWorkspace { workspace_id })
            .await?
        {
            send_update(
                updates,
                WorkerUpdate::WorkspaceInspected {
                    node_id: endpoint.expected_node_id.to_string(),
                    inspection,
                },
            )
            .await;
        }
    }
    Ok(())
}

struct WorktreeResponseProjection {
    selected_workspace_id: Option<String>,
    refresh_workspace_id: Option<WorkspaceId>,
    notice: String,
}

fn project_worktree_response(
    response: &NodeResponse,
    node_id: &str,
    source_workspace_id: Option<&WorkspaceId>,
) -> Option<WorktreeResponseProjection> {
    match response {
        NodeResponse::WorktreeCreated { worktree, workspace } => Some(WorktreeResponseProjection {
            selected_workspace_id: Some(workspace.workspace_id.to_string()),
            refresh_workspace_id: Some(workspace.workspace_id.clone()),
            notice: format!("{node_id}: worktree {} created", host_path_display(&worktree.path)),
        }),
        NodeResponse::WorktreeRemoved { target_root, workspace_id } => Some(WorktreeResponseProjection {
            selected_workspace_id: None,
            refresh_workspace_id: source_workspace_id
                .filter(|source| workspace_id.as_ref() != Some(*source))
                .cloned(),
            notice: format!(
                "{node_id}: worktree {} removed{}",
                host_path_display(target_root),
                workspace_id
                    .as_ref()
                    .map(|workspace_id| format!(" ({workspace_id})"))
                    .unwrap_or_default()
            ),
        }),
        _ => None,
    }
}

async fn await_input_completion(
    node: &mut NamedPipeNodeClient,
    address: &SessionAddress,
    updates: &mpsc::Sender<WorkerUpdate>,
    endpoint: &NodeEndpoint,
    controller_owned: &mut bool,
    connection_id: u64,
) -> Result<(), NodeClientError> {
    let started = Instant::now();
    loop {
        sleep(Duration::from_millis(10)).await;
        let response = node.request(NodeRequest::Snapshot).await?;
        let NodeResponse::Snapshot { event_sequence, controller, snapshot } = response else {
            continue;
        };
        *controller_owned = owns_controller(&controller, connection_id);
        let complete = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id.as_str() == address.workspace_id)
            .and_then(|workspace| workspace.sessions.iter().find(|session| {
                session.instance_id.0 == address.instance_id
                    && session.generation.0 == address.generation
            }))
            .is_some_and(|session| session.pending_input.is_none() && session.pending_operation.is_none());
        send_snapshot(updates, endpoint, snapshot, *controller_owned, event_sequence).await;
        if complete {
            return Ok(());
        }
        if started.elapsed() >= INPUT_COMPLETION_TIMEOUT {
            send_update(
                updates,
                WorkerUpdate::Notice(format!(
                    "{}: PTY input completion timed out; later payloads remain queued",
                    endpoint.expected_node_id
                )),
            )
            .await;
            return Ok(());
        }
    }
}

async fn publish_pending_events(
    node: &mut NamedPipeNodeClient,
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
    controller_owned: &mut bool,
    connection_id: u64,
) {
    while let Some(envelope) = node.take_event() {
        publish_node_event(envelope.event, updates, node_id, controller_owned, connection_id).await;
    }
}

async fn publish_node_event(
    event: NodeEvent,
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
    controller_owned: &mut bool,
    connection_id: u64,
) {
    match event {
        NodeEvent::ControllerChanged { controller } => {
            *controller_owned = owns_controller(&controller, connection_id);
            if !*controller_owned {
                send_update(updates, WorkerUpdate::Notice(format!("{node_id}: controller held elsewhere"))).await;
            }
        }
        NodeEvent::ResyncRequired { .. } => {
            send_update(
                updates,
                WorkerUpdate::State {
                    node_id: node_id.to_owned(),
                    endpoint: String::new(),
                    state: ConnectionState::Resyncing,
                },
            )
            .await;
        }
        NodeEvent::Control { address, event } => {
            if let Some(notice) = safe_control_notice(node_id, &address, &event) {
                send_update(updates, WorkerUpdate::Notice(notice)).await;
            }
        }
        NodeEvent::SessionRecordUpserted { record } => {
            send_update(
                updates,
                WorkerUpdate::SessionRecordUpserted(project_managed_session(node_id, record)),
            )
            .await;
        }
        NodeEvent::SessionRecordRemoved { record_id } => {
            send_update(
                updates,
                WorkerUpdate::SessionRecordRemoved {
                    node_id: node_id.to_owned(),
                    record_id: record_id.to_string(),
                },
            )
            .await;
        }
        NodeEvent::WorkspaceAdded { .. } | NodeEvent::WorkspaceRemoved { .. } => {}
    }
}

fn apply_update(app: &mut App, c2: &mut C2ApplyState, update: WorkerUpdate) {
    match update {
        WorkerUpdate::State { node_id, endpoint, state } => {
            let endpoint = if endpoint.is_empty() {
                app.nodes
                    .iter()
                    .find(|node| node.node_id == node_id)
                    .map(|node| node.endpoint.clone())
                    .unwrap_or_default()
            } else {
                endpoint
            };
            app.set_node_connection(&node_id, &endpoint, state);
        }
        WorkerUpdate::TopologyNodeRemoved { node_id } => {
            c2.watermarks.remove(&node_id);
            app.remove_topology_node(&node_id);
        }
        WorkerUpdate::Snapshot {
            expected_node_id,
            endpoint,
            snapshot,
            controller_owned,
            event_sequence,
        } => app.upsert_node(project_node(
            expected_node_id,
            endpoint,
            snapshot,
            controller_owned,
            event_sequence,
        )),
        WorkerUpdate::C2Snapshot {
            expected_node_id,
            endpoint,
            incarnation_id,
            snapshot,
            event_sequence,
        } => {
            let cursor = gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: event_sequence,
            };
            if c2.accept_snapshot(&expected_node_id, cursor) {
                app.upsert_node(project_c2_node(
                    expected_node_id,
                    endpoint,
                    snapshot,
                    event_sequence,
                ));
            }
        }
        WorkerUpdate::C2Event { node_id, cursor, event } => {
            if !c2.accept_event(&node_id, cursor) {
                return;
            }
            match event {
                C2EventUpdate::SessionRecordUpserted(record) => {
                    app.upsert_managed_session(record)
                }
                C2EventUpdate::SessionRecordRemoved { record_id } => {
                    app.remove_managed_session(&node_id, &record_id)
                }
                C2EventUpdate::Notice(notice) => app.notice = Some(notice),
                C2EventUpdate::ResyncRequired => {
                    let endpoint = app.nodes.iter()
                        .find(|node| node.node_id == node_id)
                        .map(|node| node.endpoint.clone())
                        .unwrap_or_default();
                    app.set_node_connection(&node_id, &endpoint, ConnectionState::Resyncing);
                }
                C2EventUpdate::Ignored => {}
            }
            if let Some(node) = app.nodes.iter_mut().find(|node| node.node_id == node_id) {
                node.event_sequence = cursor.sequence;
            }
        }
        WorkerUpdate::OpenSession(address) => app.request_open(address),
        WorkerUpdate::SelectWorkspace { node_id, workspace_id } => {
            app.request_space_selection(node_id, workspace_id)
        }
        WorkerUpdate::WorkspaceInspected { node_id, inspection } => {
            app.apply_workspace_inspection(node_id, inspection)
        }
        WorkerUpdate::WorkspaceInspectionFailed {
            node_id,
            workspace_id,
            message,
        } => app.fail_workspace_inspection(node_id, workspace_id, message),
        WorkerUpdate::SessionRecordUpserted(record) => app.upsert_managed_session(record),
        WorkerUpdate::SessionRecordRemoved { node_id, record_id } => {
            app.remove_managed_session(&node_id, &record_id)
        }
        WorkerUpdate::Notice(notice) => app.notice = Some(notice),
    }
}

fn project_c2_node(
    expected_node_id: String,
    endpoint: String,
    snapshot: C2NodeSnapshot,
    event_sequence: u64,
) -> NodeView {
    let providers = snapshot
        .enabled_providers
        .iter()
        .map(|provider| ProviderInventory {
            provider: project_provider(*provider),
            enabled: true,
        })
        .collect::<Vec<_>>();
    let node_id = snapshot.node_id.to_string();
    debug_assert_eq!(node_id, expected_node_id);
    let workspaces = snapshot
        .workspaces
        .into_iter()
        .map(|workspace| {
            let workspace_id = workspace.workspace_id.to_string();
            let label = workspace_id.clone();
            let sessions = workspace
                .sessions
                .into_iter()
                .filter_map(|session| project_c2_session(&node_id, &workspace_id, session))
                .collect();
            WorkspaceView {
                workspace_id,
                label,
                canonical_root: workspace.canonical_root,
                providers: providers.clone(),
                sessions,
            }
        })
        .collect();
    let session_records = snapshot
        .session_records
        .into_iter()
        .map(|record| project_c2_managed_session(&node_id, record))
        .collect();
    NodeView {
        node_id,
        endpoint,
        connection: ConnectionState::Connected,
        controller_owned: true,
        event_sequence,
        workspaces,
        session_records,
    }
}

fn project_node(
    expected_node_id: String,
    endpoint: String,
    snapshot: NodeSnapshot,
    controller_owned: bool,
    event_sequence: u64,
) -> NodeView {
    let providers = snapshot
        .enabled_providers
        .iter()
        .map(|provider| ProviderInventory {
            provider: project_provider(*provider),
            enabled: true,
        })
        .collect::<Vec<_>>();
    let node_id = snapshot.node_id.to_string();
    debug_assert_eq!(node_id, expected_node_id);
    let workspaces = snapshot
        .workspaces
        .into_iter()
        .map(|workspace| {
            let workspace_id = workspace.workspace_id.to_string();
            let label = workspace_id.clone();
            let sessions = workspace
                .sessions
                .into_iter()
                .filter_map(|session| project_session(&node_id, &workspace_id, session))
                .collect();
            WorkspaceView {
                workspace_id,
                label,
                canonical_root: workspace.canonical_root,
                providers: providers.clone(),
                sessions,
            }
        })
        .collect();
    let session_records = snapshot
        .session_records
        .into_iter()
        .map(|record| project_managed_session(&node_id, record))
        .collect();
    NodeView {
        node_id,
        endpoint,
        connection: ConnectionState::Connected,
        controller_owned,
        event_sequence,
        workspaces,
        session_records,
    }
}

fn project_managed_session(node_id: &str, record: ManagedSessionRecord) -> ManagedSessionView {
    ManagedSessionView {
        node_id: node_id.to_owned(),
        record_id: record.record_id.to_string(),
        display_name: record.display_name,
        provider: project_provider(record.provider),
        mode: record.mode,
        state: record.state,
        workspace_id: record.workspace_id.to_string(),
        canonical_root: Some(record.canonical_root),
        has_provider_session_identity: record.provider_session.is_some(),
        active_session: record.active_session.map(|address| SessionAddress {
            node_id: node_id.to_owned(),
            workspace_id: address.workspace_id.to_string(),
            instance_id: address.session.instance_id.0,
            generation: address.session.generation.0,
        }),
        last_error: record.last_error,
    }
}

fn project_c2_managed_session(
    node_id: &str,
    record: C2ManagedSessionRecord,
) -> ManagedSessionView {
    ManagedSessionView {
        node_id: node_id.to_owned(),
        record_id: record.record_id.to_string(),
        display_name: record.display_name,
        provider: project_provider(record.provider),
        mode: record.mode,
        state: record.state,
        workspace_id: record.workspace_id.to_string(),
        canonical_root: None,
        has_provider_session_identity: record.provider_identity_present,
        active_session: record.active_session.map(|address| SessionAddress {
            node_id: node_id.to_owned(),
            workspace_id: address.workspace_id.to_string(),
            instance_id: address.session.instance_id.0,
            generation: address.session.generation.0,
        }),
        last_error: None,
    }
}

fn project_c2_workspace_inspection(
    inspection: C2WorkspaceInspection,
) -> gate4agent_node_protocol::WorkspaceInspection {
    gate4agent_node_protocol::WorkspaceInspection {
        workspace_id: inspection.workspace_id,
        entries: inspection.entries,
        tree_truncated: inspection.tree_truncated,
        git: gate4agent_node_protocol::GitSnapshot {
            is_repository: inspection.git.is_repository,
            branch: inspection.git.branch,
            status: inspection.git.status,
            recent_commits: inspection.git.recent_commits,
            worktrees: inspection.git.worktrees.into_iter().map(|worktree| {
                gate4agent_node_protocol::GitWorktreeSnapshot {
                    path: worktree.path,
                    head: worktree.head,
                    branch: worktree.branch,
                    is_bare: worktree.is_bare,
                    is_main: worktree.is_main,
                    locked: worktree.locked,
                    lock_reason: worktree.locked.then(|| "locked".to_owned()),
                    prunable: worktree.prunable,
                    prunable_reason: worktree.prunable.then(|| "prunable".to_owned()),
                    workspace_id: worktree.workspace_id,
                }
            }).collect(),
            truncated: inspection.git.truncated,
            diagnostic: inspection.git.diagnostic_present
                .then(|| "git inspection unavailable".to_owned()),
        },
    }
}

fn project_c2_session(
    node_id: &str,
    workspace_id: &str,
    session: C2SessionSnapshot,
) -> Option<SessionView> {
    let provider = session.agent_id.as_str().parse().ok()?;
    if !supports_tui_transport(session.transport) {
        return None;
    }
    let lifecycle = project_c2_lifecycle(&session.status);
    let terminal_cursor = session.terminal_frame.as_ref()
        .map(|frame| (frame.cursor_row, frame.cursor_column));
    let terminal_formatted = session.terminal_frame.as_ref()
        .map(|frame| frame.formatted.clone())
        .unwrap_or_default();
    let terminal_scrollback = session.terminal_frame.as_ref()
        .map(|frame| frame.scrollback_formatted.clone())
        .unwrap_or_default();
    let terminal_alternate_screen = session.terminal_frame.as_ref()
        .is_some_and(|frame| frame.alternate_screen);
    let terminal_mouse_protocol_enabled = session.terminal_frame.as_ref()
        .is_some_and(|frame| frame.mouse_protocol_enabled);
    let terminal_mouse_protocol_encoding = session.terminal_frame.as_ref()
        .map(|frame| frame.mouse_protocol_encoding)
        .unwrap_or(TerminalMouseProtocolEncoding::Default);
    Some(SessionView {
        address: SessionAddress {
            node_id: node_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            instance_id: session.instance_id.0,
            generation: session.generation.0,
        },
        provider,
        status: c2_status_label(&session.status),
        running: lifecycle.running,
        stoppable: lifecycle.stoppable,
        removable: lifecycle.removable,
        restartable: lifecycle.restartable,
        attention: matches!(
            session.provider_activity,
            ProviderActivity::WaitingForInput | ProviderActivity::Blocked
        ) || session.provider_interaction_pending,
        has_provider_session_identity: session.provider_identity_present,
        terminal_formatted,
        terminal_scrollback,
        terminal_alternate_screen,
        terminal_mouse_protocol_enabled,
        terminal_mouse_protocol_encoding,
        terminal_cursor,
    })
}

fn project_session(
    node_id: &str,
    workspace_id: &str,
    session: SessionSnapshot,
) -> Option<SessionView> {
    let provider = session.agent_id.as_str().parse().ok()?;
    if !supports_tui_transport(session.transport) {
        return None;
    }
    let lifecycle = project_lifecycle(&session.status);
    let attention = matches!(
        session.provider.activity,
        ProviderActivity::WaitingForInput | ProviderActivity::Blocked
    ) || !session.provider.interactions.is_empty();
    let terminal_cursor = session
        .terminal_frame
        .as_ref()
        .map(|frame| (frame.cursor_row, frame.cursor_column));
    let terminal_formatted = session
        .terminal_frame
        .as_ref()
        .map(|frame| frame.formatted.clone())
        .unwrap_or_default();
    let terminal_scrollback = session
        .terminal_frame
        .as_ref()
        .map(|frame| frame.scrollback_formatted.clone())
        .unwrap_or_default();
    let terminal_alternate_screen = session
        .terminal_frame
        .as_ref()
        .is_some_and(|frame| frame.alternate_screen);
    let terminal_mouse_protocol_enabled = session
        .terminal_frame
        .as_ref()
        .is_some_and(|frame| frame.mouse_protocol_enabled);
    let terminal_mouse_protocol_encoding = session
        .terminal_frame
        .as_ref()
        .map(|frame| frame.mouse_protocol_encoding)
        .unwrap_or(TerminalMouseProtocolEncoding::Default);
    Some(SessionView {
        address: SessionAddress {
            node_id: node_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            instance_id: session.instance_id.0,
            generation: session.generation.0,
        },
        provider,
        status: status_label(&session.status),
        running: lifecycle.running,
        stoppable: lifecycle.stoppable,
        removable: lifecycle.removable,
        restartable: lifecycle.restartable,
        attention,
        has_provider_session_identity: session.provider.session.is_some(),
        terminal_formatted,
        terminal_scrollback,
        terminal_alternate_screen,
        terminal_mouse_protocol_enabled,
        terminal_mouse_protocol_encoding,
        terminal_cursor,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProjectedLifecycle {
    running: bool,
    stoppable: bool,
    removable: bool,
    restartable: bool,
}

fn project_lifecycle(status: &SessionStatus) -> ProjectedLifecycle {
    match status {
        SessionStatus::Registered => ProjectedLifecycle {
            running: false,
            stoppable: true,
            removable: false,
            restartable: false,
        },
        SessionStatus::Starting | SessionStatus::Running => ProjectedLifecycle {
            running: true,
            stoppable: true,
            removable: false,
            restartable: false,
        },
        SessionStatus::Stopping => ProjectedLifecycle {
            running: false,
            stoppable: true,
            removable: false,
            restartable: false,
        },
        SessionStatus::Exited { .. } | SessionStatus::Failed { .. } => ProjectedLifecycle {
            running: false,
            stoppable: false,
            removable: true,
            restartable: true,
        },
    }
}

fn project_c2_lifecycle(status: &C2SessionStatus) -> ProjectedLifecycle {
    match status {
        C2SessionStatus::Registered => ProjectedLifecycle {
            running: false,
            stoppable: true,
            removable: false,
            restartable: false,
        },
        C2SessionStatus::Starting | C2SessionStatus::Running => ProjectedLifecycle {
            running: true,
            stoppable: true,
            removable: false,
            restartable: false,
        },
        C2SessionStatus::Stopping => ProjectedLifecycle {
            running: false,
            stoppable: true,
            removable: false,
            restartable: false,
        },
        C2SessionStatus::Exited { .. } | C2SessionStatus::Failed => ProjectedLifecycle {
            running: false,
            stoppable: false,
            removable: true,
            restartable: true,
        },
    }
}

fn supports_tui_transport(transport: TransportKind) -> bool {
    transport == TransportKind::Pty
}

fn safe_control_notice(
    node_id: &str,
    address: &WireSessionAddress,
    event: &ControlEvent,
) -> Option<String> {
    let class = match &event.event {
        ControlEventKind::CommandRejected { .. } => "command rejected",
        ControlEventKind::InputFailed { .. } => "input failed",
        ControlEventKind::ResizeFailed { .. } => "resize failed",
        ControlEventKind::ForegroundFailed { .. } => "foreground refresh failed",
        ControlEventKind::CapabilityProbeFailed { .. } => "capability probe failed",
        ControlEventKind::HistoryFailed { .. } => "history failed",
        ControlEventKind::ResumeDenied { .. } => "resume denied",
        ControlEventKind::ResumeFailed { .. } => "resume failed",
        ControlEventKind::TerminalStale { .. } => "terminal snapshot stale",
        ControlEventKind::InteractionResolutionFailed { .. } => "interaction resolution failed",
        ControlEventKind::Resumed { .. } => "session resumed",
        ControlEventKind::Exited { forced, .. } => {
            if *forced { "session force-stopped" } else { "session exited" }
        }
        ControlEventKind::Failed { .. } => "session failed",
        _ => return None,
    };
    Some(format!(
        "{node_id}/{} #{}:{}: {class}",
        address.workspace_id,
        address.session.instance_id.0,
        address.session.generation.0
    ))
}

fn safe_c2_control_notice(
    node_id: &str,
    address: &WireSessionAddress,
    event: &C2ControlEvent,
) -> Option<String> {
    let class = match &event.event {
        C2ControlEventKind::CommandRejected => "command rejected",
        C2ControlEventKind::InputFailed => "input failed",
        C2ControlEventKind::ResizeFailed => "resize failed",
        C2ControlEventKind::ForegroundFailed => "foreground refresh failed",
        C2ControlEventKind::CapabilityProbeFailed => "capability probe failed",
        C2ControlEventKind::HistoryFailed => "history failed",
        C2ControlEventKind::ResumeDenied => "resume denied",
        C2ControlEventKind::ResumeFailed => "resume failed",
        C2ControlEventKind::TerminalStale => "terminal snapshot stale",
        C2ControlEventKind::InteractionResolutionFailed => "interaction resolution failed",
        C2ControlEventKind::Resumed => "session resumed",
        C2ControlEventKind::Exited { forced } => {
            if *forced { "session force-stopped" } else { "session exited" }
        }
        C2ControlEventKind::Failed => "session failed",
        _ => return None,
    };
    Some(format!(
        "{node_id}/{} #{}:{}: {class}",
        address.workspace_id,
        address.session.instance_id.0,
        address.session.generation.0
    ))
}

fn safe_client_error(error: &NodeClientError) -> &'static str {
    match error {
        NodeClientError::Node(failure) => safe_node_failure_code(failure.code),
        NodeClientError::Io(_) => "node transport unavailable",
        NodeClientError::Frame(_) => "node frame invalid or timed out",
        NodeClientError::Protocol(_) => "node protocol mismatch",
        NodeClientError::AuthenticationTimedOut => "authentication timed out",
        NodeClientError::Authentication(_) => "authentication unavailable",
        NodeClientError::UnsupportedCapability(_) => "required capability unavailable",
        NodeClientError::RequestIdExhausted => "request counter exhausted",
    }
}

fn safe_node_failure_code(code: NodeFailureCode) -> &'static str {
    match code {
        NodeFailureCode::InvalidRequest => "invalid request",
        NodeFailureCode::UnsupportedCapability => "required capability unavailable",
        NodeFailureCode::Unauthorized => "authentication rejected",
        NodeFailureCode::ObserverReadOnly => "operator access required",
        NodeFailureCode::ControllerBusy => "controller busy",
        NodeFailureCode::ControllerRequired => "controller required",
        NodeFailureCode::UnknownWorkspace => "workspace unavailable",
        NodeFailureCode::InvalidRepositoryPath => "repository path invalid",
        NodeFailureCode::RepositoryFileNotFound => "repository file unavailable",
        NodeFailureCode::RepositoryFileNotRegular => "repository path is not a regular file",
        NodeFailureCode::RepositoryPathUnsafe => "repository path is unsafe",
        NodeFailureCode::RepositoryFileReadTimedOut => "repository file read timed out",
        NodeFailureCode::RepositoryFileReadFailed => "repository file read failed",
        NodeFailureCode::InvalidWorkspaceRoot => "workspace root invalid",
        NodeFailureCode::DuplicateWorkspaceId => "workspace ID already registered",
        NodeFailureCode::DuplicateWorkspaceRoot => "workspace root already registered",
        NodeFailureCode::WorkspaceBusy => "workspace has managed sessions",
        NodeFailureCode::LastWorkspace => "node must retain one workspace",
        NodeFailureCode::NotGitRepository => "workspace is not a git repository",
        NodeFailureCode::WorktreeConflict => "worktree path or branch conflicts",
        NodeFailureCode::WorktreeProtected => "protected worktree cannot be removed",
        NodeFailureCode::WorktreeDirty => "worktree has uncommitted changes",
        NodeFailureCode::WorktreeLocked => "worktree is locked",
        NodeFailureCode::UnknownSession => "session unavailable",
        NodeFailureCode::UnknownSessionRecord => "managed session record unavailable",
        NodeFailureCode::SessionRecordNotResumable => "managed session cannot resume",
        NodeFailureCode::SessionRecordBusy => "managed session is already live",
        NodeFailureCode::SessionRecordConflict => "managed session identity conflicts",
        NodeFailureCode::SessionWorkspaceMismatch => "session/workspace mismatch",
        NodeFailureCode::StaleGeneration => "stale session generation",
        NodeFailureCode::BackendBusy => "node backend busy",
        NodeFailureCode::BackendDisconnected => "node backend disconnected",
        NodeFailureCode::BackendOperationFailed => "node backend operation failed",
        NodeFailureCode::ShuttingDown => "node shutting down",
    }
}

fn is_transport_error(error: &NodeClientError) -> bool {
    !matches!(
        error,
        NodeClientError::Node(_) | NodeClientError::UnsupportedCapability(_)
    )
}

async fn disconnected(updates: &mpsc::Sender<WorkerUpdate>, endpoint: &NodeEndpoint, reason: &str) {
    crate::diagnostics::record_runtime(RuntimeDiagnostic::NodeDisconnected);
    send_update(
        updates,
        WorkerUpdate::State {
            node_id: endpoint.expected_node_id.to_string(),
            endpoint: endpoint.endpoint.clone(),
            state: ConnectionState::Disconnected(reason.to_owned()),
        },
    )
    .await;
}

fn reject_disconnected_commands(
    commands: &mut mpsc::Receiver<AppAction>,
    updates: &mpsc::Sender<WorkerUpdate>,
    node_id: &str,
) {
    let mut rejected = false;
    while commands.try_recv().is_ok() {
        rejected = true;
    }
    if rejected {
        let _ = updates.try_send(WorkerUpdate::Notice(format!(
            "{node_id}: disconnected; queued actions cancelled"
        )));
    }
}

async fn send_snapshot(
    updates: &mpsc::Sender<WorkerUpdate>,
    endpoint: &NodeEndpoint,
    snapshot: NodeSnapshot,
    controller_owned: bool,
    event_sequence: u64,
) {
    send_update(
        updates,
        WorkerUpdate::Snapshot {
            expected_node_id: endpoint.expected_node_id.to_string(),
            endpoint: endpoint.endpoint.clone(),
            snapshot,
            controller_owned,
            event_sequence,
        },
    )
    .await;
}

async fn send_update(updates: &mpsc::Sender<WorkerUpdate>, update: WorkerUpdate) {
    let _ = updates.send(update).await;
}

fn owns_controller(controller: &Option<ControllerState>, connection_id: u64) -> bool {
    controller
        .as_ref()
        .is_some_and(|state| state.connection_id == connection_id)
}

fn wire_address(address: &SessionAddress) -> Result<WireSessionAddress, String> {
    Ok(WireSessionAddress {
        workspace_id: WorkspaceId::new(address.workspace_id.clone()).map_err(|error| error.to_string())?,
        session: SessionKey {
            instance_id: gate4agent_types::AgentInstanceId(address.instance_id),
            generation: gate4agent_types::SessionGeneration(address.generation),
        },
    })
}

fn map_provider(provider: Provider) -> AgentProvider {
    match provider {
        Provider::Claude => AgentProvider::Claude,
        Provider::Codex => AgentProvider::Codex,
        Provider::Kimi => AgentProvider::Kimi,
    }
}

fn project_provider(provider: AgentProvider) -> Provider {
    match provider {
        AgentProvider::Claude => Provider::Claude,
        AgentProvider::Codex => Provider::Codex,
        AgentProvider::Kimi => Provider::Kimi,
    }
}

fn status_label(status: &SessionStatus) -> String {
    match status {
        SessionStatus::Registered => "registered".to_owned(),
        SessionStatus::Starting => "starting".to_owned(),
        SessionStatus::Running => "running".to_owned(),
        SessionStatus::Stopping => "stopping".to_owned(),
        SessionStatus::Exited { exit_code } => format!(
            "exited({})",
            exit_code.map_or_else(|| "unknown".to_owned(), |code| code.to_string())
        ),
        SessionStatus::Failed { .. } => "failed".to_owned(),
    }
}

fn c2_status_label(status: &C2SessionStatus) -> String {
    match status {
        C2SessionStatus::Registered => "registered".to_owned(),
        C2SessionStatus::Starting => "starting".to_owned(),
        C2SessionStatus::Running => "running".to_owned(),
        C2SessionStatus::Stopping => "stopping".to_owned(),
        C2SessionStatus::Exited { exit_code } => format!(
            "exited({})",
            exit_code.map_or_else(|| "unknown".to_owned(), |code| code.to_string())
        ),
        C2SessionStatus::Failed => "failed".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_node_protocol::OpaqueHostPath;
    use crate::app::{
        DragState, GridPane, GridPaneLayout, HitRegion, HitTarget, PtyColorMode, SessionTab,
        SurfaceMode,
    };

    fn host_path(value: impl Into<String>) -> OpaqueHostPath {
        OpaqueHostPath::utf8(value.into()).unwrap()
    }

    fn cursor_app() -> App {
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 1,
            generation: 1,
        };
        let mut app = App::default();
        app.nodes.push(NodeView {
            node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            connection: ConnectionState::Connected,
            controller_owned: true,
            event_sequence: 1,
            session_records: Vec::new(),
            workspaces: vec![WorkspaceView {
                workspace_id: "workspace-a".to_owned(),
                label: "nemo".to_owned(),
                canonical_root: host_path(r"C:\work\nemo"),
                providers: Vec::new(),
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
                    terminal_formatted: Vec::new(),
                    terminal_scrollback: Vec::new(),
                    terminal_alternate_screen: false,
                    terminal_mouse_protocol_enabled: false,
                    terminal_mouse_protocol_encoding: TerminalMouseProtocolEncoding::Default,
                    terminal_cursor: Some((99, 99)),
                }],
            }],
        });
        app.tabs.push(SessionTab { address });
        app.focus = crate::app::Focus::Viewport;
        app.layout.viewport = uzor_tui::Rect::new(26, 1, 10, 5);
        app
    }

    #[test]
    fn initial_viewport_matches_fixed_sidebar_and_single_tab_row() {
        assert_eq!(initial_viewport_size(100, 24), TerminalSize { rows: 23, columns: 74 });
        assert_eq!(initial_viewport_size(48, 12), TerminalSize { rows: 11, columns: 24 });
    }

    #[test]
    fn c2_workspace_projection_uses_id_label_and_preserves_foreign_path_token() {
        let opaque = OpaqueHostPath::unix_bytes(vec![b'/', b's', b'r', b'v', b'/', 0xff, b'/', b'.', b'.']).unwrap();
        let view = project_c2_node(
            "node-a".to_owned(),
            "remote".to_owned(),
            C2NodeSnapshot {
                node_id: NodeId::new("node-a").unwrap(),
                enabled_providers: Vec::new(),
                workspaces: vec![gate4agent_c2_protocol::C2WorkspaceSnapshot {
                    workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                    canonical_root: opaque.clone(),
                    sessions: Vec::new(),
                }],
                session_records: Vec::new(),
            },
            7,
        );

        assert_eq!(view.workspaces[0].label, "workspace-a");
        assert_eq!(view.workspaces[0].canonical_root, opaque);
    }

    #[test]
    fn c2_shared_command_route_accepts_actions_for_discovered_nodes() {
        let mut app = cursor_app();
        let address = app.tabs[0].address.clone();
        let (sender, mut receiver) = mpsc::channel(1);
        let commands = BTreeMap::from([(C2_COMMAND_ROUTE.to_owned(), sender)]);

        send_operator_action(
            &mut app,
            &commands,
            AppAction::Resize {
                address: address.clone(),
                rows: 23,
                cols: 79,
            },
        );

        assert!(matches!(
            receiver.try_recv(),
            Ok(AppAction::Resize { address: routed, rows: 23, cols: 79 }) if routed == address
        ));
    }

    #[tokio::test]
    async fn c2_topology_reconciles_offline_parked_online_incarnation_and_removal() {
        use gate4agent_c2_protocol::C2TopologyNode;
        use gate4agent_node_protocol::NodeIncarnationId;

        let node_id = NodeId::new("node-a").unwrap();
        let endpoint = r"\\.\pipe\node-a".to_owned();
        let offline = C2Topology {
            nodes: vec![C2TopologyNode {
                node_id: node_id.clone(),
                endpoint: endpoint.clone(),
                transport: NodeTransportState::Offline,
                current_incarnation_id: None,
            }],
        };
        let mut routes = BTreeMap::new();
        let mut endpoints = BTreeMap::new();
        let (updates, mut receiver) = mpsc::channel(8);

        let snapshots = reconcile_c2_topology(
            &offline,
            &mut routes,
            &mut endpoints,
            &updates,
        )
        .await;
        assert!(snapshots.is_empty());
        assert!(matches!(
            receiver.recv().await,
            Some(WorkerUpdate::State {
                node_id: ref observed,
                state: ConnectionState::Disconnected(_),
                ..
            }) if observed == "node-a"
        ));

        let parked = C2Topology {
            nodes: vec![C2TopologyNode {
                node_id: node_id.clone(),
                endpoint: endpoint.clone(),
                transport: NodeTransportState::Parked,
                current_incarnation_id: None,
            }],
        };
        let snapshots = reconcile_c2_topology(
            &parked,
            &mut routes,
            &mut endpoints,
            &updates,
        )
        .await;
        assert!(snapshots.is_empty());
        assert!(routes.is_empty());
        assert_eq!(endpoints.get("node-a"), Some(&endpoint));
        assert!(matches!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty)));

        let incarnation = NodeIncarnationId::from_bytes([7; 16]);
        let online = C2Topology {
            nodes: vec![C2TopologyNode {
                node_id: node_id.clone(),
                endpoint: endpoint.clone(),
                transport: NodeTransportState::Online,
                current_incarnation_id: Some(incarnation),
            }],
        };
        let snapshots = reconcile_c2_topology(
            &online,
            &mut routes,
            &mut endpoints,
            &updates,
        )
        .await;
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].expected_incarnation_id, incarnation);
        assert!(matches!(
            receiver.recv().await,
            Some(WorkerUpdate::State {
                state: ConnectionState::Connecting,
                ..
            })
        ));

        let snapshots = reconcile_c2_topology(
            &C2Topology { nodes: Vec::new() },
            &mut routes,
            &mut endpoints,
            &updates,
        )
        .await;
        assert!(snapshots.is_empty());
        assert!(routes.is_empty());
        assert!(matches!(
            receiver.recv().await,
            Some(WorkerUpdate::TopologyNodeRemoved { node_id }) if node_id == "node-a"
        ));
    }

    #[test]
    fn provider_shortcuts_preserve_ctrl_g_alt_bytes_and_operator_escape() {
        let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(map_key(ctrl_g), Some(UiKey::Ctrl('g')));
        let operator_escape = KeyEvent::new(
            KeyCode::Char('G'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(map_key(operator_escape), Some(UiKey::OperatorEscape));
        for ch in ['b', 'f', 'p', 't', 'y'] {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::ALT);
            assert_eq!(map_key(key), Some(UiKey::TerminalBytes(vec![0x1b, ch as u8])));
        }
    }

    #[test]
    fn cursor_is_visible_only_for_focused_pty_and_clamped() {
        let mut app = cursor_app();
        assert_eq!(visible_cursor_position(&app), Some((35, 5)));
        app.nodes[0].workspaces[0].sessions[0].running = false;
        assert_eq!(visible_cursor_position(&app), None);
        app.nodes[0].workspaces[0].sessions[0].running = true;
        app.focus = crate::app::Focus::Tabs;
        assert_eq!(visible_cursor_position(&app), None);
        app.focus = crate::app::Focus::Viewport;
        let address = app.tabs[0].address.clone();
        app.terminal_scroll_offsets.insert(address, 1);
        assert_eq!(visible_cursor_position(&app), None);
    }

    #[test]
    fn cursor_uses_focused_grid_pane_viewport() {
        let mut app = cursor_app();
        let address = app.tabs[0].address.clone();
        app.surface_mode = SurfaceMode::Grid;
        app.grid.panes.push(GridPane { address });
        app.grid.focused = 0;
        app.layout.grid_panes.push(GridPaneLayout {
            pane_index: 0,
            frame: uzor_tui::Rect::new(39, 8, 10, 6),
            header: uzor_tui::Rect::new(40, 9, 8, 1),
            viewport: uzor_tui::Rect::new(40, 10, 7, 3),
        });

        assert_eq!(visible_cursor_position(&app), Some((46, 12)));
    }

    #[test]
    fn mouse_down_drag_up_moves_a_tab_to_grid_once() {
        let mut app = cursor_app();
        app.layout.hits.push(HitRegion {
            rect: uzor_tui::Rect::new(1, 0, 8, 1),
            target: HitTarget::Tab(0),
        });
        app.layout.grid_drop = uzor_tui::Rect::new(20, 1, 10, 8);

        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(map_mouse(&mut app, down), AppAction::None);
        assert!(matches!(app.drag_state, Some(DragState::SessionChip { moved: false, .. })));

        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 21,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(map_mouse(&mut app, drag), AppAction::None);
        assert!(matches!(app.drag_state, Some(DragState::SessionChip { moved: true, .. })));

        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 21,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(map_mouse(&mut app, up), AppAction::None);
        assert!(app.drag_state.is_none());
        assert_eq!(app.surface_mode, SurfaceMode::Grid);
        assert_eq!(app.grid.panes.len(), 1);
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn terminal_size_diff_tracks_visible_sessions_without_repeat_storms() {
        let first = cursor_app().tabs[0].address.clone();
        let mut second = first.clone();
        second.instance_id = 2;
        let mut third = first.clone();
        third.instance_id = 3;
        let mut last = BTreeMap::new();

        let initial = diff_terminal_sizes(
            vec![(first.clone(), 20, 80), (second.clone(), 20, 80)],
            &mut last,
        );
        assert_eq!(initial.len(), 2);
        assert!(diff_terminal_sizes(
            vec![(first.clone(), 20, 80), (second.clone(), 20, 80)],
            &mut last,
        )
        .is_empty());

        let changed = diff_terminal_sizes(
            vec![(second.clone(), 18, 60), (third.clone(), 18, 60)],
            &mut last,
        );
        assert_eq!(
            changed,
            vec![
                AppAction::Resize {
                    address: second.clone(),
                    rows: 18,
                    cols: 60,
                },
                AppAction::Resize {
                    address: third.clone(),
                    rows: 18,
                    cols: 60,
                },
            ]
        );
        assert_eq!(last.len(), 2);
        assert!(!last.contains_key(&first));
    }

    #[test]
    fn action_projection_is_pty_only_and_restart_has_no_prompt() {
        let address = cursor_app().tabs[0].address.clone();
        let spawn = action_to_request(AppAction::Spawn {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            provider: Provider::Kimi,
            rows: 24,
            cols: 80,
        })
        .unwrap();
        assert!(matches!(spawn, NodeRequest::Spawn { mode: SessionMode::Pty, initial_prompt: None, .. }));
        let resume = action_to_request(AppAction::Resume { address, rows: 24, cols: 80 }).unwrap();
        assert!(matches!(resume, NodeRequest::Resume { initial_prompt: None, .. }));
    }

    #[test]
    fn raw_terminal_bytes_project_without_text_or_paste_framing() {
        let address = cursor_app().tabs[0].address.clone();
        let request = action_to_request(AppAction::TerminalBytes {
            address,
            bytes: b"\x1bp".to_vec(),
        })
        .unwrap();
        assert!(matches!(
            request,
            NodeRequest::TerminalBytes { bytes, .. } if bytes == b"\x1bp"
        ));
    }

    #[test]
    fn workspace_actions_project_exact_id_and_root() {
        let register = action_to_request(AppAction::RegisterWorkspace {
            node_id: "node-a".to_owned(),
            workspace_id: "scratch".to_owned(),
            root: host_path(r"C:\tmp\scratch"),
        })
        .unwrap();
        assert!(matches!(register, NodeRequest::RegisterWorkspace { workspace_id, root } if workspace_id.as_str() == "scratch" && root == host_path(r"C:\tmp\scratch")));
        let unregister = action_to_request(AppAction::UnregisterWorkspace {
            node_id: "node-a".to_owned(),
            workspace_id: "scratch".to_owned(),
        })
        .unwrap();
        assert!(matches!(unregister, NodeRequest::UnregisterWorkspace { workspace_id } if workspace_id.as_str() == "scratch"));
        let inspect = action_to_request(AppAction::InspectWorkspace {
            node_id: "node-a".to_owned(),
            workspace_id: "scratch".to_owned(),
        })
        .unwrap();
        assert!(matches!(inspect, NodeRequest::InspectWorkspace { workspace_id } if workspace_id.as_str() == "scratch"));
    }

    #[test]
    fn managed_session_actions_project_exact_node_protocol_requests() {
        let resume = action_to_request(AppAction::ResumeSessionRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-1".to_owned(),
            rows: 31,
            cols: 107,
        })
        .unwrap();
        assert!(matches!(
            resume,
            NodeRequest::ResumeSessionRecord { record_id, terminal_size, initial_prompt: None }
                if record_id.as_str() == "record-1"
                    && terminal_size.rows == 31
                    && terminal_size.columns == 107
        ));
        let rename = action_to_request(AppAction::RenameSessionRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-1".to_owned(),
            display_name: "review".to_owned(),
        })
        .unwrap();
        assert!(matches!(
            rename,
            NodeRequest::RenameSessionRecord { record_id, display_name }
                if record_id.as_str() == "record-1" && display_name == "review"
        ));
        let forget = action_to_request(AppAction::ForgetSessionRecord {
            node_id: "node-a".to_owned(),
            record_id: "record-1".to_owned(),
        })
        .unwrap();
        assert!(matches!(
            forget,
            NodeRequest::ForgetSessionRecord { record_id }
                if record_id.as_str() == "record-1"
        ));
    }

    #[test]
    fn worktree_actions_project_exact_source_target_branch_and_base() {
        let create = action_to_request(AppAction::CreateWorktree {
            node_id: "node-a".to_owned(),
            source_workspace_id: "workspace-a".to_owned(),
            workspace_id: "feature-a".to_owned(),
            target_root: host_path(r"C:\work\feature-a"),
            branch: "feature/a".to_owned(),
            base: Some("origin/main".to_owned()),
        })
        .unwrap();
        assert!(matches!(create, NodeRequest::CreateWorktree {
            source_workspace_id,
            workspace_id,
            target_root,
            branch,
            base: Some(base),
        } if source_workspace_id.as_str() == "workspace-a"
            && workspace_id.as_str() == "feature-a"
            && target_root == host_path(r"C:\work\feature-a")
            && branch == "feature/a"
            && base == "origin/main"));
        let remove = action_to_request(AppAction::RemoveWorktree {
            node_id: "node-a".to_owned(),
            source_workspace_id: "workspace-a".to_owned(),
            target_root: host_path(r"C:\work\feature-a"),
        })
        .unwrap();
        assert!(matches!(remove, NodeRequest::RemoveWorktree { source_workspace_id, target_root }
            if source_workspace_id.as_str() == "workspace-a" && target_root == host_path(r"C:\work\feature-a")));
    }

    #[test]
    fn worktree_response_projection_selects_created_and_refreshes_authoritative_workspace() {
        let created_workspace_id = WorkspaceId::new("feature-a").unwrap();
        let created = NodeResponse::WorktreeCreated {
            worktree: gate4agent_node_protocol::GitWorktreeSnapshot {
                path: host_path(r"C:\work\feature-a"),
                head: "abc".to_owned(),
                branch: Some("feature/a".to_owned()),
                is_bare: false,
                is_main: false,
                locked: false,
                lock_reason: None,
                prunable: false,
                prunable_reason: None,
                workspace_id: Some(created_workspace_id.clone()),
            },
            workspace: gate4agent_node_protocol::WorkspaceSnapshot {
                workspace_id: created_workspace_id.clone(),
                canonical_root: host_path(r"C:\work\feature-a"),
                sessions: Vec::new(),
            },
        };
        let projection = project_worktree_response(
            &created,
            "node-a",
            Some(&WorkspaceId::new("workspace-a").unwrap()),
        )
        .unwrap();
        assert_eq!(projection.selected_workspace_id.as_deref(), Some("feature-a"));
        assert_eq!(projection.refresh_workspace_id, Some(created_workspace_id));
        assert!(projection.notice.contains("created"));

        let source = WorkspaceId::new("workspace-a").unwrap();
        let removed = NodeResponse::WorktreeRemoved {
            target_root: host_path(r"C:\work\feature-a"),
            workspace_id: Some(WorkspaceId::new("feature-a").unwrap()),
        };
        let projection = project_worktree_response(&removed, "node-a", Some(&source)).unwrap();
        assert_eq!(projection.selected_workspace_id, None);
        assert_eq!(projection.refresh_workspace_id, Some(source.clone()));
        assert!(projection.notice.contains("removed"));

        let removed_source = NodeResponse::WorktreeRemoved {
            target_root: host_path(r"C:\work\workspace-a"),
            workspace_id: Some(source.clone()),
        };
        let projection = project_worktree_response(&removed_source, "node-a", Some(&source)).unwrap();
        assert_eq!(projection.selected_workspace_id, None);
        assert_eq!(projection.refresh_workspace_id, None);
        assert!(projection.notice.contains("removed"));
    }

    #[test]
    fn inspection_uses_observer_queue_not_operator_command_queue() {
        let mut app = cursor_app();
        let (operator_tx, mut operator_rx) = mpsc::channel(1);
        let mut operator = BTreeMap::new();
        operator.insert("node-a".to_owned(), operator_tx);
        let (inspection_tx, mut inspection_rx) = watch::channel(None);
        let mut inspections = BTreeMap::new();
        inspections.insert("node-a".to_owned(), inspection_tx);

        send_action(
            &mut app,
            &operator,
            &inspections,
            AppAction::InspectWorkspace {
                node_id: "node-a".to_owned(),
                workspace_id: "workspace-a".to_owned(),
            },
        );

        assert!(matches!(
            operator_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(inspection_rx.has_changed().unwrap());
        assert_eq!(
            inspection_rx.borrow_and_update().as_ref().unwrap().as_str(),
            "workspace-a"
        );
    }

    #[test]
    fn projection_accepts_only_pty_sessions() {
        assert!(supports_tui_transport(TransportKind::Pty));
        assert!(!supports_tui_transport(TransportKind::Pipe));
        assert!(!supports_tui_transport(TransportKind::Acp));
    }

    #[test]
    fn lifecycle_projection_never_removes_registered_or_stopping_sessions() {
        for status in [SessionStatus::Registered, SessionStatus::Starting, SessionStatus::Running, SessionStatus::Stopping] {
            let lifecycle = project_lifecycle(&status);
            assert!(lifecycle.stoppable);
            assert!(!lifecycle.removable);
            assert!(!lifecycle.restartable);
        }
        for status in [
            SessionStatus::Exited { exit_code: Some(0) },
            SessionStatus::Failed { message: "failed".to_owned() },
        ] {
            let lifecycle = project_lifecycle(&status);
            assert!(!lifecycle.stoppable);
            assert!(lifecycle.removable);
            assert!(lifecycle.restartable);
        }
    }

    #[test]
    fn raw_paste_chunks_preserve_utf8_boundaries() {
        let text = format!("{}Г©", "a".repeat(MAX_NODE_TEXT_BYTES - 1));
        let chunks = utf8_chunks(&text, MAX_NODE_TEXT_BYTES);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn run_options_can_carry_an_explicit_style_override() {
        let options = RunOptions {
            nodes: Vec::new(),
            c2: None,
            startup: None,
            color_mode_override: Some(PtyColorMode::Inherited),
        };
        assert_eq!(options.color_mode_override, Some(PtyColorMode::Inherited));
    }

    #[test]
    fn invocation_style_override_is_not_persisted_without_a_user_change() {
        let mut app = App::default();
        app.color_mode = PtyColorMode::GateOverride;

        let preferences = preferences_for_save(&app, PtyColorMode::Inherited);

        assert_eq!(app.color_mode, PtyColorMode::GateOverride);
        assert_eq!(preferences.color_mode, PtyColorMode::Inherited);
    }

    #[test]
    fn c2_control_notice_uses_only_redacted_event_category_and_address() {
        let address = WireSessionAddress {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            session: SessionKey {
                instance_id: gate4agent_types::AgentInstanceId(7),
                generation: gate4agent_types::SessionGeneration(2),
            },
        };
        let event = C2ControlEvent {
            protocol_version: gate4agent_types::CONTROL_PROTOCOL_VERSION,
            sequence: 4,
            command_id: None,
            instance_id: gate4agent_types::AgentInstanceId(7),
            generation: gate4agent_types::SessionGeneration(2),
            event: C2ControlEventKind::Resumed,
        };
        assert_eq!(
            safe_c2_control_notice("node-a", &address, &event).as_deref(),
            Some("node-a/workspace-a #7:2: session resumed"),
        );

        let provider_event = C2ControlEvent {
            event: C2ControlEventKind::ProviderEvent {
                event: gate4agent_c2_protocol::C2ProviderEventKind::SessionIdentityObserved,
            },
            ..event
        };
        assert_eq!(safe_c2_control_notice("node-a", &address, &provider_event), None);
    }

    fn c2_test_record(display_name: &str) -> C2ManagedSessionRecord {
        C2ManagedSessionRecord {
            record_id: SessionRecordId::new("record-1").unwrap(),
            display_name: display_name.to_owned(),
            provider: AgentProvider::Codex,
            mode: SessionMode::Pty,
            state: gate4agent_node_protocol::ManagedSessionState::Dormant,
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            active_session: None,
            provider_identity_present: true,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        }
    }

    fn c2_test_snapshot(display_name: &str) -> C2NodeSnapshot {
        C2NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: vec![AgentProvider::Codex],
            workspaces: Vec::new(),
            session_records: vec![c2_test_record(display_name)],
        }
    }

    #[test]
    fn c2_event_before_snapshot_stays_monotonic_and_snapshot_is_authoritative_at_watermark() {
        let incarnation_id = gate4agent_c2_protocol::NodeIncarnationId::from_bytes([5; 16]);
        let mut app = App::default();
        let mut c2 = C2ApplyState::default();
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_test_snapshot("baseline"),
            event_sequence: 4,
        });
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Event {
            node_id: "node-a".to_owned(),
            cursor: gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: 6,
            },
            event: C2EventUpdate::SessionRecordUpserted(project_c2_managed_session(
                "node-a",
                c2_test_record("event-newer"),
            )),
        });
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_test_snapshot("snapshot-stale"),
            event_sequence: 5,
        });

        assert_eq!(app.nodes[0].session_records[0].display_name, "event-newer");
        assert_eq!(app.nodes[0].event_sequence, 6);

        apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_test_snapshot("snapshot-authoritative"),
            event_sequence: 6,
        });
        assert_eq!(
            app.nodes[0].session_records[0].display_name,
            "snapshot-authoritative"
        );
        assert_eq!(app.nodes[0].event_sequence, 6);
    }

    #[test]
    fn c2_duplicate_and_stale_events_cannot_regress_snapshot_record() {
        let incarnation_id = gate4agent_c2_protocol::NodeIncarnationId::from_bytes([6; 16]);
        let mut app = App::default();
        let mut c2 = C2ApplyState::default();
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Snapshot {
            expected_node_id: "node-a".to_owned(),
            endpoint: r"\\.\pipe\node-a".to_owned(),
            incarnation_id,
            snapshot: c2_test_snapshot("snapshot-current"),
            event_sequence: 5,
        });
        for (sequence, display_name) in [(5, "duplicate"), (4, "stale")] {
            apply_update(&mut app, &mut c2, WorkerUpdate::C2Event {
                node_id: "node-a".to_owned(),
                cursor: gate4agent_c2_protocol::NodeCursor {
                    incarnation_id,
                    sequence,
                },
                event: C2EventUpdate::SessionRecordUpserted(project_c2_managed_session(
                    "node-a",
                    c2_test_record(display_name),
                )),
            });
        }
        assert_eq!(app.nodes[0].session_records[0].display_name, "snapshot-current");
        assert_eq!(app.nodes[0].event_sequence, 5);

        apply_update(&mut app, &mut c2, WorkerUpdate::C2Event {
            node_id: "node-a".to_owned(),
            cursor: gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: 6,
            },
            event: C2EventUpdate::SessionRecordUpserted(project_c2_managed_session(
                "node-a",
                c2_test_record("fresh"),
            )),
        });
        assert_eq!(app.nodes[0].session_records[0].display_name, "fresh");
        assert_eq!(app.nodes[0].event_sequence, 6);

        apply_update(&mut app, &mut c2, WorkerUpdate::C2Event {
            node_id: "node-a".to_owned(),
            cursor: gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: 7,
            },
            event: C2EventUpdate::Ignored,
        });
        apply_update(&mut app, &mut c2, WorkerUpdate::C2Event {
            node_id: "node-a".to_owned(),
            cursor: gate4agent_c2_protocol::NodeCursor {
                incarnation_id,
                sequence: 6,
            },
            event: C2EventUpdate::SessionRecordUpserted(project_c2_managed_session(
                "node-a",
                c2_test_record("stale-after-ignored-event"),
            )),
        });
        assert_eq!(app.nodes[0].session_records[0].display_name, "fresh");
        assert_eq!(app.nodes[0].event_sequence, 7);
    }

    #[tokio::test]
    async fn c2_snapshot_and_record_event_feed_the_same_authoritative_app_projection() {
        let record = C2ManagedSessionRecord {
            record_id: SessionRecordId::new("record-1").unwrap(),
            display_name: "review".to_owned(),
            provider: AgentProvider::Codex,
            mode: SessionMode::Pty,
            state: gate4agent_node_protocol::ManagedSessionState::IdentityPending,
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            active_session: None,
            provider_identity_present: true,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        let snapshot = C2NodeSnapshot {
            node_id: NodeId::new("node-a").unwrap(),
            enabled_providers: vec![AgentProvider::Codex],
            workspaces: Vec::new(),
            session_records: vec![record.clone()],
        };
        let (updates, mut receiver) = mpsc::channel(8);
        publish_c2_response(
            C2NodeResponse::Snapshot {
                event_sequence: 4,
                controller: None,
                snapshot,
            },
            &updates,
            "node-a",
            gate4agent_c2_protocol::NodeIncarnationId::from_bytes([4; 16]),
            r"\\.\pipe\node-a".to_owned(),
        )
        .await;
        let mut app = App::default();
        let mut c2 = C2ApplyState::default();
        apply_update(&mut app, &mut c2, receiver.recv().await.unwrap());
        assert_eq!(app.nodes[0].session_records[0].display_name, "review");
        assert!(app.nodes[0].controller_owned);
        assert!(app.nodes[0].session_records[0].has_provider_session_identity);

        let mut dormant = record;
        dormant.state = gate4agent_node_protocol::ManagedSessionState::Dormant;
        dormant.display_name = "renamed".to_owned();
        publish_c2_event(
            C2NodeEvent::SessionRecordUpserted { record: dormant },
            gate4agent_c2_protocol::NodeCursor {
                incarnation_id: gate4agent_c2_protocol::NodeIncarnationId::from_bytes([4; 16]),
                sequence: 5,
            },
            &updates,
            "node-a",
        )
        .await;
        apply_update(&mut app, &mut c2, receiver.recv().await.unwrap());
        assert_eq!(app.nodes[0].session_records[0].display_name, "renamed");
        assert_eq!(
            app.nodes[0].session_records[0].state,
            gate4agent_node_protocol::ManagedSessionState::Dormant
        );
        assert!(app.nodes[0].session_records[0].has_provider_session_identity);
    }
}
