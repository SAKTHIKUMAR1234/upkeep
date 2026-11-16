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
