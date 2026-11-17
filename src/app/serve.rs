use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use axum::http::HeaderMap;
use serde::Deserialize;

/// How long sessions live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionLimits {
    /// A session ends this long after the last use; the screen then asks for the password.
    pub idle_ms: i64,
    /// A session ends this long after sign-in, used or not.
    pub absolute_ms: i64,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            idle_ms: 15 * 60_000,
            absolute_ms: 12 * 3_600_000,
        }
    }
}

/// Where the service listens and who may reach it. Without a public origin it serves the
/// loopback address only; with one it is a website behind a TLS proxy.
#[derive(Clone, Debug)]
pub struct ServeConfig {
    pub listen: SocketAddr,
    /// The site's origin, such as `https://upkeep.example.com`, in lower case.
    pub public_origin: Option<String>,
    /// The proxy whose `X-Forwarded-For` and `X-Real-IP` headers name the client.
    pub trusted_proxy: Option<IpAddr>,
    pub sessions: SessionLimits,
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("{0} is not 127.0.0.1; set public_url to serve a website")]
    NotLoopback(SocketAddr),
    #[error("public_url must look like https://host or https://host:port, not {0:?}")]
    BadPublicUrl(String),
    #[error("{name} must be a duration of 2m or more, such as 15m or 12h, not {value:?}")]
    BadDuration { name: &'static str, value: String },
    #[error("idle_timeout must be shorter than session_lifetime")]
    IdleNotShorter,
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid config {path}: {source}")]
    Parse {
        path: String,
        source: Box<toml::de::Error>,
    },
}

/// The `upkeep app serve` settings of a TOML file or of the flags. Flags win.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServeSettings {
    pub listen: Option<SocketAddr>,
    pub public_url: Option<String>,
    pub trusted_proxy: Option<IpAddr>,
    pub idle_timeout: Option<String>,
    pub session_lifetime: Option<String>,
}

impl ServeSettings {
    pub fn read(path: &Path) -> Result<Self, ServeError> {
        let shown = || path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|source| ServeError::Read {
            path: shown(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| ServeError::Parse {
            path: shown(),
            source: Box::new(source),
        })
    }

    /// These settings, with the gaps filled from `fallback`.
    pub fn or(self, fallback: Self) -> Self {
        Self {
            listen: self.listen.or(fallback.listen),
            public_url: self.public_url.or(fallback.public_url),
            trusted_proxy: self.trusted_proxy.or(fallback.trusted_proxy),
            idle_timeout: self.idle_timeout.or(fallback.idle_timeout),
            session_lifetime: self.session_lifetime.or(fallback.session_lifetime),
        }
    }
}

impl ServeConfig {
    pub const DEFAULT_PORT: u16 = 7890;

    /// The desktop app's mode: 127.0.0.1:`port` (0 picks a free port), no proxy.
    pub fn loopback(port: u16) -> Self {
        Self {
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            public_origin: None,
            trusted_proxy: None,
            sessions: SessionLimits::default(),
        }
    }

    /// Checks the settings. An address other than 127.0.0.1 needs a public URL: the loopback guard
    /// accepts only the hosts `127.0.0.1` and `localhost`.
    pub fn new(settings: ServeSettings) -> Result<Self, ServeError> {
        let defaults = SessionLimits::default();
        let sessions = SessionLimits {
            idle_ms: duration_ms("idle_timeout", settings.idle_timeout, defaults.idle_ms)?,
            absolute_ms: duration_ms(
                "session_lifetime",
                settings.session_lifetime,
                defaults.absolute_ms,
            )?,
        };
        if sessions.idle_ms >= sessions.absolute_ms {
            return Err(ServeError::IdleNotShorter);
        }
        let config = Self {
            listen: settings
                .listen
                .unwrap_or_else(|| Self::loopback(Self::DEFAULT_PORT).listen),
            public_origin: settings
                .public_url
                .as_deref()
                .map(public_origin)
                .transpose()?,
            trusted_proxy: settings.trusted_proxy,
            sessions,
        };
        if config.public_origin.is_none() && config.listen.ip() != Ipv4Addr::LOCALHOST {
            return Err(ServeError::NotLoopback(config.listen));
        }
        Ok(config)
    }

    pub fn is_website(&self) -> bool {
        self.public_origin.is_some()
    }

