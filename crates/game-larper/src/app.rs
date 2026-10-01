use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

use chrono::{Local, NaiveDate, NaiveTime, TimeZone};
use game_larper_core::{
    AppConfig, AppPaths, CatalogCache, ConfigStore, DEFAULT_SEARCH_LIMIT, DEFAULT_TRANSITION_GAP,
    GameDefinition, QueueAction, QueueActivity, QueueItem, QueueMachine, SessionClock,
    SessionState, format_hms, load_catalog_with_legacy, load_queue, save_queue, unix_time_ms,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use crate::host::{self, ExitWatch, LaunchReport, RunnerHost};
use crate::log::{Area, Log, LogChanges, LogCursor, LogEntry, LogLevel, MAX_HISTORY, redact};
use crate::net;
use crate::panel::{Dock, Mode};
use crate::platform;

slint::include_modules!();

type Wake = Arc<dyn Fn() + Send + Sync>;

/// Rows whose artwork may hold back a new result list.
const REVEAL_ROWS: usize = 10;
/// Typing pause before artwork is fetched.
const ART_DEBOUNCE: Duration = Duration::from_millis(110);
/// Longest wait for artwork before rows show with the placeholder.
const REVEAL_WAIT: Duration = Duration::from_millis(260);
/// Keeps the startup loader from flashing for a single frame.
const LOADING_MIN: Duration = Duration::from_millis(280);
const CATALOG_STALE: Duration = Duration::from_secs(24 * 60 * 60);

struct Models {
    hits: Rc<VecModel<Hit>>,
    rows: Rc<VecModel<QueueRow>>,
    logs: Rc<VecModel<LogRow>>,
    /// How far `logs` has caught up with the live history.
    log_cursor: Cell<LogCursor>,
}

thread_local! {
    static MODELS: RefCell<Option<Models>> = const { RefCell::new(None) };
    static ART: RefCell<HashMap<String, slint::Image>> = RefCell::new(HashMap::new());
}

enum Msg {
    Loaded {
        games: Vec<GameDefinition>,
        updated: Option<SystemTime>,
    },
    Catalog(Result<Vec<GameDefinition>, String>),
    Art {
        id: String,
        path: Option<PathBuf>,
    },
    Launch(Result<LaunchReport, String>),
    Stopped(Result<(), String>),
    Exited {
        generation: u64,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Results {
    Idle,
    Pending,
    Ready,
    Empty,
}

/// A result list waiting for its first rows of artwork.
struct Reveal {
    shown: Vec<usize>,
    waiting: HashSet<String>,
    deadline: Instant,
}

/// Artwork a query wants: the first rows gate the reveal, the rest load quietly.
#[derive(Default)]
struct ArtPlan {
    first: Vec<GameDefinition>,
    rest: Vec<GameDefinition>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Info,
    Success,
    Error,
}

struct Session {
    paths: AppPaths,
    log: Arc<Log>,
    config: AppConfig,
    games: Vec<GameDefinition>,
    loaded: bool,
    started: Instant,
    catalog_updated: Option<SystemTime>,
    refreshing: bool,
    manual_refresh: bool,
    offline: bool,
    query: String,
    shown: Vec<usize>,
    results: Results,
    reveal: Option<Reveal>,
    selected: Option<String>,
    /// The game the runner was last launched for.
    active: Option<String>,
    clock: SessionClock,
    queue: QueueMachine,
    host: Arc<Mutex<RunnerHost>>,
    toast: String,
    toast_kind: ToastKind,
    toast_until: Option<Instant>,
    busy: bool,
    art_inflight: HashSet<String>,
    art_failed: HashSet<String>,
    restored: bool,
    quitting: bool,
}

pub fn run(
    paths: AppPaths,
    log: Arc<Log>,
    start_minimized_flag: bool,
) -> Result<(), slint::PlatformError> {
    log.debug(Area::App, format!("Data folder {}", redact(&paths.root)));
    let ui = MainWindow::new()?;
    let panel = SidePanel::new()?;
    let log_window = LogWindow::new()?;
    let tray = TrayIcon::new()?;
    let dock = Dock::new(&ui, &panel);
    let (tx, rx) = mpsc::channel();
    let rx = Arc::new(Mutex::new(rx));
    let session = Arc::new(Mutex::new(load_session(&paths, &log)));
    let hits = Rc::new(VecModel::from(Vec::<Hit>::new()));
    let rows = Rc::new(VecModel::from(Vec::<QueueRow>::new()));
    let logs = Rc::new(VecModel::from(Vec::<LogRow>::new()));
    ui.set_hits(ModelRc::from(hits.clone()));
    panel.set_queue_rows(ModelRc::from(rows.clone()));
    log_window.set_rows(ModelRc::from(logs.clone()));
    MODELS.with(|slot| {
        *slot.borrow_mut() = Some(Models {
            hits,
            rows,
            logs,
            log_cursor: Cell::default(),
        })
    });
    panel.set_version_label(env!("CARGO_PKG_VERSION").into());
    sync_settings_from_config(
        &panel,
        &session.lock().unwrap_or_else(|p| p.into_inner()).config,
    );

    // Drains worker results on the UI thread, then renders. Wake itself is handed to the
    // drain through a slot, because some results start more work.
    let wake_slot: Arc<Mutex<Option<Wake>>> = Arc::new(Mutex::new(None));
    let wake: Wake = {
        let rx = rx.clone();
        let session = session.clone();
        let ui_weak = ui.as_weak();
        let panel_weak = panel.as_weak();
        let tray_weak = tray.as_weak();
        let log = log.clone();
        let tx = tx.clone();
        let wake_slot = wake_slot.clone();
        Arc::new(move || {
            let ui_weak = ui_weak.clone();
            let panel_weak = panel_weak.clone();
            let tray_weak = tray_weak.clone();
            let session = session.clone();
            let rx = rx.clone();
            let log = log.clone();
            let tx = tx.clone();
            let wake_slot = wake_slot.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let (Some(ui), Some(panel), Some(tray)) =
                    (ui_weak.upgrade(), panel_weak.upgrade(), tray_weak.upgrade())
                else {
                    return;
                };
                let Some(wake) = wake_slot.lock().ok().and_then(|slot| slot.clone()) else {
                    return;
                };
                drain(&rx, &session, &log, &tx, &wake);
                render(&ui, &panel, &tray, &session);
                render_logs(&log);
            });
        })
    };
    if let Ok(mut slot) = wake_slot.lock() {
        *slot = Some(wake.clone());
    }

    wire(
        &ui,
        &panel,
        &log_window,
        &tray,
        &dock,
        &session,
        &log,
        &tx,
        &wake,
    );
    platform::watch_activation({
        let ui_weak = ui.as_weak();
        let log = log.clone();
        move || {
            log.info(
                Area::App,
                "A second launch asked this instance to show its window",
            );
            let ui_weak = ui_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    show_main(&ui);
                }
            });
        }
    });

    render(&ui, &panel, &tray, &session);
    render_logs(&log);
    let start_minimized_setting = session
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .config
        .start_minimized;
    let minimized = start_minimized_flag || start_minimized_setting;
    if minimized {
        log.info(
            Area::App,
            if start_minimized_flag {
                "Starting minimized to the tray (--minimized)"
            } else {
                "Starting minimized to the tray (setting)"
            },
        );
    } else {
        ui.show()?;
        style_main(&ui);
    }
    spawn_load(tx.clone(), paths.clone(), log.clone(), wake.clone());

    let timer = Timer::default();
    {
        let session = session.clone();
        let ui_weak = ui.as_weak();
        let panel_weak = panel.as_weak();
        let tray_weak = tray.as_weak();
        let tx = tx.clone();
        let log = log.clone();
        let wake = wake.clone();
        let rx = rx.clone();
        timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
            let (Some(ui), Some(panel), Some(tray)) =
                (ui_weak.upgrade(), panel_weak.upgrade(), tray_weak.upgrade())
            else {
                return;
            };
            drain(&rx, &session, &log, &tx, &wake);
            tick(&session, &log, &tx, &wake);
            render(&ui, &panel, &tray, &session);
            render_logs(&log);
        });
    }

    ui.window().on_close_requested({
        let session = session.clone();
        let ui_weak = ui.as_weak();
        let dock = dock.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            let close_to_tray = session
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .config
                .close_to_tray;
            if close_to_tray {
                log.debug(Area::App, "Main window closed to the tray");
                dock.dismiss();
                if let Some(ui) = ui_weak.upgrade() {
                    let _ = ui.hide();
                }
            } else {
                begin_quit(&session, &log, &tx, &wake);
            }
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    panel.window().on_close_requested({
        let dock = dock.clone();
        move || {
            dock.close();
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    // The console is created once and only hidden, so reopening shows the same window.
    log_window.window().on_close_requested({
        let log_window = log_window.as_weak();
        let log = log.clone();
        move || {
            if let Some(log_window) = log_window.upgrade() {
                hide_log_window(&log_window, &log);
            }
            slint::CloseRequestResponse::KeepWindowShown
        }
    });

    log.debug(Area::App, "Event loop running");
    slint::run_event_loop()?;
    log.info(Area::App, "Event loop ended; releasing the runner");
    if let Ok(mut host) = session
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .host
        .lock()
    {
        let _ = host.stop();
    }
    log.info(Area::App, "Game Larper stopped");
    Ok(())
}

/// Config and queue load at once. The catalog is large, so it loads on a worker.
fn load_session(paths: &AppPaths, log: &Arc<Log>) -> Session {
    let (config, warnings) = ConfigStore::new(paths.config()).load();
    for warning in warnings {
        log.warn(Area::Settings, warning);
    }
    log.debug(
        Area::Settings,
        format!("Settings: {}", describe_settings(&config)),
    );
    let now_ms = unix_time_ms(SystemTime::now());
    let (queue, queue_warnings) = if config.preserve_queue {
        load_queue(&paths.queue(), now_ms, DEFAULT_TRANSITION_GAP).unwrap_or_else(|error| {
            (
                QueueMachine::new(DEFAULT_TRANSITION_GAP),
                vec![error.to_string()],
            )
        })
    } else {
        (QueueMachine::new(DEFAULT_TRANSITION_GAP), Vec::new())
    };
    for warning in queue_warnings {
        log.warn(Area::Queue, warning);
    }
    if !queue.items().is_empty() {
        log.debug(
            Area::Queue,
            format!(
                "Restored {} queued item(s), {}",
                queue.items().len(),
                describe_activity(&queue.activity())
            ),
        );
    }
    Session {
        paths: paths.clone(),
        log: log.clone(),
        config,
        games: Vec::new(),
        loaded: false,
        started: Instant::now(),
        catalog_updated: None,
        refreshing: false,
        manual_refresh: false,
        offline: false,
        query: String::new(),
        shown: Vec::new(),
        results: Results::Idle,
        reveal: None,
        selected: None,
        active: None,
        clock: SessionClock::new(),
        queue,
        host: Arc::new(Mutex::new(RunnerHost::new(paths.runtime(), log.clone()))),
        toast: String::new(),
        toast_kind: ToastKind::Info,
        toast_until: None,
        busy: false,
        art_inflight: HashSet::new(),
        art_failed: HashSet::new(),
        restored: false,
        quitting: false,
    }
}

fn spawn_load(tx: mpsc::Sender<Msg>, paths: AppPaths, log: Arc<Log>, wake: Wake) {
    std::thread::spawn(move || {
        let started = Instant::now();
        let current = CatalogCache::new(paths.catalog());
        let legacy = CatalogCache::new(paths.legacy_catalog());
        if current.path().exists() {
            log.debug(
                Area::Catalog,
                format!("Loading cached database {}", redact(current.path())),
            );
        } else if legacy.path().exists() {
            log.info(
                Area::Catalog,
                format!(
                    "No current cache; falling back to the legacy {}",
                    redact(legacy.path())
                ),
            );
        }
        let (games, warning) = load_catalog_with_legacy(&current, &legacy);
        if let Some(warning) = warning {
            log.warn(Area::Catalog, warning);
        }
        let parsed = games.as_ref().map(Vec::len);
        let games: Vec<GameDefinition> = games
            .unwrap_or_default()
            .into_iter()
            .filter(|game| game.supported_path().is_some())
            .collect();
        let updated = current.last_updated().or_else(|| legacy.last_updated());
        match parsed {
            Some(parsed) => log.info(
                Area::Catalog,
                format!(
                    "Loaded {} supported games from {} entries in {} ms ({})",
                    group_digits(games.len()),
                    group_digits(parsed),
                    started.elapsed().as_millis(),
                    catalog_age(updated)
                ),
            ),
            None => log.info(Area::Catalog, "No cached game database yet"),
        }
        let _ = tx.send(Msg::Loaded { games, updated });
        wake();
    });
}

fn drain(
    rx: &Mutex<mpsc::Receiver<Msg>>,
    session: &Arc<Mutex<Session>>,
    log: &Arc<Log>,
    tx: &mpsc::Sender<Msg>,
    wake: &Wake,
) {
    let mut batch = Vec::new();
    if let Ok(rx) = rx.lock() {
        while let Ok(message) = rx.try_recv() {
            batch.push(message);
        }
    }
    for message in batch {
        apply_message(session, log, tx, wake, message);
    }
}

#[allow(clippy::too_many_arguments)]
fn wire(
    ui: &MainWindow,
    panel: &SidePanel,
    log_window: &LogWindow,
    tray: &TrayIcon,
    dock: &Rc<Dock>,
    session: &Arc<Mutex<Session>>,
    log: &Arc<Log>,
    tx: &mpsc::Sender<Msg>,
    wake: &Wake,
) {
    let debounce = Rc::new(Timer::default());
    ui.on_query_edited({
        let session = session.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move |text| {
            let plan = session.lock().ok().map(|mut session| {
                session.query = text.to_string();
                plan_results(&mut session)
            });
            match plan {
                Some(plan) if !plan.first.is_empty() || !plan.rest.is_empty() => {
                    let session = session.clone();
                    let tx = tx.clone();
                    let wake = wake.clone();
                    debounce.start(TimerMode::SingleShot, ART_DEBOUNCE, move || {
                        start_art(&session, &tx, &wake, plan.first.clone(), plan.rest.clone());
                    });
                }
                _ => debounce.stop(),
            }
            wake();
        }
    });
    ui.on_step({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move |delta| {
            let next = session.lock().ok().and_then(|session| {
                let last = session.shown.len().checked_sub(1)?;
                let current = session.selected.as_ref().and_then(|id| {
                    session
                        .shown
                        .iter()
                        .position(|&index| session.games.get(index).is_some_and(|g| &g.id == id))
                });
                Some(match current {
                    Some(position) => position.saturating_add_signed(delta as isize).min(last),
                    None if delta < 0 => last,
                    None => 0,
                })
            });
            if let Some(index) = next {
                select_index(&session, &log, &tx, &wake, index);
            }
            wake();
        }
    });
    ui.on_choose({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move |index| {
            select_index(&session, &log, &tx, &wake, index as usize);
            wake();
        }
    });
    ui.on_activate({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move |index| {
            select_index(&session, &log, &tx, &wake, index as usize);
            begin_play(&session, &log, &tx, &wake);
            wake();
        }
    });
    let play = {
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            if let Ok(mut session) = session.lock() {
                reselect_paused(&mut session, &log);
            }
            begin_play(&session, &log, &tx, &wake);
            wake();
        }
    };
    let pause = {
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            begin_pause(&session, &log, &tx, &wake);
            wake();
        }
    };
    let stop = {
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            begin_stop(&session, &log, &tx, &wake, true);
            wake();
        }
    };
    let queue_resume = {
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            let action = session.lock().ok().map(|mut session| {
                let action = session.queue.resume(Instant::now());
                if let Some(index) = action.launch_index {
                    session
                        .log
                        .info(Area::Queue, format!("Resuming item {}", index + 1));
                }
                action
            });
            if let Some(action) = action {
                dispatch_queue_action(&session, &log, &tx, &wake, action);
            }
            wake();
        }
    };
    ui.on_play(play.clone());
    ui.on_pause(pause.clone());
    ui.on_stop(stop.clone());
    ui.on_queue_resume(queue_resume.clone());
    tray.on_play(play);
    tray.on_pause(pause);
    tray.on_stop(stop);

    let open_queue = {
        let dock = dock.clone();
        let panel = panel.as_weak();
        move || {
            if let Some(panel) = panel.upgrade()
                && panel.get_schedule_date().is_empty()
            {
                panel.set_schedule_date(Local::now().format("%Y-%m-%d").to_string().into());
            }
            dock.open(Mode::Queue);
        }
    };
    let open_settings = {
        let dock = dock.clone();
        let panel = panel.as_weak();
        let session = session.clone();
        move || {
            if let (Some(panel), Ok(session)) = (panel.upgrade(), session.lock()) {
                sync_settings_from_config(&panel, &session.config);
            }
            dock.open(Mode::Settings);
        }
    };
    ui.on_toggle_queue({
        let dock = dock.clone();
        let open_queue = open_queue.clone();
        move || {
            if dock.mode() == Some(Mode::Queue) {
                dock.close();
            } else {
                open_queue();
            }
        }
    });
    ui.on_open_settings({
        let dock = dock.clone();
        let open_settings = open_settings.clone();
        move || {
            if dock.mode() == Some(Mode::Settings) {
                dock.close();
            } else {
                open_settings();
            }
        }
    });
    ui.on_add_to_queue({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        let panel = panel.as_weak();
        move |index| {
            select_index(&session, &log, &tx, &wake, index as usize);
            add_selected_to_queue(&session, &panel);
            wake();
        }
    });
    ui.on_copy_text({
        let session = session.clone();
        let wake = wake.clone();
        move |value, label| {
            if let Ok(mut session) = session.lock() {
                match platform::copy_text(&value) {
                    Ok(()) => toast_ok(&mut session, format!("{label} copied")),
                    Err(error) => toast_err(&mut session, error),
                }
            }
            wake();
        }
    });
    ui.on_notify({
        let session = session.clone();
        let wake = wake.clone();
        move |message| {
            if let Ok(mut session) = session.lock() {
                toast_err(&mut session, message);
            }
            wake();
        }
    });
    let refresh = {
        let session = session.clone();
        let tx = tx.clone();
        let log = log.clone();
        let wake = wake.clone();
        move || {
            let paths = session.lock().ok().and_then(|mut session| {
                if session.refreshing {
                    return None;
                }
                session.refreshing = true;
                session.manual_refresh = true;
                Some(session.paths.clone())
            });
            if let Some(paths) = paths {
                log.info(Area::Catalog, "Manual refresh requested");
                spawn_refresh(tx.clone(), paths, log.clone(), wake.clone());
            } else {
                log.debug(Area::Catalog, "Refresh ignored: one is already running");
            }
            wake();
        }
    };
    ui.on_refresh_catalog(refresh.clone());
    ui.on_request_close({
        let session = session.clone();
        let ui = ui.as_weak();
        let dock = dock.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            let close_to_tray = session
                .lock()
                .map(|session| session.config.close_to_tray)
                .unwrap_or(true);
            if close_to_tray {
                log.debug(Area::App, "Main window closed to the tray");
                dock.dismiss();
                if let Some(ui) = ui.upgrade() {
                    let _ = ui.hide();
                }
            } else {
                begin_quit(&session, &log, &tx, &wake);
            }
        }
    });

    panel.on_close_panel({
        let dock = dock.clone();
        move || dock.close()
    });
    panel.on_save_settings({
        let session = session.clone();
        let panel = panel.as_weak();
        let dock = dock.clone();
        let log = log.clone();
        let wake = wake.clone();
        move || {
            if save_settings(&session, &panel, &log) {
                dock.close();
            }
            wake();
        }
    });
    panel.on_refresh_catalog(refresh);
    panel.on_clear_images({
        let session = session.clone();
        let log = log.clone();
        let wake = wake.clone();
        move || {
            if let Ok(mut session) = session.lock() {
                match net::clear_artwork(&session.paths.images()) {
                    Ok(count) => {
                        ART.with(|art| art.borrow_mut().clear());
                        session.art_failed.clear();
                        log.info(Area::Art, format!("Cleared {count} cached images"));
                        toast_ok(&mut session, format!("Cleared {count} cached images."));
                    }
                    Err(error) => {
                        log.error(Area::Art, format!("Artwork cache clear failed: {error}"));
                        toast_err(&mut session, error);
                    }
                }
            }
            wake();
        }
    });
    panel.on_open_logs({
        let log_window = log_window.as_weak();
        let log = log.clone();
        move || {
            if let Some(log_window) = log_window.upgrade() {
                show_log_window(&log_window, &log);
            }
        }
    });
    log_window.on_close_requested({
        let log_window = log_window.as_weak();
        let log = log.clone();
        move || {
            if let Some(log_window) = log_window.upgrade() {
                hide_log_window(&log_window, &log);
            }
        }
    });
    log_window.on_copy({
        let session = session.clone();
        let log = log.clone();
        let wake = wake.clone();
        move || {
            let text = log.history_text();
            if text.is_empty() {
                return;
            }
            if let Ok(mut session) = session.lock() {
                match platform::copy_text(&text) {
                    Ok(()) => {
                        log.debug(
                            Area::App,
                            format!("Copied {} console lines", text.lines().count()),
                        );
                        toast_ok(&mut session, "Logs copied");
                    }
                    Err(error) => {
                        log.error(Area::App, format!("Copying the console failed: {error}"));
                        toast_err(&mut session, error);
                    }
                }
            }
            wake();
        }
    });
    log_window.on_clear({
        let log = log.clone();
        move || {
            log.clear_history();
            render_logs(&log);
        }
    });
    log_window.on_open_folder({
        let session = session.clone();
        let log = log.clone();
        move || {
            if let Ok(session) = session.lock()
                && let Err(error) = platform::open_in_explorer(&session.paths.logs())
            {
                log.error(
                    Area::Files,
                    format!("Opening the log folder failed: {error}"),
                );
            }
        }
    });
    panel.on_open_data({
        let session = session.clone();
        move || {
            if let Ok(session) = session.lock()
                && let Err(error) = platform::open_in_explorer(&session.paths.root)
            {
                session.log.error(
                    Area::Files,
                    format!("Opening the data folder failed: {error}"),
                );
            }
        }
    });
    panel.on_add_queue({
        let session = session.clone();
        let panel = panel.as_weak();
        let wake = wake.clone();
        move || {
            add_selected_to_queue(&session, &panel);
            wake();
        }
    });
    panel.on_queue_up({
        let session = session.clone();
        let wake = wake.clone();
        move |index| {
            if index > 0 {
                move_queue(&session, index as usize, index as usize - 1);
            }
            wake();
        }
    });
    panel.on_queue_down({
        let session = session.clone();
        let wake = wake.clone();
        move |index| {
            move_queue(&session, index as usize, index as usize + 1);
            wake();
        }
    });
    panel.on_queue_remove({
        let session = session.clone();
        let wake = wake.clone();
        move |index| {
            if let Ok(mut session) = session.lock() {
                match session.queue.remove(index as usize) {
                    Ok(item) => {
                        session.log.info(
                            Area::Queue,
                            format!(
                                "Removed item {} ({}); {} left",
                                index + 1,
                                item.name,
                                session.queue.items().len()
                            ),
                        );
                        persist_queue(&mut session);
                    }
                    Err(error) => {
                        session
                            .log
                            .warn(Area::Queue, format!("Remove refused: {error}"));
                        toast_err(&mut session, error.to_string());
                    }
                }
            }
            wake();
        }
    });
    panel.on_queue_clear({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            let action = session.lock().ok().map(|mut session| {
                let count = session.queue.items().len();
                let action = session.queue.clear();
                session
                    .log
                    .info(Area::Queue, format!("Cleared {count} item(s)"));
                persist_queue(&mut session);
                action
            });
            if let Some(action) = action {
                dispatch_queue_action(&session, &log, &tx, &wake, action);
            }
            wake();
        }
    });
    panel.on_queue_start({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            queue_command(&session, &log, &tx, &wake, "Queue start", |queue| {
                queue.start_now(Instant::now())
            });
            wake();
        }
    });
    panel.on_queue_pause({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            let action = session.lock().ok().map(|mut session| {
                let action = session.queue.pause(Instant::now());
                if action.stop_runner {
                    if let QueueActivity::Paused { index } = session.queue.activity() {
                        session
                            .log
                            .info(Area::Queue, format!("Paused on item {}", index + 1));
                    }
                    session.clock.pause(Instant::now());
                }
                action
            });
            if let Some(action) = action {
                dispatch_queue_action(&session, &log, &tx, &wake, action);
            }
            wake();
        }
    });
    panel.on_queue_resume(queue_resume);
    panel.on_queue_skip({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            let action = session.lock().ok().map(|mut session| {
                let from = session.queue.active_index();
                let action = session.queue.skip(Instant::now());
                if let Some(from) = from {
                    session.log.info(
                        Area::Queue,
                        format!(
                            "Skipped item {}; now {}",
                            from + 1,
                            describe_activity(&session.queue.activity())
                        ),
                    );
                }
                action
            });
            if let Some(action) = action {
                dispatch_queue_action(&session, &log, &tx, &wake, action);
            }
            wake();
        }
    });
    panel.on_queue_stop({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            begin_stop(&session, &log, &tx, &wake, false);
            wake();
        }
    });
    panel.on_arm_schedule({
        let session = session.clone();
        let panel = panel.as_weak();
        let wake = wake.clone();
        move || {
            arm_schedule(&session, &panel);
            wake();
        }
    });
    panel.on_disarm_schedule({
        let session = session.clone();
        let wake = wake.clone();
        move || {
            if let Ok(mut session) = session.lock() {
                if let Err(error) = session.queue.disarm() {
                    session
                        .log
                        .warn(Area::Queue, format!("Schedule clear refused: {error}"));
                    toast_err(&mut session, error.to_string());
                } else {
                    session.log.info(Area::Queue, "Schedule disarmed");
                    persist_queue(&mut session);
                    toast(&mut session, "Schedule cleared.");
                }
            }
            wake();
        }
    });

    tray.on_open_requested({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                show_main(&ui);
            }
        }
    });
    tray.on_open_queue({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                show_main(&ui);
            }
            open_queue();
        }
    });
    tray.on_open_settings({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                show_main(&ui);
            }
            open_settings();
        }
    });
    tray.on_quit({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || begin_quit(&session, &log, &tx, &wake)
    });
}

