use super::{AgentScenario, apply_scenario, export_state};
use crate::{app, platform, quick_menu_popup};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

type Exporter = fn(&Path) -> Result<(), Box<dyn std::error::Error>>;

struct Scenario {
    id: AgentScenario,
    name: &'static str,
    export: Option<Exporter>,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        id: AgentScenario::PermissionDenied,
        name: "permission-denied",
        export: None,
    },
    Scenario {
        id: AgentScenario::FileOperationCenter,
        name: "file-operation-center",
        export: Some(|path| Ok(app::export_file_operation_center_state(path)?)),
    },
    Scenario {
        id: AgentScenario::DragDropFoundation,
        name: "drag-drop-foundation",
        export: None,
    },
    Scenario {
        id: AgentScenario::MultiWindowStateLayering,
        name: "multi-window-state-layering",
        export: Some(|path| Ok(app::export_multi_window_state_layering(path)?)),
    },
    Scenario {
        id: AgentScenario::TabReorder,
        name: "tab-reorder",
        export: Some(|path| Ok(app::export_tab_reorder_state(path)?)),
    },
    Scenario {
        id: AgentScenario::TabDetach,
        name: "tab-detach",
        export: Some(|path| Ok(app::export_tab_detach_state(path)?)),
    },
    Scenario {
        id: AgentScenario::TabCrossWindow,
        name: "tab-cross-window",
        export: Some(|path| Ok(app::export_tab_cross_window_state(path)?)),
    },
    Scenario {
        id: AgentScenario::ExplorerPins,
        name: "explorer-pins",
        export: Some(export_explorer_pins),
    },
    Scenario {
        id: AgentScenario::WindowsLibraries,
        name: "windows-libraries",
        export: Some(|path| Ok(app::export_windows_libraries_state(path)?)),
    },
    Scenario {
        id: AgentScenario::QuickAccess,
        name: "quick-access",
        export: Some(|path| Ok(app::export_quick_access_state(path)?)),
    },
    Scenario {
        id: AgentScenario::ShellThumbnail,
        name: "shell-thumbnail",
        export: Some(export_shell_thumbnail_probe),
    },
    Scenario {
        id: AgentScenario::ThumbnailScheduler,
        name: "thumbnail-scheduler",
        export: Some(|path| Ok(app::export_thumbnail_scheduler_state(path)?)),
    },
    Scenario {
        id: AgentScenario::FolderSizeScheduler,
        name: "folder-size-scheduler",
        export: Some(|path| Ok(app::export_folder_size_scheduler_state(path)?)),
    },
    Scenario {
        id: AgentScenario::QuickMenuSearch,
        name: "quick-menu-search",
        export: Some(|path| Ok(app::export_quick_menu_search_state(path)?)),
    },
    Scenario {
        id: AgentScenario::QuickMenuPopup,
        name: "quick-menu-popup",
        export: Some(|path| Ok(quick_menu_popup::export_state(path)?)),
    },
    Scenario {
        id: AgentScenario::NetworkFoundation,
        name: "network-foundation",
        export: Some(|path| Ok(app::export_network_foundation_state(path)?)),
    },
    Scenario {
        id: AgentScenario::DriveCapacity,
        name: "drive-capacity",
        export: Some(|path| Ok(app::export_drive_capacity_state(path)?)),
    },
    Scenario {
        id: AgentScenario::Home,
        name: "home",
        export: Some(|path| Ok(app::export_home_state(path)?)),
    },
    Scenario {
        id: AgentScenario::FileListTypeSelect,
        name: "file-list-type-select",
        export: Some(|path| Ok(app::export_file_list_type_select_state(path)?)),
    },
    Scenario {
        id: AgentScenario::AgentActions,
        name: "agent-actions",
        export: Some(|path| Ok(app::action_scenario::export(path)?)),
    },
];

impl AgentScenario {
    fn definition(self) -> &'static Scenario {
        SCENARIOS
            .iter()
            .find(|scenario| scenario.id == self)
            .expect("every AgentScenario is registered")
    }

    pub fn name(self) -> &'static str {
        self.definition().name
    }
}

pub(super) fn parse_scenario(value: &str) -> Result<AgentScenario, String> {
    SCENARIOS
        .iter()
        .find(|scenario| scenario.name == value)
        .map(|scenario| scenario.id)
        .ok_or_else(|| format!("unknown agent scenario: {value}"))
}

