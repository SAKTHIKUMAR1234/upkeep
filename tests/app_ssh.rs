//! Runs operations over real SSH against the throwaway container of `tests/ssh/run.sh`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use upkeep::app::AppConfig;
use upkeep::app::operations::{self, Scripts};
use upkeep::app::runs::Executor;
use upkeep::app::runs::ssh::Ssh;
use upkeep::app::servers::test_connection;
use upkeep::app::store::{
    CachedOperation, Db, RunRecord, RunStatus, Server, ServerFields, StepStatus,
};

const SKIP: &str = "needs the sshd container: run tests/ssh/run.sh";

struct Remote {
    _dir: tempfile::TempDir,
    config: AppConfig,
    db: Arc<Db>,
    server: Server,
    executor: Arc<Executor>,
}

impl Remote {
    fn new() -> Self {
        let port = std::env::var("UPKEEP_TEST_SSH_PORT")
            .expect(SKIP)
            .parse()
            .unwrap();
        let key = PathBuf::from(std::env::var("UPKEEP_TEST_SSH_KEY").expect(SKIP));
        let dir = tempfile::tempdir().unwrap();
        let mut config = AppConfig::new(dir.path().to_path_buf());
        // The container has no real reboot: killing the listening sshd drops SSH for a few seconds.
        config.reboot_command = "sudo -n pkill -o -x sshd".into();
        config.reboot_poll = Duration::from_secs(1);
        config.reboot_timeout = Duration::from_secs(90);
        let db = Arc::new(config.open_db().unwrap());
        let fields = ServerFields {
            name: "Test container".into(),
            address: "127.0.0.1".into(),
            port,
            ssh_user: "upk".into(),
            key_path: key.display().to_string(),
        };
        let id = db.add_server(&fields, 1).unwrap();
        let server = db.server(id).unwrap().unwrap();
        let executor = Arc::new(Executor::new(db.clone(), Arc::new(config.clone())));
        Self {
            _dir: dir,
            config,
            db,
            server,
            executor,
        }
    }

    fn ssh(&self) -> Ssh {
        Ssh::new(&self.config, &self.server.fields())
    }

    async fn create(&self, folder: &str, run_sh: &str, check_sh: &str) {
        let scripts = Scripts {
            run_sh: run_sh.into(),
            check_sh: check_sh.into(),
        };
        operations::create(&self.ssh(), folder, &scripts)
            .await
            .unwrap();
    }

    async fn run(&self, folder: &str) -> RunRecord {
        let mut changes = self.executor.changes();
        let started = self
            .executor
            .start(&self.server, &operation(folder), "tester@example.com")
            .unwrap();
        loop {
            let run = tokio::time::timeout(Duration::from_secs(120), changes.recv())
                .await
                .expect("the run ends")
                .unwrap();
            if run.id == started.id && run.status != RunStatus::Running {
                return run;
            }
        }
    }

    fn output(&self, run_id: i64) -> String {
        std::fs::read_to_string(self.config.run_log(run_id)).unwrap_or_default()
    }

    async fn remote(&self, command: &str) -> String {
        self.ssh()
            .run(command, None, Duration::from_secs(30))
            .await
            .unwrap()
            .stdout
    }
}

fn operation(folder: &str) -> CachedOperation {
    CachedOperation {
        folder: folder.into(),
        title: None,
        has_check: false,
    }
}

fn steps(run: &RunRecord) -> Vec<(&str, StepStatus)> {
    run.steps
        .iter()
        .map(|step| (step.title.as_str(), step.status))
        .collect()
}

