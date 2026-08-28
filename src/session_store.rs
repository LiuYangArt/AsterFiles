use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};

const MAGIC: &[u8; 5] = b"ASTF1";

pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("AsterFiles").join("session.bin"))
}

pub fn load(path: &Path) -> io::Result<Vec<PathBuf>> {
    let bytes = fs::read(path)?;
    decode(&bytes)
}

pub fn save(path: &Path, paths: &[PathBuf]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, encode(paths))?;
    fs::rename(temporary, path)
}

fn encode(paths: &[PathBuf]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(paths.len() as u32).to_le_bytes());
    for path in paths {
        let units = encode_os(path.as_os_str());
        bytes.extend_from_slice(&(units.len() as u32).to_le_bytes());
        for unit in units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    bytes
}

fn decode(bytes: &[u8]) -> io::Result<Vec<PathBuf>> {
    if bytes.len() < 9 || &bytes[..5] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid AsterFiles session",
        ));
    }
    let mut offset = 5;
    let count = read_u32(bytes, &mut offset)? as usize;
    let mut paths = Vec::with_capacity(count);
    for _ in 0..count {
        let length = read_u32(bytes, &mut offset)? as usize;
        let end = offset
            .checked_add(length.saturating_mul(2))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated session"))?;
        let units = bytes[offset..end]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        paths.push(PathBuf::from(decode_os(&units)));
        offset = end;
    }
    Ok(paths)
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> io::Result<u32> {
    let end = offset
        .checked_add(4)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated session"))?;
    let value = u32::from_le_bytes(
        bytes[*offset..end]
            .try_into()
            .expect("four-byte slice has fixed size"),
    );
    *offset = end;
    Ok(value)
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

    #[test]
    fn round_trips_unicode_paths() {
        let paths = vec![
            PathBuf::from(r"C:\中文\📁"),
            PathBuf::from(r"\\server\共享\資料"),
        ];
        assert_eq!(decode(&encode(&paths)).expect("valid session"), paths);
    }

    #[test]
    fn rejects_truncated_data() {
        assert_eq!(
            decode(MAGIC).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
