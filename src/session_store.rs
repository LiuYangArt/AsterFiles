use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
};

use crate::{
    domain::{
        ColumnKind, ColumnLayout, DirectoryViewPreference, EverythingConfig, FileVisibility,
        GroupField, MAX_DIRECTORY_VIEW_PREFERENCES, SearchViewPreference, SortDirection, SortField,
        ViewMode,
    },
    i18n::Language,
    network::{NetworkLocation, NetworkLocationSource, NetworkTarget},
};

#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};

const MAGIC: &[u8; 6] = b"ASTF10";
const MAX_TABS: usize = 1_024;
const MAX_WINDOWS: usize = 128;
const MAX_NETWORK_LOCATIONS: usize = 1_024;
const MAX_PATH_UNITS: usize = 32_767;
const MIN_WINDOW_WIDTH: u32 = 820;
const MIN_WINDOW_HEIGHT: u32 = 520;
const MAX_WINDOW_WIDTH: u32 = 7_680;
const MAX_WINDOW_HEIGHT: u32 = 4_320;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub const fn storage_code(self) -> u8 {
        match self {
            Self::System => 0,
            Self::Light => 1,
            Self::Dark => 2,
        }
    }

    pub const fn from_storage_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::System),
            1 => Some(Self::Light),
            2 => Some(Self::Dark),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowPlacement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionState {
    pub windows: Vec<WindowSessionState>,
    pub default_directory_view: DirectoryViewPreference,
    pub search_view: SearchViewPreference,
    pub directory_views: Vec<(PathBuf, DirectoryViewPreference)>,
    pub theme_mode: ThemeMode,
    pub language: Language,
    pub everything: EverythingConfig,
    pub file_visibility: FileVisibility,
    pub network_locations: Vec<NetworkLocation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowSessionState {
    pub placement: WindowPlacement,
    pub active_tab: usize,
    pub tab_paths: Vec<PathBuf>,
}

impl SessionState {
    #[cfg(test)]
    pub fn new(
        window: WindowPlacement,
        active_tab: usize,
        tab_paths: Vec<PathBuf>,
    ) -> io::Result<Self> {
        Self::with_windows_and_settings(
            vec![WindowSessionState {
                placement: window,
                active_tab,
                tab_paths,
            }],
            DirectoryViewPreference::default(),
            SearchViewPreference::default(),
            Vec::new(),
            ThemeMode::System,
            Language::Chinese,
            EverythingConfig::default(),
            FileVisibility::default(),
            Vec::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_windows_and_settings(
        mut windows: Vec<WindowSessionState>,
        default_directory_view: DirectoryViewPreference,
        search_view: SearchViewPreference,
        directory_views: Vec<(PathBuf, DirectoryViewPreference)>,
        theme_mode: ThemeMode,
        language: Language,
        everything: EverythingConfig,
        file_visibility: FileVisibility,
        network_locations: Vec<NetworkLocation>,
    ) -> io::Result<Self> {
        if windows.is_empty() || windows.len() > MAX_WINDOWS {
            return Err(invalid_data("invalid session window count"));
        }
        for window in &mut windows {
            validate_window(window.placement)?;
            if window.tab_paths.len() > MAX_TABS {
                return Err(invalid_data("invalid session tab count"));
            }
            for path in &window.tab_paths {
                validate_path(path)?;
            }
            window.active_tab = if window.tab_paths.is_empty() {
                0
            } else {
                window.active_tab.min(window.tab_paths.len() - 1)
            };
        }
        validate_directory_preference(default_directory_view)?;
        validate_search_preference(search_view)?;
        validate_directory_views(&directory_views)?;
        validate_everything_config(&everything)?;
        validate_network_locations(&network_locations)?;
        Ok(Self {
            windows,
            default_directory_view,
            search_view,
            directory_views,
            theme_mode,
            language,
            everything,
            file_visibility,
            network_locations,
        })
    }
}

pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("AsterFiles").join("session.bin"))
}

pub fn load(path: &Path) -> io::Result<SessionState> {
    let bytes = fs::read(path)?;
    decode(&bytes)
}

pub fn save(path: &Path, state: &SessionState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, encode(state)?)
}

