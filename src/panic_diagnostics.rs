use crate::operation_audit;

pub(crate) fn install() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let detail = format!("{info}\n{}", std::backtrace::Backtrace::force_capture());
        operation_audit::record("rust-panic", detail);
        // A fatal UI panic cannot rely on event-loop teardown to drain the audit worker.
        // Filesystem access stays on that worker, even on this fatal path.
        if let Err(error) = operation_audit::flush() {
            eprintln!("panic evidence flush failed: {error}");
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD: &str = "ASTERFILES_ISSUE_188_PANIC_CHILD";
    const MARKER: &str = "issue-188-owned-panic-diagnostic-probe";

    #[test]
    fn issue_188_panic_child() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        install();
        panic!("{MARKER}");
    }

    #[test]
    fn issue_188_fatal_panic_records_message_and_stack_before_process_exit() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "panic_diagnostics::tests::issue_188_panic_child",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        assert_eq!(child.wait().unwrap().code(), Some(101));
        let log = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/artifacts/logs/file-operation-audit.jsonl"
        ))
        .unwrap();
        let record = log
            .lines()
            .find(|line| {
                line.contains("\"event\":\"rust-panic\"")
                    && line.contains(&format!("\"pid\":{pid},"))
                    && line.contains(MARKER)
            })
            .expect("child panic evidence is persisted before exit");
        assert!(record.contains("panic_diagnostics.rs:"));
        assert!(
            record.contains("issue_188_panic_child"),
            "forced backtrace includes the crashing frame"
        );
    }
}
