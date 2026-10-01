fn begin_play(
    session: &Arc<Mutex<Session>>,
    log: &Arc<Log>,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
) {
    let Ok(mut guard) = session.lock() else {
        return;
    };
    if guard.busy {
        log.debug(Area::Session, "Play ignored: a launch or stop is in progress");
        return;
    }
    if matches!(
        guard.queue.activity(),
        QueueActivity::Running { .. }
            | QueueActivity::Paused { .. }
            | QueueActivity::Transition { .. }
    ) {
        log.debug(Area::Session, "Play ignored: the queue owns the runner");
        toast(&mut guard, "Stop the queue before playing one game.");
        return;
    }
    let Some(id) = guard.selected.clone() else {
        log.debug(Area::Session, "Play ignored: no game selected");
        toast(&mut guard, "Select a game first.");
        return;
    };
    let Some(game) = guard.games.iter().find(|game| game.id == id).cloned() else {
        log.warn(
            Area::Session,
            format!("Play refused: application {id} is not a supported game in the catalog"),
        );
        toast_err(&mut guard, "That game is no longer supported.");
        return;
    };
    log.info(
        Area::Session,
        format!("Play requested: {} ({})", game.name, game.id),
    );
    guard.busy = true;
    guard.active = Some(game.id.clone());
    let host = guard.host.clone();
    let images = guard.paths.images();
    drop(guard);
    spawn_launch(host, images, game, log.clone(), tx.clone(), wake.clone());
}

fn begin_pause(
    session: &Arc<Mutex<Session>>,
    log: &Arc<Log>,
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
            log.info(Area::Queue, "Pause requested from the transport controls");
            let action = guard.queue.pause(Instant::now());
            guard.clock.pause(Instant::now());
            Some(action)
        } else if guard.clock.state() == SessionState::Playing {
            log.info(Area::Session, "Pause requested");
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
    log: &Arc<Log>,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
    clear_manual: bool,
) {
    let action = session.lock().ok().map(|mut guard| {
        let queue_active = guard.queue.active_index().is_some();
        if clear_manual {
            log.info(Area::Session, "Stop requested");
        }
        if queue_active {
            log.info(Area::Queue, "Queue stopped");
        }
        let mut action = guard.queue.stop();
        guard.clock.stop();
        // The queue only reports its own runner. A manual session owns one too.
        action.stop_runner |= clear_manual;
        action
    });
    if let Some(action) = action {
        dispatch_queue_action(session, log, tx, wake, action);
    }
}

