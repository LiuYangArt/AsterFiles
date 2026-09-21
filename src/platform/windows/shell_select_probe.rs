//! Isolated process probe for the launcher's STA and two-phase delivery; never creates application UI.
use super::{
    CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, ExternalLaunchPath, GetCurrentThreadId,
    IShellView, IShellWindows, ITEMIDLIST, IWebBrowserApp, Interface, OleGuard, ShellWindows,
    VARIANT, pidl_variant, run_launch_request,
};
use std::os::windows::{ffi::OsStrExt, process::CommandExt};
use std::{
    io,
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::HWND,
    System::Threading::CREATE_NO_WINDOW,
    UI::{
        Shell::{
            FOLDERID_Downloads, ILCreateFromPathW, ILFindLastID, KF_FLAG_DEFAULT,
            SHGetKnownFolderPath, SVSI_ENSUREVISIBLE, SVSI_FOCUSED, SVSI_SELECT, SWC_BROWSER,
            SWFO_NEEDDISPATCH,
        },
        WindowsAndMessaging::GetWindowThreadProcessId,
    },
};
use windows::core::PCWSTR;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn caller_pidl(path: &Path) -> Result<*mut ITEMIDLIST> {
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let pidl = unsafe { ILCreateFromPathW(PCWSTR(wide.as_ptr())) };
    if pidl.is_null() {
        Err(format!("caller could not create a filesystem PIDL for {path:?}").into())
    } else {
        Ok(pidl)
    }
}

pub fn try_run() -> Result<bool> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args
        .first()
        .is_some_and(|a| a == "--agent-shell-launch-probe")
    {
        if args.len() != 2 {
            return Err("shell launch probe requires an output path".into());
        }
        export(Path::new(&args[1]))?;
        return Ok(true);
    }
    if args
        .first()
        .is_some_and(|a| a == "--agent-shell-launch-receiver")
    {
        if args.len() != 4 {
            return Err("shell launch receiver requires folder, output and mode".into());
        }
        receive(
            Path::new(&args[1]),
            Path::new(&args[2]),
            &args[3].to_string_lossy(),
        )?;
        return Ok(true);
    }
    Ok(false)
}

fn receive(folder: &Path, output: &Path, mode: &str) -> Result<()> {
    let started = Instant::now();
    let owner_thread = unsafe { GetCurrentThreadId() };
    let expected_folder = folder.to_path_buf();
    let expected_file = folder.join("中文 selected file.txt");
    let output = output.to_path_buf();
    let mode = mode.to_owned();
    run_launch_request(folder, move |request| {
        let opened_ms = started.elapsed().as_millis();
        let expected = if mode == "early" {
            ExternalLaunchPath::select(expected_file.clone())
        } else {
            ExternalLaunchPath::open(expected_folder)
        };
        if request.paths != [expected] {
            return Err(io::Error::other("unexpected initial launch payload"));
        }
        std::fs::write(output.with_extension("open"), opened_ms.to_string())?;
        let selected = match request.selection {
            Some(receiver) => receiver
                .recv_timeout(Duration::from_secs(12))
                .map_err(io::Error::other)?,
            None => Some(request.paths[0].path.clone()),
        };
        if selected != (mode != "directory").then_some(expected_file) {
            return Err(io::Error::other(
                "incorrect selection or request association",
            ));
        }
        std::fs::write(
            output,
            format!(
                "{{\"mode\":{mode:?},\"owner_thread\":{owner_thread},\"open_ms\":{opened_ms},\"complete_ms\":{},\"selected\":{},\"passed\":true}}",
                started.elapsed().as_millis(),
                selected.is_some()
            ),
        )
    })?;
    Ok(())
}

