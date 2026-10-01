use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use game_larper_core::{
    CatalogCache, DETECTABLE_ENDPOINTS, Error as CoreError, GameDefinition, MAX_CATALOG_BYTES,
    USER_AGENT,
};
use image::{ImageFormat, ImageReader};

use crate::log::{Area, Log, redact};

const MAX_IMAGE_BYTES: usize = 1_000_000;
const MAX_IMAGE_EDGE: u32 = 1024;
/// A complete PNG ends with an IEND chunk. Looking for it in the last bytes catches a file cut
/// short without decoding any pixels.
const PNG_END_WINDOW: u64 = 64;
/// One reveal fetches its first rows in parallel plus a background worker, all to the same
/// two CDNs. Keeping that many idle connections per host lets the next search reuse them
/// instead of repeating TLS handshakes (ureq keeps only one per host by default).
const ARTWORK_IDLE_PER_HOST: usize = 12;

/// The agent for every artwork download. An `Agent` is a cheap handle onto a shared connection
/// pool and TLS configuration and is safe to use from any thread, so the parallel workers
/// share it instead of each building their own.
static ARTWORK_AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(12))
        .max_idle_connections_per_host(ARTWORK_IDLE_PER_HOST)
        .build()
});

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

/// What fetching one game's artwork needs: a few short strings instead of a catalog record, so
/// artwork workers and timers own their input and never share the catalog.
#[derive(Debug, Clone)]
pub struct ArtRequest {
    pub id: String,
    pub name: String,
    /// Only a usable hash: non-empty and hexadecimal.
    icon_hash: Option<String>,
    steam_app_id: Option<String>,
}

fn usable_icon_hash(game: &GameDefinition) -> Option<&str> {
    game.icon_hash.as_deref().filter(|hash| {
        !hash.is_empty() && hash.chars().all(|character| character.is_ascii_hexdigit())
    })
}

/// Whether the catalog names any place to fetch this game's artwork from.
pub fn has_artwork_source(game: &GameDefinition) -> bool {
    usable_icon_hash(game).is_some() || game.steam_app_id.is_some()
}

impl From<&GameDefinition> for ArtRequest {
    fn from(game: &GameDefinition) -> Self {
        Self {
            id: game.id.clone(),
            name: game.name.clone(),
            icon_hash: usable_icon_hash(game).map(str::to_string),
            steam_app_id: game.steam_app_id.clone(),
        }
    }
}

impl ArtRequest {
    pub fn has_source(&self) -> bool {
        self.icon_hash.is_some() || self.steam_app_id.is_some()
    }

    /// (cache file name, URL) of each source, best first.
    pub fn candidates(&self) -> Vec<(String, String)> {
        let mut sources = Vec::new();
        if let Some(hash) = self.icon_hash.as_deref() {
            sources.push((
                format!("discord-{}-{hash}.png", self.id),
                format!(
                    "https://cdn.discordapp.com/app-icons/{}/{hash}.png?size=128",
                    self.id
                ),
            ));
        }
        if let Some(steam_id) = self.steam_app_id.as_deref() {
            sources.push((
                format!("steam-{steam_id}.jpg"),
                format!(
                    "https://shared.fastly.steamstatic.com/store_item_assets/steam/apps/{steam_id}/capsule_231x87.jpg"
                ),
            ));
        }
        sources
    }

    /// What this artwork is fetched from. Two requests with the same id and identity yield the
    /// same picture; when the catalog changes the icon hash or Steam id, the identity changes
    /// and any picture decoded for the old one is stale. The candidate file names embed all of
    /// application id, icon hash and Steam id.
    pub fn identity(&self) -> String {
        self.candidates()
            .iter()
            .map(|(file_name, _)| file_name.as_str())
            .collect::<Vec<_>>()
            .join("|")
    }
}

