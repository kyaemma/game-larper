use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use game_larper_core::{QueueActivity, QueueMachine, load_queue, save_queue};

fn machine() -> QueueMachine {
    QueueMachine::new(Duration::from_secs(2))
}

fn seed(machine: &mut QueueMachine) {
    machine
        .add("1", "ELDEN RING", Duration::from_secs(10))
        .unwrap();
    machine
        .add("2", "Cyberpunk 2077", Duration::from_secs(10))
        .unwrap();
    machine
        .add("3", "The Quarry", Duration::from_secs(5))
        .unwrap();
}

#[test]
fn add_remove_reorder_and_clear() {
    let mut queue = machine();
    seed(&mut queue);
    queue.move_item(0, 2).unwrap();
    assert_eq!(queue.items()[2].name, "ELDEN RING");
    queue.remove(1).unwrap();
    assert_eq!(queue.items().len(), 2);
    let action = queue.clear();
    assert!(!action.stop_runner);
    assert!(queue.items().is_empty());
    assert!(matches!(queue.activity(), QueueActivity::Idle));
}

#[test]
fn duration_pause_resume_skip_and_stop() {
    let start = Instant::now();
    let mut queue = QueueMachine::new(Duration::ZERO);
    seed(&mut queue);
    let started = queue.start_now(start).unwrap();
    assert_eq!(started.launch_index, Some(0));
    assert!(
        queue
            .tick(start + Duration::from_secs(9), 0)
            .launch_index
            .is_none()
    );
    assert_eq!(
        queue.remaining(start + Duration::from_secs(4)),
        Some(Duration::from_secs(6))
    );

    let paused = queue.pause(start + Duration::from_secs(4));
    assert!(paused.stop_runner);
    assert!(matches!(
        queue.activity(),
        QueueActivity::Paused { index: 0 }
    ));
    assert_eq!(
        queue.remaining(start + Duration::from_secs(100)),
        Some(Duration::from_secs(6))
    );

    let resumed = queue.resume(start + Duration::from_secs(100));
    assert_eq!(resumed.launch_index, Some(0));
    assert!(!resumed.stop_runner);
    let expired = queue.tick(start + Duration::from_secs(106), 0);
    assert_eq!(expired.launch_index, Some(1));

    let skipped = queue.skip(start + Duration::from_secs(106));
    assert_eq!(skipped.launch_index, Some(2));
    let stopped = queue.stop();
    assert!(stopped.stop_runner);
    assert_eq!(queue.items().len(), 3);
    assert!(matches!(queue.activity(), QueueActivity::Idle));
}

#[test]
fn natural_advance_waits_for_the_transition_gap() {
    let start = Instant::now();
    let mut queue = QueueMachine::new(Duration::from_millis(500));
    queue.add("1", "One", Duration::from_secs(1)).unwrap();
    queue.add("2", "Two", Duration::from_secs(1)).unwrap();
    queue.start_now(start).unwrap();
    let advanced = queue.tick(start + Duration::from_secs(1), 0);
    assert!(advanced.stop_runner);
    assert!(advanced.launch_index.is_none());
    assert!(matches!(
        queue.activity(),
        QueueActivity::Transition { next_index: 1 }
    ));
    assert!(
        queue
            .tick(start + Duration::from_millis(1499), 0)
            .launch_index
            .is_none()
    );
    assert_eq!(
        queue
            .tick(start + Duration::from_millis(1500), 0)
            .launch_index,
        Some(1)
    );
}

#[test]
fn failed_item_stops_the_queue() {
    let start = Instant::now();
    let mut queue = machine();
    seed(&mut queue);
    queue.start_now(start).unwrap();
    let failed = queue.fail(0, "runner missing");
    assert!(failed.stop_runner);
    assert!(matches!(
        queue.activity(),
        QueueActivity::Failed { index: 0, .. }
    ));
    assert!(
        queue
            .tick(start + Duration::from_secs(30), 0)
            .launch_index
            .is_none()
    );
    queue.add("4", "Extra", Duration::from_secs(60)).unwrap();
    assert_eq!(queue.items().len(), 4);
}

#[test]
fn edits_are_refused_while_running() {
    let mut queue = machine();
    seed(&mut queue);
    queue.start_now(Instant::now()).unwrap();
    assert!(queue.move_item(0, 1).is_err());
    assert!(queue.remove(2).is_err());
}

#[test]
fn schedule_arms_fires_and_a_missed_restart_does_not_run() {
    let now = 1_700_000_000_000;
    let mut queue = machine();
    seed(&mut queue);
    assert!(queue.arm(now - 1, now).is_err());
    queue.arm(now + 5_000, now).unwrap();
    assert!(
        matches!(queue.activity(), QueueActivity::Scheduled { at_unix_ms } if at_unix_ms == now + 5_000)
    );
    assert!(queue.tick(Instant::now(), now).launch_index.is_none());
    let fired = queue.tick(Instant::now(), now + 5_000);
    assert_eq!(fired.launch_index, Some(0));

    let mut queued = machine();
    seed(&mut queued);
    queued.arm(now + 5_000, now).unwrap();
    let root = temp_dir();
    let path = root.join("queue.json");
    save_queue(&path, &queued).unwrap();
    let (mut loaded, warnings) = load_queue(&path, now + 9_000, Duration::from_secs(2)).unwrap();
    assert!(loaded.take_load_adjustment());
    assert!(warnings.iter().any(|warning| warning.contains("missed")));
    assert!(matches!(loaded.activity(), QueueActivity::Missed { .. }));
    assert!(
        loaded
            .tick(Instant::now(), now + 9_000)
            .launch_index
            .is_none()
    );
    assert_eq!(loaded.items().len(), 3);
}

#[test]
fn future_schedule_survives_restart() {
    let now = 1_700_000_000_000;
    let mut queue = machine();
    seed(&mut queue);
    queue.arm(now + 60_000, now).unwrap();
    let root = temp_dir();
    let path = root.join("queue.json");
    save_queue(&path, &queue).unwrap();
    let (loaded, warnings) = load_queue(&path, now + 1_000, Duration::from_secs(2)).unwrap();
    assert!(warnings.is_empty());
    assert!(matches!(loaded.activity(), QueueActivity::Scheduled { .. }));
}

#[test]
fn corrupt_queue_resets() {
    let root = temp_dir();
    let path = root.join("queue.json");
    std::fs::write(&path, "{not json").unwrap();
    let (loaded, warnings) = load_queue(&path, 0, Duration::from_secs(2)).unwrap();
    assert!(loaded.items().is_empty());
    assert!(!warnings.is_empty());
    assert!(!path.exists());
}

fn temp_dir() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "game-larper-queue-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}
