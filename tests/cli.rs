use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn a_closed_output_pipe_ends_the_client_without_a_panic() {
    let client_dir = tempfile::tempdir().unwrap();
    let token_file = client_dir.path().join("runner.token");
    std::fs::write(&token_file, "secret-token\n").unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let upkeep = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_upkeep"));
        command.arg("--client-dir").arg(client_dir.path());
        command
    };
    let added = upkeep()
        .args([
            "host",
            "add",
            "vm-a",
            "--url",
            "http://127.0.0.1:7878",
            "--token-file",
        ])
        .arg(&token_file)
        .output()
        .unwrap();
    assert!(added.status.success(), "{added:?}");

    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let mut child = upkeep()
        .args(["host", "list"])
        .stdout(writer)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_ne!(status.code(), Some(101), "{stderr}");
}

#[test]
fn a_missing_restic_is_named_in_the_error() {
    let client_dir = tempfile::tempdir().unwrap();
    let key = client_dir.path().join("dest_key");
    std::fs::write(&key, "not a real key\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let upkeep = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_upkeep"))
            .arg("--client-dir")
            .arg(client_dir.path())
            .env("UPKEEP_RESTIC", "/nonexistent/restic")
            .args(args)
            .output()
            .unwrap()
    };
    let added = upkeep(&[
        "dest",
        "add",
        "laptop",
        "--ssh",
        "backup@127.0.0.1",
        "--path",
        "/srv/backups",
        "--key",
        key.to_str().unwrap(),
        "--host-key",
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
    ]);
    assert!(added.status.success(), "{added:?}");

    let listed = upkeep(&["snapshots", "vm-a", "--dest", "laptop"]);
    let stderr = String::from_utf8_lossy(&listed.stderr);
    assert!(!listed.status.success());
    assert!(
        stderr.contains("cannot run restic at /nonexistent/restic"),
        "{stderr}"
    );
}

#[test]
fn web_users_are_added_listed_and_removed() {
    let root = tempfile::tempdir().unwrap();
    let settings = root.path().join("web.toml");
    std::fs::write(
        &settings,
        "listen = \"127.0.0.1:7880\"\npublic_url = \"https://upkeep.example.com\"\n\
         google_client_id = \"client-id\"\ngoogle_client_secret_file = \"/missing\"\n\
         [hosts.replica]\nlabel = \"Replica\"\nurl = \"http://127.0.0.1:7878\"\ntoken_file = \"/missing\"\n",
    )
    .unwrap();
    let web = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_upkeep"))
            .arg("--state-dir")
            .arg(root.path())
            .args(["web", "--settings"])
            .arg(&settings)
            .args(args)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        (output.status.success(), text)
    };

    let (added, text) = web(&["user", "add", "Owner@Example.com", "--role", "admin"]);
    assert!(added, "{text}");
    assert!(!web(&["user", "add", "x@example.com", "--role", "root"]).0);
    assert!(!web(&["user", "add", "not-an-email", "--role", "viewer"]).0);
    let (_, listing) = web(&["user", "list"]);
    assert!(
        listing.contains("admin") && listing.contains("owner@example.com"),
        "{listing}"
    );
    assert!(root.path().join("web.db").exists());

    assert!(web(&["user", "remove", "owner@example.com"]).0);
    let (removed_again, text) = web(&["user", "remove", "owner@example.com"]);
    assert!(!removed_again && text.contains("no user"), "{text}");
}
