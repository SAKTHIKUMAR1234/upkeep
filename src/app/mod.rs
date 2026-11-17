pub mod auth;
pub mod operations;
pub mod routes;
pub mod runs;
pub mod servers;
pub mod store;

use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::net::TcpListener;

use runs::Executor;
use store::Db;

/// Where the desktop app keeps its state, and how it reaches servers.
#[derive(Clone, Debug)]
pub struct AppConfig {
    pub data_dir: PathBuf,
    /// The `ssh` program. Tests put a fake here.
    pub ssh_program: PathBuf,
    /// The remote command that restarts a server after `run.sh` exits with 10.
    pub reboot_command: String,
    /// How often the app tries SSH while it waits for a restarted server.
    pub reboot_poll: Duration,
    /// How long the app waits for a restarted server to come back.
    pub reboot_timeout: Duration,
}

impl AppConfig {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            ssh_program: "ssh".into(),
            reboot_command: "sudo -n systemctl reboot".into(),
            reboot_poll: Duration::from_secs(5),
            reboot_timeout: Duration::from_secs(15 * 60),
        }
    }

    pub fn database(&self) -> PathBuf {
        self.data_dir.join("upkeep-app.db")
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.data_dir.join("runs")
    }

    pub fn run_log(&self, run_id: i64) -> PathBuf {
        self.runs_dir().join(format!("{run_id}.log"))
    }

    pub fn known_hosts(&self) -> PathBuf {
        self.data_dir.join("known_hosts")
    }

    /// Creates the owner-only data folder and opens the database.
    pub fn open_db(&self) -> anyhow::Result<Db> {
        for dir in [&self.data_dir, &self.runs_dir()] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = self.database();
        Db::open(&path).with_context(|| format!("cannot open {}", path.display()))
    }
}

/// `~/.local/share/upkeep-app`.
pub fn default_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
    PathBuf::from(home).join(".local/share/upkeep-app")
}

/// The REST service, bound to a loopback port and ready to serve.
pub struct Service {
    listener: TcpListener,
    executor: Arc<Executor>,
    router: axum::Router,
}

impl Service {
    /// Opens the data folder, marks runs that a previous process left as interrupted, and binds
    /// 127.0.0.1:`port` (0 picks a free port).
    pub async fn bind(config: AppConfig, port: u16) -> anyhow::Result<Self> {
        let db = Arc::new(config.open_db()?);
        let interrupted = db.interrupt_running_runs()?;
        if interrupted > 0 {
            tracing::warn!(
                interrupted,
                "runs left running by the last process are interrupted"
            );
        }
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .await
            .with_context(|| format!("cannot listen on 127.0.0.1:{port}"))?;
        let port = listener.local_addr()?.port();
        let executor = Arc::new(Executor::new(db.clone(), Arc::new(config)));
        let router = routes::router(routes::AppState::new(db, executor.clone(), port));
        Ok(Self {
            listener,
            executor,
            router,
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .expect("a bound listener has an address")
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.address().port())
    }

    pub fn executor(&self) -> Arc<Executor> {
        self.executor.clone()
    }

    /// Serves until `shutdown` completes. Open event streams end with it.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
        tokio::select! {
            result = axum::serve(self.listener, self.router) => result?,
            () = shutdown => {}
        }
        Ok(())
    }
}
