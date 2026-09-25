use std::time::{Duration, Instant};

use game_larper_core::{SessionClock, SessionState, format_hms};

#[test]
fn play_pause_resume_and_stop_track_monotonic_time() {
    let start = Instant::now();
    let mut clock = SessionClock::new();
    assert_eq!(clock.state(), SessionState::Stopped);
    assert_eq!(clock.elapsed(start), Duration::ZERO);

    clock.play(start);
    assert_eq!(
        clock.elapsed(start + Duration::from_secs(5)),
        Duration::from_secs(5)
    );
    clock.play(start + Duration::from_secs(5));
    assert_eq!(
        clock.elapsed(start + Duration::from_secs(5)),
        Duration::from_secs(5)
    );

    clock.pause(start + Duration::from_secs(5));
    assert_eq!(clock.state(), SessionState::Paused);
    assert_eq!(
        clock.elapsed(start + Duration::from_secs(50)),
        Duration::from_secs(5)
    );

    clock.play(start + Duration::from_secs(50));
    assert_eq!(
        clock.elapsed(start + Duration::from_secs(58)),
        Duration::from_secs(13)
    );

    clock.stop();
    assert_eq!(clock.state(), SessionState::Stopped);
    assert_eq!(
        clock.elapsed(start + Duration::from_secs(90)),
        Duration::ZERO
    );
}

#[test]
fn unexpected_exit_resets_to_stopped() {
    let start = Instant::now();
    let mut clock = SessionClock::new();
    clock.play(start);
    clock.unexpected_exit();
    assert_eq!(clock.state(), SessionState::Stopped);
    assert_eq!(
        clock.elapsed(start + Duration::from_secs(3)),
        Duration::ZERO
    );
    clock.pause(start);
    assert_eq!(clock.state(), SessionState::Stopped);
}

#[test]
fn format_hms_rolls_hours() {
    assert_eq!(format_hms(Duration::from_secs(0)), "00:00:00");
    assert_eq!(format_hms(Duration::from_secs(3661)), "01:01:01");
    assert_eq!(format_hms(Duration::from_secs(3600 * 100)), "100:00:00");
}
