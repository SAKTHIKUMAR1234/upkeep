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
    #[error("{0}")]
    Failed(String),
}

/// Sends messages with the Telegram Bot API. The token is never logged or shown.
pub struct Telegram {
    client: reqwest::Client,
    send_url: String,
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
            send_url: format!(
                "{}/bot{}/sendMessage",
                settings.api_url.trim_end_matches('/'),
                token.as_str()
            ),
            chat_id: settings.chat_id.clone(),
            message_thread_id: settings.message_thread_id,
        })
    }

    pub async fn send(&self, text: &str) -> Result<(), SendError> {
        let mut body = json!({
            "chat_id": self.chat_id,
            "text": text,
            "disable_web_page_preview": true,
        });
        if let Some(thread) = self.message_thread_id {
            body["message_thread_id"] = thread.into();
        }
        // `without_url` keeps the token, which is part of the URL, out of the error.
        let response = self
            .client
            .post(&self.send_url)
            .json(&body)
            .send()
            .await
            .map_err(|error| SendError::Failed(error.without_url().to_string()))?;
        let status = response.status();
        let reply: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() && reply["ok"] == true {
            return Ok(());
        }
        if let Some(seconds) = reply["parameters"]["retry_after"].as_i64() {
            return Err(SendError::RetryAfter(seconds));
        }
        let description = reply["description"].as_str().unwrap_or("no description");
        Err(SendError::Failed(format!("HTTP {status}: {description}")))
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
/// Returns how many were sent.
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
        match telegram.send(&pending.text).await {
            Ok(()) => {
                db.delivered(pending.id)?;
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
    #[derive(Clone, Default)]
    struct FakeBot {
        failures: Arc<Mutex<u32>>,
        received: Arc<Mutex<Vec<(String, Value)>>>,
    }

    async fn send_message(
        State(bot): State<FakeBot>,
        Path(bot_path): Path<String>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let mut failures = bot.failures.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            let reply = json!({"ok": false, "error_code": 429, "description": "Too Many Requests",
                               "parameters": {"retry_after": 7}});
            return (StatusCode::TOO_MANY_REQUESTS, Json(reply));
        }
        bot.received.lock().unwrap().push((bot_path, body));
        (StatusCode::OK, Json(json!({"ok": true, "result": {}})))
    }

    async fn start(bot: FakeBot) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/{bot}/sendMessage", post(send_message))
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
        let texts: Vec<&Value> = received.iter().map(|(_, body)| &body["text"]).collect();
        assert_eq!(texts, ["first", "second"]);
        assert_eq!(received[0].0, "bot123:abc");
        assert_eq!(received[0].1["chat_id"], "-100");
        assert_eq!(received[0].1["message_thread_id"], 9);
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
