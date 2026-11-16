mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::post;
use common::Harness;
use serde_json::{Value, json};

const BOT_TOKEN: &str = "4242:test-bot-token";
/// Every check kind except the adapter services and pings, peers and the disk.
const QUIET_KINDS: &str =
    r#""inodes", "memory", "swap", "load", "failed-units", "reboot-required", "security-updates""#;

/// A local stand-in for the Telegram Bot API that records each message text.
#[derive(Clone, Default)]
struct FakeBot {
    texts: Arc<Mutex<Vec<String>>>,
    down: Arc<Mutex<bool>>,
}

impl FakeBot {
    async fn start(&self) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/{bot}/sendMessage", post(send_message))
            .with_state(self.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{address}")
    }

    fn texts(&self) -> Vec<String> {
        self.texts.lock().unwrap().clone()
    }

    fn set_down(&self, down: bool) {
        *self.down.lock().unwrap() = down;
    }

    /// Waits for a message that starts with `title` and contains `detail`.
    async fn expect(&self, harness: &Harness, title: &str, detail: &str) -> String {
        harness
            .wait_until(&format!("a {title:?} message about {detail:?}"), || async {
                self.texts()
                    .into_iter()
                    .find(|text| text.starts_with(title) && text.contains(detail))
            })
            .await
    }
}

async fn send_message(
    State(bot): State<FakeBot>,
    UrlPath(bot_path): UrlPath<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if *bot.down.lock().unwrap() {
        let reply = json!({"ok": false, "error_code": 429, "description": "Too Many Requests",
                           "parameters": {"retry_after": 1}});
        return (StatusCode::TOO_MANY_REQUESTS, Json(reply));
    }
    assert_eq!(bot_path, format!("bot{BOT_TOKEN}"));
    assert_eq!(body["chat_id"], "-1001");
    assert_eq!(body["message_thread_id"], 7);
    assert_eq!(body["parse_mode"], "HTML");
    let text = body["text"].as_str().unwrap().to_owned();
    bot.texts.lock().unwrap().push(text);
    (StatusCode::OK, Json(json!({"ok": true, "result": {}})))
}

