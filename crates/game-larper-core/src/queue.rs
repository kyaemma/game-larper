use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::atomic::{self, quarantine_corrupt};
use crate::error::Error;

pub const MAX_QUEUE_ITEMS: usize = 24;
pub const DEFAULT_TRANSITION_GAP: Duration = Duration::from_secs(2);
const SCHEMA_VERSION: u32 = 1;
const MIN_DURATION: Duration = Duration::from_secs(1);
const MAX_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    pub id: u64,
    pub application_id: String,
    pub name: String,
    pub duration: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueActivity {
    Idle,
    Scheduled {
        at_unix_ms: i64,
    },
    Missed {
        at_unix_ms: i64,
    },
    Running {
        index: usize,
    },
    Paused {
        index: usize,
    },
    /// Waiting out the short gap before `next_index` launches.
    Transition {
        next_index: usize,
    },
    Failed {
        index: usize,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueAction {
    pub stop_runner: bool,
    pub launch_index: Option<usize>,
}

impl QueueAction {
    fn none() -> Self {
        Self {
            stop_runner: false,
            launch_index: None,
        }
    }

    fn stop() -> Self {
        Self {
            stop_runner: true,
            launch_index: None,
        }
    }
}

#[derive(Debug, Clone)]
struct Running {
    index: usize,
    elapsed: Duration,
    since: Instant,
}

#[derive(Debug, Clone)]
enum Phase {
    Idle,
    Running(Running),
    Paused { index: usize, elapsed: Duration },
    Transition { next_index: usize, until: Instant },
    Failed { index: usize, message: String },
}

#[derive(Debug, Clone)]
pub struct QueueMachine {
    items: Vec<QueueItem>,
    next_id: u64,
    phase: Phase,
    schedule_at_unix_ms: Option<i64>,
    schedule_missed: bool,
    gap: Duration,
    load_adjusted: bool,
}

impl QueueMachine {
    pub fn new(gap: Duration) -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
            phase: Phase::Idle,
            schedule_at_unix_ms: None,
            schedule_missed: false,
            gap,
            load_adjusted: false,
        }
    }

    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn gap(&self) -> Duration {
        self.gap
    }

    pub fn activity(&self) -> QueueActivity {
        match &self.phase {
            Phase::Running(running) => QueueActivity::Running {
                index: running.index,
            },
            Phase::Paused { index, .. } => QueueActivity::Paused { index: *index },
            Phase::Transition { next_index, .. } => QueueActivity::Transition {
                next_index: *next_index,
            },
            Phase::Failed { index, message } => QueueActivity::Failed {
                index: *index,
                message: message.clone(),
            },
            Phase::Idle => match (self.schedule_at_unix_ms, self.schedule_missed) {
                (Some(at_unix_ms), true) => QueueActivity::Missed { at_unix_ms },
                (Some(at_unix_ms), false) => QueueActivity::Scheduled { at_unix_ms },
                (None, _) => QueueActivity::Idle,
            },
        }
    }

    pub fn take_load_adjustment(&mut self) -> bool {
        std::mem::take(&mut self.load_adjusted)
    }

    pub fn add(
        &mut self,
        application_id: impl Into<String>,
        name: impl Into<String>,
        duration: Duration,
    ) -> Result<&QueueItem, Error> {
        self.ensure_editable()?;
        if self.items.len() >= MAX_QUEUE_ITEMS {
            return Err(Error::Queue("The queue is full."));
        }
        let application_id = application_id.into();
        if application_id.is_empty() || !application_id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(Error::InvalidApplicationId);
        }
        let name = name.into().trim().to_string();
        if name.is_empty() || name.chars().count() > 200 {
            return Err(Error::Queue("That game name cannot be queued."));
        }
        if duration < MIN_DURATION || duration > MAX_DURATION {
            return Err(Error::Queue(
                "Queue duration must be between 1 second and 24 hours.",
            ));
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.items.push(QueueItem {
            id,
            application_id,
            name,
            duration,
        });
        Ok(self.items.last().expect("item was just pushed"))
    }

    pub fn remove(&mut self, index: usize) -> Result<QueueItem, Error> {
        self.ensure_editable()?;
        if index >= self.items.len() {
            return Err(Error::Queue("Queue index is out of range."));
        }
        Ok(self.items.remove(index))
    }

    /// Move the item at `from` so it lands at `to` in the resulting list.
    pub fn move_item(&mut self, from: usize, to: usize) -> Result<(), Error> {
        self.ensure_editable()?;
        if from >= self.items.len() {
            return Err(Error::Queue("Queue index is out of range."));
        }
        if from == to {
            return Ok(());
        }
        let item = self.items.remove(from);
        let destination = to.min(self.items.len());
        self.items.insert(destination, item);
        Ok(())
    }

    pub fn clear(&mut self) -> QueueAction {
        let stop_runner = self.runner_is_active();
        self.items.clear();
        self.phase = Phase::Idle;
        self.schedule_at_unix_ms = None;
        self.schedule_missed = false;
        QueueAction {
            stop_runner,
            launch_index: None,
        }
    }

    pub fn start_now(&mut self, now: Instant) -> Result<QueueAction, Error> {
        self.ensure_can_start()?;
        self.schedule_at_unix_ms = None;
        self.schedule_missed = false;
        Ok(self.begin_run(0, now))
    }

    pub fn arm(&mut self, at_unix_ms: i64, now_unix_ms: i64) -> Result<(), Error> {
        self.ensure_can_start()?;
        if at_unix_ms <= now_unix_ms {
            return Err(Error::Queue("That start time is already past."));
        }
        self.phase = Phase::Idle;
        self.schedule_at_unix_ms = Some(at_unix_ms);
        self.schedule_missed = false;
        Ok(())
    }

    pub fn disarm(&mut self) -> Result<(), Error> {
        if self.runner_is_active() {
            return Err(Error::Queue("Stop the queue before changing the schedule."));
        }
        self.schedule_at_unix_ms = None;
        self.schedule_missed = false;
        if matches!(self.phase, Phase::Failed { .. }) {
            self.phase = Phase::Idle;
        }
        Ok(())
    }

    pub fn pause(&mut self, now: Instant) -> QueueAction {
        match &self.phase {
            Phase::Running(running) => {
                let elapsed = running_elapsed(running, now);
                let index = running.index;
                self.phase = Phase::Paused { index, elapsed };
                QueueAction::stop()
            }
            Phase::Transition { next_index, .. } => {
                let next_index = *next_index;
                self.phase = Phase::Paused {
                    index: next_index,
                    elapsed: Duration::ZERO,
                };
                QueueAction::stop()
            }
            _ => QueueAction::none(),
        }
    }

    pub fn resume(&mut self, now: Instant) -> QueueAction {
        let Phase::Paused { index, elapsed } = self.phase else {
            return QueueAction::none();
        };
        self.phase = Phase::Running(Running {
            index,
            elapsed,
            since: now,
        });
        QueueAction {
            stop_runner: false,
            launch_index: Some(index),
        }
    }

    pub fn skip(&mut self, now: Instant) -> QueueAction {
        let index = match &self.phase {
            Phase::Running(running) => running.index,
            Phase::Paused { index, .. } => *index,
            Phase::Transition { next_index, .. } => {
                let next_index = *next_index;
                return self.advance_to(next_index, now);
            }
            _ => return QueueAction::none(),
        };
        self.advance_to(index.saturating_add(1), now)
    }

    pub fn stop(&mut self) -> QueueAction {
        let stop_runner = self.runner_is_active();
        self.phase = Phase::Idle;
        self.schedule_at_unix_ms = None;
        self.schedule_missed = false;
        QueueAction {
            stop_runner,
            launch_index: None,
        }
    }

    /// Record a launch failure and stop. The queue does not skip forward on its own.
    pub fn fail(&mut self, index: usize, message: impl Into<String>) -> QueueAction {
        self.phase = Phase::Failed {
            index,
            message: message.into(),
        };
        self.schedule_at_unix_ms = None;
        self.schedule_missed = false;
        QueueAction::stop()
    }

    pub fn tick(&mut self, now: Instant, now_unix_ms: i64) -> QueueAction {
        if matches!(self.phase, Phase::Idle)
            && !self.items.is_empty()
            && self
                .schedule_at_unix_ms
                .is_some_and(|at| !self.schedule_missed && now_unix_ms >= at)
        {
            self.schedule_at_unix_ms = None;
            return self.begin_run(0, now);
        }
        match &self.phase {
            Phase::Running(running) => {
                let index = running.index;
                let duration = self
                    .items
                    .get(index)
                    .map(|item| item.duration)
                    .unwrap_or(MAX_DURATION);
                if running_elapsed(running, now) >= duration {
                    self.advance_to(index.saturating_add(1), now)
                } else {
                    QueueAction::none()
                }
            }
            Phase::Transition { next_index, until } if now >= *until => {
                let next_index = *next_index;
                self.begin_run(next_index, now)
            }
            _ => QueueAction::none(),
        }
    }

    pub fn active_index(&self) -> Option<usize> {
        match self.activity() {
            QueueActivity::Running { index }
            | QueueActivity::Paused { index }
            | QueueActivity::Failed { index, .. } => Some(index),
            QueueActivity::Transition { next_index } => Some(next_index),
            QueueActivity::Idle
            | QueueActivity::Scheduled { .. }
            | QueueActivity::Missed { .. } => None,
        }
    }

    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        let (index, elapsed) = match &self.phase {
            Phase::Running(running) => (running.index, running_elapsed(running, now)),
            Phase::Paused { index, elapsed } => (*index, *elapsed),
            _ => return None,
        };
        Some(self.items.get(index)?.duration.saturating_sub(elapsed))
    }

    pub fn elapsed_in_item(&self, now: Instant) -> Option<Duration> {
        let duration = self.remaining(now)?;
        let index = match self.activity() {
            QueueActivity::Running { index } | QueueActivity::Paused { index } => index,
            _ => return None,
        };
        Some(self.items.get(index)?.duration.saturating_sub(duration))
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            schema_version: SCHEMA_VERSION,
            items: self
                .items
                .iter()
                .map(|item| QueueItemSnapshot {
                    id: item.id,
                    application_id: item.application_id.clone(),
                    name: item.name.clone(),
                    duration_ms: u64::try_from(item.duration.as_millis()).unwrap_or(u64::MAX),
                })
                .collect(),
            schedule_at_unix_ms: self.schedule_at_unix_ms,
            schedule_missed: self.schedule_missed,
        }
    }

    fn ensure_editable(&self) -> Result<(), Error> {
        if self.runner_is_active() {
            Err(Error::Queue("Stop the queue before editing it."))
        } else {
            Ok(())
        }
    }

    fn ensure_can_start(&self) -> Result<(), Error> {
        if self.items.is_empty() {
            return Err(Error::Queue("The queue is empty."));
        }
        if self.runner_is_active() {
            return Err(Error::Queue("The queue is already running."));
        }
        Ok(())
    }

    fn runner_is_active(&self) -> bool {
        matches!(
            self.phase,
            Phase::Running(_) | Phase::Paused { .. } | Phase::Transition { .. }
        )
    }

    fn begin_run(&mut self, index: usize, now: Instant) -> QueueAction {
        if index >= self.items.len() {
            self.phase = Phase::Idle;
            return QueueAction::stop();
        }
        self.phase = Phase::Running(Running {
            index,
            elapsed: Duration::ZERO,
            since: now,
        });
        QueueAction {
            stop_runner: true,
            launch_index: Some(index),
        }
    }

    fn advance_to(&mut self, index: usize, now: Instant) -> QueueAction {
        if index >= self.items.len() {
            self.phase = Phase::Idle;
            return QueueAction::stop();
        }
        if self.gap.is_zero() {
            return self.begin_run(index, now);
        }
        self.phase = Phase::Transition {
            next_index: index,
            until: now + self.gap,
        };
        QueueAction::stop()
    }
}

