use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::app::{
    App, ControlSection, GridPreset, MenuPlacement, PtyColorMode, RosterMode, SidebarMode,
    SidebarPresentation,
};

const CONFIG_VERSION: u16 = 2;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiPreferences {
    pub color_mode: PtyColorMode,
    pub menu_placement: MenuPlacement,
    pub sidebar_presentation: SidebarPresentation,
    pub sidebar_collapsed: bool,
    pub control_section: ControlSection,
    pub sidebar_width: u16,
    pub sidebar_split_percent: u16,
    pub control_modal_position: Option<(u16, u16)>,
    pub control_modal_size: Option<(u16, u16)>,
    pub grid_preset: GridPreset,
    pub grid_column_cuts: [u16; 3],
    pub grid_row_cuts: [u16; 3],
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            color_mode: PtyColorMode::Inherited,
            menu_placement: MenuPlacement::Sidebar,
            sidebar_presentation: SidebarPresentation::Split,
            sidebar_collapsed: false,
            control_section: ControlSection::Files,
            sidebar_width: 26,
            sidebar_split_percent: 50,
            control_modal_position: None,
            control_modal_size: None,
            grid_preset: GridPreset::Quad,
            grid_column_cuts: [2_500, 5_000, 7_500],
            grid_row_cuts: [2_500, 5_000, 7_500],
        }
    }
}

impl UiPreferences {
    pub fn from_app(app: &App) -> Self {
        let control_section = match app.control_section {
            ControlSection::Settings => ControlSection::Files,
            section => section,
        };
        Self {
            color_mode: app.color_mode,
            menu_placement: app.menu_placement,
            sidebar_presentation: app.sidebar_presentation,
            sidebar_collapsed: app.sidebar_collapsed,
            control_section,
            sidebar_width: app.sidebar_width,
            sidebar_split_percent: app.sidebar_split_percent,
            control_modal_position: app.control_modal_position,
            control_modal_size: app.control_modal_size.map(sanitize_modal_size),
            grid_preset: app.grid.preset,
            grid_column_cuts: app.grid.column_cuts,
            grid_row_cuts: app.grid.row_cuts,
        }
    }

