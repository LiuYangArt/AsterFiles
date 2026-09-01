#![allow(dead_code)]

use std::{
    cell::Cell,
    io,
    marker::PhantomData,
    mem::size_of,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr,
    rc::Rc,
    sync::mpsc,
    thread,
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RPC_E_CHANGED_MODE, WPARAM},
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
        UI::{
            Shell::{
                CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFOEX,
                GCS_VERBW, IContextMenu, IContextMenu2, IContextMenu3, IShellFolder,
                SHBindToParent, SHParseDisplayName,
            },
            WindowsAndMessaging::{
                CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
                GetMenuItemCount, GetMenuItemInfoW, HMENU, MENUITEMINFOW, MFS_CHECKED, MFS_DEFAULT,
                MFS_DISABLED, MFT_SEPARATOR, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING,
                MIIM_SUBMENU, RegisterClassW, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON,
                TrackPopupMenuEx, WINDOW_EX_STYLE, WINDOW_STYLE, WM_DRAWITEM, WM_INITMENUPOPUP,
                WM_MEASUREITEM, WM_MENUCHAR, WNDCLASSW,
            },
        },
    },
    core::{Interface, PCSTR, PCWSTR, PSTR, PWSTR, w},
};

const FIRST_COMMAND_ID: u32 = 1;
const LAST_COMMAND_ID: u32 = 0x7fff;
const MENU_WINDOW_CLASS: PCWSTR = w!("AsterFiles.ClassicMenuOwner");

thread_local! {
    static ACTIVE_MENU_SESSION: Cell<*const ClassicMenuSession> = const { Cell::new(ptr::null()) };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassicMenuItem {
    pub command_id: Option<u32>,
    pub title: String,
    pub verb: Option<String>,
    pub enabled: bool,
    pub checked: bool,
    pub default: bool,
    pub kind: ClassicMenuItemKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassicMenuItemKind {
    Command,
    Separator,
    Submenu(Vec<ClassicMenuItem>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassicMenuInvocation {
    BuiltIn { verb: String },
    Shell { verb: Option<String> },
}

pub type ShellMenuSessionId = u64;
pub type ShellMenuRequestId = u64;

#[derive(Debug, Clone)]
pub enum ShellMenuLoadTarget {
    Paths(Vec<PathBuf>),
    Background(PathBuf),
}

#[derive(Debug)]
pub enum ShellMenuCommand {
    Load {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        target: ShellMenuLoadTarget,
        include_extended_verbs: bool,
        owner_window: isize,
    },
    Invoke {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        command_id: u32,
        owner_window: isize,
        screen_x: i32,
        screen_y: i32,
    },
    Close {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
    },
}

#[derive(Debug)]
pub enum ShellMenuEvent {
    Loaded {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        items: Vec<ClassicMenuItem>,
        elapsed_ms: u128,
    },
    Invoked {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        invocation: ClassicMenuInvocation,
        elapsed_ms: u128,
    },
    Error {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        operation: &'static str,
        message: String,
        elapsed_ms: u128,
    },
    Closed {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
    },
}

#[derive(Clone)]
pub struct ShellMenuWorker {
    commands: mpsc::Sender<ShellMenuCommand>,
}

impl ShellMenuWorker {
    pub fn spawn() -> (Self, mpsc::Receiver<ShellMenuEvent>) {
        let (commands, command_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        thread::spawn(move || shell_menu_worker_loop(command_rx, event_tx));
        (Self { commands }, events)
    }

    pub fn send(&self, command: ShellMenuCommand) -> io::Result<()> {
        self.commands
            .send(command)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "shell menu worker stopped"))
    }
}

fn shell_menu_worker_loop(
    commands: mpsc::Receiver<ShellMenuCommand>,
    events: mpsc::Sender<ShellMenuEvent>,
) {
    let mut session: Option<(ShellMenuSessionId, ShellMenuRequestId, ClassicMenuSession)> = None;
    while let Ok(command) = commands.recv() {
        match command {
            ShellMenuCommand::Load {
                session_id,
                request_id,
                target,
                include_extended_verbs,
                owner_window,
            } => {
                let started = std::time::Instant::now();
                let result = match target {
                    ShellMenuLoadTarget::Paths(paths) => ClassicMenuSession::for_paths_with_owner(
                        &paths,
                        include_extended_verbs,
                        owner_window,
                    ),
                    ShellMenuLoadTarget::Background(folder) => {
                        ClassicMenuSession::for_background_with_owner(
                            &folder,
                            include_extended_verbs,
                            owner_window,
                        )
                    }
                }
                .and_then(|menu| {
                    let items = menu.items_top_level()?;
                    session = Some((session_id, request_id, menu));
                    Ok(items)
                });
                match result {
                    Ok(items) => {
                        let _ = events.send(ShellMenuEvent::Loaded {
                            session_id,
                            request_id,
                            items,
                            elapsed_ms: started.elapsed().as_millis(),
                        });
                    }
                    Err(error) => {
                        let _ = events.send(ShellMenuEvent::Error {
                            session_id,
                            request_id,
                            operation: "load",
                            message: error.to_string(),
                            elapsed_ms: started.elapsed().as_millis(),
                        });
                    }
                }
            }
            ShellMenuCommand::Invoke {
                session_id,
                request_id,
                command_id,
                owner_window,
                screen_x,
                screen_y,
            } => {
                let started = std::time::Instant::now();
                let result = session
                    .as_ref()
                    .filter(|(id, req, _)| *id == session_id && *req == request_id)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "shell menu session not found")
                    })
                    .and_then(|(_, _, menu)| {
                        menu.invoke_command(command_id, owner_window, screen_x, screen_y)
                    });
                match result {
                    Ok(invocation) => {
                        let _ = events.send(ShellMenuEvent::Invoked {
                            session_id,
                            request_id,
                            invocation,
                            elapsed_ms: started.elapsed().as_millis(),
                        });
                    }
                    Err(error) => {
                        let _ = events.send(ShellMenuEvent::Error {
                            session_id,
                            request_id,
                            operation: "invoke",
                            message: error.to_string(),
                            elapsed_ms: started.elapsed().as_millis(),
                        });
                    }
                }
            }
            ShellMenuCommand::Close {
                session_id,
                request_id,
            } => {
                if session
                    .as_ref()
                    .is_some_and(|(id, req, _)| *id == session_id && *req == request_id)
                {
                    session = None;
                }
                let _ = events.send(ShellMenuEvent::Closed {
                    session_id,
                    request_id,
                });
            }
        }
    }
}

