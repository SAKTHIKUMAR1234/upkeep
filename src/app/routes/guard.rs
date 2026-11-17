use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::{AppError, AppState};

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; frame-ancestors 'none'";
const STRICT_TRANSPORT_SECURITY: &str = "max-age=31536000";
/// On a website, the longest a request may take until its response starts. Event streams start
/// at once. Changes on a server run to the end after it: see `operations::to_the_end`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The `Host` values of the service: the loopback address or `localhost`, with its port.
pub fn is_allowed_host(host: &str, port: u16) -> bool {
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

/// The page's own origin. A request from another site, or from a name that resolves to
/// 127.0.0.1 (DNS rebinding), has another origin.
pub fn is_allowed_origin(origin: &str, port: u16) -> bool {
    origin
        .strip_prefix("http://")
        .is_some_and(|host| is_allowed_host(host, port))
}

/// On a website, the `Host` and `Origin` of the public URL and nothing else.
fn is_allowed_public(host: Option<&str>, origin: Option<&str>, public_origin: &str) -> bool {
    let public_host = public_origin.trim_start_matches("https://");
    host.is_some_and(|host| host.eq_ignore_ascii_case(public_host))
        && origin.is_none_or(|origin| origin.eq_ignore_ascii_case(public_origin))
}

fn is_allowed(headers: &HeaderMap, port: u16, public_origin: Option<&str>) -> bool {
    let text = |name| {
        headers
            .get(name)
            .map(|value: &HeaderValue| value.to_str().unwrap_or(""))
    };
    let (host, origin) = (text(header::HOST), text(header::ORIGIN));
    match public_origin {
        Some(public_origin) => is_allowed_public(host, origin, public_origin),
        None => {
            host.is_some_and(|host| is_allowed_host(host, port))
                && origin.is_none_or(|origin| is_allowed_origin(origin, port))
        }
    }
}

/// Refuses requests for another host or from another origin, ends website requests that take
/// too long, and adds the security headers to every response.
pub async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let is_api = request.uri().path().starts_with("/api/");
    let public_origin = state.serve.public_origin.as_deref();
    let mut response = if !is_allowed(request.headers(), state.port, public_origin) {
        AppError::ForeignOrigin.into_response()
    } else if public_origin.is_some() {
        tokio::time::timeout(REQUEST_TIMEOUT, next.run(request))
            .await
            .unwrap_or_else(|_| AppError::Timeout.into_response())
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    let set = |headers: &mut HeaderMap, name, value| {
        headers.insert(name, HeaderValue::from_static(value));
    };
    set(
        headers,
        header::CONTENT_SECURITY_POLICY,
        CONTENT_SECURITY_POLICY,
    );
    set(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(headers, header::REFERRER_POLICY, "no-referrer");
    set(headers, header::X_FRAME_OPTIONS, "DENY");
    if is_api {
        set(headers, header::CACHE_CONTROL, "no-store");
    }
    if public_origin.is_some() {
        set(
            headers,
            header::STRICT_TRANSPORT_SECURITY,
            STRICT_TRANSPORT_SECURITY,
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_loopback_host_and_origin_with_the_port_pass() {
        assert!(is_allowed_host("127.0.0.1:7000", 7000));
        assert!(is_allowed_host("localhost:7000", 7000));
        for host in [
            "127.0.0.1:7001",
            "127.0.0.1",
            "evil.example:7000",
            "127.0.0.1.evil:7000",
            "",
        ] {
            assert!(!is_allowed_host(host, 7000), "{host}");
        }
        assert!(is_allowed_origin("http://127.0.0.1:7000", 7000));
        for origin in [
            "https://127.0.0.1:7000",
            "http://evil.example:7000",
            "null",
            "http://127.0.0.1:7000/x",
        ] {
            assert!(!is_allowed_origin(origin, 7000), "{origin}");
        }
    }

    #[test]
    fn requests_need_a_good_host_and_no_foreign_origin() {
        let headers = |pairs: &[(header::HeaderName, &'static str)]| {
            let mut map = HeaderMap::new();
            for (name, value) in pairs {
                map.insert(name.clone(), HeaderValue::from_static(value));
            }
            map
        };
        assert!(is_allowed(
            &headers(&[(header::HOST, "127.0.0.1:80")]),
            80,
            None
        ));
        assert!(!is_allowed(&headers(&[]), 80, None));
        assert!(!is_allowed(
            &headers(&[
                (header::HOST, "127.0.0.1:80"),
                (header::ORIGIN, "http://rebind.example:80")
            ]),
            80,
            None
        ));
        assert!(is_allowed(
            &headers(&[
                (header::HOST, "localhost:80"),
                (header::ORIGIN, "http://localhost:80")
            ]),
            80,
            None
        ));
    }

    #[test]
    fn a_website_accepts_only_its_public_host_and_origin() {
        let public = "https://upkeep.example.com";
        assert!(is_allowed_public(Some("upkeep.example.com"), None, public));
        assert!(is_allowed_public(
            Some("Upkeep.Example.com"),
            Some("https://upkeep.example.com"),
            public
        ));
        for (host, origin) in [
            (None, None),
            (Some("127.0.0.1:7890"), None),
            (Some("localhost:7890"), None),
            (
                Some("upkeep.example.com"),
                Some("http://upkeep.example.com"),
            ),
            (Some("upkeep.example.com"), Some("https://evil.example")),
            (Some("upkeep.example.com"), Some("http://127.0.0.1:7890")),
            (Some("upkeep.example.com.evil"), None),
        ] {
            assert!(
                !is_allowed_public(host, origin, public),
                "{host:?} {origin:?}"
            );
        }
    }

    #[test]
    fn a_website_with_a_port_needs_the_port_in_the_host() {
        let public = "https://upkeep.example.com:8443";
        assert!(is_allowed_public(
            Some("upkeep.example.com:8443"),
            Some(public),
            public
        ));
        assert!(!is_allowed_public(Some("upkeep.example.com"), None, public));
        let nginx = include_str!("../../../packaging/nginx/upkeep.conf");
        assert!(
            nginx.contains("proxy_set_header Host $http_host;"),
            "the proxy must pass the port of the Host header"
        );
    }
}
