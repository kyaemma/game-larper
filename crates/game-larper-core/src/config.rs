use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic::{self, quarantine_corrupt};
use crate::error::Error;

pub const SCHEMA_VERSION: u32 = 1;

fn default_true() -> bool {
    true
}

fn current_schema() -> u32 {
    SCHEMA_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    #[serde(default = "current_schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub launch_with_windows: bool,
    #[serde(default)]
    pub start_minimized: bool,
    #[serde(default = "default_true")]
    pub restore_last_selected_game: bool,
    #[serde(default)]
    pub auto_resume: bool,
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    #[serde(default = "default_true")]
    pub preserve_queue: bool,
    #[serde(default)]
    pub last_selected_discord_application_id: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            launch_with_windows: false,
            start_minimized: false,
            restore_last_selected_game: true,
            auto_resume: false,
            close_to_tray: true,
            preserve_queue: true,
            last_selected_discord_application_id: None,
        }
    }
}

impl AppConfig {
    pub fn normalized(mut self) -> Self {
        self.schema_version = SCHEMA_VERSION;
        self
    }
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> (AppConfig, Vec<String>) {
        let mut warnings = Vec::new();
        let json = match fs::read_to_string(&self.path) {
            Ok(json) => json,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (AppConfig::default(), warnings);
            }
            Err(error) => {
                warnings.push(format!("Could not load config: {error}"));
                return (AppConfig::default(), warnings);
            }
        };
        match serde_json::from_str::<AppConfig>(&json) {
            Ok(config) => {
                if config.schema_version > SCHEMA_VERSION {
                    warnings.push(format!(
                        "Config schema {} is newer than {SCHEMA_VERSION}. Unknown settings were ignored.",
                        config.schema_version
                    ));
                }
                (config.normalized(), warnings)
            }
            Err(error) => {
                warnings.push(format!("Could not load config: {error}"));
                quarantine_corrupt(&self.path);
                (AppConfig::default(), warnings)
            }
        }
    }

    pub fn save(&self, config: &AppConfig) -> Result<(), Error> {
        let config = config.clone().normalized();
        let json = serde_json::to_vec_pretty(&config)?;
        atomic::write_atomic(&self.path, &json)?;
        Ok(())
    }
}

/// Quoted per-user Run value. Rejects a path that already contains a quote.
pub fn format_startup_command(executable: &Path, start_minimized: bool) -> Result<String, Error> {
    let path = executable.to_string_lossy();
    if path.is_empty() || path.contains('"') {
        return Err(Error::UnsafePath(
            "The executable path cannot be quoted safely.",
        ));
    }
    let mut command = format!("\"{path}\"");
    if start_minimized {
        command.push_str(" --minimized");
    }
    Ok(command)
}
