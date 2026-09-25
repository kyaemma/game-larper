fn begin_play(
    session: &Arc<Mutex<Session>>,
    _log: &Log,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
) {
    let Ok(mut guard) = session.lock() else {
        return;
    };
    if guard.busy {
        return;
    }
    if matches!(
        guard.queue.activity(),
        QueueActivity::Running { .. }
            | QueueActivity::Paused { .. }
            | QueueActivity::Transition { .. }
    ) {
        toast(&mut guard, "Stop the queue before playing one game.");
        return;
    }
    let Some(id) = guard.selected.clone() else {
        toast(&mut guard, "Select a game first.");
        return;
    };
    let Some(game) = guard.games.iter().find(|game| game.id == id).cloned() else {
        toast(&mut guard, "That game is no longer supported.");
        return;
    };
    guard.busy = true;
    let host = guard.host.clone();
    let images = guard.paths.images();
    drop(guard);
    spawn_launch(host, images, game, tx.clone(), wake.clone());
}

fn begin_pause(
    session: &Arc<Mutex<Session>>,
    log: &Log,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
) {
    let action = session.lock().ok().and_then(|mut guard| {
        if guard.busy {
            return None;
        }
        let queued = matches!(
            guard.queue.activity(),
            QueueActivity::Running { .. } | QueueActivity::Transition { .. }
        );
        if queued {
            let action = guard.queue.pause(Instant::now());
            guard.clock.pause(Instant::now());
            Some(action)
        } else if guard.clock.state() == SessionState::Playing {
            guard.clock.pause(Instant::now());
            Some(QueueAction {
                stop_runner: true,
                launch_index: None,
            })
        } else {
            None
        }
    });
    if let Some(action) = action {
        dispatch_queue_action(session, log, tx, wake, action);
    }
}

fn begin_stop(
    session: &Arc<Mutex<Session>>,
    log: &Log,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
    clear_manual: bool,
) {
    let action = session.lock().ok().map(|mut guard| {
        let _ = clear_manual;
        let action = guard.queue.stop();
        guard.clock.stop();
        action
    });
    if let Some(action) = action {
        dispatch_queue_action(session, log, tx, wake, action);
    }
}

fn begin_quit(
    session: &Arc<Mutex<Session>>,
    log: &Log,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
) {
    if let Ok(mut guard) = session.lock() {
        guard.quitting = true;
        guard.queue.stop();
        guard.clock.stop();
        log.info("Quit requested");
    }
    dispatch_queue_action(
        session,
        log,
        tx,
        wake,
        QueueAction {
            stop_runner: true,
            launch_index: None,
        },
    );
}

fn dispatch_queue_action(
    session: &Arc<Mutex<Session>>,
    _log: &Log,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
    action: QueueAction,
) {
    let Ok(mut guard) = session.lock() else {
        return;
    };
    if action.launch_index.is_some() {
        guard.clock.stop();
    }
    let launch = action.launch_index.and_then(|index| {
        let item = guard.queue.items().get(index)?.clone();
        let game = guard
            .games
            .iter()
            .find(|game| game.id == item.application_id)
            .cloned();
        guard.selected = Some(item.application_id.clone());
        guard.config.last_selected_discord_application_id = Some(item.application_id);
        let _ = ConfigStore::new(guard.paths.config()).save(&guard.config);
        game.or_else(|| {
            toast(
                &mut guard,
                format!("{} is not in the current catalog.", item.name),
            );
            None
        })
    });
    if action.launch_index.is_some() && launch.is_none() {
        guard.busy = false;
        return;
    }
    if action.stop_runner || launch.is_some() {
        guard.busy = true;
    }
    let host = guard.host.clone();
    let images = guard.paths.images();
    drop(guard);
    if !action.stop_runner && launch.is_none() {
        return;
    }
    let tx = tx.clone();
    let wake = wake.clone();
    std::thread::spawn(move || {
        let stop_result = if action.stop_runner {
            host.lock().unwrap_or_else(|p| p.into_inner()).stop()
        } else {
            Ok(())
        };
        if stop_result.is_err() {
            let _ = tx.send(Msg::Stopped(stop_result));
            wake();
            return;
        }
        if let Some(game) = launch {
            spawn_launch_blocking(host, images, game, tx, wake);
        } else {
            let _ = tx.send(Msg::Stopped(Ok(())));
            wake();
        }
    });
}