fn encode(state: &SessionState) -> io::Result<Vec<u8>> {
    let state = SessionState::with_windows_and_settings(
        state.windows.clone(),
        state.default_directory_view,
        state.search_view,
        state.directory_views.clone(),
        state.theme_mode,
        state.language,
        state.everything.clone(),
        state.file_visibility,
        state.network_locations.clone(),
    )?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(state.windows.len() as u32).to_le_bytes());
    write_directory_preference(&mut bytes, state.default_directory_view);
    write_search_preference(&mut bytes, state.search_view);
    bytes.extend_from_slice(&(state.directory_views.len() as u32).to_le_bytes());
    for (path, preference) in &state.directory_views {
        write_os(&mut bytes, path.as_os_str())?;
        write_directory_preference(&mut bytes, *preference);
    }
    bytes.push(state.theme_mode.storage_code());
    bytes.push(state.language.storage_code());
    write_optional_os(&mut bytes, state.everything.executable_path.as_deref())?;
    write_string(&mut bytes, &state.everything.instance_name)?;
    write_optional_string(&mut bytes, state.everything.verified_version.as_deref())?;
    bytes.push(u8::from(state.everything.allow_launch));
    bytes.push(u8::from(state.file_visibility.show_hidden));
    bytes.push(u8::from(state.file_visibility.show_system));
    bytes.extend_from_slice(&(state.network_locations.len() as u32).to_le_bytes());
    for location in &state.network_locations {
        bytes.extend_from_slice(&location.id.to_le_bytes());
        write_string(&mut bytes, &location.display_name)?;
        bytes.extend_from_slice(&location.sort_order.to_le_bytes());
        let NetworkTarget::WindowsPath(path) = &location.target else {
            return Err(invalid_data("network location target cannot be persisted"));
        };
        write_os(&mut bytes, path.as_os_str())?;
    }

    for window in &state.windows {
        bytes.extend_from_slice(&window.placement.x.to_le_bytes());
        bytes.extend_from_slice(&window.placement.y.to_le_bytes());
        bytes.extend_from_slice(&window.placement.width.to_le_bytes());
        bytes.extend_from_slice(&window.placement.height.to_le_bytes());
        bytes.extend_from_slice(&(window.active_tab as u32).to_le_bytes());
        bytes.extend_from_slice(&(window.tab_paths.len() as u32).to_le_bytes());
        for path in &window.tab_paths {
            write_os(&mut bytes, path.as_os_str())?;
        }
    }
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> io::Result<SessionState> {
    if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
        return Err(invalid_data("invalid AsterFiles session"));
    }
    let mut offset = MAGIC.len();
    let window_count = read_u32(bytes, &mut offset)? as usize;
    if window_count == 0 || window_count > MAX_WINDOWS {
        return Err(invalid_data("invalid session window count"));
    }
    let default_directory_view = read_directory_preference(bytes, &mut offset)?;
    let search_view = read_search_preference(bytes, &mut offset)?;
    let directory_count = read_u32(bytes, &mut offset)? as usize;
    if directory_count > MAX_DIRECTORY_VIEW_PREFERENCES {
        return Err(invalid_data("too many directory view preferences"));
    }
    let mut directory_views = Vec::with_capacity(directory_count);
    for _ in 0..directory_count {
        directory_views.push((
            PathBuf::from(read_os(bytes, &mut offset)?),
            read_directory_preference(bytes, &mut offset)?,
        ));
    }
    let theme_mode = ThemeMode::from_storage_code(read_u8(bytes, &mut offset)?)
        .ok_or_else(|| invalid_data("invalid session theme mode"))?;
    let language = Language::from_storage_code(read_u8(bytes, &mut offset)?)
        .ok_or_else(|| invalid_data("invalid session language"))?;
    let everything = EverythingConfig {
        executable_path: read_optional_os(bytes, &mut offset)?.map(PathBuf::from),
        instance_name: read_string(bytes, &mut offset)?,
        verified_version: read_optional_string(bytes, &mut offset)?,
        allow_launch: read_bool(bytes, &mut offset, "invalid Everything launch setting")?,
    };
    let file_visibility = FileVisibility {
        show_hidden: read_bool(bytes, &mut offset, "invalid hidden-file setting")?,
        show_system: read_bool(bytes, &mut offset, "invalid system-file setting")?,
    };
    let network_location_count = read_u32(bytes, &mut offset)? as usize;
    if network_location_count > MAX_NETWORK_LOCATIONS {
        return Err(invalid_data("too many network locations"));
    }
    let mut network_locations = Vec::with_capacity(network_location_count);
    for _ in 0..network_location_count {
        network_locations.push(NetworkLocation {
            id: read_u64(bytes, &mut offset)?,
            source: NetworkLocationSource::AsterOwned,
            display_name: read_string(bytes, &mut offset)?,
            sort_order: read_u32(bytes, &mut offset)?,
            target: NetworkTarget::WindowsPath(PathBuf::from(read_os(bytes, &mut offset)?)),
        });
    }

    let mut windows = Vec::with_capacity(window_count);
    for _ in 0..window_count {
        let placement = WindowPlacement {
            x: read_i32(bytes, &mut offset)?,
            y: read_i32(bytes, &mut offset)?,
            width: read_u32(bytes, &mut offset)?,
            height: read_u32(bytes, &mut offset)?,
        };
        let active_tab = read_u32(bytes, &mut offset)? as usize;
        let count = read_u32(bytes, &mut offset)? as usize;
        if count > MAX_TABS {
            return Err(invalid_data("too many session tabs"));
        }
        let mut tab_paths = Vec::with_capacity(count);
        for _ in 0..count {
            tab_paths.push(PathBuf::from(read_os(bytes, &mut offset)?));
        }
        windows.push(WindowSessionState {
            placement,
            active_tab,
            tab_paths,
        });
    }
    if offset != bytes.len() {
        return Err(invalid_data("unexpected trailing session data"));
    }
    SessionState::with_windows_and_settings(
        windows,
        default_directory_view,
        search_view,
        directory_views,
        theme_mode,
        language,
        everything,
        file_visibility,
        network_locations,
    )
}