pub struct ClassicMenuSession {
    menu: HMENU,
    context_menu: IContextMenu,
    context_menu2: Option<IContextMenu2>,
    context_menu3: Option<IContextMenu3>,
    com_initialized: bool,
    _thread_affinity: PhantomData<Rc<()>>,
}

impl ClassicMenuSession {
    pub fn for_paths_with_owner(
        paths: &[PathBuf],
        include_extended_verbs: bool,
        owner_window: isize,
    ) -> io::Result<Self> {
        validate_selection(paths)?;
        Self::create(include_extended_verbs, || {
            create_selection_context_menu(paths, HWND(owner_window as *mut _))
        })
    }

    pub fn for_background_with_owner(
        folder: &Path,
        include_extended_verbs: bool,
        owner_window: isize,
    ) -> io::Result<Self> {
        validate_background(folder)?;
        Self::create(include_extended_verbs, || {
            create_background_context_menu(folder, HWND(owner_window as *mut _))
        })
    }

    fn create(
        include_extended_verbs: bool,
        create_context_menu: impl FnOnce() -> io::Result<IContextMenu>,
    ) -> io::Result<Self> {
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if initialized == RPC_E_CHANGED_MODE {
            return Err(io::Error::other(
                "classic menu requires an STA owning thread",
            ));
        }
        initialized.ok().map_err(windows_error)?;
        let context_menu = match create_context_menu() {
            Ok(value) => value,
            Err(error) => {
                unsafe { CoUninitialize() };
                return Err(error);
            }
        };
        let context_menu3 = context_menu.cast::<IContextMenu3>().ok();
        let context_menu2 = context_menu.cast::<IContextMenu2>().ok();
        let menu = unsafe { CreatePopupMenu() }.map_err(|error| {
            unsafe { CoUninitialize() };
            windows_error(error)
        })?;
        let flags = CMF_NORMAL
            | if include_extended_verbs {
                CMF_EXTENDEDVERBS
            } else {
                0
            };
        let query = unsafe {
            context_menu.QueryContextMenu(menu, 0, FIRST_COMMAND_ID, LAST_COMMAND_ID, flags)
        };
        if query.is_err() {
            unsafe {
                let _ = DestroyMenu(menu);
                CoUninitialize();
            }
            return Err(io::Error::other(format!(
                "QueryContextMenu failed: {query:?}"
            )));
        }
        Ok(Self {
            menu,
            context_menu,
            context_menu2,
            context_menu3,
            com_initialized: true,
            _thread_affinity: PhantomData,
        })
    }

