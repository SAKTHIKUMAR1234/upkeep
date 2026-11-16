pub mod checks;
pub mod clock;
pub mod db;
pub mod message;
pub mod peers;
pub mod settings;
pub mod state;
pub mod telegram;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::adapter::health;
use crate::engine::{JobEvent, Runner};
use crate::plan;
use crate::settings::Settings;
use crate::store::StoreError;
use crate::time::now_unix;
use checks::host::{self, SecurityUpdates};
use checks::{Observation, system};
use db::MonitorDb;
use message::digest::{HostReport, Section};
use message::{Event, Letterhead};
use settings::MonitorSettings;
use state::{CheckState, Policy};
use telegram::Telegram;

const DELIVERY_TICK: Duration = Duration::from_secs(5);
const DIGEST_DAY: &str = "digest_day";

#[derive(Debug, thiserror::Error)]
pub enum MonitorError {
    #[error("monitor database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("no [monitor.telegram] in upkeep.toml")]
    NoTelegram,
    #[error("cannot send the message: {0}")]
    Send(#[from] telegram::SendError),
}

/// The monitor settings of one round, with the Telegram sender they configure.
struct Active {
    settings: MonitorSettings,
    telegram: Option<Arc<Telegram>>,
}

impl Active {
    /// Fails when the Telegram token or a peer cannot be used.
    fn new(settings: MonitorSettings) -> anyhow::Result<Self> {
        peers::validate(&settings.peers)?;
        let telegram = settings.telegram.as_ref().map(Telegram::new).transpose()?;
        Ok(Self {
            settings,
            telegram: telegram.map(Arc::new),
        })
    }
}

/// Checks this server every round, keeps each check's state and queues Telegram alerts.
pub struct Monitor {
    runner: Arc<Runner>,
    db: MonitorDb,
    host: String,
    active: RwLock<Arc<Active>>,
    security: SecurityUpdates,
    outbox_changed: Notify,
}

impl Monitor {
    pub fn new(
        runner: Arc<Runner>,
        settings: MonitorSettings,
        host: String,
    ) -> anyhow::Result<Self> {
        let db = MonitorDb::open(&runner.paths().monitor_database())?;
        Ok(Self {
            active: RwLock::new(Arc::new(Active::new(settings)?)),
            runner,
            db,
            host,
            security: SecurityUpdates::default(),
            outbox_changed: Notify::new(),
        })
    }

    /// Starts the check rounds, the job messages and the outbox delivery.
    /// Call it before the runner resumes a job, so that its end is announced.
    pub fn spawn(self: &Arc<Self>) {
        tokio::spawn(self.clone().check_forever());
        tokio::spawn(self.clone().announce_jobs(self.runner.subscribe()));
        tokio::spawn(self.clone().deliver_forever());
    }

    fn active(&self) -> Arc<Active> {
        self.active
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Reads `[monitor]` again, so a changed threshold applies without a restart. A bad file keeps the old settings.
    fn reload(&self, settings_file: &Path) {
        let fresh = match Settings::load(settings_file) {
            Ok(settings) if settings.monitor == self.active().settings => return,
            Ok(settings) => Active::new(settings.monitor),
            Err(error) => Err(error.into()),
        };
        match fresh {
            Ok(active) => {
                tracing::info!("monitor settings changed");
                *self
                    .active
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(active);
            }
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "monitor settings not changed")
            }
        }
    }

    async fn check_forever(self: Arc<Self>) {
        loop {
            self.reload(&self.runner.paths().settings_file());
            let active = self.active();
            if active.settings.enabled
                && let Err(error) = self.round(&active, now_unix()).await
            {
                tracing::error!(%error, "monitor round failed");
            }
            tokio::time::sleep(active.settings.interval).await;
        }
    }

    async fn deliver_forever(self: Arc<Self>) {
        loop {
            if let Some(telegram) = &self.active().telegram
                && let Err(error) = telegram::flush(&self.db, telegram, now_unix()).await
            {
                tracing::error!(%error, "alert delivery failed");
            }
            tokio::select! {
                () = self.outbox_changed.notified() => {}
                () = tokio::time::sleep(DELIVERY_TICK) => {}
            }
        }
    }

