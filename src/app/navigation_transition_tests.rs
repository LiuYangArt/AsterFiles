use super::*;

fn fixture() -> (AppState, TabId, PathBuf) {
    let path = PathBuf::from(r"C:\issue-151");
    let app = AppState::new_for_test(vec![path.clone()], 0, [0, 1, 2, 3]);
    let tab_id = app.active_window_state().active_tab;
    (app, tab_id, path)
}

fn pending_focus(path: PathBuf, request_id: RequestId) -> PendingFocus {
    PendingFocus {
        directory: path.clone(),
        request_id: Some(request_id),
        paths: vec![path.join("created.txt")],
        action: PendingFocusAction::Select,
    }
}

#[test]
fn issue_151_dispatch_failure_cancels_request_and_preserves_committed_location() {
    let (mut app, tab_id, original) = fixture();
    let target = original.join("next");
    let tab = app.tab_mut(tab_id).unwrap();
    let history = tab.back_history().clone();
    let (request_id, cancel) =
        tab.begin_directory_navigation(target.clone(), NavigationKind::Normal);
    app.focus_after_refresh
        .insert(tab_id, pending_focus(target.clone(), request_id));
    let request = DirectoryRequest {
        tab_id,
        request_id,
        path: target,
        library: None,
        library_sources: None,
        unavailable_library_sources: 0,
        visibility: crate::domain::FileVisibility::default(),
        cancel: cancel.clone(),
        webdav: None,
    };
    let state = Arc::new(Mutex::new(app));
    fail_directory_dispatch(&state, &request);
    let app = state.lock().unwrap();
    let tab = app.tab(tab_id).unwrap();
    assert!(cancel.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(tab.load_state, LoadState::Failed);
    assert_eq!(tab.error.as_deref(), Some("queue-unavailable"));
    assert_eq!(
        tab.current_location(),
        &Some(NavigationLocation::Directory(original))
    );
    assert_eq!(tab.back_history(), &history);
    assert!(tab.pending_entries().is_empty());
    assert!(!app.focus_after_refresh.contains_key(&tab_id));
}

#[test]
fn issue_151_wrong_finished_location_preserves_shell_and_focus_waiters() {
    let (mut app, tab_id, path) = fixture();
    let wrong = path.join("wrong");
    let request_id = app
        .tab_mut(tab_id)
        .unwrap()
        .begin_directory_navigation(path, NavigationKind::Refresh)
        .0;
    let window_id = app.active_window;
    let focus = pending_focus(wrong.clone(), request_id);
    app.focus_after_refresh.insert(tab_id, focus.clone());
    app.pending_shell_creates.insert(
        window_id,
        PendingShellCreate {
            window_id,
            tab_id,
            directory: wrong.clone(),
            baseline: HashSet::new(),
            started_at: Instant::now(),
        },
    );
    let state = Arc::new(Mutex::new(app));
    let change = apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id,
            request_id,
            path: wrong,
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );
    assert!(!change.accepted);
    assert!(change.icon_requests.is_empty());
    let app = state.lock().unwrap();
    assert_eq!(app.tab(tab_id).unwrap().load_state, LoadState::Loading);
    assert!(app.pending_shell_creates.contains_key(&window_id));
    assert_eq!(app.focus_after_refresh.get(&tab_id), Some(&focus));
    assert!(app.pending_rename_ui.is_empty());
}

#[test]
fn issue_151_rejected_late_and_post_completion_batches_acknowledge_consumption() {
    let (mut app, tab_id, path) = fixture();
    let old = app
        .tab_mut(tab_id)
        .unwrap()
        .begin_directory_navigation(path.clone(), NavigationKind::Refresh)
        .0;
    let current = app
        .tab_mut(tab_id)
        .unwrap()
        .begin_directory_navigation(path.clone(), NavigationKind::Refresh)
        .0;
    let state = Arc::new(Mutex::new(app));
    let (acknowledgement, consumed) = mpsc::channel();
    apply_event(
        &state,
        DirectoryEvent::Batch {
            tab_id,
            request_id: old,
            entries: vec![],
            acknowledgement,
        },
    );
    consumed.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        state.lock().unwrap().tab(tab_id).unwrap().load_state,
        LoadState::Loading
    );
    apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id,
            request_id: current,
            path,
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );
    let (acknowledgement, consumed) = mpsc::channel();
    apply_event(
        &state,
        DirectoryEvent::Batch {
            tab_id,
            request_id: current,
            entries: vec![],
            acknowledgement,
        },
    );
    consumed.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        state.lock().unwrap().tab(tab_id).unwrap().load_state,
        LoadState::Complete
    );
}

#[test]
fn issue_151_cancel_event_clears_focus_after_local_cancellation() {
    let (mut app, tab_id, path) = fixture();
    let request_id = app
        .tab_mut(tab_id)
        .unwrap()
        .begin_directory_navigation(path.clone(), NavigationKind::Refresh)
        .0;
    app.focus_after_refresh
        .insert(tab_id, pending_focus(path, request_id));
    app.tab_mut(tab_id).unwrap().cancel_pending();
    let state = Arc::new(Mutex::new(app));
    for _ in 0..2 {
        apply_event(&state, DirectoryEvent::Cancelled { tab_id, request_id });
        let app = state.lock().unwrap();
        assert_eq!(app.tab(tab_id).unwrap().load_state, LoadState::Cancelled);
        assert!(!app.focus_after_refresh.contains_key(&tab_id));
        assert!(app.pending_rename_ui.is_empty());
    }
}
