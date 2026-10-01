use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use game_larper_core::{
    CatalogCache, DETECTABLE_ENDPOINTS, Error as CoreError, GameDefinition, MAX_CATALOG_BYTES,
    USER_AGENT,
};
use image::ImageReader;

use crate::log::{Area, Log, redact};

const MAX_IMAGE_BYTES: usize = 1_000_000;
const MAX_IMAGE_EDGE: u32 = 1024;

pub fn refresh_catalog(cache: &CatalogCache, log: &Log) -> Result<Vec<GameDefinition>, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    let mut last_error = String::from("No detectable catalog endpoint responded.");
    for endpoint in DETECTABLE_ENDPOINTS {
        log.debug(Area::Network, format!("GET {endpoint}"));
        let started = Instant::now();
        let body = match fetch_text(&agent, endpoint) {
            Ok(body) => body,
            Err(error) => {
                log.warn(
                    Area::Network,
                    format!("{error} (after {} ms)", started.elapsed().as_millis()),
                );
                last_error = error;
                continue;
            }
        };
        log.debug(
            Area::Network,
            format!(
                "{endpoint}: {} KiB in {} ms",
                body.len() / 1024,
                started.elapsed().as_millis()
            ),
        );
        match cache.store_response(&body) {
            Ok(games) => {
                log.debug(
                    Area::Catalog,
                    format!(
                        "Parsed {} entries; saved {}",
                        games.len(),
                        redact(cache.path())
                    ),
                );
                return Ok(games);
            }
            Err(CoreError::Io(error)) => {
                log.error(
                    Area::Files,
                    format!("Saving {} failed: {error}", redact(cache.path())),
                );
                last_error = error.to_string();
            }
            Err(error) => {
                log.warn(
                    Area::Network,
                    format!("{endpoint}: response rejected: {error}"),
                );
                last_error = error.to_string();
            }
        }
    }
    Err(last_error)
}

fn fetch_text(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let response = agent
        .get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|error| format!("{url}: {error}"))?;
    let mut reader = response.into_reader().take((MAX_CATALOG_BYTES as u64) + 1);
    let mut buffer = Vec::new();
    reader
        .read_to_end(&mut buffer)
        .map_err(|error| format!("{url}: {error}"))?;
    if buffer.len() > MAX_CATALOG_BYTES {
        return Err(format!("{url}: catalog response is too large"));
    }
    String::from_utf8(buffer).map_err(|_| format!("{url}: catalog was not UTF-8"))
}

pub fn artwork_candidates(game: &GameDefinition) -> Vec<(String, String)> {
    let mut sources = Vec::new();
    if let Some(hash) = game.icon_hash.as_deref()
        && !hash.is_empty()
        && hash.chars().all(|character| character.is_ascii_hexdigit())
    {
        sources.push((
            format!("discord-{}-{hash}.png", game.id),
            format!(
                "https://cdn.discordapp.com/app-icons/{}/{hash}.png?size=128",
                game.id
            ),
        ));
    }
    if let Some(steam_id) = game.steam_app_id.as_deref() {
        sources.push((
            format!("steam-{steam_id}.jpg"),
            format!(
                "https://shared.fastly.steamstatic.com/store_item_assets/steam/apps/{steam_id}/capsule_231x87.jpg"
            ),
        ));
    }
    sources
}

/// What a game's artwork is fetched from. Two games sharing an id and an identity share a
/// picture; when the catalog changes the icon hash or Steam id, the identity changes and any
/// picture decoded for the old one is stale.
pub fn artwork_identity(game: &GameDefinition) -> String {
    artwork_candidates(game)
        .iter()
        .map(|(file_name, _)| file_name.as_str())
        .collect::<Vec<_>>()
        .join("|")
}

/// Return a cached image path. Network and decode failures try the next source.
///
/// Disk cache hits stay silent; downloads and failures are logged once per game.
pub fn ensure_artwork(directory: &Path, game: &GameDefinition, log: &Log) -> Option<PathBuf> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(12))
        .build();
    let candidates = artwork_candidates(game);
    for (file_name, url) in &candidates {
        let path = directory.join(file_name);
        if cached_image_ok(&path) {
            return Some(path);
        }
        let source = file_name.split('-').next().unwrap_or("artwork");
        let bytes = match download_limited(&agent, url) {
            Ok(bytes) => bytes,
            Err(error) => {
                log.debug(
                    Area::Network,
                    format!("{source} artwork for {} unavailable: {error}", game.name),
                );
                continue;
            }
        };
        if !image_bytes_ok(&bytes) {
            log.debug(
                Area::Art,
                format!(
                    "{source} artwork for {} is not a usable image ({} bytes)",
                    game.name,
                    bytes.len()
                ),
            );
            continue;
        }
        if let Err(error) = fs::create_dir_all(directory) {
            log.warn(
                Area::Files,
                format!("Artwork folder {} unavailable: {error}", redact(directory)),
            );
            return None;
        }
        match fs::write(&path, &bytes) {
            Ok(()) if cached_image_ok(&path) => {
                log.debug(
                    Area::Art,
                    format!(
                        "Cached {source} artwork for {} ({} KiB)",
                        game.name,
                        bytes.len().div_ceil(1024)
                    ),
                );
                return Some(path);
            }
            Ok(()) => log.warn(
                Area::Files,
                format!("{} did not read back as an image", redact(&path)),
            ),
            Err(error) => log.warn(
                Area::Files,
                format!("Writing {} failed: {error}", redact(&path)),
            ),
        }
    }
    if !candidates.is_empty() {
        log.debug(
            Area::Art,
            format!(
                "No artwork for {} ({}) after {} source(s)",
                game.name,
                game.id,
                candidates.len()
            ),
        );
    }
    None
}

fn download_limited(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
    let response = agent
        .get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|error| error.to_string())?;
    let mut reader = response.into_reader().take((MAX_IMAGE_BYTES as u64) + 1);
    let mut buffer = Vec::new();
    reader
        .read_to_end(&mut buffer)
        .map_err(|error| error.to_string())?;
    if buffer.len() > MAX_IMAGE_BYTES {
        return Err(format!("larger than {MAX_IMAGE_BYTES} bytes"));
    }
    Ok(buffer)
}

fn cached_image_ok(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_IMAGE_BYTES as u64 {
        return false;
    }
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    image_bytes_ok(&bytes)
}

fn image_bytes_ok(bytes: &[u8]) -> bool {
    let Ok(reader) = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format() else {
        return false;
    };
    let Ok(decoded) = reader.decode() else {
        return false;
    };
    decoded.width() > 0
        && decoded.height() > 0
        && decoded.width() <= MAX_IMAGE_EDGE
        && decoded.height() <= MAX_IMAGE_EDGE
}

pub fn clear_artwork(directory: &Path) -> Result<usize, String> {
    if !directory.exists() {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(directory).map_err(|error| error.to_string())?;
    if is_reparse(&metadata) {
        return Err("Artwork cache is a link. Nothing was deleted.".into());
    }
    let mut removed = 0;
    for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let meta = fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
        if meta.is_file() && !is_reparse(&meta) {
            fs::remove_file(entry.path()).map_err(|error| error.to_string())?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(windows)]
fn is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

/// Clearing through a symlink would delete files outside the cache.
#[cfg(target_os = "linux")]
fn is_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}
