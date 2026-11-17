use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::runs::ssh::{Ssh, SshError, quote};

/// The folder on each server that holds one folder per operation.
pub const ROOT: &str = "/.upkeep";
const TIMEOUT: Duration = Duration::from_secs(60);

/// Exit codes of the remote commands below, chosen apart from the codes of sudo and coreutils.
const EXIT_SUDO: i32 = 41;
const EXIT_NO_ROOT: i32 = 42;
const EXIT_NOT_FOUND: i32 = 43;
const EXIT_EXISTS: i32 = 44;
/// Every command first checks that sudo works without a password.
const SUDO_CHECK: &str = "sudo -n true 2>/dev/null || exit 41";

#[derive(Debug, thiserror::Error)]
pub enum OperationError {
    #[error("Use a folder name of lowercase letters, digits and dashes.")]
    InvalidFolder,
    #[error("{0} must start with a #! line, such as #!/bin/bash.")]
    NoShebang(&'static str),
    #[error("sudo asks for a password on this server: allow passwordless sudo for the SSH user")]
    Sudo,
    #[error("/.upkeep does not exist on this server")]
    NoFolder,
    #[error("the operation folder does not exist on the server")]
    NotFound,
    #[error("the folder already exists on the server")]
    Exists,
    #[error(transparent)]
    Ssh(#[from] SshError),
    #[error("the server answered with an error: {0}")]
    Failed(String),
}

/// Whether a remote call reached the server: `None` when it failed before connecting, such as
/// for a key file with open permissions.
pub fn reachability<T>(result: &Result<T, OperationError>) -> Option<bool> {
    match result {
        Err(OperationError::Ssh(SshError::Connect(_) | SshError::Timeout(_))) => Some(false),
        Err(
            OperationError::Ssh(_) | OperationError::InvalidFolder | OperationError::NoShebang(_),
        ) => None,
        _ => Some(true),
    }
}

/// The scripts of one operation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scripts {
    pub run_sh: String,
    /// Empty when the operation has no check.
    #[serde(default)]
    pub check_sh: String,
}

impl Scripts {
    pub fn validate(&self) -> Result<(), OperationError> {
        if !self.run_sh.starts_with("#!") {
            return Err(OperationError::NoShebang("run.sh"));
        }
        if self.has_check() && !self.check_sh.starts_with("#!") {
            return Err(OperationError::NoShebang("check.sh"));
        }
        Ok(())
    }

