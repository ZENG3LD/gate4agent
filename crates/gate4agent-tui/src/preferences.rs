use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use gate4agent_node_protocol::{
    NodeId, RepositoryPath, WorkspaceId, MAX_REPOSITORY_PATH_BYTES,
};

use crate::app::{
    App, ControlSection, ManagedAgentPreference, MenuPlacement, PtyColorMode, RosterMode,
    SidebarMode, SidebarPresentation, MAX_LOCAL_AGENT_ALIAS_BYTES,
    MAX_MANAGED_AGENT_PREFERENCES, MAX_MANAGED_AGENT_RECORD_ID_BYTES,
};
use crate::surface::LayoutPreset;

const CONFIG_VERSION: u16 = 6;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_COLLAPSED_DIRECTORY_PREFERENCES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollapsedDirectoryPreference {
    pub node_id: String,
    pub workspace_id: String,
    pub path: RepositoryPath,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiPreferences {
    pub color_mode: PtyColorMode,
    pub menu_placement: MenuPlacement,
    pub sidebar_presentation: SidebarPresentation,
    pub sidebar_collapsed: bool,
    pub control_section: ControlSection,
    pub roster_mode: RosterMode,
    pub sidebar_width: u16,
    pub sidebar_split_percent: u16,
    pub control_modal_position: Option<(u16, u16)>,
    pub control_modal_size: Option<(u16, u16)>,
    pub surface_layout: LayoutPreset,
    pub managed_agents: Vec<ManagedAgentPreference>,
    pub collapsed_directories: Vec<CollapsedDirectoryPreference>,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            color_mode: PtyColorMode::Inherited,
            menu_placement: MenuPlacement::Sidebar,
            sidebar_presentation: SidebarPresentation::Split,
            sidebar_collapsed: false,
            control_section: ControlSection::Files,
            roster_mode: RosterMode::Agents,
            sidebar_width: 26,
            sidebar_split_percent: 50,
            control_modal_position: None,
            control_modal_size: None,
            surface_layout: LayoutPreset::OneByOne,
            managed_agents: Vec::new(),
            collapsed_directories: Vec::new(),
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
            roster_mode: match app.roster_mode {
                RosterMode::NativeSessions => RosterMode::Agents,
                mode => mode,
            },
            sidebar_width: app.sidebar_width,
            sidebar_split_percent: app.sidebar_split_percent,
            control_modal_position: app.control_modal_position,
            control_modal_size: app.control_modal_size.map(sanitize_modal_size),
            surface_layout: app.surface.preset.unwrap_or(LayoutPreset::OneByOne),
            managed_agents: app.managed_agent_preferences.values().cloned().collect(),
            collapsed_directories: app
                .collapsed_directories
                .iter()
                .map(|(node_id, workspace_id, path)| CollapsedDirectoryPreference {
                    node_id: node_id.clone(),
                    workspace_id: workspace_id.clone(),
                    path: path.clone(),
                })
                .collect(),
        }
    }

    pub fn apply_to(&self, app: &mut App) {
        let _ = self.try_apply_to(app);
    }

    pub fn try_apply_to(&self, app: &mut App) -> io::Result<()> {
        let managed_agent_preferences = validated_managed_agent_map(&self.managed_agents)?;
        let collapsed_directories =
            validated_collapsed_directory_set(&self.collapsed_directories)?;
        // Applying preferences must accept exactly the same bounded state that can be
        // persisted. This check happens before any App field is mutated.
        let _ = self.encode()?;
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
            ControlSection::Agents => {
                app.roster_mode = match self.roster_mode {
                    RosterMode::Agents | RosterMode::NativeSessions => RosterMode::Agents,
                    RosterMode::Workspaces => RosterMode::Agents,
                }
            }
            ControlSection::Workspaces => app.roster_mode = RosterMode::Workspaces,
            ControlSection::Settings => unreachable!("settings is normalized above"),
        }
        app.sidebar_width = self.sidebar_width.clamp(18, 60);
        app.sidebar_split_percent = self.sidebar_split_percent.clamp(25, 75);
        app.control_modal_position = self.control_modal_position;
        app.control_modal_size = self.control_modal_size.map(sanitize_modal_size);
        let _ = app.surface.apply_layout_preset(self.surface_layout);
        app.managed_agent_preferences = managed_agent_preferences;
        app.collapsed_directories = collapsed_directories;
        Ok(())
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
        let encoded = self.encode()?;
        fs::create_dir_all(parent)?;
        let temporary = sibling_path(path, "tmp");
        let backup = sibling_path(path, "bak");
        let result = (|| {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(encoded.as_bytes())?;
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

    fn encode(&self) -> io::Result<String> {
        validate_managed_agents(&self.managed_agents)?;
        validate_collapsed_directories(&self.collapsed_directories)?;
        let mut encoded = format!(
            "version={CONFIG_VERSION}\nstyle={}\nmenu={}\nsidebar_presentation={}\nsidebar_collapsed={}\ncontrol_section={}\nroster_mode={}\nsidebar_width={}\nsidebar_split_percent={}\ncontrol_modal_position={}\ncontrol_modal_size={}\nsurface_layout={}\n",
            self.color_mode.id(),
            self.menu_placement.id(),
            self.sidebar_presentation.id(),
            self.sidebar_collapsed,
            self.control_section.id(),
            match self.roster_mode {
                RosterMode::NativeSessions => RosterMode::Agents.id(),
                mode => mode.id(),
            },
            self.sidebar_width,
            self.sidebar_split_percent,
            encode_pair(self.control_modal_position),
            encode_pair(self.control_modal_size),
            self.surface_layout.id(),
        );
        let mut managed_agents = self.managed_agents.clone();
        managed_agents.sort_by(|left, right| {
            (&left.node_id, &left.record_id).cmp(&(&right.node_id, &right.record_id))
        });
        for preference in managed_agents {
            encoded.push_str("managed_agent=");
            encoded.push_str(&encode_hex(preference.node_id.as_bytes()));
            encoded.push(',');
            encoded.push_str(&encode_hex(preference.record_id.as_bytes()));
            encoded.push(',');
            encoded.push_str(if preference.pinned { "1" } else { "0" });
            encoded.push(',');
            encoded.push_str(&preference.order.map_or_else(|| "-".to_owned(), |order| order.to_string()));
            encoded.push(',');
            encoded.push_str(&preference.alias.as_deref().map_or_else(|| "-".to_owned(), |alias| encode_hex(alias.as_bytes())));
            encoded.push('\n');
            if encoded.len() as u64 > MAX_CONFIG_BYTES {
                return Err(invalid_data("encoded preferences exceed the size limit"));
            }
        }
        let mut collapsed_directories = self.collapsed_directories.iter().collect::<Vec<_>>();
        collapsed_directories.sort_by(|left, right| {
            (&left.node_id, &left.workspace_id, &left.path)
                .cmp(&(&right.node_id, &right.workspace_id, &right.path))
        });
        for preference in collapsed_directories {
            encoded.push_str("collapsed_directory=");
            encoded.push_str(&preference.node_id);
            encoded.push(',');
            encoded.push_str(&preference.workspace_id);
            encoded.push(',');
            encoded.push_str(&encode_hex(preference.path.as_bytes()));
            encoded.push('\n');
            if encoded.len() as u64 > MAX_CONFIG_BYTES {
                return Err(invalid_data("encoded preferences exceed the size limit"));
            }
        }
        if encoded.len() as u64 > MAX_CONFIG_BYTES {
            return Err(invalid_data("encoded preferences exceed the size limit"));
        }
        Ok(encoded)
    }
}

fn validate_managed_agents(managed_agents: &[ManagedAgentPreference]) -> io::Result<()> {
    if managed_agents.len() > MAX_MANAGED_AGENT_PREFERENCES {
        return Err(invalid_data("too many managed agent preferences"));
    }
    let mut keys = BTreeSet::new();
    for preference in managed_agents {
        validate_preference_id(
            "node ID",
            &preference.node_id,
            MAX_MANAGED_AGENT_RECORD_ID_BYTES,
        )?;
        validate_preference_id(
            "record ID",
            &preference.record_id,
            MAX_MANAGED_AGENT_RECORD_ID_BYTES,
        )?;
        if !keys.insert((preference.node_id.as_str(), preference.record_id.as_str())) {
            return Err(invalid_data("duplicate managed agent preference"));
        }
        if preference.alias.as_ref().is_some_and(|alias| {
            alias.is_empty()
                || alias.len() > MAX_LOCAL_AGENT_ALIAS_BYTES
                || alias.chars().any(char::is_control)
        }) {
            return Err(invalid_data("managed agent alias is invalid"));
        }
    }
    Ok(())
}

fn validated_managed_agent_map(
    managed_agents: &[ManagedAgentPreference],
) -> io::Result<BTreeMap<(String, String), ManagedAgentPreference>> {
    validate_managed_agents(managed_agents)?;
    Ok(managed_agents
        .iter()
        .map(|preference| {
            (
                (preference.node_id.clone(), preference.record_id.clone()),
                preference.clone(),
            )
        })
        .collect())
}

fn validate_collapsed_directories(
    collapsed_directories: &[CollapsedDirectoryPreference],
) -> io::Result<()> {
    if collapsed_directories.len() > MAX_COLLAPSED_DIRECTORY_PREFERENCES {
        return Err(invalid_data("too many collapsed directory preferences"));
    }
    let mut keys = BTreeSet::new();
    for preference in collapsed_directories {
        NodeId::new(preference.node_id.as_str())
            .map_err(|error| invalid_data(format!("collapsed directory node ID is invalid: {error}")))?;
        WorkspaceId::new(preference.workspace_id.as_str())
            .map_err(|error| invalid_data(format!("collapsed directory workspace ID is invalid: {error}")))?;
        RepositoryPath::unix_bytes(preference.path.as_bytes().to_vec())
            .map_err(|error| invalid_data(format!("collapsed directory path is invalid: {error}")))?;
        if !keys.insert((
            preference.node_id.as_str(),
            preference.workspace_id.as_str(),
            preference.path.as_bytes(),
        )) {
            return Err(invalid_data("duplicate collapsed directory preference"));
        }
    }
    Ok(())
}

fn validated_collapsed_directory_set(
    collapsed_directories: &[CollapsedDirectoryPreference],
) -> io::Result<BTreeSet<(String, String, RepositoryPath)>> {
    validate_collapsed_directories(collapsed_directories)?;
    Ok(collapsed_directories
        .iter()
        .map(|preference| {
            (
                preference.node_id.clone(),
                preference.workspace_id.clone(),
                preference.path.clone(),
            )
        })
        .collect())
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
    if contents.len() as u64 > MAX_CONFIG_BYTES {
        return Err(invalid_data("preferences file is too large"));
    }
    let mut preferences = UiPreferences::default();
    let mut version = None;
    let mut managed_agents = Vec::new();
    let mut managed_agent_keys = std::collections::BTreeSet::new();
    let mut collapsed_directory_values = Vec::new();
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
            "roster_mode" => {
                preferences.roster_mode = match value.trim() {
                    "agents" => RosterMode::Agents,
                    "native sessions" => RosterMode::Agents,
                    "workspaces" => RosterMode::Workspaces,
                    _ => preferences.roster_mode,
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
            "surface_layout" => {
                preferences.surface_layout = parse_layout_preset(value.trim())
                    .unwrap_or(preferences.surface_layout);
            }
            "managed_agent" => {
                if managed_agents.len() >= MAX_MANAGED_AGENT_PREFERENCES {
                    return Err(invalid_data("too many managed agent preferences"));
                }
                let preference = parse_managed_agent_preference(value.trim())?;
                let key = (preference.node_id.clone(), preference.record_id.clone());
                if !managed_agent_keys.insert(key) {
                    return Err(invalid_data("duplicate managed agent preference"));
                }
                managed_agents.push(preference);
            }
            "collapsed_directory" => {
                collapsed_directory_values.push(value.trim().to_owned());
            }
            "grid_preset" => {
                preferences.surface_layout = match value.trim() {
                    "2x2" | "quad" => LayoutPreset::TwoByTwo,
                    "1x4" | "columns" => LayoutPreset::OneByFour,
                    "4x1" | "rows" => LayoutPreset::FourByOne,
                    _ => preferences.surface_layout,
                };
            }
            _ => {}
        }
    }
    match version {
        Some(1) | Some(2) | Some(3) | Some(4) => {
            preferences.managed_agents.clear();
            preferences.collapsed_directories.clear();
            Ok(preferences)
        }
        Some(5) => {
            preferences.managed_agents = managed_agents;
            preferences.collapsed_directories.clear();
            Ok(preferences)
        }
        Some(CONFIG_VERSION) => {
            preferences.managed_agents = managed_agents;
            if collapsed_directory_values.len() > MAX_COLLAPSED_DIRECTORY_PREFERENCES {
                return Err(invalid_data("too many collapsed directory preferences"));
            }
            preferences.collapsed_directories = collapsed_directory_values
                .iter()
                .map(|value| parse_collapsed_directory_preference(value))
                .collect::<io::Result<Vec<_>>>()?;
            validate_collapsed_directories(&preferences.collapsed_directories)?;
            Ok(preferences)
        }
        Some(other) => Err(invalid_data(format!("unsupported preferences version {other}"))),
        None => Err(invalid_data("preferences version is missing")),
    }
}

fn parse_managed_agent_preference(value: &str) -> io::Result<ManagedAgentPreference> {
    let fields = value.split(',').collect::<Vec<_>>();
    if fields.len() != 5 {
        return Err(invalid_data("managed agent preference field count is invalid"));
    }
    let node_id = decode_hex_string(fields[0])?;
    let record_id = decode_hex_string(fields[1])?;
    validate_preference_id("node ID", &node_id, MAX_MANAGED_AGENT_RECORD_ID_BYTES)?;
    validate_preference_id("record ID", &record_id, MAX_MANAGED_AGENT_RECORD_ID_BYTES)?;
    let pinned = match fields[2] {
        "0" => false,
        "1" => true,
        _ => return Err(invalid_data("managed agent pin flag is invalid")),
    };
    let order = if fields[3] == "-" {
        None
    } else {
        Some(fields[3].parse::<u16>().map_err(|_| invalid_data("managed agent order is invalid"))?)
    };
    let alias = if fields[4] == "-" {
        None
    } else {
        let alias = decode_hex_string(fields[4])?;
        if alias.is_empty()
            || alias.len() > MAX_LOCAL_AGENT_ALIAS_BYTES
            || alias.chars().any(char::is_control)
        {
            return Err(invalid_data("managed agent alias is invalid"));
        }
        Some(alias)
    };
    Ok(ManagedAgentPreference { node_id, record_id, pinned, alias, order })
}

fn parse_collapsed_directory_preference(
    value: &str,
) -> io::Result<CollapsedDirectoryPreference> {
    let fields = value.split(',').collect::<Vec<_>>();
    if fields.len() != 3 {
        return Err(invalid_data("collapsed directory preference field count is invalid"));
    }
    let node_id = NodeId::new(fields[0])
        .map_err(|error| invalid_data(format!("collapsed directory node ID is invalid: {error}")))?;
    let workspace_id = WorkspaceId::new(fields[1]).map_err(|error| {
        invalid_data(format!("collapsed directory workspace ID is invalid: {error}"))
    })?;
    let path = decode_bounded_hex_bytes(
        fields[2],
        MAX_REPOSITORY_PATH_BYTES,
        "collapsed directory path",
    )?;
    let path = RepositoryPath::unix_bytes(path)
        .map_err(|error| invalid_data(format!("collapsed directory path is invalid: {error}")))?;
    Ok(CollapsedDirectoryPreference {
        node_id: node_id.as_str().to_owned(),
        workspace_id: workspace_id.as_str().to_owned(),
        path,
    })
}

fn validate_preference_id(label: &str, value: &str, maximum: usize) -> io::Result<()> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(invalid_data(format!("managed agent {label} is invalid")));
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex_string(value: &str) -> io::Result<String> {
    if value.is_empty() || value.len() % 2 != 0 || value.len() > MAX_CONFIG_BYTES as usize {
        return Err(invalid_data("managed agent hex field is invalid"));
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = decode_hex_nibble(pair[0])?;
        let low = decode_hex_nibble(pair[1])?;
        decoded.push((high << 4) | low);
    }
    String::from_utf8(decoded).map_err(|_| invalid_data("managed agent hex field is not UTF-8"))
}

fn decode_bounded_hex_bytes(
    value: &str,
    maximum_bytes: usize,
    label: &str,
) -> io::Result<Vec<u8>> {
    if value.is_empty()
        || value.len() % 2 != 0
        || value.len() / 2 > maximum_bytes
    {
        return Err(invalid_data(format!("{label} hex field is invalid")));
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = decode_hex_nibble(pair[0])
            .map_err(|_| invalid_data(format!("{label} hex field is malformed")))?;
        let low = decode_hex_nibble(pair[1])
            .map_err(|_| invalid_data(format!("{label} hex field is malformed")))?;
        decoded.push((high << 4) | low);
    }
    Ok(decoded)
}

fn decode_hex_nibble(byte: u8) -> io::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(invalid_data("managed agent hex field is malformed")),
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

fn parse_layout_preset(value: &str) -> Option<LayoutPreset> {
    LayoutPreset::ALL
        .into_iter()
        .find(|preset| preset.id() == value)
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

    fn collapsed_directory(
        node_id: &str,
        workspace_id: &str,
        path: RepositoryPath,
    ) -> CollapsedDirectoryPreference {
        CollapsedDirectoryPreference {
            node_id: node_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            path,
        }
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
            roster_mode: RosterMode::Agents,
            sidebar_width: 41,
            sidebar_split_percent: 63,
            control_modal_position: Some((17, 9)),
            control_modal_size: Some((102, 37)),
            surface_layout: LayoutPreset::OneByFour,
            managed_agents: Vec::new(),
            collapsed_directories: Vec::new(),
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
            "version=1\nstyle=unknown\nmenu=unknown\nsidebar_width=2\nsidebar_split_percent=99\ngrid_preset=columns\n",
        )
        .unwrap();
        let loaded = UiPreferences::load(&path).unwrap();
        assert_eq!(loaded.color_mode, PtyColorMode::Inherited);
        assert_eq!(loaded.menu_placement, MenuPlacement::Sidebar);
        assert_eq!(loaded.sidebar_width, 18);
        assert_eq!(loaded.sidebar_split_percent, 75);
        assert_eq!(loaded.surface_layout, LayoutPreset::OneByFour);

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
            roster_mode: RosterMode::Workspaces,
            sidebar_width: 38,
            sidebar_split_percent: 61,
            control_modal_position: Some((12, 8)),
            control_modal_size: Some((90, 28)),
            surface_layout: LayoutPreset::FourByOne,
            managed_agents: Vec::new(),
            collapsed_directories: Vec::new(),
        };
        let mut app = App::default();

        preferences.apply_to(&mut app);

        assert_eq!(UiPreferences::from_app(&app), preferences);
        assert!(app.nodes.is_empty());
        assert!(app.surface.all_tabs().is_empty());
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
        preferences.roster_mode = RosterMode::Workspaces;
        preferences.apply_to(&mut app);
        assert_eq!(app.control_section, ControlSection::Workspaces);
        assert_eq!(app.roster_mode, RosterMode::Workspaces);

        preferences.control_section = ControlSection::Agents;
        preferences.roster_mode = RosterMode::NativeSessions;
        preferences.apply_to(&mut app);
        assert_eq!(app.control_section, ControlSection::Agents);
        assert_eq!(app.roster_mode, RosterMode::Agents);

        preferences.control_section = ControlSection::Settings;
        preferences.apply_to(&mut app);
        assert_eq!(app.control_section, ControlSection::Files);
        assert_eq!(app.sidebar_mode, SidebarMode::Files);
    }

    #[test]
    fn legacy_native_sessions_preference_migrates_to_agents() {
        let path = temp_path("legacy-native-sessions");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "version=4\ncontrol_section=agents\nroster_mode=native sessions\n",
        )
        .unwrap();

        let loaded = UiPreferences::load(&path).unwrap();
        assert_eq!(loaded.control_section, ControlSection::Agents);
        assert_eq!(loaded.roster_mode, RosterMode::Agents);
        let mut app = App::default();
        loaded.apply_to(&mut app);
        assert_eq!(app.roster_mode, RosterMode::Agents);
        assert!(loaded.encode().unwrap().contains("roster_mode=agents\n"));
        assert!(!loaded.encode().unwrap().contains("native sessions"));

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn preferences_v1_through_v4_migrate_to_v6_with_empty_collections() {
        for version in 1..=4 {
            let loaded = parse(&format!("version={version}\nstyle=gate\n")).unwrap();
            assert!(loaded.managed_agents.is_empty(), "version {version}");
            assert!(loaded.collapsed_directories.is_empty(), "version {version}");
            assert!(loaded.encode().unwrap().starts_with("version=6\n"), "version {version}");
        }
    }

    #[test]
    fn preferences_v6_managed_agents_round_trip_deterministically() {
        let mut preferences = UiPreferences::default();
        preferences.managed_agents = vec![
            ManagedAgentPreference {
                node_id: "node-z".to_owned(),
                record_id: "record-2".to_owned(),
                pinned: false,
                alias: Some("Ревью".to_owned()),
                order: Some(9),
            },
            ManagedAgentPreference {
                node_id: "node-a".to_owned(),
                record_id: "record-1".to_owned(),
                pinned: true,
                alias: None,
                order: Some(0),
            },
        ];
        let encoded = preferences.encode().unwrap();
        let decoded = parse(&encoded).unwrap();
        assert_eq!(decoded.encode().unwrap(), encoded);
        assert_eq!(decoded.managed_agents.len(), 2);
        assert!(encoded.find("6e6f64652d61").unwrap() < encoded.find("6e6f64652d7a").unwrap());

        let mut app = App::default();
        decoded.apply_to(&mut app);
        assert_eq!(UiPreferences::from_app(&app), decoded);
    }

    #[test]
    fn preferences_v6_collapsed_directories_round_trip_non_utf8_deterministically() {
        let utf8 = collapsed_directory(
            "node-a",
            "workspace-a",
            RepositoryPath::utf8("src/lib.rs".to_owned()).unwrap(),
        );
        let opaque_bytes = vec![b's', b'r', b'c', b'/', 0xff, b'-', b'd', b'i', b'r'];
        let opaque = collapsed_directory(
            "node-z",
            "workspace-z",
            RepositoryPath::unix_bytes(opaque_bytes.clone()).unwrap(),
        );
        let mut preferences = UiPreferences::default();
        preferences.collapsed_directories = vec![opaque.clone(), utf8.clone()];

        let encoded = preferences.encode().unwrap();
        let decoded = parse(&encoded).unwrap();
        assert!(encoded.starts_with("version=6\n"));
        assert_eq!(decoded.collapsed_directories, vec![utf8.clone(), opaque.clone()]);
        assert_eq!(decoded.collapsed_directories[1].path.as_bytes(), opaque_bytes);
        assert_eq!(decoded.collapsed_directories[1].path.as_utf8(), None);
        assert!(encoded.contains(&encode_hex(&opaque_bytes)));
        assert_eq!(decoded.encode().unwrap(), encoded);

        let path = temp_path("v6-collapsed-directories");
        preferences.save(&path).unwrap();
        assert_eq!(UiPreferences::load(&path).unwrap(), decoded);
        let _ = fs::remove_dir_all(path.parent().unwrap());

        let mut reversed = UiPreferences::default();
        reversed.collapsed_directories = vec![utf8, opaque];
        assert_eq!(reversed.encode().unwrap(), encoded);

        let mut app = App::default();
        decoded.try_apply_to(&mut app).unwrap();
        assert_eq!(UiPreferences::from_app(&app), decoded);
    }

    #[test]
    fn preferences_v5_migrates_with_empty_collapsed_directories() {
        let loaded = parse(
            "version=5\ncollapsed_directory=malformed-and-ignored-for-v5\nstyle=gate\n",
        )
        .unwrap();

        assert!(loaded.collapsed_directories.is_empty());
        assert_eq!(loaded.color_mode, PtyColorMode::GateOverride);
        assert!(loaded.encode().unwrap().starts_with("version=6\n"));
    }

    #[test]
    fn preferences_v6_rejects_invalid_duplicate_and_oversize_collapsed_directories_atomically() {
        let node = "node-a";
        let workspace = "workspace-a";
        let path = encode_hex(b"src");
        let valid = format!("collapsed_directory={node},{workspace},{path}\n");
        assert!(parse(&format!("version=6\n{valid}{valid}")).is_err());

        let invalid_node = "Node-A";
        let invalid_workspace = "_workspace";
        let invalid_path = encode_hex(b"../secret");
        for malformed in [
            "collapsed_directory=node-a,workspace-a,zz\n".to_owned(),
            "collapsed_directory=node-a,workspace-a\n".to_owned(),
            format!("collapsed_directory={invalid_node},{workspace},{path}\n"),
            format!("collapsed_directory={node},{invalid_workspace},{path}\n"),
            format!("collapsed_directory={node},{workspace},{invalid_path}\n"),
        ] {
            assert!(parse(&format!("version=6\n{malformed}")).is_err(), "{malformed}");
        }
        let oversized_path = encode_hex(&vec![b'x'; MAX_REPOSITORY_PATH_BYTES + 1]);
        assert!(parse(&format!(
            "version=6\ncollapsed_directory={node},{workspace},{oversized_path}\n"
        )).is_err());
        let too_many = (0..=MAX_COLLAPSED_DIRECTORY_PREFERENCES)
            .map(|index| format!(
                "collapsed_directory={node},{workspace},{}\n",
                encode_hex(format!("dir-{index}").as_bytes()),
            ))
            .collect::<String>();
        assert!(too_many.len() as u64 <= MAX_CONFIG_BYTES);
        assert!(parse(&format!("version=6\n{too_many}")).is_err());
        assert!(parse(&"x".repeat(MAX_CONFIG_BYTES as usize + 1)).is_err());

        let existing = collapsed_directory(
            "node-existing",
            "workspace-existing",
            RepositoryPath::utf8("existing".to_owned()).unwrap(),
        );
        let mut app = App::default();
        app.collapsed_directories.insert((
            existing.node_id.clone(),
            existing.workspace_id.clone(),
            existing.path.clone(),
        ));
        let before = UiPreferences::from_app(&app);
        let duplicate = collapsed_directory(
            "node-duplicate",
            "workspace-duplicate",
            RepositoryPath::utf8("duplicate".to_owned()).unwrap(),
        );
        let invalid_cases = vec![
            vec![collapsed_directory(
                "invalid node",
                "workspace-invalid",
                RepositoryPath::utf8("invalid".to_owned()).unwrap(),
            )],
            vec![duplicate.clone(), duplicate],
            (0..=MAX_COLLAPSED_DIRECTORY_PREFERENCES)
                .map(|index| collapsed_directory(
                    "node-many",
                    "workspace-many",
                    RepositoryPath::utf8(format!("dir-{index}")).unwrap(),
                ))
                .collect(),
            (0..64)
                .map(|index| collapsed_directory(
                    "node-large",
                    "workspace-large",
                    RepositoryPath::utf8(format!("dir-{index}/{}", "p".repeat(950))).unwrap(),
                ))
                .collect(),
        ];

        for collapsed_directories in invalid_cases {
            let mut invalid = UiPreferences::default();
            invalid.color_mode = PtyColorMode::GateOverride;
            invalid.sidebar_width = 55;
            invalid.collapsed_directories = collapsed_directories;
            assert!(invalid.try_apply_to(&mut app).is_err());
            assert_eq!(UiPreferences::from_app(&app), before);
            invalid.apply_to(&mut app);
            assert_eq!(UiPreferences::from_app(&app), before);
        }
    }

    #[test]
    fn preferences_v5_rejects_malformed_duplicate_and_oversize_managed_agents() {
        let valid = "managed_agent=6e6f6465,7265636f7264,1,0,616c696173\n";
        assert!(parse(&format!("version=5\n{valid}{valid}")).is_err());
        for malformed in [
            "managed_agent=zz,7265636f7264,1,0,-\n",
            "managed_agent=6e6f6465,7265636f7264,2,0,-\n",
            "managed_agent=6e6f6465,7265636f7264,1,no,-\n",
            "managed_agent=6e6f6465,7265636f7264,1,0,0a\n",
        ] {
            assert!(parse(&format!("version=5\n{malformed}")).is_err(), "{malformed}");
        }
        let oversized_alias = encode_hex(&vec![b'a'; MAX_LOCAL_AGENT_ALIAS_BYTES + 1]);
        assert!(parse(&format!(
            "version=5\nmanaged_agent=6e6f6465,7265636f7264,0,-,{oversized_alias}\n"
        )).is_err());
        let too_many = (0..=MAX_MANAGED_AGENT_PREFERENCES)
            .map(|index| format!(
                "managed_agent=6e6f6465,{},0,-,-\n",
                encode_hex(format!("record-{index}").as_bytes()),
            ))
            .collect::<String>();
        assert!(parse(&format!("version=5\n{too_many}")).is_err());

        let path = temp_path("invalid-encode-no-write");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "sentinel").unwrap();
        let duplicate = ManagedAgentPreference {
            node_id: "node-a".to_owned(),
            record_id: "record-a".to_owned(),
            pinned: false,
            alias: None,
            order: None,
        };
        let mut invalid = UiPreferences::default();
        invalid.managed_agents = vec![duplicate.clone(), duplicate];
        assert!(invalid.encode().is_err());
        assert!(invalid.save(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "sentinel");

        let mut too_large = UiPreferences::default();
        too_large.managed_agents = (0..MAX_MANAGED_AGENT_PREFERENCES)
            .map(|index| ManagedAgentPreference {
                node_id: format!("node-{index}-{}", "n".repeat(220)),
                record_id: format!("record-{index}-{}", "r".repeat(215)),
                pinned: false,
                alias: Some("a".repeat(MAX_LOCAL_AGENT_ALIAS_BYTES)),
                order: Some(index as u16),
            })
            .collect();
        assert!(too_large.encode().is_err());
        assert!(too_large.save(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "sentinel");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn preferences_apply_is_atomic_for_programmatic_managed_agent_state() {
        let existing = ManagedAgentPreference {
            node_id: "node-existing".to_owned(),
            record_id: "record-existing".to_owned(),
            pinned: true,
            alias: Some("existing alias".to_owned()),
            order: Some(3),
        };
        let mut app = App::default();
        app.managed_agent_preferences.insert(
            (existing.node_id.clone(), existing.record_id.clone()),
            existing.clone(),
        );
        let before = UiPreferences::from_app(&app);

        let duplicate = ManagedAgentPreference {
            node_id: "node-duplicate".to_owned(),
            record_id: "record-duplicate".to_owned(),
            pinned: false,
            alias: None,
            order: None,
        };
        let invalid_cases = [
            vec![ManagedAgentPreference {
                node_id: "node\ninvalid".to_owned(),
                record_id: "record-invalid".to_owned(),
                pinned: false,
                alias: None,
                order: None,
            }],
            vec![duplicate.clone(), duplicate],
            vec![ManagedAgentPreference {
                node_id: "node-alias".to_owned(),
                record_id: "record-alias".to_owned(),
                pinned: false,
                alias: Some("a".repeat(MAX_LOCAL_AGENT_ALIAS_BYTES + 1)),
                order: None,
            }],
            (0..=MAX_MANAGED_AGENT_PREFERENCES)
                .map(|index| ManagedAgentPreference {
                    node_id: "node-many".to_owned(),
                    record_id: format!("record-{index}"),
                    pinned: false,
                    alias: None,
                    order: None,
                })
                .collect(),
            (0..MAX_MANAGED_AGENT_PREFERENCES)
                .map(|index| ManagedAgentPreference {
                    node_id: format!("node-{index}-{}", "n".repeat(220)),
                    record_id: format!("record-{index}-{}", "r".repeat(215)),
                    pinned: false,
                    alias: Some("a".repeat(MAX_LOCAL_AGENT_ALIAS_BYTES)),
                    order: Some(index as u16),
                })
                .collect(),
        ];

        for managed_agents in invalid_cases {
            let mut invalid = UiPreferences::default();
            invalid.color_mode = PtyColorMode::GateOverride;
            invalid.sidebar_width = 55;
            invalid.managed_agents = managed_agents;
            assert!(invalid.try_apply_to(&mut app).is_err());
            assert_eq!(UiPreferences::from_app(&app), before);
            invalid.apply_to(&mut app);
            assert_eq!(UiPreferences::from_app(&app), before);
        }

        let replacement = ManagedAgentPreference {
            node_id: "node-replacement".to_owned(),
            record_id: "record-replacement".to_owned(),
            pinned: false,
            alias: Some("replacement alias".to_owned()),
            order: Some(1),
        };
        let mut valid = UiPreferences::default();
        valid.managed_agents = vec![replacement.clone()];
        valid.try_apply_to(&mut app).unwrap();
        assert_eq!(app.managed_agent_preferences.len(), 1);
        assert_eq!(
            app.managed_agent_preferences.get(&(
                replacement.node_id.clone(),
                replacement.record_id.clone(),
            )),
            Some(&replacement),
        );
        assert_eq!(UiPreferences::from_app(&app), valid);
    }
}
