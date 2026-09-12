use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
};

use crate::{
    domain::{
        ColumnKind, ColumnLayout, DirectoryViewPreference, EverythingConfig, FileVisibility,
        GroupField, MAX_DIRECTORY_VIEW_PREFERENCES, NavigationLocation, SearchViewPreference,
        SortDirection, SortField, ViewMode,
    },
    i18n::Language,
    network::{
        NetworkDeviceId, NetworkDeviceTarget, NetworkLocation, NetworkLocationSource, NetworkTarget,
    },
};

#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};

const MAGIC: &[u8; 6] = b"ASTF17";
pub const DEFAULT_QUICK_MENU_BACKDROP_OPACITY: u8 = 85;
const MAX_TABS: usize = 1_024;
const MAX_WINDOWS: usize = 128;
const MAX_NETWORK_LOCATIONS: usize = 1_024;
const MAX_NETWORK_DEVICES: usize = 1_024;
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
pub struct SidebarVisibility {
    values: [bool; 5],
}

impl Default for SidebarVisibility {
    fn default() -> Self {
        Self {
            values: [true, false, true, true, true],
        }
    }
}

impl SidebarVisibility {
    const fn storage_bits(self) -> u8 {
        (self.values[0] as u8)
            | ((self.values[1] as u8) << 1)
            | ((self.values[2] as u8) << 2)
            | ((self.values[3] as u8) << 3)
            | ((self.values[4] as u8) << 4)
    }

    pub const fn is_visible(self, index: usize) -> bool {
        self.values[index]
    }

    pub fn toggle(&mut self, index: usize) -> bool {
        let Some(visible) = self.values.get_mut(index) else {
            return false;
        };
        *visible = !*visible;
        true
    }