fn validate_network_locations(values: &[NetworkLocation]) -> io::Result<()> {
    if values.len() > MAX_NETWORK_LOCATIONS {
        return Err(invalid_data("too many network locations"));
    }
    let mut ids = HashSet::with_capacity(values.len());
    for location in values {
        if location.source != NetworkLocationSource::AsterOwned {
            return Err(invalid_data(
                "imported network location cannot be persisted",
            ));
        }
        let NetworkTarget::WindowsPath(path) = &location.target else {
            return Err(invalid_data("network location target cannot be persisted"));
        };
        if !ids.insert(location.id) {
            return Err(invalid_data("duplicate network location id"));
        }

        if location.display_name.trim().is_empty() {
            return Err(invalid_data("network location name cannot be empty"));
        }
        if location.display_name.encode_utf16().count() > MAX_PATH_UNITS {
            return Err(invalid_data("network location name is too long"));
        }
        validate_path(path)?;
    }
    Ok(())
}

fn validate_directory_views(values: &[(PathBuf, DirectoryViewPreference)]) -> io::Result<()> {
    if values.len() > MAX_DIRECTORY_VIEW_PREFERENCES {
        return Err(invalid_data("too many directory view preferences"));
    }
    let mut paths = HashSet::with_capacity(values.len());
    for (path, preference) in values {
        validate_path(path)?;
        if !paths.insert(path) {
            return Err(invalid_data("duplicate directory view preference"));
        }
        validate_directory_preference(*preference)?;
    }
    Ok(())
}

fn validate_directory_preference(value: DirectoryViewPreference) -> io::Result<()> {
    if value.is_valid() {
        Ok(())
    } else {
        Err(invalid_data("invalid directory view preference"))
    }
}

fn validate_search_preference(value: SearchViewPreference) -> io::Result<()> {
    if value.is_valid() {
        Ok(())
    } else {
        Err(invalid_data("invalid search view preference"))
    }
}

fn validate_path(path: &Path) -> io::Result<()> {
    if encode_os(path.as_os_str()).len() > MAX_PATH_UNITS {
        Err(invalid_data("stored path is too long"))
    } else {
        Ok(())
    }
}

fn validate_everything_config(config: &EverythingConfig) -> io::Result<()> {
    if config.instance_name.encode_utf16().count() > MAX_PATH_UNITS
        || config
            .verified_version
            .as_deref()
            .is_some_and(|version| version.encode_utf16().count() > MAX_PATH_UNITS)
        || config
            .executable_path
            .as_deref()
            .is_some_and(|path| encode_os(path.as_os_str()).len() > MAX_PATH_UNITS)
    {
        return Err(invalid_data("Everything setting is too long"));
    }
    Ok(())
}

