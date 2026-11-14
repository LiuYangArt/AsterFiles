use super::*;
use crate::domain::file_operations::OperationResult;

#[test]
fn issue_187_accepted_rename_exits_editor_before_background_completion() {
    i_slint_backend_testing::init_no_event_loop();
    for (mode, grouped) in layouts() {
        for name in ["original.txt", "changed.txt", "Original.txt"] {
            let fixture = RenameFixture::new(mode, grouped, "original.txt");
            let (sender, _pending_work) = mpsc::channel();
            rename_submission::wire(&fixture.ui, &fixture.state, &sender);
            fixture.begin();
            fixture.ui.set_rename_input(name.into());
            fixture.click(slint::LogicalPosition::new(1100.0, 650.0));
            assert!(
                !fixture.ui.get_rename_editing(),
                "accepted submission must end editing without waiting for workers: {mode:?}/{grouped}/{name}"
            );
            let app = fixture.state.lock().unwrap();
            let unchanged = name == "original.txt";
            assert_eq!(app.operations.iter().count(), usize::from(!unchanged));
            assert_eq!(app.submitted_renames.len(), usize::from(!unchanged));
            assert!(!app.rename_targets.contains_key(&fixture.state.window_id));
            drop(app);
            fixture.ui.window().request_redraw();
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
            assert!(fixture.state.lock().unwrap().operation_errors.is_empty());
            assert!(
                fixture
                    .ui
                    .root_element()
                    .query_descendants()
                    .match_id("InlineRenameEditor::rename-editor")
                    .find_all()
                    .is_empty()
            );
        }
    }
}

#[test]
fn issue_187_invalid_submission_stays_in_editor_with_input_and_error() {
    i_slint_backend_testing::init_no_event_loop();
    let fixture = RenameFixture::new(ViewMode::Details, false, "original.txt");
    let (sender, _work) = mpsc::channel();
    rename_submission::wire(&fixture.ui, &fixture.state, &sender);
    fixture.begin();
    fixture.ui.set_rename_input("bad/name.txt".into());
    fixture.click(slint::LogicalPosition::new(1100.0, 650.0));
    assert!(fixture.ui.get_rename_editing());
    assert_eq!(fixture.ui.get_rename_input(), "bad/name.txt");
    assert!(!fixture.ui.get_rename_submitting());
    let app = fixture.state.lock().unwrap();
    assert_eq!(app.operations.iter().count(), 0);
    assert_eq!(app.operation_errors.len(), 1);
}

