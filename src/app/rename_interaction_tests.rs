use super::*;
use i_slint_backend_testing::{ElementHandle, ElementRoot};
use slint::platform::{Key, PointerEventButton, WindowEvent};

struct RenameFixture {
    ui: AppWindow,
    state: WindowSessions,
    commits: Rc<RefCell<Vec<String>>>,
    cancels: Rc<Cell<usize>>,
    activations: Rc<RefCell<Vec<(i32, bool)>>>,
    mode: ViewMode,
    grouped: bool,
}

impl RenameFixture {
    fn new(mode: ViewMode, grouped: bool, name: &str) -> Self {
        Self::at(mode, grouped, name, PathBuf::from(r"C:\issue-187-memory"))
    }

    fn at(mode: ViewMode, grouped: bool, name: &str, root: PathBuf) -> Self {
        let mut app = AppState::new_for_test(vec![root.clone()], 0, [0, 1, 2, 3]);
        app.update_directory_preference(root.clone(), |preference| {
            preference.view_mode = mode;
            preference.group_field = if grouped {
                GroupField::Kind
            } else {
                GroupField::None
            };
        });
        let tab_id = app.active().id;
        let tab = app.tab_mut(tab_id).unwrap();
        let (request, _) = tab.begin_directory_navigation(root.clone(), NavigationKind::Normal);
        assert!(tab.accept_directory_batch(
            request,
            vec![FileEntry {
                id: EntryId(1),
                original_name: name.into(),
                display_name: name.into(),
                name_highlights: Vec::new(),
                path: root.join(name).into(),
                kind: crate::domain::EntryKind::File,
                open_target: None,
                library_source_index: None,
                parent_display: String::new(),
                size_bytes: Some(1),
                folder_size: FolderSizeState::NotIndexed,
                modified: None,
                created: None,
                webdav: None,
            }],
            DirectoryViewPreference::default(),
        ));
        assert!(tab.finish_directory_navigation(
            request,
            NavigationLocation::Directory(root),
            DirectoryViewPreference::default(),
            None,
        ));
        tab.select_entry(EntryId(1), false, false);
        let window_id = app.active_window;
        let state = WindowSessions::new(Arc::new(Mutex::new(app)), window_id);
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
        refresh_ui(&ui, &state);
        let commits = Rc::new(RefCell::new(Vec::new()));
        let committed = commits.clone();
        ui.on_commit_rename(move |name| committed.borrow_mut().push(name.to_string()));
        let cancels = Rc::new(Cell::new(0));
        let cancelled = cancels.clone();
        let weak = ui.as_weak();
        ui.on_cancel_rename(move || {
            cancelled.set(cancelled.get() + 1);
            if let Some(ui) = weak.upgrade() {
                ui.set_rename_editing(false);
            }
        });
        let activations = Rc::new(RefCell::new(Vec::new()));
        let activated = activations.clone();
        ui.on_activate_entry(move |id, _, _, name_hit| {
            activated.borrow_mut().push((id, name_hit));
        });
        ui.show().expect("testing backend creates no native window");
        ui.window().request_redraw();
        Self {
            ui,
            state,
            commits,
            cancels,
            activations,
            mode,
            grouped,
        }
    }

    fn begin(&self) {
        begin_rename_ui(&self.ui.as_weak(), &self.state);
        self.ui.window().request_redraw();
        assert!(self.ui.get_rename_editing());
        self.editor();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        assert!(
            self.commits.borrow().is_empty(),
            "opening the editor must not submit"
        );
    }

    fn element(&self, id: &str) -> ElementHandle {
        self.ui
            .root_element()
            .query_descendants()
            .match_id(id)
            .find_all()
            .into_iter()
            .find(|item| item.size().width > 0.0 && item.size().height > 0.0)
            .unwrap_or_else(|| panic!("visible real component {id}"))
    }

    fn editor(&self) -> ElementHandle {
        self.element("InlineRenameEditor::rename-editor")
    }

