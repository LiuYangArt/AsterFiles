use std::{
    ffi::{OsStr, OsString},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalLaunchPath {
    pub path: PathBuf,
    pub select: bool,
}

impl ExternalLaunchPath {
    pub fn open(path: PathBuf) -> Self {
        Self {
            path: normalize_external_launch_path(path),
            select: false,
        }
    }

    pub fn select(path: PathBuf) -> Self {
        Self {
            path: normalize_external_launch_path(path),
            select: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassifiedExternalLaunch {
    Directory(PathBuf),
    Reveal { parent: PathBuf, target: PathBuf },
}

impl ClassifiedExternalLaunch {
    pub fn directory(&self) -> &Path {
        match self {
            Self::Directory(path) => path,
            Self::Reveal { parent, .. } => parent,
        }
    }

    pub fn reveal_target(&self) -> Option<&Path> {
        match self {
            Self::Reveal { target, .. } => Some(target),
            Self::Directory(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedExternalArgument {
    SelectNext,
    Launch(ExternalLaunchPath),
}

pub fn parse_external_argument(argument: &OsStr) -> ParsedExternalArgument {
    let wide = argument.encode_wide().collect::<Vec<_>>();
    if let Some(kind) = match_select_prefix(&wide) {
        return match kind {
            SelectPrefix::Flag => ParsedExternalArgument::SelectNext,
            SelectPrefix::Inline(path) => ParsedExternalArgument::Launch(
                ExternalLaunchPath::select(PathBuf::from(strip_outer_quotes(path))),
            ),
        };
    }
    ParsedExternalArgument::Launch(ExternalLaunchPath::open(PathBuf::from(argument)))
}

pub fn classify_external_launch_path(item: &ExternalLaunchPath) -> ClassifiedExternalLaunch {
    let is_directory = std::fs::metadata(&item.path)
        .ok()
        .map(|metadata| metadata.is_dir());
    classify_external_launch_kind(item, is_directory)
}

pub fn classify_external_launches(items: &[ExternalLaunchPath]) -> Vec<ClassifiedExternalLaunch> {
    items.iter().map(classify_external_launch_path).collect()
}

pub fn classify_external_launches_off_thread(
    items: Vec<ExternalLaunchPath>,
) -> Vec<ClassifiedExternalLaunch> {
    if items.is_empty() {
        return Vec::new();
    }
    let worker_items = items.clone();
    match std::thread::Builder::new()
        .name("external-launch-classify".into())
        .spawn(move || classify_external_launches(&worker_items))
    {
        Ok(worker) => worker
            .join()
            .unwrap_or_else(|_| classify_external_launches(&items)),
        Err(_) => classify_external_launches(&items),
    }
}

pub fn classify_external_launch_kind(
    item: &ExternalLaunchPath,
    is_directory: Option<bool>,
) -> ClassifiedExternalLaunch {
    let should_reveal = item.select || is_directory == Some(false);
    if should_reveal {
        reveal_or_directory(item.path.clone())
    } else {
        ClassifiedExternalLaunch::Directory(item.path.clone())
    }
}

fn reveal_or_directory(path: PathBuf) -> ClassifiedExternalLaunch {
    match path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(parent) => ClassifiedExternalLaunch::Reveal {
            parent: parent.to_path_buf(),
            target: path,
        },
        None => ClassifiedExternalLaunch::Directory(path),
    }
}

enum SelectPrefix {
    Flag,
    Inline(OsString),
}

fn match_select_prefix(wide: &[u16]) -> Option<SelectPrefix> {
    for prefix in [b"/select".as_slice(), b"-select".as_slice()] {
        if ascii_eq_ignore_case(wide, prefix) {
            return Some(SelectPrefix::Flag);
        }
        let mut with_comma = prefix.to_vec();
        with_comma.push(b',');
        if ascii_starts_with_ignore_case(wide, &with_comma) {
            return Some(SelectPrefix::Inline(OsString::from_wide(
                &wide[with_comma.len()..],
            )));
        }
    }
    None
}

fn ascii_eq_ignore_case(wide: &[u16], ascii: &[u8]) -> bool {
    wide.len() == ascii.len()
        && wide
            .iter()
            .zip(ascii)
            .all(|(unit, expected)| *unit <= 0x7f && (*unit as u8).eq_ignore_ascii_case(expected))
}

fn ascii_starts_with_ignore_case(wide: &[u16], ascii: &[u8]) -> bool {
    wide.len() >= ascii.len() && ascii_eq_ignore_case(&wide[..ascii.len()], ascii)
}

fn strip_outer_quotes(value: OsString) -> OsString {
    let wide = value.encode_wide().collect::<Vec<_>>();
    if wide.len() >= 2 && wide[0] == u16::from(b'"') && *wide.last().unwrap() == u16::from(b'"') {
        OsString::from_wide(&wide[1..wide.len() - 1])
    } else {
        value
    }
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

    #[test]
    fn issue_118_parses_explorer_and_files_select_forms() {
        assert_eq!(
            parse_external_argument(OsStr::new("/select")),
            ParsedExternalArgument::SelectNext
        );
        assert_eq!(
            parse_external_argument(OsStr::new("-Select")),
            ParsedExternalArgument::SelectNext
        );
        assert_eq!(
            parse_external_argument(OsStr::new(r#"/select,"D:\中文\file.txt""#)),
            ParsedExternalArgument::Launch(ExternalLaunchPath::select(PathBuf::from(
                r"D:\中文\file.txt"
            )))
        );
        assert_eq!(
            parse_external_argument(OsStr::new(r"-select,C:\Folder With Spaces\a.csv")),
            ParsedExternalArgument::Launch(ExternalLaunchPath::select(PathBuf::from(
                r"C:\Folder With Spaces\a.csv"
            )))
        );
        assert_eq!(
            parse_external_argument(OsStr::new(r"\\server\share\folder")),
            ParsedExternalArgument::Launch(ExternalLaunchPath::open(PathBuf::from(
                r"\\server\share\folder"
            )))
        );
        assert_eq!(
            parse_external_argument(OsStr::new("//server/share/file.txt")),
            ParsedExternalArgument::Launch(ExternalLaunchPath::open(PathBuf::from(
                r"//server/share/file.txt"
            )))
        );
    }

    #[test]
    fn issue_118_classifies_files_and_select_without_disk_io() {
        let file = ExternalLaunchPath::open(PathBuf::from(r"D:\dir\Asset-Rules-Textures.csv"));
        assert_eq!(
            classify_external_launch_kind(&file, Some(false)),
            ClassifiedExternalLaunch::Reveal {
                parent: PathBuf::from(r"D:\dir"),
                target: PathBuf::from(r"D:\dir\Asset-Rules-Textures.csv"),
            }
        );

        let folder = ExternalLaunchPath::open(PathBuf::from(r"D:\dir"));
        assert_eq!(
            classify_external_launch_kind(&folder, Some(true)),
            ClassifiedExternalLaunch::Directory(PathBuf::from(r"D:\dir"))
        );

        let missing = ExternalLaunchPath::open(PathBuf::from(r"D:\dir\gone.txt"));
        assert_eq!(
            classify_external_launch_kind(&missing, None),
            ClassifiedExternalLaunch::Directory(PathBuf::from(r"D:\dir\gone.txt"))
        );

        let select_folder = ExternalLaunchPath::select(PathBuf::from(r"D:\parent\folder"));
        assert_eq!(
            classify_external_launch_kind(&select_folder, Some(true)),
            ClassifiedExternalLaunch::Reveal {
                parent: PathBuf::from(r"D:\parent"),
                target: PathBuf::from(r"D:\parent\folder"),
            }
        );

        let select_missing = ExternalLaunchPath::select(PathBuf::from(r"D:\dir\gone.txt"));
        assert_eq!(
            classify_external_launch_kind(&select_missing, None),
            ClassifiedExternalLaunch::Reveal {
                parent: PathBuf::from(r"D:\dir"),
                target: PathBuf::from(r"D:\dir\gone.txt"),
            }
        );

        let drive = ExternalLaunchPath::select(PathBuf::from(r"D:\"));
        assert_eq!(
            classify_external_launch_kind(&drive, Some(true)),
            ClassifiedExternalLaunch::Directory(PathBuf::from(r"D:\"))
        );
    }

    #[test]
    fn issue_118_classifies_real_files_off_the_calling_thread() {
        let root = std::env::temp_dir().join(format!(
            "asterfiles-issue-118-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("target.txt");
        std::fs::write(&file, b"issue-118").unwrap();
        let launches = vec![
            ExternalLaunchPath::open(root.clone()),
            ExternalLaunchPath::open(file.clone()),
            ExternalLaunchPath::select(file.clone()),
        ];
        let classified = classify_external_launches_off_thread(launches);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            classified,
            vec![
                ClassifiedExternalLaunch::Directory(root.clone()),
                ClassifiedExternalLaunch::Reveal {
                    parent: root.clone(),
                    target: file.clone(),
                },
                ClassifiedExternalLaunch::Reveal {
                    parent: root,
                    target: file,
                },
            ]
        );
    }
}
