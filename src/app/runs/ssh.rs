use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::app::AppConfig;
use crate::app::store::{Server, ServerFields};

/// What `ssh` exits with when it cannot connect or loses the connection.
const SSH_FAILED: i32 = 255;
/// The most stdout or stderr kept from a short command.
const MAX_OUTPUT: usize = 4 * 1024 * 1024;
/// How long a shared connection stays open after its last command.
const CONNECTION_IDLE: &str = "ControlPersist=5m";
/// A Unix socket path holds 107 bytes, and `ssh` adds 17 while it creates the socket.
const MAX_SOCKET_PATH: usize = 90;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("private key {0} must not be readable by group or others (chmod 600)")]
    KeyOpen(String),
    #[error("cannot read private key {path}: {source}")]
    KeyRead {
        path: String,
        source: std::io::Error,
    },
    #[error("cannot start ssh: {0}")]
    Spawn(std::io::Error),
    #[error("cannot connect over SSH: {0}")]
    Connect(String),
    #[error("the server did not answer within {} s", .0.as_secs())]
    Timeout(Duration),
}

/// One server as `ssh` reaches it.
#[derive(Clone, Debug)]
pub struct Ssh {
    program: PathBuf,
    known_hosts: PathBuf,
    target: ServerFields,
    /// The socket of the connection that commands share; `None` opens one per command.
    socket: Option<PathBuf>,
}

/// The result of a short remote command.
#[derive(Debug)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Ssh {
    /// Opens one connection per command, such as for settings that are not saved yet.
    pub fn new(config: &AppConfig, target: &ServerFields) -> Self {
        Self {
            program: config.ssh_program.clone(),
            known_hosts: config.known_hosts(),
            target: target.clone(),
            socket: None,
        }
    }

    /// Commands to a saved server share one connection, which stays open for a while after the
    /// last one. Other settings get another socket, so a changed server never uses an old
    /// connection. A data folder with a long path opens one connection per command.
    pub fn shared(config: &AppConfig, server: &Server) -> Self {
        let socket = config
            .connections_dir()
            .join(socket_name(server.id, &server.fields()));
        let fits = socket.as_os_str().len() <= MAX_SOCKET_PATH;
        Self {
            socket: fits.then_some(socket),
            ..Self::new(config, &server.fields())
        }
    }

    /// The same server with one connection per command, such as while it restarts.
    pub fn unshared(&self) -> Self {
        Self {
            socket: None,
            ..self.clone()
        }
    }

    /// Ends the shared connection. A restart request leaves it open while the server goes down.
    pub async fn close(&self) {
        if let Some(socket) = &self.socket {
            close_socket(&self.program, socket).await;
        }
    }

    /// `ssh` with fixed options, the server, and one remote command. With `tty`, the remote side
    /// gets a terminal, so closing the connection hangs up the remote command.
    pub fn command(&self, remote: &str, tty: bool) -> Result<Command, SshError> {
        check_key(Path::new(&self.target.key_path))?;
        let mut command = Command::new(&self.program);
        command
            .arg(if tty { "-tt" } else { "-T" })
            .arg("-i")
            .arg(&self.target.key_path)
            .args(["-p", &self.target.port.to_string()])
            .args(["-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes"])
            .args(["-o", "StrictHostKeyChecking=accept-new"])
            .arg("-o")
            .arg(format!("UserKnownHostsFile={}", self.known_hosts.display()))
            .args(["-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=15"])
            .args(["-o", "LogLevel=ERROR", "-l", &self.target.ssh_user]);
        match &self.socket {
            Some(socket) => command
                .args(["-o", "ControlMaster=auto", "-o", CONNECTION_IDLE, "-o"])
                .arg(format!("ControlPath={}", socket.display())),
            None => command.args(["-o", "ControlPath=none"]),
        };
        command
            .arg("--")
            .arg(&self.target.address)
            .arg(remote)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        Ok(command)
    }

    /// Runs a short remote command, with `input` on its stdin. A lost connection is an error.
    pub async fn run(
        &self,
        remote: &str,
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Output, SshError> {
        let mut child = self
            .command(remote, false)?
            .spawn()
            .map_err(SshError::Spawn)?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let input = input.unwrap_or_default().to_vec();
        let writer = async move {
            // A remote command that does not read its input closes the pipe early.
            let _ = stdin.write_all(&input).await;
        };
        let (_, output) = tokio::time::timeout(timeout, async {
            tokio::join!(writer, child.wait_with_output())
        })
        .await
        .map_err(|_| SshError::Timeout(timeout))?;
        let output = output.map_err(SshError::Spawn)?;
        let text = |bytes: &[u8]| {
            String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_OUTPUT)]).into_owned()
        };
        let output = Output {
            code: output.status.code(),
            stdout: text(&output.stdout),
            stderr: text(&output.stderr),
        };
        match output.code {
            Some(SSH_FAILED) | None => Err(SshError::Connect(output.error_line())),
            Some(_) => Ok(output),
        }
    }
}

impl Output {
    /// The last line of stderr, which says what went wrong.
    pub fn error_line(&self) -> String {
        self.stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("ssh ended without a message")
            .trim()
            .to_owned()
    }
}

/// `<server id>-<hash of the connection settings>`, short enough for a socket path.
fn socket_name(server_id: i64, target: &ServerFields) -> String {
    let settings = format!(
        "{}\0{}\0{}\0{}",
        target.address, target.port, target.ssh_user, target.key_path
    );
    let hash = Sha256::digest(settings.as_bytes());
    let hex: String = hash[..5].iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{server_id}-{hex}")
}

