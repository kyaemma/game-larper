use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Stopped,
    Playing,
    Paused,
}

/// Elapsed play time measured with [`Instant`], never the wall clock.
#[derive(Debug, Clone)]
pub struct SessionClock {
    state: SessionState,
    accumulated: Duration,
    since: Option<Instant>,
}

impl Default for SessionClock {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionClock {
    pub fn new() -> Self {
        Self {
            state: SessionState::Stopped,
            accumulated: Duration::ZERO,
            since: None,
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        match self.state {
            SessionState::Playing => self
                .accumulated
                .saturating_add(now.saturating_duration_since(self.since.unwrap_or(now))),
            SessionState::Paused => self.accumulated,
            SessionState::Stopped => Duration::ZERO,
        }
    }

    /// Start a fresh session, or continue a paused one. Playing is left unchanged.
    pub fn play(&mut self, now: Instant) {
        match self.state {
            SessionState::Playing => {}
            SessionState::Paused => {
                self.since = Some(now);
                self.state = SessionState::Playing;
            }
            SessionState::Stopped => {
                self.accumulated = Duration::ZERO;
                self.since = Some(now);
                self.state = SessionState::Playing;
            }
        }
    }

    pub fn pause(&mut self, now: Instant) {
        if self.state != SessionState::Playing {
            return;
        }
        self.accumulated = self.elapsed(now);
        self.since = None;
        self.state = SessionState::Paused;
    }

    pub fn stop(&mut self) {
        self.accumulated = Duration::ZERO;
        self.since = None;
        self.state = SessionState::Stopped;
    }

    pub fn unexpected_exit(&mut self) {
        self.stop();
    }
}

pub fn format_hms(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}
