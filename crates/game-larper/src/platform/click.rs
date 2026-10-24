//! Telling a single click on the tray icon from the first half of a double-click.
//!
//! The shell sends the same left-button-up for both, so a single click can only be acted on
//! once the double-click time has passed with no second click. This holds that decision and
//! nothing else: the caller owns the timer and the clock, which keeps it testable.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftUp {
    /// A first click. Start a timer for the double-click time; if `expired` is not called
    /// before another click arrives, it was a single click.
    Wait,
    /// The second click of a double-click. The pending click is gone.
    Double,
}

#[derive(Debug, Default)]
pub struct ClickTracker {
    first: Option<Instant>,
}

impl ClickTracker {
    /// A left button release at `now`. `delay` is the system double-click time.
    pub fn left_up(&mut self, now: Instant, delay: Duration) -> LeftUp {
        match self.first.take() {
            Some(first) if now.saturating_duration_since(first) <= delay => LeftUp::Double,
            _ => {
                self.first = Some(now);
                LeftUp::Wait
            }
        }
    }

    /// The timer fired. Returns when the single click happened, or `None` if a second click
    /// already turned it into a double-click or something cancelled it.
    pub fn expired(&mut self) -> Option<Instant> {
        self.first.take()
    }

    /// Forget a pending click, for example when another button was used.
    pub fn cancel(&mut self) {
        self.first = None;
    }

    #[cfg(test)]
    pub fn is_waiting(&self) -> bool {
        self.first.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELAY: Duration = Duration::from_millis(500);

    #[test]
    fn a_lone_click_becomes_a_single_click_when_the_timer_fires() {
        let mut tracker = ClickTracker::default();
        let start = Instant::now();
        assert_eq!(tracker.left_up(start, DELAY), LeftUp::Wait);
        assert!(tracker.is_waiting());
        assert_eq!(tracker.expired(), Some(start));
        assert!(!tracker.is_waiting());
    }

    #[test]
    fn two_clicks_inside_the_delay_are_a_double_click_and_no_single_click() {
        let mut tracker = ClickTracker::default();
        let start = Instant::now();
        assert_eq!(tracker.left_up(start, DELAY), LeftUp::Wait);
        assert_eq!(
            tracker.left_up(start + Duration::from_millis(200), DELAY),
            LeftUp::Double
        );
        // The timer for the first click still fires, and must not report a single click.
        assert_eq!(tracker.expired(), None);
    }

    #[test]
    fn the_second_click_exactly_at_the_delay_still_counts() {
        let mut tracker = ClickTracker::default();
        let start = Instant::now();
        tracker.left_up(start, DELAY);
        assert_eq!(tracker.left_up(start + DELAY, DELAY), LeftUp::Double);
    }

    #[test]
    fn a_slow_second_click_starts_a_new_single_click() {
        let mut tracker = ClickTracker::default();
        let start = Instant::now();
        tracker.left_up(start, DELAY);
        let later = start + DELAY + Duration::from_millis(1);
        assert_eq!(tracker.left_up(later, DELAY), LeftUp::Wait);
        assert_eq!(tracker.expired(), Some(later));
    }

    #[test]
    fn a_third_click_after_a_double_click_starts_over() {
        let mut tracker = ClickTracker::default();
        let start = Instant::now();
        tracker.left_up(start, DELAY);
        assert_eq!(
            tracker.left_up(start + Duration::from_millis(100), DELAY),
            LeftUp::Double
        );
        let third = start + Duration::from_millis(200);
        assert_eq!(tracker.left_up(third, DELAY), LeftUp::Wait);
        assert_eq!(tracker.expired(), Some(third));
    }

    #[test]
    fn cancel_drops_a_pending_click() {
        let mut tracker = ClickTracker::default();
        tracker.left_up(Instant::now(), DELAY);
        tracker.cancel();
        assert_eq!(tracker.expired(), None);
    }

    #[test]
    fn a_timer_with_nothing_pending_reports_nothing() {
        assert_eq!(ClickTracker::default().expired(), None);
    }
}
