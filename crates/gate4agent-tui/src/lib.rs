pub mod app;
pub mod client;
pub mod diagnostics;
pub mod pty_palette;
pub mod platform;
pub mod preferences;
pub mod render;
pub mod surface;
pub mod text_editor;

pub use app::{
    AddSpaceDialog, AddSpaceField, App, AppAction, ConnectionState, ControlSection, Focus,
    GitLocationDialogKind, HistoryDialog, LaunchContextMode, LaunchField, LaunchTarget, LayoutRects,
    LoadedHistoryView,
    ManagedSessionView, MenuPlacement, NodeView, Provider, PtyColorMode, RosterMode,
    SessionAddress, SessionView, SidebarMode, SidebarPresentation, SpawnDialog, UiKey, WorkspaceView,
};
pub use client::{
    run, C2Endpoint, HarnessOperatorEndpoint, RunOptions, StartupMode, StartupRequest,
};
pub use preferences::UiPreferences;
pub use render::render;
pub use surface::{LayoutPreset, Pane, PaneId, PaneNode, SplitAxis, SurfaceDropZone, SurfaceState};
pub use text_editor::{
    CursorPosition as TextCursorPosition, SyncState as TextSyncState, SyntaxClass,
    SyntaxLanguage, SyntaxSpan, TextEditor,
};
