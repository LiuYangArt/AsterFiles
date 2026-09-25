use super::*;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

struct Fixture {
    root: PathBuf,
    junctions: Vec<PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        // Use the repository volume for deterministic junction creation and cleanup.
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("artifacts");
        fs::create_dir_all(&base).unwrap();
        let root = base.join(format!(
            "asterfiles-issue-102-{}-{}",
            std::process::id(),
            UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self {
            root,
            junctions: Vec::new(),
        }
    }

    fn directory(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn junction(&mut self, name: &str, target: &Path) -> PathBuf {
        let link = self.root.join(name);
        assert!(link.starts_with(&self.root));
        assert!(target.starts_with(&self.root));
        self.junctions.push(link.clone());
        let output = std::process::Command::new("pwsh")
            .args([
                "-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:ASTERFILES_TEST_LINK -Target $env:ASTERFILES_TEST_TARGET | Out-Null",
            ])
            .env("ASTERFILES_TEST_LINK", &link)
            .env("ASTERFILES_TEST_TARGET", target)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction creation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        link
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Remove only the junction entries before recursively removing the controlled fixture.
        for path in self.junctions.iter().rev() {
            if fs::symlink_metadata(path).is_ok() && fs::remove_dir(path).is_err() {
                return;
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn operation(
    moving: bool,
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
) -> Result<FileOperationReport, OperationError> {
    let mut conflict = |_, _: &Path, _: &Path| ConflictAction::Replace;
    let mut discovered = |_, _: &Path| {};
    let mut progress = |_, _, _: &Path| {};
    if moving {
        move_path_with_progress(
            source,
            destination,
            cancel,
            &mut conflict,
            &mut discovered,
            &mut progress,
        )
    } else {
        copy_path_with_progress(
            source,
            destination,
            cancel,
            &mut conflict,
            &mut discovered,
            &mut progress,
            &mut |_| {},
        )
    }
}

fn assert_rejected(source: &Path, destination: &Path) {
    assert_eq!(
        reject_destination_inside_source(source, destination, &CancellationToken::new()),
        Err(OperationError::SourceInsideDestination),
        "source: {source:?}, destination: {destination:?}"
    );
}

#[test]
fn issue_102_preflight_rejects_junction_descendants_without_copying() {
    let mut fixture = Fixture::new();
    let source = fixture.directory("source");
    let nested = fixture.directory("source/deep/nested");
    let alias = fixture.junction("outside-alias", &nested);
    let destination = alias.join("not-created").join("deeper");
    assert_rejected(&source, &destination);
    assert!(!nested.join("not-created").exists());
    assert_eq!(fs::read_dir(&nested).unwrap().count(), 0);
}

#[test]
fn issue_102_preflight_checks_real_parents_of_source_and_target_aliases() {
    let mut fixture = Fixture::new();
    let parent = fixture.directory("parent");
    let source = fixture.directory("parent/source");
    let nested = fixture.directory("parent/source/child/deeper");
    let source_parent_alias = fixture.junction("source-parent-alias", &parent);
    let source_alias = source_parent_alias.join("source");
    let target_alias = fixture.junction("target-alias", &nested);
    assert_rejected(&source_alias, &target_alias.join("new"));
    assert_rejected(&source, &source_alias);
    assert_rejected(&source_alias, &source);
}

#[test]
fn issue_102_preflight_covers_case_direct_deep_and_uncreated_paths() {
    let fixture = Fixture::new();
    let source = fixture.directory("MiXeD-Source");
    let deep = fixture.directory("MiXeD-Source/child/deep");
    assert_rejected(&source, &source.join("new"));
    assert_rejected(&source, &deep);
    assert_rejected(&source, &deep.join("new/further"));
    let case_alias = fixture.root.join("mixed-source");
    assert_rejected(&source, &case_alias);
    assert_rejected(&source, &case_alias.join("child/new"));
    let outside = fixture.directory("MiXeD-Source-external");
    assert!(
        reject_destination_inside_source(
            &source,
            &outside.join("new/further"),
            &CancellationToken::new()
        )
        .is_ok()
    );
}

#[test]
fn issue_102_copy_and_move_reject_alias_descendants_before_writes() {
    for moving in [false, true] {
        let mut fixture = Fixture::new();
        let source = fixture.directory("source");
        fs::write(source.join("payload.txt"), b"unchanged").unwrap();
        let nested = fixture.directory("source/nested");
        let alias = fixture.junction("alias", &nested);
        let destination = alias.join("new");
        // The guard must reject before exercising a production operation that could recurse.
        assert_rejected(&source, &destination);
        assert_eq!(
            operation(moving, &source, &destination, &CancellationToken::new()),
            Err(OperationError::SourceInsideDestination)
        );
        assert_eq!(fs::read(source.join("payload.txt")).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(&nested).unwrap().count(), 0);
    }
}

#[test]
fn issue_102_copy_and_move_reject_same_source_alias() {
    for moving in [false, true] {
        let mut fixture = Fixture::new();
        let source = fixture.directory("source");
        fs::write(source.join("payload.txt"), b"unchanged").unwrap();
        let alias = fixture.junction("alias", &source);
        assert_rejected(&source, &alias);
        assert_eq!(
            operation(moving, &source, &alias, &CancellationToken::new()),
            Err(OperationError::SourceInsideDestination)
        );
        assert_eq!(fs::read(source.join("payload.txt")).unwrap(), b"unchanged");
    }
}

#[test]
fn issue_102_existing_external_target_with_nested_junction_is_rejected() {
    for moving in [false, true] {
        let mut fixture = Fixture::new();
        let source = fixture.directory("source");
        let child = fixture.directory("source/child");
        fs::write(child.join("payload.txt"), b"unchanged").unwrap();
        let destination = fixture.directory("outside");
        let nested_alias = fixture.junction("outside/child", &child);
        assert_rejected(&child, &nested_alias);
        let result = operation(moving, &source, &destination, &CancellationToken::new());
        assert!(
            result.is_err(),
            "nested self-alias was accepted: {result:?}"
        );
        assert_eq!(fs::read(child.join("payload.txt")).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(&child).unwrap().count(), 1);
    }
}

#[test]
fn issue_102_nested_target_cannot_redirect_into_original_source_root() {
    for moving in [false, true] {
        let mut fixture = Fixture::new();
        let source = fixture.directory("source");
        let child = fixture.directory("source/child");
        fs::write(child.join("payload.txt"), b"child payload").unwrap();
        fs::write(source.join("payload.txt"), b"root payload").unwrap();
        let destination = fixture.directory("outside");
        let nested_alias = fixture.junction("outside/child", &source);
        assert_rejected(&source, &nested_alias);
        let result = operation(moving, &source, &destination, &CancellationToken::new());
        assert!(
            result.is_err(),
            "target redirected into source root: {result:?}"
        );
        assert_eq!(
            fs::read(child.join("payload.txt")).unwrap(),
            b"child payload"
        );
        let root_payload = fs::read(source.join("payload.txt"))
            .or_else(|_| fs::read(destination.join("payload.txt")))
            .unwrap();
        assert_eq!(root_payload, b"root payload");
    }
}
#[test]
fn issue_102_normal_external_junction_target_allows_copy_and_move() {
    for moving in [false, true] {
        let mut fixture = Fixture::new();
        let source = fixture.directory("source");
        fs::write(source.join("payload.txt"), b"payload").unwrap();
        let outside = fixture.directory("outside");
        let alias = fixture.junction("alias", &outside);
        let destination = alias.join("copied");
        let result = operation(moving, &source, &destination, &CancellationToken::new());
        assert!(
            result.is_ok(),
            "moving={moving}, result={result:?}, target={:?}, actual={:?}",
            fs::symlink_metadata(&destination),
            fs::read_dir(&outside).map(|entries| entries
                .map(|entry| entry.map(|e| e.path()))
                .collect::<Vec<_>>())
        );
        assert_eq!(
            fs::read(outside.join("copied/payload.txt")).unwrap(),
            b"payload"
        );
        assert_eq!(source.exists(), !moving);
    }
}

#[test]
fn issue_102_cancellation_precedes_identity_queries_and_mutations() {
    let fixture = Fixture::new();
    let source = fixture.directory("source");
    fs::write(source.join("payload.txt"), b"unchanged").unwrap();
    let destination = fixture.root.join("destination");
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        reject_destination_inside_source(&fixture.root.join("missing"), &destination, &cancel),
        Err(OperationError::Cancelled)
    );
    for moving in [false, true] {
        assert_eq!(
            operation(moving, &source, &destination, &cancel),
            Err(OperationError::Cancelled)
        );
        assert_eq!(
            operation(moving, &source, &source, &cancel),
            Err(OperationError::Cancelled)
        );
    }
    assert_eq!(fs::read(source.join("payload.txt")).unwrap(), b"unchanged");
    assert!(!destination.exists());
    assert!(!fixture.root.join("source (2)").exists());
}
