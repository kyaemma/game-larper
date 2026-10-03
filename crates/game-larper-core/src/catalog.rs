use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::atomic;
use crate::catalog_json::Catalog;
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

/// A game Discord can detect, reduced to what Game Larper uses at runtime.
///
/// The catalog lists every executable rule of every platform for each game. Only one matters
/// here, so the best safe Windows path is chosen once, when the record is built or merged,
/// and the rest is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameDefinition {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub steam_app_id: Option<String>,
    pub icon_hash: Option<String>,
    pub(crate) executable: ExecutableChoice,
}

impl GameDefinition {
    /// A game with no executable rules yet; see [`GameDefinition::with_executable`].
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            aliases: Vec::new(),
            steam_app_id: None,
            icon_hash: None,
            executable: ExecutableChoice::default(),
        }
    }

    /// Record a Windows executable rule as the catalog lists it (`is_launcher` as given).
    pub fn with_executable(mut self, name: &str, is_launcher: bool) -> Self {
        self.executable.offer(name, is_launcher);
        self
    }

    /// Shallowest, then shortest, safe non-launcher Windows path Discord can match.
    pub fn supported_path(&self) -> Option<&Path> {
        self.executable.best.as_deref()
    }

    /// How many Windows executable rules the catalog listed. For diagnostics only: records
    /// merged from several providers add their counts, so shared rules count twice.
    pub fn executable_rules(&self) -> u32 {
        self.executable.rules
    }
}

/// The best executable among the Windows rules offered so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ExecutableChoice {
    pub(crate) best: Option<PathBuf>,
    pub(crate) rules: u32,
}

impl ExecutableChoice {
    /// Count a Windows rule and keep its path if it beats the current one. Launchers, unsafe
    /// paths and excluded basenames never qualify; of two equal candidates the first stays.
    pub(crate) fn offer(&mut self, name: &str, is_launcher: bool) {
        self.rules = self.rules.saturating_add(1);
        if is_launcher {
            return;
        }
        let Some(path) = normalize_executable(name) else {
            return;
        };
        if !is_excluded(&path) {
            self.consider(path);
        }
    }

    fn consider(&mut self, path: PathBuf) {
        if self
            .best
            .as_deref()
            .is_none_or(|best| preference(&path) < preference(best))
        {
            self.best = Some(path);
        }
    }

    /// Take over a later provider's choice; this one keeps a tie.
    fn merge(&mut self, other: Self) {
        if let Some(path) = other.best {
            self.consider(path);
        }
        self.rules = self.rules.saturating_add(other.rules);
    }
}

/// Lower is better: fewer directories, then a shorter path.
fn preference(path: &Path) -> (usize, usize) {
    (separator_count(path), path.as_os_str().len())
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
    let games = match serde_json::from_str(json)? {
        Catalog::Games(games) => games,
        Catalog::NotAnArray => {
            return Err(Error::Format(
                "The detectable catalog is not an array.".into(),
            ));
        }
    };
    if games.is_empty() {
        return Err(Error::Format(
            "The detectable catalog contains no valid games.".into(),
        ));
    }
    Ok(games)
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
    for alias in right.aliases {
        if !left
            .aliases
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&alias))
        {
            left.aliases.push(alias);
        }
    }
    left.executable.merge(right.executable);
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