    pub fn apply_to(&self, app: &mut App) {
        app.color_mode = self.color_mode;
        app.menu_placement = self.menu_placement;
        app.sidebar_presentation = self.sidebar_presentation;
        app.sidebar_collapsed = self.sidebar_collapsed;
        app.control_section = match self.control_section {
            ControlSection::Settings => ControlSection::Files,
            section => section,
        };
        match app.control_section {
            ControlSection::Files => app.sidebar_mode = SidebarMode::Files,
            ControlSection::Git => app.sidebar_mode = SidebarMode::Git,
            ControlSection::Agents => app.roster_mode = RosterMode::Agents,
            ControlSection::Workspaces => app.roster_mode = RosterMode::Workspaces,
            ControlSection::Settings => unreachable!("settings is normalized above"),
        }
        app.sidebar_width = self.sidebar_width.clamp(18, 60);
        app.sidebar_split_percent = self.sidebar_split_percent.clamp(25, 75);
        app.control_modal_position = self.control_modal_position;
        app.control_modal_size = self.control_modal_size.map(sanitize_modal_size);
        app.grid.preset = self.grid_preset;
        app.grid.column_cuts = validated_cuts(self.grid_column_cuts)
            .unwrap_or(UiPreferences::default().grid_column_cuts);
        app.grid.row_cuts = validated_cuts(self.grid_row_cuts)
            .unwrap_or(UiPreferences::default().grid_row_cuts);
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err(invalid_data("preferences file is too large"));
        }
        let file = File::open(path)?;
        let mut contents = String::new();
        file.take(MAX_CONFIG_BYTES + 1).read_to_string(&mut contents)?;
        if contents.len() as u64 > MAX_CONFIG_BYTES {
            return Err(invalid_data("preferences file is too large"));
        }
        parse(&contents)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let Some(parent) = path.parent() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "preferences path has no parent",
            ));
        };
        fs::create_dir_all(parent)?;
        let temporary = sibling_path(path, "tmp");
        let backup = sibling_path(path, "bak");
        let result = (|| {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(self.encode().as_bytes())?;
            file.flush()?;
            file.sync_all()?;
            drop(file);

            match fs::rename(&temporary, path) {
                Ok(()) => Ok(()),
                Err(_first_error) if path.exists() => {
                    let _ = fs::remove_file(&backup);
                    fs::rename(path, &backup)?;
                    match fs::rename(&temporary, path) {
                        Ok(()) => {
                            let _ = fs::remove_file(&backup);
                            Ok(())
                        }
                        Err(error) => {
                            let _ = fs::rename(&backup, path);
                            Err(error)
                        }
                    }
                }
                Err(error) => Err(error),
            }
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn encode(&self) -> String {
        format!(
            "version={CONFIG_VERSION}\nstyle={}\nmenu={}\nsidebar_presentation={}\nsidebar_collapsed={}\ncontrol_section={}\nsidebar_width={}\nsidebar_split_percent={}\ncontrol_modal_position={}\ncontrol_modal_size={}\ngrid_preset={}\ngrid_column_cuts={}\ngrid_row_cuts={}\n",
            self.color_mode.id(),
            self.menu_placement.id(),
            self.sidebar_presentation.id(),
            self.sidebar_collapsed,
            self.control_section.id(),
            self.sidebar_width,
            self.sidebar_split_percent,
            encode_pair(self.control_modal_position),
            encode_pair(self.control_modal_size),
            self.grid_preset.id(),
            encode_cuts(self.grid_column_cuts),
            encode_cuts(self.grid_row_cuts),
        )
    }
}

pub fn default_path() -> Option<PathBuf> {
    if cfg!(windows) {
        return nonempty_env("LOCALAPPDATA")
            .map(|root| root.join("Gate4Agent").join("tui.conf"));
    }
    if let Some(root) = nonempty_env("XDG_CONFIG_HOME") {
        return Some(root.join("gate4agent").join("tui.conf"));
    }
    nonempty_env("HOME").map(|root| root.join(".config").join("gate4agent").join("tui.conf"))
}

fn nonempty_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn parse(contents: &str) -> io::Result<UiPreferences> {
    let mut preferences = UiPreferences::default();
    let mut version = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "version" => version = value.trim().parse::<u16>().ok(),
            "style" => {
                preferences.color_mode = match value.trim() {
                    "inherit" => PtyColorMode::Inherited,
                    "gate" => PtyColorMode::GateOverride,
                    _ => preferences.color_mode,
                }
            }
            "menu" => {
                preferences.menu_placement = match value.trim() {
                    "sidebar" => MenuPlacement::Sidebar,
                    "modal" => MenuPlacement::Modal,
                    _ => preferences.menu_placement,
                }
            }
            "sidebar_presentation" => {
                preferences.sidebar_presentation = match value.trim() {
                    "split" => SidebarPresentation::Split,
                    "activity" => SidebarPresentation::Activity,
                    _ => preferences.sidebar_presentation,
                }
            }
            "sidebar_collapsed" => {
                preferences.sidebar_collapsed = match value.trim() {
                    "true" => true,
                    "false" => false,
                    _ => preferences.sidebar_collapsed,
                }
            }
            "control_section" => {
                preferences.control_section = match value.trim() {
                    "files" => ControlSection::Files,
                    "git" => ControlSection::Git,
                    "agents" => ControlSection::Agents,
                    "workspaces" => ControlSection::Workspaces,
                    "settings" => ControlSection::Settings,
                    _ => preferences.control_section,
                }
            }
            "sidebar_width" => {
                if let Ok(width) = value.trim().parse::<u16>() {
                    preferences.sidebar_width = width.clamp(18, 60);
                }
            }
            "sidebar_split_percent" => {
                if let Ok(percent) = value.trim().parse::<u16>() {
                    preferences.sidebar_split_percent = percent.clamp(25, 75);
                }
            }
            "control_modal_position" => {
                preferences.control_modal_position = parse_pair(value.trim());
            }
            "control_modal_size" => {
                preferences.control_modal_size = parse_pair(value.trim());
            }
            "grid_preset" => {
                preferences.grid_preset = match value.trim() {
                    "2x2" | "quad" => GridPreset::Quad,
                    "1x4" | "columns" => GridPreset::Columns,
                    "4x1" | "rows" => GridPreset::Rows,
                    _ => preferences.grid_preset,
                }
            }
            "grid_column_cuts" => {
                if let Some(cuts) = parse_cuts(value.trim()) {
                    preferences.grid_column_cuts = cuts;
                }
            }
            "grid_row_cuts" => {
                if let Some(cuts) = parse_cuts(value.trim()) {
                    preferences.grid_row_cuts = cuts;
                }
            }
            _ => {}
        }
    }
    match version {
        Some(1) | Some(CONFIG_VERSION) => Ok(preferences),
        Some(other) => Err(invalid_data(format!("unsupported preferences version {other}"))),
        None => Err(invalid_data("preferences version is missing")),
    }
}

fn parse_pair(value: &str) -> Option<(u16, u16)> {
    if value == "none" {
        return None;
    }
    let (first, second) = value.split_once(',')?;
    Some((first.parse().ok()?, second.parse().ok()?))
}

fn encode_pair(value: Option<(u16, u16)>) -> String {
    value.map_or_else(|| "none".to_owned(), |(first, second)| format!("{first},{second}"))
}