    fn move_pointer(&self, position: slint::LogicalPosition) {
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerMoved { position });
    }

    fn click(&self, position: slint::LogicalPosition) {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));
        self.move_pointer(position);
        // The native window filter commits before Slint dispatches the original press.
        self.ui
            .invoke_dismiss_rename_outside(position.x, position.y);
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
    }

    fn key(&self, text: impl Into<slint::SharedString>) {
        let text = text.into();
        self.ui
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        self.ui
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text });
    }

    fn assert_editing_without_commit(&self) {
        assert!(
            self.ui.get_rename_editing(),
            "{:?}/{}",
            self.mode,
            self.grouped
        );
        assert!(
            self.commits.borrow().is_empty(),
            "{:?}/{}: {:?}",
            self.mode,
            self.grouped,
            self.commits.borrow()
        );
        assert!(!self.ui.get_rename_submitting());
    }

    fn touch(&self) -> ElementHandle {
        self.element(match (self.mode, self.grouped) {
            (ViewMode::Details | ViewMode::List | ViewMode::Content, _) => "DetailsList::mouse",
            (_, false) => "FileGrid::card-touch",
            (_, true) => "FileGrid::grouped-card-touch",
        })
    }
}

fn layouts() -> [(ViewMode, bool); 5] {
    [
        (ViewMode::Details, false),
        (ViewMode::List, false),
        (ViewMode::Content, false),
        (ViewMode::MediumIcons, false),
        (ViewMode::MediumIcons, true),
    ]
}

#[test]
fn issue_187_editor_click_moves_caret_and_padding_does_not_submit_in_all_layouts() {
    i_slint_backend_testing::init_no_event_loop();
    for (mode, grouped) in layouts() {
        for target in 0..3 {
            let fixture = RenameFixture::new(mode, grouped, "abcdef.txt");
            fixture.begin();
            let editor = fixture.editor();
            let p = editor.absolute_position();
            let size = editor.size();
            let x = match target {
                0 => p.x + 1.0,
                1 if mode.uses_grid_layout() => p.x + size.width / 2.0,
                1 => p.x + 25.0,
                _ => p.x + size.width - 2.0,
            };
            let y = p.y
                + if mode.uses_grid_layout() {
                    7.0
                } else {
                    size.height / 2.0
                };
            fixture.click(slint::LogicalPosition::new(x, y));
            fixture.assert_editing_without_commit();
            fixture.key("X");
            let edited = fixture.ui.get_rename_input().to_string();
            let index = edited.find('X').expect("typing reaches the clicked editor");
            assert_eq!(
                edited.replace('X', ""),
                "abcdef.txt",
                "single click collapses the initial stem selection: {mode:?}/{grouped}/{target}"
            );
            match target {
                0 => assert_eq!(index, 0, "{mode:?}/{grouped}: start"),
                1 => assert!(
                    (1..10).contains(&index),
                    "{mode:?}/{grouped}: middle at {index}"
                ),
                _ => assert_eq!(index, 10, "{mode:?}/{grouped}: end"),
            }
            fixture.assert_editing_without_commit();
            // The padding is outside TextInput but inside InlineRenameEditor.
            fixture.click(slint::LogicalPosition::new(p.x - 3.0, p.y - 2.0));
            fixture.assert_editing_without_commit();
        }
    }
}

#[test]
fn issue_187_drag_selects_text_and_editing_keeps_the_complete_name() {
    i_slint_backend_testing::init_no_event_loop();
    for (mode, grouped) in layouts() {
        let fixture = RenameFixture::new(mode, grouped, "abcdef.txt");
        fixture.begin();
        let editor = fixture.editor();
        let p = editor.absolute_position();
        let size = editor.size();
        let y = p.y
            + if mode.uses_grid_layout() {
                7.0
            } else {
                size.height / 2.0
            };
        let start = slint::LogicalPosition::new(p.x + 1.0, y);
        let end = slint::LogicalPosition::new(p.x + size.width - 2.0, y);
        fixture.move_pointer(start);
        fixture
            .ui
            .window()
            .dispatch_event(WindowEvent::PointerPressed {
                position: start,
                button: PointerEventButton::Left,
            });
        fixture.move_pointer(end);
        fixture
            .ui
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position: end,
                button: PointerEventButton::Left,
            });
        fixture.assert_editing_without_commit();
        fixture.key("replacement.txt");
        assert_eq!(
            fixture.ui.get_rename_input(),
            "replacement.txt",
            "{mode:?}/{grouped}: drag selection replaces the full text"
        );
        fixture.assert_editing_without_commit();
    }
}

