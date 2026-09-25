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
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use crate::host::{LaunchReport, RunnerHost};
use crate::log::Log;
use crate::net;
use crate::panel::{Dock, Mode};
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Info,
    Success,
    Error,
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
    toast: String,
    toast_kind: ToastKind,
    toast_until: Option<Instant>,
    busy: bool,
    catalog_updated: Option<SystemTime>,
    refreshing: bool,
    manual_refresh: bool,
    offline: bool,
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
    let panel = SidePanel::new()?;
    let tray = TrayIcon::new()?;
    let dock = Dock::new(&ui, &panel);
    let (tx, rx) = mpsc::channel();
    let rx = Arc::new(Mutex::new(rx));
    let session = Arc::new(Mutex::new(load_session(&paths, &log)));
    let hits = Rc::new(VecModel::from(Vec::<Hit>::new()));
    let rows = Rc::new(VecModel::from(Vec::<QueueRow>::new()));
    ui.set_hits(ModelRc::from(hits.clone()));
    panel.set_queue_rows(ModelRc::from(rows.clone()));
    MODELS.with(|slot| {
        *slot.borrow_mut() = Some(Models {
            hits: hits.clone(),
            rows: rows.clone(),
        });
    });
    panel.set_version_label(env!("CARGO_PKG_VERSION").into());
    sync_settings_from_config(
        &panel,
        &session.lock().unwrap_or_else(|p| p.into_inner()).config,
    );
    render(&ui, &panel, &tray, &session);

    let wake: Arc<dyn Fn() + Send + Sync> = {
        let rx = rx.clone();
        let session = session.clone();
        let ui_weak = ui.as_weak();
        let panel_weak = panel.as_weak();
        let tray_weak = tray.as_weak();
        let log = log.clone();
        let tx = tx.clone();
        Arc::new(move || {
            let ui_weak = ui_weak.clone();
            let panel_weak = panel_weak.clone();
            let tray_weak = tray_weak.clone();
            let session = session.clone();
            let rx = rx.clone();
            let log = log.clone();
            let tx = tx.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let (Some(ui), Some(panel), Some(tray)) =
                    (ui_weak.upgrade(), panel_weak.upgrade(), tray_weak.upgrade())
                else {
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
                render(&ui, &panel, &tray, &session);
            });
        }) as Arc<dyn Fn() + Send + Sync>
    };

    wire(&ui, &panel, &tray, &dock, &session, &log, &tx, &wake);
    platform::watch_activation({
        let ui_weak = ui.as_weak();
        move || {
            let ui_weak = ui_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    show_main(&ui);
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
        style_main(&ui);
    }
    let first_download = {
        let mut session = session.lock().unwrap_or_else(|p| p.into_inner());
        session.refreshing = session.games.is_empty();
        session.refreshing
    };
    if first_download {
        spawn_refresh(tx.clone(), paths.clone(), log.clone(), wake.clone());
    }

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
            render(&ui, &panel, &tray, &session);
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
        toast: String::new(),
        toast_kind: ToastKind::Info,
        toast_until: None,
        busy: false,
        catalog_updated: updated,
        refreshing: false,
        manual_refresh: false,
        offline: false,
        art_generation: 1,
        art_pending: true,
        restored: false,
        quitting: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn wire(
    ui: &MainWindow,
    panel: &SidePanel,
    tray: &TrayIcon,
    dock: &Rc<Dock>,
    session: &Arc<Mutex<Session>>,
    log: &Arc<Log>,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
) {
    ui.on_query_edited({
        let session = session.clone();
        let wake = wake.clone();
        move |text| {
            if let Ok(mut session) = session.lock() {
                session.query = text.to_string();
                session.art_pending = true;
            }
            wake();
        }
    });
    ui.on_step({
        let session = session.clone();
        let log = log.clone();
        let wake = wake.clone();
        move |delta| {
            let next = session.lock().ok().and_then(|session| {
                let shown = visible(&session);
                let last = shown.len().checked_sub(1)?;
                let current = session.selected.as_ref().and_then(|id| {
                    shown
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
                select_index(&session, &log, index);
            }
            wake();
        }
    });
    ui.on_choose({
        let session = session.clone();
        let log = log.clone();
        let wake = wake.clone();
        move |index| {
            select_index(&session, &log, index as usize);
            wake();
        }
    });
    ui.on_activate({
        let session = session.clone();
        let log = log.clone();
        let tx = tx.clone();
        let wake = wake.clone();
        move |index| {
            select_index(&session, &log, index as usize);
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
            let action = session
                .lock()
                .ok()
                .map(|mut session| session.queue.resume(Instant::now()));
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
                spawn_refresh(tx.clone(), paths, log.clone(), wake.clone());
            }
            wake();
        }
    };
    ui.on_refresh_catalog(refresh.clone());
    panel.on_refresh_catalog(refresh);
    ui.on_add_to_queue({
        let session = session.clone();
        let log = log.clone();
        let wake = wake.clone();
        let panel = panel.as_weak();
        move |index| {
            select_index(&session, &log, index as usize);
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
    panel.on_clear_images({
        let session = session.clone();
        let log = log.clone();
        let wake = wake.clone();
        move || {
            if let Ok(mut session) = session.lock() {
                match net::clear_artwork(&session.paths.images()) {
                    Ok(count) => {
                        ART.with(|art| art.borrow_mut().clear());
                        session.art_pending = true;
                        toast_ok(&mut session, format!("Cleared {count} cached images."));
                    }
                    Err(error) => {
                        log.info(&error);
                        toast_err(&mut session, error);
                    }
                }
            }
            wake();
        }
    });
    panel.on_open_logs({
        let session = session.clone();
        move || {
            if let Ok(session) = session.lock() {
                let _ = platform::open_in_explorer(&session.paths.logs());
            }
        }
    });
    panel.on_open_data({
        let session = session.clone();
        move || {
            if let Ok(session) = session.lock() {
                let _ = platform::open_in_explorer(&session.paths.root);
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
                if let Err(error) = session.queue.remove(index as usize) {
                    toast_err(&mut session, error.to_string());
                } else {
                    persist_queue(&mut session);
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
                let action = session.queue.clear();
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
            queue_command(&session, &log, &tx, &wake, |queue| {
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
            let action = session
                .lock()
                .ok()
                .map(|mut session| session.queue.skip(Instant::now()));
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
                    toast_err(&mut session, error.to_string());
                } else {
                    persist_queue(&mut session);
                    toast(&mut session, "Schedule cleared.");
                }
            }
            wake();
        }
    });
    panel.on_close_panel({
        let dock = dock.clone();
        move || dock.close()
    });
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
                dock.dismiss();
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
    let Ok(session) = session.lock() else {
        return;
    };
    let now = Instant::now();
    let shown = visible(&session);
    MODELS.with(|models| {
        if let Some(models) = models.borrow().as_ref() {
            sync_model(&models.hits, hit_rows(&session, &shown));
            sync_model(&models.rows, queue_rows(&session, now));
        }
    });
    render_main(ui, &session, &shown, now);
    render_panel(panel, &session, now);
    render_tray(tray, &session, now);
}

fn render_main(ui: &MainWindow, session: &Session, shown: &[usize], now: Instant) {
    let unavailable = session.games.is_empty() && !session.refreshing;
    ui.set_results_state(
        if unavailable {
            "unavailable"
        } else if session.query.trim().is_empty() {
            "idle"
        } else if shown.is_empty() {
            "empty"
        } else {
            "ready"
        }
        .into(),
    );
    let selected = selected_game(session);
    ui.set_selected_index(
        selected
            .and_then(|game| {
                shown
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
    let (label, mode, tone) = match state {
        SessionState::Playing => ("Playing", "playing", Tone::Success),
        SessionState::Paused => ("Paused", "paused", Tone::Warning),
        SessionState::Stopped => ("Ready", "stopped", Tone::Muted),
    };
    ui.set_session_visible(selected.is_some());
    ui.set_session_state(label.into());
    ui.set_session_mode(mode.into());
    ui.set_session_color(tone.into());
    ui.set_session_time(displayed_time(session, now).into());
    ui.set_session_note(queue_note(session, now).into());
    if let Some(game) = selected {
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
    let selected = selected_game(session);
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
                    session.offline = false;
                    session.catalog_updated = Some(SystemTime::now());
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
                    if manual {
                        toast_ok(
                            &mut session,
                            format!("Game list updated · {} games", group_digits(count)),
                        );
                    }
                }
                Err(error) => {
                    log.info(format!("Metadata refresh failed: {error}"));
                    session.offline = !session.games.is_empty();
                    toast_err(
                        &mut session,
                        "Refresh failed. The last good database is still here.",
                    );
                }
            }
        }
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
                    toast_err(&mut session, error);
                }
            }
        }
        Msg::Stopped(result) => {
            session.busy = false;
            if let Err(error) = result {
                log.info(format!("Stop failed: {error}"));
                toast_err(&mut session, error);
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
                toast_err(&mut session, "The runner exited.");
            }
        }
    }
}

include!("commands.rs");
