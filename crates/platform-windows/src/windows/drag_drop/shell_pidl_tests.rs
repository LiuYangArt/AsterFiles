use super::*;
use std::collections::BTreeMap;

#[derive(Default)]
struct Probe {
    failure: Option<(&'static str, usize)>,
    steps: Vec<&'static str>,
    allocated: Vec<usize>,
    released: Vec<usize>,
}

thread_local! {
    static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) };
}

struct ProbeGuard;

impl ProbeGuard {
    fn start(failure: Option<(&'static str, usize)>) -> Self {
        PROBE.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Probe {
                failure,
                ..Probe::default()
            });
        });
        Self
    }

    fn finish(self) -> Probe {
        PROBE.with(|slot| slot.borrow_mut().take().unwrap())
    }
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        PROBE.with(|slot| *slot.borrow_mut() = None);
    }
}

fn should_fail(step: &'static str) -> bool {
    PROBE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(probe) = slot.as_mut() else {
            return false;
        };
        probe.steps.push(step);
        probe.failure
            == Some((
                step,
                probe.steps.iter().filter(|seen| **seen == step).count(),
            ))
    })
}

pub(super) fn checkpoint(step: &'static str) -> io::Result<()> {
    if should_fail(step) {
        Err(io::Error::other(format!("injected Shell {step} failure")))
    } else {
        Ok(())
    }
}

pub(super) fn clone_pidl(pidl: *mut ITEMIDLIST) -> *mut ITEMIDLIST {
    if should_fail("clone") {
        ptr::null_mut()
    } else {
        // SAFETY: The production caller retains the parsed absolute PIDL during cloning.
        unsafe { ILClone(pidl) }
    }
}

pub(super) fn remove_last_id(pidl: *mut ITEMIDLIST) -> bool {
    if should_fail("remove") {
        false
    } else {
        // SAFETY: The production caller retains this nonnull, uniquely owned clone.
        unsafe { ILRemoveLastID(Some(pidl)) }.as_bool()
    }
}

pub(super) fn allocated(pidl: *mut ITEMIDLIST) {
    PROBE.with(|slot| {
        if let Some(probe) = slot.borrow_mut().as_mut() {
            assert!(!pidl.is_null());
            probe.allocated.push(pidl as usize);
        }
    });
}

pub(super) fn released(pidl: *mut ITEMIDLIST) {
    PROBE.with(|slot| {
        if let Some(probe) = slot.borrow_mut().as_mut() {
            probe.released.push(pidl as usize);
        }
    });
}

