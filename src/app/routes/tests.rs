use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

use crate::app::auth::password;
use crate::app::store::{Db, NewRun, RunStatus};
use crate::app::{AppConfig, Service};

const PASSWORD: &str = "correct horse battery";

/// Stands in for `ssh`: runs the remote command on this machine with `/.upkeep` moved to a test
/// folder, and fails like `ssh` while `<root>.down` exists.
const FAKE_SSH: &str = r#"#!/bin/bash
root="__ROOT__"
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break;;
    -i|-p|-o|-l) shift 2;;
    *) shift;;
  esac
done
shift
if [ -e "$root.down" ]; then echo "ssh: connect to host: Connection refused" >&2; exit 255; fi
sudo() {
  [ "$1" = -n ] && shift
  case "$1" in
    chown) return 0;;
    install) shift; local args=(); while [ $# -gt 0 ]; do
      case "$1" in -o|-g) shift 2;; *) args+=("$1"); shift;; esac; done
      command install "${args[@]}";;
    *) "$@";;
  esac
}
export -f sudo
bash -c "${1//\/.upkeep/$root}" | sed -u "s#$root#/.upkeep#g"
exit "${PIPESTATUS[0]}"
"#;

struct World {
    base: String,
    http: reqwest::Client,
    db: Arc<Db>,
    dir: tempfile::TempDir,
}

/// A signed-in browser: its cookie and CSRF token.
struct Session {
    cookie: String,
    csrf: String,
}

