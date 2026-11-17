use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::{AppError, AppState};

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; frame-ancestors 'none'";

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

fn is_allowed(headers: &HeaderMap, port: u16) -> bool {
    let text = |name| {
        headers
            .get(name)
            .map(|value: &HeaderValue| value.to_str().unwrap_or(""))
    };
    let host_ok = text(header::HOST).is_some_and(|host| is_allowed_host(host, port));
    let origin_ok = text(header::ORIGIN).is_none_or(|origin| is_allowed_origin(origin, port));
    host_ok && origin_ok
}

/// Refuses requests for another host or from another origin, and adds the security headers to
/// every response.
pub async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let is_api = request.uri().path().starts_with("/api/");
    let mut response = if is_allowed(request.headers(), state.port) {
        next.run(request).await
    } else {
        AppError::ForeignOrigin.into_response()
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
        assert!(is_allowed(&headers(&[(header::HOST, "127.0.0.1:80")]), 80));
        assert!(!is_allowed(&headers(&[]), 80));
        assert!(!is_allowed(
            &headers(&[
                (header::HOST, "127.0.0.1:80"),
                (header::ORIGIN, "http://rebind.example:80")
            ]),
            80
        ));
        assert!(is_allowed(
            &headers(&[
                (header::HOST, "localhost:80"),
                (header::ORIGIN, "http://localhost:80")
            ]),
            80
        ));
    }
}
