//! Domain logic for Game Larper: catalog, search, and path safety.

#![deny(unsafe_op_in_unsafe_fn)]

mod atomic;
mod catalog;
mod error;
mod paths;
mod safe_path;
mod search;

pub use catalog::{
    CATALOG_TTL, CatalogCache, DETECTABLE_ENDPOINTS, ExecutableDefinition, GameDefinition,
    MAX_CATALOG_BYTES, USER_AGENT, is_catalog_stale, load_catalog_with_legacy, merge_games,
    parse_catalog,
};
pub use error::Error;
pub use paths::AppPaths;
pub use safe_path::{normalize_executable, resolve_executable};
pub use search::{DEFAULT_SEARCH_LIMIT, normalize_query, search};
