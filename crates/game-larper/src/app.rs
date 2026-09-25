use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

use chrono::{Local, NaiveDate, NaiveTime, TimeZone};
use game_larper_core::{
    AppConfig, AppPaths, CatalogCache, ConfigStore, DEFAULT_SEARCH_LIMIT, DEFAULT_TRANSITION_GAP,
    GameDefinition, QueueAction, QueueActivity, QueueMachine, SessionClock, SessionState,
    format_hms, load_catalog_with_legacy, load_queue, save_queue, unix_time_ms,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use crate::host::{LaunchReport, RunnerHost};
use crate::log::Log;
use crate::net;
use crate::platform;

slint::include_modules!();

struct Models {
    hits: Rc<VecModel<Hit>>,
    rows: Rc<VecModel<QueueRow>>,
}

thread_local! {
    static MODELS: std::cell::RefCell<Option<Models>> = const { std::cell::RefCell::new(None) };
    static ART: std::cell::RefCell<HashMap<String, slint::Image>> = std::cell::RefCell::new(HashMap::new());
}

enum Msg {
    Catalog(Result<Vec<GameDefinition>, String>),
    Art {
        id: String,
        generation: u64,
        path: Option<PathBuf>,
    },
    Launch(Result<LaunchReport, String>),
    Stopped(Result<(), String>),
    Exited {
        generation: u64,
    },
}

struct Session {
    paths: AppPaths,
    config: AppConfig,
    games: Vec<GameDefinition>,
    query: String,
    selected: Option<String>,
    clock: SessionClock,
    queue: QueueMachine,
    host: Arc<Mutex<RunnerHost>>,
    catalog_note: String,
    toast: String,
    toast_until: Option<Instant>,
    busy: bool,
    queue_open: bool,
    art_generation: u64,
    art_pending: bool,
    restored: bool,
    quitting: bool,
}

pub fn run(
    paths: AppPaths,
    log: Log,
    start_minimized_flag: bool,
) -> Result<(), slint::PlatformError> {
    let log = Arc::new(log);
    let ui = MainWindow::new()?;
    let tray = TrayIcon::new()?;
    let (tx, rx) = mpsc::channel();
    let rx = Arc::new(Mutex::new(rx));
    let session = Arc::new(Mutex::new(load_session(&paths, &log)));
    let hits = Rc::new(VecModel::from(Vec::<Hit>::new()));
    let rows = Rc::new(VecModel::from(Vec::<QueueRow>::new()));
    ui.set_hits(ModelRc::from(hits.clone()));
    ui.set_queue_rows(ModelRc::from(rows.clone()));
    MODELS.with(|slot| {
        *slot.borrow_mut() = Some(Models {
            hits: hits.clone(),
            rows: rows.clone(),
        });
    });
    ui.set_version_label(env!("CARGO_PKG_VERSION").into());
    sync_settings_from_config(
        &ui,
        &session.lock().unwrap_or_else(|p| p.into_inner()).config,
    );
    render(&ui, &tray, &hits, &rows, &session);

    let wake: Arc<dyn Fn() + Send + Sync> = {
        let rx = rx.clone();
        let session = session.clone();
        let ui_weak = ui.as_weak();
        let tray_weak = tray.as_weak();
        let log = log.clone();
        let tx = tx.clone();
        Arc::new(move || {
            let ui_weak = ui_weak.clone();
            let tray_weak = tray_weak.clone();
            let session = session.clone();
            let rx = rx.clone();
            let log = log.clone();
            let tx = tx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let Some(tray) = tray_weak.upgrade() else {
                    return;
                };
                let mut batch = Vec::new();
                if let Ok(rx) = rx.lock() {
                    while let Ok(message) = rx.try_recv() {
                        batch.push(message);
                    }
                }
                for message in batch {
                    apply_message(&session, &log, &tx, &ui, message);
                }
                MODELS.with(|models| {
                    if let Some(models) = models.borrow().as_ref() {
                        render(&ui, &tray, &models.hits, &models.rows, &session);
                    }
                });
            });
        }) as Arc<dyn Fn() + Send + Sync>
    };

    wire(&ui, &tray, &session, &log, &tx, &wake, &hits);
    platform::watch_activation({
        let ui_weak = ui.as_weak();
        move || {
            let ui_weak = ui_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let _ = ui.show();
                }
            });
        }
    });

    let minimized = start_minimized_flag
        || session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .config
            .start_minimized;
    if !minimized {
        ui.show()?;
        round_after_show(&ui);
    }
    if session
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .catalog_note
        .contains("stale")
        || session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .games
            .is_empty()
    {
        spawn_refresh(tx.clone(), paths.clone(), log.clone(), wake.clone());
    }

    let timer = Timer::default();
    {
        let session = session.clone();
        let ui_weak = ui.as_weak();
        let tray_weak = tray.as_weak();
        let tx = tx.clone();
        let log = log.clone();
        let wake = wake.clone();
        let rx = rx.clone();
        timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(tray) = tray_weak.upgrade() else {
                return;
            };
            let mut batch = Vec::new();
            if let Ok(rx) = rx.lock() {
                while let Ok(message) = rx.try_recv() {
                    batch.push(message);
                }
            }
            for message in batch {
                apply_message(&session, &log, &tx, &ui, message);
            }
            tick(&session, &log, &tx, &wake);
            MODELS.with(|models| {
                if let Some(models) = models.borrow().as_ref() {
                    render(&ui, &tray, &models.hits, &models.rows, &session);
                }
            });
        });
    }

    ui.window().on_close_requested({
        let session = session.clone();
        let ui_weak = ui.as_weak();
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
                if let Some(ui) = ui_weak.upgrade() {
                    let _ = ui.hide();
                }
            } else {
                begin_quit(&session, &log, &tx, &wake);
            }
            slint::CloseRequestResponse::KeepWindowShown
        }
    });

    slint::run_event_loop()?;
    if let Ok(mut host) = session
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .host
        .lock()
    {
        let _ = host.stop();
    }
    log.info("Game Larper stopped");
    Ok(())
}