    async fn announce_jobs(self: Arc<Self>, mut events: broadcast::Receiver<JobEvent>) {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if let Err(error) = self.announce_job(event) {
                        tracing::error!(%error, "cannot queue the job message");
                    }
                }
                Err(RecvError::Lagged(missed)) => tracing::warn!(missed, "job messages skipped"),
                Err(RecvError::Closed) => return,
            }
        }
    }

    /// Queues the start or end message of a plan's job.
    fn announce_job(&self, event: JobEvent) -> Result<(), MonitorError> {
        let active = self.active();
        if active.telegram.is_none() {
            return Ok(());
        }
        let (job_id, started) = match event {
            JobEvent::Started(job_id) => (job_id, true),
            JobEvent::Finished(job_id) => (job_id, false),
        };
        let store = self.runner.store();
        let job = store.job(job_id)?;
        if !plan::notifies(&job.target, &self.runner.paths().plans_dir()) {
            return Ok(());
        }
        let steps = store.steps(job_id)?;
        let letterhead = self.letterhead(&active.settings);
        let now = now_unix();
        let text = if started {
            letterhead.job_started(&job, &steps)
        } else {
            letterhead.job_finished(&job, &steps, now)
        };
        self.db.enqueue(&text, now)?;
        self.outbox_changed.notify_one();
        Ok(())
    }

    /// One round: observe, move each check's state, save it and queue the messages it asks for.
    async fn round(&self, active: &Active, now: i64) -> Result<(), MonitorError> {
        let settings = &active.settings;
        let observations = self.observe(settings, now).await?;
        let quiet = settings
            .quiet_hours
            .is_some_and(|hours| hours.contains(settings.utc_offset.minute_of_day(now)));
        let policy = Policy {
            alert_after: settings.alert_after,
            repeat_every: settings.repeat_every.as_secs() as i64,
        };
        let letterhead = self.letterhead(settings);

        let mut states = self.db.states()?;
        let mut notices = Vec::new();
        let mut round = Vec::new();
        for observation in observations {
            let mut state = states
                .remove(&observation.key)
                .unwrap_or_else(|| CheckState::new(now));
            notices.push(state.advance(observation.level, now, quiet, policy));
            round.push((observation, state));
        }
        let events: Vec<Event> = round
            .iter()
            .zip(&notices)
            .filter_map(|((observation, state), notice)| {
                Some(Event {
                    notice: notice.as_ref()?,
                    observation,
                    state,
                })
            })
            .collect();
        let mut messages = letterhead.round(&events, now);
        if let Some(digest) = self.due_digest(settings, &round, now).await? {
            messages.push(digest);
        }

        if active.telegram.is_none() {
            messages.clear();
        }
        self.db.save_round(&round, &messages, now)?;
        if !messages.is_empty() {
            self.outbox_changed.notify_one();
        }
        Ok(())
    }

    /// Every check of this round, one per key; the worst wins when two share a key.
    async fn observe(
        &self,
        settings: &MonitorSettings,
        now: i64,
    ) -> Result<Vec<Observation>, MonitorError> {
        let catalog = self.runner.catalog();
        let system_settings = settings.clone();
        let system = tokio::task::spawn_blocking(move || system::observe(&system_settings));
        let services = async {
            if !settings.is_enabled("services") {
                return Vec::new();
            }
            host::adapter_health(&health::check_all(&catalog, self.runner.paths()).await)
        };
        let (mut observations, failed_units, security, peers, system) = tokio::join!(
            services,
            when(settings.is_enabled("failed-units"), host::failed_units()),
            when(
                settings.is_enabled("security-updates"),
                self.security.observe()
            ),
            peers::observe(&settings.peers),
            system,
        );

        match system {
            Ok(system) => observations.extend(system),
            Err(error) => tracing::error!(%error, "system checks failed"),
        }
        observations.extend(failed_units.into_iter().chain(security).chain(peers));
        if settings.is_enabled("reboot-required") {
            observations.push(host::reboot_required(Path::new(host::REBOOT_REQUIRED)));
        }
        if let Some(max_age) = settings.snapshot_max_age {
            let last_snapshot_at = self.runner.store().last_snapshot_at()?;
            observations.push(host::snapshot_age(last_snapshot_at, max_age, now));
        }
        Ok(one_per_key(observations))
    }

    /// The daily summary of this server and its peers, once a day after `digest_at`.
    async fn due_digest(
        &self,
        settings: &MonitorSettings,
        round: &[(Observation, CheckState)],
        now: i64,
    ) -> Result<Option<String>, MonitorError> {
        let Some(digest_at) = settings.digest_at else {
            return Ok(None);
        };
        let today = settings.utc_offset.day(now).to_string();
        if settings.utc_offset.minute_of_day(now) < digest_at.minutes
            || self.db.meta(DIGEST_DAY)?.as_deref() == Some(today.as_str())
        {
            return Ok(None);
        }
        self.db.set_meta(DIGEST_DAY, &today)?;
        let letterhead = self.letterhead(settings);
        let mut sections = vec![Section::Host(HostReport {
            name: letterhead.host.clone(),
            checks: round.to_vec(),
            updates: self.runner.store().last_update_check()?,
        })];
        for peer in &settings.peers {
            sections.push(peers::section(peer).await);
        }
        Ok(Some(letterhead.digest(&sections, now)))
    }

    fn display_name(&self, settings: &MonitorSettings) -> String {
        settings
            .display_name
            .clone()
            .unwrap_or_else(|| self.host.clone())
    }

    fn letterhead(&self, settings: &MonitorSettings) -> Letterhead {
        Letterhead {
            host: self.display_name(settings),
            offset: settings.utc_offset,
            repeat_every: settings.repeat_every.as_secs() as i64,
        }
    }

    /// Sends a test message now, outside the outbox, so the caller sees any error.
    pub async fn send_test(&self) -> Result<(), MonitorError> {
        let active = self.active();
        let telegram = active.telegram.as_ref().ok_or(MonitorError::NoTelegram)?;
        let text = self.letterhead(&active.settings).test();
        Ok(telegram.send(&text).await?)
    }

    /// The `GET /monitor` body: every check with its state.
    pub fn report(&self) -> Result<Value, MonitorError> {
        let active = self.active();
        let checks = self.db.checks()?;
        Ok(json!({
            "host": self.host,
            "display_name": self.display_name(&active.settings),
            "updates": self.runner.store().last_update_check()?,
            "enabled": active.settings.enabled,
            "interval_secs": active.settings.interval.as_secs(),
            "telegram": active.telegram.is_some(),
            "outbox": self.db.outbox_len()?,
            "checks": checks,
        }))
    }
}

/// Keeps the worst observation per key, sorted by key.
fn one_per_key(observations: impl IntoIterator<Item = Observation>) -> Vec<Observation> {
    let mut by_key: BTreeMap<String, Observation> = BTreeMap::new();
    for observation in observations {
        match by_key.get(&observation.key) {
            Some(kept) if kept.level >= observation.level => {}
            _ => {
                by_key.insert(observation.key.clone(), observation);
            }
        }
    }
    by_key.into_values().collect()
}

async fn when<T>(enabled: bool, check: impl Future<Output = T>) -> Option<T> {
    if enabled { Some(check.await) } else { None }
}
