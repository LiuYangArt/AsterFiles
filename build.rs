fn main() {
    slint_build::compile("ui/app-window.slint").expect("failed to compile Slint UI");

    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/windows/asterfiles.rc");
        println!("cargo:rerun-if-changed=assets/windows/asterfiles.ico");
        embed_resource::compile("assets/windows/asterfiles.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("failed to compile Windows application resources");
    }
}
