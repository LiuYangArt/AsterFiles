use super::*;

pub(super) fn wire_operations(
    ui: &AppWindow,
    state: &WindowSessions,
    workers: &WorkerSenders,
    confirmations: &ConfirmationWindows,
) {
    let operation_sender = workers.operation.clone();
    let clipboard_sender = workers.clipboard.clone();
    let state_for_undo = state.clone();
    let sender_for_undo = operation_sender.clone();
    ui.on_request_undo(move || {
        let origin_tab = state_for_undo
            .lock()
            .ok()
            .map(|app| app.active_window_state().active_tab);
        if let Some(origin_tab) = origin_tab {
            request_undo(&state_for_undo, &sender_for_undo, origin_tab);
        }
    });

    let state_for_copy = state.clone();
    let clipboard_for_copy = clipboard_sender.clone();
    ui.on_copy_selection(move |cut| {
        request_clipboard_write(&state_for_copy, &clipboard_for_copy, cut);
    });
    let state_for_paste = state.clone();
    let clipboard_for_paste = clipboard_sender;
    ui.on_paste_files(move || {
        request_clipboard_paste(&state_for_paste, &clipboard_for_paste);
    });

    let weak = ui.as_weak();
    let state_for_rename = state.clone();
    ui.on_begin_rename(move || {
        begin_rename_ui(&weak, &state_for_rename);
    });
    let weak = ui.as_weak();
    let state_for_commit_rename = state.clone();
    let sender_for_rename = operation_sender.clone();
    ui.on_commit_rename(move |name| {
        if let Err(message) =
            submit_rename(&state_for_commit_rename, &sender_for_rename, name.as_str())
        {
            if let Ok(mut app) = state_for_commit_rename.lock() {
                app.operation_errors.push(message);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_rename_submitting(false);
                ui.set_rename_submit_generation(ui.get_rename_submit_generation() + 1);
                refresh_regions(&ui, &state_for_commit_rename, RefreshRegions::STATUS);
            }
        }
    });
    let weak = ui.as_weak();
    let state_for_cancel_rename = state.clone();
    ui.on_cancel_rename(move || {
        if let Ok(mut app) = state_for_cancel_rename.lock() {
            app.rename_targets
                .remove(&state_for_cancel_rename.window_id);
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_rename_submitting(false);
            ui.set_rename_editing(false);
            refresh_regions(&ui, &state_for_cancel_rename, RefreshRegions::STATUS);
        }
    });

    let weak = ui.as_weak();
    let state_for_delete = state.clone();
    let sender_for_delete = operation_sender.clone();
    let delete_weak = confirmations.delete.clone();
    ui.on_request_delete(move |permanent| {
        if permanent {
            if let (Some(ui), Some(delete_ui)) = (weak.upgrade(), delete_weak.upgrade()) {
                request_permanent_delete(&state_for_delete, &ui, &delete_ui);
            }
        } else {
            submit_delete(&state_for_delete, &sender_for_delete, false);
        }
    });
    let weak = ui.as_weak();
    let state_for_cancel_operation = state.clone();
    ui.on_cancel_operation(move |id| {
        if webdav_ui::cancel_transfer(id as u64) {
            if let Some(ui) = weak.upgrade() {
                refresh_regions(&ui, &state_for_cancel_operation, RefreshRegions::STATUS);
            }
            return;
        }
        let _ = actions::cancel(
            &state_for_cancel_operation.shared,
            state_for_cancel_operation.window_id,
            OperationId(id as u64),
        );
        if let Some(ui) = weak.upgrade() {
            refresh_regions(&ui, &state_for_cancel_operation, RefreshRegions::STATUS);
        }
    });
    let state_for_retry = state.clone();
    let sender_for_retry = operation_sender.clone();
    ui.on_retry_operation(move |id| {
        if let Some(request) = prepare_retry(&state_for_retry, OperationId(id as u64)) {
            let _ = sender_for_retry.send(request);
        }
    });
}

pub(super) fn wire_close(
    ui: &AppWindow,
    state: &WindowSessions,
    confirmations: &ConfirmationWindows,
) {
    let weak = ui.as_weak();
    let state_for_close = state.clone();
    let exit_weak = confirmations.exit.clone();
    ui.on_request_close(move || {
        let action = state_for_close
            .lock()
            .map(|app| app.request_window_close(state_for_close.window_id))
            .unwrap_or(WindowCloseAction::Ignore);
        match action {
            WindowCloseAction::ConfirmApplicationExit => {
                if let (Some(ui), Some(exit_ui)) = (weak.upgrade(), exit_weak.upgrade()) {
                    show_confirmation_window(&ui, None, &exit_ui);
                }
            }
            WindowCloseAction::CloseWindow => {
                if let Ok(mut app) = state_for_close.lock() {
                    app.cancel_tab_drag_for_window(state_for_close.window_id);
                    let _ = app.close_window(state_for_close.window_id);
                }
                if let Some(ui) = weak.upgrade() {
                    let _ = ui.hide();
                }
                remove_window_runtime(state_for_close.window_id);
            }
            WindowCloseAction::ExitApplication => {
                platform::windows::network::record_runtime_event("event_loop_exit_requested");
                platform::windows::drag_drop::begin_shutdown_current();
                if let Some(ui) = weak.upgrade() {
                    let _ = ui.hide();
                }
            }
            WindowCloseAction::Ignore => {}
        }
    });
}
