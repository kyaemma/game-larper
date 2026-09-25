use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::atomic;
use crate::error::Error;
use crate::safe_path::{normalize_executable, separator_count};

pub const DETECTABLE_ENDPOINTS: &[&str] = &[
    "https://discord.com/api/v10/applications/detectable",
    "https://discord.com/api/v9/applications/detectable",
];

pub const USER_AGENT: &str = "GameLarper/0.1";
pub const MAX_CATALOG_BYTES: usize = 40_000_000;
pub const CATALOG_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Basenames Discord's own detectable exclusion list ignores.
/// A game is unsupported when every safe Windows executable is on this list.
const EXCLUDED_BASENAMES: &[&str] = &[
    "brocrashreporter.exe",
    "config.exe",
    "crashreportclient.exe",
    "crosshairx.exe",
    "dxsetup.exe",
    "eaanticheat.gameservicelauncher.exe",
    "eaanticheat.installer.exe",
    "easyanticheat_setup.exe",
    "gamerangeroemsetup.exe",
    "install.exe",
    "launcher.exe",
    "launcherpatcher.exe",
    "modlauncher.exe",
    "pbsetup.exe",
    "proxyinstallshield.exe",
    "radiant_modtools.exe",
    "rockstar-games-launcher.exe",
    "sharex.exe",
    "start_protected_game.exe",
    "ue4prereqsetup_x64.exe",
    "ueprereqsetup_x64.exe",
    "ui32.exe",
    "unitycrashhandler64.exe",
    "vrmonitor.exe",
    "wallpaper64.exe",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableDefinition {
    pub name: String,
    pub is_launcher: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameDefinition {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub executables: Vec<ExecutableDefinition>,
    pub steam_app_id: Option<String>,
    pub icon_hash: Option<String>,
    pub cover_image_hash: Option<String>,
}

impl GameDefinition {
    /// Shallowest, then shortest, safe non-launcher Windows path Discord can match.
    pub fn supported_path(&self) -> Option<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .executables
            .iter()
            .filter(|executable| !executable.is_launcher)
            .filter_map(|executable| normalize_executable(&executable.name))
            .filter(|path| !is_excluded(path))
            .collect();
        paths.sort_by(|left, right| {
            separator_count(left)
                .cmp(&separator_count(right))
                .then_with(|| left.as_os_str().len().cmp(&right.as_os_str().len()))
        });
        paths.into_iter().next()
    }
}

fn is_excluded(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    let lower = name.to_ascii_lowercase();
    if EXCLUDED_BASENAMES.contains(&lower.as_str()) {
        return true;
    }
    lower.starts_with("vcredist") && lower.ends_with(".exe")
}

pub fn parse_catalog(json: &str) -> Result<Vec<GameDefinition>, Error> {
    let value: Value = serde_json::from_str(json)?;
    let Some(items) = value.as_array() else {
        return Err(Error::Format(
            "The detectable catalog is not an array.".into(),
        ));
    };
    let mut games = Vec::new();
    for item in items {
        let Some(game) = parse_game(item) else {
            continue;
        };
        games.push(game);
    }
    if games.is_empty() {
        return Err(Error::Format(
            "The detectable catalog contains no valid games.".into(),
        ));
    }
    Ok(games)
}

fn parse_game(value: &Value) -> Option<GameDefinition> {
    let object = value.as_object()?;
    let id = text(object, "id")?;
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let name = text(object, "name")?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let mut aliases = Vec::new();
    if let Some(items) = object.get("aliases").and_then(Value::as_array) {
        for alias in items {
            if let Some(alias) = alias.as_str()
                && !alias.trim().is_empty()
            {
                aliases.push(alias.to_string());
            }
        }
    }
    let mut executables = Vec::new();
    if let Some(items) = object.get("executables").and_then(Value::as_array) {
        for executable in items {
            let Some(executable) = executable.as_object() else {
                continue;
            };
            if !text(executable, "os").is_some_and(|os| os.eq_ignore_ascii_case("win32")) {
                continue;
            }
            let Some(path) = text(executable, "name") else {
                continue;
            };
            let is_launcher = executable.get("is_launcher").and_then(Value::as_bool) == Some(true);
            executables.push(ExecutableDefinition {
                name: path,
                is_launcher,
            });
        }
    }
    Some(GameDefinition {
        id,
        name,
        aliases,
        executables,
        steam_app_id: steam_app_id(object),
        icon_hash: text(object, "icon_hash").or_else(|| text(object, "icon")),
        cover_image_hash: text(object, "cover_image_hash"),
    })
}

fn steam_app_id(object: &serde_json::Map<String, Value>) -> Option<String> {
    let skus = object.get("third_party_skus")?.as_array()?;
    for sku in skus {
        let Some(sku) = sku.as_object() else {
            continue;
        };
        if !text(sku, "distributor").is_some_and(|name| name.eq_ignore_ascii_case("steam")) {
            continue;
        }
        let Some(id) = text(sku, "id") else {
            continue;
        };
        if !id.is_empty() && id.len() <= 12 && id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Some(id);
        }
    }
    None
}