fn write_private(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Settings with a fast monitor that sends to `api_url`. `monitor` adds lines to `[monitor]`.
fn monitor_settings(harness: &Harness, api_url: &str, monitor: &str, tail: &str) -> String {
    let token_file = harness.config_dir().join("telegram-token");
    write_private(&token_file, &format!("{BOT_TOKEN}\n"));
    format!(
        "host = \"test-host\"\n[monitor]\ninterval = \"300ms\"\nalert_after = 2\n{monitor}\n\
         [monitor.telegram]\ntoken_file = \"{}\"\nchat_id = \"-1001\"\nmessage_thread_id = 7\napi_url = \"{api_url}\"\n{tail}",
        token_file.display()
    )
}

fn disk_settings(harness: &Harness, api_url: &str, warn: f64, critical: f64) -> String {
    let monitor = format!(
        "disable = [\"services\", {QUIET_KINDS}]\ndisk_mounts = [\"{}\"]\ndisk = {{ warn = {warn:?}, critical = {critical:?} }}",
        harness.config_dir().display()
    );
    monitor_settings(harness, api_url, &monitor, "")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disk_over_its_threshold_alerts_then_recovers() {
    let bot = FakeBot::default();
    let api_url = bot.start().await;
    let mut harness = Harness::new();
    harness.write_settings(&disk_settings(&harness, &api_url, 0.0, 0.0));
    harness.start().await;

    let disk = format!("<code>{}</code> has ", harness.config_dir().display());
    let alert = bot
        .expect(&harness, "<b>🔴 CRITICAL · test-host</b>\n<b>Disk ", &disk)
        .await;
    assert!(alert.contains("% full</b>\n"), "{alert}");
    assert!(
        alert.contains("\nSince ") && alert.contains(" · checked 2 times\n"),
        "{alert}"
    );

    let report = harness.client().get("/monitor").await.unwrap();
    let key = format!("disk:{}", harness.config_dir().display());
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["key"] == key.as_str())
        .unwrap();
    assert_eq!(
        (check["level"].as_str(), check["observed"].as_str()),
        (Some("critical"), Some("critical"))
    );

    // A raised threshold applies on the next round, without a restart.
    harness.write_settings(&disk_settings(&harness, &api_url, 99.99, 100.0));
    let fixed = format!("Disk {} has space again", harness.config_dir().display());
    let recovery = bot
        .expect(&harness, "<b>✅ FIXED · test-host</b>\n", &fixed)
        .await;
    assert!(recovery.contains("\nLasted "), "{recovery}");
    let alerts = bot
        .texts()
        .iter()
        .filter(|text| text.starts_with("<b>🔴 CRITICAL"))
        .count();
    assert_eq!(alerts, 1, "one alert, not one per round: {:?}", bot.texts());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_ping_alerts_then_recovers() {
    let bot = FakeBot::default();
    let api_url = bot.start().await;
    let mut harness = Harness::new();
    let monitor = format!("disable = [\"disk\", {QUIET_KINDS}]");
    harness.write_settings(&monitor_settings(&harness, &api_url, &monitor, ""));
    harness.start().await;
    std::fs::write(harness.adapter_state("demo-app").join("broken"), "").unwrap();
    bot.expect(
        &harness,
        "<b>🔴 CRITICAL · test-host</b>\n",
        "<b>Site demo-app answers not answering</b>",
    )
    .await;
    std::fs::remove_file(harness.adapter_state("demo-app").join("broken")).unwrap();
    bot.expect(
        &harness,
        "<b>✅ FIXED · test-host</b>\n",
        "<b>Site demo-app answers answers again</b>\nWas down for ",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn alerts_wait_in_the_outbox_across_a_runner_restart() {
    let bot = FakeBot::default();
    let api_url = bot.start().await;
    bot.set_down(true);
    let mut harness = Harness::new();
    harness.write_settings(&disk_settings(&harness, &api_url, 0.0, 0.0));
    harness.start().await;

    harness
        .wait_until("an alert waits in the outbox", || async {
            let report = harness.client().get("/monitor").await.ok()?;
            (report["outbox"].as_i64() > Some(0)).then_some(())
        })
        .await;
    harness.kill();
    bot.set_down(false);
    harness.start().await;

    bot.expect(&harness, "<b>🔴 CRITICAL · test-host</b>\n", "<b>Disk ")
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_peer_runner_alerts_from_its_watcher_then_recovers() {
    let bot = FakeBot::default();
    let api_url = bot.start().await;

    let mut peer = Harness::new();
    let peer_port = free_port();
    write_private(&peer.config_dir().join("token"), "peer-secret\n");
    peer.write_settings(&format!(
        "listen = \"127.0.0.1:{peer_port}\"\n[monitor]\nenabled = false\n"
    ));
    peer.start().await;

    let mut watcher = Harness::new();
    let peer_token = watcher.config_dir().join("peer-b.token");
    write_private(&peer_token, "peer-secret\n");
    let monitor =
        format!("digest_at = \"00:00\"\ndisable = [\"services\", \"disk\", {QUIET_KINDS}]");
    let peers = format!(
        "[[monitor.peer]]\nname = \"b\"\ndisplay_name = \"Peer <b>\"\nurl = \"http://127.0.0.1:{peer_port}\"\ntoken_file = \"{}\"\n",
        peer_token.display()
    );
    watcher.write_settings(&monitor_settings(&watcher, &api_url, &monitor, &peers));
    watcher.start().await;

    watcher
        .wait_until("the peer check is ok", || async {
            let report = watcher.client().get("/monitor").await.ok()?;
            (report["checks"][0]["key"] == "peer:b" && report["checks"][0]["level"] == "ok")
                .then_some(())
        })
        .await;
    let summary = bot
        .expect(
            &watcher,
            "<b>📊 Daily summary · ",
            "<b>test-host</b> · ✅ all OK",
        )
        .await;
    assert!(
        summary.contains("\n\n<b>Peer &lt;b&gt;</b> · ✅ all OK\n0 checks OK"),
        "{summary}"
    );
    assert_eq!(bot.texts().len(), 1, "{:?}", bot.texts());

    peer.kill();
    let down = bot
        .expect(
            &watcher,
            "<b>🚨 SERVER NOT ANSWERING · Peer &lt;b&gt;</b>\n",
            "Error: Connection refused",
        )
        .await;
    assert!(
        down.contains("\ntest-host cannot reach Upkeep on Peer &lt;b&gt;.\n"),
        "{down}"
    );
    peer.start().await;
    bot.expect(
        &watcher,
        "<b>✅ FIXED · Peer &lt;b&gt;</b>\n",
        "Seen from test-host.",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn alert_test_and_status_all_from_the_cli() {
    let bot = FakeBot::default();
    let api_url = bot.start().await;
    let mut harness = Harness::new();
    let port = free_port();
    write_private(&harness.config_dir().join("token"), "runner-secret\n");
    let monitor = format!("disable = [\"disk\", {QUIET_KINDS}]");
    let settings = monitor_settings(&harness, &api_url, &monitor, "");
    harness.write_settings(&format!("listen = \"127.0.0.1:{port}\"\n{settings}"));
    harness.start().await;

    let output = harness.command("alert").arg("test").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        bot.texts()[0].lines().take(2).collect::<Vec<_>>(),
        ["<b>🧪 TEST · test-host</b>", "Upkeep can reach this chat."]
    );

    let client_dir = tempfile::tempdir().unwrap();
    let host_token = client_dir.path().join("vm.token");
    write_private(&host_token, "runner-secret\n");
    let client = |args: &[&str]| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_upkeep"))
            .arg("--client-dir")
            .arg(client_dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let url = format!("http://127.0.0.1:{port}");
    client(&[
        "host",
        "add",
        "vm",
        "--url",
        &url,
        "--token-file",
        host_token.to_str().unwrap(),
    ]);
    client(&[
        "host",
        "add",
        "gone",
        "--url",
        &format!("http://127.0.0.1:{}", free_port()),
        "--token-file",
        host_token.to_str().unwrap(),
    ]);

    harness
        .wait_until("the first round", || async {
            let report = harness.client().get("/monitor").await.ok()?;
            (!report["checks"].as_array()?.is_empty()).then_some(())
        })
        .await;
    let table = client(&["status", "--all"]);
    println!("{table}");
    assert!(
        table
            .lines()
            .any(|line| line.starts_with("vm ") && line.contains(" ok ")),
        "{table}"
    );
    assert!(
        table
            .lines()
            .any(|line| line.starts_with("gone ") && line.contains("UNREACHABLE")),
        "{table}"
    );
    let one = client(&["--host", "vm", "status"]);
    assert!(one.contains("ping:demo-app/demo-app answers"), "{one}");
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_jobs_announce_their_start_and_end() {
    let bot = FakeBot::default();
    let api_url = bot.start().await;
    let mut harness = Harness::new();
    let monitor = "enabled = false\ndisplay_name = \"YRP server (.22)\"";
    harness.write_settings(&monitor_settings(&harness, &api_url, monitor, ""));
    harness.add_adapter(
        "quick",
        "[actions.first]\n[actions.second]\n[actions.broken]\n[actions.cleanup]\n",
        "[ \"$1\" != broken ]\n",
    );
    harness.add_plan("good", "steps = [\"quick:first\", \"quick:second\"]\n");
    harness.add_plan(
        "bad",
        "steps = [\"quick:first\", \"quick:broken\", \"quick:second\"]\non_failure = [\"quick:cleanup\"]\n",
    );
    harness.add_plan("quiet", "steps = [\"quick:first\"]\nnotify = false\n");
    harness.start().await;

    let id = harness.run("good", false).await.unwrap();
    harness.wait_for_status(id, "succeeded").await;
    let started = bot
        .expect(
            &harness,
            "<b>🛠️ UPDATE STARTED · YRP server (.22)</b>\n",
            "<b>good</b>",
        )
        .await;
    assert!(
        started.ends_with("2 steps: <code>quick:first</code>, <code>quick:second</code>"),
        "{started}"
    );
    bot.expect(
        &harness,
        "<b>✅ UPDATE DONE · YRP server (.22)</b>\n",
        "<b>good</b> · all 2 steps done",
    )
    .await;

    let id = harness.run("bad", false).await.unwrap();
    harness.wait_for_status(id, "failed").await;
    let failed = bot
        .expect(
            &harness,
            "<b>❌ UPDATE FAILED · YRP server (.22)</b>\n",
            "Failed at step 2/3 <code>quick:broken</code>: exit code 1",
        )
        .await;
    assert!(
        failed.contains("\nRecovery: 1 step ran, all OK\n"),
        "{failed}"
    );

    let id = harness.run("quiet", false).await.unwrap();
    harness.wait_for_status(id, "succeeded").await;
    let id = harness.run("quick:first", false).await.unwrap();
    harness.wait_for_status(id, "succeeded").await;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert_eq!(bot.texts().len(), 4, "{:?}", bot.texts());
}
