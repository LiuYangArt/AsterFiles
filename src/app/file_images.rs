//! Shared icon resources and viewport-sized image updates (#175).
use super::*;
use platform::windows::shell_icons::{ShellIconRgba, ShellTypeIconKey};

type UiImages = HashMap<usize, (std::sync::Weak<[u8]>, Image)>;

thread_local! {
    static IMAGES: RefCell<UiImages> = RefCell::new(HashMap::new());
    #[cfg(test)]
    static CONVERSIONS: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn image(source: &ShellIconRgba) -> Image {
    let key = source.pixels.as_ptr() as usize;
    IMAGES.with_borrow_mut(|images| {
        if let Some((pixels, image)) = images.get(&key)
            && pixels
                .upgrade()
                .is_some_and(|pixels| Arc::ptr_eq(&pixels, &source.pixels))
            && image.size().width == source.width
            && image.size().height == source.height
        {
            return image.clone();
        }
        images.retain(|_, (pixels, _)| pixels.strong_count() > 0);
        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
            source.pixels.as_ref(),
            source.width,
            source.height,
        );
        let image = Image::from_rgba8(buffer);
        images.insert(key, (Arc::downgrade(&source.pixels), image.clone()));
        #[cfg(test)]
        CONVERSIONS.set(CONVERSIONS.get() + 1);
        image
    })
}

pub(super) fn shared_icon_request(
    app: &mut AppState,
    tab_id: TabId,
    request_id: RequestId,
    entry: &FileEntry,
    size: u32,
) -> Option<IconRequest> {
    let type_key = entry_type_icon_key(&entry.path, entry.kind);
    let path = if let Some(key) = &type_key {
        let identity = (key.clone(), size);
        if app.type_icon_cache.contains_key(&identity) || !app.type_icon_requests.insert(identity) {
            return None;
        }
        key.query_path()
    } else {
        if app.icon_cache.contains_key(&entry.path)
            || !app
                .ordinary_icon_requests
                .insert((tab_id, request_id, entry.path.clone()))
        {
            return None;
        }
        entry.path.clone()
    };
    Some(IconRequest {
        tab_id,
        request_id,
        target: IconTarget::Entry(entry.id),
        path,
        thumbnail: false,
        requested_px: size,
        type_key,
    })
}

pub(super) fn load_shared_icon(
    state: &SharedSessions,
    key: &ShellTypeIconKey,
    size: u32,
    cached: Option<ShellIconRgba>,
) -> Option<ShellIconRgba> {
    if size > 0
        && state
            .lock()
            .is_ok_and(|app| !app.thumbnail_types.contains_key(key))
    {
        let supported = platform::windows::shell_icons::supports_content_thumbnail(key)
            .unwrap_or_else(|error| {
                // A failed association query must not disable a working preview provider.
                eprintln!("thumbnail association {key:?}: {error}");
                true
            });
        if let Ok(mut app) = state.lock() {
            app.thumbnail_types.insert(key.clone(), supported);
        }
    }
    cached.or_else(|| {
        let result = if size == 0 {
            platform::windows::shell_icons::shell_type_icon_rgba(key)
        } else {
            platform::windows::shell_icons::shell_type_icon_rgba_at_size(key, size)
        };
        match result {
            Ok(icon) => {
                crate::operation_audit::record(
                    "shared_type_icon_extracted",
                    format!("type={key:?} requested_px={size}"),
                );
                Some(icon)
            }
            Err(error) => {
                eprintln!("shared icon {key:?} size={size}: {error}");
                None
            }
        }
    })
}

pub(super) fn row_image(
    entry: &FileEntry,
    tab: &TabSession,
    app: &AppState,
    grid_requested_px: Option<u32>,
) -> (Image, i32) {
    let grid_image = grid_requested_px
        .and_then(|requested_px| app.thumbnail_cache.get(&(entry.path.clone(), requested_px)));
    let grid_icon = grid_requested_px
        .and_then(|requested_px| app.grid_icon_cache.get(&(entry.path.clone(), requested_px)));
    let shared_icon = entry_type_icon_key(&entry.path, entry.kind).and_then(|key| {
        let size = if crate::network::is_unc_path(&entry.path) {
            0
        } else {
            grid_requested_px.unwrap_or(0)
        };
        app.type_icon_cache.get(&(key, size))
    });
    let image = grid_image.or(grid_icon).or(shared_icon).or_else(|| {
        if grid_requested_px.is_some() {
            return None;
        }
        app.icons
            .get(&(tab.id, tab.latest_request(), entry.id))
            .or_else(|| app.icon_cache.get(&entry.path))
    });
    let state = grid_requested_px.map_or(0, |requested_px| {
        if grid_image.is_some() {
            2
        } else if grid_icon.is_some() || shared_icon.is_some() {
            4
        } else if app.thumbnail_failures.contains_key(&(
            tab.id,
            tab.latest_request(),
            entry.path.clone(),
            requested_px,
        )) {
            3
        } else {
            1
        }
    });
    (image.map(self::image).unwrap_or_default(), state)
}

