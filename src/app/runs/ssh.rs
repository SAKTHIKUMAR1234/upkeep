use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::app::AppConfig;
use crate::app::store::ServerFields;

/// What `ssh` exits with when it cannot connect or loses the connection.
const SSH_FAILED: i32 = 255;
/// The most stdout or stderr kept from a short command.
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

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
}

/// The result of a short remote command.
#[derive(Debug)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Ssh {
    pub fn new(config: &AppConfig, target: &ServerFields) -> Self {
        Self {
            program: config.ssh_program.clone(),
            known_hosts: config.known_hosts(),
            target: target.clone(),
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
            .args(["-o", "LogLevel=ERROR", "-l", &self.target.ssh_user])
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
            "-l ubuntu -- 10.0.0.1 sudo -n true",
        ] {
            assert!(joined.contains(expected), "{expected} in {joined}");
        }
        assert!(joined.contains(&format!(
            "UserKnownHostsFile={}",
            config.known_hosts().display()
        )));
        assert_eq!(arguments.last().unwrap(), "sudo -n true");
    }
}