    pub fn items(&self) -> io::Result<Vec<ClassicMenuItem>> {
        read_menu(self.menu, self, true)
    }

    fn items_top_level(&self) -> io::Result<Vec<ClassicMenuItem>> {
        read_menu(self.menu, self, false)
    }

    fn invoke_command(
        &self,
        command_id: u32,
        owner_window: isize,
        screen_x: i32,
        screen_y: i32,
    ) -> io::Result<ClassicMenuInvocation> {
        let offset = command_id
            .checked_sub(FIRST_COMMAND_ID)
            .ok_or_else(|| io::Error::other("shell returned an invalid menu command"))?;
        let verb = command_verb(&self.context_menu, command_id);
        if let Some(verb) = verb.as_deref().filter(|verb| is_builtin_verb(verb)) {
            return Ok(ClassicMenuInvocation::BuiltIn {
                verb: verb.to_owned(),
            });
        }
        let invocation = CMINVOKECOMMANDINFOEX {
            cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_PTINVOKE,
            hwnd: HWND(owner_window as *mut _),
            lpVerb: PCSTR(offset as usize as *const u8),
            lpParameters: PCSTR::null(),
            lpDirectory: PCSTR::null(),
            nShow: SW_SHOWNORMAL.0,
            dwHotKey: 0,
            hIcon: Default::default(),
            lpTitle: PCSTR::null(),
            lpVerbW: PCWSTR::null(),
            lpParametersW: PCWSTR::null(),
            lpDirectoryW: PCWSTR::null(),
            lpTitleW: PCWSTR::null(),
            ptInvoke: POINT {
                x: screen_x,
                y: screen_y,
            },
        };
        let base = (&invocation as *const CMINVOKECOMMANDINFOEX).cast();
        unsafe { self.context_menu.InvokeCommand(&*base) }.map_err(windows_error)?;
        Ok(ClassicMenuInvocation::Shell { verb })
    }
    pub fn show_native_and_invoke(
        &self,
        owner_window: isize,
        screen_x: i32,
        screen_y: i32,
    ) -> io::Result<Option<ClassicMenuInvocation>> {
        let popup_owner = PopupOwner::create(HWND(owner_window as *mut _), self)?;
        let selected = unsafe {
            TrackPopupMenuEx(
                self.menu,
                (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
                screen_x,
                screen_y,
                popup_owner.hwnd,
                None,
            )
        };
        if selected.0 == 0 {
            return Ok(None);
        }
        let command_id = selected.0 as u32;
        self.invoke_command(command_id, owner_window, screen_x, screen_y)
            .map(Some)
    }

    fn initialize_submenu(&self, submenu: HMENU, position: u32) {
        let _ = self.forward_menu_message(
            WM_INITMENUPOPUP,
            WPARAM(submenu.0 as usize),
            LPARAM(position as isize),
        );
    }

    fn forward_menu_message(
        &self,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        if !is_dynamic_menu_message(message) {
            return None;
        }
        if let Some(menu3) = &self.context_menu3 {
            let mut result = LRESULT::default();
            if unsafe { menu3.HandleMenuMsg2(message, wparam, lparam, Some(&mut result)) }.is_ok() {
                return Some(result);
            }
        }
        if let Some(menu2) = &self.context_menu2
            && unsafe { menu2.HandleMenuMsg(message, wparam, lparam) }.is_ok()
        {
            return Some(LRESULT::default());
        }
        None
    }
}

impl Drop for ClassicMenuSession {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.menu);
        }
        if self.com_initialized {
            unsafe { CoUninitialize() };
        }
    }
}