fn executable(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl World {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("remote-upkeep");
        let ssh = dir.path().join("ssh");
        executable(
            &ssh,
            &FAKE_SSH.replace("__ROOT__", &root.display().to_string()),
        );
        let key = dir.path().join("id_test");
        std::fs::write(&key, "key").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();

        let mut config = AppConfig::new(dir.path().join("data"));
        config.ssh_program = ssh;
        let down = format!("{}.down", root.display());
        config.reboot_command =
            format!("touch {down}; (sleep 0.6; rm -f {down}) >/dev/null 2>&1 &");
        config.reboot_poll = Duration::from_millis(200);
        config.reboot_timeout = Duration::from_secs(20);
        let db = Arc::new(config.open_db().unwrap());
        let hash = password::hash(PASSWORD).unwrap();
        db.add_user("owner@example.com", &hash, 1).unwrap();

        let service = Service::bind(config, 0).await.unwrap();
        let base = format!("http://127.0.0.1:{}", service.address().port());
        tokio::spawn(service.run(std::future::pending()));
        Self {
            base,
            http: reqwest::Client::new(),
            db,
            dir,
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("remote-upkeep")
    }

    fn key(&self) -> String {
        self.dir.path().join("id_test").display().to_string()
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        session: Option<&Session>,
    ) -> reqwest::Response {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(session) = session {
            request = request
                .header("cookie", &session.cookie)
                .header("x-csrf-token", &session.csrf);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.unwrap()
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        session: &Session,
    ) -> (StatusCode, Value) {
        let response = self.request(method, path, body, Some(session)).await;
        let status = response.status();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn sign_in_as(&self, email: &str, password: &str) -> reqwest::Response {
        let body = json!({"email": email, "password": password});
        self.request(Method::POST, "/api/session", Some(body), None)
            .await
    }

    async fn sign_in(&self) -> Session {
        let response = self.sign_in_as("Owner@Example.com", PASSWORD).await;
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .to_owned();
        let body: Value = response.json().await.unwrap();
        Session {
            cookie: cookie.split(';').next().unwrap().to_owned(),
            csrf: body["csrf"].as_str().unwrap().to_owned(),
        }
    }

    async fn add_server(&self, session: &Session) -> i64 {
        let body = json!({"name": "Replica", "address": "10.0.0.1", "ssh_user": "ubuntu", "key_path": self.key()});
        let (status, server) = self
            .call(Method::POST, "/api/servers", Some(body), session)
            .await;
        assert_eq!(status, StatusCode::CREATED, "{server}");
        server["id"].as_i64().unwrap()
    }

    async fn create_operation(
        &self,
        session: &Session,
        server: i64,
        folder: &str,
        run_sh: &str,
        check_sh: &str,
    ) {
        let body =
            json!({"title": folder, "folder": folder, "run_sh": run_sh, "check_sh": check_sh});
        let path = format!("/api/servers/{server}/operations");
        let (status, body) = self.call(Method::POST, &path, Some(body), session).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    async fn start_run(&self, session: &Session, server: i64, folder: &str) -> i64 {
        let path = format!("/api/servers/{server}/operations/{folder}/runs");
        let (status, run) = self.call(Method::POST, &path, None, session).await;
        assert_eq!(status, StatusCode::CREATED, "{run}");
        assert_eq!(run["status"], "running");
        run["id"].as_i64().unwrap()
    }

    async fn finished_run(&self, session: &Session, id: i64) -> Value {
        for _ in 0..200 {
            let (_, run) = self
                .call(Method::GET, &format!("/api/runs/{id}"), None, session)
                .await;
            if run["status"] != "running" {
                return run;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("run {id} did not end");
    }

    async fn output(&self, session: &Session, id: i64) -> String {
        let (_, chunk) = self
            .call(
                Method::GET,
                &format!("/api/runs/{id}/output"),
                None,
                session,
            )
            .await;
        chunk["text"].as_str().unwrap().to_owned()
    }

    async fn activity(&self, session: &Session) -> Vec<Value> {
        let (_, page) = self.call(Method::GET, "/api/activity", None, session).await;
        page["items"].as_array().unwrap().clone()
    }
}

fn step_titles(run: &Value) -> Vec<(String, String)> {
    run["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            (
                step["title"].as_str().unwrap().to_owned(),
                step["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn every_api_route_refuses_a_signed_out_request() {
    let world = World::new().await;
    let routes = [
        (Method::GET, "/api/session"),
        (Method::DELETE, "/api/session"),
        (Method::GET, "/api/overview"),
        (Method::GET, "/api/ssh-keys"),
        (Method::GET, "/api/servers"),
        (Method::POST, "/api/servers"),
        (Method::POST, "/api/servers/test"),
        (Method::GET, "/api/servers/1"),
        (Method::PUT, "/api/servers/1"),
        (Method::DELETE, "/api/servers/1"),
        (Method::GET, "/api/servers/1/operations"),
        (Method::POST, "/api/servers/1/operations"),
        (Method::GET, "/api/servers/1/operations/os-update"),
        (Method::PUT, "/api/servers/1/operations/os-update"),
        (Method::DELETE, "/api/servers/1/operations/os-update"),
        (Method::POST, "/api/servers/1/operations/os-update/runs"),
        (Method::GET, "/api/runs"),
        (Method::GET, "/api/runs/export.csv"),
        (Method::GET, "/api/runs/1"),
        (Method::GET, "/api/runs/1/output"),
        (Method::GET, "/api/runs/1/events"),
        (Method::POST, "/api/runs/1/stop"),
        (Method::GET, "/api/events"),
        (Method::GET, "/api/filters"),
        (Method::GET, "/api/activity"),
    ];
    for (method, path) in routes {
        let body = (method != Method::GET).then(|| json!({}));
        let response = world.request(method.clone(), path, body, None).await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
        let body: Value = response.json().await.unwrap_or(Value::Null);
        assert_eq!(body["code"], "signed_out", "{method} {path}");
    }
}

#[tokio::test]
async fn sign_in_answers_the_same_for_unknown_emails_and_wrong_passwords() {
    let world = World::new().await;
    world
        .db
        .add_user("off@example.com", &password::hash(PASSWORD).unwrap(), 1)
        .unwrap();
    world.db.set_disabled("off@example.com", true).unwrap();
    let mut messages = Vec::new();
    for (email, password) in [
        ("owner@example.com", "wrong password!"),
        ("nobody@example.com", PASSWORD),
        ("off@example.com", PASSWORD),
    ] {
        let response = world.sign_in_as(email, password).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{email}");
        assert!(response.headers().get("set-cookie").is_none());
        let body: Value = response.json().await.unwrap();
        messages.push(body["error"].clone());
    }
    assert!(
        messages
            .iter()
            .all(|message| *message == "Wrong email or password."),
        "{messages:?}"
    );

    let session = world.sign_in().await;
    let (status, me) = world
        .call(Method::GET, "/api/session", None, &session)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["email"], "owner@example.com");
    assert_eq!(me["csrf"], session.csrf);
    let actions: Vec<Value> = world
        .activity(&session)
        .await
        .iter()
        .map(|entry| entry["action"].clone())
        .collect();
    assert_eq!(
        actions,
        [
            "sign_in",
            "failed_sign_in",
            "failed_sign_in",
            "failed_sign_in"
        ]
    );
}

#[tokio::test]
async fn the_session_cookie_is_strict_and_the_password_never_returns() {
    let world = World::new().await;
    let response = world.sign_in_as("owner@example.com", PASSWORD).await;
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cookie.starts_with("upkeep_session="));
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    let text = response.text().await.unwrap();
    assert!(
        !text.contains(PASSWORD) && !text.contains("argon2"),
        "{text}"
    );
}

#[tokio::test]
async fn five_failures_lock_the_email_and_success_resets_the_count() {
    let world = World::new().await;
    for attempt in 1..=5 {
        let response = world
            .sign_in_as("owner@example.com", "wrong password!")
            .await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "attempt {attempt}"
        );
    }
    let locked = world.sign_in_as("owner@example.com", PASSWORD).await;
    assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);
    let body: Value = locked.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("Try again in 30 s"),
        "{body}"
    );
    // Unknown emails lock the same way, so a lock tells nothing about an account.
    for _ in 1..=5 {
        world
            .sign_in_as("nobody@example.com", "wrong password!")
            .await;
    }
    assert_eq!(
        world.sign_in_as("nobody@example.com", "x").await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );

    world
        .db
        .set_login_failures("owner@example.com", 4, 0)
        .unwrap();
    world.sign_in().await;
    assert_eq!(
        world.db.login_failures("owner@example.com").unwrap(),
        (0, 0)
    );
}

#[tokio::test]
async fn changes_need_the_csrf_header_and_sign_out_ends_the_session() {
    let world = World::new().await;
    let session = world.sign_in().await;
    let no_csrf = world
        .http
        .post(format!("{}/api/servers/test", world.base))
        .header("cookie", &session.cookie)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(no_csrf.status(), StatusCode::FORBIDDEN);

    let (status, _) = world
        .call(Method::DELETE, "/api/session", None, &session)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = world
        .call(Method::GET, "/api/servers", None, &session)
        .await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::UNAUTHORIZED, &json!("signed_out"))
    );
}

#[tokio::test]
async fn an_idle_session_asks_for_the_password_again() {
    let world = World::new().await;
    let session = world.sign_in().await;
    let token = session.cookie.split_once('=').unwrap().1;
    world
        .db
        .touch_session(token, crate::time::now_millis() - 16 * 60_000)
        .unwrap();
    let (status, body) = world
        .call(Method::GET, "/api/servers", None, &session)
        .await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::UNAUTHORIZED, &json!("locked"))
    );
    let (status, _) = world
        .call(Method::GET, "/api/servers", None, &session)
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "locking deleted the session"
    );
}

