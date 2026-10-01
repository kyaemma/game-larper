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
fn invalid_entries_are_skipped_without_failing_the_catalog() {
    let json = r#"[7, null, "text", [1, [2]], {}, true,
        {"id":"1","name":"Kept"},
        {"id":1,"name":"Numeric id"}, {"id":"x2","name":"Letters"}, {"id":"","name":"Empty id"},
        {"id":"3","name":"   "}, {"id":"4"}, {"name":"No id"},
        {"id":"5","name":" Also kept "}]"#;
    let games = parse_catalog(json).unwrap();
    let names: Vec<_> = games.iter().map(|game| game.name.as_str()).collect();
    assert_eq!(names, ["Kept", "Also kept"]);
}

#[test]
fn wrong_typed_fields_count_as_absent_and_unknown_fields_are_skipped() {
    let json = r#"[{"id":"1","name":"A","aliases":"not a list","executables":{"name":"a.exe"},
        "third_party_skus":"steam","icon_hash":5,"icon":"fallback","cover_image_hash":null,
        "future":{"deep":[1,2,{"x":null}],"n":1e30}}]"#;
    let game = parse_catalog(json).unwrap().pop().unwrap();
    assert!(game.aliases.is_empty());
    assert!(game.executables.is_empty());
    assert_eq!(game.steam_app_id, None);
    assert_eq!(game.icon_hash.as_deref(), Some("fallback"));

    let json = r#"[{"id":"2","name":"B","aliases":[1,"keep"," ",null,["x"],{"y":1},"also"],
        "executables":[3,null,"s",[1],{"os":"win32"},{"name":5,"os":"win32"},{"name":"a.exe","os":5},
                       {"name":"b.exe","os":"WIN32","is_launcher":"yes"},
                       {"name":"c.exe","os":"win32","is_launcher":true}],
        "third_party_skus":[3,{"distributor":"steam","id":"12345678901234"},{"distributor":"Steam","id":"77"},
                            {"distributor":"steam","id":"88"}]}]"#;
    let game = parse_catalog(json).unwrap().pop().unwrap();
    assert_eq!(game.aliases, ["keep", "also"]);
    let executables: Vec<_> = game
        .executables
        .iter()
        .map(|executable| (executable.name.as_str(), executable.is_launcher))
        .collect();
    assert_eq!(executables, [("b.exe", false), ("c.exe", true)]);
    assert_eq!(game.steam_app_id.as_deref(), Some("77"));
}

#[test]
fn repeated_and_escaped_keys_behave_like_a_json_object() {
    // The last value of a repeated key wins, even when it has the wrong type.
    let json = r#"[{"id":"1","name":"First","name":"Second"},
                   {"id":"2","name":"Third","id":9},
                   {"id":"3","name":"Café"}]"#;
    let games = parse_catalog(json).unwrap();
    let seen: Vec<_> = games
        .iter()
        .map(|game| (game.id.as_str(), game.name.as_str()))
        .collect();
    assert_eq!(seen, [("1", "Second"), ("3", "Café")]);
}

#[test]
fn only_an_array_is_a_catalog_and_only_bad_syntax_is_a_json_error() {
    for not_an_array in ["{}", r#"{"id":"1"}"#, "null", "5", "true", r#""text""#] {
        assert!(
            matches!(
                parse_catalog(not_an_array),
                Err(game_larper_core::Error::Format(_))
            ),
            "{not_an_array}"
        );
    }
    for broken in [
        "",
        r#"[{"id":"1","name":"A"}"#,
        r#"[{"id":"1","name":"A"}] trailing"#,
        r#"[{"id":"1","name":"A"},]"#,
        "{not json",
    ] {
        assert!(
            matches!(parse_catalog(broken), Err(game_larper_core::Error::Json(_))),
            "{broken}"
        );
    }
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
