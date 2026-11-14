use super::*;

#[derive(Debug)]
pub(super) struct Submission {
    target: actions::EntryTarget,
    input: String,
    selection_end: i32,
    eligible_for_restore: bool,
}

pub(super) fn wire(
    ui: &AppWindow,
    state: &WindowSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
) {
    let weak = ui.as_weak();
    let state_for_rename = state.clone();
    ui.on_begin_rename(move || {
        begin_rename_ui(&weak, &state_for_rename);
    });
    let weak = ui.as_weak();
    let state_for_commit_rename = state.clone();
    let sender_for_rename = sender.clone();
    ui.on_commit_rename(move |name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let result = submit_rename(&state_for_commit_rename, &sender_for_rename, name.as_str());
        match result {
            Ok(operation) => {
                if let Ok(mut app) = state_for_commit_rename.lock()
                    && let Some(target) = app
                        .rename_targets
                        .remove(&state_for_commit_rename.window_id)
                    && let Some(id) = operation
                {
                    app.submitted_renames.insert(
                        id,
                        Submission {
                            target,
                            input: name.to_string(),
                            selection_end: ui.get_rename_selection_end(),
                            eligible_for_restore: false,
                        },
                    );
                }
                // Accepted work owns its snapshot; the editor must not wait for preparation or I/O.
                ui.set_rename_editing(false);
                ui.set_rename_submitting(false);
            }
            Err(message) => {
                if let Ok(mut app) = state_for_commit_rename.lock() {
                    app.operation_errors.push(message);
                }
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
}

pub(super) fn finish(
    state: &SharedSessions,
    id: OperationId,
) -> Option<(Submission, Option<String>)> {
    let mut app = state.lock().expect("app state mutex is not poisoned");
    let task = app.operations.task(id)?;
    if !task.state.is_terminal() {
        return None;
    }
    let error = task.items.iter().find_map(|item| item.error.clone());
    let mut submission = app.submitted_renames.remove(&id)?;
    let Some(window_id) = app.window_for_tab(submission.target.tab.tab_id) else {
        if let Some(message) = error {
            app.operation_errors.push(message);
        }
        return None;
    };
    submission.target.tab.window_id = window_id;
    let target = submission.target;
    submission.eligible_for_restore = !app.rename_targets.contains_key(&window_id)
        && app.tab(target.tab.tab_id).is_some_and(|tab| {
            tab.latest_request() == target.tab.request_id
                && tab.visible_entry(target.entry_id).is_some()
        });
    Some((submission, error))
}

impl Submission {
    pub(super) fn window_id(&self) -> WindowId {
        self.target.tab.window_id
    }
}

pub(super) fn present_completion(
    ui: &AppWindow,
    state: &SharedSessions,
    submission: Submission,
    error: Option<String>,
) {
    let Some(message) = error else {
        return;
    };
    let mut target = submission.target;
    let restore = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.operation_errors.push(message);
        // A late failure must not replace a newer editor or reopen a different directory.
        // Eligibility was captured before this completion's own background refresh.
        let current = submission.eligible_for_restore
            && !app.rename_targets.contains_key(&target.tab.window_id)
            && app.window(target.tab.window_id).is_some_and(|window| {
                window.active_tab == target.tab.tab_id
                    && window
                        .tabs
                        .get(&target.tab.tab_id)
                        .is_some_and(|tab| tab.visible_entry(target.entry_id).is_some())
            });
        if current {
            target.tab.request_id = app.tab(target.tab.tab_id).unwrap().latest_request();
            app.rename_targets.insert(target.tab.window_id, target);
        }
        current
    };
    if restore {
        ui.set_rename_entry_id(target.entry_id.0 as i32);
        ui.set_rename_input(submission.input.into());
        ui.set_rename_selection_end(submission.selection_end);
        ui.set_rename_submitting(false);
        ui.set_rename_editing(true);
        ui.set_rename_submit_generation(ui.get_rename_submit_generation() + 1);
    }
    refresh_regions(
        ui,
        &WindowSessions::new(state.clone(), target.tab.window_id),
        RefreshRegions::STATUS,
    );
}