    pub fn has_check(&self) -> bool {
        !self.check_sh.trim().is_empty()
    }
}

/// `[a-z0-9-]+`, at most 64 characters.
pub fn is_valid_folder(folder: &str) -> bool {
    !folder.is_empty()
        && folder.len() <= 64
        && folder.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

/// The name shown for a folder that has no stored name: `disk-cleanup` is "Disk cleanup".
pub fn title_of(folder: &str) -> String {
    let words = folder.replace('-', " ");
    let mut characters = words.trim().chars();
    characters
        .next()
        .map(|first| first.to_uppercase().chain(characters).collect())
        .unwrap_or_default()
}

/// The quoted path of a file in an operation folder; `file` empty for the folder itself.
fn path(folder: &str, file: &str) -> String {
    let path = if file.is_empty() {
        format!("{ROOT}/{folder}")
    } else {
        format!("{ROOT}/{folder}/{file}")
    };
    quote(&path)
}

fn checked(folder: &str) -> Result<&str, OperationError> {
    is_valid_folder(folder)
        .then_some(folder)
        .ok_or(OperationError::InvalidFolder)
}

/// Runs a remote command and maps its exit code to an error.
async fn remote(
    ssh: &Ssh,
    command: &str,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<String, OperationError> {
    let output = ssh.run(command, input, timeout).await?;
    match output.code {
        Some(0) => Ok(output.stdout),
        Some(EXIT_SUDO) => Err(OperationError::Sudo),
        Some(EXIT_NO_ROOT) => Err(OperationError::NoFolder),
        Some(EXIT_NOT_FOUND) => Err(OperationError::NotFound),
        Some(EXIT_EXISTS) => Err(OperationError::Exists),
        _ => Err(OperationError::Failed(output.error_line())),
    }
}

/// The operation folders on the server: each folder with a `run.sh`, and whether it has a
/// `check.sh`.
pub async fn list(ssh: &Ssh, timeout: Duration) -> Result<Vec<(String, bool)>, OperationError> {
    let command = format!(
        "{SUDO_CHECK}; sudo -n test -d {ROOT} || exit {EXIT_NO_ROOT}; \
         sudo -n find {ROOT} -mindepth 2 -maxdepth 2 -type f '(' -name run.sh -o -name check.sh ')'"
    );
    let stdout = remote(ssh, &command, None, timeout).await?;
    Ok(parse_listing(&stdout))
}

fn parse_listing(stdout: &str) -> Vec<(String, bool)> {
    let files: Vec<(&str, &str)> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix(ROOT)?.strip_prefix('/')?.split_once('/'))
        .filter(|(folder, _)| is_valid_folder(folder))
        .collect();
    let mut operations: Vec<(String, bool)> = files
        .iter()
        .filter(|(_, file)| *file == "run.sh")
        .map(|(folder, _)| {
            let has_check = files.contains(&(folder, "check.sh"));
            ((*folder).to_owned(), has_check)
        })
        .collect();
    operations.sort();
    operations
}

/// The current scripts of an operation.
pub async fn read(ssh: &Ssh, folder: &str) -> Result<Scripts, OperationError> {
    let folder = checked(folder)?;
    let (run, check) = (path(folder, "run.sh"), path(folder, "check.sh"));
    let command = format!(
        "{SUDO_CHECK}; sudo -n test -f {run} || exit {EXIT_NOT_FOUND}; \
         sudo -n base64 -w0 -- {run}; echo; \
         if sudo -n test -f {check}; then sudo -n base64 -w0 -- {check}; fi; echo"
    );
    let stdout = remote(ssh, &command, None, TIMEOUT).await?;
    let mut lines = stdout.lines().map(decode);
    let run_sh = lines.next().transpose()?.unwrap_or_default();
    let check_sh = lines.next().transpose()?.unwrap_or_default();
    Ok(Scripts { run_sh, check_sh })
}

fn decode(line: &str) -> Result<String, OperationError> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(line.trim())
        .map_err(|error| OperationError::Failed(format!("unreadable script: {error}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Whether the operation exists and has a `check.sh`. Also proves SSH and sudo work.
pub async fn probe(ssh: &Ssh, folder: &str) -> Result<bool, OperationError> {
    let folder = checked(folder)?;
    let command = format!(
        "{SUDO_CHECK}; sudo -n test -f {} || exit {EXIT_NOT_FOUND}; \
         if sudo -n test -f {}; then echo check; fi",
        path(folder, "run.sh"),
        path(folder, "check.sh")
    );
    Ok(remote(ssh, &command, None, TIMEOUT).await?.trim() == "check")
}

/// The remote command that runs one script of an operation as root.
pub fn script_command(folder: &str, file: &str) -> Result<String, OperationError> {
    Ok(format!("sudo -n {}", path(checked(folder)?, file)))
}

/// Creates `/.upkeep/<folder>` with its scripts, owned by root with mode 0750.
pub async fn create(ssh: &Ssh, folder: &str, scripts: &Scripts) -> Result<(), OperationError> {
    let folder = checked(folder)?;
    scripts.validate()?;
    let command = format!(
        "{SUDO_CHECK}; sudo -n test ! -e {dir} || exit {EXIT_EXISTS}; \
         sudo -n install -d -m 0750 -o root -g root {ROOT} {dir}",
        dir = path(folder, "")
    );
    remote(ssh, &command, None, TIMEOUT).await?;
    write_file(ssh, folder, "run.sh", &scripts.run_sh).await?;
    if scripts.has_check() {
        write_file(ssh, folder, "check.sh", &scripts.check_sh).await?;
    }
    Ok(())
}

/// Saves the scripts that changed; an empty `check.sh` removes the file. Returns the files that
/// changed, each with a short diff summary.
pub async fn update(
    ssh: &Ssh,
    folder: &str,
    old: &Scripts,
    new: &Scripts,
) -> Result<Vec<(&'static str, String)>, OperationError> {
    let folder = checked(folder)?;
    new.validate()?;
    let mut changes = Vec::new();
    if old.run_sh != new.run_sh {
        write_file(ssh, folder, "run.sh", &new.run_sh).await?;
        changes.push(("run.sh", diff_summary(&old.run_sh, &new.run_sh)));
    }
    if old.check_sh != new.check_sh {
        if new.has_check() {
            write_file(ssh, folder, "check.sh", &new.check_sh).await?;
        } else {
            let command = format!(
                "{SUDO_CHECK}; sudo -n rm -f -- {}",
                path(folder, "check.sh")
            );
            remote(ssh, &command, None, TIMEOUT).await?;
        }
        changes.push(("check.sh", diff_summary(&old.check_sh, &new.check_sh)));
    }
    Ok(changes)
}

/// Writes one script through a temporary file, so the server never runs half a script.
async fn write_file(
    ssh: &Ssh,
    folder: &str,
    file: &str,
    content: &str,
) -> Result<(), OperationError> {
    let (target, temporary) = (path(folder, file), path(folder, &format!(".{file}.new")));
    let command = format!(
        "{SUDO_CHECK}; sudo -n test -d {dir} || exit {EXIT_NOT_FOUND}; \
         sudo -n tee {temporary} >/dev/null && sudo -n chown root:root {temporary} && \
         sudo -n chmod 0750 {temporary} && sudo -n mv -f -- {temporary} {target}",
        dir = path(folder, "")
    );
    remote(ssh, &command, Some(content.as_bytes()), TIMEOUT).await?;
    Ok(())
}

/// Removes `/.upkeep/<folder>` and everything in it.
pub async fn delete(ssh: &Ssh, folder: &str) -> Result<(), OperationError> {
    let dir = path(checked(folder)?, "");
    let command = format!(
        "{SUDO_CHECK}; sudo -n test -d {dir} || exit {EXIT_NOT_FOUND}; sudo -n rm -rf -- {dir}"
    );
    remote(ssh, &command, None, TIMEOUT).await?;
    Ok(())
}

/// "+3 −1 lines": lines added and removed, from the longest common subsequence of lines.
pub fn diff_summary(old: &str, new: &str) -> String {
    let (old, new): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let mut row = vec![0usize; new.len() + 1];
    for old_line in &old {
        let mut diagonal = 0;
        for (index, new_line) in new.iter().enumerate() {
            let above = row[index + 1];
            row[index + 1] = if old_line == new_line {
                diagonal + 1
            } else {
                above.max(row[index])
            };
            diagonal = above;
        }
    }
    let common = row[new.len()];
    format!("+{} −{} lines", new.len() - common, old.len() - common)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_are_lowercase_digits_and_dashes() {
        for good in ["os-update", "hr-bench-update", "a", "disk-cleanup-2"] {
            assert!(is_valid_folder(good), "{good}");
        }
        for bad in [
            "",
            "OS",
            "a b",
            "a/b",
            "..",
            "a;b",
            "a_b",
            "é",
            &"a".repeat(65),
        ] {
            assert!(!is_valid_folder(bad), "{bad}");
        }
    }

    #[test]
    fn titles_come_from_folder_names() {
        assert_eq!(title_of("disk-cleanup"), "Disk cleanup");
        assert_eq!(title_of("a"), "A");
    }

    #[test]
    fn listing_needs_run_sh_and_notes_check_sh() {
        let stdout = "/.upkeep/os-update/run.sh\n/.upkeep/os-update/check.sh\n\
                      /.upkeep/disk-cleanup/run.sh\n/.upkeep/only-check/check.sh\n\
                      /.upkeep/Bad Name/run.sh\n/elsewhere/x/run.sh\n";
        assert_eq!(
            parse_listing(stdout),
            [("disk-cleanup".into(), false), ("os-update".into(), true)]
        );
    }

    #[test]
    fn scripts_need_a_shebang() {
        let scripts = |run: &str, check: &str| Scripts {
            run_sh: run.into(),
            check_sh: check.into(),
        };
        assert!(scripts("#!/bin/bash\n", "").validate().is_ok());
        assert!(scripts("#!/bin/bash\n", "  \n").validate().is_ok());
        assert!(matches!(
            scripts("echo hi", "").validate(),
            Err(OperationError::NoShebang("run.sh"))
        ));
        assert!(matches!(
            scripts("#!/bin/sh", "df -h").validate(),
            Err(OperationError::NoShebang("check.sh"))
        ));
    }

    #[test]
    fn diff_summary_counts_changed_lines() {
        assert_eq!(diff_summary("a\nb\nc", "a\nb\nc"), "+0 −0 lines");
        assert_eq!(diff_summary("a\nb\nc", "a\nx\nc\nd"), "+2 −1 lines");
        assert_eq!(diff_summary("", "a\nb"), "+2 −0 lines");
    }

    #[test]
    fn paths_are_quoted_for_the_remote_shell() {
        assert_eq!(path("os-update", "run.sh"), "'/.upkeep/os-update/run.sh'");
        assert!(checked("x;reboot").is_err());
    }
}
