#![deny(unsafe_op_in_unsafe_fn)]
#![windows_subsystem = "windows"]

mod app;
mod host;
mod log;
mod net;
mod panel;
mod platform;

use std::process::ExitCode;

use game_larper_core::AppPaths;

fn main() -> ExitCode {
    let minimized = std::env::args().any(|arg| arg.eq_ignore_ascii_case("--minimized"));
    if !platform::claim_primary_instance() {
        return ExitCode::SUCCESS;
    }
    let paths = AppPaths::system();
    let log = log::Log::new(paths.logs());
    log.info(
        log::Area::App,
        format!("Game Larper {} started", env!("CARGO_PKG_VERSION")),
    );
    if let Err(error) = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg".into())
        .select()
    {
        log.error(log::Area::App, format!("UI backend failed: {error}"));
        return ExitCode::from(1);
    }
    match app::run(paths, log, minimized) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}