fn running_elapsed(running: &Running, now: Instant) -> Duration {
    running
        .elapsed
        .saturating_add(now.saturating_duration_since(running.since))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItemSnapshot {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub application_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueSnapshot {
    #[serde(default = "current_schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub items: Vec<QueueItemSnapshot>,
    #[serde(default)]
    pub schedule_at_unix_ms: Option<i64>,
    #[serde(default)]
    pub schedule_missed: bool,
}

fn current_schema() -> u32 {
    SCHEMA_VERSION
}

impl QueueSnapshot {
    pub fn into_machine(self, now_unix_ms: i64, gap: Duration) -> (QueueMachine, Vec<String>) {
        let mut warnings = Vec::new();
        if self.schema_version > SCHEMA_VERSION {
            warnings.push(format!(
                "Queue schema {} is newer than {SCHEMA_VERSION}. Unknown fields were ignored.",
                self.schema_version
            ));
        }
        let mut machine = QueueMachine::new(gap);
        let mut next_id = 1_u64;
        for item in self.items {
            if machine.items.len() >= MAX_QUEUE_ITEMS {
                warnings.push("Extra queued games were ignored.".into());
                break;
            }
            if item.application_id.is_empty()
                || !item
                    .application_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
            {
                warnings.push("A queued game with a bad id was dropped.".into());
                continue;
            }
            let name = item.name.trim();
            if name.is_empty() || name.chars().count() > 200 {
                warnings.push("A queued game with a bad name was dropped.".into());
                continue;
            }
            let duration = Duration::from_millis(item.duration_ms);
            if duration < MIN_DURATION || duration > MAX_DURATION {
                warnings.push(format!("Dropped {name}: duration is out of range."));
                continue;
            }
            let id = if item.id == 0 { next_id } else { item.id };
            next_id = next_id.max(id.saturating_add(1));
            machine.items.push(QueueItem {
                id,
                application_id: item.application_id,
                name: name.to_string(),
                duration,
            });
        }
        machine.next_id = next_id.max(1);
        machine.schedule_at_unix_ms = self.schedule_at_unix_ms;
        machine.schedule_missed = self.schedule_missed;
        if let Some(at) = machine.schedule_at_unix_ms
            && !machine.schedule_missed
            && at <= now_unix_ms
        {
            machine.schedule_missed = true;
            machine.load_adjusted = true;
            warnings
                .push("A scheduled queue start was missed while Game Larper was closed.".into());
        }
        (machine, warnings)
    }
}

pub fn save_queue(path: &Path, machine: &QueueMachine) -> Result<(), Error> {
    let json = serde_json::to_vec_pretty(&machine.snapshot())?;
    atomic::write_atomic(path, &json)?;
    Ok(())
}

pub fn load_queue(
    path: &Path,
    now_unix_ms: i64,
    gap: Duration,
) -> Result<(QueueMachine, Vec<String>), Error> {
    let json = match fs::read_to_string(path) {
        Ok(json) => json,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((QueueMachine::new(gap), Vec::new()));
        }
        Err(error) => {
            return Ok((
                QueueMachine::new(gap),
                vec![format!("Could not load the queue: {error}")],
            ));
        }
    };
    match serde_json::from_str::<QueueSnapshot>(&json) {
        Ok(snapshot) => Ok(snapshot.into_machine(now_unix_ms, gap)),
        Err(error) => {
            quarantine_corrupt(path);
            let mut machine = QueueMachine::new(gap);
            machine.load_adjusted = true;
            Ok((
                machine,
                vec![format!(
                    "Could not load the queue, so it was reset: {error}"
                )],
            ))
        }
    }
}

pub fn unix_time_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}
