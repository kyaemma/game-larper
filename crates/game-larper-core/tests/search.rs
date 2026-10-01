use game_larper_core::{DEFAULT_SEARCH_LIMIT, GameDefinition, search};

fn game(id: &str, name: &str) -> GameDefinition {
    GameDefinition::new(id, name).with_executable("game.exe", false)
}

#[test]
fn search_orders_exact_prefix_word_prefix_and_substring() {
    let mut games = vec![
        game("1", "The Elden Game"),
        game("2", "Eldenvale"),
        game("3", "ELDEN"),
        game("4", "Superelden"),
    ];
    let mut aliased = game("5", "Other");
    aliased.aliases = vec!["Elden World".into()];
    games.push(aliased);

    let ids: Vec<_> = search(&games, "  elden ", DEFAULT_SEARCH_LIMIT)
        .into_iter()
        .map(|index| games[index].id.as_str())
        .collect();
    assert_eq!(ids, ["3", "2", "5", "1", "4"]);
}

#[test]
fn empty_query_returns_nothing_and_limit_is_honored() {
    let games = vec![game("1", "Alpha"), game("2", "Alpine"), game("3", "Beta")];
    assert!(search(&games, "   ", 40).is_empty());
    assert_eq!(search(&games, "alp", 1).len(), 1);
}
