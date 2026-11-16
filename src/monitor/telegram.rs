use std::fmt;
use std::time::Duration;

use serde_json::{Value, json};

use super::db::MonitorDb;
use super::settings::TelegramSettings;
use crate::api::auth::{Token, TokenError};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const FIRST_RETRY_SECS: i64 = 15;
const MAX_RETRY_SECS: i64 = 3600;
/// Messages that could not be sent for this long are dropped.
pub const OUTBOX_MAX_AGE_SECS: i64 = 2 * 86_400;

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("Telegram asks to wait {0}s")]
    RetryAfter(i64),
    /// Telegram refused the request itself, for example an edit of a deleted message.
    #[error("{0}")]
    Rejected(String),
    #[error("{0}")]
    Failed(String),
}

/// Sends messages with the Telegram Bot API. The token is never logged or shown.
pub struct Telegram {
    client: reqwest::Client,
    /// The Bot API URL with the token, without the method name.
    bot_url: String,
    chat_id: String,
    message_thread_id: Option<i64>,
}

impl fmt::Debug for Telegram {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Telegram")
            .field("chat_id", &self.chat_id)
            .field("message_thread_id", &self.message_thread_id)
            .finish_non_exhaustive()
    }
}

impl Telegram {
    /// Reads the bot token from its owner-only file.
    pub fn new(settings: &TelegramSettings) -> Result<Self, TokenError> {
        let token = Token::load(&settings.token_file)?;
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("the HTTP client settings are valid");
        Ok(Self {
            client,
            bot_url: format!(
                "{}/bot{}",
                settings.api_url.trim_end_matches('/'),
                token.as_str()
            ),
            chat_id: settings.chat_id.clone(),
            message_thread_id: settings.message_thread_id,
        })
    }

    /// Sends a message and returns its id, which `edit` needs.
    pub async fn send(&self, text: &str) -> Result<Option<i64>, SendError> {
        let mut body = self.body(text);
        if let Some(thread) = self.message_thread_id {
            body["message_thread_id"] = thread.into();
        }
        let reply = self.call("sendMessage", &body).await?;
        Ok(reply["result"]["message_id"].as_i64())
    }

    /// Replaces the text of a sent message. An unchanged text counts as done.
    pub async fn edit(&self, message_id: i64, text: &str) -> Result<(), SendError> {
        let mut body = self.body(text);
        body["message_id"] = message_id.into();
        match self.call("editMessageText", &body).await {
            Err(SendError::Rejected(reason)) if reason.contains("message is not modified") => {
                Ok(())
            }
            other => other.map(drop),
        }
    }

    fn body(&self, text: &str) -> Value {
        json!({
            "chat_id": self.chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        })
    }

    async fn call(&self, method: &str, body: &Value) -> Result<Value, SendError> {
        // `without_url` keeps the token, which is part of the URL, out of the error.
        let response = self
            .client
            .post(format!("{}/{method}", self.bot_url))
            .json(body)
            .send()
            .await
            .map_err(|error| SendError::Failed(error.without_url().to_string()))?;
        let status = response.status();
        let reply: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() && reply["ok"] == true {
            return Ok(reply);
        }
        if let Some(seconds) = reply["parameters"]["retry_after"].as_i64() {
            return Err(SendError::RetryAfter(seconds));
        }
        let description = reply["description"].as_str().unwrap_or("no description");
        let reason = format!("HTTP {status}: {description}");
        if status == reqwest::StatusCode::BAD_REQUEST {
            return Err(SendError::Rejected(reason));
        }
        Err(SendError::Failed(reason))
    }
}

/// Seconds to wait before the next try, doubling from 15 s up to an hour.
pub fn retry_delay(attempts: u32, error: &SendError) -> i64 {
    if let SendError::RetryAfter(seconds) = error {
        return (*seconds).clamp(1, MAX_RETRY_SECS);
    }
    let doubled = FIRST_RETRY_SECS.saturating_mul(1 << attempts.min(12));
    doubled.min(MAX_RETRY_SECS)
}

