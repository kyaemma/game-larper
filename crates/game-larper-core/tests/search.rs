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

#[test]
fn a_limit_keeps_exactly_the_best_rows_of_the_full_ranking() {
    let names = [
        "Zeta Alpha",
        "alpha",
        "Alpha Two",
        "beta alpha",
        "ALPHA",
        "Alphabet",
        "Gamma",
        "xalphax",
        "Alpha",
        "the alpha",
        "Alpha Zero",
        "alpha one",
    ];
    let games: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(index, name)| game(&index.to_string(), name))
        .collect();
    let full = search(&games, "alpha", usize::MAX);
    assert_eq!(full.len(), 11);
    for limit in 1..=full.len() {
        assert_eq!(
            search(&games, "alpha", limit),
            full[..limit],
            "limit {limit}"
        );
    }
}

#[test]
fn matching_and_ordering_follow_unicode_uppercase() {
    let games = vec![game("1", "Straße"), game("2", "Ölmühle"), game("3", "olm")];
    let ids = |query: &str| -> Vec<_> {
        search(&games, query, DEFAULT_SEARCH_LIMIT)
            .into_iter()
            .map(|index| games[index].id.as_str())
            .collect()
    };
    assert_eq!(ids("STRASSE"), ["1"]);
    assert_eq!(ids("ölm"), ["2"]);
    // "OLM" sorts before "ÖLMÜHLE" by code point, as the uppercased names would.
    assert_eq!(ids("lm"), ["3", "2"]);
}
