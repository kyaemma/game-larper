use std::ffi::OsString;
use std::path::PathBuf;

/// Files under one per-user root: `%LOCALAPPDATA%\GameLarper` on Windows,
/// `$XDG_DATA_HOME/GameLarper` (else `~/.local/share/GameLarper`) on Linux.
///
/// Linux keeps the Windows layout in a single data root on purpose: the runtime copies must
/// live somewhere persistent and executable, and "Open data folder" stays one folder. Cache and
/// state could move to `$XDG_CACHE_HOME` / `$XDG_STATE_HOME` later without touching callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub root: PathBuf,
}

impl AppPaths {
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[cfg(windows)]
    pub fn system() -> Self {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return Self::from_root(PathBuf::from(local).join("GameLarper"));
        }
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return Self::from_root(
                PathBuf::from(profile)
                    .join("AppData")
                    .join("Local")
                    .join("GameLarper"),
            );
        }
        Self::from_root(std::env::temp_dir().join("GameLarper"))
    }

    #[cfg(target_os = "linux")]
    pub fn system() -> Self {
        let root = xdg_data_home(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
            // No usable home at all: a private fallback is impossible to guarantee here, so the
            // app warns about this root at startup (see `AppPaths::is_fallback`).
            .unwrap_or_else(std::env::temp_dir);
        Self::from_root(root.join("GameLarper"))
    }

    /// True when the root is the last-resort temporary directory instead of a per-user one.
    pub fn is_fallback(&self) -> bool {
        self.root.starts_with(std::env::temp_dir())
    }

    pub fn config(&self) -> PathBuf {
        self.root.join("config.json")
    }

    pub fn queue(&self) -> PathBuf {
        self.root.join("queue.json")
    }

    pub fn catalog(&self) -> PathBuf {
        self.root.join("cache").join("catalog.json")
    }

    /// Previous C# build stored the catalog under this name.
    pub fn legacy_catalog(&self) -> PathBuf {
        self.root.join("cache").join("discord-detectables.json")
    }

    pub fn images(&self) -> PathBuf {
        self.root.join("cache").join("images")
    }

    pub fn runtime(&self) -> PathBuf {
        self.root.join("runtime")
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
}

/// The XDG base directory for user data. The specification requires absolute paths and says a
/// relative value is invalid and must be ignored; an empty value counts as unset.
/// <https://specifications.freedesktop.org/basedir-spec/latest/#variables>
pub fn xdg_data_home(xdg_data_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |value: Option<OsString>| {
        value
            .map(PathBuf::from)
            .filter(|path| path.has_root() && !path.as_os_str().is_empty())
    };
    absolute(xdg_data_home).or_else(|| absolute(home).map(|home| home.join(".local").join("share")))
}