struct PopupOwner {
    hwnd: HWND,
}
impl PopupOwner {
    fn create(parent: HWND, session: &ClassicMenuSession) -> io::Result<Self> {
        let class = WNDCLASSW {
            lpfnWndProc: Some(menu_window_proc),
            lpszClassName: MENU_WINDOW_CLASS,
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&class);
        }
        ACTIVE_MENU_SESSION.with(|active| active.set(session));
        match unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                MENU_WINDOW_CLASS,
                w!(""),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                (!parent.is_invalid()).then_some(parent),
                None,
                None,
                None,
            )
        } {
            Ok(hwnd) => Ok(Self { hwnd }),
            Err(error) => {
                ACTIVE_MENU_SESSION.with(|active| active.set(ptr::null()));
                Err(windows_error(error))
            }
        }
    }
}
impl Drop for PopupOwner {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        ACTIVE_MENU_SESSION.with(|active| active.set(ptr::null()));
    }
}

unsafe extern "system" fn menu_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(result) = ACTIVE_MENU_SESSION
        .with(|active| {
            let session = active.get();
            (!session.is_null()).then(|| unsafe { &*session })
        })
        .and_then(|session| session.forward_menu_message(message, wparam, lparam))
    {
        return result;
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn validate_selection(paths: &[PathBuf]) -> io::Result<()> {
    let Some(first) = paths.first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shell menu selection is empty",
        ));
    };
    let parent = first.parent();
    if parent.is_none()
        || paths
            .iter()
            .any(|path| path.as_os_str().is_empty() || path.parent() != parent)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "classic menu selection must share one parent folder",
        ));
    }
    Ok(())
}

fn validate_background(folder: &Path) -> io::Result<()> {
    if folder.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "classic background menu folder is empty",
        ));
    }
    Ok(())
}

fn create_selection_context_menu(paths: &[PathBuf], owner: HWND) -> io::Result<IContextMenu> {
    let mut full_pidls = Vec::with_capacity(paths.len());
    let mut child_pidls = Vec::with_capacity(paths.len());
    let mut parent_folder = None;
    let result = (|| {
        for path in paths {
            let wide = wide_null(path);
            let mut full_pidl = ptr::null_mut();
            unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut full_pidl, 0, None) }
                .map_err(windows_error)?;
            full_pidls.push(full_pidl);
            let mut child_pidl = ptr::null_mut();
            let folder: IShellFolder = unsafe { SHBindToParent(full_pidl, Some(&mut child_pidl)) }
                .map_err(windows_error)?;
            if parent_folder.is_none() {
                parent_folder = Some(folder);
            }
            child_pidls.push(child_pidl.cast_const());
        }
        unsafe {
            parent_folder
                .as_ref()
                .expect("validated selection")
                .GetUIObjectOf(owner, &child_pidls, None)
        }
        .map_err(windows_error)
    })();
    free_pidls(full_pidls);
    result
}

fn create_background_context_menu(folder: &Path, owner: HWND) -> io::Result<IContextMenu> {
    let wide = wide_null(folder);
    let mut full_pidl = ptr::null_mut();
    unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut full_pidl, 0, None) }
        .map_err(windows_error)?;
    let result = (|| {
        let mut child_pidl = ptr::null_mut();
        let parent: IShellFolder =
            unsafe { SHBindToParent(full_pidl, Some(&mut child_pidl)) }.map_err(windows_error)?;
        let folder: IShellFolder =
            unsafe { parent.BindToObject(child_pidl.cast_const(), None) }.map_err(windows_error)?;
        unsafe { folder.CreateViewObject(owner) }.map_err(windows_error)
    })();
    free_pidls(vec![full_pidl]);
    result
}

fn free_pidls(pidls: Vec<*mut windows::Win32::UI::Shell::Common::ITEMIDLIST>) {
    for pidl in pidls {
        unsafe { CoTaskMemFree(Some(pidl.cast())) };
    }
}

