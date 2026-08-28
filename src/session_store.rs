use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};

const MAGIC: &[u8; 5] = b"ASTF2";
const MAX_TABS: usize = 1_024;
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
    pub window: WindowPlacement,
    pub active_tab: usize,
    pub tab_paths: Vec<PathBuf>,
}

impl SessionState {
    pub fn new(
        window: WindowPlacement,
        active_tab: usize,
        tab_paths: Vec<PathBuf>,
    ) -> io::Result<Self> {
        validate_window(window)?;
        if tab_paths.len() > MAX_TABS {
            return Err(invalid_data("too many session tabs"));
        }

        let active_tab = if tab_paths.is_empty() {
            0
        } else {
            active_tab.min(tab_paths.len() - 1)
        };

        Ok(Self {
            window,
            active_tab,
            tab_paths,
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
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, encode(state)?)?;
    fs::rename(temporary, path)
}

fn encode(state: &SessionState) -> io::Result<Vec<u8>> {
    let state = SessionState::new(state.window, state.active_tab, state.tab_paths.clone())?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&state.window.x.to_le_bytes());
    bytes.extend_from_slice(&state.window.y.to_le_bytes());
    bytes.extend_from_slice(&state.window.width.to_le_bytes());
    bytes.extend_from_slice(&state.window.height.to_le_bytes());
    bytes.extend_from_slice(&(state.active_tab as u32).to_le_bytes());
    bytes.extend_from_slice(&(state.tab_paths.len() as u32).to_le_bytes());

    for path in &state.tab_paths {
        let units = encode_os(path.as_os_str());
        if units.len() > MAX_PATH_UNITS {
            return Err(invalid_data("session path is too long"));
        }
        bytes.extend_from_slice(&(units.len() as u32).to_le_bytes());
        for unit in units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> io::Result<SessionState> {
    if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
        return Err(invalid_data("invalid AsterFiles session"));
    }

    let mut offset = MAGIC.len();
    let window = WindowPlacement {
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
        let length = read_u32(bytes, &mut offset)? as usize;
        if length > MAX_PATH_UNITS {
            return Err(invalid_data("session path is too long"));
        }
        let byte_length = length
            .checked_mul(2)
            .ok_or_else(|| invalid_data("invalid session path length"))?;
        let end = offset
            .checked_add(byte_length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| invalid_data("truncated session"))?;
        let units = bytes[offset..end]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        tab_paths.push(PathBuf::from(decode_os(&units)));
        offset = end;
    }

    if offset != bytes.len() {
        return Err(invalid_data("unexpected trailing session data"));
    }

    SessionState::new(window, active_tab, tab_paths)
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
        SessionState::new(
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
        )
        .expect("valid state")
    }

    #[test]
    fn round_trips_complete_session() {
        let state = sample_state();
        assert_eq!(decode(&encode(&state).expect("encodable")).unwrap(), state);
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
        )
        .unwrap();

        assert_eq!(decode(&encode(&state).unwrap()).unwrap(), state);
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
        state.window.width = 0;
        assert_eq!(
            encode(&state).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let mut bytes = encode(&sample_state()).unwrap();
        bytes[13..17].copy_from_slice(&0_u32.to_le_bytes());
        assert_eq!(
            decode(&bytes).unwrap_err().kind(),
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
                SessionState::new(window, 0, vec![PathBuf::from(r"C:\")])
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
        )
        .unwrap();
        assert_eq!(state.active_tab, 1);

        let mut bytes = encode(&state).unwrap();
        bytes[21..25].copy_from_slice(&99_u32.to_le_bytes());
        assert_eq!(decode(&bytes).unwrap().active_tab, 1);
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
        )
        .unwrap();
        assert_eq!(state.active_tab, 0);
    }

    #[test]
    fn rejects_old_format() {
        assert_eq!(
            decode(b"ASTF1\0\0\0\0").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
