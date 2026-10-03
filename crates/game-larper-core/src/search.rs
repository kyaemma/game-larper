use std::cmp::Ordering;

use crate::catalog::GameDefinition;

pub const DEFAULT_SEARCH_LIMIT: usize = 40;

pub fn normalize_query(text: &str) -> String {
    let mut normalized = String::new();
    normalize_into(text, &mut normalized);
    normalized
}

/// Trim, collapse whitespace runs to one space, and uppercase, reusing `normalized`'s buffer.
fn normalize_into(text: &str, normalized: &mut String) {
    normalized.clear();
    let mut pending_space = false;
    for character in text.trim().chars() {
        if character.is_whitespace() {
            if !normalized.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            normalized.push(' ');
            pending_space = false;
        }
        // Most catalog names are ASCII, where uppercasing needs no case-mapping table.
        if character.is_ascii() {
            normalized.push(character.to_ascii_uppercase());
        } else {
            normalized.extend(character.to_uppercase());
        }
    }
}

/// Ranked indexes into `games`. Empty query returns no rows.
///
/// This runs on the UI thread for every keystroke, so it allocates per query, never per game:
/// names are normalized into one reused buffer, and only the best `limit` matches are sorted.
pub fn search(games: &[GameDefinition], query: &str, limit: usize) -> Vec<usize> {
    let needle = normalize_query(query);
    if needle.is_empty() || limit == 0 {
        return Vec::new();
    }
    let word_start = format!(" {needle}");
    let mut name = String::new();
    let mut ranked: Vec<(u8, usize)> = games
        .iter()
        .enumerate()
        .filter_map(|(index, game)| {
            rank_game(game, &needle, &word_start, &mut name).map(|rank| (rank, index))
        })
        .collect();
    let order = |left: &(u8, usize), right: &(u8, usize)| {
        left.0.cmp(&right.0).then_with(|| {
            cmp_ignore_case(&games[left.1].name, &games[right.1].name).then(left.1.cmp(&right.1))
        })
    };
    // The order is total (the index breaks every tie), so partitioning off the best `limit`
    // and sorting only those gives exactly the rows a full sort would.
    if ranked.len() > limit {
        ranked.select_nth_unstable_by(limit - 1, order);
        ranked.truncate(limit);
    }
    ranked.sort_unstable_by(order);
    ranked.into_iter().map(|(_, index)| index).collect()
}

/// The best rank of the game's name and aliases. `name` is scratch space.
fn rank_game(
    game: &GameDefinition,
    needle: &str,
    word_start: &str,
    name: &mut String,
) -> Option<u8> {
    let mut best = None;
    for candidate in std::iter::once(&game.name).chain(&game.aliases) {
        normalize_into(candidate, name);
        if let Some(rank) = rank_name(name, needle, word_start) {
            best = Some(best.map_or(rank, |best: u8| best.min(rank)));
        }
    }
    best
}

/// `name` is already normalized; `word_start` is the needle after a space.
fn rank_name(name: &str, needle: &str, word_start: &str) -> Option<u8> {
    if name.is_empty() {
        None
    } else if name == needle {
        Some(0)
    } else if name.starts_with(needle) {
        Some(1)
    } else if name.contains(word_start) {
        Some(2)
    } else if name.contains(needle) {
        Some(3)
    } else {
        None
    }
}

/// Compares the uppercased names without building them. Code point order is the byte order of
/// the UTF-8 strings, so this matches comparing the uppercased `String`s.
fn cmp_ignore_case(left: &str, right: &str) -> Ordering {
    left.chars()
        .flat_map(char::to_uppercase)
        .cmp(right.chars().flat_map(char::to_uppercase))
}