#[test]
fn issue_187_outside_and_enter_submit_once_while_escape_only_cancels() {
    i_slint_backend_testing::init_no_event_loop();
    for (mode, grouped) in layouts() {
        for action in 0..3 {
            let fixture = RenameFixture::new(mode, grouped, "abcdef.txt");
            fixture.begin();
            fixture.ui.set_rename_input("changed.txt".into());
            match action {
                0 => {
                    fixture.click(slint::LogicalPosition::new(1100.0, 650.0));
                    fixture.click(slint::LogicalPosition::new(1100.0, 650.0));
                    fixture.key(Key::Return);
                }
                1 => {
                    fixture.key(Key::Return);
                    fixture.key(Key::Return);
                }
                _ => fixture.key(Key::Escape),
            }
            if action == 2 {
                assert_eq!(fixture.cancels.get(), 1, "{mode:?}/{grouped}");
                assert!(fixture.commits.borrow().is_empty());
                assert!(!fixture.ui.get_rename_editing());
            } else {
                assert_eq!(
                    *fixture.commits.borrow(),
                    vec!["changed.txt"],
                    "{mode:?}/{grouped}/{action}"
                );
                assert_eq!(fixture.cancels.get(), 0);
            }
        }
    }
}

#[test]
fn issue_188_real_file_rows_distinguish_name_from_icon_blank_and_other_columns() {
    i_slint_backend_testing::init_no_event_loop();
    for (mode, grouped) in layouts() {
        let fixture = RenameFixture::new(mode, grouped, "abcdef.txt");
        let touch = fixture.touch();
        let p = touch.absolute_position();
        let size = touch.size();
        let name = if mode.uses_grid_layout() {
            fixture.element(if grouped {
                "FileGrid::grouped-file-name"
            } else {
                "FileGrid::file-name"
            })
        } else if mode == ViewMode::Content {
            fixture.element("DetailsList::content-name")
        } else {
            fixture.element("DetailsList::details-name")
        };
        let text = name.absolute_position();
        let text_size = name.size();
        let name_position = slint::LogicalPosition::new(
            text.x
                + if mode.uses_grid_layout() {
                    text_size.width / 2.0
                } else {
                    5.0
                },
            text.y
                + if mode.uses_grid_layout() {
                    7.0
                } else {
                    text_size.height / 2.0
                },
        );
        fixture.click(name_position);
        assert_eq!(
            *fixture.activations.borrow(),
            vec![(1, true)],
            "{mode:?}/{grouped}: name at {name_position:?}"
        );
        fixture.activations.borrow_mut().clear();
        let icon = slint::LogicalPosition::new(
            p.x + if mode.uses_grid_layout() {
                size.width / 2.0
            } else {
                10.0
            },
            p.y + if mode.uses_grid_layout() {
                10.0
            } else {
                size.height / 2.0
            },
        );
        fixture.click(icon);
        assert_eq!(
            *fixture.activations.borrow(),
            vec![(1, false)],
            "{mode:?}/{grouped}: icon"
        );
        fixture.activations.borrow_mut().clear();
        fixture.click(slint::LogicalPosition::new(
            p.x + size.width - 3.0,
            p.y + size.height - 3.0,
        ));
        assert_eq!(
            *fixture.activations.borrow(),
            vec![(1, false)],
            "{mode:?}/{grouped}: blank"
        );
        if mode == ViewMode::Details {
            fixture.activations.borrow_mut().clear();
            fixture.click(slint::LogicalPosition::new(
                p.x + fixture.ui.get_normal_name_width() + 30.0,
                p.y + size.height / 2.0,
            ));
            assert_eq!(
                *fixture.activations.borrow(),
                vec![(1, false)],
                "other details column"
            );
        }
    }
}

