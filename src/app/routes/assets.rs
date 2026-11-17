use axum::extract::Path;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};

use super::AppError;

const INDEX: &str = include_str!("../assets/index.html");

/// The files of the page: (name, content type, content).
const FILES: [(&str, &str, &str); 3] = [
    (
        "app.css",
        "text/css; charset=utf-8",
        include_str!("../assets/app.css"),
    ),
    (
        "app.js",
        "text/javascript; charset=utf-8",
        include_str!("../assets/app.js"),
    ),
    (
        "icon.svg",
        "image/svg+xml",
        include_str!("../assets/icon.svg"),
    ),
];

pub async fn index() -> Html<&'static str> {
    Html(INDEX)
}

pub async fn file(Path(name): Path<String>) -> Result<Response, AppError> {
    let (_, content_type, content) = FILES
        .iter()
        .find(|(file, _, _)| *file == name)
        .ok_or(AppError::NotFound("file"))?;
    Ok((
        [
            (header::CONTENT_TYPE, *content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        *content,
    )
        .into_response())
}
