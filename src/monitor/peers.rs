use std::time::Duration;

use futures_util::future::join_all;
use serde_json::Value;

use super::checks::{Level, Observation};
use super::db::CheckRow;
use super::message::digest::{HostReport, Section};
use super::settings::PeerSettings;
use crate::client::config::check_name;
use crate::client::hosts::Host;
use crate::client::{Client, ClientError};

const PEER_TIMEOUT: Duration = Duration::from_secs(10);

/// Checks that each peer's name, URL and token file are usable.
pub fn validate(peers: &[PeerSettings]) -> anyhow::Result<()> {
    for peer in peers {
        check_name(&peer.name)?;
        Host::new(&peer.url, peer.token_file.clone())?.endpoint()?;
    }
    Ok(())
}

/// One `peer:<name>` check per peer: ok when its `/health` answers, critical when it does not.
pub async fn observe(peers: &[PeerSettings]) -> Vec<Observation> {
    join_all(peers.iter().map(|peer| async move {
        let (level, value) = match get(peer, "/health").await {
            Ok(_) => (Level::Ok, "up".to_owned()),
            Err(error) => (Level::Critical, format!("down: {error}")),
        };
        Observation::new(format!("peer:{}", peer.name), level, value)
            .with_subject(peer.display_name())
    }))
    .await
}

/// The peer's part of the daily summary, from its `GET /monitor`.
pub async fn section(peer: &PeerSettings) -> Section {
    let report = async {
        let body = get(peer, "/monitor").await?;
        let checks: Vec<CheckRow> = serde_json::from_value(body["checks"].clone())?;
        let name = match (&peer.display_name, body["display_name"].as_str()) {
            (Some(name), _) => name.clone(),
            (None, Some(name)) => name.to_owned(),
            (None, None) => peer.name.clone(),
        };
        anyhow::Ok(HostReport {
            name,
            checks: checks
                .into_iter()
                .map(|row| (row.observation(), row.state))
                .collect(),
            updates: serde_json::from_value(body["updates"].clone()).unwrap_or_default(),
        })
    };
    match report.await {
        Ok(host) => Section::Host(host),
        Err(error) => Section::Unreachable {
            name: peer.display_name().to_owned(),
            error: error.to_string(),
        },
    }
}

/// GETs `path` from the peer's API, within the peer timeout.
pub async fn get(peer: &PeerSettings, path: &str) -> anyhow::Result<Value> {
    let host = Host::new(&peer.url, peer.token_file.clone())?;
    let client = Client::new(host.endpoint()?);
    match tokio::time::timeout(PEER_TIMEOUT, client.get(path)).await {
        Ok(Ok(body)) => Ok(body),
        Ok(Err(ClientError::Connect { reason, .. })) => Err(reason.into()),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => anyhow::bail!("no answer in {}s", PEER_TIMEOUT.as_secs()),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use axum::routing::get;

    use super::*;
    use crate::api::auth::{Token, require_token};

    fn peer(dir: &std::path::Path, name: &str, port: u16) -> PeerSettings {
        let token_file = dir.join(format!("{name}.token"));
        std::fs::write(&token_file, "peer-token").unwrap();
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        PeerSettings {
            name: name.into(),
            display_name: None,
            url: format!("http://127.0.0.1:{port}"),
            token_file,
            down_after: Duration::from_secs(600),
        }
    }

    #[tokio::test]
    async fn an_unreachable_peer_is_critical_and_a_live_one_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_port = listener.local_addr().unwrap().port();
        let live = peer(dir.path(), "live", live_port);
        let token = Token::load(&live.token_file).unwrap();
        let app = axum::Router::new()
            .route(
                "/health",
                get(|| async { axum::Json(serde_json::json!({"ok": false})) }),
            )
            .layer(axum::middleware::from_fn_with_state(token, require_token));
        tokio::spawn(async move { axum::serve(listener, app).await });

        let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dead = peer(dir.path(), "dead", closed_port);
        validate(&[live.clone(), dead.clone()]).unwrap();

        let observations = observe(&[live, dead]).await;
        assert_eq!(
            observations[0],
            Observation::new("peer:live", Level::Ok, "up").with_subject("live")
        );
        assert_eq!(observations[1].key, "peer:dead");
        assert_eq!(observations[1].level, Level::Critical);
        assert!(
            observations[1]
                .value
                .starts_with("down: Connection refused"),
            "{}",
            observations[1].value
        );
    }

    #[test]
    fn rejects_bad_peer_names_and_urls() {
        let dir = tempfile::tempdir().unwrap();
        let mut bad_name = peer(dir.path(), "ok", 1);
        bad_name.name = "Bad Name".into();
        assert!(validate(&[bad_name]).is_err());
        let mut bad_url = peer(dir.path(), "ok", 1);
        bad_url.url = "https://x:1".into();
        assert!(validate(&[bad_url]).is_err());
    }

    #[tokio::test]
    async fn a_summary_section_comes_from_the_peer_monitor_report() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let live = peer(dir.path(), "erp", port);
        let report = serde_json::json!({
            "display_name": "YRP server (.22)",
            "updates": {"packages": 70, "risky": 4, "checked_at": 5},
            "checks": [{"key": "disk:/", "level": "warn", "since": 1, "streak": 0,
                        "last_sent_at": null, "notified": true, "observed": "warn",
                        "value": "84% used", "amount": 84.0, "updated_at": 9}],
        });
        let app =
            axum::Router::new().route("/monitor", get(move || async move { axum::Json(report) }));
        tokio::spawn(async move { axum::serve(listener, app).await });

        let Section::Host(host) = section(&live).await else {
            panic!("the peer answers");
        };
        assert_eq!(host.name, "YRP server (.22)");
        assert_eq!(host.updates.unwrap().risky, 4);
        let (disk, state) = &host.checks[0];
        assert_eq!((disk.amount, state.level), (Some(84.0), Level::Warn));

        let closed = peer(dir.path(), "gone", 1);
        assert!(matches!(
            section(&closed).await,
            Section::Unreachable { name, .. } if name == "gone"
        ));
    }
}