fn write_directory_preference(bytes: &mut Vec<u8>, value: DirectoryViewPreference) {
    bytes.push(value.view_mode.storage_code());
    bytes.push(value.sort_field.storage_code());
    bytes.push(value.sort_direction.storage_code());
    bytes.push(value.group_field.storage_code());
    bytes.push(value.group_direction.storage_code());
    write_column_layout(bytes, value.columns);
}

fn write_search_preference(bytes: &mut Vec<u8>, value: SearchViewPreference) {
    bytes.push(value.view_mode.storage_code());
    bytes.push(value.sort_field.storage_code());
    bytes.push(value.sort_direction.storage_code());
    write_column_layout(bytes, value.columns);
}

fn write_column_layout(bytes: &mut Vec<u8>, value: ColumnLayout) {
    bytes.extend(value.order.map(ColumnKind::storage_code));
    for width in value.widths {
        bytes.extend_from_slice(&width.to_le_bytes());
    }
    for visible in value.visible {
        bytes.push(u8::from(visible));
    }
}

fn read_directory_preference(
    bytes: &[u8],
    offset: &mut usize,
) -> io::Result<DirectoryViewPreference> {
    let value = DirectoryViewPreference {
        view_mode: ViewMode::from_storage_code(read_u8(bytes, offset)?)
            .ok_or_else(|| invalid_data("invalid view mode"))?,
        sort_field: SortField::from_storage_code(read_u8(bytes, offset)?)
            .ok_or_else(|| invalid_data("invalid sort field"))?,
        sort_direction: SortDirection::from_storage_code(read_u8(bytes, offset)?)
            .ok_or_else(|| invalid_data("invalid sort direction"))?,
        group_field: GroupField::from_storage_code(read_u8(bytes, offset)?)
            .ok_or_else(|| invalid_data("invalid group field"))?,
        group_direction: SortDirection::from_storage_code(read_u8(bytes, offset)?)
            .ok_or_else(|| invalid_data("invalid group direction"))?,
        columns: read_column_layout(bytes, offset)?,
    };
    validate_directory_preference(value)?;
    Ok(value)
}

fn read_search_preference(bytes: &[u8], offset: &mut usize) -> io::Result<SearchViewPreference> {
    let value = SearchViewPreference {
        view_mode: ViewMode::from_storage_code(read_u8(bytes, offset)?)
            .ok_or_else(|| invalid_data("invalid view mode"))?,
        sort_field: SortField::from_storage_code(read_u8(bytes, offset)?)
            .ok_or_else(|| invalid_data("invalid sort field"))?,
        sort_direction: SortDirection::from_storage_code(read_u8(bytes, offset)?)
            .ok_or_else(|| invalid_data("invalid sort direction"))?,
        columns: read_column_layout(bytes, offset)?,
    };
    validate_search_preference(value)?;
    Ok(value)
}

fn read_column_layout(bytes: &[u8], offset: &mut usize) -> io::Result<ColumnLayout> {
    let mut order = [ColumnKind::Name; ColumnKind::COUNT];
    for column in &mut order {
        *column = ColumnKind::from_storage_code(read_u8(bytes, offset)?)
            .ok_or_else(|| invalid_data("invalid column kind"))?;
    }
    let mut widths = [0; ColumnKind::COUNT];
    for width in &mut widths {
        *width = read_u32(bytes, offset)?;
    }
    let mut visible = [false; ColumnKind::COUNT];
    for item in &mut visible {
        *item = read_bool(bytes, offset, "invalid column visibility")?;
    }
    Ok(ColumnLayout {
        order,
        widths,
        visible,
    })
}

fn write_optional_os(bytes: &mut Vec<u8>, value: Option<&Path>) -> io::Result<()> {
    match value {
        Some(value) => {
            bytes.push(1);
            write_os(bytes, value.as_os_str())
        }
        None => {
            bytes.push(0);
            Ok(())
        }
    }
}

fn write_os(bytes: &mut Vec<u8>, value: &OsStr) -> io::Result<()> {
    let units = encode_os(value);
    if units.len() > MAX_PATH_UNITS {
        return Err(invalid_data("stored path is too long"));
    }
    bytes.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(())
}

fn write_string(bytes: &mut Vec<u8>, value: &str) -> io::Result<()> {
    let units = value.encode_utf16().collect::<Vec<_>>();
    if units.len() > MAX_PATH_UNITS {
        return Err(invalid_data("stored string is too long"));
    }
    bytes.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(())
}

