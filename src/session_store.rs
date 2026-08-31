use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
};

use crate::{
    domain::{EverythingConfig, FileVisibility},
    i18n::Language,
};

#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};

const MAGIC: &[u8; 5] = b"ASTF8";

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
pub type ColumnOrder = [u8; 4];
pub type SearchColumnOrder = [u8; 4];
pub type ColumnWidths = [u32; 4];
pub type SearchColumnWidths = [u32; 4];
pub const DEFAULT_COLUMN_ORDER: ColumnOrder = [0, 1, 2, 3];
#[allow(dead_code)]
pub const DEFAULT_SEARCH_COLUMN_ORDER: SearchColumnOrder = [0, 1, 2, 3];
pub const DEFAULT_COLUMN_WIDTHS: ColumnWidths = [480, 160, 120, 200];
pub const DEFAULT_SEARCH_COLUMN_WIDTHS: SearchColumnWidths = [400, 320, 120, 200];
const MIN_COLUMN_WIDTH: u32 = 64;
const MAX_COLUMN_WIDTH: u32 = 4_096;
const MAX_TABS: usize = 1_024;
const MAX_WINDOWS: usize = 128;
const MAX_PATH_UNITS: usize = 32_767;
const MIN_WINDOW_WIDTH: u32 = 820;
const MIN_WINDOW_HEIGHT: u32 = 520;
const MAX_WINDOW_WIDTH: u32 = 7_680;
const MAX_WINDOW_HEIGHT: u32 = 4_320;

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
    pub column_order: ColumnOrder,
    pub search_column_order: SearchColumnOrder,
    pub column_widths: ColumnWidths,
    pub search_column_widths: SearchColumnWidths,
    pub theme_mode: ThemeMode,
    pub language: Language,
    pub everything: EverythingConfig,
    pub file_visibility: FileVisibility,
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
        column_order: ColumnOrder,
    ) -> io::Result<Self> {
        Self::with_settings(
            window,
            active_tab,
            tab_paths,
            column_order,
            ThemeMode::System,
            Language::Chinese,
        )
    }

    #[allow(dead_code)]
    pub fn with_settings(
        window: WindowPlacement,
        active_tab: usize,
        tab_paths: Vec<PathBuf>,
        column_order: ColumnOrder,
        theme_mode: ThemeMode,
        language: Language,
    ) -> io::Result<Self> {
        Self::with_everything_settings(
            window,
            active_tab,
            tab_paths,
            column_order,
            DEFAULT_SEARCH_COLUMN_ORDER,
            DEFAULT_COLUMN_WIDTHS,
            DEFAULT_SEARCH_COLUMN_WIDTHS,
            theme_mode,
            language,
            EverythingConfig::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_everything_settings(
        window: WindowPlacement,
        active_tab: usize,
        tab_paths: Vec<PathBuf>,
        column_order: ColumnOrder,
        search_column_order: SearchColumnOrder,
        column_widths: ColumnWidths,
        search_column_widths: SearchColumnWidths,
        theme_mode: ThemeMode,
        language: Language,
        everything: EverythingConfig,
    ) -> io::Result<Self> {
        Self::with_file_visibility_settings(
            window,
            active_tab,
            tab_paths,
            column_order,
            search_column_order,
            column_widths,
            search_column_widths,
            theme_mode,
            language,
            everything,
            FileVisibility::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_file_visibility_settings(
        window: WindowPlacement,
        active_tab: usize,
        tab_paths: Vec<PathBuf>,
        column_order: ColumnOrder,
        search_column_order: SearchColumnOrder,
        column_widths: ColumnWidths,
        search_column_widths: SearchColumnWidths,
        theme_mode: ThemeMode,
        language: Language,
        everything: EverythingConfig,
        file_visibility: FileVisibility,
    ) -> io::Result<Self> {
        Self::with_windows_and_settings(
            vec![WindowSessionState {
                placement: window,
                active_tab,
                tab_paths,
            }],
            column_order,
            search_column_order,
            column_widths,
            search_column_widths,
            theme_mode,
            language,
            everything,
            file_visibility,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_windows_and_settings(
        mut windows: Vec<WindowSessionState>,
        column_order: ColumnOrder,
        search_column_order: SearchColumnOrder,
        column_widths: ColumnWidths,
        search_column_widths: SearchColumnWidths,
        theme_mode: ThemeMode,
        language: Language,
        everything: EverythingConfig,
        file_visibility: FileVisibility,
    ) -> io::Result<Self> {
        if windows.is_empty() || windows.len() > MAX_WINDOWS {
            return Err(invalid_data("invalid session window count"));
        }
        for window in &mut windows {
            validate_window(window.placement)?;
            if window.tab_paths.len() > MAX_TABS {
                return Err(invalid_data("invalid session tab count"));
            }
            window.active_tab = if window.tab_paths.is_empty() {
                0
            } else {
                window.active_tab.min(window.tab_paths.len() - 1)
            };
        }
        validate_column_order(column_order)?;
        validate_column_order(search_column_order)?;
        validate_column_widths(column_widths)?;
        validate_column_widths(search_column_widths)?;
        validate_everything_config(&everything)?;
        Ok(Self {
            windows,
            column_order,
            search_column_order,
            column_widths,
            search_column_widths,
            theme_mode,
            language,
            everything,
            file_visibility,
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
        state.column_order,
        state.search_column_order,
        state.column_widths,
        state.search_column_widths,
        state.theme_mode,
        state.language,
        state.everything.clone(),
        state.file_visibility,
    )?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(state.windows.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&state.column_order);
    bytes.extend_from_slice(&state.search_column_order);
    write_four_u32(&mut bytes, state.column_widths);
    write_four_u32(&mut bytes, state.search_column_widths);
    bytes.push(state.theme_mode.storage_code());
    bytes.push(state.language.storage_code());
    write_optional_os(&mut bytes, state.everything.executable_path.as_deref())?;
    write_string(&mut bytes, &state.everything.instance_name)?;
    write_optional_string(&mut bytes, state.everything.verified_version.as_deref())?;
    bytes.push(u8::from(state.everything.allow_launch));
    bytes.push(u8::from(state.file_visibility.show_hidden));
    bytes.push(u8::from(state.file_visibility.show_system));

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
    let column_order = read_four(bytes, &mut offset)?;
    let search_column_order = read_four(bytes, &mut offset)?;
    let column_widths = read_four_u32(bytes, &mut offset)?;
    let search_column_widths = read_four_u32(bytes, &mut offset)?;
    let theme_mode = ThemeMode::from_storage_code(read_u8(bytes, &mut offset)?)
        .ok_or_else(|| invalid_data("invalid session theme mode"))?;
    let language = Language::from_storage_code(read_u8(bytes, &mut offset)?)
        .ok_or_else(|| invalid_data("invalid session language"))?;
    let executable_path = read_optional_os(bytes, &mut offset)?.map(PathBuf::from);
    let instance_name = read_string(bytes, &mut offset)?;
    let verified_version = read_optional_string(bytes, &mut offset)?;
    let allow_launch = match read_u8(bytes, &mut offset)? {
        0 => false,
        1 => true,
        _ => return Err(invalid_data("invalid Everything launch setting")),
    };
    let everything = EverythingConfig {
        executable_path,
        instance_name,
        verified_version,
        allow_launch,
    };
    let file_visibility = FileVisibility {
        show_hidden: read_bool(bytes, &mut offset, "invalid hidden-file setting")?,
        show_system: read_bool(bytes, &mut offset, "invalid system-file setting")?,
    };

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
        column_order,
        search_column_order,
        column_widths,
        search_column_widths,
        theme_mode,
        language,
        everything,
        file_visibility,
    )
}

fn validate_column_order(column_order: ColumnOrder) -> io::Result<()> {
    let mut seen = [false; DEFAULT_COLUMN_ORDER.len()];
    for column in column_order {
        let index = usize::from(column);
        if index >= seen.len() || seen[index] {
            return Err(invalid_data("invalid session column order"));
        }
        seen[index] = true;
    }
    Ok(())
}

fn validate_column_widths(column_widths: ColumnWidths) -> io::Result<()> {
    if column_widths
        .into_iter()
        .any(|width| !(MIN_COLUMN_WIDTH..=MAX_COLUMN_WIDTH).contains(&width))
    {
        return Err(invalid_data("invalid session column width"));
    }
    Ok(())
}
fn validate_everything_config(config: &EverythingConfig) -> io::Result<()> {
    if config.instance_name.encode_utf16().count() > MAX_PATH_UNITS
        || config
            .verified_version
            .as_deref()
            .is_some_and(|version| version.encode_utf16().count() > MAX_PATH_UNITS)
    {
        return Err(invalid_data("Everything setting is too long"));
    }
    if config
        .executable_path
        .as_deref()
        .is_some_and(|path| encode_os(path.as_os_str()).len() > MAX_PATH_UNITS)
    {
        return Err(invalid_data("Everything executable path is too long"));
    }
    Ok(())
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

fn write_four_u32(bytes: &mut Vec<u8>, values: [u32; 4]) {
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
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
        .map_err(|_| invalid_data("invalid stored string"))
}

fn read_optional_string(bytes: &[u8], offset: &mut usize) -> io::Result<Option<String>> {
    match read_u8(bytes, offset)? {
        0 => Ok(None),
        1 => read_string(bytes, offset).map(Some),
        _ => Err(invalid_data("invalid optional string")),
    }
}

fn read_units(bytes: &[u8], offset: &mut usize) -> io::Result<Vec<u16>> {
    let length = read_u32(bytes, offset)? as usize;
    if length > MAX_PATH_UNITS {
        return Err(invalid_data("stored value is too long"));
    }
    let byte_length = length
        .checked_mul(2)
        .ok_or_else(|| invalid_data("invalid stored value length"))?;
    let end = offset
        .checked_add(byte_length)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| invalid_data("truncated session"))?;
    let units = bytes[*offset..end]
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    *offset = end;
    Ok(units)
}
fn read_four_u32(bytes: &[u8], offset: &mut usize) -> io::Result<[u32; 4]> {
    Ok([
        read_u32(bytes, offset)?,
        read_u32(bytes, offset)?,
        read_u32(bytes, offset)?,
        read_u32(bytes, offset)?,
    ])
}
fn validate_window(window: WindowPlacement) -> io::Result<()> {
    if window.width < MIN_WINDOW_WIDTH
        || window.height < MIN_WINDOW_HEIGHT
        || window.width > MAX_WINDOW_WIDTH
        || window.height > MAX_WINDOW_HEIGHT
    {
        return Err(invalid_data("invalid session window size"));
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
    let value = bytes
        .get(*offset)
        .copied()
        .ok_or_else(|| invalid_data("truncated session"))?;
    *offset += 1;
    Ok(value)
}

fn read_i32(bytes: &[u8], offset: &mut usize) -> io::Result<i32> {
    Ok(i32::from_le_bytes(read_four(bytes, offset)?))
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(read_four(bytes, offset)?))
}

fn read_four(bytes: &[u8], offset: &mut usize) -> io::Result<[u8; 4]> {
    let end = offset
        .checked_add(4)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| invalid_data("truncated session"))?;
    let value = bytes[*offset..end]
        .try_into()
        .expect("four-byte slice has fixed size");
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
        SessionState::with_settings(
            WindowPlacement {
                x: -120,
                y: 84,
                width: 1440,
                height: 900,
            },
            1,
            vec![
                PathBuf::from(r"C:\中文\📁"),
                PathBuf::from(r"\\server\共享\資料"),
            ],
            [2, 0, 3, 1],
            ThemeMode::Dark,
            Language::English,
        )
        .expect("valid state")
    }

    #[test]
    fn round_trips_complete_session() {
        let state = sample_state();
        assert_eq!(decode(&encode(&state).expect("encodable")).unwrap(), state);
    }

    #[test]
    fn round_trips_multiple_windows_with_independent_layout_and_order() {
        let mut state = sample_state();
        state.windows.push(WindowSessionState {
            placement: WindowPlacement {
                x: 420,
                y: 180,
                width: 1024,
                height: 700,
            },
            active_tab: 0,
            tab_paths: vec![PathBuf::from(r"D:\three"), PathBuf::from(r"D:\four")],
        });
        let restored = decode(&encode(&state).unwrap()).unwrap();
        assert_eq!(restored, state);
        assert_eq!(restored.windows[1].tab_paths[0], PathBuf::from(r"D:\three"));
    }

    #[test]
    fn default_constructor_uses_system_theme_and_chinese() {
        let state = SessionState::new(
            WindowPlacement {
                x: 0,
                y: 0,
                width: 900,
                height: 600,
            },
            0,
            Vec::new(),
            DEFAULT_COLUMN_ORDER,
        )
        .unwrap();

        assert_eq!(state.theme_mode, ThemeMode::System);
        assert_eq!(state.language, Language::Chinese);
        assert_eq!(state.column_widths, DEFAULT_COLUMN_WIDTHS);
        assert_eq!(state.search_column_widths, DEFAULT_SEARCH_COLUMN_WIDTHS);
        assert_eq!(state.file_visibility, FileVisibility::default());
    }

    #[test]
    fn everything_settings_and_search_columns_round_trip() {
        let state = SessionState::with_everything_settings(
            WindowPlacement {
                x: 20,
                y: 30,
                width: 1200,
                height: 800,
            },
            0,
            vec![PathBuf::from(r"\\LiuYanghomeNAS\Multimedia")],
            DEFAULT_COLUMN_ORDER,
            [1, 0, 3, 2],
            [560, 180, 128, 240],
            [440, 420, 128, 240],
            ThemeMode::Dark,
            Language::English,
            EverythingConfig {
                executable_path: Some(PathBuf::from(
                    r"C:\Program Files\Everything 1.5a\Everything64.exe",
                )),
                instance_name: "1.5a 特殊".to_owned(),
                verified_version: Some("1.5.0.1396a x64".to_owned()),
                allow_launch: false,
            },
        )
        .unwrap();

        assert_eq!(decode(&encode(&state).unwrap()).unwrap(), state);
    }
    #[test]
    fn file_visibility_settings_round_trip_independently() {
        let mut state = sample_state();
        state.file_visibility = FileVisibility {
            show_hidden: false,
            show_system: true,
        };

        assert_eq!(decode(&encode(&state).unwrap()).unwrap(), state);
    }

    #[test]
    fn rejects_invalid_file_visibility_flags() {
        let mut bytes = encode(&sample_state()).unwrap();
        let flags_offset = bytes.len()
            - sample_state().windows[0]
                .tab_paths
                .iter()
                .map(|path| 4 + encode_os(path.as_os_str()).len() * 2)
                .sum::<usize>()
            - 2;
        bytes[flags_offset] = 2;
        assert_eq!(
            decode(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let mut bytes = encode(&sample_state()).unwrap();
        bytes[flags_offset + 1] = 2;
        assert_eq!(
            decode(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn setting_storage_codes_are_stable() {
        assert_eq!(ThemeMode::System.storage_code(), 0);
        assert_eq!(ThemeMode::Light.storage_code(), 1);
        assert_eq!(ThemeMode::Dark.storage_code(), 2);
        assert_eq!(ThemeMode::from_storage_code(0), Some(ThemeMode::System));
        assert_eq!(ThemeMode::from_storage_code(1), Some(ThemeMode::Light));
        assert_eq!(ThemeMode::from_storage_code(2), Some(ThemeMode::Dark));
        assert_eq!(ThemeMode::from_storage_code(3), None);
        assert_eq!(ThemeMode::from_storage_code(u8::MAX), None);
    }

    #[test]
    fn rejects_invalid_setting_codes() {
        let mut invalid_theme = encode(&sample_state()).unwrap();
        invalid_theme[49] = u8::MAX;
        assert_eq!(
            decode(&invalid_theme).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let mut invalid_language = encode(&sample_state()).unwrap();
        invalid_language[50] = u8::MAX;
        assert_eq!(
            decode(&invalid_language).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(windows)]
    #[test]
    fn round_trips_unpaired_utf16_path_units() {
        let raw_path =
            OsString::from_wide(&[b'C' as u16, b':' as u16, b'\\' as u16, 0xd800, b'x' as u16]);
        let state = SessionState::new(
            WindowPlacement {
                x: 0,
                y: 0,
                width: 900,
                height: 600,
            },
            0,
            vec![PathBuf::from(raw_path)],
            DEFAULT_COLUMN_ORDER,
        )
        .unwrap();

        assert_eq!(decode(&encode(&state).unwrap()).unwrap(), state);
    }

    #[test]
    fn saves_over_an_existing_session_file() {
        let directory =
            std::env::temp_dir().join(format!("asterfiles-session-store-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("session.bin");
        fs::write(&path, b"ASTF2 stale session").unwrap();

        let state = sample_state();
        save(&path, &state).unwrap();
        assert_eq!(load(&path).unwrap(), state);

        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn rejects_truncated_data() {
        let encoded = encode(&sample_state()).unwrap();
        for length in 0..encoded.len() {
            assert_eq!(
                decode(&encoded[..length]).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "prefix length {length} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_invalid_window_dimensions() {
        let mut state = sample_state();
        state.windows[0].placement.width = 0;
        assert_eq!(
            encode(&state).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_unreasonable_window_dimensions() {
        for window in [
            WindowPlacement {
                x: 0,
                y: 0,
                width: 819,
                height: 760,
            },
            WindowPlacement {
                x: 0,
                y: 0,
                width: 1180,
                height: 519,
            },
            WindowPlacement {
                x: 0,
                y: 0,
                width: 7_681,
                height: 760,
            },
            WindowPlacement {
                x: 0,
                y: 0,
                width: 1180,
                height: 4_321,
            },
        ] {
            assert_eq!(
                SessionState::new(window, 0, vec![PathBuf::from(r"C:\")], DEFAULT_COLUMN_ORDER,)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
    #[test]
    fn corrects_active_tab_index() {
        let state = SessionState::new(
            WindowPlacement {
                x: 0,
                y: 0,
                width: 900,
                height: 600,
            },
            99,
            vec![PathBuf::from(r"C:\one"), PathBuf::from(r"C:\two")],
            DEFAULT_COLUMN_ORDER,
        )
        .unwrap();
        assert_eq!(state.windows[0].active_tab, 1);
    }

    #[test]
    fn empty_session_uses_zero_active_tab() {
        let state = SessionState::new(
            WindowPlacement {
                x: 0,
                y: 0,
                width: 900,
                height: 600,
            },
            99,
            Vec::new(),
            DEFAULT_COLUMN_ORDER,
        )
        .unwrap();
        assert_eq!(state.windows[0].active_tab, 0);
    }

    #[test]
    fn rejects_invalid_column_orders() {
        for column_order in [[0, 1, 2, 2], [0, 1, 2, 4]] {
            assert_eq!(
                SessionState::new(
                    WindowPlacement {
                        x: 0,
                        y: 0,
                        width: 900,
                        height: 600,
                    },
                    0,
                    Vec::new(),
                    column_order,
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::InvalidData
            );
        }

        let mut bytes = encode(&sample_state()).unwrap();
        bytes[9..13].copy_from_slice(&[0, 1, 1, 3]);
        assert_eq!(
            decode(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut bytes = encode(&sample_state()).unwrap();
        bytes[13..17].copy_from_slice(&[0, 1, 1, 3]);
        assert_eq!(
            decode(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn accepts_column_width_boundaries() {
        let mut state = sample_state();
        state.column_widths = [MIN_COLUMN_WIDTH, 160, 120, MAX_COLUMN_WIDTH];
        state.search_column_widths = [MAX_COLUMN_WIDTH, 320, 120, MIN_COLUMN_WIDTH];

        assert_eq!(decode(&encode(&state).unwrap()).unwrap(), state);
    }

    #[test]
    fn rejects_invalid_column_widths() {
        for invalid_width in [MIN_COLUMN_WIDTH - 1, MAX_COLUMN_WIDTH + 1] {
            let mut state = sample_state();
            state.column_widths[0] = invalid_width;
            assert_eq!(
                encode(&state).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );

            let mut state = sample_state();
            state.search_column_widths[1] = invalid_width;
            assert_eq!(
                encode(&state).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }

        let mut bytes = encode(&sample_state()).unwrap();
        bytes[17..21].copy_from_slice(&(MIN_COLUMN_WIDTH - 1).to_le_bytes());
        assert_eq!(
            decode(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let mut bytes = encode(&sample_state()).unwrap();
        bytes[33..37].copy_from_slice(&(MAX_COLUMN_WIDTH + 1).to_le_bytes());
        assert_eq!(
            decode(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn rejects_old_format() {
        for bytes in [
            b"ASTF1\0\0\0\0".as_slice(),
            b"ASTF2\0\0\0\0".as_slice(),
            b"ASTF3\0\0\0\0".as_slice(),
            b"ASTF4\0\0\0\0".as_slice(),
            b"ASTF5\0\0\0\0".as_slice(),
            b"ASTF6\0\0\0\0".as_slice(),
            b"ASTF7\0\0\0\0".as_slice(),
        ] {
            assert_eq!(
                decode(bytes).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