fn render(ui: &MainWindow, panel: &SidePanel, tray: &TrayIcon, session: &Arc<Mutex<Session>>) {
    let Ok(mut session) = session.lock() else {
        return;
    };
    let now = Instant::now();
    settle_reveal(&mut session, now);
    MODELS.with(|models| {
        if let Some(models) = models.borrow().as_ref() {
            sync_model(&models.hits, hit_rows(&session));
            sync_model(&models.rows, queue_rows(&session, now));
        }
    });
    render_main(ui, &session, now);
    render_panel(panel, &session, now);
    render_tray(tray, &session, now);
}

fn render_main(ui: &MainWindow, session: &Session, now: Instant) {
    let first_download = session.loaded && session.games.is_empty() && session.refreshing;
    ui.set_loading(!session.loaded || first_download || now < session.started + LOADING_MIN);
    ui.set_loading_label(
        if first_download {
            "Downloading the game list…"
        } else {
            "Loading games…"
        }
        .into(),
    );
    let unavailable = session.loaded && session.games.is_empty() && !session.refreshing;
    ui.set_results_state(
        if unavailable {
            "unavailable"
        } else {
            match session.results {
                Results::Idle => "idle",
                Results::Pending => "pending",
                Results::Ready => "ready",
                Results::Empty => "empty",
            }
        }
        .into(),
    );
    let selected = selected_game(session);
    ui.set_selected_index(
        selected
            .and_then(|game| {
                session
                    .shown
                    .iter()
                    .position(|&index| session.games.get(index).is_some_and(|g| g.id == game.id))
            })
            .map_or(-1, |position| position as i32),
    );
    let (status, color) = if session.refreshing && !session.games.is_empty() {
        ("Refreshing…", Tone::Accent)
    } else if session.offline {
        ("Offline · using cache", Tone::Warning)
    } else if unavailable {
        ("Database unavailable", Tone::Danger)
    } else {
        ("", Tone::Muted)
    };
    ui.set_db_status(status.into());
    ui.set_db_color(color.into());

    let state = session.clock.state();
    let (label, mode, tone) =
        if matches!(session.queue.activity(), QueueActivity::Transition { .. }) {
            ("Switching…", "switching", Tone::Muted)
        } else {
            match state {
                SessionState::Playing => ("Playing", "playing", Tone::Success),
                SessionState::Paused => ("Paused", "paused", Tone::Warning),
                SessionState::Stopped => ("Ready", "stopped", Tone::Muted),
            }
        };
    let current = session_game(session);
    ui.set_session_visible(current.is_some());
    ui.set_active_id(
        current
            .filter(|_| state != SessionState::Stopped)
            .map(|game| game.id.clone())
            .unwrap_or_default()
            .into(),
    );
    ui.set_session_state(label.into());
    ui.set_session_mode(mode.into());
    ui.set_session_color(tone.into());
    ui.set_session_time(displayed_time(session, now).into());
    ui.set_session_note(queue_note(session, now).into());
    if let Some(game) = current {
        ui.set_session_name(game.name.clone().into());
        ui.set_session_art(cached_art(&game.id));
    } else {
        ui.set_session_name(SharedString::default());
        ui.set_session_art(slint::Image::default());
    }
    let controls = controls(session);
    ui.set_play_enabled(controls.play);
    ui.set_pause_enabled(controls.pause);
    ui.set_stop_enabled(controls.stop);
    ui.set_resume_queue(controls.resume_queue);
    ui.set_play_blocked(controls.blocked);
    ui.set_play_label(controls.play_label.into());

    ui.set_queue_count(session.queue.items().len() as i32);
    ui.set_queue_line(queue_line(session).into());
    ui.set_toast_visible(session.toast_until.is_some_and(|until| now < until));
    if !session.toast.is_empty() {
        ui.set_toast(session.toast.clone().into());
        ui.set_toast_kind(
            match session.toast_kind {
                ToastKind::Info => "info",
                ToastKind::Success => "success",
                ToastKind::Error => "error",
            }
            .into(),
        );
    }
}

