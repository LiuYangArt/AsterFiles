fn main() {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/app-window.slint");
    let emit_debug_info = std::env::var("PROFILE").is_ok_and(|profile| profile != "release");
    slint_build::compile_with_config(
        source,
        slint_build::CompilerConfiguration::new().with_debug_info(emit_debug_info),
    )
    .expect("failed to compile Slint UI");
}
