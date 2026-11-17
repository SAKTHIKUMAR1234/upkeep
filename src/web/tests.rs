use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::Response;
use serde_json::{Value, json};

use super::oauth::Login;
use super::oauth::tests::{FakeGoogle, settings_text};
use super::settings::WebSettings;
use super::users::{Role, WebDb};
use super::{WebState, routes};

/// A runner that records the run requests it gets and answers 409 when `busy`.
#[derive(Clone, Default)]
struct FakeRunner {
    runs: Arc<Mutex<Vec<Value>>>,
    busy: Arc<Mutex<bool>>,
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some("Bearer runner-token")
}

async fn run(
    State(runner): State<FakeRunner>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    assert!(authorized(&headers));
    if *runner.busy.lock().unwrap() {
        let error = json!({"error": "another job is already active"});
        return (StatusCode::CONFLICT, Json(error));
    }
    runner.runs.lock().unwrap().push(body);
    (StatusCode::CREATED, Json(json!({"id": 7})))
}

async fn start_runner() -> (FakeRunner, String) {
    let runner = FakeRunner::default();
    let monitor =
        json!({"display_name": "Replica", "enabled": true, "checks": [], "updates": null});
    let router = Router::new()
        .route("/run", post(run))
        .route("/monitor", get(move || async move { Json(monitor) }))
        .route(
            "/jobs",
            get(|| async { Json(json!({"jobs": [{"id": 6, "target": "os-update"}]})) }),
        )
        .route(
            "/plans",
            get(|| async { Json(json!({"plans": [{"name": "os-update"}]})) }),
        )
        .with_state(runner.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    (runner, url)
}

/// The web page in this process, with a fake Google and a fake runner.
struct World {
    base: String,
    http: reqwest::Client,
    google: FakeGoogle,
    runner: FakeRunner,
    db: Arc<WebDb>,
    _dir: tempfile::TempDir,
}

fn owner_only(path: &std::path::Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

impl World {
    async fn start() -> Self {
        let (google, google_address) = FakeGoogle::start().await;
        let (runner, runner_url) = start_runner().await;
        let dir = tempfile::tempdir().unwrap();
        let (secret, token) = (dir.path().join("secret"), dir.path().join("token"));
        owner_only(&secret, "google-secret\n");
        owner_only(&token, "runner-token\n");
        let text = settings_text(google_address, &secret)
            + &format!(
                "[hosts.replica]\nlabel = \"Replica server\"\nurl = \"{runner_url}\"\ntoken_file = \"{}\"\n",
                token.display()
            );
        std::fs::write(dir.path().join("web.toml"), text).unwrap();
        let settings = WebSettings::load(&dir.path().join("web.toml")).unwrap();
        let state = WebState::new(&settings, dir.path()).unwrap();
        let db = state.db.clone();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, routes::router(state)).await });
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Self {
            base,
            http,
            google,
            runner,
            db,
            _dir: dir,
        }
    }

    async fn get(&self, path: &str, cookie: &str) -> Response {
        let request = self.http.get(format!("{}{path}", self.base));
        request.header("cookie", cookie).send().await.unwrap()
    }

    async fn send(&self, method: &str, path: &str, user: &User, body: Value) -> Response {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        request = request.header("cookie", &user.cookie).json(&body);
        if let Some(csrf) = &user.csrf {
            request = request.header("x-csrf-token", csrf);
        }
        request.send().await.unwrap()
    }

    /// Goes to Google and back. Returns the callback response and the login cookie.
    async fn callback(&self) -> (Response, String) {
        let login = self.get("/auth/login", "").await;
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        let location = reqwest::Url::parse(login.headers()["location"].to_str().unwrap()).unwrap();
        let query: HashMap<_, _> = location.query_pairs().into_owned().collect();
        self.google.expect(&Login {
            state: query["state"].clone(),
            nonce: query["nonce"].clone(),
            challenge: query["code_challenge"].clone(),
        });
        let cookie = set_cookie(&login, "__Host-upkeep-login");
        let path = format!("/auth/callback?code=good-code&state={}", query["state"]);
        (self.get(&path, &cookie).await, cookie)
    }

    async fn sign_in(&self, email: &str) -> User {
        *self.google.email.lock().unwrap() = (email.to_owned(), true);
        let (response, _) = self.callback().await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = set_cookie(&response, "__Host-upkeep");
        let me: Value = self.get("/api/me", &cookie).await.json().await.unwrap();
        User {
            cookie,
            csrf: me["csrf"].as_str().map(str::to_owned),
        }
    }
}