fn render_panel(panel: &SidePanel, session: &Session, now: Instant) {
    let (state, status) = queue_state(session);
    panel.set_queue_state(state.into());
    panel.set_queue_status(status.into());
    panel.set_queue_summary(queue_summary(session, now).into());
    panel.set_schedule_armed(matches!(
        session.queue.activity(),
        QueueActivity::Scheduled { .. }
    ));
    let selected = selected_game(session);
    panel.set_has_selection(selected.is_some());
    if let Some(game) = selected {
        panel.set_selected_name(game.name.clone().into());
        panel.set_selected_art(cached_art(&game.id));
    }
    panel.set_database_detail(database_detail(session).into());
}

fn render_tray(tray: &TrayIcon, session: &Session, now: Instant) {
    let selected = session_game(session);
    let label = match session.clock.state() {
        SessionState::Playing => "Playing",
        SessionState::Paused => "Paused",
        SessionState::Stopped => "Stopped",
    };
    tray.set_status_tip(if let Some(game) = selected {
        format!("Game Larper · {} · {label}", game.name).into()
    } else {
        "Game Larper".into()
    });
    tray.set_game_name(
        selected
            .map(|game| game.name.clone())
            .unwrap_or_else(|| "No game selected".into())
            .into(),
    );
    tray.set_status_line(format!("{label} · {}", displayed_time(session, now)).into());
    let controls = controls(session);
    tray.set_play_label(controls.play_label.into());
    tray.set_play_enabled(controls.play);
    tray.set_pause_enabled(controls.pause);
    tray.set_stop_enabled(controls.stop);
    tray.set_queue_line(format!("Queue ({})", session.queue.items().len()).into());
}

