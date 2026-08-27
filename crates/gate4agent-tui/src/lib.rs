pub mod app;
pub mod client;
pub mod diagnostics;
pub mod icons;
pub mod pty_palette;
pub mod platform;
pub(crate) mod pet_arcade;
pub mod preferences;
pub(crate) mod profile;
pub(crate) mod pty_render_cache;
pub mod render;
pub(crate) mod shimmer;
pub mod surface;
pub mod terminal_bg;
pub mod text_editor;

pub use app::{
    AddSpaceDialog, AddSpaceField, App, AppAction, ConnectionState, ControlSection, Focus,
    GitLocationDialogKind, HistoryDialog, LaunchContextMode, LaunchField, LaunchTarget, LayoutRects,
    LoadedHistoryView,
    ManagedSessionView, MenuPlacement, NodeView, Provider, PtyColorMode, RosterMode,
    SessionAddress, SessionView, SidebarMode, SidebarPresentation, SpawnDialog, UiKey, WorkspaceView,
};
pub use client::{run, HarnessOperatorEndpoint, RunOptions};
pub use preferences::UiPreferences;
pub use render::render;
pub use surface::{LayoutPreset, Pane, PaneId, PaneNode, SplitAxis, SurfaceDropZone, SurfaceState};
pub use text_editor::{
    CursorPosition as TextCursorPosition, SyncState as TextSyncState, SyntaxClass,
    SyntaxLanguage, SyntaxSpan, TextEditor,
};
