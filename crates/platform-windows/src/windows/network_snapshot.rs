use std::{io, path::Path, time::Duration};

pub(crate) const MAX_HELPER_UTF16_UNITS: usize = 32767;

pub(crate) fn snapshot_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

// Locks cover only small local IPC snapshots; never wait on network I/O.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::{Seek, Write};
    let write = || -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let started = std::time::Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock)
                    if started.elapsed() < Duration::from_secs(1) =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "local IPC snapshot lock timed out",
                    ));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error),
            }
        }
        file.rewind()?;
        file.write_all(bytes)?;
        file.set_len(bytes.len() as u64)?;
        Ok(())
    };
    write().map_err(|error| snapshot_error(path, error))
}

pub(crate) fn read_snapshot(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let read = || -> io::Result<Vec<u8>> {
        let file = std::fs::File::open(path)?;
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(io::ErrorKind::WouldBlock.into()),
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
        let mut bytes = Vec::new();
        file.take(1_048_577).read_to_end(&mut bytes)?;
        if bytes.is_empty() {
            // A newly created snapshot is unpublished until its first locked write.
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if bytes.len() > 1_048_576 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC snapshot exceeds size limit",
            ));
        }
        Ok(bytes)
    };
    read().map_err(|error| snapshot_error(path, error))
}

pub(crate) fn write_units(bytes: &mut Vec<u8>, units: &[u16]) -> io::Result<()> {
    if units.len() > MAX_HELPER_UTF16_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "helper item too long",
        ));
    }
    bytes.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(())
}

pub(crate) fn read_units_at(bytes: &[u8], offset: &mut usize) -> io::Result<Vec<u16>> {
    let count = read_u32(bytes, offset)? as usize;
    if count > MAX_HELPER_UTF16_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "UTF-16 value too long",
        ));
    }
    let end = offset
        .checked_add(
            count
                .checked_mul(2)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "path too long"))?,
        )
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "path too long"))?;
    if end > bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated UTF-16 data",
        ));
    }
    let mut units = Vec::with_capacity(count);
    while *offset < end {
        units.push(u16::from_le_bytes([bytes[*offset], bytes[*offset + 1]]));
        *offset += 2;
    }
    Ok(units)
}

pub(crate) fn read_u64(bytes: &[u8], offset: &mut usize) -> io::Result<u64> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid length"))?;
    let value = bytes
        .get(*offset..end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated number"))?;
    *offset = end;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}

pub(crate) fn read_u32(bytes: &[u8], offset: &mut usize) -> io::Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid length"))?;
    let value = bytes
        .get(*offset..end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated length"))?;
    *offset = end;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}