fn export(output: &Path) -> Result<()> {
    let _ole = OleGuard::new().ok_or("probe could not initialize OLE")?;
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let fixtures = Fixture(std::env::temp_dir().join(format!(
        "asterfiles-shell-launch-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()
    )));
    std::fs::create_dir(&fixtures.0)?;
    let downloads_fixture = KnownFolderFixture::new()?;
    let shell: IShellWindows = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }?;
    let mut results = Vec::new();
    let cases = [
        ("early", "early", fixtures.0.join("early")),
        ("late", "late", fixtures.0.join("late")),
        ("directory", "directory", fixtures.0.join("directory")),
        ("downloads", "late", downloads_fixture.0.clone()),
    ];
    for (case_name, mode, folder) in cases {
        if !folder.exists() {
            std::fs::create_dir(&folder)?;
        }
        let file = folder.join("中文 selected file.txt");
        std::fs::write(&file, b"#122")?;
        let report = fixtures.0.join(format!("{case_name}.json"));
        let mut child = ProbeChild(
            Command::new(std::env::current_exe()?)
                .arg("--agent-shell-launch-receiver")
                .arg(&folder)
                .arg(&report)
                .arg(mode)
                .creation_flags(CREATE_NO_WINDOW.0)
                .spawn()?,
        );
        let started = Instant::now();
        let pidl = caller_pidl(&folder)?;
        let location = pidl_variant(pidl)?;
        unsafe { CoTaskMemFree(Some(pidl.cast())) };
        let (dispatch, window_thread) = loop {
            let mut hwnd = 0;
            if let Ok(dispatch) = unsafe {
                shell.FindWindowSW(
                    &location,
                    &VARIANT::default(),
                    SWC_BROWSER,
                    &mut hwnd,
                    SWFO_NEEDDISPATCH,
                )
            } {
                let mut pid = 0;
                let tid =
                    unsafe { GetWindowThreadProcessId(HWND(hwnd as isize as _), Some(&mut pid)) };
                if pid == child.0.id() {
                    break (dispatch, tid);
                }
            }
            if started.elapsed() > Duration::from_secs(5) {
                return Err("launched process never registered its own Shell window".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let app: IWebBrowserApp = dispatch.cast()?;
        let executable = unsafe { app.FullName() }?;
        let expected = std::env::current_exe()?
            .as_os_str()
            .encode_wide()
            .collect::<Vec<_>>();
        if &*executable != expected.as_slice() {
            return Err("Shell executable identity is not the real path".into());
        }
        if mode == "late" {
            std::thread::sleep(Duration::from_millis(3200));
            let open_ms: u128 = std::fs::read_to_string(report.with_extension("open"))?.parse()?;
            if open_ms > 1500 {
                return Err("directory delivery waited for the late selection".into());
            }
        }
        if mode != "directory" {
            // Target the discovered child explicitly so a failed probe cannot launch the user's default manager.
            let view: IShellView = dispatch.cast()?;
            let pidl = caller_pidl(&file)?;
            let result = unsafe {
                view.SelectItem(
                    ILFindLastID(pidl),
                    (SVSI_SELECT.0 | SVSI_FOCUSED.0 | SVSI_ENSUREVISIBLE.0) as u32,
                )
            };
            unsafe { CoTaskMemFree(Some(pidl.cast())) };
            result?;
        }
        loop {
            if let Some(status) = child.0.try_wait()? {
                if !status.success() {
                    return Err(format!("{mode} launcher failed: {status}").into());
                }
                break;
            }
            if started.elapsed() > Duration::from_secs(14) {
                return Err("launcher did not terminate after completion/expiry".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let result = std::fs::read_to_string(&report)?;
        if !result.contains(&format!("\"owner_thread\":{window_thread},")) {
            return Err("Shell window moved off the launched process's main thread".into());
        }
        results.push(format!("{{\"case\":{case_name:?},\"result\":{result}}}"));
    }
    std::fs::write(
        output,
        format!(
            "{{\"scenario\":\"shell-launcher\",\"scope\":\"cross_process_shell_receiver_no_ui\",\"cases\":[{}],\"passed\":true}}\n",
            results.join(",")
        ),
    )?;
    Ok(())
}

struct ProbeChild(Child);
impl Drop for ProbeChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct KnownFolderFixture(PathBuf);
impl KnownFolderFixture {
    fn new() -> Result<Self> {
        let known_folder =
            unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None) }?;
        let root = super::take_shell_path(known_folder);
        let path = root.join(format!(
            "asterfiles-shell-launch-downloads-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}
impl Drop for KnownFolderFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