fn assert_balanced(probe: &Probe, expected: usize) {
    let counts = |items: &[usize]| {
        let mut counts = BTreeMap::new();
        for pointer in items {
            *counts.entry(*pointer).or_insert(0usize) += 1;
        }
        counts
    };
    assert_eq!(probe.allocated.len(), expected);
    assert_eq!(counts(&probe.allocated), counts(&probe.released));
    assert!(counts(&probe.released).values().all(|count| *count == 1));
    eprintln!(
        "#181 failure={:?}, allocated={}, freed={}, steps={:?}",
        probe.failure,
        probe.allocated.len(),
        probe.released.len(),
        probe.steps
    );
}

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "asterfiles-issue-181-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(path.join("nested")).unwrap();
        for relative in ["a.txt", "b.txt", "nested/c.txt"] {
            std::fs::write(path.join(relative), b"PIDL lifetime test").unwrap();
        }
        Self(path)
    }

    fn paths(&self, mixed_parents: bool) -> Vec<PathBuf> {
        vec![
            self.0.join("a.txt"),
            if mixed_parents {
                self.0.join("nested").join("c.txt")
            } else {
                self.0.join("b.txt")
            },
        ]
    }

    fn supplemental(paths: &[PathBuf]) -> IDataObject {
        IDataObject::from(OutboundDataObject {
            formats: vec![(CF_HDROP, encode_dropfiles(paths).unwrap())],
            dynamic_formats: Mutex::new(Vec::new()),
            performed_format: 0,
            performed_effect: Arc::new(Mutex::new(None)),
            accept_extra_set_data: false,
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn issue_181_shell_pidl_failure_paths_release_every_allocation_once() {
    let _ole = OleApartment::initialize().unwrap();
    let fixture = Fixture::new();
    let paths = fixture.paths(false);
    let supplemental = Fixture::supplemental(&paths);
    for (stage, occurrence, allocated) in [
        ("parse", 1, 0),
        ("parse", 2, 2),
        ("clone", 1, 1),
        ("clone", 2, 3),
        ("remove", 1, 2),
        ("remove", 2, 4),
        ("create", 1, 4),
    ] {
        let guard = ProbeGuard::start(Some((stage, occurrence)));
        assert!(shell_data_object(&paths, &supplemental).is_err());
        assert_balanced(&guard.finish(), allocated);
    }
    let mixed = fixture.paths(true);
    let supplemental = Fixture::supplemental(&mixed);
    let guard = ProbeGuard::start(Some(("create", 1)));
    assert!(shell_data_object(&mixed, &supplemental).is_err());
    assert_balanced(&guard.finish(), 5);
}

#[test]
fn issue_181_shell_pidl_success_preserves_single_common_and_desktop_parents() {
    let _ole = OleApartment::initialize().unwrap();
    let fixture = Fixture::new();
    for (mut paths, single, expected) in [
        (fixture.paths(false), true, 2),
        (fixture.paths(false), false, 4),
        (fixture.paths(true), false, 5),
    ] {
        if single {
            paths.truncate(1);
        }
        let supplemental = Fixture::supplemental(&paths);
        let guard = ProbeGuard::start(None);
        let data = shell_data_object(&paths, &supplemental).unwrap();
        assert_balanced(&guard.finish(), expected);
        assert_eq!(read_drop_paths(&data).unwrap(), paths);
        assert_shell_id_list_paths(&data, &paths);
        for format in [CF_HDROP, clipboard_format("Shell IDList Array").unwrap()] {
            // SAFETY: This test retains the live data object and initialized format on its OLE thread.
            let available = unsafe { data.QueryGetData(&format_etc(format)) };
            assert_eq!(available, HRESULT(0));
        }
    }
}

fn assert_shell_id_list_paths(data: &IDataObject, paths: &[PathBuf]) {
    use windows::Win32::{System::Com::CoTaskMemFree, UI::Shell::ILCombine};

    let format = format_etc(clipboard_format("Shell IDList Array").unwrap());
    // SAFETY: The initialized OLE test retains data through the synchronous request.
    let mut medium = unsafe { data.GetData(&format) }.unwrap();
    let bytes = read_hglobal_bytes(&medium);
    // SAFETY: read_hglobal_bytes completed its owned copy before this single release.
    unsafe { ReleaseStgMedium(&mut medium) };
    let bytes = bytes.unwrap();
    let read_u32 =
        |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
    assert_eq!(read_u32(0), paths.len());
    let header_size = 4 * (paths.len() + 2);
    let pidl_at = |index: usize| {
        let offset = read_u32(4 * (index + 1));
        assert!(offset >= header_size);
        let mut cursor = offset;
        loop {
            let item_size =
                u16::from_le_bytes(bytes[cursor..cursor + 2].try_into().unwrap()) as usize;
            if item_size == 0 {
                break;
            }
            assert!(item_size >= 2);
            cursor += item_size;
        }
        bytes.as_ptr().wrapping_add(offset).cast::<ITEMIDLIST>()
    };
    let parent = pidl_at(0);
    for (index, path) in paths.iter().enumerate() {
        let child = pidl_at(index + 1);
        // SAFETY: Both PIDLs were bounds-checked through their terminators in the retained CIDA allocation.
        let combined = unsafe { ILCombine(Some(parent), Some(child)) };
        assert!(!combined.is_null());
        let wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut expected = ptr::null_mut();
        // SAFETY: This test retains the terminated original path and writable output during parsing.
        let parsed =
            unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut expected, 0, None) };
        // SAFETY: combined is owned; successful parsing produces an owned absolute PIDL for comparison.
        let equal = parsed.is_ok() && unsafe { ILIsEqual(combined, expected) }.as_bool();
        // SAFETY: Comparison is complete; these are distinct owned Shell allocations.
        unsafe {
            CoTaskMemFree(Some(combined.cast()));
            CoTaskMemFree(Some(expected.cast()));
        }
        assert!(
            equal,
            "Shell IDList Array changed source identity: {path:?}"
        );
    }
}