fn tick(session: &Arc<Mutex<Session>>, log: &Arc<Log>, tx: &mpsc::Sender<Msg>, wake: &Wake) {
    let Ok(mut session_guard) = session.lock() else {
        return;
    };
    if session_guard
        .toast_until
        .is_some_and(|until| Instant::now() >= until)
    {
        session_guard.toast_until = None;
    }
    if !session_guard.restored && session_guard.loaded {
        session_guard.restored = true;
        if session_guard.config.auto_resume && session_guard.selected.is_some() {
            log.info(
                Area::Session,
                "Auto-resume is on; playing the restored selection",
            );
            drop(session_guard);
            begin_play(session, log, tx, wake);
            return;
        }
    }
    let before = session_guard.queue.activity();
    let action = {
        let now = Instant::now();
        let unix = unix_time_ms(SystemTime::now());
        session_guard.queue.tick(now, unix)
    };
    let after = session_guard.queue.activity();
    if after != before {
        let queue = &session_guard.queue;
        for (level, message) in queue_tick_events(&before, &after, queue.items(), queue.gap()) {
            log.record(level, Area::Queue, &message);
        }
    }
    // The dock and the queue panel show artwork too; fetch what they miss.
    let wanted: Vec<GameDefinition> = {
        let session = &*session_guard;
        let mut ids: Vec<&str> = session
            .selected
            .iter()
            .chain(session.active.iter())
            .map(String::as_str)
            .collect();
        ids.extend(
            session
                .queue
                .items()
                .iter()
                .map(|item| item.application_id.as_str()),
        );
        // Most ticks have nothing to fetch; skip the catalog scan for those.
        ids.retain(|id| {
            !ART.with(|art| art.borrow().contains_key(*id))
                && !session.art_failed.contains(*id)
                && !session.art_inflight.contains(*id)
        });
        ids.iter()
            .filter_map(|id| session.games.iter().find(|game| game.id == *id))
            .cloned()
            .collect()
    };
    fetch_art(&mut session_guard, &wanted, tx, wake, false);
    drop(session_guard);
    if action.stop_runner || action.launch_index.is_some() {
        dispatch_queue_action(session, log, tx, wake, action);
    }
}