#[tokio::test]
async fn foreign_hosts_and_origins_are_refused_and_headers_are_set() {
    let world = World::new().await;
    let page = world.http.get(&world.base).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let headers = page.headers().clone();
    assert_eq!(
        headers["content-security-policy"],
        "default-src 'self'; frame-ancestors 'none'"
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["referrer-policy"], "no-referrer");
    assert!(page.text().await.unwrap().contains("<title>Upkeep</title>"));

    let api = world.request(Method::GET, "/api/servers", None, None).await;
    assert_eq!(api.headers()["cache-control"], "no-store");
    for name in ["app.css", "app.js", "icon.svg"] {
        let asset = world
            .http
            .get(format!("{}/assets/{name}", world.base))
            .send()
            .await
            .unwrap();
        assert_eq!(asset.status(), StatusCode::OK, "{name}");
    }
    let missing = world
        .http
        .get(format!("{}/assets/nope.js", world.base))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    let foreign_origin = world
        .http
        .post(format!("{}/api/session", world.base))
        .header("origin", "http://evil.example")
        .json(&json!({"email": "owner@example.com", "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(foreign_origin.status(), StatusCode::FORBIDDEN);
    let rebound = world
        .http
        .get(&world.base)
        .header("host", "rebind.example")
        .send()
        .await
        .unwrap();
    assert_eq!(rebound.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn servers_are_added_tested_edited_and_deleted() {
    let world = World::new().await;
    let session = world.sign_in().await;
    let open_key = world.dir.path().join("open_key");
    std::fs::write(&open_key, "key").unwrap();
    std::fs::set_permissions(&open_key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let body = json!({"name": "Replica", "address": "10.0.0.1", "ssh_user": "ubuntu", "key_path": open_key});
    let (status, error) = world
        .call(Method::POST, "/api/servers", Some(body), &session)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error["error"].as_str().unwrap().contains("chmod 600"),
        "{error}"
    );
    let body = json!({"name": "Bad", "address": "-oProxyCommand=x", "ssh_user": "ubuntu", "key_path": world.key()});
    assert_eq!(
        world
            .call(Method::POST, "/api/servers", Some(body), &session)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );

    let id = world.add_server(&session).await;
    let body = json!({"name": "Replica", "address": "10.0.0.2", "ssh_user": "ubuntu", "key_path": world.key()});
    assert_eq!(
        world
            .call(Method::POST, "/api/servers", Some(body), &session)
            .await
            .0,
        StatusCode::CONFLICT
    );

    let test = json!({"name": "", "address": "10.0.0.1", "port": 22, "ssh_user": "ubuntu", "key_path": world.key()});
    let (status, result) = world
        .call(
            Method::POST,
            "/api/servers/test",
            Some(test.clone()),
            &session,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["ok"], true);
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("/.upkeep not found"),
        "{result}"
    );
    world
        .create_operation(&session, id, "os-update", "#!/bin/sh\necho hi\n", "")
        .await;
    let (_, result) = world
        .call(Method::POST, "/api/servers/test", Some(test), &session)
        .await;
    assert_eq!(
        result["message"],
        "connected, sudo OK, /.upkeep found (1 operation)"
    );

    let (status, list) = world
        .call(Method::GET, "/api/servers", None, &session)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["servers"][0]["name"], "Replica");
    assert_eq!(list["servers"][0]["port"], 22);
    assert!(list["servers"][0].get("running_run").is_some());

    let edit = json!({"name": "Replica (.11)", "address": "10.0.0.11", "port": 2222, "ssh_user": "ubuntu", "key_path": world.key()});
    let (status, server) = world
        .call(
            Method::PUT,
            &format!("/api/servers/{id}"),
            Some(edit),
            &session,
        )
        .await;
    assert_eq!((status, &server["port"]), (StatusCode::OK, &json!(2222)));
    let (status, page) = world
        .call(Method::GET, &format!("/api/servers/{id}"), None, &session)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["server"]["name"], "Replica (.11)");
    assert_eq!(
        (page["operations"].clone(), page["runs_30d"].clone()),
        (json!(1), json!(0))
    );
    assert_eq!(
        world
            .call(Method::GET, "/api/servers/99", None, &session)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    let (status, keys) = world
        .call(Method::GET, "/api/ssh-keys", None, &session)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(keys["keys"].is_array());

    assert_eq!(
        world
            .call(
                Method::DELETE,
                &format!("/api/servers/{id}"),
                None,
                &session
            )
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        world
            .call(
                Method::DELETE,
                &format!("/api/servers/{id}"),
                None,
                &session
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let actions: Vec<Value> = world
        .activity(&session)
        .await
        .iter()
        .map(|entry| entry["action"].clone())
        .collect();
    assert_eq!(
        actions[..4],
        [
            "server_delete",
            "server_edit",
            "operation_create",
            "server_add"
        ]
    );
}

#[tokio::test]
async fn operations_are_created_read_edited_and_deleted_on_the_server() {
    let world = World::new().await;
    let session = world.sign_in().await;
    let id = world.add_server(&session).await;
    let base = format!("/api/servers/{id}/operations");

    let (status, list) = world.call(Method::GET, &base, None, &session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (list["connected"].clone(), list["operations"].clone()),
        (json!(true), json!([]))
    );

    let bad = json!({"title": "X", "folder": "Bad Name", "run_sh": "#!/bin/sh\n"});
    assert_eq!(
        world.call(Method::POST, &base, Some(bad), &session).await.0,
        StatusCode::BAD_REQUEST
    );
    let no_shebang = json!({"title": "X", "folder": "x", "run_sh": "echo hi"});
    assert_eq!(
        world
            .call(Method::POST, &base, Some(no_shebang), &session)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );

    let body = json!({"title": "Disk cleanup", "folder": "disk-cleanup", "run_sh": "#!/bin/sh\necho one\n", "check_sh": "#!/bin/sh\ntrue\n"});
    let (status, created) = world
        .call(Method::POST, &base, Some(body.clone()), &session)
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["has_check"], true);
    assert_eq!(
        world
            .call(Method::POST, &base, Some(body), &session)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let run_sh = world.root().join("disk-cleanup/run.sh");
    let mode = std::fs::metadata(&run_sh).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o750);

    let (_, list) = world.call(Method::GET, &base, None, &session).await;
    assert_eq!(list["operations"][0]["title"], "Disk cleanup");
    assert_eq!(list["operations"][0]["has_check"], true);

    let path = format!("{base}/disk-cleanup");
    let (status, scripts) = world.call(Method::GET, &path, None, &session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(scripts["run_sh"], "#!/bin/sh\necho one\n");
    assert_eq!(scripts["check_sh"], "#!/bin/sh\ntrue\n");
    assert_eq!(
        world
            .call(Method::GET, &format!("{base}/missing"), None, &session)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    let edit = json!({"title": "Clean disks", "run_sh": "#!/bin/sh\necho one\necho two\n", "check_sh": ""});
    let (status, result) = world.call(Method::PUT, &path, Some(edit), &session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        result["changes"][0],
        json!({"file": "run.sh", "summary": "+1 −0 lines"})
    );
    assert_eq!(result["changes"][1]["file"], "check.sh");
    assert!(!world.root().join("disk-cleanup/check.sh").exists());
    assert_eq!(
        std::fs::read_to_string(&run_sh).unwrap(),
        "#!/bin/sh\necho one\necho two\n"
    );
    let edits: Vec<Value> = world
        .activity(&session)
        .await
        .into_iter()
        .filter(|entry| entry["action"] == "script_edit")
        .map(|entry| entry["detail"].clone())
        .collect();
    assert!(
        edits.contains(&json!("disk-cleanup/run.sh · +1 −0 lines")),
        "{edits:?}"
    );
    let renamed = world
        .activity(&session)
        .await
        .into_iter()
        .find(|entry| entry["action"] == "operation_edit")
        .unwrap();
    assert_eq!(
        renamed["detail"],
        "disk-cleanup: name Disk cleanup → Clean disks"
    );

    assert_eq!(
        world.call(Method::DELETE, &path, None, &session).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(!world.root().join("disk-cleanup").exists());
    assert_eq!(
        world.call(Method::DELETE, &path, None, &session).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_run_streams_steps_and_output_then_runs_check_sh() {
    let world = World::new().await;
    let session = world.sign_in().await;
    let id = world.add_server(&session).await;
    let run_sh =
        "#!/bin/sh\necho '::step Stop apps'\necho stopped\necho '::step Install'\necho done\n";
    world
        .create_operation(
            &session,
            id,
            "os-update",
            run_sh,
            "#!/bin/sh\necho healthy\n",
        )
        .await;

    let run_id = world.start_run(&session, id, "os-update").await;
    let events = world
        .request(
            Method::GET,
            &format!("/api/runs/{run_id}/events"),
            None,
            Some(&session),
        )
        .await;
    assert_eq!(events.status(), StatusCode::OK);
    assert_eq!(events.headers()["content-type"], "text/event-stream");
    let run = world.finished_run(&session, run_id).await;
    assert_eq!(run["status"], "succeeded");
    assert_eq!(
        step_titles(&run),
        [
            ("Connect over SSH".into(), "ok".into()),
            ("Stop apps".into(), "ok".into()),
            ("Install".into(), "ok".into()),
            ("check.sh".into(), "ok".into()),
        ]
    );
    let text = events.text().await.unwrap();
    assert!(
        text.contains("event: run") && text.contains("event: output"),
        "{text}"
    );
    assert!(text.contains("healthy"), "{text}");

    let output = world.output(&session, run_id).await;
    assert!(
        output.starts_with("$ sudo -n '/.upkeep/os-update/run.sh'\n::step Stop apps\nstopped\n"),
        "{output}"
    );
    let path = format!("/api/runs/{run_id}/output?offset=0&limit=10");
    let (_, first) = world.call(Method::GET, &path, None, &session).await;
    assert_eq!(first["start"], 0);
    assert!(first["end"].as_u64().unwrap() < first["size"].as_u64().unwrap());
    let log = world.dir.path().join(format!("data/runs/{run_id}.log"));
    assert_eq!(std::fs::read_to_string(log).unwrap(), output);
}

#[tokio::test]
async fn progress_output_keeps_only_the_final_redraw() {
    let world = World::new().await;
    let session = world.sign_in().await;
    let id = world.add_server(&session).await;
    let run_sh = concat!(
        "#!/bin/bash\n",
        "printf '\\033[32mstart\\033[0m\\n'\n",
        "for i in 1 2 3; do printf '\\r\\033[2K[0:0%s] %s files\\n\\r\\033[2K/file-%s\\r\\033[1A' $i $i $i; done\n",
        "printf '\\r\\033[2K\\n\\r\\033[2K\\r\\033[1Asaved\\n'\n",
        "printf '\\r10%%\\r50%%\\r100%%\\n'\n",
    );
    world
        .create_operation(&session, id, "backup", run_sh, "#!/bin/sh\ntrue\n")
        .await;
    let run_id = world.start_run(&session, id, "backup").await;
    let events = world
        .request(
            Method::GET,
            &format!("/api/runs/{run_id}/events"),
            None,
            Some(&session),
        )
        .await;
    assert_eq!(
        world.finished_run(&session, run_id).await["status"],
        "succeeded"
    );

    let output = world.output(&session, run_id).await;
    assert!(
        output.contains("/run.sh'\nstart\nsaved\n100%\n$ sudo"),
        "{output:?}"
    );
    let text = events.text().await.unwrap();
    assert!(text.contains(r#""live":[]"#), "{text}");
}

#[tokio::test]
async fn failing_scripts_restarts_busy_servers_and_stops() {
    let world = World::new().await;
    let session = world.sign_in().await;
    let id = world.add_server(&session).await;
    world
        .create_operation(
            &session,
            id,
            "check-fails",
            "#!/bin/sh\necho ok\n",
            "#!/bin/sh\nexit 3\n",
        )
        .await;
    world
        .create_operation(
            &session,
            id,
            "run-fails",
            "#!/bin/sh\nexit 2\n",
            "#!/bin/sh\ntrue\n",
        )
        .await;
    world
        .create_operation(
            &session,
            id,
            "restart",
            "#!/bin/sh\necho kernel\nexit 10\n",
            "#!/bin/sh\necho back\n",
        )
        .await;
    world
        .create_operation(
            &session,
            id,
            "long",
            "#!/bin/sh\necho started\nsleep 30\necho never\n",
            "",
        )
        .await;

    let run = world
        .finished_run(&session, world.start_run(&session, id, "check-fails").await)
        .await;
    assert_eq!(
        (run["status"].clone(), run["exit_code"].clone()),
        (json!("check_failed"), json!(3))
    );
    assert_eq!(
        step_titles(&run).last().unwrap(),
        &("check.sh".to_owned(), "failed".to_owned())
    );

    let run = world
        .finished_run(&session, world.start_run(&session, id, "run-fails").await)
        .await;
    assert_eq!(
        (run["status"].clone(), run["exit_code"].clone()),
        (json!("failed"), json!(2))
    );
    assert_eq!(
        step_titles(&run)[2],
        ("check.sh".to_owned(), "waiting".to_owned())
    );

    let run_id = world.start_run(&session, id, "restart").await;
    let run = world.finished_run(&session, run_id).await;
    assert_eq!(run["status"], "succeeded", "{run}");
    let titles: Vec<String> = step_titles(&run)
        .into_iter()
        .map(|(title, _)| title)
        .collect();
    assert_eq!(
        titles,
        ["Connect over SSH", "run.sh", "Restart and wait", "check.sh"]
    );
    let output = world.output(&session, run_id).await;
    assert!(
        output.contains("the server is back\n") && output.contains("back\n"),
        "{output}"
    );

    let long = world.start_run(&session, id, "long").await;
    let path = format!("/api/servers/{id}/operations/check-fails/runs");
    let (status, busy) = world.call(Method::POST, &path, None, &session).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        busy["error"]
            .as_str()
            .unwrap()
            .contains(&format!("long (run #{long})")),
        "{busy}"
    );
    let path = format!("/api/servers/{id}/operations/long");
    assert_eq!(
        world.call(Method::DELETE, &path, None, &session).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        world
            .call(
                Method::DELETE,
                &format!("/api/servers/{id}"),
                None,
                &session
            )
            .await
            .0,
        StatusCode::CONFLICT
    );

    while !world.output(&session, long).await.contains("started") {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stop = format!("/api/runs/{long}/stop");
    assert_eq!(
        world.call(Method::POST, &stop, None, &session).await.0,
        StatusCode::ACCEPTED
    );
    let run = world.finished_run(&session, long).await;
    assert_eq!(run["status"], "stopped");
    assert_eq!(run["detail"], "stopped by owner@example.com");
    assert_eq!(
        world.call(Method::POST, &stop, None, &session).await.0,
        StatusCode::CONFLICT
    );
    assert!(!world.output(&session, long).await.contains("never"));
}

#[tokio::test]
async fn history_pages_filters_exports_and_activity() {
    let world = World::new().await;
    let session = world.sign_in().await;
    let id = world.add_server(&session).await;
    for index in 0..60 {
        let run = NewRun {
            server_id: id,
            server_name: "Replica",
            operation: if index % 2 == 0 {
                "os-update"
            } else {
                "disk-cleanup"
            },
            title: "Demo",
            user: "owner@example.com",
            steps: &[],
            now: 1_000 + index,
        };
        let run_id = world.db.start_run(&run).unwrap().unwrap();
        world
            .db
            .finish_run(run_id, RunStatus::Succeeded, Some(0), None, 2_000)
            .unwrap();
    }

    let (status, first) = world.call(Method::GET, "/api/runs", None, &session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["items"].as_array().unwrap().len(), 50);
    let next = first["next"].as_str().unwrap();
    let (_, second) = world
        .call(
            Method::GET,
            &format!("/api/runs?before={next}"),
            None,
            &session,
        )
        .await;
    assert_eq!(second["items"].as_array().unwrap().len(), 10);
    assert!(second["next"].is_null());

    let (_, filtered) = world
        .call(Method::GET, &format!("/api/runs?server={id}&operation=os-update&result=succeeded&user=owner%40example.com&since=1000&q=os"), None, &session)
        .await;
    assert_eq!(filtered["items"].as_array().unwrap().len(), 30);
    assert_eq!(
        world
            .call(Method::GET, "/api/runs?result=nope", None, &session)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        world
            .call(Method::GET, "/api/runs?before=x", None, &session)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        world
            .call(Method::GET, "/api/runs/999", None, &session)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    let export = world
        .request(
            Method::GET,
            "/api/runs/export.csv?operation=os-update",
            None,
            Some(&session),
        )
        .await;
    assert_eq!(export.status(), StatusCode::OK);
    assert!(
        export.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/csv")
    );
    let csv = export.text().await.unwrap();
    assert_eq!(csv.lines().count(), 31);
    assert!(csv.starts_with("run,server,operation,result,user,started_at,finished_at,exit_code\n"));

    let (status, filters) = world
        .call(Method::GET, "/api/filters", None, &session)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(filters["servers"][0]["name"], "Replica");
    assert_eq!(filters["users"], json!(["owner@example.com"]));

    let (status, overview) = world
        .call(Method::GET, "/api/overview", None, &session)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (overview["servers"].clone(), overview["running"].clone()),
        (json!(1), json!(0))
    );

    let (status, activity) = world
        .call(
            Method::GET,
            "/api/activity?action=server_add&user=owner%40example.com&server=Replica",
            None,
            &session,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(activity["items"][0]["detail"], "ubuntu@10.0.0.1");
    assert_eq!(
        world
            .call(Method::GET, "/api/activity?action=nope", None, &session)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );

    let changes = world
        .request(Method::GET, "/api/events", None, Some(&session))
        .await;
    assert_eq!(changes.status(), StatusCode::OK);
    assert_eq!(changes.headers()["content-type"], "text/event-stream");
}

#[tokio::test]
async fn runs_left_running_are_interrupted_when_the_service_starts() {
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig::new(dir.path().to_path_buf());
    let db = config.open_db().unwrap();
    let run = NewRun {
        server_id: 1,
        server_name: "Replica",
        operation: "os-update",
        title: "OS update",
        user: "owner@example.com",
        steps: &[],
        now: 1,
    };
    let id = db.start_run(&run).unwrap().unwrap();
    let _service = Service::bind(config, 0).await.unwrap();
    assert_eq!(db.run(id).unwrap().unwrap().status, RunStatus::Interrupted);
}
