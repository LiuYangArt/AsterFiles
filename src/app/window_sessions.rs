//! Window/tab lifecycle rules operate on the app-owned session registry under its existing lock.
//! Shared state remains private in app; this module does not create or control native windows.
use super::{
    AppState, DetachedTabOutcome, DetachedTabRestart, TAB_DRAG_THRESHOLD, TabDragPhase,
    TabDragSession, WindowCloseAction, WindowCloseDecision, WindowId, WindowState,
    cancel_folder_sizes,
};
use crate::{
    domain::{LoadState, NavigationLocation, PageSource, SearchState, TabId, TabKind, TabSession},
    session_store,
};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
};
impl AppState {
    pub(super) fn active_window_state(&self) -> &WindowState {
        self.windows
            .get(&self.active_window)
            .expect("active window state exists")
    }

    pub(super) fn active_window_state_mut(&mut self) -> &mut WindowState {
        self.windows
            .get_mut(&self.active_window)
            .expect("active window state exists")
    }
    pub(super) fn tab(&self, tab_id: TabId) -> Option<&TabSession> {
        self.windows
            .values()
            .find_map(|window| window.tabs.get(&tab_id))
    }

    pub(super) fn tab_mut(&mut self, tab_id: TabId) -> Option<&mut TabSession> {
        self.windows
            .values_mut()
            .find_map(|window| window.tabs.get_mut(&tab_id))
    }