fn write_optional_string(bytes: &mut Vec<u8>, value: Option<&str>) -> io::Result<()> {
    match value {
        Some(value) => {
            bytes.push(1);
            write_string(bytes, value)
        }
        None => {
            bytes.push(0);
            Ok(())
        }
    }
}

fn read_optional_os(bytes: &[u8], offset: &mut usize) -> io::Result<Option<OsString>> {
    match read_u8(bytes, offset)? {
        0 => Ok(None),
        1 => read_os(bytes, offset).map(Some),
        _ => Err(invalid_data("invalid optional path")),
    }
}

fn read_os(bytes: &[u8], offset: &mut usize) -> io::Result<OsString> {
    read_units(bytes, offset).map(|units| decode_os(&units))
}

fn read_string(bytes: &[u8], offset: &mut usize) -> io::Result<String> {
    String::from_utf16(&read_units(bytes, offset)?)
        .map_err(|_| invalid_data("invalid UTF-16 string"))
}

fn read_optional_string(bytes: &[u8], offset: &mut usize) -> io::Result<Option<String>> {
    match read_u8(bytes, offset)? {
        0 => Ok(None),
        1 => read_string(bytes, offset).map(Some),
        _ => Err(invalid_data("invalid optional string")),
    }
}

fn read_units(bytes: &[u8], offset: &mut usize) -> io::Result<Vec<u16>> {
    let count = read_u32(bytes, offset)? as usize;
    if count > MAX_PATH_UNITS {
        return Err(invalid_data("stored string is too long"));
    }
    let byte_count = count
        .checked_mul(2)
        .ok_or_else(|| invalid_data("invalid string length"))?;
    let end = offset
        .checked_add(byte_count)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| invalid_data("truncated session data"))?;
    let mut units = Vec::with_capacity(count);
    for pair in bytes[*offset..end].chunks_exact(2) {
        units.push(u16::from_le_bytes([pair[0], pair[1]]));
    }
    *offset = end;
    Ok(units)
}

fn validate_window(window: WindowPlacement) -> io::Result<()> {
    if !(MIN_WINDOW_WIDTH..=MAX_WINDOW_WIDTH).contains(&window.width)
        || !(MIN_WINDOW_HEIGHT..=MAX_WINDOW_HEIGHT).contains(&window.height)
    {
        return Err(invalid_data("invalid session window placement"));
    }
    Ok(())
}

fn read_bool(bytes: &[u8], offset: &mut usize, message: &'static str) -> io::Result<bool> {
    match read_u8(bytes, offset)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid_data(message)),
    }
}

fn read_u8(bytes: &[u8], offset: &mut usize) -> io::Result<u8> {
    let value = *bytes
        .get(*offset)
        .ok_or_else(|| invalid_data("truncated session data"))?;
    *offset += 1;
    Ok(value)
}

fn read_i32(bytes: &[u8], offset: &mut usize) -> io::Result<i32> {
    read_array::<4>(bytes, offset).map(i32::from_le_bytes)
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> io::Result<u32> {
    read_array::<4>(bytes, offset).map(u32::from_le_bytes)
}

fn read_u64(bytes: &[u8], offset: &mut usize) -> io::Result<u64> {
    read_array::<8>(bytes, offset).map(u64::from_le_bytes)
}
fn read_array<const N: usize>(bytes: &[u8], offset: &mut usize) -> io::Result<[u8; N]> {
    let end = offset
        .checked_add(N)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| invalid_data("truncated session data"))?;
    let value = bytes[*offset..end]
        .try_into()
        .map_err(|_| invalid_data("truncated session data"))?;
    *offset = end;
    Ok(value)
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(windows)]
fn encode_os(value: &OsStr) -> Vec<u16> {
    value.encode_wide().collect()
}

#[cfg(windows)]
fn decode_os(value: &[u16]) -> OsString {
    OsString::from_wide(value)
}

#[cfg(not(windows))]
fn encode_os(value: &OsStr) -> Vec<u16> {
    value.to_string_lossy().encode_utf16().collect()
}

