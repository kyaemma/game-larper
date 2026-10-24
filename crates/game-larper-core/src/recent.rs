use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic::{self, quarantine_corrupt};
use crate::error::Error;
use crate::favorites::valid_application_id;

const SCHEMA_VERSION: u32 = 1;
const MAX_NAME_CHARS: usize = 200;
/// How many launches are kept. The tray panel shows fewer; the rest is headroom for later.
pub const MAX_RECENT: usize = 10;

/// A game whose runner started successfully. `name` is the last name seen in the catalog, so the
/// entry can still be shown while the catalog does not list the game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentGame {
    /// Discord application id.
    pub id: String,
    pub name: String,
    /// Wall-clock time of the launch, in milliseconds since the Unix epoch. Display only.
    pub played_at_ms: i64,
}

/// Recently launched games, newest first, one entry per game.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecentGames {
    items: Vec<RecentGame>,
}

impl RecentGames {
    pub fn items(&self) -> &[RecentGame] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Put `id` first. A game launched again moves up instead of appearing twice, and the
    /// oldest entries fall off past `MAX_RECENT`.
    pub fn record(&mut self, id: &str, name: &str, played_at_ms: i64) -> Result<(), Error> {
        if !valid_application_id(id) {
            return Err(Error::InvalidApplicationId);
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
            return Err(Error::Favorites("That game name cannot be saved."));
        }
        self.items.retain(|item| item.id != id);
        self.items.insert(
            0,
            RecentGame {
                id: id.to_string(),
                name: name.to_string(),
                played_at_ms,
            },
        );
        self.items.truncate(MAX_RECENT);
        Ok(())
    }

    /// Copy current catalog names into the list. Entries the catalog no longer lists keep their
    /// old name. Returns true when any name changed, so the caller knows to save.
    pub fn refresh_names<'a>(&mut self, lookup: impl Fn(&str) -> Option<&'a str>) -> bool {
        let mut changed = false;
        for item in &mut self.items {
            if let Some(name) = lookup(&item.id)
                && name != item.name
            {
                item.name = name.to_string();
                changed = true;
            }
        }
        changed
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecentSnapshot {
    #[serde(default = "current_schema")]
    schema_version: u32,
    #[serde(default)]
    games: Vec<RecentGameSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecentGameSnapshot {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    played_at_ms: i64,
}

fn current_schema() -> u32 {
    SCHEMA_VERSION
}

impl RecentSnapshot {
    /// Keeps what is valid and says what was dropped. Duplicates keep their first (newest)
    /// occurrence, and anything past `MAX_RECENT` is cut.
    fn into_recent(self) -> (RecentGames, Vec<String>) {
        let mut warnings = Vec::new();
        if self.schema_version > SCHEMA_VERSION {
            warnings.push(format!(
                "Recent games schema {} is newer than {SCHEMA_VERSION}. Unknown fields were ignored.",
                self.schema_version
            ));
        }
        let mut recent = RecentGames::default();
        let mut seen = HashSet::new();
        for game in self.games {
            if !valid_application_id(&game.id) {
                warnings.push("A recent game with a bad id was dropped.".into());
                continue;
            }
            if !seen.insert(game.id.clone()) {
                warnings.push(format!("Dropped a duplicate recent game ({}).", game.id));
                continue;
            }
            if recent.items.len() >= MAX_RECENT {
                continue;
            }
            // A missing or unusable name shows the id until the catalog supplies the real one.
            let trimmed = game.name.trim();
            let name = if trimmed.is_empty() || trimmed.chars().count() > MAX_NAME_CHARS {
                game.id.clone()
            } else {
                trimmed.to_string()
            };
            recent.items.push(RecentGame {
                id: game.id,
                name,
                played_at_ms: game.played_at_ms,
            });
        }
        (recent, warnings)
    }
}

/// Same contract as `load_favorites`: a missing file is the normal first run, a file that cannot
/// be read is an error and is left alone, and a file that cannot be parsed is moved aside.
///
/// An `Err` means a file is there that was not loaded, so the caller must not save over it.
pub fn load_recent(path: &Path) -> Result<(RecentGames, Vec<String>), String> {
    let json = match fs::read_to_string(path) {
        Ok(json) => json,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((RecentGames::default(), Vec::new()));
        }
        Err(error) => return Err(format!("Could not read recent games: {error}")),
    };
    match serde_json::from_str::<RecentSnapshot>(&json) {
        Ok(snapshot) => Ok(snapshot.into_recent()),
        Err(error) => {
            quarantine_corrupt(path);
            if path.exists() {
                return Err(format!(
                    "Could not load recent games, and the damaged file could not be moved aside: {error}"
                ));
            }
            Ok((
                RecentGames::default(),
                vec![format!(
                    "Could not load recent games, so they were reset: {error}"
                )],
            ))
        }
    }
}

pub fn save_recent(path: &Path, recent: &RecentGames) -> Result<(), Error> {
    let snapshot = RecentSnapshot {
        schema_version: SCHEMA_VERSION,
        games: recent
            .items
            .iter()
            .map(|item| RecentGameSnapshot {
                id: item.id.clone(),
                name: item.name.clone(),
                played_at_ms: item.played_at_ms,
            })
            .collect(),
    };
    let json = serde_json::to_vec_pretty(&snapshot)?;
    atomic::write_atomic(path, &json)?;
    Ok(())
}
