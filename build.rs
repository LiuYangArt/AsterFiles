fn main() {
    let emit_debug_info = std::env::var("PROFILE").is_ok_and(|profile| profile != "release");
    slint_build::compile_with_config(
        "ui/app-window.slint",
        slint_build::CompilerConfiguration::new().with_debug_info(emit_debug_info),
    )
    .expect("failed to compile Slint UI");

    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/windows/asterfiles.rc");
        println!("cargo:rerun-if-changed=assets/windows/asterfiles.ico");
        embed_resource::compile("assets/windows/asterfiles.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("failed to compile Windows application resources");
    }
}