/// Ends every shared connection of a server, such as when it is deleted or its settings change.
pub async fn close_connections(config: &AppConfig, server_id: i64) {
    let prefix = format!("{server_id}-");
    let Ok(entries) = std::fs::read_dir(config.connections_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            close_socket(&config.ssh_program, &entry.path()).await;
        }
    }
}

/// Asks the connection on `socket` to end. When none answers, the socket is left over from a
/// connection that died, and it is removed.
async fn close_socket(program: &Path, socket: &Path) {
    let exit = Command::new(program)
        .arg("-o")
        .arg(format!("ControlPath={}", socket.display()))
        // `-O` talks to the local socket only; the host name is not used.
        .args(["-O", "exit", "--", "upkeep"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    let ended = matches!(
        tokio::time::timeout(CLOSE_TIMEOUT, exit).await,
        Ok(Ok(status)) if status.success()
    );
    if ended {
        return;
    }
    match std::fs::remove_file(socket) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            tracing::warn!(socket = %socket.display(), %error, "cannot remove an SSH socket");
        }
        _ => {}
    }
}

/// Refuses a private key that group or others can read.
pub fn check_key(path: &Path) -> Result<(), SshError> {
    let metadata = std::fs::metadata(path).map_err(|source| SshError::KeyRead {
        path: path.display().to_string(),
        source,
    })?;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(SshError::KeyOpen(path.display().to_string()));
    }
    Ok(())
}

/// Quotes one argument for the remote POSIX shell.
pub fn quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_keeps_any_text_as_one_argument() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("$(reboot); `x` \"y\""), "'$(reboot); `x` \"y\"'");
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", quote("a'b $HOME;\n`id`")))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "a'b $HOME;\n`id`"
        );
    }

    #[test]
    fn keys_readable_by_others_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id_ed25519");
        std::fs::write(&key, "key").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(check_key(&key), Err(SshError::KeyOpen(_))));
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_key(&key).is_ok());
        assert!(matches!(
            check_key(&dir.path().join("missing")),
            Err(SshError::KeyRead { .. })
        ));
    }

    #[test]
    fn the_command_passes_fixed_options_and_one_remote_argument() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key");
        std::fs::write(&key, "key").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = AppConfig::new(dir.path().into());
        let target = ServerFields {
            name: "Replica".into(),
            address: "10.0.0.1".into(),
            port: 2222,
            ssh_user: "ubuntu".into(),
            key_path: key.display().to_string(),
        };
        let command = Ssh::new(&config, &target)
            .command("sudo -n true", true)
            .unwrap();
        let arguments: Vec<String> = command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        let joined = arguments.join(" ");
        for expected in [
            "-tt -i",
            "-p 2222",
            "-o BatchMode=yes",
            "-o IdentitiesOnly=yes",
            "-o StrictHostKeyChecking=accept-new",
            "-o ConnectTimeout=10",
            "-o ServerAliveInterval=15",
            "-l ubuntu -o ControlPath=none -- 10.0.0.1 sudo -n true",
        ] {
            assert!(joined.contains(expected), "{expected} in {joined}");
        }
        assert!(joined.contains(&format!(
            "UserKnownHostsFile={}",
            config.known_hosts().display()
        )));
        assert_eq!(arguments.last().unwrap(), "sudo -n true");
    }

    fn arguments(ssh: &Ssh) -> String {
        let command = ssh.command("true", false).unwrap();
        let arguments: Vec<String> = command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        arguments.join(" ")
    }

    #[test]
    fn saved_servers_share_one_connection_per_settings() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key");
        std::fs::write(&key, "key").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = AppConfig::new(dir.path().join("data"));
        let server = Server {
            id: 7,
            name: "Replica".into(),
            address: "10.0.0.1".into(),
            port: 22,
            ssh_user: "ubuntu".into(),
            key_path: key.display().to_string(),
            reachable: None,
            created_at: 0,
        };
        let shared = arguments(&Ssh::shared(&config, &server));
        let socket = config
            .connections_dir()
            .join(socket_name(7, &server.fields()));
        for expected in [
            "-o ControlMaster=auto".to_owned(),
            "-o ControlPersist=5m".to_owned(),
            format!("-o ControlPath={}", socket.display()),
        ] {
            assert!(shared.contains(&expected), "{expected} in {shared}");
        }
        assert!(
            socket
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("7-")
        );

        let moved = Server {
            port: 2222,
            ..server.clone()
        };
        assert_ne!(
            socket_name(7, &moved.fields()),
            socket_name(7, &server.fields()),
            "changed settings never reuse a connection"
        );
        assert!(arguments(&Ssh::shared(&config, &server).unshared()).contains("ControlPath=none"));

        let deep = AppConfig::new(dir.path().join("d".repeat(MAX_SOCKET_PATH)));
        assert!(arguments(&Ssh::shared(&deep, &server)).contains("ControlPath=none"));
    }

    #[tokio::test]
    async fn closing_a_server_removes_only_its_dead_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AppConfig::new(dir.path().into());
        config.ssh_program = "false".into();
        let sockets = config.connections_dir();
        std::fs::create_dir_all(&sockets).unwrap();
        for name in ["3-aaaaaaaaaa", "3-bbbbbbbbbb", "33-aaaaaaaaaa"] {
            std::fs::write(sockets.join(name), "").unwrap();
        }
        close_connections(&config, 3).await;
        let left: Vec<String> = std::fs::read_dir(&sockets)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["33-aaaaaaaaaa"]);
    }
}
