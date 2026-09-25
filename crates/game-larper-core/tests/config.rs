use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use game_larper_core::{AppConfig, ConfigStore, format_startup_command};

#[test]
fn config_persists_selection_and_settings() {
    let root = temp_dir();
    let store = ConfigStore::new(root.join("config.json"));
    store
        .save(&AppConfig {
            launch_with_windows: true,
            auto_resume: true,
            last_selected_discord_application_id: Some("123".into()),
            ..AppConfig::default()
        })
        .unwrap();
    let (loaded, warnings) = store.load();
    assert!(warnings.is_empty());
    assert!(loaded.launch_with_windows);
    assert!(loaded.auto_resume);
    assert!(loaded.restore_last_selected_game);
    assert!(loaded.close_to_tray);
    assert!(loaded.preserve_queue);
    assert_eq!(
        loaded.last_selected_discord_application_id.as_deref(),
        Some("123")
    );
    assert_eq!(loaded.schema_version, 1);
}

#[test]
fn legacy_camel_case_config_loads() {
    let root = temp_dir();
    let path = root.join("config.json");
    std::fs::write(
        &path,
        r#"{"launchWithWindows":true,"future":1,"lastSelectedDiscordApplicationId":"9"}"#,
    )
    .unwrap();
    let (loaded, warnings) = ConfigStore::new(path).load();
    assert!(warnings.is_empty());
    assert!(loaded.launch_with_windows);
    assert!(loaded.restore_last_selected_game);
    assert!(!loaded.auto_resume);
    assert_eq!(
        loaded.last_selected_discord_application_id.as_deref(),
        Some("9")
    );
}

#[test]
fn corrupt_config_falls_back_to_defaults() {
    let root = temp_dir();
    let path = root.join("config.json");
    std::fs::write(&path, "{not json").unwrap();
    let (loaded, warnings) = ConfigStore::new(&path).load();
    assert!(loaded.restore_last_selected_game);
    assert!(!loaded.auto_resume);
    assert!(!warnings.is_empty());
    assert!(!path.exists());
    assert!(std::fs::read_dir(&root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("json.bad")
    }));
}

#[test]
fn startup_command_is_quoted() {
    let command = format_startup_command(Path::new(r"C:\Games\Game Larper.exe"), true).unwrap();
    assert_eq!(command, r#""C:\Games\Game Larper.exe" --minimized"#);
    assert!(format_startup_command(Path::new("C:\\a\"b.exe"), false).is_err());
}

fn temp_dir() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "game-larper-config-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}
