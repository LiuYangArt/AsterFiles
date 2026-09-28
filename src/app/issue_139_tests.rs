use super::*;
use crate::domain::file_operations::OperationResult;

fn submit_running_transfer(app: &mut AppState, kind: FileOperationKind) -> OperationId {
    let id = app.operations.submit(
        OperationResource::Network,
        kind,
        None,
        vec![OperationItem::pending(
            Some(PathBuf::from(r"\\server\share\files")),
            Some(PathBuf::from("C:/test/files")),
        )],
    );
    app.operations
        .start_next(OperationResource::Network)
        .unwrap();
    app.operations.mark_running(id).unwrap();
    id
}

fn complete_transfer_with_skips(
    app: &mut AppState,
    kind: FileOperationKind,
    copied: usize,
) -> OperationId {
    let id = submit_running_transfer(app, kind);
    let task = app.operations.task_mut(id).unwrap();
    task.progress.completed_items = 1;
    task.progress.completed_files = copied;
    task.progress.skipped_files = 14;
    task.progress.total_files = Some(copied + 14);
    task.progress.processed_bytes = copied as u64 * 1_048_576;
    task.progress.skipped_bytes = 14 * 1_048_576;
    task.progress.total_bytes = Some((copied + 14) as u64 * 1_048_576);
    task.progress.scanning_complete = true;
    app.operations
        .finish(
            id,
            OperationState::Completed,
            OperationResult {
                succeeded: if copied > 0 {
                    vec![PathBuf::from("C:/test/files")]
                } else {
                    vec![]
                },
                skipped: vec![PathBuf::from(r"\\server\share\files\existing")],
                failed: vec![],
                affected_directories: vec![],
            },
        )
        .unwrap();
    id
}

#[test]
fn issue_139_mixed_and_all_skipped_transfers_clear_completed_rows_and_close_window() {
    for kind in [FileOperationKind::Copy, FileOperationKind::Move] {
        for copied in [0, 1] {
            let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
            let id = complete_transfer_with_skips(&mut app, kind, copied);
            assert!(operation_rows(&app).is_empty());
            assert!(pending_operation_window_notifications(&app, &mut HashSet::new()).is_empty());
            assert_eq!(
                app.operations
                    .prune_transient_with_retained_cleanup(Duration::ZERO, Duration::from_secs(10)),
                1
            );
            assert!(app.operations.task(id).is_none());
            assert!(should_close_operation_window(&app));
        }
    }
}

#[test]
fn issue_139_completed_skip_does_not_close_other_live_or_failed_tasks() {
    for state in [
        OperationState::Queued,
        OperationState::Preflight,
        OperationState::Running,
        OperationState::Paused,
        OperationState::WaitingConflict,
        OperationState::Cancelling,
        OperationState::Failed,
        OperationState::PartiallyCompleted,
    ] {
        let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
        let completed = complete_transfer_with_skips(&mut app, FileOperationKind::Copy, 1);
        let other = submit_running_transfer(&mut app, FileOperationKind::Copy);
        app.operations.task_mut(other).unwrap().state = state;
        assert_eq!(app.operations.prune_transient(Duration::ZERO), 1);
        assert!(app.operations.task(completed).is_none());
        assert_eq!(operation_rows(&app).len(), 1);
        assert!(!should_close_operation_window(&app), "{state:?}");
    }
}

#[test]
fn issue_139_window_close_matches_cleanup_row_visibility() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let cleanup = complete_transfer_with_skips(&mut app, FileOperationKind::Copy, 1);
    let task = app.operations.task_mut(cleanup).unwrap();
    task.resource = OperationResource::Cleanup;
    task.kind = FileOperationKind::PermanentDelete;
    assert_eq!(
        app.operations
            .prune_transient_with_retained_cleanup(Duration::ZERO, Duration::from_secs(10)),
        0
    );
    assert_eq!(operation_rows(&app).len(), 1);
    assert!(!should_close_operation_window(&app));
    app.operations.task_mut(cleanup).unwrap().finished_at =
        Some(Instant::now() - Duration::from_secs(11));
    assert_eq!(
        app.operations
            .prune_transient_with_retained_cleanup(Duration::ZERO, Duration::from_secs(10)),
        1
    );
    assert!(should_close_operation_window(&app));

    let recovered = submit_running_transfer(&mut app, FileOperationKind::PermanentDelete);
    let task = app.operations.task_mut(recovered).unwrap();
    task.resource = OperationResource::Cleanup;
    task.recovered_cleanup = true;
    assert!(operation_rows(&app).is_empty());
    assert!(should_close_operation_window(&app));
    app.operations.task_mut(recovered).unwrap().state = OperationState::Failed;
    assert_eq!(operation_rows(&app).len(), 1);
    assert!(!should_close_operation_window(&app));
}

#[test]
fn issue_139_active_progress_includes_skips_and_eta_excludes_skipped_bytes() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let id = submit_running_transfer(&mut app, FileOperationKind::Copy);
    let task = app.operations.task_mut(id).unwrap();
    task.progress.completed_files = 5;
    task.progress.skipped_files = 14;
    task.progress.total_files = Some(20);
    task.progress.processed_bytes = 5 * 1_048_576;
    task.progress.skipped_bytes = 14 * 1_048_576;
    task.progress.total_bytes = Some(20 * 1_048_576);
    task.progress.scanning_complete = true;
    task.progress.recent_speed_bps = Some(1_048_576);

    let row = operation_rows(&app).remove(0);
    assert_eq!(row.progress, 0.95);
    assert_eq!(row.speed.as_str(), "1.0 MB/s");
    assert_eq!(
        row.eta.as_str(),
        format_remaining_time(Language::Chinese, 1.0)
    );
}
