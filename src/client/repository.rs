use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::config::{ClientDir, ConfigError, read_secret};
use super::destinations::{Destination, SshAuth};

/// restic, installed by `packaging/install-restic.sh`, or found on PATH.
const INSTALLED_ENGINE: &str = "/usr/local/lib/upkeep/bin/restic";
/// Name in the known-hosts file, so the check does not depend on how the address is written.
const HOST_KEY_ALIAS: &str = "upkeep-destination";

/// Runs restic on this machine against one host's repository on a destination, over SFTP.
pub struct Repository {
    engine: PathBuf,
    url: String,
    ssh_command: String,
    password_file: PathBuf,
    askpass: Option<PathBuf>,
    _session: SessionDir,
}

impl Repository {
    pub fn open(
        dir: &ClientDir,
        name: &str,
        destination: &Destination,
        host: &str,
    ) -> Result<Self, ConfigError> {
        let password_file = dir.repository_key_file(name);
        read_secret(&password_file)?;
        let session = SessionDir::create()?;

        let known_hosts = session.0.join("known_hosts");
        write_session_file(&known_hosts, &known_hosts_text(destination), 0o600)?;
        let askpass = match destination.auth {
            SshAuth::Key => None,
            SshAuth::Password => {
                let password_file = dir.ssh_password_file(name);
                read_secret(&password_file)?;
                let script = session.0.join("askpass");
                write_session_file(&script, &askpass_script(&password_file)?, 0o700)?;
                Some(script)
            }
        };

        Ok(Self {
            engine: engine(),
            url: format!("sftp:{HOST_KEY_ALIAS}:{}", destination.repository(host)),
            ssh_command: shell_join(&ssh_command(destination, &known_hosts)),
            password_file,
            askpass,
            _session: session,
        })
    }

    /// Why restic could not start, naming the binary that was tried.
    pub fn engine_error(&self) -> String {
        format!(
            "cannot run restic at {}: install it or set UPKEEP_RESTIC",
            self.engine.display()
        )
    }

    /// `restic -r <repository> <args>` with the SSH and key settings in place.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.engine);
        command
            .arg("-r")
            .arg(&self.url)
            .arg("-o")
            .arg(format!("sftp.command={}", self.ssh_command))
            .args(args)
            .env("RESTIC_PASSWORD_FILE", &self.password_file)
            .env_remove("RESTIC_PASSWORD")
            .env_remove("RESTIC_REPOSITORY");
        if let Some(askpass) = &self.askpass {
            command
                .env("SSH_ASKPASS", askpass)
                .env("SSH_ASKPASS_REQUIRE", "force");
        }
        command
    }
}

fn engine() -> PathBuf {
    if let Some(engine) = std::env::var_os("UPKEEP_RESTIC").filter(|value| !value.is_empty()) {
        return engine.into();
    }
    if Path::new(INSTALLED_ENGINE).exists() {
        return INSTALLED_ENGINE.into();
    }
    "restic".into()
}

