#![cfg_attr(windows, windows_subsystem = "windows")]

mod agent_debug;
mod app;
mod domain;
mod fs;
mod group_projection;
mod i18n;
mod platform;
mod session_store;

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    let agent_options = agent_debug::AgentOptions::from_env()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;

    if let Some(scenario) = agent_options.scenario {
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

    #[cfg(windows)]
    {
        use slint::winit_030::winit::platform::windows::{
            CornerPreference, WindowAttributesExtWindows,
        };
        use slint::winit_030::winit::window::WindowLevel;

        slint::BackendSelector::new()
            .backend_name("winit".into())
            .with_winit_window_attributes_hook(|attributes| {
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

    app::run(agent_options.scenario)?;
    Ok(())
}

fn export_shell_thumbnail_probe(
    output: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let supplied = std::env::var_os("ASTERFILES_THUMBNAIL_PROBE_PATH").map(PathBuf::from);
    let temporary = supplied.is_none();
    let directory = temporary.then(|| {
        std::env::temp_dir().join(format!("asterfiles-thumbnail-probe-{}", std::process::id()))
    });
    let png = if let Some(path) = supplied {
        path
    } else {
        let directory = directory.as_ref().expect("temporary directory exists");
        std::fs::create_dir_all(directory)?;
        let png = directory.join("probe.png");
        let bytes: &[u8] = include_bytes!("../assets/app-icon.png");
        let mut file = std::fs::File::create(&png)?;
        file.write_all(bytes)?;
        drop(file);
        png
    };
    let requested = 128_u32;
    let _shell_apartment = platform::windows_shell_icons::initialize_shell_worker()?;
    let result = platform::windows_shell_icons::shell_thumbnail_rgba(&png, requested, false);
    let json = match result {
        Ok(result) => {
            if result.image.width < 64 || result.image.height < 64 {
                return Err(format!(
                    "Shell returned undersized PNG thumbnail: {}x{}",
                    result.image.width, result.image.height
                )
                .into());
            }
            format!(
                "{{\"schema_version\":1,\"scenario\":\"shell-thumbnail\",\"scope\":\"{}\",\"requested_px\":{},\"returned_px\":[{},{}],\"source\":\"{}\",\"icon_fallback\":false,\"pixel_fingerprint\":\"{}\"}}\n",
                if temporary {
                    "real_windows_shell_temporary_png"
                } else {
                    "user_supplied_png"
                },
                requested,
                result.image.width,
                result.image.height,
                match result.source {
                    platform::windows_shell_icons::ThumbnailSource::Cache => "thumbnail_cache",
                    platform::windows_shell_icons::ThumbnailSource::Provider => "provider",
                },
                thumbnail_fingerprint(&result.image),
            )
        }
        Err(error) => return Err(format!("PNG thumbnail extraction failed: {error}").into()),
    };
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, json)?;
    if let Some(directory) = directory {
        std::fs::remove_file(&png)?;
        std::fs::remove_dir(&directory)?;
    }
    Ok(())
}

fn thumbnail_fingerprint(image: &platform::windows_shell_icons::ShellIconRgba) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in &image.pixels {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