    pub(super) fn window_for_tab(&self, tab_id: TabId) -> Option<WindowId> {
        self.windows
            .iter()
            .find_map(|(id, window)| window.tabs.contains_key(&tab_id).then_some(*id))
    }
    fn allocate_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_tab_id);
        self.next_tab_id = self
            .next_tab_id
            .checked_add(1)
            .expect("tab identity space is exhausted");
        id
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn register_window(
        &mut self,
        initial_locations: Vec<NavigationLocation>,
        active_index: usize,
        placement: session_store::WindowPlacement,
    ) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id = self
            .next_window_id
            .checked_add(1)
            .expect("window identity space is exhausted");
        let paths = if initial_locations.is_empty() {
            vec![NavigationLocation::Home]
        } else {
            initial_locations
        };
        let mut tabs = HashMap::new();
        let mut tab_order = Vec::new();
        for location in paths {
            let tab_id = self.allocate_tab_id();
            let mut tab = if matches!(location, NavigationLocation::Home) {
                TabSession::new_home(tab_id)
            } else {
                TabSession::new(tab_id)
            };
            tab.current_location = Some(location);
            tabs.insert(tab_id, tab);
            tab_order.push(tab_id);
        }
        let active_tab = tab_order[active_index.min(tab_order.len() - 1)];
        self.windows.insert(
            id,
            WindowState {
                tabs,
                tab_order,
                active_tab,
                closed_tabs: VecDeque::new(),
                placement,
            },
        );
        id
    }

    pub(super) fn reserve_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id = self
            .next_window_id
            .checked_add(1)
            .expect("window identity space is exhausted");
        id
    }

    pub(super) fn detach_dragged_tab_to_window(
        &mut self,
        destination_window: WindowId,
        placement: session_store::WindowPlacement,
    ) -> Option<DetachedTabOutcome> {
        let drag = self.tab_drag?;
        if !matches!(drag.phase, TabDragPhase::Dragging { .. })
            || self.windows.contains_key(&destination_window)
        {
            return None;
        }
        let source = self.windows.get(&drag.window_id)?;
        let source_placement = source.placement;
        let source_closed_tabs = source.closed_tabs.clone();
        let source_active_tab = source.active_tab;
        if source.tab_order.get(drag.source_index) != Some(&drag.tab_id)
            || source
                .tabs
                .get(&drag.tab_id)
                .is_none_or(|tab| tab.kind != TabKind::Files)
        {
            return None;
        }

        let mut tab = self
            .windows
            .get_mut(&drag.window_id)?
            .tabs
            .remove(&drag.tab_id)?;
        self.windows
            .get_mut(&drag.window_id)?
            .tab_order
            .remove(drag.source_index);
        let restart = detached_tab_restart(&mut tab);
        self.windows.insert(
            destination_window,
            WindowState {
                tabs: HashMap::from([(drag.tab_id, tab)]),
                tab_order: vec![drag.tab_id],
                active_tab: drag.tab_id,
                closed_tabs: VecDeque::new(),
                placement,
            },
        );

        let source_has_tabs = self
            .windows
            .get(&drag.window_id)
            .is_some_and(|window| !window.tab_order.is_empty());
        let source_window_closed = if source_has_tabs {
            let source = self.windows.get_mut(&drag.window_id)?;
            if source.active_tab == drag.tab_id {
                source.active_tab = *source.tab_order.first()?;
            }
            false
        } else {
            self.windows.remove(&drag.window_id);
            true
        };
        self.active_window = destination_window;
        self.tab_drag = None;
        Some(DetachedTabOutcome {
            source_window: drag.window_id,
            destination_window,
            tab_id: drag.tab_id,
            source_index: drag.source_index,
            source_placement,
            source_closed_tabs,
            source_active_tab,
            source_window_closed,
            restart,
        })
    }

    pub(super) fn move_dragged_tab_to_window(
        &mut self,
        destination_window: WindowId,
        insertion_index: usize,
    ) -> Option<DetachedTabOutcome> {
        let drag = self.tab_drag?;
        if drag.window_id == destination_window || !self.windows.contains_key(&destination_window) {
            return None;
        }
        let source = self.windows.get(&drag.window_id)?;
        if source.tab_order.get(drag.source_index) != Some(&drag.tab_id)
            || source
                .tabs
                .get(&drag.tab_id)
                .is_none_or(|tab| tab.kind != TabKind::Files)
        {
            return None;
        }
        let destination = self.windows.get(&destination_window)?;
        let movable_end = destination
            .tab_order
            .iter()
            .position(|id| {
                destination
                    .tabs
                    .get(id)
                    .is_some_and(|tab| tab.kind == TabKind::Settings)
            })
            .unwrap_or(destination.tab_order.len());
        let insertion_index = insertion_index.min(movable_end);

        let source_placement = source.placement;
        let source_closed_tabs = source.closed_tabs.clone();
        let source_active_tab = source.active_tab;
        let mut tab = self
            .windows
            .get_mut(&drag.window_id)?
            .tabs
            .remove(&drag.tab_id)?;
        self.windows
            .get_mut(&drag.window_id)?
            .tab_order
            .remove(drag.source_index);
        let restart = detached_tab_restart(&mut tab);
        let destination = self.windows.get_mut(&destination_window)?;
        destination.tabs.insert(drag.tab_id, tab);
        destination.tab_order.insert(insertion_index, drag.tab_id);
        destination.active_tab = drag.tab_id;

        let source_has_tabs = self
            .windows
            .get(&drag.window_id)
            .is_some_and(|window| !window.tab_order.is_empty());
        let source_window_closed = if source_has_tabs {
            let source = self.windows.get_mut(&drag.window_id)?;
            if source.active_tab == drag.tab_id {
                source.active_tab = *source.tab_order.first()?;
            }
            false
        } else {
            self.windows.remove(&drag.window_id);
            true
        };
        self.active_window = destination_window;
        self.tab_drag = None;
        Some(DetachedTabOutcome {
            source_window: drag.window_id,
            destination_window,
            tab_id: drag.tab_id,
            source_index: drag.source_index,
            source_placement,
            source_closed_tabs,
            source_active_tab,
            source_window_closed,
            restart,
        })
    }

    pub(super) fn window(&self, id: WindowId) -> Option<&WindowState> {
        self.windows.get(&id)
    }

    #[cfg(test)]
    pub(super) fn window_mut(&mut self, id: WindowId) -> Option<&mut WindowState> {
        self.windows.get_mut(&id)
    }

    pub(super) fn close_window(&mut self, id: WindowId) -> Option<WindowCloseDecision> {
        let mut window = self.windows.remove(&id)?;
        for tab in window.tabs.values_mut() {
            cancel_folder_sizes(tab);
            tab.cancel_pending();
        }
        if let Some(mut discovery) = self.network_discovery.remove(&id) {
            discovery.cancel_current();
        }
        self.network_discovery_errors.remove(&id);
        self.icons
            .retain(|(tab_id, _, _), _| !window.tabs.contains_key(tab_id));
        self.focus_after_refresh
            .retain(|tab_id, _| !window.tabs.contains_key(tab_id));
        self.pending_rename_ui.remove(&id);
        self.rename_targets.remove(&id);
        self.pending_shell_creates.remove(&id);
        if self.windows.is_empty() {
            return Some(WindowCloseDecision::ExitApplication);
        }
        if self.active_window == id {
            self.active_window = *self.windows.keys().min_by_key(|id| id.0)?;
        }
        Some(WindowCloseDecision::KeepRunning)
    }

    pub(super) fn request_window_close(&self, id: WindowId) -> WindowCloseAction {
        if !self.windows.contains_key(&id) {
            return WindowCloseAction::Ignore;
        }
        if self.windows.len() > 1 {
            WindowCloseAction::CloseWindow
        } else if self.operations.has_foreground_active_tasks() {
            WindowCloseAction::ConfirmApplicationExit
        } else {
            WindowCloseAction::ExitApplication
        }
    }

    pub(super) fn begin_tab_drag(
        &mut self,
        window_id: WindowId,
        tab_id: TabId,
        source_index: usize,
        press_x: f32,
        press_y: f32,
    ) -> bool {
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        if window.tab_order.get(source_index) != Some(&tab_id)
            || window
                .tabs
                .get(&tab_id)
                .is_none_or(|tab| tab.kind != TabKind::Files)
        {
            return false;
        }
        self.tab_drag = Some(TabDragSession {
            window_id,
            tab_id,
            source_index,
            press_x,
            press_y,
            phase: TabDragPhase::Pressed,
        });
        true
    }

    pub(super) fn update_tab_drag(
        &mut self,
        pointer_x: f32,
        pointer_y: f32,
        strip_x: f32,
        strip_width: f32,
        viewport_x: f32,
        tab_width: f32,
    ) -> Option<usize> {
        let mut drag = self.tab_drag?;
        if matches!(drag.phase, TabDragPhase::Pressed)
            && (pointer_x - drag.press_x).hypot(pointer_y - drag.press_y) < TAB_DRAG_THRESHOLD
        {
            return None;
        }
        let window = self.windows.get(&drag.window_id)?;
        let range = movable_tab_range(window, drag.source_index)?;
        let insertion_index = tab_insertion_slot(
            pointer_x,
            strip_x,
            strip_width,
            viewport_x,
            tab_width,
            range,
        );
        drag.phase = TabDragPhase::Dragging { insertion_index };
        self.tab_drag = Some(drag);
        ((0.0..=46.0).contains(&pointer_y))
            .then_some(insertion_index)
            .flatten()
    }

    pub(super) fn finish_tab_drag(&mut self, valid_release: bool) -> bool {
        let Some(drag) = self.tab_drag.take() else {
            return false;
        };
        let TabDragPhase::Dragging {
            insertion_index: Some(insertion_index),
        } = drag.phase
        else {
            return false;
        };
        if !valid_release {
            return false;
        }
        let Some(window) = self.windows.get_mut(&drag.window_id) else {
            return false;
        };
        if window.tab_order.get(drag.source_index) != Some(&drag.tab_id) {
            return false;
        }
        let moved = window.tab_order.remove(drag.source_index);
        let target = insertion_index.min(window.tab_order.len());
        window.tab_order.insert(target, moved);
        target != drag.source_index
    }

    pub(super) fn drag_source_is_single_file_tab_window(&self, source_window: WindowId) -> bool {
        let Some(drag) = self.tab_drag.filter(|drag| drag.window_id == source_window) else {
            return false;
        };
        self.windows.get(&source_window).is_some_and(|window| {
            window.tab_order.len() == 1
                && window.tab_order[0] == drag.tab_id
                && window
                    .tabs
                    .get(&drag.tab_id)
                    .is_some_and(|tab| tab.kind == TabKind::Files)
        })
    }

    pub(super) fn commit_drag_source_window_move(
        &mut self,
        source_window: WindowId,
        x: i32,
        y: i32,
    ) -> bool {
        if !self.drag_source_is_single_file_tab_window(source_window)
            || !self.tab_drag.is_some_and(|drag| {
                drag.window_id == source_window
                    && matches!(drag.phase, TabDragPhase::Dragging { .. })
            })
        {
            return false;
        }
        let Some(window) = self.windows.get_mut(&source_window) else {
            return false;
        };
        window.placement.x = x;
        window.placement.y = y;
        self.tab_drag = None;
        true
    }

    pub(super) fn cancel_tab_drag(&mut self) -> bool {
        self.tab_drag.take().is_some()
    }

    pub(super) fn cancel_tab_drag_for_window(&mut self, window_id: WindowId) -> bool {
        if self
            .tab_drag
            .is_some_and(|drag| drag.window_id == window_id)
        {
            self.cancel_tab_drag()
        } else {
            false
        }
    }

    pub(super) fn duplicate_active_tab(&mut self) -> Option<TabId> {
        let source_id = self.active_window_state().active_tab;
        let home_page = self.home_pages.get(&source_id).cloned();
        let id = self.allocate_tab_id();
        let tab = {
            let source = self.active_window_state().tabs.get(&source_id)?;
            if source.kind != TabKind::Files || source.load_state != LoadState::Complete {
                return None;
            }
            TabSession::duplicate_complete(id, source)
        };
        let window = self.active_window_state_mut();
        window.tabs.insert(id, tab);
        window.tab_order.push(id);
        window.active_tab = id;
        if let Some(home_page) = home_page {
            self.home_pages.insert(id, home_page);
        }
        Some(id)
    }
    pub(super) fn create_tab(&mut self, location: impl Into<NavigationLocation>) -> TabId {
        self.create_tab_in_window(self.active_window, location)
            .expect("active window must exist")
    }

    pub(super) fn create_tab_in_window(
        &mut self,
        window_id: WindowId,
        location: impl Into<NavigationLocation>,
    ) -> Option<TabId> {
        let id = self.allocate_tab_id();
        let location = location.into();
        let mut tab = if matches!(location, NavigationLocation::Home) {
            TabSession::new_home(id)
        } else {
            TabSession::new(id)
        };
        tab.current_location = Some(location);
        let window = self.windows.get_mut(&window_id)?;
        window.tabs.insert(id, tab);
        window.tab_order.push(id);
        window.active_tab = id;
        Some(id)
    }

    pub(super) fn open_settings(&mut self) -> TabId {
        if let Some(id) = self
            .active_window_state()
            .tab_order
            .iter()
            .copied()
            .find(|id| {
                self.active_window_state()
                    .tabs
                    .get(id)
                    .is_some_and(|tab| tab.kind == TabKind::Settings)
            })
        {
            self.active_window_state_mut().active_tab = id;
            return id;
        }
        let id = self.allocate_tab_id();
        let window = self.active_window_state_mut();
        window.tabs.insert(id, TabSession::new_settings(id));
        window.tab_order.push(id);
        window.active_tab = id;
        id
    }

    pub(super) fn close_tab(&mut self, closing: TabId) -> Option<TabId> {
        if self.active_window_state().tab_order.len() == 1 {
            return None;
        }
        let closing_kind = self.active_window_state().tabs.get(&closing)?.kind;
        if closing_kind == TabKind::Files
            && self
                .active_window_state()
                .tabs
                .values()
                .filter(|tab| tab.kind == TabKind::Files)
                .count()
                == 1
        {
            return None;
        }
        let index = self
            .active_window_state()
            .tab_order
            .iter()
            .position(|id| *id == closing)?;
        let closing_was_active = closing == self.active_window_state().active_tab;
        let removed = self.active_window_state_mut().tabs.remove(&closing);
        if let Some(mut tab) = removed {
            cancel_folder_sizes(&mut tab);
            tab.cancel_pending();
            self.icons.retain(|(tab_id, _, _), _| *tab_id != closing);
            self.ordinary_icon_requests
                .retain(|(tab_id, _, _)| *tab_id != closing);
            if tab.kind == TabKind::Files
                && let Some(path) = tab.current_location.take()
            {
                let window = self.active_window_state_mut();
                window.closed_tabs.push_front(path);
                window.closed_tabs.truncate(10);
            }
        }
        self.focus_after_refresh.remove(&closing);
        self.home_pages.remove(&closing);
        self.pending_shell_creates
            .retain(|_, pending| pending.tab_id != closing);
        self.pending_rename_ui
            .retain(|_, pending| pending.tab_id != closing);
        self.rename_targets
            .retain(|_, target| target.tab.tab_id != closing);
        self.active_window_state_mut().tab_order.remove(index);
        if closing_was_active {
            let window = self.active_window_state_mut();
            window.active_tab = window.tab_order[index.min(window.tab_order.len() - 1)];
        }
        Some(self.active_window_state().active_tab)
    }

    pub(super) fn restore_closed(&mut self) -> Option<(TabId, NavigationLocation)> {
        let location = self.active_window_state_mut().closed_tabs.pop_front()?;
        let tab_id = self.create_tab(location.clone());
        Some((tab_id, location))
    }

    pub(super) fn active(&self) -> &TabSession {
        self.active_window_state().active()
    }

    pub(super) fn stable_locations(&self) -> Vec<NavigationLocation> {
        self.active_window_state().stable_locations()
    }

    #[cfg(test)]
    pub(super) fn stable_active_location_index(&self) -> usize {
        self.active_window_state().stable_active_location_index()
    }
}
impl WindowState {
    pub(super) fn active(&self) -> &TabSession {
        self.tabs
            .get(&self.active_tab)
            .expect("active tab session exists")
    }

