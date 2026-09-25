//! Domain logic for Game Larper: catalog, search, path safety, config, and session.

#![deny(unsafe_op_in_unsafe_fn)]

mod atomic;
mod catalog;
mod config;
mod error;
mod paths;
mod safe_path;
mod search;
mod session;

pub use catalog::{
    CATALOG_TTL, CatalogCache, DETECTABLE_ENDPOINTS, ExecutableDefinition, GameDefinition,
    MAX_CATALOG_BYTES, USER_AGENT, is_catalog_stale, load_catalog_with_legacy, merge_games,
    parse_catalog,
};
pub use config::{AppConfig, ConfigStore, SCHEMA_VERSION, format_startup_command};
pub use error::Error;
pub use paths::AppPaths;
pub use safe_path::{normalize_executable, resolve_executable};
pub use search::{DEFAULT_SEARCH_LIMIT, normalize_query, search};
pub use session::{SessionClock, SessionState, format_hms};
