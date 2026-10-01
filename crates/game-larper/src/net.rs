use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use game_larper_core::{
    CatalogCache, DETECTABLE_ENDPOINTS, GameDefinition, MAX_CATALOG_BYTES, USER_AGENT,
};
use image::ImageReader;

const MAX_IMAGE_BYTES: usize = 1_000_000;
const MAX_IMAGE_EDGE: u32 = 1024;

pub fn refresh_catalog(cache: &CatalogCache) -> Result<Vec<GameDefinition>, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    let mut last_error = String::from("No detectable catalog endpoint responded.");
    for endpoint in DETECTABLE_ENDPOINTS {
        match fetch_text(&agent, endpoint) {
            Ok(body) => match cache.store_response(&body) {
                Ok(games) => return Ok(games),
                Err(error) => last_error = error.to_string(),
            },
            Err(error) => last_error = error,
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

/// Return a cached image path. Network and decode failures try the next source.
pub fn ensure_artwork(directory: &Path, game: &GameDefinition) -> Option<PathBuf> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(12))
        .build();
    for (file_name, url) in artwork_candidates(game) {
        let path = directory.join(&file_name);
        if cached_image_ok(&path) {
            return Some(path);
        }
        let Ok(bytes) = download_limited(&agent, &url) else {
            continue;
        };
        if !image_bytes_ok(&bytes) {
            continue;
        }
        if fs::create_dir_all(directory).is_err() {
            return None;
        }
        if fs::write(&path, &bytes).is_ok() && cached_image_ok(&path) {
            return Some(path);
        }
    }
    None
}

fn download_limited(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, ()> {
    let response = agent
        .get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|_| ())?;
    let mut reader = response.into_reader().take((MAX_IMAGE_BYTES as u64) + 1);
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer).map_err(|_| ())?;
    if buffer.len() > MAX_IMAGE_BYTES {
        return Err(());
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

/// The artwork cache may not be a link: clearing it would follow the link out.
#[cfg(not(windows))]
fn is_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}