#[test]
fn issue_187_long_and_unicode_names_keep_editing_and_escape_discards_changes() {
    i_slint_backend_testing::init_no_event_loop();
    let long_name = format!("{}.txt", "long-name-".repeat(20));
    for (mode, grouped) in layouts() {
        for name in ["中文报告.txt", long_name.as_str()] {
            let fixture = RenameFixture::new(mode, grouped, name);
            fixture.begin();
            // TextInput may be wider than its clip for horizontal caret scrolling.
            let viewport = fixture.element("InlineRenameEditor::editor-viewport");
            let p = viewport.absolute_position();
            let size = viewport.size();
            fixture.click(slint::LogicalPosition::new(
                p.x + size.width / 2.0,
                p.y + 7.0,
            ));
            fixture.assert_editing_without_commit();
            fixture.ui.window().dispatch_event(WindowEvent::KeyPressed {
                text: Key::Control.into(),
            });
            fixture.key(Key::End);
            fixture
                .ui
                .window()
                .dispatch_event(WindowEvent::KeyReleased {
                    text: Key::Control.into(),
                });
            fixture.key("X");
            assert_eq!(fixture.ui.get_rename_input(), format!("{name}X"));
            fixture.assert_editing_without_commit();
            fixture.key(Key::Escape);
            assert_eq!(fixture.cancels.get(), 1);
            assert!(fixture.commits.borrow().is_empty());
            assert!(!fixture.ui.get_rename_editing());
        }
    }
}

#[test]
fn issue_188_delayed_rename_can_move_between_visible_files() {
    i_slint_backend_testing::init_no_event_loop();
    for (mode, grouped) in layouts() {
        let fixture = RenameFixture::new(mode, grouped, "A_LensFlare.as.bak11");
        {
            let mut app = fixture.state.lock().unwrap();
            let mut entries = Vec::new();
            let template = app.active().entries[0].clone();
            for id in 1..=70 {
                let name = match id % 3 {
                    0 => "AGENTS.md",
                    1 => "apply_v132.py",
                    _ => "A_LensFlare.as.bak11",
                };
                let mut entry = template.clone();
                entry.id = EntryId(id);
                entry.original_name = format!("{id:02}-{name}").into();
                entry.display_name = format!("{id:02}-{name}");
                entry.path = PathBuf::from(r"C:\issue-188-memory")
                    .join(&entry.display_name)
                    .into();
                entries.push(entry);
            }
            app.tab_mut(TabId(1)).unwrap().replace_entries(entries);
        }
        refresh_ui(&fixture.ui, &fixture.state);
        let clicks = Rc::new(RefCell::new(rename_click::RenameClickState::default()));
        let timer = Rc::new(slint::Timer::default());
        let cancel_clicks = clicks.clone();
        let cancel_timer = timer.clone();
        fixture.ui.on_cancel_delayed_rename(move |reset| {
            cancel_clicks.borrow_mut().cancel(reset);
            cancel_timer.stop();
        });
        for id in [3, 4, 5, 3, 4, 5] {
            fixture.ui.invoke_cancel_rename();
            let changed = {
                let mut app = fixture.state.lock().unwrap();
                mutate_selection(app.tab_mut(TabId(1)).unwrap(), |tab| {
                    tab.select_entry(EntryId(id), false, false)
                })
            };
            update_file_rows(&fixture.ui, &fixture.state, TabId(1), &changed);
            update_selection_summary(&fixture.ui, &fixture.state);
            clicks.borrow_mut().pending =
                rename_click::RenameClickTarget::current(&fixture.state, EntryId(id));
            rename_click::schedule(
                fixture.ui.as_weak(),
                fixture.state.clone(),
                clicks.clone(),
                &timer,
                Duration::from_millis(500),
            );
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(501));
            fixture.ui.window().request_redraw();
            let editor = fixture.editor();
            assert!(editor.size().height > 0.0);
            assert_eq!(fixture.ui.get_rename_entry_id(), id as i32);
            assert!(!fixture.ui.get_rename_submitting());
        }
    }
}

