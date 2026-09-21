//! Catch `SHOpenFolderAndSelectItems` after Folder Open only received the parent directory.
//!
//! Apps that follow Explorer's pattern ShellExecute the Folder verb, then `SelectItem` on
//! `IShellWindows`. This module registers a short-lived shell window so that request can be
//! rewritten as an #118 reveal. The trap never blocks Folder Open or window startup: the folder
//! opens immediately, and a matching `SelectItem` is applied afterwards. Desktop Explorer folder
//! opens do not send `SelectItem` and skip the trap. The main window is never published.
//! Downloaders that never send `SelectItem` (for example FDM) are out of scope here.

use std::{
    cell::{Cell, RefCell},
    ffi::{OsString, c_void},
    mem,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr,
    rc::Rc,
    sync::mpsc,
    time::Duration,
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
                IDispatch_Impl, IServiceProvider, IServiceProvider_Impl, IStream, ITypeInfo,
            },
            Ole::{
                IOleWindow, IOleWindow_Impl, OLEMENUGROUPWIDTHS, OleInitialize, OleUninitialize,
            },
            Threading::GetCurrentThreadId,
            Variant::{InitVariantFromBuffer, VARIANT, VariantClear},
        },
        UI::{
            Controls::{LPFNSVADDPROPSHEETPAGE, TBBUTTON},
            Shell::{
                _SVGIO, Common::ITEMIDLIST, FOLDERSETTINGS, ILCombine, ILGetSize, IShellBrowser,
                IShellBrowser_Impl, IShellItem, IShellView, IShellView_Impl, IShellWindows,
                IWebBrowser_Impl, IWebBrowserApp, IWebBrowserApp_Impl, SHCreateItemFromIDList,
                SHParseDisplayName, SID_STopLevelBrowser, SIGDN_DESKTOPABSOLUTEPARSING,
                SWC_BROWSER, SWC_EXPLORER, ShellWindows,
            },
            WindowsAndMessaging::{
                CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
                DispatchMessageW, GetMessageW, HMENU, KillTimer, MSG, PostQuitMessage,
                RegisterClassExW, SW_HIDE, SetTimer, ShowWindow, TranslateMessage,
                UnregisterClassW, WM_DESTROY, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE,
                WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    },
    core::{BOOL, BSTR, GUID, HRESULT, IUnknownImpl, Interface, PCWSTR, PWSTR, Ref, implement},
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

use crate::platform::windows::{
    address_path::ExternalLaunchPath,
    launch_probe::{self, LaunchProbe},
};

const TRAP_TIMEOUT: Duration = Duration::from_millis(3_000);
const TIMER_ID: usize = 1;
const CLASS_NAME: &str = "AsterFiles.ShellSelectTrap.v1";

pub struct SelectFollowup {
    receiver: mpsc::Receiver<Option<PathBuf>>,
}

impl SelectFollowup {
    pub fn wait(self) -> Option<PathBuf> {
        self.receiver.recv().ok().flatten()
    }

    pub fn into_receiver(self) -> mpsc::Receiver<Option<PathBuf>> {
        self.receiver
    }
}

pub fn enrich_folder_open_selects(paths: Vec<ExternalLaunchPath>) -> Vec<ExternalLaunchPath> {
    let probe = launch_probe::collect();
    launch_probe::record(&probe, &paths);
    let Some(folder) = trap_candidate(&paths) else {
        return paths;
    };
    rewrite_explorer_select(folder, &probe, "result=explorer-select-command").unwrap_or(paths)
}

pub fn begin_select_followup(paths: &[ExternalLaunchPath]) -> Option<SelectFollowup> {
    spawn_select_followup(paths, true)
}

pub fn begin_select_followup_for_ipc(paths: &[ExternalLaunchPath]) -> Option<SelectFollowup> {
    spawn_select_followup(paths, false)
}

fn spawn_select_followup(
    paths: &[ExternalLaunchPath],
    skip_desktop_parent: bool,
) -> Option<SelectFollowup> {
    if paths.len() != 1 || paths[0].select {
        return None;
    }
    if skip_desktop_parent && launch_probe::parent_is_desktop_shell() {
        crate::operation_audit::record("shell-select-trap", "result=skip-shell-parent");
        return None;
    }
    let folder = paths[0].path.clone();
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("shell-select-followup".into())
        .spawn(move || {
            let selected = capture_shell_select(&folder, TRAP_TIMEOUT, None);
            crate::operation_audit::record(
                "shell-select-trap",
                if selected.is_some() {
                    "result=selected"
                } else {
                    "result=timeout-or-unregistered"
                },
            );
            let _ = sender.send(selected);
        })
        .ok()?;
    Some(SelectFollowup { receiver })
}

fn rewrite_explorer_select(
    folder: &Path,
    probe: &LaunchProbe,
    event: &str,
) -> Option<Vec<ExternalLaunchPath>> {
    let target = launch_probe::reveal_from_explorer_select(folder, probe)?;
    crate::operation_audit::record("shell-select-trap", event);
    launch_probe::close_explorer_select_windows(folder, probe);
    Some(vec![ExternalLaunchPath::select(target)])
}

fn trap_candidate(paths: &[ExternalLaunchPath]) -> Option<&Path> {
    if paths.len() != 1 {
        return None;
    }
    let item = &paths[0];
    if item.select {
        return None;
    }
    match std::fs::metadata(&item.path) {
        Ok(metadata) if metadata.is_dir() => Some(item.path.as_path()),
        _ => None,
    }
}

fn capture_shell_select(
    folder: &Path,
    timeout: Duration,
    ready: Option<mpsc::Sender<()>>,
) -> Option<PathBuf> {
    let _ole = OleGuard::new()?;
    let inner = Rc::new(Inner {
        hwnd: Cell::new(HWND(ptr::null_mut())),
        folder_pidl: Cell::new(ptr::null_mut()),
        selected: RefCell::new(None),
        browser: RefCell::new(None),
        pending_cookie: Cell::new(0),
        register_cookie: Cell::new(0),
        explorer_cookie: Cell::new(0),
        pending_registered: Cell::new(false),
        window_registered: Cell::new(false),
        explorer_registered: Cell::new(false),
        shell_windows: RefCell::new(None),
    });
    if let Err(detail) = register_pending_shell_window(&inner, folder) {
        crate::operation_audit::record("shell-select-trap", format!("register-failed={detail}"));
        return None;
    }
    let _shell = ShellWindowGuard {
        inner: inner.clone(),
    };

    let host = ShellSelectHost {
        inner: inner.clone(),
    };
    let app: IWebBrowserApp = host.into();
    let dispatch: IDispatch = app.cast().ok()?;
    let view: IShellView = dispatch.cast().ok()?;
    let browser: IShellBrowser = ShellSelectBrowser {
        inner: inner.clone(),
        view,
    }
    .into();
    *inner.browser.borrow_mut() = Some(browser);

    let class = register_class()?;
    let hwnd = create_trap_window(&class)?;
    inner.hwnd.set(hwnd);
    let _window = WindowGuard { hwnd, class };

    if let Err(detail) = complete_shell_window_register(&inner, &dispatch) {
        crate::operation_audit::record("shell-select-trap", format!("register-failed={detail}"));
        return None;
    }
    if let Some(ready) = ready {
        let _ = ready.send(());
    }

    pump_until_select_or_timeout(hwnd, timeout);
    inner.selected.borrow().clone()
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
    match unsafe { shell_windows.Register(dispatch, hwnd, SWC_EXPLORER) } {
        Ok(explorer_cookie) => {
            inner.explorer_cookie.set(explorer_cookie);
            inner.explorer_registered.set(true);
            let _ = unsafe { shell_windows.OnNavigate(explorer_cookie, &location) };
        }
        Err(error) => {
            crate::operation_audit::record(
                "shell-select-trap",
                format!("explorer-register-failed={error}"),
            );
        }
    }
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
    let mut pidl = ptr::null_mut();
    unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None) }.ok()?;
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

