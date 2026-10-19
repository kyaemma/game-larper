use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic::{self, quarantine_corrupt};
use crate::error::Error;

const SCHEMA_VERSION: u32 = 1;
const MAX_NAME_CHARS: usize = 200;

/// A game the user starred. `name` is the last name seen in the catalog, so the favorite can
/// still be shown and removed while the catalog does not list the game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Favorite {
    /// Discord application id.
    pub id: String,
    pub name: String,
}

/// Favorites in the order they were added.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Favorites {
    items: Vec<Favorite>,
}

impl Favorites {
    pub fn items(&self) -> &[Favorite] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.items.iter().any(|item| item.id == id)
    }

    /// `Ok(true)` when added, `Ok(false)` when it was already a favorite.
    pub fn add(&mut self, id: &str, name: &str) -> Result<bool, Error> {
        if !valid_application_id(id) {
            return Err(Error::InvalidApplicationId);
        }
        if self.contains(id) {
            return Ok(false);
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
            return Err(Error::Favorites("That game name cannot be saved."));
        }
        self.items.push(Favorite {
            id: id.to_string(),
            name: name.to_string(),
        });
        Ok(true)
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|item| item.id != id);
        self.items.len() != before
    }

    /// Copy current catalog names into the favorites. Favorites the catalog no longer lists keep
    /// their old name. Returns true when any name changed, so the caller knows to save.
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

/// Discord application ids are decimal digits. Anything else cannot be launched or matched.
fn valid_application_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FavoritesSnapshot {
    #[serde(default = "current_schema")]
    schema_version: u32,
    #[serde(default)]
    games: Vec<FavoriteSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FavoriteSnapshot {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

fn current_schema() -> u32 {
    SCHEMA_VERSION
}

impl FavoritesSnapshot {
    /// Keeps what is valid and says what was dropped. Duplicates keep their first occurrence.
    fn into_favorites(self) -> (Favorites, Vec<String>) {
        let mut warnings = Vec::new();
        if self.schema_version > SCHEMA_VERSION {
            warnings.push(format!(
                "Favorites schema {} is newer than {SCHEMA_VERSION}. Unknown fields were ignored.",
                self.schema_version
            ));
        }
        let mut favorites = Favorites::default();
        let mut seen = HashSet::new();
        for game in self.games {
            if !valid_application_id(&game.id) {
                warnings.push("A favorite with a bad id was dropped.".into());
                continue;
            }
            if !seen.insert(game.id.clone()) {
                warnings.push(format!("Dropped a duplicate favorite ({}).", game.id));
                continue;
            }
            // A missing or unusable name shows the id until the catalog supplies the real one.
            let trimmed = game.name.trim();
            let name = if trimmed.is_empty() || trimmed.chars().count() > MAX_NAME_CHARS {
                game.id.clone()
            } else {
                trimmed.to_string()
            };
            favorites.items.push(Favorite { id: game.id, name });
        }
        (favorites, warnings)
    }
}

/// Missing file is the normal first run. A file that is there but cannot be read is an error and
/// is left alone. A file that cannot be parsed is moved aside and the list starts empty.
///
/// An `Err` means a file is there that was not loaded, so the caller must not save over it.
pub fn load_favorites(path: &Path) -> Result<(Favorites, Vec<String>), String> {
    let json = match fs::read_to_string(path) {
        Ok(json) => json,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Favorites::default(), Vec::new()));
        }
        Err(error) => return Err(format!("Could not read favorites: {error}")),
    };
    match serde_json::from_str::<FavoritesSnapshot>(&json) {
        Ok(snapshot) => Ok(snapshot.into_favorites()),
        Err(error) => {
            quarantine_corrupt(path);
            if path.exists() {
                return Err(format!(
                    "Could not load favorites, and the damaged file could not be moved aside: {error}"
                ));
            }
            Ok((
                Favorites::default(),
                vec![format!(
                    "Could not load favorites, so they were reset: {error}"
                )],
            ))
        }
    }
}

pub fn save_favorites(path: &Path, favorites: &Favorites) -> Result<(), Error> {
    let snapshot = FavoritesSnapshot {
        schema_version: SCHEMA_VERSION,
        games: favorites
            .items
            .iter()
            .map(|item| FavoriteSnapshot {
                id: item.id.clone(),
                name: item.name.clone(),
            })
            .collect(),
    };
    let json = serde_json::to_vec_pretty(&snapshot)?;
    atomic::write_atomic(path, &json)?;
    Ok(())
}
