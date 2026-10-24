use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use game_larper_core::{Error, MAX_RECENT, RecentGames, load_recent, save_recent};

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "game-larper-recent-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn ids(recent: &RecentGames) -> Vec<&str> {
    recent.items().iter().map(|item| item.id.as_str()).collect()
}

#[test]
fn newest_launch_comes_first() {
    let mut recent = RecentGames::default();
    assert!(recent.is_empty());
    recent.record("1", "Alpha", 100).unwrap();
    recent.record("2", "Beta", 200).unwrap();
    recent.record("3", "Gamma", 300).unwrap();
    assert_eq!(ids(&recent), ["3", "2", "1"]);
    assert_eq!(recent.items()[0].played_at_ms, 300);
}

#[test]
fn launching_again_moves_the_game_up_without_a_duplicate() {
    let mut recent = RecentGames::default();
    recent.record("1", "Alpha", 100).unwrap();
    recent.record("2", "Beta", 200).unwrap();
    recent.record("1", "Alpha", 300).unwrap();
    assert_eq!(ids(&recent), ["1", "2"]);
    assert_eq!(recent.len(), 2);
    assert_eq!(recent.items()[0].played_at_ms, 300);
}

#[test]
fn the_oldest_entries_fall_off_past_the_limit() {
    let mut recent = RecentGames::default();
    for id in 1..=(MAX_RECENT as i64 + 5) {
        recent
            .record(&id.to_string(), &format!("Game {id}"), id)
            .unwrap();
    }
    assert_eq!(recent.len(), MAX_RECENT);
    assert_eq!(recent.items()[0].id, (MAX_RECENT + 5).to_string());
    assert_eq!(recent.items()[MAX_RECENT - 1].id, "6");
}

#[test]
fn invalid_ids_and_names_are_rejected() {
    let mut recent = RecentGames::default();
    for id in ["", "12a", " 12", "-3"] {
        assert!(
            matches!(
                recent.record(id, "Game", 1),
                Err(Error::InvalidApplicationId)
            ),
            "{id:?} should be rejected"
        );
    }
    assert!(recent.record("1", "   ", 1).is_err());
    assert!(recent.record("1", &"x".repeat(201), 1).is_err());
    assert!(recent.is_empty());
}

#[test]
fn a_rejected_launch_keeps_the_existing_entry() {
    let mut recent = RecentGames::default();
    recent.record("1", "Alpha", 100).unwrap();
    assert!(recent.record("1", "", 200).is_err());
    assert_eq!(recent.items()[0].name, "Alpha");
    assert_eq!(recent.items()[0].played_at_ms, 100);
}

#[test]
fn save_then_load_keeps_order_names_and_times() {
    let root = temp_dir("round-trip");
    let path = root.join("recent.json");
    let mut recent = RecentGames::default();
    recent.record("30", "Zelda", 1_000).unwrap();
    recent.record("10", "Alpha", 2_000).unwrap();
    save_recent(&path, &recent).unwrap();
    let (loaded, warnings) = load_recent(&path).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(loaded, recent);
    assert_eq!(ids(&loaded), ["10", "30"]);
}

#[test]
fn missing_file_loads_empty_without_warnings() {
    let root = temp_dir("missing");
    let (loaded, warnings) = load_recent(&root.join("recent.json")).unwrap();
    assert!(loaded.is_empty());
    assert!(warnings.is_empty());
}

#[test]
fn corrupt_file_is_moved_aside_and_loads_empty() {
    let root = temp_dir("corrupt");
    let path = root.join("recent.json");
    std::fs::write(&path, "{not json").unwrap();
    let (loaded, warnings) = load_recent(&path).unwrap();
    assert!(loaded.is_empty());
    assert_eq!(warnings.len(), 1);
    assert!(!path.exists());
    let quarantined = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("recent.json.bad-")
        });
    assert!(quarantined);
}

#[test]
fn unreadable_file_is_an_error_and_left_in_place() {
    let root = temp_dir("unreadable");
    let path = root.join("recent.json");
    // A directory cannot be read as a file, and it is not a missing file either.
    std::fs::create_dir_all(&path).unwrap();
    assert!(load_recent(&path).is_err());
    assert!(path.is_dir());
}

#[test]
fn load_drops_invalid_and_duplicate_ids_and_caps_the_list() {
    let root = temp_dir("invalid");
    let path = root.join("recent.json");
    let mut games = String::from(
        r#"{ "id": "1", "name": "First", "playedAtMs": 5 },
           { "id": "x", "name": "Bad" },
           { "id": "1", "name": "Duplicate" },
           { "id": "2", "name": "   " }"#,
    );
    for id in 100..(100 + MAX_RECENT) {
        games.push_str(&format!(r#", {{ "id": "{id}", "name": "Filler {id}" }}"#));
    }
    std::fs::write(
        &path,
        format!(r#"{{ "schemaVersion": 1, "games": [ {games} ] }}"#),
    )
    .unwrap();
    let (loaded, warnings) = load_recent(&path).unwrap();
    assert_eq!(warnings.len(), 2);
    assert_eq!(loaded.len(), MAX_RECENT);
    assert_eq!(loaded.items()[0].name, "First");
    assert_eq!(loaded.items()[0].played_at_ms, 5);
    // A blank name falls back to the id until the catalog supplies one.
    assert_eq!(loaded.items()[1].name, "2");
}

#[test]
fn refresh_names_updates_present_and_keeps_missing() {
    let mut recent = RecentGames::default();
    recent.record("1", "Old name", 1).unwrap();
    recent.record("2", "Gone", 2).unwrap();
    let lookup = |id: &str| (id == "1").then_some("New name");
    assert!(recent.refresh_names(lookup));
    assert_eq!(recent.items()[1].name, "New name");
    assert_eq!(recent.items()[0].name, "Gone");
    assert!(!recent.refresh_names(lookup));
}
