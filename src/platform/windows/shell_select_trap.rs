//! Receive Folder Open and the subsequent Shell selection in the launched process.
//!
//! Match Files' launcher: the original STA owns the shell window for both the first
//! 500 ms and the 10-second late-selection period. IPC delivery runs separately so
//! starting the UI never prevents this thread from servicing COM calls.

use std::{
    cell::{Cell, RefCell},
    ffi::{OsString, c_void},
    mem,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr,
    rc::Rc,
    sync::mpsc,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{
            DISP_E_MEMBERNOTFOUND, DISP_E_UNKNOWNNAME, E_FAIL, E_NOINTERFACE, E_NOTIMPL, E_POINTER,
            HINSTANCE, HWND, LPARAM, LRESULT, RPC_E_CHANGED_MODE, SHANDLE_PTR, VARIANT_BOOL,
            VARIANT_FALSE, VARIANT_TRUE, WPARAM,
        },
        System::{
            Com::{
                CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, DISPPARAMS, EXCEPINFO, IDispatch,
                IDispatch_Impl, IServiceProvider, IServiceProvider_Impl, ITypeInfo,
            },
            Ole::{IOleWindow_Impl, OleInitialize, OleUninitialize},
            Threading::GetCurrentThreadId,
            Variant::{InitVariantFromBuffer, VARIANT, VariantClear},
        },
        UI::{
            Controls::LPFNSVADDPROPSHEETPAGE,
            Shell::{
                _SVGIO, Common::ITEMIDLIST, FOLDERSETTINGS, ILCombine, ILGetSize, IShellBrowser,
                IShellItem, IShellView, IShellView_Impl, IShellWindows, IWebBrowser_Impl,
                IWebBrowserApp, IWebBrowserApp_Impl, SHCreateItemFromIDList, SHGetDesktopFolder,
                SIGDN_DESKTOPABSOLUTEPARSING, SVSI_EDIT, SVSI_ENSUREVISIBLE, SVSI_FOCUSED,
                SVSI_SELECT, SWC_BROWSER, ShellWindows,
            },
            WindowsAndMessaging::{
                CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
                DispatchMessageW, GetMessageW, IsWindow, KillTimer, MSG, PostMessageW,
                PostQuitMessage, RegisterClassExW, SetTimer, TranslateMessage, UnregisterClassW,
                WINDOW_STYLE, WM_CLOSE, WM_DESTROY, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE,
                WS_EX_TOOLWINDOW,
            },
        },
    },
    core::{BOOL, BSTR, GUID, HRESULT, IUnknownImpl, Interface, PCWSTR, PWSTR, Ref, implement},
};
use windows_sys::Win32::{
    Storage::FileSystem::GetLongPathNameW, System::LibraryLoader::GetModuleHandleW,
};

use crate::platform::windows::address_path::ExternalLaunchPath;

#[path = "shell_select_probe.rs"]
pub mod probe;

const INITIAL_SELECTION_WAIT: Duration = Duration::from_millis(500);
const LATE_SELECTION_WAIT: Duration = Duration::from_secs(10);
const TIMER_ID: usize = 1;
const CLASS_NAME: &str = "AsterFiles.ShellSelectTrap.v1";

/// This runs before the UI and single-instance forwarding, on the launched main thread.
pub fn run_launcher(paths: &[ExternalLaunchPath]) -> std::io::Result<bool> {
    if paths.len() != 1 || paths[0].select || !paths[0].path.is_dir() {
        return Ok(false);
    }
    run_launch_request(&paths[0].path, |request| {
        super::single_instance::deliver_shell_open(request.paths, request.selection)
    })?;
    Ok(true)
}