pub fn export_scenario(
    scenario: AgentScenario,
    output: &Path,
    no_ui: bool,
    events: &mut impl Write,
) -> Result<bool, Box<dyn std::error::Error>> {
    let exporter = scenario.definition().export;
    match exporter {
        Some(export) => export(output)?,
        None => export_session(scenario, output)?,
    }
    // The action sequence owns its report and has always exited without a shared event.
    if scenario == AgentScenario::AgentActions {
        return Ok(true);
    }
    write_export_event(events, scenario, output)?;
    // Preserve the session projection used before opening a scenario in the application.
    if !no_ui && exporter.is_some() {
        export_session(scenario, output)?;
        write_export_event(events, scenario, output)?;
    }
    Ok(no_ui)
}

fn export_session(scenario: AgentScenario, output: &Path) -> io::Result<()> {
    let mut session = crate::domain::TabSession::new(crate::domain::TabId(1));
    apply_scenario(&mut session, scenario);
    export_state(&session, scenario, output)
}

fn write_export_event(
    events: &mut impl Write,
    scenario: AgentScenario,
    output: &Path,
) -> io::Result<()> {
    writeln!(
        events,
        "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
        scenario.name(),
        output.to_string_lossy().as_ref()
    )
}

fn export_explorer_pins(output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let pins = platform::explorer_pinned_locations()?;
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let items = pins
        .iter()
        .map(|item| {
            format!(
                "{{\"label\":{:?},\"path\":{:?}}}",
                item.label,
                item.path.to_string_lossy()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(
        output,
        format!(
            "{{\"schema_version\":1,\"scenario\":\"explorer-pins\",\"scope\":\"real_windows_shell_read_only\",\"count\":{},\"items\":[{}]}}\n",
            pins.len(),
            items
        ),
    )?;
    Ok(())
}

fn export_shell_thumbnail_probe(
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use platform::windows::shell_icons::{self, GridImageSource, ThumbnailSource};
    let supplied = std::env::var_os("ASTERFILES_THUMBNAIL_PROBE_PATH").map(PathBuf::from);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "asterfiles-thumbnail-probe-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&directory)?;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let png = directory.join("probe.png");
        let mtl = directory.join("probe.mtl");
        let unknown = directory.join("probe.asterfiles_issue_98_unknown");
        let folder = directory.join("folder");
        std::fs::write(&png, include_bytes!("../../assets/app-icon.png"))?;
        std::fs::write(&mtl, b"newmtl Probe\nKd 0.5 0.5 0.5\n")?;
        std::fs::write(&unknown, b"AsterFiles #98 unknown file fixture\n")?;
        std::fs::create_dir(&folder)?;
        let mut fixtures = vec![
            ("png", png.as_path()),
            ("mtl", mtl.as_path()),
            ("unknown", unknown.as_path()),
            ("folder", folder.as_path()),
        ];
        if let Some(path) = supplied.as_deref() {
            fixtures.push(("user_supplied", path));
        }
        let _shell_apartment = shell_icons::initialize_shell_worker()?;
        let mut records = Vec::new();
        let mut primary = None;
        for (kind, path) in fixtures {
            for requested in [128_u32, 256] {
                for pass in ["first", "repeat"] {
                    let image = shell_icons::shell_grid_image_rgba(path, requested)?;
                    let source = match image.source {
                        GridImageSource::Thumbnail(ThumbnailSource::Cache) => "thumbnail_cache",
                        GridImageSource::Thumbnail(ThumbnailSource::Provider) => "provider",
                        GridImageSource::SystemIcon => "system_icon",
                    };
                    if image.image.width == 0 || image.image.height == 0 {
                        return Err(format!("{kind}: Shell returned an empty image").into());
                    }
                    if kind == "png"
                        && (image.source == GridImageSource::SystemIcon
                            || image.image.width < 64
                            || image.image.height < 64)
                    {
                        return Err("PNG fixture did not produce a real thumbnail".into());
                    }
                    if matches!(kind, "mtl" | "unknown")
                        && image.source != GridImageSource::SystemIcon
                    {
                        return Err(format!("{kind}: expected a system file icon").into());
                    }
                    records.push(format!(
                        "{{\"fixture\":\"{kind}\",\"pass\":\"{pass}\",\"requested_px\":{requested},\"returned_px\":[{},{}],\"source\":\"{source}\",\"thumbnail_error\":{}}}",
                        image.image.width,
                        image.image.height,
                        image.thumbnail_error.as_deref().map(shell_probe_json_string).unwrap_or_else(|| "null".to_owned()),
                    ));
                    if requested == 128
                        && pass == "first"
                        && ((supplied.is_none() && kind == "png") || kind == "user_supplied")
                    {
                        primary = Some((image, source));
                    }
                }
                let icon = shell_icons::shell_icon_rgba_at_size(path, requested)?;
                if icon.width == 0 || icon.height == 0 {
                    return Err(format!("{kind}: Shell returned an empty direct icon").into());
                }
                records.push(format!(
                    "{{\"fixture\":\"{kind}\",\"pass\":\"direct_icon\",\"requested_px\":{requested},\"returned_px\":[{},{}],\"source\":\"system_icon\",\"thumbnail_error\":null}}",
                    icon.width, icon.height,
                ));
            }
        }
        let (image, source) = primary.expect("the primary fixture was extracted");
        let scope = if supplied.is_some() {
            "user_supplied_file"
        } else {
            "real_windows_shell_temporary_png"
        };
        let json = format!(
            "{{\"schema_version\":1,\"scenario\":\"shell-thumbnail\",\"scope\":\"{scope}\",\"requested_px\":128,\"returned_px\":[{},{}],\"source\":\"{source}\",\"icon_fallback\":{},\"pixel_fingerprint\":\"{}\",\"cache_probe\":\"first_and_repeated_extraction_without_shell_cache_reset\",\"grid_images\":[{}]}}\n",
            image.image.width,
            image.image.height,
            image.source == GridImageSource::SystemIcon,
            thumbnail_fingerprint(&image.image),
            records.join(","),
        );
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(output, json)?;
        Ok(())
    })();
    // This unique directory only contains probe-owned fixtures, including when extraction fails.
    let cleanup = std::fs::remove_dir_all(&directory);
    match (result, cleanup) {
        (Err(error), Err(cleanup_error)) => {
            Err(format!("{error}; fixture cleanup failed: {cleanup_error}").into())
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn shell_probe_json_string(value: &str) -> String {
    use std::fmt::Write;
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            character if character.is_control() => {
                write!(output, "\\u{:04x}", character as u32)
                    .expect("writing to a string succeeds");
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}
fn thumbnail_fingerprint(image: &platform::windows::shell_icons::ShellIconRgba) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in image.pixels.iter() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_debug::AgentOptions;
    use std::{ffi::OsString, fs};

    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "asterfiles-dispatch-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            // The exclusively created sandbox contains only this test's reports.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn issue_169_registry_preserves_all_names_paths_and_exporters() {
        use AgentScenario::*;
        let expected = [
            (
                PermissionDenied,
                "permission-denied",
                "permission-denied.json",
                false,
            ),
            (
                FileOperationCenter,
                "file-operation-center",
                "file-operation-center.json",
                true,
            ),
            (
                DragDropFoundation,
                "drag-drop-foundation",
                "drag-drop-foundation.json",
                false,
            ),
            (
                MultiWindowStateLayering,
                "multi-window-state-layering",
                "multi-window-state-layering.json",
                true,
            ),
            (TabReorder, "tab-reorder", "tab-reorder.json", true),
            (TabDetach, "tab-detach", "tab-detach.json", true),
            (
                TabCrossWindow,
                "tab-cross-window",
                "tab-cross-window.json",
                true,
            ),
            (ExplorerPins, "explorer-pins", "explorer-pins.json", true),
            (
                WindowsLibraries,
                "windows-libraries",
                "windows-libraries/foundation.json",
                true,
            ),
            (QuickAccess, "quick-access", "quick-access.json", true),
            (
                ShellThumbnail,
                "shell-thumbnail",
                "shell-thumbnail.json",
                true,
            ),
            (
                ThumbnailScheduler,
                "thumbnail-scheduler",
                "thumbnails/scheduler.json",
                true,
            ),
            (
                FolderSizeScheduler,
                "folder-size-scheduler",
                "folder-size-scheduler.json",
                true,
            ),
            (
                QuickMenuSearch,
                "quick-menu-search",
                "quick-menu-search.json",
                true,
            ),
            (
                QuickMenuPopup,
                "quick-menu-popup",
                "quick-menu-popup.json",
                true,
            ),
            (
                NetworkFoundation,
                "network-foundation",
                "network-foundation.json",
                true,
            ),
            (
                DriveCapacity,
                "drive-capacity",
                "drives/capacity.json",
                true,
            ),
            (Home, "home", "home/state.json", true),
            (
                FileListTypeSelect,
                "file-list-type-select",
                "file-list-type-select.json",
                true,
            ),
            (
                AgentActions,
                "agent-actions",
                "agent-actions/sequence.json",
                true,
            ),
        ];
        assert_eq!(SCENARIOS.len(), expected.len());
        for (index, &(id, name, path, specialized)) in expected.iter().enumerate() {
            assert_eq!(parse_scenario(name).unwrap(), id);
            assert_eq!(id.name(), name);
            assert_eq!(id.definition().export.is_some(), specialized);
            assert_eq!(id.default_path(), Path::new("artifacts/state").join(path));
            assert_eq!(SCENARIOS.iter().filter(|entry| entry.id == id).count(), 1);
            assert_eq!(
                SCENARIOS.iter().filter(|entry| entry.name == name).count(),
                1
            );
            assert!(!expected[..index].iter().any(|entry| entry.0 == id));
            let output = PathBuf::from(r"C:\custom output\报告.json");
            let options = AgentOptions::from_arguments([
                OsString::from("--agent-scenario"),
                OsString::from(name),
                OsString::from("--no-ui"),
                OsString::from("--agent-state-out"),
                output.clone().into_os_string(),
            ])
            .unwrap();
            assert_eq!(options.scenario, Some(id));
            assert_eq!(options.state_output(), Some(output));
            assert!(options.no_ui);
        }
        assert_eq!(
            parse_scenario("unknown").unwrap_err(),
            "unknown agent scenario: unknown"
        );
    }

    fn expected_event(scenario: AgentScenario, output: &Path) -> String {
        format!(
            "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}\n",
            scenario.name(),
            output.to_string_lossy().as_ref()
        )
    }

    #[test]
    fn issue_169_session_exports_share_events_and_no_ui_handling() {
        let sandbox = Sandbox::new();
        for scenario in [
            AgentScenario::PermissionDenied,
            AgentScenario::DragDropFoundation,
        ] {
            for no_ui in [false, true] {
                let output = sandbox
                    .0
                    .join("nested")
                    .join(format!("{}.json", scenario.name()));
                let mut events = Vec::new();
                assert_eq!(
                    export_scenario(scenario, &output, no_ui, &mut events).unwrap(),
                    no_ui
                );
                assert_eq!(
                    String::from_utf8(events).unwrap(),
                    expected_event(scenario, &output)
                );
                let session = fs::read_to_string(output).unwrap();
                assert!(session.contains("\"page_state\":"));
                assert!(session.contains(scenario.name()));
            }
        }
    }

    #[test]
    fn issue_169_specialized_export_preserves_no_ui_and_session_projection() {
        let sandbox = Sandbox::new();
        let output = sandbox.0.join("home.json");
        let scenario = AgentScenario::Home;
        let mut events = Vec::new();
        assert!(export_scenario(scenario, &output, true, &mut events).unwrap());
        assert_eq!(
            String::from_utf8(events).unwrap(),
            expected_event(scenario, &output)
        );
        assert!(
            fs::read_to_string(&output)
                .unwrap()
                .contains("\"scope\": \"pure_model_no_ui_no_shell_no_disk_scan\"")
        );

        let mut events = Vec::new();
        assert!(!export_scenario(scenario, &output, false, &mut events).unwrap());
        assert_eq!(
            String::from_utf8(events).unwrap(),
            expected_event(scenario, &output).repeat(2)
        );
        let report = fs::read_to_string(output).unwrap();
        assert!(report.contains("\"location_kind\": \"home\""));
        assert!(!report.contains("\"scope\""));
    }

    #[test]
    fn issue_169_actions_always_exit_without_shared_events() {
        let sandbox = Sandbox::new();
        let output = sandbox.0.join("sequence.json");
        for no_ui in [false, true] {
            let mut events = Vec::new();
            assert!(
                export_scenario(AgentScenario::AgentActions, &output, no_ui, &mut events).unwrap()
            );
            assert!(events.is_empty());
            assert!(
                fs::read_to_string(&output)
                    .unwrap()
                    .contains("\"passed\": true")
            );
        }
    }

    #[test]
    fn issue_169_export_errors_stop_before_events_or_session_fallback() {
        let sandbox = Sandbox::new();
        for scenario in [
            AgentScenario::PermissionDenied,
            AgentScenario::Home,
            AgentScenario::AgentActions,
        ] {
            let mut events = Vec::new();
            assert!(export_scenario(scenario, &sandbox.0, false, &mut events).is_err());
            assert!(events.is_empty());
            assert!(sandbox.0.is_dir());
        }
    }
}