fn load_session(paths: &AppPaths, log: &Log) -> Session {
    let (config, warnings) = ConfigStore::new(paths.config()).load();
    for warning in warnings {
        log.info(warning);
    }
    let current = CatalogCache::new(paths.catalog());
    let legacy = CatalogCache::new(paths.legacy_catalog());
    let (games, warning) = load_catalog_with_legacy(&current, &legacy);
    if let Some(warning) = warning {
        log.info(warning);
    }
    let games = games.unwrap_or_default();
    let updated = current.last_updated().or_else(|| legacy.last_updated());
    let stale = updated.is_none_or(|time| {
        SystemTime::now().duration_since(time).unwrap_or_default()
            > Duration::from_secs(24 * 60 * 60)
    });
    let note = if games.is_empty() {
        "Game database unavailable. Refresh it in Settings.".into()
    } else if stale {
        format!(
            "{} supported games · refresh pending",
            supported_count(&games)
        )
    } else {
        format!("{} supported games", supported_count(&games))
    };
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
        log.info(warning);
    }
    let selected = if config.restore_last_selected_game {
        config
            .last_selected_discord_application_id
            .clone()
            .filter(|id| {
                games
                    .iter()
                    .any(|game| game.id == *id && game.supported_path().is_some())
            })
    } else {
        None
    };
    Session {
        paths: paths.clone(),
        config,
        games: games
            .into_iter()
            .filter(|game| game.supported_path().is_some())
            .collect(),
        query: String::new(),
        selected,
        clock: SessionClock::new(),
        queue,
        host: Arc::new(Mutex::new(RunnerHost::new(paths.runtime()))),
        catalog_note: note,
        toast: String::new(),
        toast_until: None,
        busy: false,
        queue_open: false,
        art_generation: 1,
        art_pending: true,
        restored: false,
        quitting: false,
    }
}