fn run_launch_request(
    folder: &Path,
    deliver: impl FnOnce(super::single_instance::ExternalOpenRequest) -> std::io::Result<()>
    + Send
    + 'static,
) -> std::io::Result<()> {
    let started = Instant::now();
    let trap = ShellSelectTrap::new(folder);
    let (paths, selection, late_sender) = match trap.as_ref() {
        Some(trap) => {
            trap.pump(INITIAL_SELECTION_WAIT);
            match trap.selected() {
                Some(target) => (vec![ExternalLaunchPath::select(target)], None, None),
                None => {
                    let (sender, receiver) = mpsc::channel();
                    (
                        vec![ExternalLaunchPath::open(folder.to_path_buf())],
                        Some(receiver),
                        Some(sender),
                    )
                }
            }
        }
        None => (
            vec![ExternalLaunchPath::open(folder.to_path_buf())],
            None,
            None,
        ),
    };
    let request = super::single_instance::ExternalOpenRequest { paths, selection };
    // The Shell caller can still be waiting for an RPC on this STA while IPC starts the UI.
    let receiver_hwnd = trap.as_ref().map(|trap| trap.inner.hwnd.get().0 as isize);
    let delivery = std::thread::Builder::new()
        .name("shell-open-delivery".into())
        .spawn(move || {
            let result = deliver(request);
            if result.is_err()
                && let Some(hwnd) = receiver_hwnd
            {
                unsafe {
                    let _ = PostMessageW(Some(HWND(hwnd as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
            result
        })?;
    if let Some(sender) = late_sender
        && let Some(trap) = trap.as_ref()
    {
        trap.pump(LATE_SELECTION_WAIT);
        let _ = sender.send(trap.selected());
    }
    crate::operation_audit::record(
        "shell-select-trap",
        format!(
            "role=launcher result={} elapsed_ms={}",
            if trap.as_ref().and_then(ShellSelectTrap::selected).is_some() {
                "selected"
            } else {
                "timeout-or-unregistered"
            },
            started.elapsed().as_millis()
        ),
    );
    drop(trap);
    delivery
        .join()
        .map_err(|_| std::io::Error::other("shell delivery worker panicked"))?
}

struct ShellSelectTrap {
    inner: Rc<Inner>,
    _dispatch: IDispatch,
    _shell: ShellWindowGuard,
    _window: WindowGuard,
    _ole: OleGuard,
}

impl ShellSelectTrap {
    fn new(folder: &Path) -> Option<Self> {
        let ole = OleGuard::new()?;
        let inner = Rc::new(Inner {
            hwnd: Cell::new(HWND(ptr::null_mut())),
            owner_thread: unsafe { GetCurrentThreadId() },
            started: Instant::now(),
            folder: folder.to_path_buf(),
            folder_pidl: Cell::new(ptr::null_mut()),
            selected: RefCell::new(None),
            pending_cookie: Cell::new(0),
            register_cookie: Cell::new(0),
            pending_registered: Cell::new(false),
            window_registered: Cell::new(false),
            shell_windows: RefCell::new(None),
        });
        let class = register_class()?;
        let hwnd = create_trap_window(&class)?;
        inner.hwnd.set(hwnd);
        let window = WindowGuard { hwnd, class };
        let shell = ShellWindowGuard {
            inner: inner.clone(),
        };
        let app: IWebBrowserApp = ShellSelectHost {
            inner: inner.clone(),
        }
        .into();
        let dispatch: IDispatch = app.cast().ok()?;
        if let Err(detail) = register_pending_shell_window(&inner, folder)
            .and_then(|()| complete_shell_window_register(&inner, &dispatch))
        {
            crate::operation_audit::record(
                "shell-select-trap",
                format!("register-failed={detail}"),
            );
            return None;
        }
        crate::operation_audit::record(
            "shell-select-registered",
            format!(
                "owner_thread={} hwnd={} folder={folder:?}",
                inner.owner_thread, hwnd.0 as isize
            ),
        );
        Some(Self {
            inner,
            _dispatch: dispatch,
            _shell: shell,
            _window: window,
            _ole: ole,
        })
    }

    fn pump(&self, timeout: Duration) {
        if self.selected().is_none() && unsafe { IsWindow(Some(self.inner.hwnd.get())) }.as_bool() {
            pump_until_select_or_timeout(self.inner.hwnd.get(), timeout);
        }
    }

    fn selected(&self) -> Option<PathBuf> {
        self.inner.selected.borrow().clone()
    }
}

#[cfg(test)]
fn capture_shell_select(
    folder: &Path,
    timeout: Duration,
    ready: mpsc::Sender<()>,
) -> Option<PathBuf> {
    let trap = ShellSelectTrap::new(folder)?;
    let _ = ready.send(());
    trap.pump(timeout);
    trap.selected()
}

fn register_pending_shell_window(inner: &Rc<Inner>, folder: &Path) -> Result<(), String> {
    let shell_windows: IShellWindows = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) }
        .map_err(|error| error.to_string())?;
    let pidl = parse_pidl(folder).ok_or_else(|| "folder-pidl".to_owned())?;
    inner.folder_pidl.set(pidl);

    let mut location = pidl_variant(pidl).map_err(|error| error.to_string())?;
    let mut unused = VARIANT::default();
    let thread_id = unsafe { GetCurrentThreadId() } as i32;
    let pending =
        unsafe { shell_windows.RegisterPending(thread_id, &location, &unused, SWC_BROWSER) }
            .map_err(|error| error.to_string())?;
    inner.pending_cookie.set(pending);
    inner.pending_registered.set(true);
    *inner.shell_windows.borrow_mut() = Some(shell_windows);
    unsafe {
        let _ = VariantClear(&mut location);
        let _ = VariantClear(&mut unused);
    }
    Ok(())
}

fn complete_shell_window_register(inner: &Rc<Inner>, dispatch: &IDispatch) -> Result<(), String> {
    let shell_windows = inner.shell_windows.borrow();
    let shell_windows = shell_windows
        .as_ref()
        .ok_or_else(|| "shell-windows".to_owned())?;
    let pidl = inner.folder_pidl.get();
    if pidl.is_null() {
        return Err("folder-pidl".to_owned());
    }
    let mut location = pidl_variant(pidl).map_err(|error| error.to_string())?;
    let hwnd = inner.hwnd.get().0 as i32;
    let cookie = unsafe { shell_windows.Register(dispatch, hwnd, SWC_BROWSER) }
        .map_err(|error| error.to_string())?;
    inner.register_cookie.set(cookie);
    inner.window_registered.set(true);
    unsafe { shell_windows.OnNavigate(cookie, &location) }.map_err(|error| error.to_string())?;
    unsafe {
        let _ = VariantClear(&mut location);
    }
    Ok(())
}

fn parse_pidl(path: &Path) -> Option<*mut ITEMIDLIST> {
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let desktop = unsafe { SHGetDesktopFolder() }.ok()?;
    let mut pidl = ptr::null_mut();
    let mut attributes = 0;
    // Files uses the desktop IShellFolder parser. It preserves filesystem PIDLs
    // for known folders, which lets IShellWindows match the caller's folder.
    unsafe {
        desktop
            .ParseDisplayName(
                HWND::default(),
                None,
                PCWSTR(wide.as_ptr()),
                None,
                &mut pidl,
                &mut attributes,
            )
            .ok()?;
    }
    if pidl.is_null() { None } else { Some(pidl) }
}

fn pidl_variant(pidl: *mut ITEMIDLIST) -> windows::core::Result<VARIANT> {
    let size = unsafe { ILGetSize(Some(pidl)) };
    unsafe { InitVariantFromBuffer(pidl.cast(), size) }
}

fn pidl_to_path(pidl: *const ITEMIDLIST) -> Option<PathBuf> {
    if pidl.is_null() {
        return None;
    }
    let item: IShellItem = unsafe { SHCreateItemFromIDList(pidl) }.ok()?;
    let value = unsafe { item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) }.ok()?;
    Some(take_shell_path(value))
}

fn take_shell_path(value: PWSTR) -> PathBuf {
    if value.is_null() {
        return PathBuf::new();
    }
    let mut length = 0;
    unsafe {
        while *value.0.add(length) != 0 {
            length += 1;
        }
    }
    let path = PathBuf::from(OsString::from_wide(unsafe {
        std::slice::from_raw_parts(value.0, length)
    }));
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    path
}

fn long_path(path: &Path) -> PathBuf {
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let needed = unsafe { GetLongPathNameW(wide.as_ptr(), ptr::null_mut(), 0) };
    if needed == 0 {
        return path.to_path_buf();
    }
    let mut buffer = vec![0; needed as usize];
    let written = unsafe { GetLongPathNameW(wide.as_ptr(), buffer.as_mut_ptr(), needed) };
    if written == 0 || written >= needed {
        return path.to_path_buf();
    }
    buffer.truncate(written as usize);
    PathBuf::from(OsString::from_wide(&buffer))
}

fn relative_from_prefix(path: &Path, prefix: &Path) -> Option<PathBuf> {
    if let Ok(relative) = path.strip_prefix(prefix) {
        return Some(relative.to_path_buf());
    }
    let path_wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let prefix_wide = prefix.as_os_str().encode_wide().collect::<Vec<_>>();
    if prefix_wide.is_empty() || path_wide.len() < prefix_wide.len() {
        return None;
    }
    let matched = path_wide
        .iter()
        .zip(&prefix_wide)
        .all(|(unit, expected)| path_units_equal(*unit, *expected));
    if !matched {
        return None;
    }
    let rest = match path_wide.get(prefix_wide.len()) {
        None => return Some(PathBuf::new()),
        Some(&unit) if unit == u16::from(b'\\') || unit == u16::from(b'/') => {
            &path_wide[prefix_wide.len() + 1..]
        }
        Some(_) => return None,
    };
    Some(PathBuf::from(OsString::from_wide(rest)))
}

fn path_units_equal(left: u16, right: u16) -> bool {
    (left <= 0x7f && right <= 0x7f && (left as u8).eq_ignore_ascii_case(&(right as u8)))
        || left == right
}

fn rebase_selected_onto_folder(folder: &Path, selected: &Path) -> PathBuf {
    let long_folder = long_path(folder);
    let long_selected = long_path(selected);
    match relative_from_prefix(&long_selected, &long_folder) {
        Some(relative) if !relative.as_os_str().is_empty() => folder.join(relative),
        _ => long_selected,
    }
}

fn pump_until_select_or_timeout(hwnd: HWND, timeout: Duration) {
    let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
    unsafe {
        if SetTimer(Some(hwnd), TIMER_ID, millis, None) == 0 {
            crate::operation_audit::record("shell-select-trap", "timer-failed");
            return;
        }
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = KillTimer(Some(hwnd), TIMER_ID);
    }
}

fn register_class() -> Option<Vec<u16>> {
    let class = wide(CLASS_NAME);
    let wnd_class = WNDCLASSEXW {
        cbSize: mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(trap_wnd_proc),
        hInstance: HINSTANCE(unsafe { GetModuleHandleW(ptr::null()) }),
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    let atom = unsafe { RegisterClassExW(&wnd_class) };
    if atom == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(1410) {
            return None;
        }
    }
    Some(class)
}

fn create_trap_window(class: &[u16]) -> Option<HWND> {
    let title = wide("AsterFiles Shell Select");
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )
    }
    .ok()?;
    if hwnd.0.is_null() {
        None
    } else {
        unsafe {
            let cloak: i32 = 1;
            let _ = windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute(
                hwnd.0,
                windows_sys::Win32::Graphics::Dwm::DWMWA_CLOAK as u32,
                (&cloak as *const i32).cast(),
                mem::size_of::<i32>() as u32,
            );
        }
        Some(hwnd)
    }
}

