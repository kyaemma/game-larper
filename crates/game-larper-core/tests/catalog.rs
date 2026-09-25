use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use game_larper_core::{
    CATALOG_TTL, CatalogCache, ExecutableDefinition, GameDefinition, is_catalog_stale, merge_games,
    parse_catalog,
};

fn game(id: &str, name: &str) -> GameDefinition {
    GameDefinition {
        id: id.into(),
        name: name.into(),
        aliases: Vec::new(),
        executables: vec![ExecutableDefinition {
            name: "game.exe".into(),
            is_launcher: false,
        }],
        steam_app_id: None,
        icon_hash: None,
        cover_image_hash: None,
    }
}

#[test]
fn catalog_parses_current_discord_fields_and_ignores_unknown_fields() {
    let json = r#"[{"id":"1402418436809953330","name":"ELDEN RING","aliases":["Elden"],"future_field":42,
          "executables":[{"name":"game/eldenring.exe","os":"win32","is_launcher":false,"new":true},
                         {"name":"eldenring.app","os":"darwin"},{"name":"setup.exe","os":"win32","is_launcher":true}],
          "third_party_skus":[{"distributor":"steam","id":"1245620"}],"icon_hash":"abcdef"}]"#;
    let game = parse_catalog(json).unwrap().pop().unwrap();
    assert_eq!(game.id, "1402418436809953330");
    assert_eq!(game.steam_app_id.as_deref(), Some("1245620"));
    assert_eq!(
        game.supported_path(),
        Some(PathBuf::from("game").join("eldenring.exe"))
    );
    assert_eq!(game.aliases, vec!["Elden".to_string()]);
    assert_eq!(game.executables.len(), 2);

    let launcher_only = parse_catalog(
        r#"[{"id":"1","name":"Launcher","executables":[{"name":"setup.exe","os":"win32","is_launcher":true}]}]"#,
    )
    .unwrap()
    .pop()
    .unwrap();
    assert!(launcher_only.supported_path().is_none());

    let choices = GameDefinition {
        id: "2".into(),
        name: "Choices".into(),
        aliases: Vec::new(),
        executables: vec![
            ExecutableDefinition {
                name: "launcher.exe".into(),
                is_launcher: true,
            },
            ExecutableDefinition {
                name: "deep/game.exe".into(),
                is_launcher: false,
            },
            ExecutableDefinition {
                name: ">game.exe".into(),
                is_launcher: false,
            },
            ExecutableDefinition {
                name: "game.exe".into(),
                is_launcher: false,
            },
        ],
        steam_app_id: None,
        icon_hash: None,
        cover_image_hash: None,
    };
    assert_eq!(choices.supported_path(), Some(PathBuf::from("game.exe")));
}

#[test]
fn catalog_rejects_empty_or_malformed_responses() {
    assert!(matches!(
        parse_catalog("[]"),
        Err(game_larper_core::Error::Format(_))
    ));
    assert!(matches!(
        parse_catalog("{"),
        Err(game_larper_core::Error::Json(_))
    ));
    assert!(matches!(
        parse_catalog(r#"{"id":"1"}"#),
        Err(game_larper_core::Error::Format(_))
    ));
}

#[test]
fn excluded_basename_is_not_a_supported_path() {
    let game = GameDefinition {
        id: "9".into(),
        name: "Installer".into(),
        aliases: Vec::new(),
        executables: vec![
            ExecutableDefinition {
                name: "launcher.exe".into(),
                is_launcher: false,
            },
            ExecutableDefinition {
                name: "bin/game.exe".into(),
                is_launcher: false,
            },
        ],
        steam_app_id: None,
        icon_hash: None,
        cover_image_hash: None,
    };
    assert_eq!(
        game.supported_path(),
        Some(PathBuf::from("bin").join("game.exe"))
    );
}

#[test]
fn steam_id_is_the_first_short_numeric_steam_sku() {
    let json = r#"[{"id":"1","name":"Game","executables":[{"name":"game.exe","os":"win32"}],
        "third_party_skus":[{"distributor":"xbox","id":"ABCDEF"},{"distributor":"steam","id":""},
        {"distributor":"steam","id":"1245620"},{"distributor":"steam","id":"1"}]}]"#;
    let game = parse_catalog(json).unwrap().pop().unwrap();
    assert_eq!(game.steam_app_id.as_deref(), Some("1245620"));
}

#[test]
fn merge_fills_missing_metadata_and_keeps_the_first_name() {
    let mut first = game("7", "First");
    first.icon_hash = None;
    let mut second = game("7", "Second");
    second.steam_app_id = Some("55".into());
    second.icon_hash = Some("abc".into());
    second.aliases = vec!["Alias".into()];
    second.executables.push(ExecutableDefinition {
        name: "other.exe".into(),
        is_launcher: false,
    });
    let merged = merge_games(vec![first], vec![second]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].name, "First");
    assert_eq!(merged[0].steam_app_id.as_deref(), Some("55"));
    assert_eq!(merged[0].icon_hash.as_deref(), Some("abc"));
    assert_eq!(merged[0].aliases, vec!["Alias".to_string()]);
    assert_eq!(merged[0].executables.len(), 2);
}

#[test]
fn bad_refresh_retains_valid_cache() {
    let root = temp_dir();
    let cache = CatalogCache::new(root.join("catalog.json"));
    let good = r#"[{"id":"1","name":"Game","executables":[{"name":"game.exe","os":"win32"}]}]"#;
    cache.store_response(good).unwrap();
    assert!(cache.store_response("[]").is_err());
    assert!(cache
        .store_response(r#"[{"id":"2","name":"Launcher","executables":[{"name":"setup.exe","os":"win32","is_launcher":true}]}]"#)
        .is_err());
    let (loaded, warning) = cache.load();
    assert!(warning.is_none());
    assert_eq!(loaded.unwrap().len(), 1);
    assert_eq!(std::fs::read_to_string(cache.path()).unwrap(), good);
}

#[test]
fn corrupt_cache_does_not_panic() {
    let root = temp_dir();
    let path = root.join("catalog.json");
    std::fs::write(&path, "{not json").unwrap();
    let (loaded, warning) = CatalogCache::new(&path).load();
    assert!(loaded.is_none());
    assert!(warning.is_some());
}

#[test]
fn staleness_uses_a_24_hour_ttl() {
    let modified = UNIX_EPOCH + Duration::from_secs(1_000_000);
    assert!(!is_catalog_stale(Some(modified), modified + CATALOG_TTL));
    assert!(is_catalog_stale(
        Some(modified),
        modified + CATALOG_TTL + Duration::from_secs(1)
    ));
    assert!(is_catalog_stale(None, SystemTime::now()));
}

fn temp_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "game-larper-catalog-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}