#[tokio::test]
#[ignore = "needs the sshd container: run tests/ssh/run.sh"]
async fn operations_run_over_real_ssh() {
    let remote = Remote::new();
    let fields = remote.server.fields();
    let tested = test_connection(&remote.config, &fields).await;
    assert!(tested.ok, "{tested:?}");

    // Create, list, read, edit.
    let run_sh = "#!/bin/bash\nset -euo pipefail\necho '::step Prepare'\necho preparing\n\
                  echo '::step Work'\necho \"working as $(id -un)\"\n";
    remote
        .create("steps", run_sh, "#!/bin/bash\necho healthy\n")
        .await;
    remote
        .create(
            "check-fails",
            "#!/bin/bash\necho fine\n",
            "#!/bin/bash\necho broken >&2\nexit 4\n",
        )
        .await;
    let tested = test_connection(&remote.config, &fields).await;
    assert_eq!(
        tested.message,
        "connected, sudo OK, /.upkeep found (2 operations)"
    );
    let listed = operations::list(&remote.ssh(), Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(
        listed,
        [("check-fails".into(), true), ("steps".into(), true)]
    );
    let mode = remote
        .remote("sudo -n stat -c '%a %U:%G' /.upkeep/steps/run.sh /.upkeep/steps")
        .await;
    assert_eq!(mode, "750 root:root\n750 root:root\n");
    let read = operations::read(&remote.ssh(), "steps").await.unwrap();
    assert_eq!(read.run_sh, run_sh);
    let edited = Scripts {
        check_sh: "#!/bin/bash\necho healthy\necho still healthy\n".into(),
        ..read.clone()
    };
    let changes = operations::update(&remote.ssh(), "steps", &read, &edited)
        .await
        .unwrap();
    assert_eq!(changes, [("check.sh", "+1 −0 lines".to_owned())]);

    // A run with steps, then check.sh.
    let run = remote.run("steps").await;
    assert_eq!(
        run.status,
        RunStatus::Succeeded,
        "{}",
        remote.output(run.id)
    );
    assert_eq!(
        steps(&run),
        [
            ("Connect over SSH", StepStatus::Ok),
            ("Prepare", StepStatus::Ok),
            ("Work", StepStatus::Ok),
            ("check.sh", StepStatus::Ok)
        ]
    );
    let output = remote.output(run.id);
    assert!(
        output.contains("working as root\n") && output.contains("still healthy\n"),
        "{output}"
    );
    assert!(!output.contains('\r'), "terminal line ends are removed");

    // check.sh fails.
    let run = remote.run("check-fails").await;
    assert_eq!(
        (run.status, run.exit_code),
        (RunStatus::CheckFailed, Some(4))
    );
    assert!(remote.output(run.id).contains("broken"));

    // Exit 10 restarts the server, waits for SSH to drop and come back, then runs check.sh.
    remote
        .create(
            "restart",
            "#!/bin/bash\necho needs restart\nexit 10\n",
            "#!/bin/bash\necho back\n",
        )
        .await;
    let run = remote.run("restart").await;
    assert_eq!(
        run.status,
        RunStatus::Succeeded,
        "{}",
        remote.output(run.id)
    );
    assert_eq!(steps(&run)[2], ("Restart and wait", StepStatus::Ok));
    assert!(remote.output(run.id).contains("the server is back"));

    // Stop ends the remote process.
    remote
        .create(
            "long",
            "#!/bin/bash\necho started\nsleep 300\necho never\n",
            "",
        )
        .await;
    let mut changes = remote.executor.changes();
    let started = remote
        .executor
        .start(&remote.server, &operation("long"), "tester@example.com")
        .unwrap();
    while !remote.output(started.id).contains("started") {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let second = remote
        .executor
        .start(&remote.server, &operation("steps"), "tester@example.com");
    assert!(
        second
            .unwrap_err()
            .to_string()
            .contains("already has an active run")
    );
    remote
        .executor
        .stop(started.id, "tester@example.com")
        .unwrap();
    let stopped = loop {
        let run = changes.recv().await.unwrap();
        if run.id == started.id && run.status != RunStatus::Running {
            break run;
        }
    };
    assert_eq!(stopped.status, RunStatus::Stopped);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let left = remote.remote("pgrep -x sleep || echo none").await;
    assert_eq!(left.trim(), "none", "the remote script ended");
    assert_eq!(remote.db.active_run(remote.server.id).unwrap(), None);

    // Delete.
    for folder in ["steps", "check-fails", "restart", "long"] {
        operations::delete(&remote.ssh(), folder).await.unwrap();
    }
    assert!(
        operations::list(&remote.ssh(), Duration::from_secs(30))
            .await
            .unwrap()
            .is_empty()
    );
}