fn apply_message(
    session: &Arc<Mutex<Session>>,
    log: &Arc<Log>,
    tx: &mpsc::Sender<Msg>,
    wake: &Wake,
    message: Msg,
) {
    let Ok(mut session) = session.lock() else {
        return;
    };
    match message {
        Msg::Loaded { games, updated } => {
            session.games = games;
            session.loaded = true;
            session.catalog_updated = updated;
            if session.config.restore_last_selected_game
                && let Some(id) = session.config.last_selected_discord_application_id.clone()
            {
                match session.games.iter().find(|game| game.id == id) {
                    Some(game) => log.debug(
                        Area::Session,
                        format!("Restored selection {id} / {}", game.name),
                    ),
                    None => log.warn(
                        Area::Session,
                        format!("Last selection {id} is not a supported game; not restored"),
                    ),
                }
                session.selected =
                    Some(id).filter(|id| session.games.iter().any(|game| game.id == *id));
            }
            let plan = plan_results(&mut session);
            fetch_art(&mut session, &plan.first, tx, wake, true);
            fetch_art(&mut session, &plan.rest, tx, wake, false);
            let stale = updated.is_none_or(|time| {
                SystemTime::now().duration_since(time).unwrap_or_default() > CATALOG_STALE
            });
            if (stale || session.games.is_empty()) && !session.refreshing {
                log.info(
                    Area::Catalog,
                    if session.games.is_empty() {
                        "No usable database; downloading it".to_string()
                    } else {
                        format!("Database is stale ({}); refreshing", catalog_age(updated))
                    },
                );
                session.refreshing = true;
                spawn_refresh(tx.clone(), session.paths.clone(), log.clone(), wake.clone());
            }
            // Render again once the loader has had its minimum time on screen.
            let remaining =
                (session.started + LOADING_MIN).saturating_duration_since(Instant::now());
            let wake = wake.clone();
            Timer::single_shot(remaining + Duration::from_millis(10), move || wake());
        }
        Msg::Catalog(result) => {
            let manual = std::mem::take(&mut session.manual_refresh);
            session.refreshing = false;
            match result {
                Ok(games) => {
                    session.games = games
                        .into_iter()
                        .filter(|game| game.supported_path().is_some())
                        .collect();
                    let count = session.games.len();
                    session.loaded = true;
                    session.offline = false;
                    session.catalog_updated = Some(SystemTime::now());
                    log.success(
                        Area::Catalog,
                        format!("Refreshed: {} supported games", group_digits(count)),
                    );
                    if let Some(id) = session
                        .selected
                        .clone()
                        .filter(|id| !session.games.iter().any(|game| &game.id == id))
                    {
                        log.warn(
                            Area::Session,
                            format!("Selected game {id} is no longer supported; cleared"),
                        );
                        session.selected = None;
                        session.config.last_selected_discord_application_id = None;
                        if let Err(error) =
                            ConfigStore::new(session.paths.config()).save(&session.config)
                        {
                            log.error(Area::Settings, format!("Config save failed: {error}"));
                        }
                    }
                    session.art_failed.clear();
                    let plan = plan_results(&mut session);
                    fetch_art(&mut session, &plan.first, tx, wake, true);
                    fetch_art(&mut session, &plan.rest, tx, wake, false);
                    if manual {
                        toast_ok(
                            &mut session,
                            format!("Game list updated · {} games", group_digits(count)),
                        );
                    }
                }
                Err(error) => {
                    if session.games.is_empty() {
                        log.error(
                            Area::Catalog,
                            format!("Refresh failed and no database is available: {error}"),
                        );
                    } else {
                        log.warn(
                            Area::Catalog,
                            format!(
                                "Refresh failed; offline with the cached {} games: {error}",
                                group_digits(session.games.len())
                            ),
                        );
                    }
                    session.offline = !session.games.is_empty();
                    toast_err(
                        &mut session,
                        "Refresh failed. The last good database is still here.",
                    );
                }
            }
        }
        Msg::Art { id, path } => {
            session.art_inflight.remove(&id);
            let image = path.and_then(|path| {
                slint::Image::load_from_path(&path)
                    .inspect_err(|error| {
                        log.warn(
                            Area::Art,
                            format!("Could not load {}: {error}", redact(&path)),
                        );
                    })
                    .ok()
            });
            match image {
                Some(image) => ART.with(|art| {
                    art.borrow_mut().insert(id.clone(), image);
                }),
                None => {
                    session.art_failed.insert(id.clone());
                }
            }
            if let Some(reveal) = session.reveal.as_mut() {
                reveal.waiting.remove(&id);
            }
        }
        Msg::Launch(result) => {
            session.busy = false;
            match result {
                Ok(report) => {
                    session.clock.play(Instant::now());
                    log.success(
                        Area::Runner,
                        format!(
                            "Running PID={} {}=0x{:X} title=\"{}\" integrity={} basename={} path={} cwd={} (generation {})",
                            report.pid,
                            host::WINDOW_LABEL,
                            report.hwnd,
                            report.title,
                            report.integrity,
                            report.basename,
                            redact(&report.executable),
                            redact(&report.working_directory),
                            report.generation
                        ),
                    );
                    watch_exit(report.generation, report.exit_watch, tx.clone());
                }
                Err(error) => {
                    // The cause is logged where the launch failed; this is what it means here.
                    session.clock.stop();
                    if matches!(
                        session.queue.activity(),
                        QueueActivity::Running { .. }
                            | QueueActivity::Paused { .. }
                            | QueueActivity::Transition { .. }
                    ) {
                        let index = match session.queue.activity() {
                            QueueActivity::Running { index } | QueueActivity::Paused { index } => {
                                index
                            }
                            QueueActivity::Transition { next_index } => next_index,
                            _ => 0,
                        };
                        log.error(
                            Area::Queue,
                            format!("Item {} failed to launch; queue stopped", index + 1),
                        );
                        session.queue.fail(index, error.clone());
                    }
                    toast_err(&mut session, error);
                }
            }
        }
        Msg::Stopped(result) => {
            session.busy = false;
            if let Err(error) = result {
                toast_err(&mut session, error);
            }
            if session.quitting {
                log.info(Area::App, "Shutting down");
                let _ = slint::quit_event_loop();
            }
        }
        Msg::Exited { generation } => {
            let unexpected = session
                .host
                .lock()
                .map(|mut host| host.take_unexpected_exit(generation))
                .unwrap_or(false);
            if unexpected {
                session.clock.unexpected_exit();
                if let QueueActivity::Running { index } | QueueActivity::Paused { index } =
                    session.queue.activity()
                {
                    log.error(
                        Area::Queue,
                        format!(
                            "Item {} failed: the runner exited; queue stopped",
                            index + 1
                        ),
                    );
                    session.queue.fail(index, "The runner exited.");
                }
                session.busy = false;
                toast_err(&mut session, "The runner exited.");
            }
        }
    }
}