fn wire(
    ui: &MainWindow,
    tray: &TrayIcon,
    session: &Arc<Mutex<Session>>,
    log: &Arc<Log>,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
    hits: &Rc<VecModel<Hit>>,
) {
    ui.on_query_edited({
        let session = session.clone();
        let ui = ui.as_weak();
        let tray = tray.as_weak();
        let hits = hits.clone();
        move |text| {
            if let Ok(mut session) = session.lock() {
                session.query = text.to_string();
                session.art_pending = true;
            }
            if let (Some(ui), Some(tray)) = (ui.upgrade(), tray.upgrade()) {
                render(
                    &ui,
                    &tray,
                    &hits,
                    &Rc::new(VecModel::from(Vec::new())),
                    &session,
                );
            }
        }
    });
    // The queue model is owned by render(); query edits still go through the full render below.
    let _ = hits;

    ui.on_choose({
        let session = session.clone();
        let log = log.clone();
        move |index| select_index(&session, &log, index as usize, false)
    });
    ui.on_activate({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move |index| {
            select_index(&session, &log, index as usize, false);
            begin_play(&session, &log, &tx, &wake);
        }
    });
    ui.on_play({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || begin_play(&session, &log, &tx, &wake)
    });
    ui.on_pause({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || begin_pause(&session, &log, &tx, &wake)
    });
    ui.on_stop({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || begin_stop(&session, &log, &tx, &wake, true)
    });
    ui.on_toggle_queue({
        let session = session.clone();
        move || {
            if let Ok(mut session) = session.lock() {
                session.queue_open = !session.queue_open;
            }
        }
    });
    ui.on_open_settings({
        let session = session.clone();
        let ui = ui.as_weak();
        move || {
            if let (Ok(mut session), Some(ui)) = (session.lock(), ui.upgrade()) {
                sync_settings_from_config(&ui, &session.config);
                session.queue_open = false;
                let _ = ui.show();
            }
            if let Ok(mut session) = session.lock() {
                let _ = &mut session;
            }
            if let Some(ui) = ui.upgrade() {
                ui.set_settings_open(true);
            }
        }
    });
    ui.on_close_settings({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                ui.set_settings_open(false);
            }
        }
    });
    ui.on_save_settings({
        let session = session.clone();
        let ui = ui.as_weak();
        let log = log.clone();
        move || save_settings(&session, &ui, &log)
    });
    ui.on_refresh_catalog({
        let session = session.clone();
        let tx = tx.clone();
        let log = log.clone();
        let wake = wake.clone();
        move || {
            let paths = session.lock().map(|session| session.paths.clone()).ok();
            if let Some(paths) = paths {
                spawn_refresh(tx.clone(), paths, log.clone(), wake.clone());
            }
        }
    });
    ui.on_clear_images({
        let session = session.clone();
        let log = log.clone();
        move || {
            if let Ok(mut session) = session.lock() {
                match net::clear_artwork(&session.paths.images()) {
                    Ok(count) => {
                        ART.with(|art| art.borrow_mut().clear());
                        session.art_pending = true;
                        toast(&mut session, format!("Cleared {count} cached images."));
                    }
                    Err(error) => {
                        log.info(&error);
                        toast(&mut session, error);
                    }
                }
            }
        }
    });
    ui.on_open_logs({
        let session = session.clone();
        move || {
            if let Ok(session) = session.lock() {
                let _ = platform::open_in_explorer(&session.paths.logs());
            }
        }
    });
    ui.on_open_data({
        let session = session.clone();
        move || {
            if let Ok(session) = session.lock() {
                let _ = platform::open_in_explorer(&session.paths.root);
            }
        }
    });
    ui.on_add_queue({
        let session = session.clone();
        let ui = ui.as_weak();
        move || add_selected_to_queue(&session, &ui)
    });
    ui.on_queue_up({
        let session = session.clone();
        move |index| move_queue(&session, index as usize, index as usize - 1)
    });
    ui.on_queue_down({
        let session = session.clone();
        move |index| move_queue(&session, index as usize, index as usize + 1)
    });
    ui.on_queue_remove({
        let session = session.clone();
        move |index| {
            if let Ok(mut session) = session.lock() {
                if let Err(error) = session.queue.remove(index as usize) {
                    toast(&mut session, error.to_string());
                } else {
                    persist_queue(&mut session);
                }
            }
        }
    });
    ui.on_queue_clear({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            let action = session.lock().ok().map(|mut session| {
                let action = session.queue.clear();
                persist_queue(&mut session);
                action
            });
            if let Some(action) = action {
                dispatch_queue_action(&session, &log, &tx, &wake, action);
            }
        }
    });
    ui.on_queue_start({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            queue_command(&session, &log, &tx, &wake, |queue| {
                queue.start_now(Instant::now())
            })
        }
    });
    ui.on_queue_pause({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            let action = session.lock().ok().map(|mut session| {
                let action = session.queue.pause(Instant::now());
                if action.stop_runner {
                    session.clock.pause(Instant::now());
                }
                action
            });
            if let Some(action) = action {
                dispatch_queue_action(&session, &log, &tx, &wake, action);
            }
        }
    });
    ui.on_queue_resume({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            let action = session
                .lock()
                .ok()
                .map(|mut session| session.queue.resume(Instant::now()));
            if let Some(action) = action {
                dispatch_queue_action(&session, &log, &tx, &wake, action);
            }
        }
    });
    ui.on_queue_skip({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            let action = session
                .lock()
                .ok()
                .map(|mut session| session.queue.skip(Instant::now()));
            if let Some(action) = action {
                dispatch_queue_action(&session, &log, &tx, &wake, action);
            }
        }
    });
    ui.on_queue_stop({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || begin_stop(&session, &log, &tx, &wake, false)
    });
    ui.on_arm_schedule({
        let session = session.clone();
        let ui = ui.as_weak();
        move || arm_schedule(&session, &ui)
    });
    ui.on_disarm_schedule({
        let session = session.clone();
        move || {
            if let Ok(mut session) = session.lock() {
                if let Err(error) = session.queue.disarm() {
                    toast(&mut session, error.to_string());
                } else {
                    persist_queue(&mut session);
                    toast(&mut session, "Schedule cleared.");
                }
            }
        }
    });
    ui.on_request_close({
        let session = session.clone();
        let ui = ui.as_weak();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || {
            let close_to_tray = session
                .lock()
                .map(|session| session.config.close_to_tray)
                .unwrap_or(true);
            if close_to_tray {
                if let Some(ui) = ui.upgrade() {
                    let _ = ui.hide();
                }
            } else {
                begin_quit(&session, &log, &tx, &wake);
            }
        }
    });

    tray.on_open_requested({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                let _ = ui.show();
            }
        }
    });
    tray.on_play({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || begin_play(&session, &log, &tx, &wake)
    });
    tray.on_pause({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || begin_pause(&session, &log, &tx, &wake)
    });
    tray.on_stop({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move || begin_stop(&session, &log, &tx, &wake, true)
    });
    tray.on_open_queue({
        let session = session.clone();
        let ui = ui.as_weak();
        move || {
            if let Ok(mut session) = session.lock() {
                session.queue_open = true;
            }
            if let Some(ui) = ui.upgrade() {
                let _ = ui.show();
            }
        }
    });
    tray.on_open_settings({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                let _ = ui.show();
                ui.set_settings_open(true);
            }
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

fn render(
    ui: &MainWindow,
    tray: &TrayIcon,
    hits: &VecModel<Hit>,
    rows: &VecModel<QueueRow>,
    session: &Arc<Mutex<Session>>,
) {
    let Ok(session) = session.lock() else { return };
    let now = Instant::now();
    let visible: Vec<usize> =
        game_larper_core::search(&session.games, &session.query, DEFAULT_SEARCH_LIMIT);
    let mut models = Vec::with_capacity(visible.len());
    for index in visible {
        let game = &session.games[index];
        models.push(Hit {
            id: game.id.clone().into(),
            name: game.name.clone().into(),
            detail: detail_line(game).into(),
            art: cached_art(&game.id),
            selected: session.selected.as_deref() == Some(game.id.as_str()),
        });
    }
    hits.set_vec(models);
    ui.set_catalog_status(session.catalog_note.clone().into());
    ui.set_empty_message(if session.games.is_empty() {
        "No supported games yet. Open Settings and refresh the database.".into()
    } else if session.query.trim().is_empty() {
        "Type a name. Elden is a fine place to start.".into()
    } else {
        "No games match that.".into()
    });
    ui.set_queue_open(session.queue_open);
    ui.set_queue_count(format!("Queue ({})", session.queue.items().len()).into());
    let selected = session
        .selected
        .as_ref()
        .and_then(|id| session.games.iter().find(|game| &game.id == id));
    let queue_running = matches!(
        session.queue.activity(),
        QueueActivity::Running { .. }
            | QueueActivity::Paused { .. }
            | QueueActivity::Transition { .. }
    );
    let state = session.clock.state();
    let (label, color) = match state {
        SessionState::Playing => ("Playing", ThemeColor::Success),
        SessionState::Paused => ("Paused", ThemeColor::Warning),
        SessionState::Stopped => ("Stopped", ThemeColor::Muted),
    };
    ui.set_session_state(label.into());
    ui.set_session_color(color.into());
    ui.set_session_time(displayed_time(&session, now).into());
    if let Some(game) = selected {
        ui.set_session_name(game.name.clone().into());
        ui.set_session_detail(detail_line(game).into());
        ui.set_session_art(cached_art(&game.id));
    } else {
        ui.set_session_name("No game selected".into());
        ui.set_session_detail("Search, then press Play.".into());
        ui.set_session_art(slint::Image::default());
    }
    let play_enabled =
        selected.is_some() && !session.busy && state != SessionState::Playing && !queue_running;
    let pause_enabled = !session.busy && state == SessionState::Playing;
    let stop_enabled = !session.busy && (state != SessionState::Stopped || queue_running);
    ui.set_play_enabled(play_enabled);
    ui.set_pause_enabled(pause_enabled);
    ui.set_stop_enabled(stop_enabled);
    ui.set_play_label(
        if state == SessionState::Paused {
            "Resume"
        } else {
            "Play"
        }
        .into(),
    );
    ui.set_queue_status(queue_status(&session).into());
    ui.set_toast(if session.toast_until.is_some_and(|until| now < until) {
        session.toast.clone().into()
    } else {
        SharedString::default()
    });
    rows.set_vec(queue_rows(&session, now));

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
    tray.set_status_line(format!("{label} · {}", displayed_time(&session, now)).into());
    tray.set_play_label(
        if state == SessionState::Paused {
            "Resume"
        } else {
            "Play"
        }
        .into(),
    );
    tray.set_play_enabled(play_enabled);
    tray.set_pause_enabled(pause_enabled);
    tray.set_stop_enabled(stop_enabled);
    tray.set_queue_line(format!("Queue ({})", session.queue.items().len()).into());
}

fn tick(
    session: &Arc<Mutex<Session>>,
    log: &Arc<Log>,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
) {
    let Ok(mut session_guard) = session.lock() else {
        return;
    };
    if session_guard
        .toast_until
        .is_some_and(|until| Instant::now() >= until)
    {
        session_guard.toast.clear();
        session_guard.toast_until = None;
    }
    if !session_guard.restored {
        session_guard.restored = true;
        if session_guard.config.auto_resume && session_guard.selected.is_some() {
            drop(session_guard);
            begin_play(session, log, tx, wake);
            return;
        }
    }
    let action = {
        let now = Instant::now();
        let unix = unix_time_ms(SystemTime::now());
        session_guard.queue.tick(now, unix)
    };
    let fetch_art = session_guard.art_pending;
    if fetch_art {
        session_guard.art_pending = false;
        session_guard.art_generation = session_guard.art_generation.saturating_add(1);
    }
    let generation = session_guard.art_generation;
    let games = if fetch_art {
        art_targets(&session_guard)
    } else {
        Vec::new()
    };
    let images = session_guard.paths.images();
    drop(session_guard);
    if action.stop_runner || action.launch_index.is_some() {
        dispatch_queue_action(session, log, tx, wake, action);
    }
    if !games.is_empty() {
        let tx = tx.clone();
        let wake = wake.clone();
        std::thread::spawn(move || {
            for game in games {
                let path = net::ensure_artwork(&images, &game);
                let _ = tx.send(Msg::Art {
                    id: game.id,
                    generation,
                    path,
                });
                wake();
            }
        });
    }
}

fn apply_message(
    session: &Arc<Mutex<Session>>,
    log: &Log,
    tx: &mpsc::Sender<Msg>,
    _ui: &MainWindow,
    message: Msg,
) {
    let Ok(mut session) = session.lock() else {
        return;
    };
    match message {
        Msg::Catalog(result) => match result {
            Ok(games) => {
                let count = supported_count(&games);
                session.games = games
                    .into_iter()
                    .filter(|game| game.supported_path().is_some())
                    .collect();
                session.catalog_note = format!("{count} supported games");
                log.info(format!("Catalog refreshed: {count} supported games"));
                if session
                    .selected
                    .as_ref()
                    .is_some_and(|id| !session.games.iter().any(|game| &game.id == id))
                {
                    session.selected = None;
                    session.config.last_selected_discord_application_id = None;
                    let _ = ConfigStore::new(session.paths.config()).save(&session.config);
                }
                session.art_pending = true;
            }
            Err(error) => {
                log.info(format!("Metadata refresh failed: {error}"));
                session.catalog_note = if session.games.is_empty() {
                    "Game database unavailable. Use Settings to retry.".into()
                } else {
                    "Offline: using the cached game database".into()
                };
                toast(
                    &mut session,
                    "Refresh failed. The last good database is still here.",
                );
            }
        },
        Msg::Art {
            id,
            generation,
            path,
        } => {
            if generation == session.art_generation
                && let Some(path) = path
                && let Ok(image) = slint::Image::load_from_path(&path)
            {
                ART.with(|art| art.borrow_mut().insert(id, image));
            }
        }
        Msg::Launch(result) => {
            session.busy = false;
            match result {
                Ok(report) => {
                    session.clock.play(Instant::now());
                    log.info(format!(
                        "Runner diagnostic: PID={}, path={}, basename={}, workingDirectory={}, alive=true, HWND=0x{:X}, title={}, integrity={}",
                        report.pid,
                        report.executable.display(),
                        report.basename,
                        report.working_directory.display(),
                        report.hwnd,
                        report.title,
                        report.integrity
                    ));
                    watch_exit(report.generation, report.waiter, tx.clone());
                }
                Err(error) => {
                    log.info(format!("Launch failed: {error}"));
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
                        session.queue.fail(index, error.clone());
                    }
                    toast(&mut session, error);
                }
            }
        }
        Msg::Stopped(result) => {
            session.busy = false;
            if let Err(error) = result {
                log.info(format!("Stop failed: {error}"));
                toast(&mut session, error);
            }
            if session.quitting {
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
                log.info(format!(
                    "Runner exited unexpectedly (generation {generation})"
                ));
                session.clock.unexpected_exit();
                if let QueueActivity::Running { index } | QueueActivity::Paused { index } =
                    session.queue.activity()
                {
                    session.queue.fail(index, "The runner exited.");
                }
                session.busy = false;
                toast(&mut session, "The runner exited.");
            }
        }
    }
}

include!("commands.rs");