fn spawn_launch(
    host: Arc<Mutex<RunnerHost>>,
    images: PathBuf,
    game: GameDefinition,
    tx: mpsc::Sender<Msg>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    std::thread::spawn(move || spawn_launch_blocking(host, images, game, tx, wake));
}

fn spawn_launch_blocking(
    host: Arc<Mutex<RunnerHost>>,
    images: PathBuf,
    game: GameDefinition,
    tx: mpsc::Sender<Msg>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    let template = runner_template();
    let relative = game
        .supported_path()
        .map(|path| path.to_string_lossy().replace('\\', "/"));
    let result = match (template, relative) {
        (Ok(template), Some(relative)) => {
            let icon = net::ensure_artwork(&images, &game);
            host.lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .launch(&template, &game.id, &relative, icon.as_deref())
        }
        (Err(error), _) => Err(error),
        (_, None) => Err("This game has no safe Windows executable.".into()),
    };
    let _ = tx.send(Msg::Launch(result));
    wake();
}

fn watch_exit(generation: u64, waiter: isize, tx: mpsc::Sender<Msg>) {
    std::thread::spawn(move || {
        unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(
                waiter as windows_sys::Win32::Foundation::HANDLE,
                windows_sys::Win32::System::Threading::INFINITE,
            );
            windows_sys::Win32::Foundation::CloseHandle(
                waiter as windows_sys::Win32::Foundation::HANDLE,
            );
        }
        let _ = tx.send(Msg::Exited { generation });
    });
}

fn spawn_refresh(
    tx: mpsc::Sender<Msg>,
    paths: AppPaths,
    log: Arc<Log>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    std::thread::spawn(move || {
        let cache = CatalogCache::new(paths.catalog());
        let result = net::refresh_catalog(&cache);
        if let Err(error) = &result {
            log.info(format!("Metadata refresh failed: {error}"));
        }
        let _ = tx.send(Msg::Catalog(result));
        wake();
    });
}

fn select_index(session: &Arc<Mutex<Session>>, log: &Log, index: usize, _play: bool) {
    let Ok(mut session) = session.lock() else {
        return;
    };
    let visible = game_larper_core::search(&session.games, &session.query, DEFAULT_SEARCH_LIMIT);
    let Some(game_index) = visible.get(index).copied() else {
        return;
    };
    let id = session.games[game_index].id.clone();
    let name = session.games[game_index].name.clone();
    session.selected = Some(id.clone());
    session.config.last_selected_discord_application_id = Some(id);
    session.art_pending = true;
    if let Err(error) = ConfigStore::new(session.paths.config()).save(&session.config) {
        log.info(format!("Config save failed: {error}"));
    } else {
        log.info(format!(
            "Selected game: {} {name}",
            session.games[game_index].id
        ));
    }
}

fn add_selected_to_queue(session: &Arc<Mutex<Session>>, ui: &Weak<MainWindow>) {
    let Some(ui) = ui.upgrade() else { return };
    let minutes: u64 = ui.get_minutes().parse().unwrap_or(0);
    let Ok(mut session) = session.lock() else {
        return;
    };
    if !(1..=24 * 60).contains(&minutes) {
        toast(&mut session, "Duration must be between 1 and 1440 minutes.");
        return;
    }
    let Some(id) = session.selected.clone() else {
        toast(&mut session, "Select a game first.");
        return;
    };
    let Some(game) = session.games.iter().find(|game| game.id == id) else {
        return;
    };
    let name = game.name.clone();
    let application_id = game.id.clone();
    match session
        .queue
        .add(&application_id, &name, Duration::from_secs(minutes * 60))
    {
        Ok(_) => {
            persist_queue(&mut session);
            session.queue_open = true;
            toast(&mut session, format!("Queued {name} for {minutes} min."));
        }
        Err(error) => toast(&mut session, error.to_string()),
    }
}

