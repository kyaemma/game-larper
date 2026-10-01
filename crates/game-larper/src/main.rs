#![deny(unsafe_op_in_unsafe_fn)]
#![windows_subsystem = "windows"]

mod app;
mod art_cache;
mod host;
mod log;
mod net;
mod panel;
mod platform;

use std::process::ExitCode;
use std::sync::Arc;

use game_larper_core::AppPaths;

use crate::log::{Area, Log, redact};

#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("Game Larper supports Windows and Linux only.");

fn main() -> ExitCode {
    let minimized = std::env::args().any(|arg| arg.eq_ignore_ascii_case("--minimized"));
    let paths = AppPaths::system();
    let log = Arc::new(Log::new(paths.logs()));
    if !platform::claim_primary_instance(&paths, &log) {
        log.info(
            Area::App,
            "Another instance is already running; asked it to show its window",
        );
        return ExitCode::SUCCESS;
    }
    log_panics(&log);
    if let Some(session) = platform::describe_session() {
        log.debug(Area::App, session);
    }
    if paths.is_fallback() {
        log.warn(
            Area::Files,
            format!(
                "No per-user data folder could be found; using {}",
                redact(&paths.root)
            ),
        );
    }
    log.info(
        Area::App,
        format!(
            "Game Larper {} started (pid {})",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        ),
    );
    if let Err(error) = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .select()
    {
        log.error(Area::App, format!("UI backend failed: {error}"));
        return ExitCode::from(1);
    }
    match app::run(paths, log.clone(), minimized) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log.error(Area::App, format!("UI failed: {error}"));
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

/// A windowed app has no console, so panics go to the log file as well as stderr.
fn log_panics(log: &Arc<Log>) {
    let log = Arc::clone(log);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        log.error(
            Area::App,
            format!(
                "Panic on thread '{}': {info}",
                thread.name().unwrap_or("unnamed")
            ),
        );
        previous(info);
    }));
}