/// Change only image fields, retaining selection, editors and row geometry.
pub(super) fn patch_images(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    ids: &HashSet<EntryId>,
) -> usize {
    if ids.is_empty() {
        return 0;
    }
    let app = state.lock().expect("app state mutex is not poisoned");
    let Some(window_id) = app.window_for_tab(tab_id) else {
        return 0;
    };
    let Some(window) = app.window(window_id) else {
        return 0;
    };
    let Some(tab) = window.tabs.get(&tab_id) else {
        return 0;
    };
    if window.active_tab != tab_id
        || ui.get_projected_file_tab_id() != tab_id.0 as i32
        || ui.get_projected_file_request_id() != tab.latest_request().0 as i32
    {
        return 0;
    }
    let Some(mode) = app.view_mode_for_tab(tab_id) else {
        return 0;
    };
    let size = file_thumbnail_request_px(mode, ui.window().scale_factor());
    let updates = ids
        .iter()
        .filter_map(|id| tab.visible_entry(*id))
        .map(|entry| (entry.id, row_image(entry, tab, &app, size)))
        .collect::<HashMap<_, _>>();
    let mut changed = 0;
    let files = ui.get_files();
    if let Some(rows) = files.as_any().downcast_ref::<VecModel<FileRow>>() {
        for (id, index) in file_row_index::locate(rows, tab, updates.keys().copied()) {
            if let Some(mut row) = rows.row_data(index) {
                let next = &updates[&id];
                if row.icon != next.0 || row.thumbnail_state != next.1 {
                    row.icon = next.0.clone();
                    row.thumbnail_state = next.1;
                    rows.set_row_data(index, row);
                    changed += 1;
                }
            }
        }
    }
    let grid = ui.get_grid_rows();
    for (id, next) in updates {
        let position = GRID_ENTRY_POSITIONS.with_borrow(|windows| {
            windows
                .get(&window_id)
                .and_then(|positions| positions.get(&id).copied())
        });
        let Some((row_index, entry_index)) = position else {
            continue;
        };
        let Some(row) = grid.row_data(row_index) else {
            continue;
        };
        let Some(entries) = row.entries.as_any().downcast_ref::<VecModel<FileRow>>() else {
            continue;
        };
        if let Some(mut entry) = entries.row_data(entry_index)
            && entry.id == id.0 as i32
            && (entry.icon != next.0 || entry.thumbnail_state != next.1)
        {
            entry.icon = next.0;
            entry.thumbnail_state = next.1;
            entries.set_row_data(entry_index, entry);
            changed += 1;
        }
    }
    changed
}

/// Shared resources notify only current viewports, including another window awaiting that type.
pub(super) fn present_batch(
    fallback: &AppWindow,
    state: &SharedSessions,
    scheduler: &ThumbnailScheduler,
    events: Vec<IconEvent>,
) -> usize {
    let mut changed = 0;
    let mut types = HashSet::new();
    let mut entries: HashMap<TabId, HashSet<EntryId>> = HashMap::new();
    let mut locations = HashSet::new();
    let mut retries = HashSet::new();
    for event in events {
        if event
            .thumbnail_plan
            .as_ref()
            .is_some_and(|(_, key)| !scheduler.accepts(key))
        {
            continue;
        }
        let retry = event.thumbnail_failed;
        if let Some(update) = apply_icon_event(state, event) {
            if retry {
                retries.insert(update.tab_id);
            }
            if let Some(key) = update.type_key {
                types.insert(key);
            } else if let Some(id) = update.entry_id {
                entries.entry(update.tab_id).or_default().insert(id);
            } else {
                locations.insert(update.tab_id);
            }
        }
    }
    let (windows, active_window) = {
        let app = state.lock().expect("app state mutex is not poisoned");
        (
            app.windows
                .iter()
                .map(|(id, window)| (*id, window.active_tab))
                .collect::<Vec<_>>(),
            app.active_window,
        )
    };
    for (window_id, tab_id) in windows {
        let target = window_ui(window_id);
        let ui = match target.as_ref() {
            Some(ui) => ui,
            None if window_id == active_window => fallback,
            None => continue,
        };
        let mut ids = entries.remove(&tab_id).unwrap_or_default();
        if !types.is_empty() {
            let app = state.lock().expect("app state mutex is not poisoned");
            if let Some(tab) = app.tab(tab_id) {
                ids.extend(
                    resident_ids(ui, &app, window_id, tab)
                        .into_iter()
                        .filter(|id| {
                            tab.visible_entry(*id)
                                .and_then(|entry| entry_type_icon_key(&entry.path, entry.kind))
                                .is_some_and(|key| types.contains(&key))
                        }),
                );
            }
        }
        changed += patch_images(ui, state, tab_id, &ids);
        if locations.contains(&tab_id) {
            update_icon_row(
                ui,
                state,
                IconUpdate {
                    tab_id,
                    entry_id: None,
                    type_key: None,
                },
            );
        }
        if !types.is_empty() {
            request_visible_file_images(ui, state, window_id, scheduler);
        }
    }
    for tab_id in retries {
        defer_thumbnail_retry(state.clone(), tab_id, scheduler.clone());
    }
    changed
}

pub(super) fn resident_ids(
    ui: &AppWindow,
    app: &AppState,
    window_id: WindowId,
    tab: &TabSession,
) -> Vec<EntryId> {
    GRID_THUMBNAIL_RESIDENTS
        .with_borrow(|windows| {
            windows
                .get(&window_id)
                .map(|ids| ids.iter().copied().collect())
        })
        .unwrap_or_else(|| visible_shortcut_entry_ids(ui, app, tab))
}

#[cfg(test)]
pub(super) fn conversion_count() -> usize {
    CONVERSIONS.get()
}
