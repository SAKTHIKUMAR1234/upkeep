#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;
use upkeep::client::{Client, ClientError, Endpoint};

const WAIT_LIMIT: Duration = Duration::from_secs(30);

/// A runner daemon in a temp dir, with the example config and the demo adapters.
pub struct Harness {
    root: TempDir,
    daemon: Option<Child>,
}

impl Harness {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        copy_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/config"),
            &root.path().join("config"),
        );
        let reboot_script = root.path().join("reboot.sh");
        std::fs::write(
            &reboot_script,
            format!(
                "#!/bin/sh\necho reboot >> {}/reboots\n",
                root.path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&reboot_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let harness = Self { root, daemon: None };
        harness.write_settings("");
        harness
    }

    pub fn write_settings(&self, extra: &str) {
        let reboot = self.root.path().join("reboot.sh");
        let settings = format!(
            "launcher = \"setsid\"\nreboot_command = [\"{}\"]\n{extra}",
            reboot.display()
        );
        std::fs::write(self.config_dir().join("upkeep.toml"), settings).unwrap();
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.path().join("config")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.path().join("state")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.root.path().join("log")
    }

    pub fn command(&self, subcommand: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_upkeep"));
        command
            .arg("--config-dir")
            .arg(self.config_dir())
            .arg("--state-dir")
            .arg(self.state_dir())
            .arg("--log-dir")
            .arg(self.log_dir())
            .arg(subcommand);
        command
    }

    pub async fn start(&mut self) {
        let daemon = self
            .command("serve")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(self.root.path().join("daemon.log")).unwrap())
            .spawn()
            .unwrap();
        self.daemon = Some(daemon);
        self.wait_until("the daemon answers", || async {
            self.client().get("/jobs").await.ok()
        })
        .await;
    }

    /// Kills the daemon like a crash would: no shutdown, steps keep running.
    pub fn kill(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            daemon.kill().unwrap();
            daemon.wait().unwrap();
        }
    }

    pub fn client(&self) -> Client {
        Client::new(Endpoint::Unix(self.state_dir().join("upkeep.sock")))
    }

    pub async fn run(&self, target: &str, confirm: bool) -> Result<i64, ClientError> {
        let started = self
            .client()
            .post("/run", &json!({"target": target, "confirm": confirm}))
            .await?;
        Ok(started["id"].as_i64().unwrap())
    }

    pub async fn job(&self, id: i64) -> Value {
        self.client().get(&format!("/jobs/{id}")).await.unwrap()
    }

    pub async fn wait_for_status(&self, id: i64, status: &str) -> Value {
        self.wait_until(&format!("job {id} is {status}"), || async {
            let detail = self.job(id).await;
            (detail["job"]["status"] == status).then_some(detail)
        })
        .await
    }

    pub async fn wait_for_step(&self, id: i64, action: &str, status: &str) {
        self.wait_until(&format!("step {action} is {status}"), || async {
            let detail = self.job(id).await;
            let steps = detail["steps"].as_array()?;
            steps
                .iter()
                .any(|step| step["action"] == action && step["status"] == status)
                .then_some(())
        })
        .await;
    }

    pub async fn wait_until<T, F, Fut>(&self, what: &str, mut probe: F) -> T
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        let started = Instant::now();
        loop {
            if let Some(value) = probe().await {
                return value;
            }
            assert!(
                started.elapsed() < WAIT_LIMIT,
                "timed out waiting until {what}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Lines the adapter appended to `actions.log`, without the job suffix.
    pub fn actions(&self, adapter: &str) -> Vec<String> {
        let path = self
            .state_dir()
            .join("adapters")
            .join(adapter)
            .join("actions.log");
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| {
                line.split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }

    pub fn reboots(&self) -> usize {
        std::fs::read_to_string(self.root.path().join("reboots"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    pub fn adapter_state(&self, adapter: &str) -> PathBuf {
        let dir = self.state_dir().join("adapters").join(adapter);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Adds a shell adapter. `script` handles "$1" and appends it to actions.log.
    pub fn add_adapter(&self, name: &str, actions: &str, script: &str) {
        let dir = self.config_dir().join("adapters").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = format!("name = \"{name}\"\nrun = \"sh adapter.sh\"\n{actions}");
        std::fs::write(dir.join("adapter.toml"), manifest).unwrap();
        let script = format!("set -eu\necho \"$1\" >> \"$UPKEEP_STATE_DIR/actions.log\"\n{script}");
        std::fs::write(dir.join("adapter.sh"), script).unwrap();
    }

    pub fn add_plan(&self, name: &str, text: &str) {
        std::fs::write(
            self.config_dir().join("plans").join(format!("{name}.toml")),
            text,
        )
        .unwrap();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.kill();
    }
}

pub fn statuses(detail: &Value) -> Vec<String> {
    detail["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            format!(
                "{}:{}:{}={}",
                step["phase"].as_str().unwrap(),
                step["adapter"].as_str().unwrap(),
                step["action"].as_str().unwrap(),
                step["status"].as_str().unwrap()
            )
        })
        .collect()
}

fn copy_dir(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let destination = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &destination);
        } else {
            std::fs::copy(entry.path(), destination).unwrap();
        }
    }
}
