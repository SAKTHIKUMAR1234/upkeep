use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::future::join_all;
use serde_json::{Value, json};

use super::settings::WebSettings;
use crate::client::{Client, ClientError};
use crate::monitor::checks::Level;
use crate::monitor::db::CheckRow;
use crate::monitor::message::wording::describe;

const RUNNER_TIMEOUT: Duration = Duration::from_secs(15);
/// The page shows at most the end of this many bytes of a job log.
const LOG_LIMIT: usize = 512 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("{message}")]
    Status { status: u16, message: String },
    #[error("server not reachable: {0}")]
    Unreachable(String),
}

impl From<ClientError> for RunnerError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Status { status, message } => Self::Status { status, message },
            ClientError::Connect { reason, .. } => Self::Unreachable(reason.to_string()),
            other => Self::Unreachable(other.to_string()),
        }
    }
}

/// A runner the page drives, with the client that holds its token.
pub struct Runner {
    pub label: String,
    client: Client,
}

/// The configured runners by name.
pub struct Runners(BTreeMap<String, Runner>);

impl Runners {
    /// Reads every runner's token file; a token readable by others fails here.
    pub fn new(settings: &WebSettings) -> anyhow::Result<Self> {
        let mut runners = BTreeMap::new();
        for (name, host) in &settings.hosts {
            let runner = Runner {
                label: host.label.clone(),
                client: Client::new(host.host()?.endpoint()?),
            };
            runners.insert(name.clone(), runner);
        }
        Ok(Self(runners))
    }

    pub fn get(&self, name: &str) -> Option<&Runner> {
        self.0.get(name)
    }

    /// One card per runner, asked at the same time.
    pub async fn cards(&self) -> Vec<Value> {
        join_all(self.0.iter().map(|(name, runner)| runner.card(name))).await
    }
}

impl Runner {
    pub async fn get(&self, path: &str) -> Result<Value, RunnerError> {
        within_timeout(self.client.get(path)).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, RunnerError> {
        within_timeout(self.client.post(path, body)).await
    }

    /// The job log, or its last `LOG_LIMIT` bytes.
    pub async fn log(&self, id: i64) -> Result<String, RunnerError> {
        let mut log = Vec::new();
        let path = format!("/jobs/{id}/log");
        let read = self.client.stream(&path, |chunk| {
            log.extend_from_slice(chunk);
            if log.len() > 2 * LOG_LIMIT {
                log.drain(..log.len() - LOG_LIMIT);
            }
        });
        within_timeout(read).await?;
        let start = log.len().saturating_sub(LOG_LIMIT);
        Ok(String::from_utf8_lossy(&log[start..]).into_owned())
    }

    /// The server card: its checks in short and its latest job, or why it cannot be reached.
    async fn card(&self, name: &str) -> Value {
        let (report, jobs) = tokio::join!(self.get("/monitor"), self.get("/jobs?limit=1"));
        let mut card = json!({"name": name, "label": self.label});
        match (report, jobs) {
            (Ok(report), Ok(jobs)) => {
                card["reachable"] = true.into();
                card["monitor"] = summary(&report);
                card["latest_job"] = jobs["jobs"][0].clone();
            }
            (Err(error), _) | (_, Err(error)) => {
                card["reachable"] = false.into();
                card["error"] = error.to_string().into();
            }
        }
        card
    }
}

async fn within_timeout<T>(
    request: impl Future<Output = Result<T, ClientError>>,
) -> Result<T, RunnerError> {
    match tokio::time::timeout(RUNNER_TIMEOUT, request).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(RunnerError::Unreachable(format!(
            "no answer in {}s",
            RUNNER_TIMEOUT.as_secs()
        ))),
    }
}

/// The checks of a runner's `GET /monitor` in short: counts by level and the problems, worst first.
pub fn summary(report: &Value) -> Value {
    let mut checks: Vec<CheckRow> =
        serde_json::from_value(report["checks"].clone()).unwrap_or_default();
    checks.sort_by_key(|check| std::cmp::Reverse(check.observed));
    let count = |level: Level| {
        checks
            .iter()
            .filter(|check| check.observed == level)
            .count()
    };
    let host = report["display_name"].as_str().unwrap_or_default();
    let problems: Vec<Value> = checks
        .iter()
        .filter(|check| check.observed != Level::Ok)
        .map(|check| {
            let title = describe(&check.observation(), host).title;
            json!({"level": check.observed, "title": unescape(&title)})
        })
        .collect();
    json!({
        "enabled": report["enabled"],
        "ok": count(Level::Ok),
        "warn": count(Level::Warn),
        "critical": count(Level::Critical),
        "problems": problems,
        "updates": report["updates"],
    })
}

/// Undoes the Telegram HTML escaping of a check title; the page shows it as text.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(key: &str, level: &str, value: &str, amount: Option<f64>) -> Value {
        json!({
            "key": key, "level": level, "observed": level, "value": value, "amount": amount,
            "since": 0, "streak": 0, "rounds": 1, "notified": false, "updated_at": 0,
        })
    }

    #[test]
    fn summary_counts_levels_and_lists_problems_worst_first() {
        let report = json!({
            "display_name": "Replica server",
            "enabled": true,
            "updates": {"packages": 51, "risky": 1, "checked_at": 0},
            "checks": [
                check("load", "ok", "0.1", Some(0.1)),
                check("disk:/", "warn", "84% used", Some(84.0)),
                check("service:app/web", "critical", "down", None),
            ],
        });
        let summary = summary(&report);
        assert_eq!(
            (&summary["ok"], &summary["warn"], &summary["critical"]),
            (&json!(1), &json!(1), &json!(1))
        );
        assert_eq!(summary["problems"][0]["level"], "critical");
        assert_eq!(summary["problems"][0]["title"], "web is down");
        assert_eq!(summary["problems"][1]["level"], "warn");
        assert_eq!(summary["updates"]["packages"], 51);
    }

    #[test]
    fn unescape_reverses_the_telegram_escaping() {
        assert_eq!(unescape("a &amp;lt; b &lt;c&gt;"), "a &lt; b <c>");
    }
}
