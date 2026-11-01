//! Real directory entry allocation evidence (#170), without opening the UI.
use asterfiles_domain::{FileEntry, FileVisibility};
use asterfiles_platform_windows::windows::network;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicIsize, Ordering},
};
struct Allocator;
static BYTES: AtomicIsize = AtomicIsize::new(0);
static BLOCKS: AtomicIsize = AtomicIsize::new(0);
// SAFETY: All operations forward unchanged contracts to System. Counters do not allocate.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: The caller supplies a valid layout.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            BYTES.fetch_add(layout.size() as isize, Ordering::Relaxed);
            BLOCKS.fetch_add(1, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        // SAFETY: The caller supplies the live pointer and matching layout.
        unsafe { System.dealloc(p, layout) };
        BYTES.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        BLOCKS.fetch_sub(1, Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: The caller supplies the live pointer, matching layout and valid new size.
        let q = unsafe { System.realloc(p, layout, size) };
        if !q.is_null() {
            BYTES.fetch_add(size as isize - layout.size() as isize, Ordering::Relaxed);
        }
        q
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
fn snapshot() -> (isize, isize) {
    (
        BYTES.load(Ordering::Relaxed),
        BLOCKS.load(Ordering::Relaxed),
    )
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if network::try_run_child_from_args()? {
        return Ok(());
    }
    let label = std::env::args().nth(1).expect("before or after required");
    assert!(matches!(label.as_str(), "before" | "after"));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("artifacts/perf/issue-170");
    let directory = root.join("fixture");
    std::fs::create_dir_all(&directory)?;
    let count = 100_000;
    for index in 0..count {
        let path = directory.join(format!("file-{index:06}.txt"));
        if !path.exists() {
            std::fs::File::create(path)?;
        }
    }
    let load = || -> std::io::Result<Vec<FileEntry>> {
        let mut entries = Vec::with_capacity(count);
        network::isolated_directory(
            &directory,
            FileVisibility::default(),
            &AtomicBool::new(false),
            |batch| {
                entries.extend(batch);
                Ok(())
            },
        )?;
        assert_eq!(entries.len(), count);
        Ok(entries)
    };
    drop(load()?);
    let baseline = snapshot();
    let started = std::time::Instant::now();
    let entries = load()?;
    let load_ms = started.elapsed().as_millis();
    let retained = snapshot();
    let parent_display_bytes: usize = entries.iter().map(|e| e.parent_display.capacity()).sum();
    let clone_baseline = snapshot();
    let started = std::time::Instant::now();
    let cloned = entries.clone();
    let clone_ms = started.elapsed().as_millis();
    let clone_retained = snapshot();
    drop(cloned);
    drop(entries);
    assert_eq!(snapshot(), baseline, "entry storage must be fully released");
    let report = format!(
        concat!(
            "{{\n  \"issue\":170, \"label\":\"{}\", \"entry_count\":{},\n",
            "  \"method\":\"live System allocator requested bytes after real warmed isolated enumeration; excludes allocator bookkeeping and child process\",\n",
            "  \"entry_size_bytes\":{}, \"retained_bytes\":{}, \"retained_allocations\":{},\n",
            "  \"parent_display_capacity_bytes\":{}, \"load_ms\":{}, \"clone_ms\":{},\n",
            "  \"clone_bytes\":{}, \"clone_allocations\":{}, \"fully_released\":true\n}}\n"
        ),
        label,
        count,
        std::mem::size_of::<FileEntry>(),
        retained.0 - baseline.0,
        retained.1 - baseline.1,
        parent_display_bytes,
        load_ms,
        clone_ms,
        clone_retained.0 - clone_baseline.0,
        clone_retained.1 - clone_baseline.1
    );
    std::fs::write(root.join(format!("{label}.json")), &report)?;
    println!("{report}");
    Ok(())
}
