fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("../../assets/branding/game-larper.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed=ui/app.slint");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon(icon.to_str().expect("icon path is unicode"));
        resource.set("ProductName", "Game Larper");
        resource.set("FileDescription", "Game Larper");
        resource.set("OriginalFilename", "GameLarper.exe");
        resource.set("FileVersion", "0.1.0.0");
        resource.set("ProductVersion", "0.1.0.0");
        resource.compile().expect("embed the Game Larper icon");
    }
    slint_build::compile("ui/app.slint").expect("compile the Slint UI");
}
