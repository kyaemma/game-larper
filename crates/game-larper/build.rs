fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("../../assets/branding/game-larper.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=../../assets/ui");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon(icon.to_str().expect("icon path is unicode"));
        resource.set("ProductName", "Game Larper");
        resource.set("FileDescription", "Game Larper");
        resource.set("OriginalFilename", "GameLarper.exe");
        let [major, minor, patch] = ["MAJOR", "MINOR", "PATCH"].map(|part| {
            std::env::var(format!("CARGO_PKG_VERSION_{part}"))
                .ok()
                .and_then(|value| value.parse::<u16>().ok())
                .expect("Cargo package version components fit a Windows version field")
        });
        let version = format!("{major}.{minor}.{patch}.0");
        let packed = (u64::from(major) << 48) | (u64::from(minor) << 32) | (u64::from(patch) << 16);
        resource.set("FileVersion", &version);
        resource.set("ProductVersion", &version);
        resource.set_version_info(winresource::VersionInfo::FILEVERSION, packed);
        resource.set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);
        resource.compile().expect("embed the Game Larper icon");
    }
    slint_build::compile("ui/app.slint").expect("compile the Slint UI");
}