include!("commands.rs");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands() {
        assert_eq!(group_digits(7), "7");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(10_431), "10,431");
        assert_eq!(group_digits(1_234_567), "1,234,567");
    }

    #[test]
    fn short_durations_drop_empty_hours() {
        assert_eq!(format_short(Duration::from_secs(0)), "0:00");
        assert_eq!(format_short(Duration::from_secs(12 * 60 + 4)), "12:04");
        assert_eq!(format_short(Duration::from_secs(3723)), "1:02:03");
    }

    #[test]
    fn minutes_round_up_and_read_naturally() {
        assert_eq!(format_minutes(Duration::from_secs(45 * 60)), "45 min");
        assert_eq!(format_minutes(Duration::from_secs(59 * 60 + 1)), "1 h");
        assert_eq!(format_minutes(Duration::from_secs(90 * 60)), "1 h 30 min");
    }

    #[test]
    fn model_sync_updates_rows_in_place() {
        let model = VecModel::from(vec![1, 2, 3]);
        sync_model(&model, vec![1, 5, 3]);
        assert_eq!(model.iter().collect::<Vec<_>>(), vec![1, 5, 3]);
        sync_model(&model, vec![4]);
        assert_eq!(model.iter().collect::<Vec<_>>(), vec![4]);
    }

    fn queue_items() -> Vec<QueueItem> {
        ["ELDEN RING", "Hades"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| QueueItem {
                id: index as u64 + 1,
                application_id: format!("{}", 100 + index),
                name: name.into(),
                duration: Duration::from_secs(30 * 60),
            })
            .collect()
    }

    #[test]
    fn queue_ticks_log_completion_gap_and_finish() {
        let items = queue_items();
        let gap = Duration::from_secs(2);

        let to_gap = queue_tick_events(
            &QueueActivity::Running { index: 0 },
            &QueueActivity::Transition { next_index: 1 },
            &items,
            gap,
        );
        assert_eq!(
            to_gap,
            vec![
                (
                    LogLevel::Info,
                    "Item 1 (ELDEN RING) completed its 30 min".to_string()
                ),
                (LogLevel::Debug, "Waiting 2000 ms before item 2".to_string()),
            ]
        );

        let finished = queue_tick_events(
            &QueueActivity::Running { index: 1 },
            &QueueActivity::Idle,
            &items,
            gap,
        );
        assert_eq!(finished.len(), 2);
        assert_eq!(
            finished[1],
            (
                LogLevel::Success,
                "Queue finished all 2 item(s)".to_string()
            )
        );

        // The launch itself is logged when the runner is dispatched, not here.
        let started = queue_tick_events(
            &QueueActivity::Transition { next_index: 1 },
            &QueueActivity::Running { index: 1 },
            &items,
            gap,
        );
        assert!(started.is_empty());
    }

    #[test]
    fn scheduled_starts_are_logged_when_they_fire() {
        let events = queue_tick_events(
            &QueueActivity::Scheduled { at_unix_ms: 0 },
            &QueueActivity::Running { index: 0 },
            &queue_items(),
            Duration::from_secs(2),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, LogLevel::Info);
        assert!(events[0].1.starts_with("Scheduled start ("));
    }

    #[test]
    fn queue_activity_descriptions_count_from_one() {
        assert_eq!(
            describe_activity(&QueueActivity::Transition { next_index: 0 }),
            "switching to item 1"
        );
        assert_eq!(
            describe_activity(&QueueActivity::Failed {
                index: 1,
                message: "The runner exited.".into()
            }),
            "failed on item 2: The runner exited."
        );
    }

    #[test]
    fn transport_sync_stops_false_playing_but_preserves_real_pauses() {
        let stop = QueueAction {
            stop_runner: true,
            launch_index: None,
        };
        let launch = QueueAction {
            stop_runner: true,
            launch_index: Some(1),
        };

        assert!(should_stop_clock_for_dispatch(
            &QueueActivity::Idle,
            SessionState::Playing,
            &stop
        ));
        assert!(should_stop_clock_for_dispatch(
            &QueueActivity::Transition { next_index: 1 },
            SessionState::Playing,
            &stop
        ));
        assert!(!should_stop_clock_for_dispatch(
            &QueueActivity::Paused { index: 0 },
            SessionState::Paused,
            &stop
        ));
        assert!(!should_stop_clock_for_dispatch(
            &QueueActivity::Idle,
            SessionState::Paused,
            &stop
        ));
        assert!(should_stop_clock_for_dispatch(
            &QueueActivity::Running { index: 1 },
            SessionState::Playing,
            &launch
        ));
    }
}