/// The SSH command restic starts for SFTP. Host keys are checked strictly; no password is on it.
pub fn ssh_command(destination: &Destination, known_hosts: &Path) -> Vec<String> {
    let mut command: Vec<String> = [
        "ssh",
        "-F",
        "/dev/null",
        "-p",
        &destination.port.to_string(),
        "-l",
        destination.user(),
        "-o",
        &format!("UserKnownHostsFile={}", known_hosts.display()),
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        &format!("HostKeyAlias={HOST_KEY_ALIAS}"),
        "-o",
        "ServerAliveInterval=30",
        "-o",
        "ServerAliveCountMax=6",
    ]
    .map(str::to_owned)
    .into();
    match &destination.key_file {
        Some(key_file) => command.extend([
            "-i".into(),
            key_file.display().to_string(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-o".into(),
            "BatchMode=yes".into(),
        ]),
        None => command.extend(
            // restic splits its -o values on commas, so no option here may contain one.
            [
                "-o",
                "PubkeyAuthentication=no",
                "-o",
                "NumberOfPasswordPrompts=1",
            ]
            .map(str::to_owned),
        ),
    }
    command.extend([destination.address().to_owned(), "-s".into(), "sftp".into()]);
    command
}

pub fn known_hosts_text(destination: &Destination) -> String {
    destination
        .host_keys
        .iter()
        .map(|key| format!("{HOST_KEY_ALIAS} {key}\n"))
        .collect()
}

fn askpass_script(password_file: &Path) -> Result<String, ConfigError> {
    let path = password_file.display().to_string();
    if path.contains('\'') {
        return Err(ConfigError::Invalid(format!(
            "the client folder path must not contain a quote: {path}"
        )));
    }
    Ok(format!("#!/bin/sh\nexec cat '{path}'\n"))
}

/// Joins arguments for restic's `sftp.command`, which splits on spaces and honours double quotes.
pub fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg
                .chars()
                .any(|character| character.is_whitespace() || character == '"')
            {
                format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A private temporary folder for the SSH files of one command; deleted on drop.
struct SessionDir(PathBuf);

impl SessionDir {
    fn create() -> Result<Self, ConfigError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let path = std::env::temp_dir().join(format!("upkeep-{}-{nanos}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|source| ConfigError::Write {
                path: path.display().to_string(),
                source,
            })?;
        Ok(Self(path))
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_session_file(path: &Path, text: &str, mode: u32) -> Result<(), ConfigError> {
    let write_error = |source| ConfigError::Write {
        path: path.display().to_string(),
        source,
    };
    std::fs::write(path, text).map_err(write_error)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(write_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn destination(key_file: Option<PathBuf>) -> Destination {
        Destination {
            ssh: "backup@10.0.2.2".into(),
            port: 22,
            path: "/backups".into(),
            auth: if key_file.is_some() {
                SshAuth::Key
            } else {
                SshAuth::Password
            },
            key_file,
            host_keys: vec!["ssh-ed25519 AAAA".into(), "ssh-rsa BBBB".into()],
        }
    }

    #[test]
    fn key_auth_is_batch_mode_with_one_identity() {
        let command = ssh_command(
            &destination(Some("/keys/my key".into())),
            Path::new("/s/known_hosts"),
        );
        let text = shell_join(&command);
        assert!(
            text.starts_with(
                "ssh -F /dev/null -p 22 -l backup -o UserKnownHostsFile=/s/known_hosts"
            )
        );
        assert!(text.contains("-o StrictHostKeyChecking=yes -o HostKeyAlias=upkeep-destination"));
        assert!(text.contains("-i \"/keys/my key\" -o IdentitiesOnly=yes -o BatchMode=yes"));
        assert!(text.ends_with("10.0.2.2 -s sftp"));
    }

    #[test]
    fn password_auth_uses_askpass_and_never_the_password() {
        let text = shell_join(&ssh_command(
            &destination(None),
            Path::new("/s/known_hosts"),
        ));
        assert!(text.contains("-o PubkeyAuthentication=no"));
        assert!(!text.contains("BatchMode") && !text.contains(','));
        assert_eq!(
            askpass_script(Path::new("/home/u/.config/upkeep/secrets/nas.ssh-password")).unwrap(),
            "#!/bin/sh\nexec cat '/home/u/.config/upkeep/secrets/nas.ssh-password'\n"
        );
        assert!(askpass_script(Path::new("/it's")).is_err());
    }

    #[test]
    fn known_hosts_use_the_alias() {
        assert_eq!(
            known_hosts_text(&destination(None)),
            "upkeep-destination ssh-ed25519 AAAA\nupkeep-destination ssh-rsa BBBB\n"
        );
    }

    #[test]
    fn session_folder_is_private_and_removed() {
        let session = SessionDir::create().unwrap();
        let path = session.0.clone();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        drop(session);
        assert!(!path.exists());
    }
}
