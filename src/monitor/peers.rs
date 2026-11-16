use std::time::Duration;

use futures_util::future::join_all;

use super::checks::{Level, Observation};
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
        let (level, value) = match reach(peer).await {
            Ok(()) => (Level::Ok, "up".to_owned()),
            Err(error) => (Level::Critical, format!("down: {error}")),
        };
        Observation::new(format!("peer:{}", peer.name), level, value)
    }))
    .await
}

async fn reach(peer: &PeerSettings) -> anyhow::Result<()> {
    let host = Host::new(&peer.url, peer.token_file.clone())?;
    let client = Client::new(host.endpoint()?);
    match tokio::time::timeout(PEER_TIMEOUT, client.get("/health")).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(ClientError::Connect { source, .. })) => Err(source.into()),
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
            url: format!("http://127.0.0.1:{port}"),
            token_file,
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
            Observation::new("peer:live", Level::Ok, "up")
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
}