extern "system" fn trap_wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_ID => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

struct Inner {
    hwnd: Cell<HWND>,
    owner_thread: u32,
    started: Instant,
    folder: PathBuf,
    folder_pidl: Cell<*mut ITEMIDLIST>,
    selected: RefCell<Option<PathBuf>>,
    pending_cookie: Cell<i32>,
    register_cookie: Cell<i32>,
    pending_registered: Cell<bool>,
    window_registered: Cell<bool>,
    shell_windows: RefCell<Option<IShellWindows>>,
}

impl Inner {
    fn take_selected_item(&self, pidl_item: *const ITEMIDLIST) -> Option<PathBuf> {
        if pidl_item.is_null() {
            return None;
        }
        let folder = self.folder_pidl.get();
        if folder.is_null() {
            return None;
        }
        let combined = unsafe { ILCombine(Some(folder), Some(pidl_item)) };
        if combined.is_null() {
            return None;
        }
        let path = pidl_to_path(combined.cast_const());
        unsafe { CoTaskMemFree(Some(combined.cast())) };
        // Shell parsing names expand 8.3 components; keep the Folder Open identity.
        path.filter(|path| !path.as_os_str().is_empty())
            .map(|path| rebase_selected_onto_folder(&self.folder, &path))
    }
}

struct OleGuard(bool);