fn sanitize_modal_size((width, height): (u16, u16)) -> (u16, u16) {
    (width.max(36), height.max(6))
}

fn parse_cuts(value: &str) -> Option<[u16; 3]> {
    let values = value
        .split(',')
        .map(str::parse::<u16>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let cuts: [u16; 3] = values.try_into().ok()?;
    validated_cuts(cuts)
}

fn validated_cuts(cuts: [u16; 3]) -> Option<[u16; 3]> {
    (cuts[0] >= 1_000
        && cuts[0].saturating_add(1_000) <= cuts[1]
        && cuts[1].saturating_add(1_000) <= cuts[2]
        && cuts[2] <= 9_000)
        .then_some(cuts)
}

fn encode_cuts(cuts: [u16; 3]) -> String {
    format!("{},{},{}", cuts[0], cuts[1], cuts[2])
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("tui.conf");
    path.with_file_name(format!(".{name}.{}.{}", std::process::id(), suffix))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    fn temp_path(test: &str) -> PathBuf {
        let unique = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        env::temp_dir()
            .join(format!("gate4agent-tui-preferences-{}-{unique}", std::process::id()))
            .join(format!("{test}.conf"))
    }

    #[test]
    fn preferences_round_trip_through_atomic_temp_path() {
        let path = temp_path("round-trip");
        let preferences = UiPreferences {
            color_mode: PtyColorMode::GateOverride,
            menu_placement: MenuPlacement::Modal,
            sidebar_presentation: SidebarPresentation::Activity,
            sidebar_collapsed: true,
            control_section: ControlSection::Agents,
            sidebar_width: 41,
            sidebar_split_percent: 63,
            control_modal_position: Some((17, 9)),
            control_modal_size: Some((102, 37)),
            grid_preset: GridPreset::Columns,
            grid_column_cuts: [2_000, 5_500, 8_000],
            grid_row_cuts: [1_500, 4_500, 7_500],
        };

        UiPreferences::default().save(&path).unwrap();
        preferences.save(&path).unwrap();
        assert_eq!(UiPreferences::load(&path).unwrap(), preferences);
        assert!(!sibling_path(&path, "tmp").exists());
        assert!(!sibling_path(&path, "bak").exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn invalid_values_fall_back_without_accepting_unknown_versions() {
        let path = temp_path("fallback");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "version=1\nstyle=unknown\nmenu=unknown\nsidebar_width=2\nsidebar_split_percent=99\ngrid_column_cuts=9000,5000,1000\n",
        )
        .unwrap();
        let loaded = UiPreferences::load(&path).unwrap();
        assert_eq!(loaded.color_mode, PtyColorMode::Inherited);
        assert_eq!(loaded.menu_placement, MenuPlacement::Sidebar);
        assert_eq!(loaded.sidebar_width, 18);
        assert_eq!(loaded.sidebar_split_percent, 75);
        assert_eq!(loaded.grid_column_cuts, [2_500, 5_000, 7_500]);

        fs::write(&path, "version=999\nstyle=gate\n").unwrap();
        assert_eq!(
            UiPreferences::load(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn preferences_apply_and_capture_only_ui_state() {
        let preferences = UiPreferences {
            color_mode: PtyColorMode::GateOverride,
            menu_placement: MenuPlacement::Modal,
            sidebar_presentation: SidebarPresentation::Activity,
            sidebar_collapsed: true,
            control_section: ControlSection::Workspaces,
            sidebar_width: 38,
            sidebar_split_percent: 61,
            control_modal_position: Some((12, 8)),
            control_modal_size: Some((90, 28)),
            grid_preset: GridPreset::Rows,
            grid_column_cuts: [2_000, 5_000, 8_000],
            grid_row_cuts: [1_500, 4_000, 7_000],
        };
        let mut app = App::default();

        preferences.apply_to(&mut app);

        assert_eq!(UiPreferences::from_app(&app), preferences);
        assert!(app.nodes.is_empty());
        assert!(app.tabs.is_empty());
    }

    #[test]
    fn preferences_apply_synchronizes_selected_section_with_panel_mode() {
        let mut app = App::default();
        let mut preferences = UiPreferences::default();

        preferences.control_section = ControlSection::Git;
        preferences.apply_to(&mut app);
        assert_eq!(app.control_section, ControlSection::Git);
        assert_eq!(app.sidebar_mode, SidebarMode::Git);

        preferences.control_section = ControlSection::Workspaces;
        preferences.apply_to(&mut app);
        assert_eq!(app.control_section, ControlSection::Workspaces);
        assert_eq!(app.roster_mode, RosterMode::Workspaces);

        preferences.control_section = ControlSection::Settings;
        preferences.apply_to(&mut app);
        assert_eq!(app.control_section, ControlSection::Files);
        assert_eq!(app.sidebar_mode, SidebarMode::Files);
    }
}
