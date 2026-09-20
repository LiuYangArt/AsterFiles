use std::{
    ffi::{OsStr, OsString},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
};

use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;

pub fn normalize_address_path(input: &str) -> PathBuf {
    let trimmed = input.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(trimmed);
    let expanded = expand_environment_strings(OsStr::new(unquoted));
    let mut wide = expanded.encode_wide().collect::<Vec<_>>();
    for unit in &mut wide {
        if *unit == u16::from(b'/') {
            *unit = u16::from(b'\\');
        }
    }
    repair_drive_root_argv(&mut wide);
    PathBuf::from(OsString::from_wide(&wide))
}

/// Normalize folder paths from Windows Shell / `CommandLineToArgvW` before they become navigation
/// identity: repair `D:"`, bare `D:`, and the `"%1\."` template suffix.
pub fn normalize_external_launch_path(path: PathBuf) -> PathBuf {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.len() >= 2
        && wide[wide.len() - 2] == u16::from(b'\\')
        && wide[wide.len() - 1] == u16::from(b'.')
    {
        wide.truncate(wide.len() - 2);
    }
    repair_drive_root_argv(&mut wide);
    PathBuf::from(OsString::from_wide(&wide))
}

fn repair_drive_root_argv(wide: &mut Vec<u16>) {
    if wide.len() < 2 || !(wide[0] as u8).is_ascii_alphabetic() || wide[1] != u16::from(b':') {
        return;
    }
    match wide.len() {
        2 => wide.push(u16::from(b'\\')),
        3 if wide[2] == u16::from(b'"') => wide[2] = u16::from(b'\\'),
        _ => {}
    }
}

fn expand_environment_strings(input: &OsStr) -> OsString {
    if !input.encode_wide().any(|unit| unit == u16::from(b'%')) {
        return input.to_os_string();
    }
    let source = input
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let required = unsafe { ExpandEnvironmentStringsW(source.as_ptr(), std::ptr::null_mut(), 0) };
    if required == 0 {
        return input.to_os_string();
    }
    let mut output = vec![0u16; required as usize];
    let written = unsafe {
        ExpandEnvironmentStringsW(source.as_ptr(), output.as_mut_ptr(), output.len() as u32)
    };
    if written == 0 || written as usize > output.len() {
        return input.to_os_string();
    }
    OsString::from_wide(&output[..written.saturating_sub(1) as usize])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_drive_root_slashes_unc_and_outer_quotes() {
        assert_eq!(normalize_address_path("F:"), PathBuf::from(r"F:\"));
        assert_eq!(normalize_address_path(r#"D:""#), PathBuf::from(r"D:\"));
        assert_eq!(
            normalize_address_path(r#""F:/Assets/Mixed\Child""#),
            PathBuf::from(r"F:\Assets\Mixed\Child")
        );
        assert_eq!(
            normalize_address_path("//server/share/folder"),
            PathBuf::from(r"\\server\share\folder")
        );
    }

    #[test]
    fn external_launch_repairs_drive_root_quote_and_dot_suffix() {
        assert_eq!(
            normalize_external_launch_path(PathBuf::from(r#"D:""#)),
            PathBuf::from(r"D:\")
        );
        assert_eq!(
            normalize_external_launch_path(PathBuf::from("c:")),
            PathBuf::from(r"c:\")
        );
        assert_eq!(
            normalize_external_launch_path(PathBuf::from(r"D:\.")),
            PathBuf::from(r"D:\")
        );
        assert_eq!(
            normalize_external_launch_path(PathBuf::from(r"D:\Folder With Spaces\.")),
            PathBuf::from(r"D:\Folder With Spaces")
        );
        assert_eq!(
            normalize_external_launch_path(PathBuf::from(r"D:\中文")),
            PathBuf::from(r"D:\中文")
        );
    }

    #[test]
    fn expands_windows_environment_variables_before_normalizing_slashes() {
        let user_profile = std::env::var_os("USERPROFILE").expect("USERPROFILE is set on Windows");
        assert_eq!(
            normalize_address_path(r"%USERPROFILE%/Documents"),
            PathBuf::from(user_profile).join("Documents")
        );
    }

    #[test]
    fn unknown_environment_variable_is_left_for_the_caller_to_reject() {
        assert_eq!(
            normalize_address_path(r"%ASTERFILES_UNKNOWN_VARIABLE%/Documents"),
            PathBuf::from(r"%ASTERFILES_UNKNOWN_VARIABLE%\Documents")
        );
    }
}
