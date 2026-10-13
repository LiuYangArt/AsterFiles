//! WebDAV passwords stay in Windows Credential Manager, addressed only by connection id.
use std::io;

#[cfg(windows)]
const TARGET_PREFIX: &str = "AsterFiles:WebDAV:";

pub fn store(connection_id: u64, username: &str, password: &str) -> io::Result<()> {
    #[cfg(windows)]
    {
        write_generic(connection_id, username, password)
    }
    #[cfg(not(windows))]
    {
        let _ = (connection_id, username, password);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WebDAV credentials require Windows",
        ))
    }
}

pub fn load(connection_id: u64) -> io::Result<Option<String>> {
    #[cfg(windows)]
    {
        read_generic(connection_id)
    }
    #[cfg(not(windows))]
    {
        let _ = connection_id;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WebDAV credentials require Windows",
        ))
    }
}

pub fn delete(connection_id: u64) -> io::Result<()> {
    #[cfg(windows)]
    {
        delete_generic(connection_id)
    }
    #[cfg(not(windows))]
    {
        let _ = connection_id;
        Ok(())
    }
}

#[cfg(windows)]
fn write_generic(connection_id: u64, username: &str, password: &str) -> io::Result<()> {
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredWriteW,
    };

    let mut target = wide_null(&format!("{TARGET_PREFIX}{connection_id}"));
    let mut user = wide_null(username);
    let mut secret = password.as_bytes().to_vec();
    if secret.len() > 2560 {
        secret.fill(0);
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WebDAV password is too long",
        ));
    }
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_mut_ptr(),
        CredentialBlobSize: secret.len() as u32,
        CredentialBlob: secret.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: user.as_mut_ptr(),
        ..Default::default()
    };
    let written = unsafe { CredWriteW(&credential, 0) };
    secret.fill(0);
    if written == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn read_generic(connection_id: u64) -> io::Result<Option<String>> {
    use windows_sys::Win32::{
        Foundation::ERROR_NOT_FOUND,
        Security::Credentials::{CRED_TYPE_GENERIC, CredFree, CredReadW},
    };

    let mut target = wide_null(&format!("{TARGET_PREFIX}{connection_id}"));
    let mut credential = std::ptr::null_mut();
    let found = unsafe { CredReadW(target.as_mut_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
    if found == 0 {
        let code = io::Error::last_os_error();
        return if code.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
            Ok(None)
        } else {
            Err(code)
        };
    }
    let value = unsafe {
        let header = &*credential;
        let bytes =
            std::slice::from_raw_parts(header.CredentialBlob, header.CredentialBlobSize as usize);
        let password = String::from_utf8(bytes.to_vec()).ok();
        CredFree(credential.cast());
        password
    };
    Ok(value)
}

#[cfg(windows)]
fn delete_generic(connection_id: u64) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::ERROR_NOT_FOUND,
        Security::Credentials::{CRED_TYPE_GENERIC, CredDeleteW},
    };

    let mut target = wide_null(&format!("{TARGET_PREFIX}{connection_id}"));
    let deleted = unsafe { CredDeleteW(target.as_mut_ptr(), CRED_TYPE_GENERIC, 0) };
    if deleted != 0 {
        return Ok(());
    }
    let code = io::Error::last_os_error();
    if code.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
        Ok(())
    } else {
        Err(code)
    }
}

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn webdav_password_roundtrip_stays_out_of_the_target_name() {
        let id = 0x1570_0000_u64.wrapping_add(std::process::id() as u64);
        let password = "webdav-test-secret";
        store(id, "reader", password).unwrap();
        let loaded = load(id).unwrap();
        delete(id).unwrap();
        assert_eq!(loaded.as_deref(), Some(password));
        assert!(load(id).unwrap().is_none());
        let target = format!("AsterFiles:WebDAV:{id}");
        assert!(!target.contains(password));
    }
}