#[test]
fn issue_187_real_rename_completion_and_failure_do_not_depend_on_editor_visibility() {
    i_slint_backend_testing::init_no_event_loop();
    for failure in [false, true] {
        for newer_editor in [false, true] {
            let root = std::env::temp_dir().join(format!(
                "asterfiles-issue-187-submit-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&root).unwrap();
            let _cleanup = test_support::FixtureCleanup(root.clone());
            std::fs::write(root.join("original.txt"), b"original-content").unwrap();
            if failure {
                std::fs::write(root.join("changed.txt"), b"occupied").unwrap();
            }
            let fixture = RenameFixture::at(ViewMode::Details, false, "original.txt", root.clone());
            let (sender, requests) = mpsc::channel();
            rename_submission::wire(&fixture.ui, &fixture.state, &sender);
            fixture.begin();
            fixture.ui.set_rename_input("changed.txt".into());
            fixture.click(slint::LogicalPosition::new(1100.0, 650.0));
            assert!(
                !fixture.ui.get_rename_editing(),
                "preparation must not retain the editor"
            );
            let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(
                rename_submission::finish(&fixture.state.shared, request.id).is_none(),
                "nonterminal progress cannot consume pending recovery"
            );
            if newer_editor {
                fixture.ui.invoke_begin_rename();
                fixture.editor();
                i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
                fixture.ui.set_rename_input("newer-editor.txt".into());
            }
            let result = crate::fs::file_operations::rename_path(
                &root.join("original.txt"),
                std::ffi::OsStr::new("changed.txt"),
            );
            let (status, error, result) = match result {
                Ok(path) => (
                    ItemState::Succeeded,
                    None,
                    OperationResult {
                        succeeded: vec![path],
                        skipped: vec![],
                        failed: vec![],
                        affected_directories: vec![root.clone()],
                    },
                ),
                Err(error) => {
                    let message = format!("{error:?}");
                    (
                        ItemState::Failed,
                        Some(message.clone()),
                        OperationResult {
                            succeeded: vec![],
                            skipped: vec![],
                            failed: vec![(root.join("original.txt"), message)],
                            affected_directories: vec![],
                        },
                    )
                }
            };
            assert_eq!(error.is_some(), failure);
            let completion = finish_file_operation(
                &fixture.state.shared,
                FileOperationEvent::Finished {
                    id: request.id,
                    result,
                    item_states: vec![(0, status, error)],
                    completed_targets: if failure {
                        vec![]
                    } else {
                        vec![root.join("changed.txt")]
                    },
                    undo_items: None,
                    failed_undo_items: vec![],
                },
            )
            .unwrap();
            let (submitted, error) =
                rename_submission::finish(&fixture.state.shared, request.id).unwrap();
            let (directory, _directory_requests) = mpsc::channel();
            let (network, _network_requests) = mpsc::sync_channel(4);
            refresh_affected_tabs(
                &directory,
                &network,
                &fixture.state.shared,
                &completion.affected,
            );
            refresh_ui(&fixture.ui, &fixture.state);
            rename_submission::present_completion(
                &fixture.ui,
                &fixture.state.shared,
                submitted,
                error,
            );
            assert!(fixture.state.lock().unwrap().submitted_renames.is_empty());
            if newer_editor {
                assert!(fixture.ui.get_rename_editing());
                assert_eq!(fixture.ui.get_rename_input(), "newer-editor.txt");
            } else if failure {
                assert!(fixture.ui.get_rename_editing());
                assert_eq!(fixture.ui.get_rename_input(), "changed.txt");
                fixture.editor();
                i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
                assert!(!fixture.state.lock().unwrap().operation_errors.is_empty());
                fixture.ui.invoke_cancel_rename();
            } else {
                assert!(!fixture.ui.get_rename_editing());
                assert!(!root.join("original.txt").exists());
                assert_eq!(
                    std::fs::read(root.join("changed.txt")).unwrap(),
                    b"original-content"
                );
            }
            if failure {
                assert_eq!(
                    std::fs::read(root.join("original.txt")).unwrap(),
                    b"original-content"
                );
                assert_eq!(
                    std::fs::read(root.join("changed.txt")).unwrap(),
                    b"occupied"
                );
            }
        }
    }
}

#[test]
fn issue_187_failure_follows_moved_tab_but_does_not_reopen_user_navigation() {
    i_slint_backend_testing::init_no_event_loop();
    for moved in [false, true] {
        let fixture = RenameFixture::new(ViewMode::Details, false, "original.txt");
        let (sender, requests) = mpsc::channel();
        rename_submission::wire(&fixture.ui, &fixture.state, &sender);
        fixture.begin();
        fixture.ui.set_rename_input("changed.txt".into());
        fixture.click(slint::LogicalPosition::new(1100.0, 650.0));
        let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        let owner = {
            let mut app = fixture.state.lock().unwrap();
            let task = app.operations.task_mut(request.id).unwrap();
            task.state = OperationState::Failed;
            task.items[0].error = Some("injected failure".into());
            if moved {
                let owner = app.register_window(
                    vec![NavigationLocation::Home],
                    0,
                    test_support::test_window_placement(160),
                );
                let tab = app
                    .window_mut(fixture.state.window_id)
                    .unwrap()
                    .tabs
                    .remove(&TabId(1))
                    .unwrap();
                let destination = app.window_mut(owner).unwrap();
                destination.tabs.insert(TabId(1), tab);
                destination.tab_order.push(TabId(1));
                destination.active_tab = TabId(1);
                app.close_window(fixture.state.window_id).unwrap();
                owner
            } else {
                app.tab_mut(TabId(1)).unwrap().begin_directory_navigation(
                    PathBuf::from(r"C:\another-directory"),
                    NavigationKind::Normal,
                );
                fixture.state.window_id
            }
        };
        let (submission, error) =
            rename_submission::finish(&fixture.state.shared, request.id).unwrap();
        assert_eq!(submission.window_id(), owner);
        refresh_ui(
            &fixture.ui,
            &WindowSessions::new(fixture.state.shared.clone(), owner),
        );
        rename_submission::present_completion(
            &fixture.ui,
            &fixture.state.shared,
            submission,
            error,
        );
        assert_eq!(fixture.ui.get_rename_editing(), moved);
        let app = fixture.state.shared.lock().unwrap();
        assert_eq!(app.operation_errors, ["injected failure"]);
        if moved {
            assert_eq!(fixture.ui.get_rename_input(), "changed.txt");
            assert_eq!(app.rename_targets[&owner].tab.window_id, owner);
        }
    }
}