fn begin_quit(
    session: &Arc<Mutex<Session>>,
    log: &Arc<Log>,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
) {
    if let Ok(mut guard) = session.lock() {
        guard.quitting = true;
        guard.queue.stop();
        guard.clock.stop();
        log.info(Area::App, "Quit requested");
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

fn should_stop_clock_for_dispatch(
    activity: &QueueActivity,
    state: SessionState,
    action: &QueueAction,
) -> bool {
    if action.launch_index.is_some() {
        return true;
    }
    if !action.stop_runner {
        return false;
    }

    // A real pause intentionally keeps the clock and active identity around so Resume can
    // continue the same session. Every other stop means there is no runner left to call
    // "Playing": queue gaps, natural queue completion, failures, and explicit stops.
    !matches!(activity, QueueActivity::Paused { .. })
        && !(matches!(activity, QueueActivity::Idle) && state == SessionState::Paused)
}

fn dispatch_queue_action(
    session: &Arc<Mutex<Session>>,
    log: &Arc<Log>,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
    action: QueueAction,
) {
    let Ok(mut guard) = session.lock() else {
        return;
    };
    if action.stop_runner || action.launch_index.is_some() {
        log.debug(
            Area::Session,
            format!(
                "Runner action: stop={} launch={}",
                action.stop_runner,
                action
                    .launch_index
                    .map_or("none".to_string(), |index| format!("item {}", index + 1))
            ),
        );
    }
    let activity = guard.queue.activity();
    if should_stop_clock_for_dispatch(&activity, guard.clock.state(), &action) {
        guard.clock.stop();
        if action.launch_index.is_none() {
            guard.active = None;
        }
    }
    let launch = action.launch_index.and_then(|index| {
        let item = guard.queue.items().get(index)?.clone();
        let game = guard
            .games
            .iter()
            .find(|game| game.id == item.application_id)
            .cloned();
        guard.selected = Some(item.application_id.clone());
        guard.active = Some(item.application_id.clone());
        guard.config.last_selected_discord_application_id = Some(item.application_id.clone());
        if let Err(error) = ConfigStore::new(guard.paths.config()).save(&guard.config) {
            log.error(Area::Settings, format!("Config save failed: {error}"));
        }
        game.or_else(|| {
            log.warn(
                Area::Queue,
                format!(
                    "Item {} ({}, {}) is not a supported game in the current catalog",
                    index + 1,
                    item.name,
                    item.application_id
                ),
            );
            toast_err(
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
    if let (Some(index), Some(game)) = (action.launch_index, launch.as_ref()) {
        log.info(
            Area::Queue,
            format!(
                "Starting item {} of {}: {} ({})",
                index + 1,
                guard.queue.items().len(),
                game.name,
                game.id
            ),
        );
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
    let log = log.clone();
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
            spawn_launch_blocking(host, images, game, log, tx, wake);
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
    log: Arc<Log>,
    tx: mpsc::Sender<Msg>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    std::thread::spawn(move || spawn_launch_blocking(host, images, game, log, tx, wake));
}

fn spawn_launch_blocking(
    host: Arc<Mutex<RunnerHost>>,
    images: PathBuf,
    game: GameDefinition,
    log: Arc<Log>,
    tx: mpsc::Sender<Msg>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    let template = runner_template();
    let relative = game
        .supported_path()
        .map(|path| path.to_string_lossy().replace('\\', "/"));
    let result = match (template, relative) {
        (Ok(template), Some(relative)) => {
            log.info(
                Area::Runner,
                format!(
                    "Launching {} for application {} as {relative}",
                    game.name, game.id
                ),
            );
            log.debug(
                Area::Runner,
                format!(
                    "Template {}; {} Windows executable rule(s) in the catalog",
                    redact(&template),
                    game.executable_rules()
                ),
            );
            let icon = net::ensure_artwork(&images, &game, &log);
            host.lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .launch(&template, &game.id, &relative, icon.as_deref())
        }
        (Err(error), _) => {
            log.error(Area::Runner, format!("Cannot launch {}: {error}", game.name));
            Err(error)
        }
        (_, None) => {
            log.error(
                Area::Runner,
                format!(
                    "Cannot launch {} ({}): no safe Windows executable rule",
                    game.name, game.id
                ),
            );
            Err("This game has no safe Windows executable.".into())
        }
    };
    let _ = tx.send(Msg::Launch(result));
    wake();
}

/// Report the runner's exit, requested or not, once its exit watch fires.
fn watch_exit(generation: u64, watch: ExitWatch, tx: mpsc::Sender<Msg>) {
    host::watch_exit(watch, move || {
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
        let started = Instant::now();
        log.info(Area::Catalog, "Refreshing the game database from Discord");
        let result = net::refresh_catalog(&cache, &log);
        log.debug(
            Area::Catalog,
            format!(
                "Refresh {} after {} ms",
                if result.is_ok() { "finished" } else { "failed" },
                started.elapsed().as_millis()
            ),
        );
        let _ = tx.send(Msg::Catalog(result));
        wake();
    });
}

fn select_index(
    session: &Arc<Mutex<Session>>,
    log: &Arc<Log>,
    tx: &mpsc::Sender<Msg>,
    wake: &Wake,
    index: usize,
) {
    let Ok(mut session) = session.lock() else {
        return;
    };
    // Indexes refer to the rows on screen, which may still be the previous query's.
    let Some(game) = session
        .shown
        .get(index)
        .and_then(|&index| session.games.get(index))
        .cloned()
    else {
        return;
    };
    if session.selected.as_deref() == Some(game.id.as_str()) {
        return;
    }
    session.selected = Some(game.id.clone());
    session.config.last_selected_discord_application_id = Some(game.id.clone());
    log.debug(
        Area::Session,
        format!("Selected {} / {}", game.id, game.name),
    );
    if let Err(error) = ConfigStore::new(session.paths.config()).save(&session.config) {
        log.error(Area::Settings, format!("Config save failed: {error}"));
    }
    fetch_art(&mut session, std::slice::from_ref(&game), tx, wake, true);
}

fn add_selected_to_queue(session: &Arc<Mutex<Session>>, panel: &Weak<SidePanel>) {
    let minutes: u64 = panel
        .upgrade()
        .map(|panel| panel.get_minutes().trim().parse().unwrap_or(0))
        .unwrap_or(30);
    let Ok(mut session) = session.lock() else {
        return;
    };
    if !(1..=24 * 60).contains(&minutes) {
        session.log.debug(
            Area::Queue,
            format!("Add refused: {minutes} is not a 1-1440 minute duration"),
        );
        toast_err(&mut session, "Duration must be between 1 and 1440 minutes.");
        return;
    }
    let Some(id) = session.selected.clone() else {
        session.log.debug(Area::Queue, "Add ignored: no game selected");
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
            session.log.info(
                Area::Queue,
                format!(
                    "Added {name} ({application_id}) for {minutes} min; {} item(s) queued",
                    session.queue.items().len()
                ),
            );
            persist_queue(&mut session);
            toast_ok(&mut session, format!("Queued {name} for {minutes} min."));
        }
        Err(error) => {
            session.log.warn(
                Area::Queue,
                format!("Adding {name} ({application_id}) refused: {error}"),
            );
            toast_err(&mut session, error.to_string());
        }
    }
}

fn move_queue(session: &Arc<Mutex<Session>>, from: usize, to: usize) {
    if let Ok(mut session) = session.lock() {
        if let Err(error) = session.queue.move_item(from, to) {
            session
                .log
                .warn(Area::Queue, format!("Reorder refused: {error}"));
            toast_err(&mut session, error.to_string());
        } else {
            session.log.debug(
                Area::Queue,
                format!("Moved item {} to position {}", from + 1, to + 1),
            );
            persist_queue(&mut session);
        }
    }
}

fn arm_schedule(session: &Arc<Mutex<Session>>, panel: &Weak<SidePanel>) {
    let Some(panel) = panel.upgrade() else { return };
    let date = panel.get_schedule_date().to_string();
    let time = panel.get_schedule_time().to_string();
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
                session.log.info(
                    Area::Queue,
                    format!(
                        "Schedule armed for {} ({} item(s))",
                        format_unix(at),
                        session.queue.items().len()
                    ),
                );
                persist_queue(&mut session);
                toast_ok(&mut session, "Queue armed.");
            }
            Err(error) => {
                session
                    .log
                    .warn(Area::Queue, format!("Schedule refused: {error}"));
                toast_err(&mut session, error.to_string());
            }
        },
        Err(error) => {
            session.log.debug(
                Area::Queue,
                format!("Schedule input {date:?} {time:?} rejected: {error}"),
            );
            toast_err(&mut session, error);
        }
    }
}

/// Apply the staged settings. Returns false and keeps the panel open when something failed.
fn save_settings(session: &Arc<Mutex<Session>>, panel: &Weak<SidePanel>, log: &Log) -> bool {
    let Some(panel) = panel.upgrade() else {
        return false;
    };
    let Ok(mut session) = session.lock() else {
        return false;
    };
    let executable = std::env::current_exe();
    let next = AppConfig {
        schema_version: session.config.schema_version,
        launch_with_windows: panel.get_launch_with_windows(),
        start_minimized: panel.get_start_minimized(),
        close_to_tray: panel.get_close_to_tray(),
        restore_last_selected_game: panel.get_restore_last(),
        auto_resume: panel.get_auto_resume(),
        preserve_queue: panel.get_preserve_queue(),
        last_selected_discord_application_id: session
            .config
            .last_selected_discord_application_id
            .clone(),
    };
    match executable {
        Ok(executable) => match platform::set_run_at_startup(
            next.launch_with_windows,
            next.start_minimized,
            &executable,
        ) {
            Ok(()) => log.debug(
                Area::Settings,
                if next.launch_with_windows {
                    format!(
                        "Startup entry registered for {}{}",
                        redact(&executable),
                        if next.start_minimized {
                            " --minimized"
                        } else {
                            ""
                        }
                    )
                } else {
                    "No startup entry (launch with Windows is off)".to_string()
                },
            ),
            Err(error) => {
                log.error(
                    Area::Settings,
                    format!("Startup registration failed: {error}"),
                );
                toast_err(&mut session, error);
                return false;
            }
        },
        Err(error) => log.warn(
            Area::Settings,
            format!("Startup entry left unchanged; executable path unknown: {error}"),
        ),
    }
    if !next.preserve_queue {
        match std::fs::remove_file(session.paths.queue()) {
            Ok(()) => log.debug(Area::Queue, "Saved queue removed (preserve queue is off)"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log.warn(
                Area::Files,
                format!("Could not remove the saved queue: {error}"),
            ),
        }
    }
    session.config = next;
    if let Err(error) = ConfigStore::new(session.paths.config()).save(&session.config) {
        log.error(Area::Settings, format!("Config save failed: {error}"));
        toast_err(&mut session, "Settings could not be saved.");
        return false;
    }
    log.info(
        Area::Settings,
        format!("Saved: {}", describe_settings(&session.config)),
    );
    if session.config.preserve_queue {
        persist_queue(&mut session);
    }
    panel.set_settings_dirty(false);
    toast_ok(&mut session, "Settings saved.");
    true
}

fn persist_queue(session: &mut Session) {
    if session.config.preserve_queue
        && let Err(error) = save_queue(&session.paths.queue(), &session.queue)
    {
        session
            .log
            .error(Area::Files, format!("Queue save failed: {error}"));
    }
}

/// Run a queue command named `label` (for the log) and apply what it asks of the runner.
fn queue_command(
    session: &Arc<Mutex<Session>>,
    log: &Arc<Log>,
    tx: &mpsc::Sender<Msg>,
    wake: &Arc<dyn Fn() + Send + Sync>,
    label: &str,
    command: impl FnOnce(&mut QueueMachine) -> Result<QueueAction, game_larper_core::Error>,
) {
    let action = session
        .lock()
        .ok()
        .and_then(|mut guard| match command(&mut guard.queue) {
            Ok(action) => {
                log.info(
                    Area::Queue,
                    format!("{label}: {} item(s)", guard.queue.items().len()),
                );
                Some(action)
            }
            Err(error) => {
                log.warn(Area::Queue, format!("{label} refused: {error}"));
                toast_err(&mut guard, error.to_string());
                None
            }
        });
    if let Some(action) = action {
        dispatch_queue_action(session, log, tx, wake, action);
    }
}

/// Log lines for a queue state change made by the clock, oldest first.
///
/// Commands log themselves; this covers what happens on its own: a schedule firing, an item
/// running out its time, the gap between items, and the queue finishing.
fn queue_tick_events(
    before: &QueueActivity,
    after: &QueueActivity,
    items: &[QueueItem],
    gap: Duration,
) -> Vec<(LogLevel, String)> {
    let mut events = Vec::new();
    match before {
        QueueActivity::Scheduled { at_unix_ms } => events.push((
            LogLevel::Info,
            format!("Scheduled start ({}) reached", format_unix(*at_unix_ms)),
        )),
        QueueActivity::Running { index } => {
            let item = items.get(*index);
            events.push((
                LogLevel::Info,
                format!(
                    "Item {} ({}) completed its {}",
                    index + 1,
                    item.map_or("unknown", |item| item.name.as_str()),
                    item.map_or_else(|| "time".to_string(), |item| format_minutes(item.duration))
                ),
            ));
        }
        _ => {}
    }
    match after {
        QueueActivity::Transition { next_index } => events.push((
            LogLevel::Debug,
            format!(
                "Waiting {} ms before item {}",
                gap.as_millis(),
                next_index + 1
            ),
        )),
        QueueActivity::Idle if matches!(before, QueueActivity::Running { .. }) => events.push((
            LogLevel::Success,
            format!("Queue finished all {} item(s)", items.len()),
        )),
        QueueActivity::Running { .. } => {}
        other => events.push((
            LogLevel::Debug,
            format!("Queue is now {}", describe_activity(other)),
        )),
    }
    events
}

/// A queue state for the log, with 1-based item numbers.
fn describe_activity(activity: &QueueActivity) -> String {
    match activity {
        QueueActivity::Idle => "idle".into(),
        QueueActivity::Scheduled { at_unix_ms } => {
            format!("scheduled for {}", format_unix(*at_unix_ms))
        }
        QueueActivity::Missed { at_unix_ms } => {
            format!("waiting after a missed start ({})", format_unix(*at_unix_ms))
        }
        QueueActivity::Running { index } => format!("running item {}", index + 1),
        QueueActivity::Paused { index } => format!("paused on item {}", index + 1),
        QueueActivity::Transition { next_index } => {
            format!("switching to item {}", next_index + 1)
        }
        QueueActivity::Failed { index, message } => {
            format!("failed on item {}: {message}", index + 1)
        }
    }
}

/// The user-facing switches, for the log. The last selected game is logged where it changes.
fn describe_settings(config: &AppConfig) -> String {
    let flag = |value: bool| if value { "on" } else { "off" };
    format!(
        "launch with Windows {}, start minimized {}, close to tray {}, restore last game {}, auto-resume {}, preserve queue {}",
        flag(config.launch_with_windows),
        flag(config.start_minimized),
        flag(config.close_to_tray),
        flag(config.restore_last_selected_game),
        flag(config.auto_resume),
        flag(config.preserve_queue)
    )
}

fn sync_settings_from_config(panel: &SidePanel, config: &AppConfig) {
    panel.set_launch_with_windows(config.launch_with_windows);
    panel.set_start_minimized(config.start_minimized);
    panel.set_close_to_tray(config.close_to_tray);
    panel.set_restore_last(config.restore_last_selected_game);
    panel.set_auto_resume(config.auto_resume);
    panel.set_preserve_queue(config.preserve_queue);
    panel.set_settings_dirty(false);
}

// ---- Results and artwork ------------------------------------------------------------------

/// Recompute the rows for the current query.
///
/// When the first rows have no artwork in memory yet, the new list waits (up to a short
/// deadline) so rows appear with their pictures. Until then the previous rows stay, or a
/// skeleton shows when there were none.
fn plan_results(session: &mut Session) -> ArtPlan {
    session.reveal = None;
    if session.query.trim().is_empty() {
        session.shown.clear();
        session.results = Results::Idle;
        return ArtPlan::default();
    }
    if !session.loaded {
        session.shown.clear();
        session.results = Results::Pending;
        return ArtPlan::default();
    }
    let found = game_larper_core::search(&session.games, &session.query, DEFAULT_SEARCH_LIMIT);
    if found.is_empty() {
        session.shown.clear();
        session.results = Results::Empty;
        return ArtPlan::default();
    }
    let wanted = |range: &[usize]| -> Vec<GameDefinition> {
        range
            .iter()
            .map(|&index| &session.games[index])
            .filter(|game| needs_art(session, game))
            .cloned()
            .collect()
    };
    let split = found.len().min(REVEAL_ROWS);
    let plan = ArtPlan {
        first: wanted(&found[..split]),
        rest: wanted(&found[split..]),
    };
    if plan.first.is_empty() {
        session.shown = found;
        session.results = Results::Ready;
        return plan;
    }
    if session.results != Results::Ready || session.shown.is_empty() {
        session.shown.clear();
        session.results = Results::Pending;
    }
    session.reveal = Some(Reveal {
        shown: found,
        waiting: plan.first.iter().map(|game| game.id.clone()).collect(),
        deadline: Instant::now() + ART_DEBOUNCE + REVEAL_WAIT,
    });
    plan
}

fn settle_reveal(session: &mut Session, now: Instant) {
    let ready = session
        .reveal
        .as_ref()
        .is_some_and(|reveal| reveal.waiting.is_empty() || now >= reveal.deadline);
    if ready && let Some(reveal) = session.reveal.take() {
        session.shown = reveal.shown;
        session.results = Results::Ready;
    }
}

/// Fetch the artwork a settled query needs, then make sure the reveal deadline renders.
fn start_art(
    session: &Arc<Mutex<Session>>,
    tx: &mpsc::Sender<Msg>,
    wake: &Wake,
    first: Vec<GameDefinition>,
    rest: Vec<GameDefinition>,
) {
    if let Ok(mut session) = session.lock() {
        fetch_art(&mut session, &first, tx, wake, true);
        fetch_art(&mut session, &rest, tx, wake, false);
    }
    let wake = wake.clone();
    Timer::single_shot(REVEAL_WAIT + Duration::from_millis(20), move || wake());
}

fn needs_art(session: &Session, game: &GameDefinition) -> bool {
    !ART.with(|art| art.borrow().contains(&game.id))
        && !session.art_failed.contains(&game.id)
        && !net::artwork_candidates(game).is_empty()
}

/// Start fetching artwork that is not cached, failed, or already on its way.
/// `parallel` gives each game its own worker; otherwise one worker goes through the list.
fn fetch_art(
    session: &mut Session,
    games: &[GameDefinition],
    tx: &mpsc::Sender<Msg>,
    wake: &Wake,
    parallel: bool,
) {
    let todo: Vec<GameDefinition> = games
        .iter()
        .filter(|game| needs_art(session, game) && !session.art_inflight.contains(&game.id))
        .cloned()
        .collect();
    if todo.is_empty() {
        return;
    }
    for game in &todo {
        session.art_inflight.insert(game.id.clone());
    }
    session.log.debug(
        Area::Art,
        format!(
            "Fetching artwork for {} game(s){}",
            todo.len(),
            if parallel { " in parallel" } else { "" }
        ),
    );
    let images = session.paths.images();
    let log = session.log.clone();
    let run = move |games: Vec<GameDefinition>, tx: mpsc::Sender<Msg>, wake: Wake| {
        for game in games {
            let identity = net::artwork_identity(&game);
            let path = net::ensure_artwork(&images, &game, &log);
            let _ = tx.send(Msg::Art {
                id: game.id,
                identity,
                path,
            });
            wake();
        }
    };
    if parallel {
        for game in todo {
            let run = run.clone();
            let tx = tx.clone();
            let wake = wake.clone();
            std::thread::spawn(move || run(vec![game], tx, wake));
        }
    } else {
        let tx = tx.clone();
        let wake = wake.clone();
        std::thread::spawn(move || run(todo, tx, wake));
    }
}

fn cached_art(id: &str) -> slint::Image {
    ART.with(|art| art.borrow_mut().get(id).cloned().unwrap_or_default())
}

/// Artwork the UI is drawing or about to draw. It stays in memory however full the cache is.
fn art_pinned(session: &Session, id: &str) -> bool {
    let shows = |index: &usize| session.games.get(*index).is_some_and(|game| game.id == id);
    session.selected.as_deref() == Some(id)
        || session.active.as_deref() == Some(id)
        || session
            .queue
            .items()
            .iter()
            .any(|item| item.application_id == id)
        || session.shown.iter().any(shows)
        || session
            .reveal
            .as_ref()
            .is_some_and(|reveal| reveal.shown.iter().any(shows))
}

/// Forget decoded artwork whose game left the catalog or whose artwork source changed.
fn reconcile_art(session: &Session) {
    ART.with(|art| {
        art.borrow_mut().reconcile(|id| {
            session
                .games
                .iter()
                .find(|game| game.id == id)
                .map(net::artwork_identity)
        });
    });
}

/// Bring the console model up to date with the live history. Only new lines are added, so an
/// idle console costs nothing per tick and a busy one does not rebuild a thousand rows.
fn render_logs(log: &Log) {
    MODELS.with(|models| {
        let models = models.borrow();
        let Some(models) = models.as_ref() else {
            return;
        };
        let mut cursor = models.log_cursor.get();
        match log.changes_since(&mut cursor) {
            LogChanges::None => {}
            LogChanges::Reset(entries) => {
                models
                    .logs
                    .set_vec(entries.into_iter().map(log_row).collect::<Vec<_>>());
            }
            LogChanges::Append(entries) => {
                for entry in entries {
                    models.logs.push(log_row(entry));
                }
                let excess = models.logs.row_count().saturating_sub(MAX_HISTORY);
                for _ in 0..excess {
                    models.logs.remove(0);
                }
            }
        }
        models.log_cursor.set(cursor);
    });
}

fn log_row(entry: LogEntry) -> LogRow {
    let tone = match entry.level {
        LogLevel::Debug => Tone::Debug,
        LogLevel::Info => Tone::Accent,
        LogLevel::Success => Tone::Success,
        LogLevel::Warn => Tone::Warning,
        LogLevel::Error => Tone::Danger,
    };
    LogRow {
        time: entry.time.into(),
        level: entry.level.label().into(),
        area: entry.area.label().into(),
        message: entry.message.into(),
        tone: tone.into(),
        quiet: entry.level == LogLevel::Debug,
    }
}

fn show_log_window(window: &LogWindow, log: &Log) {
    let reopening = window.window().is_visible();
    render_logs(log);
    match window.show() {
        Ok(()) => {
            if !reopening {
                log.debug(Area::App, "Log console opened");
            }
            platform::style_frame(window.window());
        }
        Err(error) => log.error(Area::App, format!("Log console failed to open: {error}")),
    }
}

fn hide_log_window(window: &LogWindow, log: &Log) {
    if window.window().is_visible() {
        log.debug(Area::App, "Log console closed");
    }
    if let Err(error) = window.hide() {
        log.error(Area::App, format!("Log console failed to close: {error}"));
    }
}

fn hit_rows(session: &Session) -> Vec<Hit> {
    session
        .shown
        .iter()
        .filter_map(|&index| session.games.get(index))
        .map(|game| Hit {
            id: game.id.clone().into(),
            steam_id: game.steam_app_id.clone().unwrap_or_default().into(),
            name: game.name.clone().into(),
            art: cached_art(&game.id),
            selected: session.selected.as_deref() == Some(game.id.as_str()),
        })
        .collect()
}

/// Update rows in place so delegates, hover state, and scroll position survive a render.
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() != rows.len() {
        model.set_vec(rows);
        return;
    }
    for (index, row) in rows.into_iter().enumerate() {
        if model.row_data(index).as_ref() != Some(&row) {
            model.set_row_data(index, row);
        }
    }
}

// ---- Presentation strings ---------------------------------------------------------------

fn toast(session: &mut Session, message: impl AsRef<str>) {
    show_toast(session, ToastKind::Info, message);
}

fn toast_ok(session: &mut Session, message: impl AsRef<str>) {
    show_toast(session, ToastKind::Success, message);
}

fn toast_err(session: &mut Session, message: impl AsRef<str>) {
    show_toast(session, ToastKind::Error, message);
}

fn show_toast(session: &mut Session, kind: ToastKind, message: impl AsRef<str>) {
    session.toast = message.as_ref().to_string();
    session.toast_kind = kind;
    session.toast_until = Some(Instant::now() + Duration::from_secs(4));
}

fn selected_game(session: &Session) -> Option<&GameDefinition> {
    session
        .selected
        .as_ref()
        .and_then(|id| session.games.iter().find(|game| &game.id == id))
}

/// The game the dock shows: the one running or paused, else the selection.
fn session_game(session: &Session) -> Option<&GameDefinition> {
    if let QueueActivity::Transition { next_index } = session.queue.activity()
        && let Some(item) = session.queue.items().get(next_index)
    {
        return session
            .games
            .iter()
            .find(|game| game.id == item.application_id)
            .or_else(|| selected_game(session));
    }
    if session.clock.state() == SessionState::Stopped {
        return selected_game(session);
    }
    session
        .active
        .as_ref()
        .and_then(|id| session.games.iter().find(|game| &game.id == id))
        .or_else(|| selected_game(session))
}

/// Resume means the paused game, even if another row got selected meanwhile.
fn reselect_paused(session: &mut Session, log: &Log) {
    if session.clock.state() != SessionState::Paused {
        return;
    }
    let Some(active) = session.active.clone() else {
        return;
    };
    if session.selected.as_ref() == Some(&active) {
        return;
    }
    session.selected = Some(active.clone());
    session.config.last_selected_discord_application_id = Some(active);
    if let Err(error) = ConfigStore::new(session.paths.config()).save(&session.config) {
        log.error(Area::Settings, format!("Config save failed: {error}"));
    }
}

struct Controls {
    play: bool,
    pause: bool,
    stop: bool,
    resume_queue: bool,
    /// Nothing new can start: a launch or stop is in flight, or the queue owns the runner.
    blocked: bool,
    play_label: &'static str,
}

fn controls(session: &Session) -> Controls {
    let activity = session.queue.activity();
    let queue_running = matches!(
        activity,
        QueueActivity::Running { .. }
            | QueueActivity::Paused { .. }
            | QueueActivity::Transition { .. }
    );
    let state = session.clock.state();
    Controls {
        play: selected_game(session).is_some()
            && !session.busy
            && state != SessionState::Playing
            && !queue_running,
        pause: !session.busy && state == SessionState::Playing,
        stop: !session.busy && (state != SessionState::Stopped || queue_running),
        resume_queue: !session.busy && matches!(activity, QueueActivity::Paused { .. }),
        blocked: session.busy || queue_running,
        play_label: if state == SessionState::Paused {
            "Resume"
        } else {
            "Play"
        },
    }
}

fn displayed_time(session: &Session, now: Instant) -> String {
    match session.queue.activity() {
        QueueActivity::Running { .. } | QueueActivity::Paused { .. } => {
            format_hms(session.queue.elapsed_in_item(now).unwrap_or_default())
        }
        QueueActivity::Transition { .. } => String::new(),
        _ => format_hms(session.clock.elapsed(now)),
    }
}

/// "12:04" under an hour, "1:02:03" above.
fn format_short(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// "45 min", "1 h", "1 h 30 min".
fn format_minutes(duration: Duration) -> String {
    let minutes = duration.as_secs().div_ceil(60);
    match (minutes / 60, minutes % 60) {
        (0, minutes) => format!("{minutes} min"),
        (hours, 0) => format!("{hours} h"),
        (hours, minutes) => format!("{hours} h {minutes} min"),
    }
}

/// The dock's quiet second line while the queue drives the session.
fn queue_note(session: &Session, now: Instant) -> String {
    let count = session.queue.items().len();
    match session.queue.activity() {
        QueueActivity::Running { index } => format!(
            "Queue {} of {count} · {} left",
            index + 1,
            format_short(session.queue.remaining(now).unwrap_or_default())
        ),
        QueueActivity::Paused { index } => format!("Queue {} of {count}", index + 1),
        QueueActivity::Transition { next_index } => {
            format!("Queue · up next {} of {count}", next_index + 1)
        }
        _ => String::new(),
    }
}

/// The utility row only speaks up about a schedule or a problem.
fn queue_line(session: &Session) -> String {
    match session.queue.activity() {
        QueueActivity::Scheduled { at_unix_ms } => format!("Starts {}", format_unix(at_unix_ms)),
        QueueActivity::Missed { at_unix_ms } => {
            format!("Missed start · {}", format_unix(at_unix_ms))
        }
        QueueActivity::Failed { message, .. } => message,
        _ => String::new(),
    }
}

fn queue_state(session: &Session) -> (&'static str, String) {
    let count = session.queue.items().len();
    match session.queue.activity() {
        QueueActivity::Idle => ("idle", "Idle".into()),
        QueueActivity::Scheduled { at_unix_ms } => {
            ("scheduled", format!("Starts {}", format_unix(at_unix_ms)))
        }
        QueueActivity::Missed { at_unix_ms } => (
            "missed",
            format!("Missed start · {}", format_unix(at_unix_ms)),
        ),
        QueueActivity::Running { index } => {
            ("running", format!("Playing {} of {count}", index + 1))
        }
        QueueActivity::Paused { index } => {
            ("paused", format!("Paused on {} of {count}", index + 1))
        }
        QueueActivity::Transition { next_index } => (
            "transition",
            format!("Switching to {} of {count}…", next_index + 1),
        ),
        QueueActivity::Failed { message, .. } => ("failed", message),
    }
}

/// Time left in the whole queue, or its total length when idle.
fn queue_summary(session: &Session, now: Instant) -> String {
    let items = session.queue.items();
    if items.is_empty() {
        return String::new();
    }
    let active = session.queue.active_index();
    let total: Duration = items
        .iter()
        .enumerate()
        .filter(|(index, _)| active.is_none_or(|active| *index >= active))
        .map(|(index, item)| {
            if Some(index) == active {
                session.queue.remaining(now).unwrap_or(item.duration)
            } else {
                item.duration
            }
        })
        .sum();
    if active.is_some() {
        format!("{} left", format_minutes(total))
    } else {
        let games = if items.len() == 1 { "game" } else { "games" };
        format!("{} {games} · {}", items.len(), format_minutes(total))
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
                    format_short(session.queue.remaining(now).unwrap_or(item.duration))
                )
            } else {
                format_minutes(item.duration)
            };
            QueueRow {
                name: item.name.clone().into(),
                meta: meta.into(),
                art: cached_art(&item.application_id),
                active: Some(index) == active,
            }
        })
        .collect()
}

fn database_detail(session: &Session) -> String {
    if session.refreshing {
        return "Refreshing…".into();
    }
    if session.games.is_empty() {
        return if session.loaded {
            "Not downloaded yet".into()
        } else {
            "Loading…".into()
        };
    }
    let updated = session
        .catalog_updated
        .and_then(|time| SystemTime::now().duration_since(time).ok())
        .map(|age| match age.as_secs() {
            0..60 => "updated just now".to_string(),
            60..3600 => format!("updated {} min ago", age.as_secs() / 60),
            3600..86_400 => format!("updated {} h ago", age.as_secs() / 3600),
            _ => {
                let days = age.as_secs() / 86_400;
                format!("updated {days} day{} ago", if days == 1 { "" } else { "s" })
            }
        });
    let games = format!("{} games", group_digits(session.games.len()));
    match (updated, session.offline) {
        (Some(updated), false) => format!("{games} · {updated}"),
        (Some(updated), true) => format!("{games} · offline, {updated}"),
        (None, _) => games,
    }
}

/// "updated 3 h ago" style age of the cached database, for the log.
fn catalog_age(updated: Option<SystemTime>) -> String {
    match updated.and_then(|time| SystemTime::now().duration_since(time).ok()) {
        Some(age) => format!(
            "updated {} ago",
            format_minutes(Duration::from_secs(age.as_secs() / 60 * 60))
        ),
        None => "age unknown".into(),
    }
}

fn group_digits(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// "today at 23:59", "tomorrow at 08:00", or "Sat 27 Sep at 08:00".
fn format_unix(millis: i64) -> String {
    let Some(time) = chrono::DateTime::from_timestamp_millis(millis) else {
        return "unknown".into();
    };
    let time = time.with_timezone(&Local);
    let today = Local::now().date_naive();
    let day = match (time.date_naive() - today).num_days() {
        0 => "today".to_string(),
        1 => "tomorrow".to_string(),
        -1 => "yesterday".to_string(),
        _ => time.format("%a %-d %b").to_string(),
    };
    format!("{day} at {}", time.format("%H:%M"))
}

fn runner_template() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let directory = executable
        .parent()
        .ok_or_else(|| "Cannot locate Game Larper.".to_string())?;
    // The release name first, then what `cargo build` produces.
    #[cfg(windows)]
    let names = ["GameLarper.Runner.exe", "game-larper-runner.exe"];
    #[cfg(target_os = "linux")]
    let names = ["GameLarper.Runner", "game-larper-runner"];
    for name in names {
        let path = directory.join(name);
        if path.exists() {
            return Ok(path);
        }
    }
    Err("The bundled native runner is missing.".into())
}

fn style_main(ui: &MainWindow) {
    platform::style_frame(ui.window());
}

/// Show the main window. The first show creates the native window, so style it each time.
fn show_main(ui: &MainWindow) {
    if ui.show().is_ok() {
        style_main(ui);
    }
}

#[derive(Clone, Copy)]
enum Tone {
    Debug,
    Accent,
    Success,
    Warning,
    Danger,
    Muted,
}

impl From<Tone> for slint::Color {
    fn from(value: Tone) -> Self {
        match value {
            Tone::Debug => slint::Color::from_rgb_u8(0x7E, 0x7A, 0x96),
            Tone::Accent => slint::Color::from_rgb_u8(0xA4, 0x9E, 0xF8),
            Tone::Success => slint::Color::from_rgb_u8(0x69, 0xDA, 0xA5),
            Tone::Warning => slint::Color::from_rgb_u8(0xF5, 0xBE, 0x66),
            Tone::Danger => slint::Color::from_rgb_u8(0xFF, 0x64, 0x75),
            Tone::Muted => slint::Color::from_rgb_u8(0x70, 0x7A, 0x8D),
        }
    }
}
