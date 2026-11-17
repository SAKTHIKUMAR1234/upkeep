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
fn app_users_are_added_with_a_typed_password_and_managed() {
    let data = tempfile::tempdir().unwrap();
    let user = |args: &[&str], input: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_upkeep"))
            .args(["user", "--data-dir"])
            .arg(data.path())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), input.as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        (output.status.success(), text)
    };
    let good = "correct horse battery\ncorrect horse battery\n";

    let (added, text) = user(&["add", "Owner@Example.com"], good);
    assert!(added, "{text}");
    assert!(
        !text.contains("correct horse"),
        "the password is never printed: {text}"
    );
    let (again, text) = user(&["add", "owner@example.com"], good);
    assert!(!again && text.contains("exists already"), "{text}");
    let (short, text) = user(&["add", "b@example.com"], "short\nshort\n");
    assert!(!short && text.contains("at least 12"), "{text}");
    let (mismatch, text) = user(
        &["add", "b@example.com"],
        "correct horse battery\nother horse battery\n",
    );
    assert!(!mismatch && text.contains("do not match"), "{text}");
    assert!(!user(&["add", "not-an-email"], good).0);

    assert!(
        user(
            &["passwd", "OWNER@example.com"],
            "new horse battery\nnew horse battery\n"
        )
        .0
    );
    assert!(!user(&["passwd", "nobody@example.com"], good).0);
    assert!(user(&["disable", "owner@example.com"], "").0);
    let (_, listing) = user(&["list"], "");
    assert!(
        listing.contains("disabled") && listing.contains("owner@example.com"),
        "{listing}"
    );
    assert!(user(&["enable", "owner@example.com"], "").0);
    let (_, listing) = user(&["list"], "");
    assert!(listing.starts_with("enabled"), "{listing}");
    assert!(data.path().join("upkeep-app.db").exists());
}