impl OleGuard {
    fn new() -> Option<Self> {
        match unsafe { OleInitialize(None) } {
            Ok(()) => Some(Self(true)),
            Err(error) if error.code() == RPC_E_CHANGED_MODE => None,
            Err(_) => None,
        }
    }
}

impl Drop for OleGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { OleUninitialize() };
        }
    }
}

struct WindowGuard {
    hwnd: HWND,
    class: Vec<u16>,
}

impl Drop for WindowGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = UnregisterClassW(PCWSTR(self.class.as_ptr()), None);
        }
    }
}

struct ShellWindowGuard {
    inner: Rc<Inner>,
}

impl Drop for ShellWindowGuard {
    fn drop(&mut self) {
        let shell_windows = self.inner.shell_windows.borrow();
        if let Some(shell_windows) = shell_windows.as_ref() {
            if self.inner.window_registered.replace(false) {
                let _ = unsafe { shell_windows.Revoke(self.inner.register_cookie.get()) };
            }
            if self.inner.pending_registered.replace(false)
                && self.inner.pending_cookie.get() != self.inner.register_cookie.get()
            {
                let _ = unsafe { shell_windows.Revoke(self.inner.pending_cookie.get()) };
            }
        }
        let pidl = self.inner.folder_pidl.replace(ptr::null_mut());
        if !pidl.is_null() {
            unsafe { CoTaskMemFree(Some(pidl.cast())) };
        }
    }
}

