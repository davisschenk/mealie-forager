use axum::{
    http::header,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

use crate::state::AppState;

fn asset(content_type: &'static str, body: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

macro_rules! route {
    ($router:expr, $path:literal, $file:literal, $type:literal) => {
        $router.route(
            $path,
            get(|| async { asset($type, include_bytes!(concat!("../web/", $file))) }),
        )
    };
}

pub fn router() -> Router<AppState> {
    let r = Router::new();
    let r = route!(r, "/", "index.html", "text/html; charset=utf-8");
    let r = route!(r, "/app.js", "app.js", "text/javascript; charset=utf-8");
    let r = route!(r, "/styles.css", "styles.css", "text/css; charset=utf-8");
    let r = route!(
        r,
        "/manifest.webmanifest",
        "manifest.webmanifest",
        "application/manifest+json"
    );
    let r = route!(r, "/icon.svg", "icon.svg", "image/svg+xml");
    let r = route!(r, "/icon-192.png", "icon-192.png", "image/png");
    let r = route!(r, "/icon-512.png", "icon-512.png", "image/png");
    route!(
        r,
        "/apple-touch-icon.png",
        "apple-touch-icon.png",
        "image/png"
    )
}