#[test]
fn issue_188_accessibility_instantiation_does_not_focus_until_next_turn() {
    i_slint_backend_testing::init_no_event_loop();
    for (mode, grouped) in layouts() {
        let fixture = RenameFixture::new(mode, grouped, ".claude.json.backup");
        begin_rename_ui(&fixture.ui.as_weak(), &fixture.state);
        // An accessibility walk materializes the conditional editor without an event turn.
        let editor = fixture.editor();
        assert_eq!(
            editor.accessible_label().as_deref(),
            Some(".claude.json.backup")
        );
        fixture.key("Q");
        assert_eq!(
            fixture.ui.get_rename_input(),
            ".claude.json.backup",
            "tree construction must not acquire focus: {mode:?}/{grouped}"
        );
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        fixture.key("X");
        assert_eq!(
            fixture.ui.get_rename_input(),
            "X.backup",
            "the next turn focuses and selects the filename stem: {mode:?}/{grouped}"
        );
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        fixture.key("Y");
        assert_eq!(
            fixture.ui.get_rename_input(),
            "XY.backup",
            "focus timer must not repeat or reset the selection"
        );
        fixture.assert_editing_without_commit();
    }
}

#[test]
fn issue_188_cancel_before_deferred_focus_never_submits_or_edits() {
    i_slint_backend_testing::init_no_event_loop();
    for (mode, grouped) in layouts() {
        let fixture = RenameFixture::new(mode, grouped, ".claude.json.backup");
        begin_rename_ui(&fixture.ui.as_weak(), &fixture.state);
        fixture.editor();
        fixture.ui.invoke_cancel_rename();
        assert!(
            fixture
                .ui
                .root_element()
                .query_descendants()
                .match_id("InlineRenameEditor::rename-editor")
                .find_all()
                .is_empty()
        );
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        fixture.key("X");
        assert_eq!(fixture.ui.get_rename_input(), ".claude.json.backup");
        assert!(fixture.commits.borrow().is_empty());
        assert!(!fixture.ui.get_rename_editing());
    }
}

