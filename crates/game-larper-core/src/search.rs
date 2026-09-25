use std::cmp::Ordering;

use crate::catalog::GameDefinition;

pub const DEFAULT_SEARCH_LIMIT: usize = 40;

pub fn normalize_query(text: &str) -> String {
    let mut normalized = String::new();
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
        for upper in character.to_uppercase() {
            normalized.push(upper);
        }
    }
    normalized
}

/// Ranked indexes into `games`. Empty query returns no rows.
pub fn search(games: &[GameDefinition], query: &str, limit: usize) -> Vec<usize> {
    let needle = normalize_query(query);
    if needle.is_empty() || limit == 0 {
        return Vec::new();
    }
    let mut ranked = Vec::new();
    for (index, game) in games.iter().enumerate() {
        if let Some(rank) = rank_game(game, &needle) {
            ranked.push((rank, index));
        }
    }
    ranked.sort_by(|left, right| {
        left.0.cmp(&right.0).then_with(|| {
            cmp_ignore_case(&games[left.1].name, &games[right.1].name).then(left.1.cmp(&right.1))
        })
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, index)| index)
        .collect()
}

fn rank_game(game: &GameDefinition, needle: &str) -> Option<u8> {
    std::iter::once(game.name.as_str())
        .chain(game.aliases.iter().map(String::as_str))
        .filter_map(|name| rank_name(name, needle))
        .min()
}

fn rank_name(name: &str, needle: &str) -> Option<u8> {
    let name = normalize_query(name);
    if name.is_empty() {
        return None;
    }
    if name == needle {
        Some(0)
    } else if name.starts_with(needle) {
        Some(1)
    } else if name.contains(&format!(" {needle}")) {
        Some(2)
    } else if name.contains(needle) {
        Some(3)
    } else {
        None
    }
}

fn cmp_ignore_case(left: &str, right: &str) -> Ordering {
    let left: String = left
        .chars()
        .flat_map(|character| character.to_uppercase())
        .collect();
    let right: String = right
        .chars()
        .flat_map(|character| character.to_uppercase())
        .collect();
    left.cmp(&right)
}