// Files keeps this object in its STA: Rc/RefCell and the hidden HWND share one owner.
#[implement(IWebBrowserApp, IServiceProvider, IShellView, Agile = false)]
struct ShellSelectHost {
    inner: Rc<Inner>,
}

#[allow(non_snake_case)]
impl IDispatch_Impl for ShellSelectHost_Impl {
    fn GetTypeInfoCount(&self) -> windows::core::Result<u32> {
        Ok(0)
    }

    fn GetTypeInfo(&self, _itinfo: u32, _lcid: u32) -> windows::core::Result<ITypeInfo> {
        Err(E_NOTIMPL.into())
    }

    fn GetIDsOfNames(
        &self,
        _riid: *const GUID,
        _rgsznames: *const PCWSTR,
        _cnames: u32,
        _lcid: u32,
        _rgdispid: *mut i32,
    ) -> windows::core::Result<()> {
        Err(DISP_E_UNKNOWNNAME.into())
    }

    fn Invoke(
        &self,
        _dispidmember: i32,
        _riid: *const GUID,
        _lcid: u32,
        _wflags: windows::Win32::System::Com::DISPATCH_FLAGS,
        _pdispparams: *const DISPPARAMS,
        _pvarresult: *mut VARIANT,
        _pexcepinfo: *mut EXCEPINFO,
        _puargerr: *mut u32,
    ) -> windows::core::Result<()> {
        Err(DISP_E_MEMBERNOTFOUND.into())
    }
}