fn read_menu(
    menu: HMENU,
    session: &ClassicMenuSession,
    recurse: bool,
) -> io::Result<Vec<ClassicMenuItem>> {
    let count = unsafe { GetMenuItemCount(Some(menu)) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut items = Vec::with_capacity(count as usize);
    for position in 0..count as u32 {
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_ID | MIIM_STATE | MIIM_STRING | MIIM_SUBMENU,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(menu, position, true, &mut info) }.map_err(windows_error)?;
        if info.fType.contains(MFT_SEPARATOR) {
            items.push(ClassicMenuItem {
                command_id: None,
                title: String::new(),
                verb: None,
                enabled: false,
                checked: false,
                default: false,
                kind: ClassicMenuItemKind::Separator,
            });
            continue;
        }
        let mut title = vec![0_u16; info.cch as usize + 1];
        info.dwTypeData = PWSTR(title.as_mut_ptr());
        info.cch = title.len() as u32;
        unsafe { GetMenuItemInfoW(menu, position, true, &mut info) }.map_err(windows_error)?;
        let command_id = (FIRST_COMMAND_ID..=LAST_COMMAND_ID)
            .contains(&info.wID)
            .then_some(info.wID);
        let kind = if info.hSubMenu.is_invalid() {
            ClassicMenuItemKind::Command
        } else if recurse {
            session.initialize_submenu(info.hSubMenu, position);
            ClassicMenuItemKind::Submenu(read_menu(info.hSubMenu, session, true)?)
        } else {
            ClassicMenuItemKind::Submenu(Vec::new())
        };
        items.push(ClassicMenuItem {
            command_id,
            title: clean_menu_title(&String::from_utf16_lossy(&title[..info.cch as usize])),
            verb: command_id.and_then(|id| command_verb(&session.context_menu, id)),
            enabled: !info.fState.contains(MFS_DISABLED),
            checked: info.fState.contains(MFS_CHECKED),
            default: info.fState.contains(MFS_DEFAULT),
            kind,
        });
    }
    Ok(items)
}

fn command_verb(context_menu: &IContextMenu, command_id: u32) -> Option<String> {
    let offset = command_id.checked_sub(FIRST_COMMAND_ID)? as usize;
    let mut buffer = vec![0_u16; 260];
    unsafe {
        context_menu.GetCommandString(
            offset,
            GCS_VERBW,
            None,
            PSTR(buffer.as_mut_ptr().cast()),
            buffer.len() as u32,
        )
    }
    .ok()?;
    let length = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    (length != 0).then(|| String::from_utf16_lossy(&buffer[..length]).to_ascii_lowercase())
}
fn is_builtin_verb(verb: &str) -> bool {
    matches!(verb, "cut" | "copy" | "paste" | "delete" | "rename")
}
fn is_dynamic_menu_message(message: u32) -> bool {
    matches!(
        message,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    )
}
fn clean_menu_title(title: &str) -> String {
    let mut cleaned = String::with_capacity(title.len());
    let mut chars = title.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '&' {
            if chars.peek() == Some(&'&') {
                cleaned.push('&');
                chars.next();
            }
        } else {
            cleaned.push(ch);
        }
    }
    cleaned
}
fn wide_null(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn windows_error(error: windows::core::Error) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_requires_a_shared_parent() {
        assert!(validate_selection(&[]).is_err());
        assert!(validate_selection(&[PathBuf::from(r"C:\")]).is_err());
        assert!(
            validate_selection(&[
                PathBuf::from(r"C:\one\a.txt"),
                PathBuf::from(r"C:\two\b.txt")
            ])
            .is_err()
        );
        assert!(
            validate_selection(&[
                PathBuf::from(r"C:\one\a.txt"),
                PathBuf::from(r"C:\one\b.txt")
            ])
            .is_ok()
        );
    }
    #[test]
    fn background_requires_a_folder_identity() {
        assert!(validate_background(Path::new("")).is_err());
        assert!(validate_background(Path::new(r"C:\one")).is_ok());
    }
    #[test]
    fn asterfiles_commands_are_not_sent_to_shell() {
        for verb in ["cut", "copy", "paste", "delete", "rename"] {
            assert!(is_builtin_verb(verb));
        }
        assert!(!is_builtin_verb("openwith"));
    }
    #[test]
    fn only_shell_dynamic_menu_messages_are_forwarded() {
        for message in [WM_INITMENUPOPUP, WM_DRAWITEM, WM_MEASUREITEM, WM_MENUCHAR] {
            assert!(is_dynamic_menu_message(message));
        }
        assert!(!is_dynamic_menu_message(0));
    }
    #[test]
    fn menu_titles_preserve_literal_ampersands() {
        assert_eq!(clean_menu_title("&Open"), "Open");
        assert_eq!(clean_menu_title("A && B"), "A & B");
        assert_eq!(clean_menu_title("Save &as"), "Save as");
    }
}