/// Return a cached image path. Network and decode failures try the next source.
///
/// Disk cache hits stay silent; downloads and failures are logged once per game.
pub fn ensure_artwork(directory: &Path, game: &ArtRequest, log: &Log) -> Option<PathBuf> {
    let candidates = game.candidates();
    for (file_name, url) in &candidates {
        let path = directory.join(file_name);
        if cached_image_ok(&path) {
            return Some(path);
        }
        let source = file_name.split('-').next().unwrap_or("artwork");
        let bytes = match download_limited(&ARTWORK_AGENT, url) {
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
        // The bytes were just validated, so a clean write is all the cache needs.
        match write_cache_file(&path, &bytes) {
            Ok(()) => {
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

/// A cached file is usable when it is a complete, bounded PNG or JPEG. Only the header and the
/// end of the file are read; the pixels are decoded once, when the UI loads the picture.
fn cached_image_ok(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    metadata.is_file() && image_ok(file, metadata.len())
}

fn image_bytes_ok(bytes: &[u8]) -> bool {
    image_ok(Cursor::new(bytes), bytes.len() as u64)
}

/// Format, dimensions and completeness checks that never decode pixel data.
fn image_ok<R: Read + Seek>(mut source: R, len: u64) -> bool {
    if len == 0 || len > MAX_IMAGE_BYTES as u64 {
        return false;
    }
    let Ok(reader) = ImageReader::new(BufReader::new(&mut source)).with_guessed_format() else {
        return false;
    };
    let format = reader.format();
    if !matches!(format, Some(ImageFormat::Png | ImageFormat::Jpeg)) {
        return false;
    }
    let Ok((width, height)) = reader.into_dimensions() else {
        return false;
    };
    if width == 0 || height == 0 || width > MAX_IMAGE_EDGE || height > MAX_IMAGE_EDGE {
        return false;
    }
    format != Some(ImageFormat::Png) || png_is_complete(&mut source, len)
}

fn png_is_complete<R: Read + Seek>(source: &mut R, len: u64) -> bool {
    let window = len.min(PNG_END_WINDOW);
    let mut tail = vec![0; window as usize];
    source.seek(SeekFrom::Start(len - window)).is_ok()
        && source.read_exact(&mut tail).is_ok()
        && tail.windows(4).any(|chunk| chunk == b"IEND")
}

/// Write through a temporary file so a crash, or a reader on another thread, never sees a
/// partly written image under its final name.
fn write_cache_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SEQUENCE: AtomicU32 = AtomicU32::new(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = path.with_file_name(name);
    let result = fs::write(&temporary, bytes).and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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

#[cfg(test)]
mod tests {
    use super::*;

    fn game(icon_hash: Option<&str>, steam_app_id: Option<&str>) -> GameDefinition {
        let mut game = GameDefinition::new("123", "Game");
        game.icon_hash = icon_hash.map(str::to_string);
        game.steam_app_id = steam_app_id.map(str::to_string);
        game
    }

    fn encoded(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::new(width, height))
            .write_to(&mut bytes, format)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn complete_bounded_pngs_and_jpegs_are_accepted() {
        assert!(image_bytes_ok(&encoded(8, 8, ImageFormat::Png)));
        assert!(image_bytes_ok(&encoded(128, 128, ImageFormat::Png)));
        assert!(image_bytes_ok(&encoded(231, 87, ImageFormat::Jpeg)));
        assert!(image_bytes_ok(&encoded(
            MAX_IMAGE_EDGE,
            1,
            ImageFormat::Png
        )));
    }

    #[test]
    fn oversized_empty_corrupt_and_foreign_images_are_rejected() {
        let png = encoded(8, 8, ImageFormat::Png);
        assert!(!image_bytes_ok(&[]));
        assert!(!image_bytes_ok(b"not an image at all"));
        assert!(!image_bytes_ok(&encoded(
            MAX_IMAGE_EDGE + 1,
            1,
            ImageFormat::Png
        )));
        assert!(!image_bytes_ok(&encoded(
            1,
            MAX_IMAGE_EDGE + 1,
            ImageFormat::Jpeg
        )));
        assert!(!image_bytes_ok(b"GIF89a     ;"));
        assert!(!image_ok(Cursor::new(&png), MAX_IMAGE_BYTES as u64 + 1));
        // A PNG cut short loses its IEND chunk even though its header still reads.
        assert!(!image_bytes_ok(&png[..png.len() - 16]));
        assert!(!image_bytes_ok(&png[..png.len() / 2]));
        // Only the header is read, so a damaged JPEG body is the UI loader's to reject.
        assert!(!image_bytes_ok(&encoded(8, 8, ImageFormat::Jpeg)[..4]));
    }

    #[test]
    fn cached_files_are_validated_in_place() {
        let directory =
            std::env::temp_dir().join(format!("game-larper-net-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let good = directory.join("good.png");
        let cut = directory.join("cut.png");
        let png = encoded(16, 16, ImageFormat::Png);
        fs::write(&good, &png).unwrap();
        fs::write(&cut, &png[..png.len() - 20]).unwrap();
        assert!(cached_image_ok(&good));
        assert!(!cached_image_ok(&cut));
        assert!(!cached_image_ok(&directory.join("missing.png")));
        assert!(!cached_image_ok(&directory));
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn cache_writes_replace_atomically_and_leave_no_temporary_files() {
        let directory =
            std::env::temp_dir().join(format!("game-larper-write-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("art.png");
        write_cache_file(&target, b"first").unwrap();
        write_cache_file(&target, b"second").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second");
        let names: Vec<_> = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["art.png"]);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn requests_list_discord_then_steam_sources() {
        let request = ArtRequest::from(&game(Some("abc123"), Some("55")));
        let names: Vec<_> = request
            .candidates()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["discord-123-abc123.png", "steam-55.jpg"]);
        assert_eq!(request.identity(), "discord-123-abc123.png|steam-55.jpg");
        assert!(request.has_source());
    }

    #[test]
    fn unusable_icon_hashes_are_not_sources() {
        for hash in ["", "../x", "xyz", "ab cd"] {
            let game = game(Some(hash), None);
            assert!(!has_artwork_source(&game), "{hash:?}");
            let request = ArtRequest::from(&game);
            assert!(!request.has_source());
            assert!(request.candidates().is_empty());
            assert_eq!(request.identity(), "");
        }
        assert!(has_artwork_source(&game(Some("zz"), Some("5"))));
    }

    #[test]
    fn identity_changes_with_the_artwork_source() {
        let before = ArtRequest::from(&game(Some("aaaa"), Some("5"))).identity();
        assert_ne!(
            before,
            ArtRequest::from(&game(Some("bbbb"), Some("5"))).identity()
        );
        assert_ne!(
            before,
            ArtRequest::from(&game(Some("aaaa"), None)).identity()
        );
        assert_eq!(
            before,
            ArtRequest::from(&game(Some("aaaa"), Some("5"))).identity()
        );
    }
}
