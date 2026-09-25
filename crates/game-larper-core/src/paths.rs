use std::path::PathBuf;

/// Files under `%LOCALAPPDATA%\GameLarper`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub root: PathBuf,
}

impl AppPaths {
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

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
