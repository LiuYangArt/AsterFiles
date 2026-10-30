use super::*;

pub(super) fn wire_dismissal(ui: &AppWindow, state: &WindowSessions) {
    let state = state.clone();
    let weak = ui.as_weak();
    ui.on_dismiss_address_edit(move || {
        {
            let mut app = state.lock().expect("app state mutex is not poisoned");
            let tab_id = app.active_window_state().active_tab;
            if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) {
                tab.dismiss_address_edit();
            }
        }
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state);
        }
    });
}

pub(super) fn finish_validation(
    ui: &AppWindow,
    state: &WindowSessions,
    workers: &actions::ActionWorkers,
    everything: &mpsc::Sender<EverythingRequest>,
    target: actions::ActionTarget,
    directory: Option<PathBuf>,
    query: String,
) {
    let current = state.lock().ok().is_some_and(|app| {
        app.window(target.window_id).is_some_and(|window| {
            window.active_tab == target.tab_id
                && window.tabs.get(&target.tab_id).is_some_and(|tab| {
                    tab.latest_request() == target.request_id && tab.address_editing
                })
        })
    });
    if !current {
        return;
    }
    if let Some(path) = directory {
        if actions::execute(
            &state.shared,
            workers,
            actions::Action::OpenDirectory { target, path },
        )
        .is_ok()
        {
            let mut app = state.lock().expect("app state mutex is not poisoned");
            if let Some(tab) = app.tab_mut(target.tab_id) {
                // A same-location action has no directory event to finish the edit.
                // Restore a search snapshot only when no new navigation was started.
                if tab.page_source == PageSource::Search {
                    tab.cancel_address_edit();
                } else {
                    tab.dismiss_address_edit();
                }
            }
        }
    } else {
        submit_search(everything, state, None, target.tab_id, query);
    }
    refresh_ui(ui, state);
}

#[cfg(test)]
mod tests;