    /// The client's address: the proxy's word for it when the peer is the trusted proxy, the
    /// peer otherwise.
    pub fn client_ip(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        match self.trusted_proxy {
            Some(proxy) if proxy == peer => forwarded_ip(headers, proxy).unwrap_or(peer),
            _ => peer,
        }
    }
}

/// The right-most `X-Forwarded-For` entry that is not the proxy, or else `X-Real-IP`. Entries to
/// the left of it come from the client and are not trusted.
fn forwarded_ip(headers: &HeaderMap, proxy: IpAddr) -> Option<IpAddr> {
    let text = |name| headers.get(name).and_then(|value| value.to_str().ok());
    let forwarded = text("x-forwarded-for").and_then(|list| {
        let entry = list
            .rsplit(',')
            .map(str::trim)
            .find(|entry| entry.parse() != Ok(proxy))?;
        entry.parse().ok()
    });
    forwarded.or_else(|| text("x-real-ip")?.trim().parse().ok())
}

/// The shortest idle time and lifetime: twice the minute between the page's pings, so input
/// always reaches the service before the session goes idle.
const MIN_DURATION: Duration = Duration::from_secs(120);

fn duration_ms(
    name: &'static str,
    value: Option<String>,
    default_ms: i64,
) -> Result<i64, ServeError> {
    let Some(value) = value else {
        return Ok(default_ms);
    };
    humantime::parse_duration(&value)
        .ok()
        .filter(|duration| *duration >= MIN_DURATION)
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or(ServeError::BadDuration { name, value })
}

/// `https://host[:port]` with an optional trailing `/`, as a lower-case origin.
fn public_origin(url: &str) -> Result<String, ServeError> {
    let bad = || ServeError::BadPublicUrl(url.to_owned());
    let host = url
        .strip_prefix("https://")
        .ok_or_else(bad)?
        .trim_end_matches('/');
    let valid = !host.is_empty()
        && host
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-.:[]".contains(character));
    if !valid {
        return Err(bad());
    }
    Ok(format!("https://{}", host.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn settings(listen: &str, public_url: Option<&str>) -> ServeSettings {
        ServeSettings {
            listen: Some(listen.parse().unwrap()),
            public_url: public_url.map(Into::into),
            ..ServeSettings::default()
        }
    }

    #[test]
    fn a_public_address_needs_a_public_url() {
        assert!(matches!(
            ServeConfig::new(settings("0.0.0.0:7890", None)),
            Err(ServeError::NotLoopback(_))
        ));
        for loopback in ["[::1]:7890", "127.0.0.2:7890"] {
            assert!(
                matches!(
                    ServeConfig::new(settings(loopback, None)),
                    Err(ServeError::NotLoopback(_))
                ),
                "the guard would refuse every request to {loopback}"
            );
        }
        let website = ServeConfig::new(settings(
            "0.0.0.0:7890",
            Some("https://Upkeep.Example.com/"),
        ))
        .unwrap();
        assert_eq!(
            website.public_origin.as_deref(),
            Some("https://upkeep.example.com")
        );
        let default = ServeConfig::new(ServeSettings::default()).unwrap();
        assert_eq!(default.listen, "127.0.0.1:7890".parse().unwrap());
        assert!(!default.is_website());
    }

    #[test]
    fn public_urls_are_https_origins_only() {
        for bad in [
            "http://upkeep.example.com",
            "https://",
            "https://upkeep.example.com/app",
            "https://user@upkeep.example.com",
            "upkeep.example.com",
        ] {
            assert!(public_origin(bad).is_err(), "{bad}");
        }
        assert_eq!(
            public_origin("https://upkeep.example.com:8443").unwrap(),
            "https://upkeep.example.com:8443"
        );
    }

    #[test]
    fn session_limits_come_from_the_settings() {
        let config = ServeConfig::new(ServeSettings {
            idle_timeout: Some("5m".into()),
            session_lifetime: Some("1h".into()),
            ..ServeSettings::default()
        })
        .unwrap();
        assert_eq!(
            config.sessions,
            SessionLimits {
                idle_ms: 300_000,
                absolute_ms: 3_600_000
            }
        );
        for (idle, lifetime) in [("soon", "1h"), ("1s", "1h"), ("1m", "1h"), ("2h", "1h")] {
            let result = ServeConfig::new(ServeSettings {
                idle_timeout: Some(idle.into()),
                session_lifetime: Some(lifetime.into()),
                ..ServeSettings::default()
            });
            assert!(result.is_err(), "{idle} {lifetime}");
        }
    }

    #[test]
    fn flags_win_over_the_file() {
        let flags = ServeSettings {
            listen: Some("127.0.0.1:9000".parse().unwrap()),
            ..ServeSettings::default()
        };
        let file: ServeSettings = toml::from_str(
            "listen = \"127.0.0.1:8000\"\npublic_url = \"https://upkeep.example.com\"",
        )
        .unwrap();
        let merged = flags.or(file);
        assert_eq!(merged.listen, Some("127.0.0.1:9000".parse().unwrap()));
        assert_eq!(
            merged.public_url.as_deref(),
            Some("https://upkeep.example.com")
        );
        assert!(toml::from_str::<ServeSettings>("port = 1").is_err());
    }

    #[test]
    fn only_the_trusted_proxy_names_the_client() {
        let proxy: IpAddr = "127.0.0.1".parse().unwrap();
        let config = ServeConfig {
            trusted_proxy: Some(proxy),
            ..ServeConfig::loopback(0)
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("6.6.6.6, 203.0.113.7, 127.0.0.1"),
        );
        let client: IpAddr = "203.0.113.7".parse().unwrap();
        assert_eq!(config.client_ip(proxy, &headers), client);

        let stranger: IpAddr = "198.51.100.9".parse().unwrap();
        assert_eq!(config.client_ip(stranger, &headers), stranger);
        assert_eq!(
            ServeConfig::loopback(0).client_ip(proxy, &headers),
            proxy,
            "no trusted proxy"
        );

        headers.insert("x-forwarded-for", HeaderValue::from_static("garbage"));
        headers.insert("x-real-ip", HeaderValue::from_static("203.0.113.8"));
        assert_eq!(
            config.client_ip(proxy, &headers),
            "203.0.113.8".parse::<IpAddr>().unwrap()
        );
        assert_eq!(config.client_ip(proxy, &HeaderMap::new()), proxy);
    }
}
