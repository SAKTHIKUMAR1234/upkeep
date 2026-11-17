use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("cannot read token file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("token file {0} must not be readable by group or others (chmod 600)")]
    Permissions(String),
    #[error("token file {0} is empty")]
    Empty(String),
}

/// The bearer token of the TCP API, read from an owner-only file.
#[derive(Clone)]
pub struct Token(Arc<str>);

impl Token {
    pub fn load(path: &Path) -> Result<Self, TokenError> {
        let display = path.display().to_string();
        let read_error = |source| TokenError::Read {
            path: display.clone(),
            source,
        };
        let mode = std::fs::metadata(path)
            .map_err(read_error)?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(TokenError::Permissions(display));
        }

        let token = std::fs::read_to_string(path)
            .map_err(read_error)?
            .trim()
            .to_owned();
        if token.is_empty() {
            return Err(TokenError::Empty(display));
        }
        Ok(Self(token.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn matches(&self, presented: &str) -> bool {
        constant_time_eq(&self.0, presented)
    }
}

/// Compares two secrets in a time that does not depend on where they differ.
pub fn constant_time_eq(expected: &str, presented: &str) -> bool {
    let difference = expected
        .bytes()
        .zip(presented.bytes())
        .fold(0u8, |acc, (left, right)| acc | (left ^ right));
    difference == 0 && expected.len() == presented.len()
}

/// Rejects TCP requests without the right `Authorization: Bearer <token>` header.
pub async fn require_token(State(token): State<Token>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match presented {
        Some(presented) if token.matches(presented) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_token_files_readable_by_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "secret\n").unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Token::load(&path),
            Err(TokenError::Permissions(_))
        ));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let token = Token::load(&path).unwrap();
        assert!(token.matches("secret"));
        assert!(!token.matches("secre") && !token.matches("secret2"));
    }
}