    pub(super) fn stable_locations(&self) -> Vec<NavigationLocation> {
        self.tab_order
            .iter()
            .filter_map(|id| self.tabs.get(id))
            .filter_map(|tab| tab.current_location.clone())
            .collect()
    }

    pub(super) fn stable_active_location_index(&self) -> usize {
        let mut file_index = 0;
        for id in &self.tab_order {
            let Some(tab) = self.tabs.get(id) else {
                continue;
            };
            if tab.kind != TabKind::Files {
                continue;
            }
            if *id == self.active_tab {
                return file_index;
            }
            file_index += 1;
        }
        file_index.saturating_sub(1)
    }
}

fn movable_tab_range(window: &WindowState, source_index: usize) -> Option<std::ops::Range<usize>> {
    window.tab_order.get(source_index)?;
    let start = window.tab_order[..source_index]
        .iter()
        .rposition(|id| {
            window
                .tabs
                .get(id)
                .is_some_and(|tab| tab.kind == TabKind::Settings)
        })
        .map_or(0, |index| index + 1);
    let end = window.tab_order[source_index + 1..]
        .iter()
        .position(|id| {
            window
                .tabs
                .get(id)
                .is_some_and(|tab| tab.kind == TabKind::Settings)
        })
        .map_or(window.tab_order.len(), |index| source_index + 1 + index);
    Some(start..end)
}

