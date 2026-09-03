fn main() {
    let emit_debug_info = std::env::var("PROFILE").is_ok_and(|profile| profile != "release");
    slint_build::compile_with_config(
        "ui/app-window.slint",
        slint_build::CompilerConfiguration::new().with_debug_info(emit_debug_info),
    )
    .expect("failed to compile Slint UI");

    #[cfg(windows)]
    {
        let version =
            std::env::var("CARGO_PKG_VERSION").expect("Cargo must provide the package version");
        let version_parts = windows_version_parts(&version);
        let out_dir = std::path::PathBuf::from(
            std::env::var_os("OUT_DIR").expect("Cargo must provide OUT_DIR"),
        );
        let version_resource = out_dir.join("asterfiles-version.rc");
        std::fs::write(
            &version_resource,
            format!(
                r#"#define ASTERFILES_VERSION_COMMAS {0},{1},{2},{3}
#define ASTERFILES_VERSION_STRING "{4}"
#define ASTERFILES_ICON_PATH "{5}"
#include "{6}"
"#,
                version_parts[0],
                version_parts[1],
                version_parts[2],
                version_parts[3],
                version,
                resource_path("assets/windows/asterfiles.ico"),
                resource_path("assets/windows/asterfiles.rc")
            ),
        )
        .expect("failed to generate Windows version resource");

        println!("cargo:rerun-if-changed=assets/windows/asterfiles.rc");
        println!("cargo:rerun-if-changed=assets/windows/asterfiles.ico");
        embed_resource::compile(version_resource, embed_resource::NONE)
            .manifest_optional()
            .expect("failed to compile Windows application resources");
    }
}

#[cfg(windows)]
fn windows_version_parts(version: &str) -> [u16; 4] {
    let mut parts = [0; 4];
    for (index, part) in version.split('.').take(3).enumerate() {
        parts[index] = part
            .parse()
            .unwrap_or_else(|_| panic!("package version component must be numeric: {version}"));
    }
    parts
}

#[cfg(windows)]
fn resource_path(path: &str) -> String {
    std::path::Path::new(path)
        .canonicalize()
        .expect("failed to resolve Windows resource path")
        .display()
        .to_string()
        .replace('\\', "/")
}