fn pump_until_select_or_timeout(hwnd: HWND, timeout: Duration) {
    let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER_ID, millis, None);
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
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
            WS_POPUP,
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
            let _ = ShowWindow(hwnd, SW_HIDE);
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
    folder_pidl: Cell<*mut ITEMIDLIST>,
    selected: RefCell<Option<PathBuf>>,
    browser: RefCell<Option<IShellBrowser>>,
    pending_cookie: Cell<i32>,
    register_cookie: Cell<i32>,
    explorer_cookie: Cell<i32>,
    pending_registered: Cell<bool>,
    window_registered: Cell<bool>,
    explorer_registered: Cell<bool>,
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
        path.filter(|path| !path.as_os_str().is_empty())
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
        let Some(shell_windows) = shell_windows.as_ref() else {
            return;
        };
        if self.inner.window_registered.replace(false) {
            let _ = unsafe { shell_windows.Revoke(self.inner.register_cookie.get()) };
        }
        if self.inner.explorer_registered.replace(false) {
            let _ = unsafe { shell_windows.Revoke(self.inner.explorer_cookie.get()) };
        }
        if self.inner.pending_registered.replace(false)
            && self.inner.pending_cookie.get() != self.inner.register_cookie.get()
        {
            let _ = unsafe { shell_windows.Revoke(self.inner.pending_cookie.get()) };
        }
        let pidl = self.inner.folder_pidl.replace(ptr::null_mut());
        if !pidl.is_null() {
            unsafe { CoTaskMemFree(Some(pidl.cast())) };
        }
    }
}

#[implement(IWebBrowserApp, IServiceProvider, IShellView)]
struct ShellSelectHost {
    inner: Rc<Inner>,
}