fn tab_insertion_slot(
    pointer_x: f32,
    strip_x: f32,
    strip_width: f32,
    viewport_x: f32,
    tab_width: f32,
    range: std::ops::Range<usize>,
) -> Option<usize> {
    if pointer_x < strip_x || pointer_x > strip_x + strip_width || range.is_empty() {
        return None;
    }
    let pitch = tab_width + 5.0;
    let range_x = pointer_x - strip_x - viewport_x - range.start as f32 * pitch;
    let remaining = range.len().saturating_sub(1);
    if range_x <= tab_width / 2.0 {
        return Some(range.start);
    }
    for slot in 1..remaining {
        let midpoint = slot as f32 * pitch + tab_width / 2.0;
        if range_x < midpoint {
            return Some(range.start + slot);
        }
    }
    Some(range.start + remaining)
}

pub(super) fn external_tab_insertion_slot(
    pointer_x: f32,
    strip_x: f32,
    strip_width: f32,
    viewport_x: f32,
    tab_width: f32,
    tab_count: usize,
) -> Option<usize> {
    if pointer_x < strip_x || pointer_x > strip_x + strip_width {
        return None;
    }
    if tab_count == 0 {
        return Some(0);
    }
    let pitch = tab_width + 5.0;
    let content_x = pointer_x - strip_x - viewport_x;
    for index in 0..tab_count {
        if content_x < index as f32 * pitch + tab_width / 2.0 {
            return Some(index);
        }
    }
    Some(tab_count)
}