#[allow(non_snake_case)]
impl IWebBrowser_Impl for ShellSelectHost_Impl {
    fn GoBack(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GoForward(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GoHome(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GoSearch(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Navigate(
        &self,
        _url: &BSTR,
        _flags: *const VARIANT,
        _targetframename: *const VARIANT,
        _postdata: *const VARIANT,
        _headers: *const VARIANT,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Refresh(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn Refresh2(&self, _level: *const VARIANT) -> windows::core::Result<()> {
        Ok(())
    }
    fn Stop(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn Application(&self) -> windows::core::Result<IDispatch> {
        self.to_interface::<IWebBrowserApp>().cast()
    }
    fn Parent(&self) -> windows::core::Result<IDispatch> {
        self.Application()
    }
    fn Container(&self) -> windows::core::Result<IDispatch> {
        self.Application()
    }
    fn Document(&self) -> windows::core::Result<IDispatch> {
        self.Application()
    }
    fn TopLevelContainer(&self) -> windows::core::Result<VARIANT_BOOL> {
        Ok(VARIANT_TRUE)
    }
    fn Type(&self) -> windows::core::Result<BSTR> {
        Ok(BSTR::from("AsterFiles"))
    }
    fn Left(&self) -> windows::core::Result<i32> {
        Ok(0)
    }
    fn SetLeft(&self, _left: i32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Top(&self) -> windows::core::Result<i32> {
        Ok(0)
    }
    fn SetTop(&self, _top: i32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Width(&self) -> windows::core::Result<i32> {
        Ok(0)
    }
    fn SetWidth(&self, _width: i32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Height(&self) -> windows::core::Result<i32> {
        Ok(0)
    }
    fn SetHeight(&self, _height: i32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn LocationName(&self) -> windows::core::Result<BSTR> {
        Ok(BSTR::from(""))
    }
    fn LocationURL(&self) -> windows::core::Result<BSTR> {
        Ok(BSTR::from(""))
    }
    fn Busy(&self) -> windows::core::Result<VARIANT_BOOL> {
        Ok(VARIANT_FALSE)
    }
}

#[allow(non_snake_case)]
impl IWebBrowserApp_Impl for ShellSelectHost_Impl {
    fn Quit(&self) -> windows::core::Result<()> {
        unsafe { PostMessageW(Some(self.inner.hwnd.get()), WM_CLOSE, WPARAM(0), LPARAM(0)) }
    }
    fn ClientToWindow(&self, pcx: *mut i32, pcy: *mut i32) -> windows::core::Result<()> {
        if pcx.is_null() || pcy.is_null() {
            Err(E_POINTER.into())
        } else {
            Ok(())
        }
    }
    fn PutProperty(&self, _property: &BSTR, _vtvalue: &VARIANT) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetProperty(&self, _property: &BSTR) -> windows::core::Result<VARIANT> {
        Err(E_NOTIMPL.into())
    }
    fn Name(&self) -> windows::core::Result<BSTR> {
        Ok(BSTR::from("AsterFiles"))
    }
    fn HWND(&self) -> windows::core::Result<SHANDLE_PTR> {
        Ok(SHANDLE_PTR(self.inner.hwnd.get().0 as isize))
    }
    fn FullName(&self) -> windows::core::Result<BSTR> {
        let path = std::env::current_exe().map_err(|_| windows::core::Error::from(E_FAIL))?;
        let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        Ok(BSTR::from_wide(&wide))
    }
    fn Path(&self) -> windows::core::Result<BSTR> {
        self.FullName()
    }
    fn Visible(&self) -> windows::core::Result<VARIANT_BOOL> {
        Ok(VARIANT_FALSE)
    }
    fn SetVisible(&self, _value: VARIANT_BOOL) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn StatusBar(&self) -> windows::core::Result<VARIANT_BOOL> {
        Ok(VARIANT_FALSE)
    }
    fn SetStatusBar(&self, _value: VARIANT_BOOL) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn StatusText(&self) -> windows::core::Result<BSTR> {
        Ok(BSTR::from(""))
    }
    fn SetStatusText(&self, _statustext: &BSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn ToolBar(&self) -> windows::core::Result<i32> {
        Ok(0)
    }
    fn SetToolBar(&self, _value: i32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn MenuBar(&self) -> windows::core::Result<VARIANT_BOOL> {
        Ok(VARIANT_FALSE)
    }
    fn SetMenuBar(&self, _value: VARIANT_BOOL) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn FullScreen(&self) -> windows::core::Result<VARIANT_BOOL> {
        Ok(VARIANT_FALSE)
    }
    fn SetFullScreen(&self, _bfullscreen: VARIANT_BOOL) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}

#[allow(non_snake_case)]
impl IServiceProvider_Impl for ShellSelectHost_Impl {
    fn QueryService(
        &self,
        _guid_service: *const GUID,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        unsafe {
            if ppvobject.is_null() {
                return Err(E_POINTER.into());
            }
            *ppvobject = ptr::null_mut();
            IUnknownImpl::QueryInterface(self, riid, ppvobject).ok()
        }
    }
}

#[allow(non_snake_case)]
impl IOleWindow_Impl for ShellSelectHost_Impl {
    fn GetWindow(&self) -> windows::core::Result<HWND> {
        let hwnd = self.inner.hwnd.get();
        if hwnd.0.is_null() {
            Err(E_FAIL.into())
        } else {
            Ok(hwnd)
        }
    }

    fn ContextSensitiveHelp(&self, _fentermode: BOOL) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}

#[allow(non_snake_case)]
impl IShellView_Impl for ShellSelectHost_Impl {
    fn TranslateAccelerator(&self, _pmsg: *const MSG) -> HRESULT {
        E_NOTIMPL
    }

    fn EnableModeless(&self, _fenable: BOOL) -> windows::core::Result<()> {
        Ok(())
    }

    fn UIActivate(&self, _ustate: u32) -> windows::core::Result<()> {
        Ok(())
    }

    fn Refresh(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn CreateViewWindow(
        &self,
        _psvprevious: Ref<IShellView>,
        _pfs: *const FOLDERSETTINGS,
        _psb: Ref<IShellBrowser>,
        _prcview: *const windows::Win32::Foundation::RECT,
    ) -> windows::core::Result<HWND> {
        Err(E_NOTIMPL.into())
    }

    fn DestroyViewWindow(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn GetCurrentInfo(&self) -> windows::core::Result<FOLDERSETTINGS> {
        Ok(FOLDERSETTINGS::default())
    }

    fn AddPropertySheetPages(
        &self,
        _dwreserved: u32,
        _pfn: LPFNSVADDPROPSHEETPAGE,
        _lparam: LPARAM,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn SaveViewState(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn SelectItem(&self, pidlitem: *const ITEMIDLIST, uflags: u32) -> windows::core::Result<()> {
        if uflags & (SVSI_SELECT.0 | SVSI_FOCUSED.0 | SVSI_ENSUREVISIBLE.0 | SVSI_EDIT.0) as u32
            == 0
        {
            return Ok(());
        }
        if let Some(path) = self.inner.take_selected_item(pidlitem) {
            crate::operation_audit::record(
                "shell-select-received",
                format!(
                    "owner_thread={} callback_thread={} elapsed_ms={} target={path:?}",
                    self.inner.owner_thread,
                    unsafe { GetCurrentThreadId() },
                    self.inner.started.elapsed().as_millis()
                ),
            );
            debug_assert_eq!(self.inner.owner_thread, unsafe { GetCurrentThreadId() });
            *self.inner.selected.borrow_mut() = Some(path);
            // Match Files: wake the HWND owner, never a COM caller's message queue.
            unsafe { PostMessageW(Some(self.inner.hwnd.get()), WM_CLOSE, WPARAM(0), LPARAM(0)) }?;
        }
        Ok(())
    }

    fn GetItemObject(
        &self,
        _uitem: &_SVGIO,
        _riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if !ppv.is_null() {
            unsafe { *ppv = ptr::null_mut() };
        }
        Err(E_NOINTERFACE.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::{
        FOLDERID_Downloads, ILCreateFromPathW, ILFindLastID, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
        SHOpenFolderAndSelectItems,
    };

    fn unique_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "asterfiles-issue-118-trap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn caller_pidl(path: &Path) -> *mut ITEMIDLIST {
        let wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let pidl = unsafe { ILCreateFromPathW(PCWSTR(wide.as_ptr())) };
        assert!(!pidl.is_null(), "caller could not create a filesystem PIDL");
        pidl
    }

    fn downloads_dir() -> PathBuf {
        let path = unsafe {
            SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None)
                .expect("Downloads known folder is available")
        };
        take_shell_path(path)
    }

    #[test]
    fn issue_118_select_display_name_keeps_the_opened_folder_identity() {
        let root = unique_dir();
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("选中文件.txt");
        std::fs::write(&file, b"issue-118").unwrap();
        assert_eq!(rebase_selected_onto_folder(&root, &long_path(&file)), file);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn issue_118_shell_windows_select_item_from_shopenfolderandselectitems() {
        assert_shell_select_returns_promptly(false);
    }

    #[test]
    fn issue_122_shell_select_with_folder_and_child_returns_promptly() {
        assert_shell_select_returns_promptly(true);
    }

    #[test]
    fn issue_122_known_folder_shell_select_with_independent_caller_returns_promptly() {
        let root = downloads_dir().join(format!(
            "asterfiles-issue-122-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert_shell_select_returns_promptly_at(root, true);
    }

    fn assert_shell_select_returns_promptly(with_child_array: bool) {
        let root = unique_dir();
        assert_shell_select_returns_promptly_at(root, with_child_array);
    }

    fn assert_shell_select_returns_promptly_at(root: PathBuf, with_child_array: bool) {
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("选中文件.txt");
        std::fs::write(&file, b"issue-118").unwrap();
        let trap_root = root.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("shell-select-trap-test".into())
            .spawn(move || capture_shell_select(&trap_root, Duration::from_secs(4), ready_tx))
            .expect("trap thread starts");
        ready_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("shell window registered before SHOpenFolderAndSelectItems");

        let started = Instant::now();
        let opener_file = file.clone();
        let opener = std::thread::Builder::new()
            .name("shell-select-trap-open".into())
            .spawn(move || {
                let _ole = OleGuard::new().expect("opener can initialize OLE");
                let pidl = caller_pidl(&opener_file);
                let opened = if with_child_array {
                    let parent = caller_pidl(opener_file.parent().unwrap());
                    let child = unsafe { ILFindLastID(pidl) };
                    let result = unsafe { SHOpenFolderAndSelectItems(parent, Some(&[child]), 0) };
                    unsafe { CoTaskMemFree(Some(parent.cast())) };
                    result
                } else {
                    unsafe { SHOpenFolderAndSelectItems(pidl, None, 0) }
                };
                unsafe { CoTaskMemFree(Some(pidl.cast())) };
                opened
            })
            .expect("opener thread starts");

        let selected = worker.join().expect("trap thread joins");
        let opened = opener.join().expect("opener thread joins");
        let _ = std::fs::remove_dir_all(&root);
        opened.expect("SHOpenFolderAndSelectItems reaches the registered trap");
        assert_eq!(selected.as_deref(), Some(file.as_path()));
        let elapsed = started.elapsed();
        eprintln!(
            "issue_122_shell_select child_array={with_child_array} elapsed_ms={}",
            elapsed.as_millis()
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "selection waited for the four-second expiry: {elapsed:?}"
        );
    }
}
