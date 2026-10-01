use std::path::{Path, PathBuf};

use game_larper_core::{Error, normalize_executable, resolve_executable};

#[test]
fn path_accepts_a_safe_nested_windows_executable() {
    let relative = normalize_executable(r"bin\win64\game.exe").unwrap();
    assert_eq!(
        relative,
        PathBuf::from("bin").join("win64").join("game.exe")
    );
    let root = std::env::temp_dir().join("GameLarperPathTest");
    let resolved = resolve_executable(&root, "123", r"bin\win64\game.exe").unwrap();
    let prefix = std::path::absolute(root.join("123")).unwrap();
    let mut expected = prefix.display().to_string();
    if !expected.ends_with(['\\', '/']) {
        expected.push(std::path::MAIN_SEPARATOR);
    }
    assert!(
        resolved
            .display()
            .to_string()
            .to_ascii_lowercase()
            .starts_with(&expected.to_ascii_lowercase())
    );
}

#[test]
fn path_rejects_absolute_traversal_and_device_names() {
    for path in [
        "",
        "game",
        "../game.exe",
        "bin/../game.exe",
        "C:/game.exe",
        r"C:\game.exe",
        r"\\server\share\game.exe",
        "/game.exe",
        "con.exe",
        "bin//game.exe",
        "game.exe:stream",
        "game?.exe",
        "game.exe.",
        "bin/./game.exe",
        ">hl2.exe",
        "com1.game.exe",
        "game.exe ",
    ] {
        assert!(
            normalize_executable(path).is_none(),
            "unsafe path was accepted: {path}"
        );
    }
    let error = resolve_executable(Path::new(r"C:\temp"), "../1", "game.exe").unwrap_err();
    assert!(matches!(error, Error::InvalidApplicationId));
}

#[test]
fn xdg_data_home_prefers_an_absolute_xdg_value_then_home() {
    use game_larper_core::xdg_data_home;
    let some = |value: &str| Some(std::ffi::OsString::from(value));
    assert_eq!(
        xdg_data_home(some("/data/kya"), some("/home/kya")),
        Some(PathBuf::from("/data/kya"))
    );
    // The base-directory spec says relative and empty values are invalid and ignored.
    for ignored in ["relative/data", ""] {
        assert_eq!(
            xdg_data_home(some(ignored), some("/home/kya")),
            Some(Path::new("/home/kya").join(".local").join("share"))
        );
    }
    assert_eq!(xdg_data_home(None, some("relative-home")), None);
    assert_eq!(xdg_data_home(None, None), None);
}