fn detached_tab_restart(tab: &mut TabSession) -> Option<DetachedTabRestart> {
    match tab.page_source {
        PageSource::Search
            if matches!(
                tab.search_state,
                SearchState::Searching | SearchState::Partial
            ) =>
        {
            let restart = DetachedTabRestart::Search {
                scope: tab.search_scope.clone(),
                depth: tab.search_depth,
                query: tab.search_query.clone(),
            };
            tab.cancel_pending();
            Some(restart)
        }
        _ if matches!(tab.load_state, LoadState::Loading | LoadState::Partial) => {
            let path = tab.visible_path().map(Path::to_path_buf);
            tab.cancel_pending();
            path.map(DetachedTabRestart::Directory)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{begin_drag_at, test_window_placement};
    use crate::app::*;
    #[test]
    fn external_tabs_are_created_in_the_most_recent_window() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("first")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let first_tab = app.active_window_state().active_tab;
        let second_window = app.register_window(
            vec![NavigationLocation::Directory(PathBuf::from("second"))],
            0,
            test_window_placement(160),
        );
        app.active_window = second_window;

        let external = app.create_tab(NavigationLocation::Directory(PathBuf::from("external")));

        assert_eq!(app.window(first_window).unwrap().tab_order, [first_tab]);
        assert_eq!(app.window(second_window).unwrap().active_tab, external);
        assert_eq!(app.window(second_window).unwrap().tab_order.len(), 2);
    }
    #[test]
    fn window_registry_allocates_global_tab_ids_without_reuse() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let first_extra = app.create_tab(PathBuf::from("two"));
        let second_window = app.register_window(
            vec![NavigationLocation::Directory(PathBuf::from("three"))],
            0,
            test_window_placement(160),
        );
        let second_first = app.window(second_window).unwrap().active_tab;

        assert_eq!(
            app.window(first_window).unwrap().tab_order,
            [TabId(1), first_extra]
        );
        assert_eq!(second_first, TabId(3));
        app.active_window = second_window;
        assert_eq!(app.close_tab(second_first), None);
        let second_extra = app.create_tab(PathBuf::from("four"));
        assert_eq!(second_extra, TabId(4));
    }
    #[test]
    fn window_state_keeps_tabs_history_and_placement_isolated() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let closed = app.create_tab(PathBuf::from("closed"));
        app.close_tab(closed).unwrap();
        let second_window = app.register_window(
            vec![
                NavigationLocation::Directory(PathBuf::from("second")),
                NavigationLocation::Directory(PathBuf::from("active")),
            ],
            1,
            test_window_placement(240),
        );

        let first = app.window(first_window).unwrap();
        let second = app.window(second_window).unwrap();
        assert_eq!(first.closed_tabs, [PathBuf::from("closed")]);
        assert!(second.closed_tabs.is_empty());
        assert_eq!(first.active_tab, TabId(1));
        assert_eq!(second.active_tab, TabId(4));
        assert_eq!(first.placement.x, 80);
        assert_eq!(second.placement.x, 240);
    }
    #[test]
    fn closing_one_window_cancels_only_its_tabs_and_keeps_shared_operation() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let second_window = app.register_window(
            vec![NavigationLocation::Directory(PathBuf::from("two"))],
            0,
            test_window_placement(160),
        );
        let first_tab = app.window(first_window).unwrap().active_tab;
        let second_tab = app.window(second_window).unwrap().active_tab;
        let (_, first_cancel) = app
            .window_mut(first_window)
            .unwrap()
            .tabs
            .get_mut(&first_tab)
            .unwrap()
            .begin_directory_navigation(PathBuf::from("one/new"), NavigationKind::Normal);
        let (_, second_cancel) = app
            .window_mut(second_window)
            .unwrap()
            .tabs
            .get_mut(&second_tab)
            .unwrap()
            .begin_directory_navigation(PathBuf::from("two/new"), NavigationKind::Normal);
        let operation = app.operations.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            Some(first_tab),
            vec![OperationItem::pending(
                Some(PathBuf::from("source")),
                Some(PathBuf::from("target")),
            )],
        );

        assert_eq!(
            app.close_window(first_window),
            Some(WindowCloseDecision::KeepRunning)
        );
        assert!(first_cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(!second_cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(app.window(second_window).is_some());
        assert!(app.operations.task(operation).is_some());
        assert_eq!(
            app.close_window(second_window),
            Some(WindowCloseDecision::ExitApplication)
        );
    }
    #[test]
    fn closing_window_cancels_and_removes_network_discovery() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first = app.active_window;
        let second = app.register_window(
            vec![NavigationLocation::Directory(PathBuf::from("two"))],
            0,
            test_window_placement(160),
        );
        let (request, cancel) = app.network_discovery.entry(first).or_default().begin();
        assert_eq!(request, DiscoveryRequestId(1));
        app.network_discovery_errors
            .insert(first, "temporary".to_owned());

        assert_eq!(
            app.close_window(first),
            Some(WindowCloseDecision::KeepRunning)
        );
        assert!(cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(!app.network_discovery.contains_key(&first));
        assert!(!app.network_discovery_errors.contains_key(&first));
        assert!(app.window(second).is_some());
    }
    #[test]
    fn close_decision_only_confirms_exit_for_last_window_with_active_tasks() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let operation = app.operations.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![OperationItem::pending(
                Some(PathBuf::from("source")),
                Some(PathBuf::from("target")),
            )],
        );
        app.operations.start_next(OperationResource::Local).unwrap();
        app.operations.mark_running(operation).unwrap();
        assert_eq!(
            app.request_window_close(first_window),
            WindowCloseAction::ConfirmApplicationExit
        );
        let second_window = app.register_window(
            vec![NavigationLocation::Directory(PathBuf::from("two"))],
            0,
            test_window_placement(160),
        );
        assert_eq!(
            app.request_window_close(first_window),
            WindowCloseAction::CloseWindow
        );
        app.close_window(first_window).unwrap();
        assert_eq!(
            app.request_window_close(second_window),
            WindowCloseAction::ConfirmApplicationExit
        );
        assert!(app.operations.task(operation).is_some());
    }
    #[test]
    fn last_window_without_tasks_keeps_state_until_session_snapshot_is_read() {
        let app = AppState::new_for_test(
            vec![PathBuf::from("one"), PathBuf::from("two")],
            1,
            [0, 1, 2, 3],
        );
        let window = app.active_window;

        assert_eq!(
            app.request_window_close(window),
            WindowCloseAction::ExitApplication
        );
        assert_eq!(
            app.stable_locations(),
            [PathBuf::from("one"), PathBuf::from("two")]
        );
        assert_eq!(app.stable_active_location_index(), 1);
        assert!(app.window(window).is_some());
    }

    #[test]
    fn tab_drag_threshold_and_invalid_release_preserve_order() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
            0,
            [0, 1, 2, 3],
        );
        let original = app.active_window_state().tab_order.clone();
        begin_drag_at(&mut app, 1);
        assert_eq!(
            app.update_tab_drag(104.0, 20.0, 47.0, 540.0, 0.0, 178.0),
            None
        );
        assert!(!app.finish_tab_drag(true));
        assert_eq!(app.active_window_state().tab_order, original);

        begin_drag_at(&mut app, 1);
        assert_eq!(
            app.update_tab_drag(500.0, 20.0, 47.0, 540.0, 0.0, 178.0),
            Some(2)
        );
        assert!(!app.finish_tab_drag(false));
        assert_eq!(app.active_window_state().tab_order, original);
    }
    #[test]
    fn single_file_tab_window_can_begin_drag() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let window = app.active_window;
        let tab = app.active_window_state().active_tab;

        assert!(app.begin_tab_drag(window, tab, 0, 100.0, 20.0));
        assert!(app.drag_source_is_single_file_tab_window(window));
        assert_eq!(app.active_window_state().tab_order, [tab]);
    }
    #[test]
    fn tab_drag_reorders_only_ids_and_preserves_request_identity() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
            1,
            [0, 1, 2, 3],
        );
        let active = app.active_window_state().active_tab;
        let tab = app.tab_mut(active).unwrap();
        let (request_id, cancel) =
            tab.begin_directory_navigation(PathBuf::from("pending"), NavigationKind::Refresh);
        let (_, moved) = begin_drag_at(&mut app, 0);
        assert_eq!(
            app.update_tab_drag(500.0, 20.0, 47.0, 540.0, 0.0, 178.0),
            Some(2)
        );
        assert!(app.finish_tab_drag(true));
        assert_eq!(
            app.active_window_state().tab_order,
            [TabId(2), TabId(3), moved]
        );
        assert_eq!(app.active_window_state().active_tab, active);
        assert_eq!(app.tab(active).unwrap().latest_request, request_id);
        assert!(!cancel.load(std::sync::atomic::Ordering::Acquire));
    }
    #[test]
    fn detached_tab_commits_only_after_destination_is_reserved() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let source_window = app.active_window;
        let source_order = app.active_window_state().tab_order.clone();
        let (_, tab_id) = begin_drag_at(&mut app, 0);
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);

        assert!(
            app.detach_dragged_tab_to_window(source_window, test_window_placement(160))
                .is_none()
        );
        assert_eq!(app.window(source_window).unwrap().tab_order, source_order);
        assert_eq!(app.window_for_tab(tab_id), Some(source_window));

        let destination = app.reserve_window_id();
        let outcome = app
            .detach_dragged_tab_to_window(destination, test_window_placement(160))
            .unwrap();
        assert_eq!(outcome.tab_id, tab_id);
        assert_eq!(app.window_for_tab(tab_id), Some(destination));
        assert_eq!(app.window(destination).unwrap().tab_order, [tab_id]);
        assert_eq!(app.window(source_window).unwrap().tab_order, [TabId(2)]);
    }
    #[test]
    fn detaching_pending_tab_cancels_old_request_and_requires_restart() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let tab_id = app.active_window_state().active_tab;
        let (_, token) = app
            .tab_mut(tab_id)
            .unwrap()
            .begin_directory_navigation(PathBuf::from("pending"), NavigationKind::Refresh);
        begin_drag_at(&mut app, 0);
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
        let destination = app.reserve_window_id();
        let outcome = app
            .detach_dragged_tab_to_window(destination, test_window_placement(160))
            .unwrap();

        assert!(token.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            outcome.restart,
            Some(DetachedTabRestart::Directory(PathBuf::from("pending")))
        );
        assert_eq!(app.window_for_tab(tab_id), Some(destination));
    }
    #[test]
    fn single_tab_cross_window_move_closes_source_and_keeps_identity() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let source = app.active_window;
        let destination = app.register_window(
            vec![
                NavigationLocation::Directory(PathBuf::from("c")),
                NavigationLocation::Directory(PathBuf::from("d")),
            ],
            0,
            test_window_placement(160),
        );
        let tab_id = app.window(source).unwrap().active_tab;
        let request = app.tab(tab_id).unwrap().latest_request;
        assert!(app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);

        let outcome = app.move_dragged_tab_to_window(destination, 1).unwrap();

        assert!(outcome.source_window_closed);
        assert!(app.window(source).is_none());
        assert_eq!(app.window_for_tab(tab_id), Some(destination));
        assert_eq!(
            app.window(destination).unwrap().tab_order,
            [TabId(2), tab_id, TabId(3)]
        );
        assert_eq!(app.tab(tab_id).unwrap().latest_request, request);
    }
    #[test]
    fn cross_window_move_inserts_at_target_and_keeps_single_owner() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let destination = app.register_window(
            vec![
                NavigationLocation::Directory(PathBuf::from("c")),
                NavigationLocation::Directory(PathBuf::from("d")),
            ],
            0,
            test_window_placement(160),
        );
        let tab_id = app.window(source).unwrap().tab_order[0];
        let request = app.tab(tab_id).unwrap().latest_request;
        assert!(app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);

        let outcome = app.move_dragged_tab_to_window(destination, 1).unwrap();

        assert_eq!(app.window(source).unwrap().tab_order, [TabId(2)]);
        assert_eq!(
            app.window(destination).unwrap().tab_order,
            [TabId(3), tab_id, TabId(4)]
        );
        assert_eq!(app.window_for_tab(tab_id), Some(destination));
        assert_eq!(app.window(destination).unwrap().active_tab, tab_id);
        assert_eq!(app.tab(tab_id).unwrap().latest_request, request);
        assert!(!outcome.source_window_closed);
    }
    #[test]
    fn cross_window_move_failure_and_cancel_leave_source_untouched() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let tab_id = app.window(source).unwrap().active_tab;
        app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0);
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
        assert!(app.move_dragged_tab_to_window(WindowId(999), 0).is_none());
        assert!(app.cancel_tab_drag());
        assert_eq!(app.window_for_tab(tab_id), Some(source));
        assert_eq!(app.window(source).unwrap().tab_order, [tab_id, TabId(2)]);
    }
    #[test]
    fn closing_target_window_does_not_cancel_source_drag() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("c")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let target = app.register_window(
            vec![NavigationLocation::Directory(PathBuf::from("b"))],
            0,
            test_window_placement(160),
        );
        let tab_id = app.window(source).unwrap().active_tab;
        assert!(app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        assert!(
            app.update_tab_drag(108.0, 20.0, 47.0, 540.0, 0.0, 178.0)
                .is_some()
        );

        assert!(!app.cancel_tab_drag_for_window(target));
        assert!(app.tab_drag.is_some_and(|drag| drag.window_id == source));
        assert_eq!(
            app.close_window(target),
            Some(WindowCloseDecision::KeepRunning)
        );
        assert!(app.tab_drag.is_some());
        assert_eq!(app.window_for_tab(tab_id), Some(source));
    }
    #[test]
    fn closing_source_window_cancels_its_drag() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("c")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let target = app.register_window(
            vec![NavigationLocation::Directory(PathBuf::from("b"))],
            0,
            test_window_placement(160),
        );
        let tab_id = app.window(source).unwrap().active_tab;
        assert!(app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        assert!(app.cancel_tab_drag_for_window(source));
        assert!(app.tab_drag.is_none());
        assert_eq!(app.window_for_tab(tab_id), Some(source));
        assert!(app.window(target).is_some());
    }
    #[test]
    fn settings_window_stays_open_when_its_file_tab_is_detached() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let source = app.active_window;
        let file_tab = app.active_window_state().active_tab;
        let settings = app.open_settings();
        assert!(app.begin_tab_drag(source, file_tab, 0, 100.0, 20.0));
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
        let destination = app.reserve_window_id();
        let outcome = app
            .detach_dragged_tab_to_window(destination, test_window_placement(160))
            .unwrap();

        assert!(!outcome.source_window_closed);
        assert_eq!(app.window(source).unwrap().tab_order, [settings]);
        assert_eq!(app.window(source).unwrap().active_tab, settings);
    }
    #[test]
    fn settings_tab_is_fixed_and_bounds_the_file_tab_range() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let settings = app.open_settings();
        let third = app.create_tab(PathBuf::from("c"));
        assert!(!app.begin_tab_drag(app.active_window, settings, 2, 100.0, 20.0));
        assert!(app.begin_tab_drag(app.active_window, third, 3, 600.0, 20.0));
        assert_eq!(
            app.update_tab_drag(50.0, 20.0, 47.0, 720.0, 0.0, 178.0),
            Some(3)
        );
        assert!(!app.finish_tab_drag(true));
        assert_eq!(
            app.active_window_state().tab_order,
            [TabId(1), TabId(2), settings, third]
        );
    }
    #[test]
    fn tab_drag_slot_accounts_for_overflow_viewport_offset() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
            0,
            [0, 1, 2, 3],
        );
        begin_drag_at(&mut app, 2);
        assert_eq!(
            app.update_tab_drag(52.0, 20.0, 47.0, 240.0, -100.0, 80.0),
            Some(1)
        );
        app.cancel_tab_drag();
        assert_eq!(
            app.active_window_state().tab_order,
            [TabId(1), TabId(2), TabId(3)]
        );
    }
    #[test]
    fn tab_insertion_slot_uses_midpoints_gaps_ranges_and_bounds() {
        assert_eq!(tab_insertion_slot(46.0, 47.0, 300.0, 0.0, 80.0, 0..3), None);
        assert_eq!(
            tab_insertion_slot(48.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(0)
        );
        assert_eq!(
            tab_insertion_slot(87.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(0)
        );
        assert_eq!(
            tab_insertion_slot(88.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(1)
        );
        assert_eq!(
            tab_insertion_slot(130.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(1)
        );
        assert_eq!(
            tab_insertion_slot(340.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(2)
        );
        assert_eq!(
            tab_insertion_slot(348.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            None
        );
        assert_eq!(
            tab_insertion_slot(52.0, 47.0, 240.0, -100.0, 80.0, 0..3),
            Some(1)
        );
        assert_eq!(
            tab_insertion_slot(305.0, 47.0, 400.0, 0.0, 80.0, 3..5),
            Some(3)
        );
        assert_eq!(
            tab_insertion_slot(390.0, 47.0, 400.0, 0.0, 80.0, 3..5),
            Some(4)
        );
    }
    #[test]
    fn complete_tab_duplication_shares_entries_without_reloading() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("same")], 0, [0, 1, 2, 3]);
        let source = app
            .active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap();
        source.latest_request = RequestId(7);
        source.load_state = LoadState::Complete;
        source.replace_entries(vec![FileEntry {
            id: EntryId(1),
            original_name: "file.txt".into(),
            display_name: "file.txt".into(),
            name_highlights: Vec::new(),
            path: PathBuf::from("same/file.txt"),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            library_source_index: None,
            parent_display: "same".into(),
            size_bytes: Some(1),
            folder_size: crate::domain::FolderSizeState::Unknown,
            modified: None,
            created: None,
        }]);
        let source_entries = source.entries.clone();

        let duplicate = app.duplicate_active_tab().expect("complete tab duplicates");
        let duplicated = app.active_window_state().tabs.get(&duplicate).unwrap();

        assert!(Arc::ptr_eq(&source_entries, &duplicated.entries));
        assert_eq!(duplicated.latest_request, RequestId(7));
        assert_eq!(duplicated.load_state, LoadState::Complete);
    }
    #[test]
    fn closing_a_tab_preserves_another_session() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let second = app.create_tab(PathBuf::from("two"));
        assert_eq!(app.active_window_state().active_tab, second);
        assert_eq!(app.close_tab(TabId(2)), Some(TabId(1)));
        assert_eq!(
            app.active().current_location,
            Some(NavigationLocation::Directory(PathBuf::from("one")))
        );
    }
    #[test]
    fn closing_an_inactive_tab_keeps_the_active_tab() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let second = app.create_tab(PathBuf::from("two"));
        let third = app.create_tab(PathBuf::from("three"));
        assert_eq!(app.active_window_state().active_tab, third);

        assert_eq!(app.close_tab(second), Some(third));
        assert_eq!(app.active_window_state().active_tab, third);
        assert_eq!(
            app.active().current_location,
            Some(NavigationLocation::Directory(PathBuf::from("three")))
        );
        assert!(!app.active_window_state().tabs.contains_key(&second));
    }
    #[test]
    fn settings_tab_is_singleton_and_does_not_restore() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);

        let settings = app.open_settings();
        assert_eq!(app.open_settings(), settings);
        assert_eq!(app.active_window_state().tab_order.len(), 2);
        assert_eq!(app.active().kind, TabKind::Settings);

        assert_eq!(app.close_tab(settings), Some(TabId(1)));
        assert!(app.active_window_state().closed_tabs.is_empty());
        assert!(app.restore_closed().is_none());
    }
    #[test]
    fn settings_tab_is_excluded_from_saved_paths_and_active_index() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("one"), PathBuf::from("two")],
            0,
            [0, 1, 2, 3],
        );

        app.open_settings();
        assert_eq!(
            app.stable_locations(),
            [PathBuf::from("one"), PathBuf::from("two")]
        );
        assert_eq!(app.stable_active_location_index(), 1);
    }
    #[test]
    fn last_file_tab_cannot_be_closed_while_settings_is_open() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        app.open_settings();

        assert_eq!(app.close_tab(TabId(1)), None);
        assert!(app.active_window_state().tabs.contains_key(&TabId(1)));
    }
}