#[implement(IShellBrowser)]
struct ShellSelectBrowser {
    inner: Rc<Inner>,
    view: IShellView,
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
        unsafe { PostQuitMessage(0) };
        Ok(())
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
        Ok(BSTR::from("AsterFiles"))
    }
    fn Path(&self) -> windows::core::Result<BSTR> {
        Ok(BSTR::from("AsterFiles"))
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
        guid_service: *const GUID,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        unsafe {
            if ppvobject.is_null() {
                return Err(E_POINTER.into());
            }
            *ppvobject = ptr::null_mut();
            let hr = IUnknownImpl::QueryInterface(self, riid, ppvobject);
            if hr.is_ok() {
                return Ok(());
            }
            let requested = *riid;
            let service = *guid_service;
            let browser = self.inner.browser.borrow().clone();
            let Some(browser) = browser else {
                return hr.ok();
            };
            if requested == IShellBrowser::IID
                || (service == SID_STopLevelBrowser && requested == IOleWindow::IID)
            {
                if requested == IShellBrowser::IID {
                    *ppvobject = Interface::into_raw(browser);
                } else {
                    let window: IOleWindow = browser.cast()?;
                    *ppvobject = Interface::into_raw(window);
                }
                return Ok(());
            }
            hr.ok()
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

    fn SelectItem(&self, pidlitem: *const ITEMIDLIST, _uflags: u32) -> windows::core::Result<()> {
        if let Some(path) = self.inner.take_selected_item(pidlitem) {
            *self.inner.selected.borrow_mut() = Some(path);
            unsafe { PostQuitMessage(0) };
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

#[allow(non_snake_case)]
impl IOleWindow_Impl for ShellSelectBrowser_Impl {
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
impl IShellBrowser_Impl for ShellSelectBrowser_Impl {
    fn InsertMenusSB(
        &self,
        _hmenushared: HMENU,
        _lpmenuwidths: *mut OLEMENUGROUPWIDTHS,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn SetMenuSB(
        &self,
        _hmenushared: HMENU,
        _holemenures: isize,
        _hwndactiveobject: HWND,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn RemoveMenusSB(&self, _hmenushared: HMENU) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn SetStatusTextSB(&self, _pszstatustext: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn EnableModelessSB(&self, _fenable: BOOL) -> windows::core::Result<()> {
        Ok(())
    }

    fn TranslateAcceleratorSB(&self, _pmsg: *const MSG, _wid: u16) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn BrowseObject(&self, _pidl: *const ITEMIDLIST, _wflags: u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn GetViewStateStream(&self, _grfmode: u32) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }

    fn GetControlWindow(&self, _id: u32) -> windows::core::Result<HWND> {
        Err(E_NOTIMPL.into())
    }

    fn SendControlMsg(
        &self,
        _id: u32,
        _umsg: u32,
        _wparam: WPARAM,
        _lparam: LPARAM,
        _pret: *mut LRESULT,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryActiveShellView(&self) -> windows::core::Result<IShellView> {
        Ok(self.view.clone())
    }

    fn OnViewWindowActive(&self, _pshv: Ref<IShellView>) -> windows::core::Result<()> {
        Ok(())
    }

    fn SetToolbarItems(
        &self,
        _lpbuttons: *const TBBUTTON,
        _nbuttons: u32,
        _uflags: u32,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::SHOpenFolderAndSelectItems;

    fn launch(path: &Path, select: bool) -> ExternalLaunchPath {
        ExternalLaunchPath {
            path: path.to_path_buf(),
            select,
        }
    }

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

    #[test]
    fn issue_118_trap_ignores_explicit_select_and_files() {
        let root = unique_dir();
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("target.txt");
        std::fs::write(&file, b"issue-118").unwrap();
        assert!(trap_candidate(&[launch(&root, true)]).is_none());
        assert!(trap_candidate(&[launch(&file, false)]).is_none());
        assert_eq!(
            trap_candidate(&[launch(&root, false)]),
            Some(root.as_path())
        );
        let started = std::time::Instant::now();
        assert_eq!(
            enrich_folder_open_selects(vec![launch(&root, false)]),
            vec![launch(&root, false)]
        );
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "Folder Open must not wait for IShellWindows SelectItem"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn issue_118_shell_windows_select_item_from_shopenfolderandselectitems() {
        let root = unique_dir();
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("选中文件.txt");
        std::fs::write(&file, b"issue-118").unwrap();
        let trap_root = root.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("shell-select-trap-test".into())
            .spawn(move || capture_shell_select(&trap_root, Duration::from_secs(4), Some(ready_tx)))
            .expect("trap thread starts");
        ready_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("shell window registered before SHOpenFolderAndSelectItems");

        let opener_file = file.clone();
        let opener = std::thread::Builder::new()
            .name("shell-select-trap-open".into())
            .spawn(move || {
                let _ole = OleGuard::new().expect("opener can initialize OLE");
                let pidl = parse_pidl(&opener_file).expect("file pidl");
                let opened = unsafe { SHOpenFolderAndSelectItems(pidl, None, 0) };
                unsafe { CoTaskMemFree(Some(pidl.cast())) };
                opened
            })
            .expect("opener thread starts");

        let selected = worker.join().expect("trap thread joins");
        let opened = opener.join().expect("opener thread joins");
        let _ = std::fs::remove_dir_all(&root);
        opened.expect("SHOpenFolderAndSelectItems reaches the registered trap");
        assert_eq!(selected.as_deref(), Some(file.as_path()));
    }
}