struct User {
    cookie: String,
    csrf: Option<String>,
}

/// `name=value` of the response's `Set-Cookie` for `name`.
fn set_cookie(response: &Response, name: &str) -> String {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap())
        .find(|value| value.starts_with(&format!("{name}=")))
        .and_then(|value| value.split(';').next())
        .unwrap()
        .to_owned()
}

async fn error(response: Response) -> (StatusCode, String) {
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    (
        status,
        body["error"].as_str().unwrap_or_default().to_owned(),
    )
}

#[tokio::test]
async fn pages_carry_security_headers_and_api_needs_a_session() {
    let world = World::start().await;
    let page = world.get("/", "").await;
    assert_eq!(page.status(), StatusCode::OK);
    let headers = page.headers();
    assert_eq!(
        headers["content-security-policy"],
        "default-src 'self'; frame-ancestors 'none'"
    );
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["referrer-policy"], "same-origin");

    let me = world.get("/api/me", "__Host-upkeep=made-up").await;
    assert_eq!(me.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(me.headers()["referrer-policy"], "same-origin");
}

#[tokio::test]
async fn operators_start_plans_with_their_email_and_the_csrf_token() {
    let world = World::start().await;
    world
        .db
        .set_user("Op@Example.com", Role::Operator, "cli")
        .unwrap();
    let mut operator = world.sign_in("op@example.com").await;

    let path = "/api/hosts/replica/run";
    let csrf = operator.csrf.take();
    let refused = world
        .send("POST", path, &operator, json!({"plan": "os-update"}))
        .await;
    assert_eq!(error(refused).await.0, StatusCode::FORBIDDEN);
    operator.csrf = Some("wrong".into());
    let refused = world
        .send("POST", path, &operator, json!({"plan": "os-update"}))
        .await;
    assert_eq!(error(refused).await.0, StatusCode::FORBIDDEN);
    assert!(world.runner.runs.lock().unwrap().is_empty());

    operator.csrf = csrf;
    let started = world
        .send("POST", path, &operator, json!({"plan": "os-update"}))
        .await;
    assert_eq!(started.status(), StatusCode::CREATED);
    assert_eq!(
        world.runner.runs.lock().unwrap()[0],
        json!({"target": "os-update", "confirm": true, "requested_by": "op@example.com"})
    );

    let single = world
        .send("POST", path, &operator, json!({"plan": "os:update"}))
        .await;
    assert_eq!(error(single).await.0, StatusCode::BAD_REQUEST);
    *world.runner.busy.lock().unwrap() = true;
    let busy = world
        .send("POST", path, &operator, json!({"plan": "os-update"}))
        .await;
    assert_eq!(
        error(busy).await,
        (
            StatusCode::CONFLICT,
            "An update is already running on this server".into()
        )
    );

    let audit: Vec<String> = world
        .db
        .audit(10)
        .unwrap()
        .into_iter()
        .map(|entry| entry.action)
        .collect();
    assert_eq!(audit[..2], ["run", "run"]);
    assert!(audit.contains(&"sign-in".to_owned()));

    let users = world.get("/api/users", &operator.cookie).await;
    assert_eq!(error(users).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn viewers_see_servers_but_cannot_start_plans() {
    let world = World::start().await;
    world
        .db
        .set_user("viewer@example.com", Role::Viewer, "cli")
        .unwrap();
    let viewer = world.sign_in("viewer@example.com").await;

    let hosts: Value = world
        .get("/api/hosts", &viewer.cookie)
        .await
        .json()
        .await
        .unwrap();
    let card = &hosts["hosts"][0];
    assert_eq!(
        (&card["label"], &card["reachable"]),
        (&json!("Replica server"), &json!(true))
    );
    assert_eq!(card["latest_job"]["id"], 6);
    assert_eq!(card["monitor"]["ok"], 0);
    let plans: Value = world
        .get("/api/hosts/replica/plans", &viewer.cookie)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(plans["plans"][0]["name"], "os-update");
    let unknown = world.get("/api/hosts/nas/plans", &viewer.cookie).await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    let run = world
        .send(
            "POST",
            "/api/hosts/replica/run",
            &viewer,
            json!({"plan": "os-update"}),
        )
        .await;
    assert_eq!(error(run).await.0, StatusCode::FORBIDDEN);

    let logout = world.send("POST", "/api/logout", &viewer, json!({})).await;
    assert_eq!(logout.status(), StatusCode::OK);
    let me = world.get("/api/me", &viewer.cookie).await;
    assert_eq!(me.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn emails_off_the_list_see_not_allowed() {
    let world = World::start().await;
    *world.google.email.lock().unwrap() = ("stranger@example.com".into(), true);
    let (response, _) = world.callback().await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(response.headers().get("set-cookie").is_none());
    assert!(response.text().await.unwrap().contains("Not allowed"));
    let entry = &world.db.audit(1).unwrap()[0];
    assert_eq!(
        (entry.email.as_str(), entry.action.as_str()),
        ("stranger@example.com", "sign-in refused")
    );

    *world.google.email.lock().unwrap() = ("owner@example.com".into(), false);
    world
        .db
        .set_user("owner@example.com", Role::Admin, "cli")
        .unwrap();
    let (unverified, _) = world.callback().await;
    assert_eq!(unverified.status(), StatusCode::BAD_GATEWAY);
    let entry = &world.db.audit(1).unwrap()[0];
    assert_eq!(entry.action, "sign-in refused");
    assert!(entry.detail.contains("not verified"), "{}", entry.detail);
}

#[tokio::test]
async fn a_sign_in_needs_its_browser_and_works_once() {
    let world = World::start().await;
    world
        .db
        .set_user("owner@example.com", Role::Admin, "cli")
        .unwrap();
    let login = world.get("/auth/login", "").await;
    let location = reqwest::Url::parse(login.headers()["location"].to_str().unwrap()).unwrap();
    let state = location
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    let path = format!("/auth/callback?code=good-code&state={state}");

    let other_browser = world.get(&path, "").await;
    assert_eq!(other_browser.status(), StatusCode::BAD_REQUEST);
    let replay = world
        .get(&path, &format!("__Host-upkeep-login={state}"))
        .await;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admins_manage_users_but_not_themselves() {
    let world = World::start().await;
    world
        .db
        .set_user("owner@example.com", Role::Admin, "cli")
        .unwrap();
    let admin = world.sign_in("owner@example.com").await;

    let added = world
        .send(
            "POST",
            "/api/users",
            &admin,
            json!({"email": "Dev@Example.com", "role": "viewer"}),
        )
        .await;
    assert_eq!(added.status(), StatusCode::OK);
    assert_eq!(
        world.db.role("dev@example.com").unwrap(),
        Some(Role::Viewer)
    );

    for body in [
        json!({"email": "owner@example.com", "role": "viewer"}),
        json!({"email": "not-an-email", "role": "viewer"}),
        json!({"email": "x@example.com"}),
    ] {
        let refused = world.send("POST", "/api/users", &admin, body).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    }

    let removed = world
        .send(
            "DELETE",
            "/api/users",
            &admin,
            json!({"email": "dev@example.com"}),
        )
        .await;
    assert_eq!(removed.status(), StatusCode::OK);
    let again = world
        .send(
            "DELETE",
            "/api/users",
            &admin,
            json!({"email": "dev@example.com"}),
        )
        .await;
    assert_eq!(again.status(), StatusCode::NOT_FOUND);

    let audit: Value = world
        .get("/api/audit", &admin.cookie)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(audit["audit"][0]["action"], "user remove");
    assert_eq!(audit["audit"][1]["detail"], "dev@example.com as viewer");
}

#[test]
fn a_runner_refusing_the_page_token_does_not_sign_the_browser_out() {
    use axum::response::IntoResponse;

    use super::WebError;
    use super::runners::RunnerError;
    use super::session::AccessError;

    let status = |error: WebError| error.into_response().status();
    let runner = |status: u16| RunnerError::Status {
        status,
        message: "refused".into(),
    };
    assert_eq!(status(runner(401).into()), StatusCode::BAD_GATEWAY);
    assert_eq!(status(runner(404).into()), StatusCode::NOT_FOUND);
    assert_eq!(
        status(AccessError::SignedOut.into()),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(status(AccessError::Csrf.into()), StatusCode::FORBIDDEN);
}
