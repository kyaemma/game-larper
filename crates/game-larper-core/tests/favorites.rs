use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use game_larper_core::{Error, Favorites, load_favorites, save_favorites};

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "game-larper-favorites-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn add_contains_and_remove() {
    let mut favorites = Favorites::default();
    assert!(favorites.is_empty());
    assert!(matches!(favorites.add("1", "ELDEN RING"), Ok(true)));
    assert!(favorites.contains("1"));
    assert!(!favorites.contains("2"));
    assert_eq!(favorites.len(), 1);
    assert!(favorites.remove("1"));
    assert!(!favorites.remove("1"));
    assert!(favorites.is_empty());
}

#[test]
fn duplicate_add_returns_false_and_keeps_the_first_name() {
    let mut favorites = Favorites::default();
    favorites.add("1", "ELDEN RING").unwrap();
    assert!(matches!(favorites.add("1", "Other"), Ok(false)));
    assert_eq!(favorites.len(), 1);
    assert_eq!(favorites.items()[0].name, "ELDEN RING");
}

#[test]
fn invalid_ids_are_rejected() {
    let mut favorites = Favorites::default();
    for id in ["", "12a", " 12", "-3", "1.5"] {
        assert!(
            matches!(favorites.add(id, "Game"), Err(Error::InvalidApplicationId)),
            "{id:?} should be rejected"
        );
    }
    assert!(favorites.is_empty());
}

#[test]
fn save_then_load_keeps_order_and_names() {
    let root = temp_dir("round-trip");
    let path = root.join("favorites.json");
    let mut favorites = Favorites::default();
    favorites.add("30", "Zelda").unwrap();
    favorites.add("10", "Alpha").unwrap();
    favorites.add("20", "Mid").unwrap();
    save_favorites(&path, &favorites).unwrap();
    let (loaded, warnings) = load_favorites(&path).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(loaded, favorites);
    let ids: Vec<&str> = loaded.items().iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids, ["30", "10", "20"]);
}

#[test]
fn many_favorites_round_trip() {
    let root = temp_dir("many");
    let path = root.join("favorites.json");
    let mut favorites = Favorites::default();
    for id in 1..=500 {
        assert!(matches!(
            favorites.add(&id.to_string(), &format!("Game {id}")),
            Ok(true)
        ));
    }
    save_favorites(&path, &favorites).unwrap();
    let (loaded, warnings) = load_favorites(&path).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(loaded.len(), 500);
    assert_eq!(loaded, favorites);
    let ids: Vec<String> = loaded.items().iter().map(|item| item.id.clone()).collect();
    let expected: Vec<String> = (1..=500).map(|id| id.to_string()).collect();
    assert_eq!(ids, expected);
}

#[test]
fn missing_file_loads_empty_without_warnings() {
    let root = temp_dir("missing");
    let (loaded, warnings) = load_favorites(&root.join("favorites.json")).unwrap();
    assert!(loaded.is_empty());
    assert!(warnings.is_empty());
}

#[test]
fn corrupt_file_is_moved_aside_and_loads_empty() {
    let root = temp_dir("corrupt");
    let path = root.join("favorites.json");
    std::fs::write(&path, "{not json").unwrap();
    let (loaded, warnings) = load_favorites(&path).unwrap();
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
                .starts_with("favorites.json.bad-")
        });
    assert!(quarantined);
}

#[test]
fn unreadable_file_is_an_error_and_left_in_place() {
    let root = temp_dir("unreadable");
    let path = root.join("favorites.json");
    // A directory cannot be read as a file, and it is not a missing file either.
    std::fs::create_dir_all(&path).unwrap();
    assert!(load_favorites(&path).is_err());
    assert!(path.is_dir());
}

#[test]
fn load_drops_invalid_and_duplicate_ids_with_warnings() {
    let root = temp_dir("invalid");
    let path = root.join("favorites.json");
    std::fs::write(
        &path,
        r#"{
            "schemaVersion": 1,
            "games": [
                { "id": "1", "name": "First" },
                { "id": "x", "name": "Bad" },
                { "id": "1", "name": "Duplicate" },
                { "id": "2", "name": "Second" }
            ]
        }"#,
    )
    .unwrap();
    let (loaded, warnings) = load_favorites(&path).unwrap();
    assert_eq!(warnings.len(), 2);
    let names: Vec<&str> = loaded
        .items()
        .iter()
        .map(|item| item.name.as_str())
        .collect();
    assert_eq!(names, ["First", "Second"]);
}

#[test]
fn blank_or_overlong_names_load_as_the_id() {
    let root = temp_dir("names");
    let path = root.join("favorites.json");
    let overlong = "x".repeat(201);
    std::fs::write(
        &path,
        format!(
            r#"{{ "schemaVersion": 1, "games": [
                {{ "id": "1", "name": "   " }},
                {{ "id": "2", "name": "{overlong}" }}
            ] }}"#
        ),
    )
    .unwrap();
    let (loaded, warnings) = load_favorites(&path).unwrap();
    assert!(warnings.is_empty());
    let names: Vec<&str> = loaded
        .items()
        .iter()
        .map(|item| item.name.as_str())
        .collect();
    assert_eq!(names, ["1", "2"]);
}

#[test]
fn refresh_names_updates_present_and_keeps_missing() {
    let mut favorites = Favorites::default();
    favorites.add("1", "Old name").unwrap();
    favorites.add("2", "Gone").unwrap();
    let lookup = |id: &str| (id == "1").then_some("New name");
    assert!(favorites.refresh_names(lookup));
    assert_eq!(favorites.items()[0].name, "New name");
    assert_eq!(favorites.items()[1].name, "Gone");
    assert!(!favorites.refresh_names(lookup));
}
