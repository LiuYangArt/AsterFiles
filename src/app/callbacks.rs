use super::*;

mod address;
mod context_menu;
mod file_list;
mod navigation;
mod operations;
mod settings;
mod sidebar;
mod tabs;

#[allow(clippy::too_many_arguments)]
pub(super) fn wire_callbacks(
    ui: &AppWindow,
    network_login_ui: slint::Weak<NetworkLoginWindow>,
    network_login_state: Arc<Mutex<NetworkLoginCoordinator>>,
    network_location_rename_ui: slint::Weak<NetworkLocationRenameWindow>,
    webdav_location_ui: slint::Weak<WebDavLocationWindow>,
    operation_window_ui: slint::Weak<OperationWindow>,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    sender: mpsc::Sender<DirectoryRequest>,
    network_sender: mpsc::SyncSender<DirectoryRequest>,
    network_discovery_sender: mpsc::SyncSender<NetworkDiscoveryRequest>,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    clipboard_sender: mpsc::Sender<ClipboardRequest>,
    everything_sender: mpsc::Sender<EverythingRequest>,
    icon_sender: mpsc::Sender<IconRequest>,
    thumbnail_scheduler: ThumbnailScheduler,
    shortcut_sender: mpsc::SyncSender<ShortcutRequest>,
    shell_menu_worker: platform::windows::context_menu::ShellMenuWorker,
    quick_menu: SharedQuickMenu,
    state: WindowSessions,
) {
    let workers = WorkerSenders {
        network_login: network_login_ui,
        network_login_state,
        network_location_rename: network_location_rename_ui,
        webdav_location: webdav_location_ui,
        operation_window: operation_window_ui,
        directory: sender,
        network_directory: network_sender,
        network_discovery: network_discovery_sender,
        operation: operation_sender,
        clipboard: clipboard_sender,
        everything: everything_sender,
        icon: icon_sender,
        thumbnail: thumbnail_scheduler,
        shortcut: shortcut_sender,
        shell_menu: shell_menu_worker,
    };
    let confirmations = ConfirmationWindows::new(delete_ui, conflict_ui, exit_ui);
    wire_system_integration(ui, &state);
    breadcrumb::wire_resize(ui);
    address_edit::wire_dismissal(ui, &state);
    sidebar::wire_network(ui, &state, &workers);
    address::wire_path(ui, &state, &workers);
    file_list::wire_viewport(ui, &state, &workers);
    address::wire_search(ui, &state, &workers);
    file_list::wire_open(ui, &state, &workers);
    address::wire_breadcrumb(ui, &state, &workers);
    sidebar::wire_locations(ui, &state, &workers);
    file_list::wire_selection(ui, &state, &workers);
    file_list::wire_layout(ui, &state, &workers);
    tabs::wire_new(ui, &state, &workers);
    sidebar::wire_home(ui, &state, &workers);
    tabs::wire_tabs(ui, &state, &workers, &confirmations);
    navigation::wire_navigation(ui, &state, &workers);
    settings::wire_settings(ui, &state, &workers, &confirmations);
    let context_anchor = Arc::new(Mutex::new((false, 240_i32, 180_i32)));
    context_menu::wire_open(ui, &state, &workers, &quick_menu, context_anchor.clone());
    context_menu::wire_interaction(ui, &workers, &quick_menu);
    context_menu::wire_command(
        ui,
        &state,
        &workers,
        &confirmations,
        &quick_menu,
        context_anchor.clone(),
    );
    operations::wire_operations(ui, &state, &workers, &confirmations);
    operations::wire_close(ui, &state, &confirmations);
}
