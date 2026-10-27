//! `automation.json`: drain marks, watchdog attempts and the last published
//! availability value. Saved atomically before the action it authorises, so a
//! restart neither repeats nor forgets one. A malformed or newer file is kept
//! as it is, and nothing automatic runs until it is fixed.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{FleetDrain, FleetDrainState, FleetWatchdogRunner};

const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DrainState {
    Draining,
    Stopped,
}

/// A reviewed drain, with the facts its deferred stop must still match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DrainMark {
    pub runner: String,
    pub host: String,
    /// The runner's exact inventory directory at review.
    pub path: String,
    /// GitHub's runner id at review.
    pub registration: i64,
    /// The review that authorised the stop.
    pub review: String,
    pub requested_at: DateTime<Utc>,
    pub state: DrainState,
    pub stopped_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl DrainMark {
    pub fn view(&self) -> FleetDrain {
        FleetDrain {
            runner: self.runner.clone(),
            host_id: self.host.clone(),
            state: match self.state {
                DrainState::Draining => FleetDrainState::Draining,
                DrainState::Stopped => FleetDrainState::Stopped,
            },
            requested_at: self.requested_at,
            stopped_at: self.stopped_at,
            error: self.last_error.clone(),
        }
    }
}

/// One runner's watchdog history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WatchdogRunner {
    pub host: String,
    pub down_since: Option<DateTime<Utc>>,
    pub attempts: Vec<DateTime<Utc>>,
    pub gave_up_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl WatchdogRunner {
    pub fn view(&self, runner: &str) -> FleetWatchdogRunner {
        FleetWatchdogRunner {
            runner: runner.into(),
            host_id: self.host.clone(),
            attempts: self.attempts.clone(),
            down_since: self.down_since,
            next_attempt_at: super::watchdog::next_attempt(self),
            gave_up_at: self.gave_up_at,
            last_error: self.last_error.clone(),
        }
    }
}

/// The last availability value written, without its timestamp.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublishState {
    /// Where it was written: `org/VARIABLE`.
    pub target: Option<String>,
    /// The counts last written, without `updatedAt`.
    pub value: Option<String>,
    /// The complete value last written.
    pub published_value: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub attempted_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub waiting: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Journal {
    pub version: u32,
    #[serde(default)]
    pub drains: BTreeMap<String, DrainMark>,
    #[serde(default)]
    pub watchdog: BTreeMap<String, WatchdogRunner>,
    #[serde(default)]
    pub publish: PublishState,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            version: VERSION,
            drains: BTreeMap::new(),
            watchdog: BTreeMap::new(),
            publish: PublishState::default(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct AutomationStore {
    path: PathBuf,
    journal: Journal,
    pub storage_error: Option<String>,
    writable: bool,
}

impl AutomationStore {
    pub fn load(path: PathBuf) -> Self {
        let result = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Journal>(&bytes)
                .map_err(|e| e.to_string())
                .and_then(|j| {
                    if j.version == VERSION {
                        Ok(j)
                    } else {
                        Err(format!("Unsupported automation state version {}.", j.version))
                    }
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Journal::default()),
            Err(e) => Err(e.to_string()),
        };
        match result {
            Ok(journal) => Self {
                path,
                journal,
                storage_error: None,
                writable: true,
            },
            Err(e) => Self {
                path,
                journal: Journal::default(),
                storage_error: Some(format!(
                    "Couldn't read drain and watchdog state: {e} The file has been preserved; automatic changes are paused."
                )),
                writable: false,
            },
        }
    }

    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Applies `change` and saves it; on a failed save nothing changes.
    pub fn update<T>(&mut self, change: impl FnOnce(&mut Journal) -> T) -> Result<T, String> {
        if !self.writable {
            return Err(self
                .storage_error
                .clone()
                .unwrap_or_else(|| "Automation state can't be saved.".into()));
        }
        let mut next = self.journal.clone();
        let value = change(&mut next);
        if next != self.journal {
            let bytes = serde_json::to_vec_pretty(&next).map_err(|e| e.to_string())?;
            if let Err(e) = crate::atomic_write::atomic_write(&self.path, &bytes) {
                let error = format!("Couldn't save drain and watchdog state: {e}");
                self.storage_error = Some(error.clone());
                return Err(error);
            }
            self.journal = next;
        }
        self.storage_error = None;
        Ok(value)
    }
}