    const fn from_storage_bits(bits: u8) -> Option<Self> {
        if bits & !0x1f != 0 {
            return None;
        }
        Some(Self {
            values: [
                bits & 0x01 != 0,
                bits & 0x02 != 0,
                bits & 0x04 != 0,
                bits & 0x08 != 0,
                bits & 0x10 != 0,
            ],
        })
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
    pub file_list_quick_search: bool,
    pub new_tab_opens_home: bool,
    pub quick_menu_backdrop: bool,
    pub quick_menu_backdrop_opacity: u8,
    pub sidebar_visibility: SidebarVisibility,
    pub network_locations: Vec<NetworkLocation>,
    pub network_devices: Vec<NetworkDeviceTarget>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowSessionState {
    pub placement: WindowPlacement,
    pub active_tab: usize,
    pub tab_locations: Vec<NavigationLocation>,
}

impl SessionState {
    #[cfg(test)]
    pub fn new(
        window: WindowPlacement,
        active_tab: usize,
        tab_locations: Vec<NavigationLocation>,
    ) -> io::Result<Self> {
        Self::with_windows_and_settings(
            vec![WindowSessionState {
                placement: window,
                active_tab,
                tab_locations,
            }],
            DirectoryViewPreference::default(),
            SearchViewPreference::default(),
            Vec::new(),
            ThemeMode::System,
            Language::Chinese,
            EverythingConfig::default(),
            FileVisibility::default(),
            false,
            true,
            true,
            DEFAULT_QUICK_MENU_BACKDROP_OPACITY,
            SidebarVisibility::default(),
            Vec::new(),
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
        file_list_quick_search: bool,
        new_tab_opens_home: bool,
        quick_menu_backdrop: bool,
        quick_menu_backdrop_opacity: u8,
        sidebar_visibility: SidebarVisibility,
        network_locations: Vec<NetworkLocation>,
        network_devices: Vec<NetworkDeviceTarget>,
    ) -> io::Result<Self> {
        if windows.is_empty() || windows.len() > MAX_WINDOWS {
            return Err(invalid_data("invalid session window count"));
        }
        for window in &mut windows {
            validate_window(window.placement)?;
            if window.tab_locations.len() > MAX_TABS {
                return Err(invalid_data("invalid session tab count"));
            }
            for location in &window.tab_locations {
                validate_navigation_location(location)?;
            }
            window.active_tab = if window.tab_locations.is_empty() {
                0
            } else {
                window.active_tab.min(window.tab_locations.len() - 1)
            };
        }
        validate_directory_preference(default_directory_view)?;
        validate_search_preference(search_view)?;
        validate_directory_views(&directory_views)?;
        validate_everything_config(&everything)?;
        if quick_menu_backdrop_opacity > 100 {
            return Err(invalid_data("invalid quick-menu backdrop opacity"));
        }
        validate_network_locations(&network_locations)?;
        validate_network_devices(&network_devices)?;
        Ok(Self {
            windows,
            default_directory_view,
            search_view,
            directory_views,
            theme_mode,
            language,
            everything,
            file_visibility,
            file_list_quick_search,
            new_tab_opens_home,
            quick_menu_backdrop,
            quick_menu_backdrop_opacity,
            sidebar_visibility,
            network_locations,
            network_devices,
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
        state.file_list_quick_search,
        state.new_tab_opens_home,
        state.quick_menu_backdrop,
        state.quick_menu_backdrop_opacity,
        state.sidebar_visibility,
        state.network_locations.clone(),
        state.network_devices.clone(),
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
    bytes.push(u8::from(state.file_list_quick_search));
    bytes.push(u8::from(state.new_tab_opens_home));
    bytes.push(u8::from(state.quick_menu_backdrop));
    bytes.push(state.quick_menu_backdrop_opacity);
    bytes.push(state.sidebar_visibility.storage_bits());
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
    bytes.extend_from_slice(&(state.network_devices.len() as u32).to_le_bytes());
    for device in &state.network_devices {
        write_string(&mut bytes, &device.display_name)?;
        let path = device
            .unc_path
            .as_deref()
            .ok_or_else(|| invalid_data("network device target cannot be persisted"))?;
        write_os(&mut bytes, path.as_os_str())?;
    }

    for window in &state.windows {
        bytes.extend_from_slice(&window.placement.x.to_le_bytes());
        bytes.extend_from_slice(&window.placement.y.to_le_bytes());
        bytes.extend_from_slice(&window.placement.width.to_le_bytes());
        bytes.extend_from_slice(&window.placement.height.to_le_bytes());
        bytes.extend_from_slice(&(window.active_tab as u32).to_le_bytes());
        bytes.extend_from_slice(&(window.tab_locations.len() as u32).to_le_bytes());
        for location in &window.tab_locations {
            write_navigation_location(&mut bytes, location)?;
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
    let file_list_quick_search =
        read_bool(bytes, &mut offset, "invalid file-list quick-search setting")?;
    let new_tab_opens_home = read_bool(bytes, &mut offset, "invalid new-tab Home setting")?;
    let quick_menu_backdrop = read_bool(bytes, &mut offset, "invalid quick-menu backdrop setting")?;
    let quick_menu_backdrop_opacity = read_u8(bytes, &mut offset)?;
    if quick_menu_backdrop_opacity > 100 {
        return Err(invalid_data("invalid quick-menu backdrop opacity"));
    }
    let sidebar_visibility = SidebarVisibility::from_storage_bits(read_u8(bytes, &mut offset)?)
        .ok_or_else(|| invalid_data("invalid sidebar visibility setting"))?;
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
            shell_path: None,
        });
    }
    let network_device_count = read_u32(bytes, &mut offset)? as usize;
    if network_device_count > MAX_NETWORK_DEVICES {
        return Err(invalid_data("too many network devices"));
    }
    let mut network_devices = Vec::with_capacity(network_device_count);
    for _ in 0..network_device_count {
        let display_name = read_string(bytes, &mut offset)?;
        let path = PathBuf::from(read_os(bytes, &mut offset)?);
        network_devices.push(NetworkDeviceTarget {
            id: network_device_id(&path),
            display_name,
            shell_identity: None,
            unc_path: Some(path),
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
        let mut tab_locations = Vec::with_capacity(count);
        for _ in 0..count {
            tab_locations.push(read_navigation_location(bytes, &mut offset)?);
        }
        windows.push(WindowSessionState {
            placement,
            active_tab,
            tab_locations,
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
        file_list_quick_search,
        new_tab_opens_home,
        quick_menu_backdrop,
        quick_menu_backdrop_opacity,
        sidebar_visibility,
        network_locations,
        network_devices,
    )
}

fn network_device_id(path: &Path) -> NetworkDeviceId {
    NetworkDeviceId(encode_os(path.as_os_str()))
}

fn validate_network_devices(values: &[NetworkDeviceTarget]) -> io::Result<()> {
    if values.len() > MAX_NETWORK_DEVICES {
        return Err(invalid_data("too many network devices"));
    }
    let mut identities = HashSet::with_capacity(values.len());
    for device in values {
        if device.display_name.trim().is_empty() || device.shell_identity.is_some() {
            return Err(invalid_data("invalid persisted network device"));
        }
        let Some(path) = device.unc_path.as_deref() else {
            return Err(invalid_data("network device target cannot be persisted"));
        };
        let identity = encode_os(path.as_os_str())
            .into_iter()
            .map(|unit| {
                if (b'A' as u16..=b'Z' as u16).contains(&unit) {
                    unit + (b'a' - b'A') as u16
                } else {
                    unit
                }
            })
            .collect::<Vec<_>>();
        if !crate::network::is_unc_server_root(path) || !identities.insert(identity) {
            return Err(invalid_data("invalid persisted network device target"));
        }
    }
    Ok(())
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

fn validate_navigation_location(location: &NavigationLocation) -> io::Result<()> {
    match location {
        NavigationLocation::Home => Ok(()),
        NavigationLocation::Directory(path) => validate_path(path),
        NavigationLocation::Library(library) => {
            if library.identity.is_empty() || library.display_name.trim().is_empty() {
                return Err(invalid_data("library display name cannot be empty"));
            }
            if encode_os(&library.identity).len() > MAX_PATH_UNITS
                || library.display_name.encode_utf16().count() > MAX_PATH_UNITS
            {
                return Err(invalid_data("library location is too long"));
            }
            Ok(())
        }
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
    bytes.extend_from_slice(&value.view_mode.icon_size().to_le_bytes());
    bytes.push(value.sort_field.storage_code());
    bytes.push(value.sort_direction.storage_code());
    bytes.push(value.group_field.storage_code());
    bytes.push(value.group_direction.storage_code());
    write_column_layout(bytes, value.columns);
}

fn write_search_preference(bytes: &mut Vec<u8>, value: SearchViewPreference) {
    bytes.push(value.view_mode.storage_code());
    bytes.extend_from_slice(&value.view_mode.icon_size().to_le_bytes());
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

fn read_view_mode(bytes: &[u8], offset: &mut usize) -> io::Result<ViewMode> {
    let code = read_u8(bytes, offset)?;
    let preset =
        ViewMode::from_storage_code(code).ok_or_else(|| invalid_data("invalid view mode"))?;
    let size = u16::from_le_bytes(read_array::<2>(bytes, offset)?);
    let mode = match preset {
        ViewMode::Icons(_) => ViewMode::Icons(size),
        _ if size == 0 => preset,
        _ => return Err(invalid_data("non-icon view has an icon size")),
    };
    if !mode.is_valid() || mode.storage_code() != code {
        return Err(invalid_data("invalid icon view size"));
    }
    Ok(mode)
}

fn read_directory_preference(
    bytes: &[u8],
    offset: &mut usize,
) -> io::Result<DirectoryViewPreference> {
    let value = DirectoryViewPreference {
        view_mode: read_view_mode(bytes, offset)?,
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
        view_mode: read_view_mode(bytes, offset)?,
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

fn write_navigation_location(bytes: &mut Vec<u8>, location: &NavigationLocation) -> io::Result<()> {
    match location {
        NavigationLocation::Directory(path) => {
            bytes.push(0);
            write_os(bytes, path.as_os_str())
        }
        NavigationLocation::Library(library) => {
            bytes.push(1);
            write_os(bytes, &library.identity)?;
            write_string(bytes, &library.display_name)
        }
        NavigationLocation::Home => {
            bytes.push(2);
            Ok(())
        }
    }
}

fn read_navigation_location(bytes: &[u8], offset: &mut usize) -> io::Result<NavigationLocation> {
    match read_u8(bytes, offset)? {
        0 => Ok(NavigationLocation::Directory(PathBuf::from(read_os(
            bytes, offset,
        )?))),
        1 => Ok(NavigationLocation::Library(
            crate::domain::LibraryLocationId::new(
                read_os(bytes, offset)?,
                read_string(bytes, offset)?,
            ),
        )),
        2 => Ok(NavigationLocation::Home),
        _ => Err(invalid_data("invalid navigation location kind")),
    }
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
                tab_locations: vec![
                    NavigationLocation::Directory(PathBuf::from(r"C:\项目\📁")),
                    NavigationLocation::Library(crate::domain::LibraryLocationId::new(
                        OsString::from(r"C:\Libraries\媒体.library-ms"),
                        "媒体".to_owned(),
                    )),
                    NavigationLocation::Home,
                ],
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
            true,
            true,
            false,
            42,
            SidebarVisibility {
                values: [true, false, true, false, true],
            },
            vec![NetworkLocation {
                id: 7,
                source: NetworkLocationSource::AsterOwned,
                display_name: "家庭 NAS".to_owned(),
                sort_order: 0,
                target: NetworkTarget::WindowsPath(PathBuf::from(r"\\NAS\媒体")),
                shell_path: None,
            }],
            vec![NetworkDeviceTarget {
                id: network_device_id(Path::new(r"\\LiuYanghomeNAS")),
                display_name: "LiuYanghomeNAS".to_owned(),
                shell_identity: None,
                unc_path: Some(PathBuf::from(r"\\LiuYanghomeNAS")),
            }],
        )
        .unwrap()
    }

    #[test]
    fn issue_86_astf16_round_trip_preserves_home_setting_network_locations_devices_and_raw_paths() {
        let state = sample_state();
        assert!(state.file_list_quick_search);
        assert!(state.new_tab_opens_home);
        assert!(!state.quick_menu_backdrop);
        assert_eq!(state.quick_menu_backdrop_opacity, 42);
        assert_eq!(
            state.sidebar_visibility,
            SidebarVisibility {
                values: [true, false, true, false, true],
            }
        );
        let decoded = decode(&encode(&state).unwrap()).unwrap();
        assert_eq!(decoded, state);
        let NavigationLocation::Library(library) = &decoded.windows[0].tab_locations[1] else {
            panic!("library location should survive the round trip");
        };
        assert_eq!(
            library.identity,
            OsString::from(r"C:\Libraries\媒体.library-ms")
        );
        assert_eq!(library.display_name, "媒体");
    }

    #[test]
    fn issue_86_home_location_uses_its_own_storage_tag() {
        let mut bytes = Vec::new();
        write_navigation_location(&mut bytes, &NavigationLocation::Home).unwrap();
        assert_eq!(bytes, vec![2]);
        assert_eq!(
            read_navigation_location(&bytes, &mut 0).unwrap(),
            NavigationLocation::Home
        );
    }
    #[test]
    fn rejects_invalid_library_locations_and_unknown_location_kind() {
        let mut state = sample_state();
        state.windows[0].tab_locations[1] = NavigationLocation::Library(
            crate::domain::LibraryLocationId::new(OsString::new(), "媒体".to_owned()),
        );
        assert_eq!(
            encode(&state).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let mut state = sample_state();
        state.windows[0].tab_locations[1] =
            NavigationLocation::Library(crate::domain::LibraryLocationId::new(
                OsString::from(r"C:\Libraries\媒体.library-ms"),
                "  ".to_owned(),
            ));
        assert_eq!(
            encode(&state).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        assert_eq!(
            read_navigation_location(&[3], &mut 0).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn sidebar_visibility_defaults_to_libraries_hidden() {
        let state = SessionState::new(
            WindowPlacement {
                x: 0,
                y: 0,
                width: 1180,
                height: 760,
            },
            0,
            vec![],
        )
        .unwrap();
        assert_eq!(state.sidebar_visibility, SidebarVisibility::default());
        assert_eq!(
            state.sidebar_visibility,
            SidebarVisibility {
                values: [true, false, true, true, true],
            }
        );
        assert_eq!(state.sidebar_visibility.storage_bits(), 0x1d);
        assert!(SidebarVisibility::from_storage_bits(0x20).is_none());
    }

    #[test]
    fn quick_search_defaults_off_and_quick_menu_backdrop_defaults_on() {
        let state = SessionState::new(
            WindowPlacement {
                x: 0,
                y: 0,
                width: 1180,
                height: 760,
            },
            0,
            vec![NavigationLocation::Directory(PathBuf::from(r"C:\work"))],
        )
        .unwrap();
        assert!(!state.file_list_quick_search);
        assert!(state.quick_menu_backdrop);
        assert_eq!(
            state.quick_menu_backdrop_opacity,
            DEFAULT_QUICK_MENU_BACKDROP_OPACITY
        );
    }

    #[test]
    fn issue_86_new_tab_home_setting_defaults_on_and_round_trips_off_for_multiple_windows() {
        let default_state = SessionState::new(
            WindowPlacement {
                x: 0,
                y: 0,
                width: 1180,
                height: 760,
            },
            0,
            vec![NavigationLocation::Home],
        )
        .unwrap();
        assert!(default_state.new_tab_opens_home);

        let mut state = sample_state();
        state.windows.push(WindowSessionState {
            placement: WindowPlacement {
                x: 40,
                y: 40,
                width: 1180,
                height: 760,
            },
            active_tab: 0,
            tab_locations: vec![NavigationLocation::Home],
        });
        state.new_tab_opens_home = false;
        let decoded = decode(&encode(&state).unwrap()).unwrap();
        assert!(!decoded.new_tab_opens_home);
        assert_eq!(decoded.windows.len(), 2);
        assert_eq!(
            decoded.windows[1].tab_locations,
            vec![NavigationLocation::Home]
        );
    }
    #[test]
    fn issue_97_restores_intermediate_icon_sizes() {
        let mut state = sample_state();
        state.default_directory_view.view_mode = ViewMode::Icons(72);
        state.search_view.view_mode = ViewMode::Icons(120);
        state.directory_views[0].1.view_mode = ViewMode::Icons(184);
        let bytes = encode(&state).unwrap();
        assert_eq!(&bytes[..6], b"ASTF17");
        assert_eq!(decode(&bytes).unwrap(), state);
    }

    #[test]
    fn issue_97_rejects_invalid_or_inconsistent_stored_icon_sizes() {
        for (code, size) in [
            (0, 16u16),
            (1, 48),
            (2, 0),
            (2, 15),
            (2, 96),
            (3, 24),
            (4, 48),
            (4, 256),
            (5, 255),
            (5, 257),
            (6, 16),
            (7, 16),
        ] {
            let mut bytes = vec![code];
            bytes.extend_from_slice(&size.to_le_bytes());
            assert_eq!(
                read_view_mode(&bytes, &mut 0).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        for size in [0, 15, 257, u16::MAX] {
            let mut state = sample_state();
            state.default_directory_view.view_mode = ViewMode::Icons(size);
            assert_eq!(
                encode(&state).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            state.default_directory_view = DirectoryViewPreference::default();
            state.search_view.view_mode = ViewMode::Icons(size);
            assert_eq!(
                encode(&state).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn issue_86_rejects_old_formats() {
        for version in 1..=16 {
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
    fn rejects_invalid_persisted_network_device() {
        let mut state = sample_state();
        state.network_devices[0].unc_path = Some(PathBuf::from(r"\\NAS\share"));
        assert_eq!(
            encode(&state).unwrap_err().kind(),
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
            vec![
                NavigationLocation::Directory(PathBuf::from(r"C:\one")),
                NavigationLocation::Directory(PathBuf::from(r"C:\two")),
            ],
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