fn move_queue(session: &Arc<Mutex<Session>>, from: usize, to: usize) {
    if let Ok(mut session) = session.lock() {
        if let Err(error) = session.queue.move_item(from, to) {
            toast(&mut session, error.to_string());
        } else {
            persist_queue(&mut session);
        }
    }
}

fn arm_schedule(session: &Arc<Mutex<Session>>, ui: &Weak<MainWindow>) {
    let Some(ui) = ui.upgrade() else { return };
    let date = ui.get_schedule_date().to_string();
    let time = ui.get_schedule_time().to_string();
    let Ok(mut session) = session.lock() else {
        return;
    };
    let parsed = (|| {
        let date = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
            .map_err(|_| "Use YYYY-MM-DD.".to_string())?;
        let time = NaiveTime::parse_from_str(time.trim(), "%H:%M")
            .map_err(|_| "Use HH:MM.".to_string())?;
        Local
            .from_local_datetime(&date.and_time(time))
            .single()
            .map(|local| local.timestamp_millis())
            .ok_or_else(|| "That local time is ambiguous.".to_string())
    })();
    match parsed {
        Ok(at) => match session.queue.arm(at, unix_time_ms(SystemTime::now())) {
            Ok(()) => {
                persist_queue(&mut session);
                toast(&mut session, "Queue armed.");
            }
            Err(error) => toast(&mut session, error.to_string()),
        },
        Err(error) => toast(&mut session, error),
    }
}

fn save_settings(session: &Arc<Mutex<Session>>, ui: &Weak<MainWindow>, log: &Log) {
    let Some(ui) = ui.upgrade() else { return };
    let Ok(mut session) = session.lock() else {
        return;
    };
    let executable = std::env::current_exe();
    let next = AppConfig {
        schema_version: session.config.schema_version,
        launch_with_windows: ui.get_launch_with_windows(),
        start_minimized: ui.get_start_minimized(),
        close_to_tray: ui.get_close_to_tray(),
        restore_last_selected_game: ui.get_restore_last(),
        auto_resume: ui.get_auto_resume(),
        preserve_queue: ui.get_preserve_queue(),
        last_selected_discord_application_id: session
            .config
            .last_selected_discord_application_id
            .clone(),
    };
    if let Ok(executable) = executable
        && let Err(error) = platform::set_run_at_startup(
            next.launch_with_windows,
            next.start_minimized,
            &executable,
        )
    {
        log.info(format!("Startup registry error: {error}"));
        toast(&mut session, error);
        return;
    }
    if !next.preserve_queue {
        let _ = std::fs::remove_file(session.paths.queue());
    }
    session.config = next;
    if let Err(error) = ConfigStore::new(session.paths.config()).save(&session.config) {
        log.info(format!("Config save failed: {error}"));
        toast(&mut session, "Settings could not be saved.");
        return;
    }
    if session.config.preserve_queue {
        persist_queue(&mut session);
    }
    ui.set_settings_open(false);
    toast(&mut session, "Settings saved.");
}

fn persist_queue(session: &mut Session) {
    if session.config.preserve_queue {
        let _ = save_queue(&session.paths.queue(), &session.queue);
    }
}

fn queue_command(
    session: &Arc<Mutex<Session>>,
    log: &Log,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
    command: impl FnOnce(&mut QueueMachine) -> Result<QueueAction, game_larper_core::Error>,
) {
    let action = session
        .lock()
        .ok()
        .and_then(|mut guard| match command(&mut guard.queue) {
            Ok(action) => Some(action),
            Err(error) => {
                toast(&mut guard, error.to_string());
                None
            }
        });
    if let Some(action) = action {
        dispatch_queue_action(session, log, tx, wake, action);
    }
}

fn sync_settings_from_config(ui: &MainWindow, config: &AppConfig) {
    ui.set_launch_with_windows(config.launch_with_windows);
    ui.set_start_minimized(config.start_minimized);
    ui.set_close_to_tray(config.close_to_tray);
    ui.set_restore_last(config.restore_last_selected_game);
    ui.set_auto_resume(config.auto_resume);
    ui.set_preserve_queue(config.preserve_queue);
}

