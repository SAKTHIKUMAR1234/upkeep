mod common;

use std::os::unix::fs::PermissionsExt;

use common::Harness;
use upkeep::client::{Client, ClientError, Endpoint};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn write_token(harness: &Harness, mode: u32) {
    let path = harness.config_dir().join("token");
    std::fs::write(&path, "s3cret-token\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn tcp(port: u16, token: Option<&str>) -> Client {
    Client::new(Endpoint::Tcp {
        address: format!("127.0.0.1:{port}"),
        token: token.map(str::to_owned),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_api_denies_requests_without_the_right_token() {
    let mut harness = Harness::new();
    let port = free_port();
    harness.write_settings(&format!("listen = \"127.0.0.1:{port}\"\n"));
    write_token(&harness, 0o600);
    harness.start().await;

    for client in [tcp(port, None), tcp(port, Some("wrong-token"))] {
        let error = client.get("/jobs").await.unwrap_err();
        assert!(
            matches!(error, ClientError::Status { status: 401, .. }),
            "{error:?}"
        );
        let run = client
            .post("/run", &serde_json::json!({"target": "demo-app:verify"}))
            .await;
        assert!(matches!(run, Err(ClientError::Status { status: 401, .. })));
    }
    assert!(tcp(port, Some("s3cret-token")).get("/jobs").await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_refuses_a_token_file_readable_by_others() {
    let harness = Harness::new();
    harness.write_settings(&format!("listen = \"127.0.0.1:{}\"\n", free_port()));
    write_token(&harness, 0o644);

    let output = harness.command("serve").output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must not be readable by group or others")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_manifests_are_reported_not_skipped() {
    let mut harness = Harness::new();
    harness.add_adapter("broken", "writes_data = true\n", "");
    harness.start().await;

    let listing = harness.client().get("/adapters").await.unwrap();
    let names: Vec<&str> = listing["adapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["demo-app", "demo-os"]);
    assert!(
        listing["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("at least one action")
    );

    let health = harness.client().get("/health").await.unwrap();
    assert_eq!(health["ok"], false);
    assert_eq!(health["errors"].as_array().unwrap().len(), 1);

    let run = harness.run("broken:verify", false).await;
    assert!(
        matches!(run, Err(ClientError::Status { status: 422, message }) if message.contains("is invalid"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn health_reports_each_service_and_ping() {
    let mut harness = Harness::new();
    harness.start().await;

    let health = harness.client().get("/health").await.unwrap();
    assert_eq!(health["ok"], true, "{health}");
    assert_eq!(health["items"].as_array().unwrap().len(), 2);

    std::fs::write(harness.adapter_state("demo-app").join("broken"), "").unwrap();
    let health = harness.client().get("/health").await.unwrap();
    assert_eq!(health["ok"], false);
    let ping = &health["items"][0];
    assert_eq!(
        (
            ping["adapter"].as_str(),
            ping["kind"].as_str(),
            ping["ok"].as_bool()
        ),
        (Some("demo-app"), Some("ping"), Some(false))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn second_daemon_on_the_same_state_is_refused() {
    let mut harness = Harness::new();
    harness.start().await;

    let output = harness.command("serve").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("another upkeep serve is running"));
}
