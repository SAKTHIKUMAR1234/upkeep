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

#[tokio::test(flavor = "multi_thread")]
async fn client_drives_a_registered_host_over_tcp() {
    let mut harness = Harness::new();
    let port = free_port();
    harness.write_settings(&format!("listen = \"127.0.0.1:{port}\"\n"));
    write_token(&harness, 0o600);
    harness.start().await;

    let client_dir = tempfile::tempdir().unwrap();
    let upkeep = |args: &[&str]| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_upkeep"))
            .arg("--client-dir")
            .arg(client_dir.path())
            .env("USER", "tester")
            .env_remove("SUDO_USER")
            .args(args)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        (output.status.success(), text)
    };
    let token_file = harness.config_dir().join("token");
    let url = format!("http://127.0.0.1:{port}");

    let (added, text) = upkeep(&[
        "host",
        "add",
        "vm-a",
        "--url",
        &url,
        "--token-file",
        token_file.to_str().unwrap(),
    ]);
    assert!(added, "{text}");
    assert!(upkeep(&["host", "list"]).1.contains(&url));

    let (started, text) = upkeep(&[
        "--host",
        "vm-a",
        "run",
        "demo-app:verify",
        "--param",
        "note=hi",
    ]);
    assert!(started, "{text}");
    let detail = harness.wait_for_status(1, "succeeded").await;
    assert_eq!(detail["job"]["requested_by"], "tester");
    assert_eq!(detail["job"]["params"]["note"], "hi");
    assert!(
        upkeep(&["--host", "vm-a", "jobs"])
            .1
            .contains("demo-app:verify")
    );

    let (_, text) = upkeep(&["--host", "nas", "jobs"]);
    assert!(
        text.contains("no host named \"nas\"; known: vm-a"),
        "{text}"
    );
    assert!(upkeep(&["host", "remove", "vm-a"]).0);
    assert!(!upkeep(&["--host", "vm-a", "jobs"]).0);
}

#[tokio::test(flavor = "multi_thread")]
async fn plans_list_titles_and_steps_in_plain_words() {
    let mut harness = Harness::new();
    harness.add_plan("broken", "steps = [\"nope:verify\"]\n");
    harness.start().await;

    let listing = harness.client().get("/plans").await.unwrap();
    let plans = listing["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0]["name"], "broken");
    assert!(
        plans[0]["error"]
            .as_str()
            .unwrap()
            .contains("unknown adapter")
    );

    let update = &plans[1];
    assert_eq!(
        (&update["name"], &update["title"], &update["confirm"]),
        (&"os-update".into(), &"OS update".into(), &true.into())
    );
    assert_eq!(
        update["steps"],
        serde_json::json!([
            "Maintenance on",
            "Backup",
            "Snapshot",
            "Uploading the snapshot",
            "Install updates",
            "Check services (demo-app)",
            "Check services (Demo server)",
            "Maintenance off"
        ])
    );
    assert_eq!(update["on_failure"][0], "Restore");
}

#[tokio::test(flavor = "multi_thread")]
async fn job_detail_gives_plain_titles() {
    let mut harness = Harness::new();
    harness.start().await;

    let id = harness.run("demo-app:verify", false).await.unwrap();
    let detail = harness.wait_for_status(id, "succeeded").await;
    assert_eq!(detail["title"], "demo-app:verify");
    assert_eq!(detail["steps"][0]["title"], "Check services");
}