#[test]
fn issue_187_outside_click_commits_original_target_and_selects_other_item_once() {
    i_slint_backend_testing::init_no_event_loop();
    for (mode, grouped, search, modifier) in layouts()
        .into_iter()
        .flat_map(|(mode, grouped)| {
            [None, Some(Key::Control), Some(Key::Shift)]
                .map(move |modifier| (mode, grouped, false, modifier))
        })
        .chain(
            [None, Some(Key::Control), Some(Key::Shift)]
                .map(|modifier| (ViewMode::Details, false, true, modifier)),
        )
    {
        let fixture = RenameFixture::new(mode, grouped, "original.txt");
        {
            let mut app = fixture.state.lock().unwrap();
            let mut entries = app.active().entries.as_ref().clone();
            let mut other = entries[0].clone();
            other.id = EntryId(2);
            other.original_name = "other.txt".into();
            other.display_name = "other.txt".into();
            other.path = PathBuf::from(r"C:\issue-187-memory\other.txt").into();
            entries.push(other);
            let tab = app.tab_mut(TabId(1)).unwrap();
            if search {
                tab.begin_search(SearchScope::Global, ".txt".into());
                tab.merge_search_page(0, entries, 2, 2, 0);
                tab.search_state = SearchState::Complete;
            } else {
                tab.replace_entries(entries);
            }
            tab.select_entry(EntryId(1), false, false);
        }
        refresh_ui(&fixture.ui, &fixture.state);
        let selected = fixture.state.clone();
        let activations = fixture.activations.clone();
        let weak = fixture.ui.as_weak();
        fixture
            .ui
            .on_activate_entry(move |id, toggle, extend, name_hit| {
                activations.borrow_mut().push((id, name_hit));
                let changed = {
                    let mut app = selected.lock().unwrap();
                    mutate_selection(app.tab_mut(TabId(1)).unwrap(), |tab| {
                        tab.select_entry(EntryId(id as u32), toggle, extend)
                    })
                };
                update_file_rows(&weak.upgrade().unwrap(), &selected, TabId(1), &changed);
            });
        let (sender, _pending_work) = mpsc::channel();
        rename_submission::wire(&fixture.ui, &fixture.state, &sender);
        fixture.begin();
        fixture.ui.set_rename_input("changed.txt".into());
        let id = match (mode, grouped) {
            (ViewMode::Details | ViewMode::List | ViewMode::Content, _) => "DetailsList::mouse",
            (_, false) => "FileGrid::card-touch",
            (_, true) => "FileGrid::grouped-card-touch",
        };
        let other = fixture
            .ui
            .root_element()
            .query_descendants()
            .match_id(id)
            .find_all()
            .into_iter()
            .filter(|item| item.size().width > 0.0 && item.size().height > 0.0)
            .nth(1)
            .unwrap();
        let origin = other.absolute_position();
        let point = slint::LogicalPosition::new(origin.x + 10.0, origin.y + 10.0);
        fixture.move_pointer(point);
        assert_eq!(
            fixture.state.lock().unwrap().operations.iter().count(),
            0,
            "hover must not submit"
        );
        if let Some(key) = modifier {
            fixture
                .ui
                .window()
                .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        }
        fixture.click(point);
        if let Some(key) = modifier {
            fixture
                .ui
                .window()
                .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
        }
        assert!(
            !fixture.ui.get_rename_editing(),
            "accepted work ends the editor"
        );
        assert_eq!(fixture.state.lock().unwrap().submitted_renames.len(), 1);
        let app = fixture.state.lock().unwrap();
        let task = app.operations.iter().next().unwrap();
        assert_eq!(
            task.items[0].source.as_deref(),
            Some(Path::new(r"C:\issue-187-memory\original.txt"))
        );
        assert!(
            !task.state.is_terminal(),
            "the background worker has not completed"
        );
        drop(app);
        assert_eq!(
            fixture.state.lock().unwrap().active().selected,
            if modifier.is_some() {
                vec![EntryId(1), EntryId(2)]
            } else {
                vec![EntryId(2)]
            },
            "one click selects the other item: {mode:?}/{grouped}/{search}/{modifier:?}"
        );
        assert_eq!(
            fixture.activations.borrow().len(),
            1,
            "the press must not be replayed"
        );
        assert_eq!(fixture.activations.borrow()[0].0, 2);
    }
}

#[test]
fn issue_187_wrapped_editor_uses_actual_bounds_after_growing_upward() {
    i_slint_backend_testing::init_no_event_loop();
    for grouped in [false, true] {
        let fixture = RenameFixture::new(ViewMode::MediumIcons, grouped, "original.txt");
        fixture
            .ui
            .window()
            .set_size(slint::LogicalSize::new(1180.0, 360.0));
        fixture.begin();
        fixture
            .ui
            .set_rename_input(format!("{}.txt", "long-name-".repeat(200)).into());
        fixture.ui.window().request_redraw();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        let editor = fixture.element("WindowOverlays::rename-editor");
        let p = editor.absolute_position();
        let size = editor.size();
        assert!(size.height > 30.0, "the editor must wrap");
        fixture.click(slint::LogicalPosition::new(
            p.x + 2.0,
            p.y + size.height - 2.0,
        ));
        fixture.assert_editing_without_commit();
        fixture.click(slint::LogicalPosition::new(
            p.x + size.width + 2.0,
            p.y + 2.0,
        ));
        assert_eq!(fixture.commits.borrow().len(), 1);
    }
}

mod submission;