#[cfg(not(windows))]
fn decode_os(value: &[u16]) -> OsString {
    OsString::from(String::from_utf16_lossy(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> SessionState {
        let mut columns = ColumnLayout::default();
        columns.visible[usize::from(ColumnKind::Created.storage_code())] = true;
        let directory_preference = DirectoryViewPreference {
            view_mode: ViewMode::LargeIcons,
            sort_field: SortField::Created,
            sort_direction: SortDirection::Descending,
            group_field: GroupField::Kind,
            group_direction: SortDirection::Ascending,
            columns,
        };
        SessionState::with_windows_and_settings(
            vec![WindowSessionState {
                placement: WindowPlacement {
                    x: -120,
                    y: 80,
                    width: 1180,
                    height: 760,
                },
                active_tab: 0,
                tab_paths: vec![PathBuf::from(r"C:\项目\📁")],
            }],
            DirectoryViewPreference::default(),
            SearchViewPreference {
                view_mode: ViewMode::Content,
                sort_field: SortField::Modified,
                sort_direction: SortDirection::Descending,
                columns,
            },
            vec![(PathBuf::from(r"C:\项目\📁"), directory_preference)],
            ThemeMode::Dark,
            Language::English,
            EverythingConfig {
                executable_path: Some(PathBuf::from(r"C:\Tools\Everything.exe")),
                instance_name: "1.5a".to_owned(),
                verified_version: Some("1.5.0.1400a".to_owned()),
                allow_launch: false,
            },
            FileVisibility {
                show_hidden: true,
                show_system: false,
            },
            vec![NetworkLocation {
                id: 7,
                source: NetworkLocationSource::AsterOwned,
                display_name: "家庭 NAS".to_owned(),
                sort_order: 0,
                target: NetworkTarget::WindowsPath(PathBuf::from(r"\\NAS\媒体")),
            }],
        )
        .unwrap()
    }

    #[test]
    fn astf10_round_trip_preserves_network_locations_and_raw_paths() {
        let state = sample_state();
        assert_eq!(decode(&encode(&state).unwrap()).unwrap(), state);
    }

    #[test]
    fn rejects_old_formats() {
        for version in 1..=9 {
            let bytes = format!("ASTF{version}\0\0\0\0");
            assert_eq!(
                decode(bytes.as_bytes()).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn rejects_non_owned_and_non_path_network_locations() {
        let mut imported = sample_state();
        imported.network_locations[0].source = NetworkLocationSource::WindowsImported;
        assert_eq!(
            encode(&imported).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let mut shell_item = sample_state();
        shell_item.network_locations[0].target =
            NetworkTarget::ShellItemId(PathBuf::from("shell:::{network-location}"));
        assert_eq!(
            encode(&shell_item).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_empty_network_location_name() {
        let mut state = sample_state();
        state.network_locations[0].display_name = "  ".to_owned();
        assert_eq!(
            encode(&state).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn rejects_duplicate_network_location_identity() {
        let mut state = sample_state();
        let mut second = state.network_locations[0].clone();
        second.sort_order = 1;
        state.network_locations.push(second);
        assert_eq!(
            encode(&state).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_duplicate_and_excess_directory_preferences() {
        let mut state = sample_state();
        state.directory_views.push(state.directory_views[0].clone());
        assert_eq!(
            encode(&state).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let mut state = sample_state();
        state.directory_views = (0..=MAX_DIRECTORY_VIEW_PREFERENCES)
            .map(|index| {
                (
                    PathBuf::from(format!(r"C:\{index}")),
                    DirectoryViewPreference::default(),
                )
            })
            .collect();
        assert_eq!(
            encode(&state).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_hidden_name_and_invalid_column_width() {
        let mut state = sample_state();
        state.default_directory_view.columns.visible
            [usize::from(ColumnKind::Name.storage_code())] = false;
        assert_eq!(
            encode(&state).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let mut state = sample_state();
        state.search_view.columns.widths[0] = crate::domain::MIN_COLUMN_WIDTH - 1;
        assert_eq!(
            encode(&state).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn corrects_active_tab_index_and_rejects_invalid_windows() {
        let state = SessionState::new(
            WindowPlacement {
                x: 0,
                y: 0,
                width: 900,
                height: 600,
            },
            99,
            vec![PathBuf::from(r"C:\one"), PathBuf::from(r"C:\two")],
        )
        .unwrap();
        assert_eq!(state.windows[0].active_tab, 1);

        assert_eq!(
            SessionState::new(
                WindowPlacement {
                    x: 0,
                    y: 0,
                    width: 819,
                    height: 600,
                },
                0,
                Vec::new(),
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_truncated_or_trailing_data() {
        let bytes = encode(&sample_state()).unwrap();
        assert_eq!(
            decode(&bytes[..bytes.len() - 1]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut trailing = bytes;
        trailing.push(0);
        assert_eq!(
            decode(&trailing).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
