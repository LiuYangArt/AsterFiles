use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    thread,
};

use slint::{Model, ModelRc, VecModel};

use crate::{
    domain::FileEntry,
    fs::{DirectoryLoad, load_directory},
};

slint::include_modules!();

pub fn run() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let initial_path = initial_path();
    let model = Rc::new(VecModel::<FileRow>::default());
    ui.set_files(ModelRc::from(model.clone()));

    let (sender, receiver) = mpsc::channel::<PathBuf>();
    spawn_directory_worker(receiver, ui.as_weak(), model);
    wire_callbacks(&ui, sender.clone());

    ui.set_current_path(initial_path.to_string_lossy().into_owned().into());
    ui.set_status_text("Loading…".into());
    let _ = sender.send(initial_path);

    ui.run()
}

fn wire_callbacks(ui: &AppWindow, sender: mpsc::Sender<PathBuf>) {
    let weak = ui.as_weak();
    ui.on_navigate(move |path| {
        let path = PathBuf::from(path.as_str());
        if path.is_dir() {
            if let Some(ui) = weak.upgrade() {
                ui.set_current_path(path.to_string_lossy().into_owned().into());
                ui.set_status_text("Loading…".into());
            }
            let _ = sender.send(path);
        } else if let Some(ui) = weak.upgrade() {
            ui.set_status_text("Folder not found".into());
        }
    });
}

fn spawn_directory_worker(
    receiver: mpsc::Receiver<PathBuf>,
    ui: slint::Weak<AppWindow>,
    _model: Rc<VecModel<FileRow>>,
) {
    thread::spawn(move || {
        while let Ok(mut requested_path) = receiver.recv() {
            while let Ok(newer_path) = receiver.try_recv() {
                requested_path = newer_path;
            }

            let result = load_directory(&requested_path);
            let ui = ui.clone();
            let display_path = requested_path.to_string_lossy().into_owned();

            ui.upgrade_in_event_loop(move |ui| match result {
                Ok(load) => apply_directory(&ui, &display_path, load),
                Err(error) => ui.set_status_text(format!("Unable to open: {error}").into()),
            })
            .ok();
        }
    });
}

fn apply_directory(ui: &AppWindow, display_path: &str, load: DirectoryLoad) {
    let count = load.entries.len();
    let model = ui.get_files();
    let model = model
        .as_any()
        .downcast_ref::<VecModel<FileRow>>()
        .expect("file model is initialized as VecModel");
    model.set_vec(load.entries.into_iter().map(file_row).collect::<Vec<_>>());
    ui.set_current_path(display_path.into());
    ui.set_status_text(
        if load.skipped == 0 {
            format!("{count} items")
        } else {
            format!("{count} items · {} skipped", load.skipped)
        }
        .into(),
    );
}

fn file_row(entry: FileEntry) -> FileRow {
    let is_directory = entry.kind == crate::domain::EntryKind::Directory;
    FileRow {
        name: entry.name.into(),
        path: entry.path.to_string_lossy().into_owned().into(),
        kind: entry.kind.label().into(),
        size: format_size(entry.size_bytes).into(),
        modified: entry.modified.into(),
        is_directory,
    }
}

fn format_size(value: Option<u64>) -> String {
    let Some(bytes) = value else {
        return String::new();
    };
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn initial_path() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| Path::new("C:\\").to_path_buf())
}
