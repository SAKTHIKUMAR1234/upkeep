use std::net::IpAddr;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

use super::tests::{PASSWORD, Session, World};
use crate::app::serve::{ServeConfig, SessionLimits};
use crate::time::now_millis;

const PUBLIC_ORIGIN: &str = "https://upkeep.example.com";

fn website(trusted_proxy: Option<&str>) -> ServeConfig {
    ServeConfig {
        public_origin: Some(PUBLIC_ORIGIN.into()),
        trusted_proxy: trusted_proxy.map(|ip| ip.parse::<IpAddr>().unwrap()),
        ..ServeConfig::loopback(0)
    }
}

fn set_cookies(response: &reqwest::Response) -> Vec<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect()
}

fn token(session: &Session) -> &str {
    session.cookie.split_once('=').unwrap().1
}

impl World {
    async fn sign_in_from(&self, forwarded_for: &str, email: &str) -> reqwest::Response {
        self.http
            .post(format!("{}/api/session", self.base))
            .header("host", "upkeep.example.com")
            .header("x-forwarded-for", forwarded_for)
            .json(&json!({"email": email, "password": "wrong password!"}))
            .send()
            .await
            .unwrap()
    }

    /// A GET the page sends by itself, without user input.
    async fn background_get(&self, path: &str, session: &Session) -> (StatusCode, Value) {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .header("cookie", &session.cookie)
            .header("x-upkeep-background", "1")
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn get_with(&self, headers: &[(&str, &str)]) -> StatusCode {
        let mut request = self.http.get(format!("{}/", self.base));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().await.unwrap().status()
    }
}

#[tokio::test]
async fn a_website_accepts_only_its_public_origin_and_sets_secure_cookies_and_hsts() {
    let world = World::with(website(None)).await;
    let page = world
        .request(Method::GET, "/", None, None)
        .await
        .headers()
        .clone();
    assert_eq!(page["strict-transport-security"], "max-age=31536000");
    assert_eq!(
        page["content-security-policy"],
        "default-src 'self'; frame-ancestors 'none'"
    );

    let response = world
        .http
        .post(format!("{}/api/session", world.base))
        .header("host", "upkeep.example.com")
        .header("origin", PUBLIC_ORIGIN)
        .json(&json!({"email": "owner@example.com", "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookies = set_cookies(&response);
    assert_eq!(cookies.len(), 2);
    assert!(cookies.iter().all(|cookie| cookie.ends_with("; Secure")));
    assert!(cookies[1].starts_with("upkeep_email=owner@example.com;"));

    let loopback = format!("127.0.0.1:{}", world.base.rsplit(':').next().unwrap());
    let loopback_origin = format!("http://{loopback}");
    for headers in [
        vec![("host", loopback.as_str())],
        vec![("host", "localhost")],
        vec![
            ("host", "upkeep.example.com"),
            ("origin", loopback_origin.as_str()),
        ],
        vec![
            ("host", "upkeep.example.com"),
            ("origin", "https://evil.example"),
        ],
        vec![
            ("host", "upkeep.example.com"),
            ("origin", "http://upkeep.example.com"),
        ],
    ] {
        assert_eq!(
            world.get_with(&headers).await,
            StatusCode::FORBIDDEN,
            "{headers:?}"
        );
    }
}

#[tokio::test]
async fn the_desktop_mode_sets_no_secure_cookies_and_no_hsts() {
    let world = World::with(ServeConfig::loopback(0)).await;
    let response = world.sign_in_as("owner@example.com", PASSWORD).await;
    assert!(
        response
            .headers()
            .get("strict-transport-security")
            .is_none()
    );
    let cookies = set_cookies(&response);
    assert_eq!(cookies.len(), 2);
    assert!(cookies.iter().all(|cookie| !cookie.contains("Secure")));
}

#[tokio::test]
async fn ten_failures_from_one_client_address_wait_with_retry_after() {
    let world = World::with(website(Some("127.0.0.1"))).await;
    for attempt in 0..10 {
        let response = world
            .sign_in_from(
                "6.6.6.6, 203.0.113.5",
                &format!("user{attempt}@example.com"),
            )
            .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{attempt}");
    }
    let refused = world
        .sign_in_from("1.1.1.1, 203.0.113.5", "fresh@example.com")
        .await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: i64 = refused.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=900).contains(&retry), "{retry}");

    let other = world.sign_in_from("203.0.113.6", "fresh@example.com").await;
    assert_eq!(other.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn parallel_sign_ins_from_one_client_stop_at_the_limit() {
    let world = &World::with(website(Some("127.0.0.1"))).await;
    let answers = futures_util::future::join_all((0..40).map(|attempt| async move {
        let email = format!("user{attempt}@example.com");
        world.sign_in_from("203.0.113.5", &email).await
    }))
    .await;
    let checked = answers
        .iter()
        .filter(|answer| answer.status() == StatusCode::UNAUTHORIZED)
        .count();
    assert!((1..=10).contains(&checked), "{checked} passwords checked");
    assert!(answers.iter().all(|answer| matches!(
        answer.status(),
        StatusCode::UNAUTHORIZED | StatusCode::TOO_MANY_REQUESTS
    )));
}

#[tokio::test]
async fn addresses_of_one_ipv6_64_network_share_one_limit() {
    let world = World::with(website(Some("127.0.0.1"))).await;
    for host in 1..=10 {
        let address = format!("2001:db8:1:2::{host:x}");
        let response = world
            .sign_in_from(&address, &format!("user{host}@example.com"))
            .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{address}");
    }
    let rotated = world
        .sign_in_from("2001:db8:1:2:ffff::1", "fresh@example.com")
        .await;
    assert_eq!(rotated.status(), StatusCode::TOO_MANY_REQUESTS);
    let mapped = world
        .sign_in_from("::ffff:203.0.113.5", "fresh@example.com")
        .await;
    assert_eq!(mapped.status(), StatusCode::UNAUTHORIZED);
    let other = world
        .sign_in_from("2001:db8:1:3::1", "fresh@example.com")
        .await;
    assert_eq!(other.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn long_or_malformed_emails_are_refused_and_stored_cut() {
    let world = World::with(website(Some("127.0.0.1"))).await;
    let huge = format!("{}@example.com", "a".repeat(100_000));
    for email in [huge.as_str(), "no-at-sign", "two@@example.com"] {
        let response = world.sign_in_from("203.0.113.5", email).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{email:.20}");
    }
    for attempt in 0..7 {
        world
            .sign_in_from("203.0.113.5", &format!("user{attempt}@example.com"))
            .await;
    }
    let throttled = world.sign_in_from("203.0.113.5", &huge).await;
    assert_eq!(throttled.status(), StatusCode::TOO_MANY_REQUESTS);

    let session = world.sign_in().await;
    let failed: Vec<Value> = world
        .activity(&session)
        .await
        .into_iter()
        .filter(|entry| entry["action"] == "failed_sign_in")
        .collect();
    assert_eq!(failed.len(), 10, "no activity row for a throttled request");
    assert!(
        failed
            .iter()
            .all(|entry| entry["user"].as_str().unwrap().len() <= 254)
    );
}

#[tokio::test]
async fn forwarded_headers_from_an_untrusted_peer_are_ignored() {
    let world = World::with(website(Some("10.9.9.9"))).await;
    for attempt in 0..10 {
        let spoofed = format!("203.0.113.{attempt}");
        let response = world
            .sign_in_from(&spoofed, &format!("user{attempt}@example.com"))
            .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{attempt}");
    }
    let refused = world
        .sign_in_from("203.0.113.99", "fresh@example.com")
        .await;
    assert_eq!(
        refused.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "every attempt counted against the peer"
    );
}

#[tokio::test]
async fn a_ping_keeps_the_session_alive_but_an_event_stream_does_not() {
    let world = World::with(ServeConfig::loopback(0)).await;
    let session = world.sign_in().await;
    let earlier = now_millis() - 5 * 60_000;
    world.db.touch_session(token(&session), earlier).unwrap();

    let stream = world
        .request(Method::GET, "/api/events", None, Some(&session))
        .await;
    assert_eq!(stream.status(), StatusCode::OK);
    drop(stream);
    let seen = |world: &World| {
        world
            .db
            .session(token(&session))
            .unwrap()
            .unwrap()
            .last_seen_at
    };
    assert_eq!(seen(&world), earlier, "the stream did not count as use");

    let (status, _) = world
        .call(Method::POST, "/api/session/ping", None, &session)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(seen(&world) > earlier + 4 * 60_000);

    world
        .db
        .touch_session(token(&session), now_millis() - 16 * 60_000)
        .unwrap();
    let (status, body) = world.call(Method::GET, "/api/events", None, &session).await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::UNAUTHORIZED, &json!("locked"))
    );
}

#[tokio::test]
async fn background_requests_do_not_keep_the_session_alive() {
    let world = World::with(ServeConfig::loopback(0)).await;
    let session = world.sign_in().await;
    let earlier = now_millis() - 5 * 60_000;
    world.db.touch_session(token(&session), earlier).unwrap();
    let seen = |world: &World| {
        world
            .db
            .session(token(&session))
            .unwrap()
            .unwrap()
            .last_seen_at
    };
    for path in ["/api/servers", "/api/overview", "/api/runs"] {
        let (status, _) = world.background_get(path, &session).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
    let (_, me) = world.background_get("/api/session", &session).await;
    assert_eq!(seen(&world), earlier, "background requests are not use");
    let left = me["idle_left_ms"].as_i64().unwrap();
    assert!((9 * 60_000..=10 * 60_000).contains(&left), "{left}");

    world
        .call(Method::GET, "/api/servers", None, &session)
        .await;
    assert!(
        seen(&world) > earlier + 4 * 60_000,
        "a plain request is use"
    );
}

#[tokio::test]
async fn idle_time_and_lifetime_come_from_the_config() {
    let world = World::with(ServeConfig {
        sessions: SessionLimits {
            idle_ms: 2 * 60_000,
            absolute_ms: 3_600_000,
        },
        ..ServeConfig::loopback(0)
    })
    .await;
    let response = world.sign_in_as("owner@example.com", PASSWORD).await;
    assert!(set_cookies(&response)[0].contains("Max-Age=3600;"));
    let session = world.sign_in().await;
    let (_, me) = world
        .call(Method::GET, "/api/session", None, &session)
        .await;
    assert_eq!(me["idle_ms"], 120_000);

    world
        .db
        .touch_session(token(&session), now_millis() - 3 * 60_000)
        .unwrap();
    let (status, body) = world
        .call(Method::GET, "/api/servers", None, &session)
        .await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::UNAUTHORIZED, &json!("locked"))
    );

    let (old, csrf) = world
        .db
        .create_session("owner@example.com", now_millis() - 3_600_000)
        .unwrap();
    world.db.touch_session(&old, now_millis()).unwrap();
    let expired = Session {
        cookie: format!("upkeep_session={old}"),
        csrf,
    };
    let (status, body) = world
        .call(Method::GET, "/api/servers", None, &expired)
        .await;
    assert_eq!(
        (status, &body["code"]),
        (StatusCode::UNAUTHORIZED, &json!("signed_out"))
    );
}

#[tokio::test]
async fn lock_keeps_the_email_cookie_and_sign_out_clears_it() {
    let world = World::with(website(None)).await;
    let session = world.sign_in().await;
    let locked = world
        .request(Method::POST, "/api/session/lock", None, Some(&session))
        .await;
    assert_eq!(locked.status(), StatusCode::NO_CONTENT);
    let cookies = set_cookies(&locked);
    assert_eq!(cookies.len(), 1);
    assert!(cookies[0].starts_with("upkeep_session=; Max-Age=0;"));
    let (status, _): (StatusCode, Value) = world
        .call(Method::GET, "/api/servers", None, &session)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let session = world.sign_in().await;
    let signed_out = world
        .request(Method::DELETE, "/api/session", None, Some(&session))
        .await;
    assert_eq!(signed_out.status(), StatusCode::NO_CONTENT);
    let cookies = set_cookies(&signed_out);
    assert!(cookies[0].starts_with("upkeep_session=; Max-Age=0;"));
    assert!(cookies[1].starts_with("upkeep_email=; Max-Age=0;"));
    assert!(cookies.iter().all(|cookie| cookie.ends_with("; Secure")));
}
