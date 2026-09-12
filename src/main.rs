#![cfg_attr(windows, windows_subsystem = "windows")]

mod agent_debug;
mod app;
mod domain;
mod fs;
mod group_projection;
mod i18n;
#[cfg(test)]
mod input_order_tests;
mod network;
mod operation_audit;
mod platform;
mod quick_menu_popup;
mod session_store;

use std::path::PathBuf;

#[cfg(windows)]
fn main_network_child() -> std::io::Result<bool> {
    platform::windows::network::try_run_child_from_args()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    struct AuditFlush;
    impl Drop for AuditFlush {
        fn drop(&mut self) {
            if let Err(error) = operation_audit::flush() {
                eprintln!("file operation audit flush failed: {error}");
            }
        }
    }
    let _audit_flush = AuditFlush;
    #[cfg(windows)]
    if main_network_child()? {
        return Ok(());
    }
    // The application owns its colors; keep native popup styling aligned with the startup system theme.
    unsafe {
        std::env::set_var(
            "SLINT_STYLE",
            if platform::system_uses_dark_theme() {
                "fluent-dark"
            } else {
                "fluent-light"
            },
        )
    };

    let mut agent_options = agent_debug::AgentOptions::from_env()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    let external_paths = agent_options.take_external_paths();

    if let Some(scenario) = agent_options.scenario {
        if scenario == agent_debug::AgentScenario::Home {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_home_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::DriveCapacity {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_drive_capacity_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::FileOperationCenter {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_file_operation_center_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::FileListTypeSelect {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_file_list_type_select_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::WindowsLibraries {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_windows_libraries_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::NetworkFoundation {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_network_foundation_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::MultiWindowStateLayering {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_multi_window_state_layering(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::TabReorder {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_tab_reorder_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::TabDetach {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_tab_detach_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::TabCrossWindow {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_tab_cross_window_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::ExplorerPins {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
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
                &output,
                format!(
                    "{{\"schema_version\":1,\"scenario\":\"explorer-pins\",\"scope\":\"real_windows_shell_read_only\",\"count\":{},\"items\":[{}]}}\n",
                    pins.len(),
                    items
                ),
            )?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::QuickAccess {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_quick_access_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::QuickMenuSearch {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_quick_menu_search_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::QuickMenuPopup {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            quick_menu_popup::export_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::FolderSizeScheduler {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_folder_size_scheduler_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::ThumbnailScheduler {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_thumbnail_scheduler_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::ShellThumbnail {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            export_shell_thumbnail_probe(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        let mut session = domain::TabSession::new(domain::TabId(1));
        agent_debug::apply_scenario(&mut session, scenario);
        let output = agent_options
            .state_output()
            .expect("scenario has a default state output");
        agent_debug::export_state(&session, scenario, &output)?;
        println!(
            "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
            scenario.name(),
            output.to_string_lossy().as_ref()
        );
        if agent_options.no_ui {
            return Ok(());
        }
    }

    let (external_paths, mut primary_instance) =
        match platform::windows::single_instance::coordinate(&external_paths)? {
            platform::windows::single_instance::InstanceOutcome::Primary(primary) => {
                (external_paths, Some(primary))
            }
            platform::windows::single_instance::InstanceOutcome::Forwarded => return Ok(()),
            platform::windows::single_instance::InstanceOutcome::Fallback => {
                eprintln!(
                    "AsterFiles could not contact the running instance; starting a fallback window."
                );
                (external_paths, None)
            }
        };
    #[cfg(windows)]
    {
        use slint::winit_030::winit::platform::windows::{
            CornerPreference, WindowAttributesExtWindows,
        };
        use slint::winit_030::winit::window::WindowLevel;

        slint::BackendSelector::new()
            .backend_name("winit".into())
            .with_winit_window_attributes_hook(|attributes| {
                if let Some(owner) =
                    platform::windows::quick_menu_window::take_pending_window_owner()
                {
                    return attributes
                        .with_owner_window(owner as _)
                        .with_active(false)
                        .with_decorations(false)
                        .with_transparent(true)
                        .with_window_level(WindowLevel::Normal)
                        .with_visible(false)
                        .with_drag_and_drop(false)
                        .with_skip_taskbar(true)
                        .with_undecorated_shadow(false)
                        .with_corner_preference(CornerPreference::Round);
                }
                if attributes.title == "AsterFiles Tab Drag Preview" {
                    return attributes
                        .with_decorations(false)
                        .with_transparent(true)
                        .with_window_level(WindowLevel::AlwaysOnTop)
                        .with_visible(false)
                        .with_drag_and_drop(false)
                        .with_skip_taskbar(true)
                        .with_undecorated_shadow(false);
                }
                attributes
                    .with_decorations(false)
                    .with_drag_and_drop(false)
                    .with_undecorated_shadow(true)
                    .with_corner_preference(CornerPreference::Round)
            })
            .select()?;
    }

    let external_path_receiver = primary_instance
        .as_mut()
        .map(platform::windows::single_instance::PrimaryInstance::take_receiver);
    app::run(
        agent_options.scenario,
        external_paths,
        external_path_receiver,
    )?;
    drop(primary_instance);
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
        std::fs::write(&png, include_bytes!("../assets/app-icon.png"))?;
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
    for byte in &image.pixels {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