fn text(object: &serde_json::Map<String, Value>, name: &str) -> Option<String> {
    object.get(name).and_then(Value::as_str).map(str::to_string)
}

/// Keep the first record for an id and fill gaps from later providers.
pub fn merge_games(
    mut base: Vec<GameDefinition>,
    incoming: impl IntoIterator<Item = GameDefinition>,
) -> Vec<GameDefinition> {
    let mut positions: HashMap<String, usize> = base
        .iter()
        .enumerate()
        .map(|(index, game)| (game.id.clone(), index))
        .collect();
    for game in incoming {
        if let Some(&index) = positions.get(&game.id) {
            merge_into(&mut base[index], game);
        } else {
            positions.insert(game.id.clone(), base.len());
            base.push(game);
        }
    }
    base
}

fn merge_into(left: &mut GameDefinition, right: GameDefinition) {
    if left.steam_app_id.is_none() {
        left.steam_app_id = right.steam_app_id;
    }
    if left.icon_hash.is_none() {
        left.icon_hash = right.icon_hash;
    }
    if left.cover_image_hash.is_none() {
        left.cover_image_hash = right.cover_image_hash;
    }
    for alias in right.aliases {
        if !left
            .aliases
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&alias))
        {
            left.aliases.push(alias);
        }
    }
    for executable in right.executables {
        if !left.executables.iter().any(|existing| {
            existing.name == executable.name && existing.is_launcher == executable.is_launcher
        }) {
            left.executables.push(executable);
        }
    }
}

pub fn is_catalog_stale(modified: Option<SystemTime>, now: SystemTime) -> bool {
    match modified {
        None => true,
        Some(modified) => now.duration_since(modified).unwrap_or(Duration::ZERO) > CATALOG_TTL,
    }
}

#[derive(Debug, Clone)]
pub struct CatalogCache {
    path: PathBuf,
}

impl CatalogCache {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn last_updated(&self) -> Option<SystemTime> {
        fs::metadata(&self.path)
            .and_then(|metadata| metadata.modified())
            .ok()
    }

    pub fn is_stale(&self, now: SystemTime) -> bool {
        is_catalog_stale(self.last_updated(), now)
    }

    pub fn load(&self) -> (Option<Vec<GameDefinition>>, Option<String>) {
        let json = match fs::read_to_string(&self.path) {
            Ok(json) => json,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (None, None),
            Err(error) => {
                return (
                    None,
                    Some(format!("Could not load cached catalog: {error}")),
                );
            }
        };
        match parse_catalog(&json) {
            Ok(games) => (Some(games), None),
            Err(error) => (
                None,
                Some(format!("Could not load cached catalog: {error}")),
            ),
        }
    }

    /// Parse `json` and replace the cache only when it contains a supported Windows game.
    pub fn store_response(&self, json: &str) -> Result<Vec<GameDefinition>, Error> {
        let games = parse_catalog(json)?;
        if !games.iter().any(|game| game.supported_path().is_some()) {
            return Err(Error::Format(
                "The catalog has no supported Windows games.".into(),
            ));
        }
        atomic::write_atomic(&self.path, json.as_bytes())?;
        Ok(games)
    }
}

pub fn load_catalog_with_legacy(
    current: &CatalogCache,
    legacy: &CatalogCache,
) -> (Option<Vec<GameDefinition>>, Option<String>) {
    let (games, warning) = current.load();
    if games.is_some() || current.path().exists() {
        return (games, warning);
    }
    legacy.load()
}