fn cached_art(id: &str) -> slint::Image {
    ART.with(|art| art.borrow().get(id).cloned().unwrap_or_default())
}

fn toast(session: &mut Session, message: impl AsRef<str>) {
    session.toast = message.as_ref().to_string();
    session.toast_until = Some(Instant::now() + Duration::from_secs(4));
}

fn displayed_time(session: &Session, now: Instant) -> String {
    if matches!(
        session.queue.activity(),
        QueueActivity::Running { .. } | QueueActivity::Paused { .. }
    ) {
        format_hms(session.queue.elapsed_in_item(now).unwrap_or_default())
    } else {
        format_hms(session.clock.elapsed(now))
    }
}

fn queue_status(session: &Session) -> String {
    match session.queue.activity() {
        QueueActivity::Idle => "Queue is idle.".into(),
        QueueActivity::Scheduled { at_unix_ms } => format!("Armed for {}", format_unix(at_unix_ms)),
        QueueActivity::Missed { at_unix_ms } => format!("Missed {}", format_unix(at_unix_ms)),
        QueueActivity::Running { index } => {
            format!("Playing {} of {}", index + 1, session.queue.items().len())
        }
        QueueActivity::Paused { index } => {
            format!("Paused on {} of {}", index + 1, session.queue.items().len())
        }
        QueueActivity::Transition { next_index } => format!("Switching to item {}", next_index + 1),
        QueueActivity::Failed { message, .. } => message,
    }
}

fn queue_rows(session: &Session, now: Instant) -> Vec<QueueRow> {
    let active = session.queue.active_index();
    session
        .queue
        .items()
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let meta = if Some(index) == active {
                format!(
                    "{} left",
                    format_hms(session.queue.remaining(now).unwrap_or(item.duration))
                )
            } else {
                format!("{} min", item.duration.as_secs() / 60)
            };
            QueueRow {
                name: item.name.clone().into(),
                meta: meta.into(),
                active: Some(index) == active,
            }
        })
        .collect()
}

fn art_targets(session: &Session) -> Vec<GameDefinition> {
    let mut games = Vec::new();
    if let Some(id) = &session.selected
        && let Some(game) = session.games.iter().find(|game| &game.id == id)
    {
        games.push(game.clone());
    }
    for index in game_larper_core::search(&session.games, &session.query, 8) {
        let game = &session.games[index];
        if games.iter().all(|existing| existing.id != game.id) {
            games.push(game.clone());
        }
    }
    games
}

fn detail_line(game: &GameDefinition) -> String {
    match &game.steam_app_id {
        Some(id) => format!("Steam {id} · Discord detected"),
        None => "Discord detected".into(),
    }
}

fn supported_count(games: &[GameDefinition]) -> usize {
    games
        .iter()
        .filter(|game| game.supported_path().is_some())
        .count()
}

fn format_unix(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .map(|time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".into())
}

fn runner_template() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let directory = executable
        .parent()
        .ok_or_else(|| "Cannot locate Game Larper.".to_string())?;
    for name in ["GameLarper.Runner.exe", "game-larper-runner.exe"] {
        let path = directory.join(name);
        if path.exists() {
            return Ok(path);
        }
    }
    Err("The bundled native runner is missing.".into())
}

fn round_after_show(ui: &MainWindow) {
    let handle = ui.window().window_handle();
    let Ok(handle) = handle.window_handle() else {
        return;
    };
    if let RawWindowHandle::Win32(window) = handle.as_raw() {
        let hwnd = window.hwnd.get() as windows_sys::Win32::Foundation::HWND;
        platform::round_corners(hwnd);
    }
}

enum ThemeColor {
    Success,
    Warning,
    Muted,
}

impl From<ThemeColor> for slint::Color {
    fn from(value: ThemeColor) -> Self {
        match value {
            ThemeColor::Success => slint::Color::from_rgb_u8(0x69, 0xDA, 0xA5),
            ThemeColor::Warning => slint::Color::from_rgb_u8(0xF5, 0xBE, 0x66),
            ThemeColor::Muted => slint::Color::from_rgb_u8(0xAA, 0xB3, 0xC5),
        }
    }
}