/// Sends due outbox messages in order. Stops at the first failure so that order is kept.
/// A job's text edits the job's message once it was sent. Returns how many were sent.
pub async fn flush(db: &MonitorDb, telegram: &Telegram, now: i64) -> rusqlite::Result<usize> {
    let expired = db.expire(now - OUTBOX_MAX_AGE_SECS)?;
    if expired > 0 {
        tracing::error!(
            expired,
            "dropped alerts that could not be sent for two days"
        );
    }
    let mut sent = 0;
    for pending in db.due(now)? {
        let message_id = match pending.job_id {
            Some(job_id) => db.job_message_id(job_id)?,
            None => None,
        };
        match deliver(telegram, &pending.text, message_id).await {
            Ok(message_id) => {
                db.delivered(&pending, message_id, now)?;
                let title = pending.text.lines().next().unwrap_or_default();
                tracing::info!(id = pending.id, title, "alert sent");
                sent += 1;
            }
            Err(error) => {
                let delay = retry_delay(pending.attempts, &error);
                tracing::warn!(id = pending.id, %error, retry_in = delay, "cannot send alert");
                db.retry_later(pending.id, now + delay, &error.to_string())?;
                break;
            }
        }
    }
    Ok(sent)
}

/// Edits message `edit` when given, else sends a new one. When Telegram refuses the edit,
/// for example because the message was deleted, a new message replaces it.
async fn deliver(
    telegram: &Telegram,
    text: &str,
    edit: Option<i64>,
) -> Result<Option<i64>, SendError> {
    if let Some(message_id) = edit {
        match telegram.edit(message_id, text).await {
            Err(SendError::Rejected(reason)) => {
                tracing::warn!(message_id, %reason, "cannot edit the job message: sending a new one");
            }
            other => return other.map(|()| Some(message_id)),
        }
    }
    telegram.send(text).await
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    use axum::Json;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::routing::post;

    use super::*;

    /// A Bot API that fails the first `failures` requests, then records the rest.
    /// Messages get ids from 100; an edit of a message in `deleted` is refused.
    #[derive(Clone, Default)]
    struct FakeBot {
        failures: Arc<Mutex<u32>>,
        received: Arc<Mutex<Vec<(String, String, Value)>>>,
        deleted: Arc<Mutex<Vec<i64>>>,
    }

    async fn bot_method(
        State(bot): State<FakeBot>,
        Path((bot_path, method)): Path<(String, String)>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let mut failures = bot.failures.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            let reply = json!({"ok": false, "error_code": 429, "description": "Too Many Requests",
                               "parameters": {"retry_after": 7}});
            return (StatusCode::TOO_MANY_REQUESTS, Json(reply));
        }
        if method == "editMessageText"
            && bot
                .deleted
                .lock()
                .unwrap()
                .contains(&body["message_id"].as_i64().unwrap())
        {
            let reply = json!({"ok": false, "error_code": 400,
                               "description": "Bad Request: message to edit not found"});
            return (StatusCode::BAD_REQUEST, Json(reply));
        }
        let mut received = bot.received.lock().unwrap();
        received.push((bot_path, method, body));
        let message_id = 99 + received.len() as i64;
        (
            StatusCode::OK,
            Json(json!({"ok": true, "result": {"message_id": message_id}})),
        )
    }

    async fn start(bot: FakeBot) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/{bot}/{method}", post(bot_method))
            .with_state(bot);
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{address}")
    }

    fn telegram(dir: &std::path::Path, api_url: String) -> Telegram {
        let token_file = dir.join("telegram-token");
        std::fs::write(&token_file, "123:abc\n").unwrap();
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        Telegram::new(&TelegramSettings {
            token_file,
            chat_id: "-100".into(),
            message_thread_id: Some(9),
            api_url,
        })
        .unwrap()
    }

    #[test]
    fn retries_back_off_and_honour_retry_after() {
        let failed = SendError::Failed("HTTP 502".into());
        let delays: Vec<i64> = (0..4)
            .map(|attempts| retry_delay(attempts, &failed))
            .collect();
        assert_eq!(delays, [15, 30, 60, 120]);
        assert_eq!(retry_delay(30, &failed), 3600);
        assert_eq!(retry_delay(0, &SendError::RetryAfter(7)), 7);
    }

    #[tokio::test]
    async fn outbox_retries_in_order_until_sent() {
        let dir = tempfile::tempdir().unwrap();
        let db = MonitorDb::open(&dir.path().join("monitor.db")).unwrap();
        let bot = FakeBot::default();
        *bot.failures.lock().unwrap() = 1;
        let telegram = telegram(dir.path(), start(bot.clone()).await);
        db.enqueue("first", 100).unwrap();
        db.enqueue("second", 100).unwrap();

        assert_eq!(flush(&db, &telegram, 100).await.unwrap(), 0);
        assert_eq!(flush(&db, &telegram, 106).await.unwrap(), 0);
        assert_eq!(flush(&db, &telegram, 107).await.unwrap(), 2);

        let received = bot.received.lock().unwrap();
        let texts: Vec<&Value> = received.iter().map(|(_, _, body)| &body["text"]).collect();
        assert_eq!(texts, ["first", "second"]);
        assert_eq!(received[0].0, "bot123:abc");
        assert_eq!(received[0].1, "sendMessage");
        assert_eq!(received[0].2["chat_id"], "-100");
        assert_eq!(received[0].2["message_thread_id"], 9);
        assert_eq!(received[0].2["parse_mode"], "HTML");
        assert_eq!(db.outbox_len().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_job_message_is_sent_once_then_edited() {
        let dir = tempfile::tempdir().unwrap();
        let db = MonitorDb::open(&dir.path().join("monitor.db")).unwrap();
        let bot = FakeBot::default();
        let telegram = telegram(dir.path(), start(bot.clone()).await);

        db.queue_job_text(7, "running", 100).unwrap();
        assert_eq!(flush(&db, &telegram, 100).await.unwrap(), 1);
        db.queue_job_text(7, "done", 110).unwrap();
        assert_eq!(flush(&db, &telegram, 110).await.unwrap(), 1);

        let received = bot.received.lock().unwrap().clone();
        let calls: Vec<(&str, &Value, &Value)> = received
            .iter()
            .map(|(_, method, body)| (method.as_str(), &body["text"], &body["message_id"]))
            .collect();
        assert_eq!(
            calls,
            [
                ("sendMessage", &json!("running"), &Value::Null),
                ("editMessageText", &json!("done"), &json!(100)),
            ]
        );
        assert_eq!(received[1].2["chat_id"], "-100");
        assert_eq!(db.job_message_id(7).unwrap(), Some(100));
    }

    #[tokio::test]
    async fn a_refused_edit_sends_a_new_message() {
        let dir = tempfile::tempdir().unwrap();
        let db = MonitorDb::open(&dir.path().join("monitor.db")).unwrap();
        let bot = FakeBot::default();
        let telegram = telegram(dir.path(), start(bot.clone()).await);
        db.queue_job_text(7, "running", 100).unwrap();
        flush(&db, &telegram, 100).await.unwrap();
        bot.deleted.lock().unwrap().push(100);

        db.queue_job_text(7, "done", 110).unwrap();
        assert_eq!(flush(&db, &telegram, 110).await.unwrap(), 1);
        let methods: Vec<String> = bot
            .received
            .lock()
            .unwrap()
            .iter()
            .map(|(_, method, _)| method.clone())
            .collect();
        assert_eq!(methods, ["sendMessage", "sendMessage"]);
        assert_eq!(db.job_message_id(7).unwrap(), Some(101));
        assert_eq!(db.outbox_len().unwrap(), 0);
    }

    #[tokio::test]
    async fn errors_never_show_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let unreachable = "http://127.0.0.1:9".to_owned();
        let error = telegram(dir.path(), unreachable)
            .send("x")
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("123:abc"), "{error}");
        assert!(!format!("{:?}", telegram(dir.path(), "http://x".into())).contains("abc"));
    }
}
