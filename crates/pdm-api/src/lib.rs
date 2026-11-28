//! PDM HTTP API (axum): auth, catalog, files/versions, locks, search, audit, portal.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::path::Path;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{get, get_service};
use tower::Layer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

mod auth;
mod branding;
mod design_link;
mod error;
mod mail;
mod onshape;
mod portal;
mod reading_font;
mod routes;
mod session;
mod state;

pub use auth::{Authed, auth_middleware};
pub use design_link::DesignLinks;
pub use error::AppError;
pub use routes::version_info;
pub use session::SessionLimits;
pub use state::AppState;

/// Resolve the portal static directory. Uses PDM_PORTAL_DIR if set, otherwise the
/// `static/` directory next to this crate's source. `None` if it does not exist.
fn portal_dir() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("PDM_PORTAL_DIR") {
        let p = Path::new(&dir).to_path_buf();
        return if p.is_dir() { Some(p) } else { None };
    }
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
    if p.is_dir() { Some(p) } else { None }
}

/// Build the full API router. The embedded single-file portal is always served at
/// `/`; if a `static/` directory is present it is mounted at `/static/*` and its
/// `index.html` (if any) takes precedence over the embedded one.
pub fn build_router(state: AppState) -> Router {
    let api = routes::api_router();

    let mut router: Router<AppState> = Router::new();
    router = router.merge(api);
    router = router.merge(branding::router());

    if let Some(dir) = portal_dir() {
        // `no-cache` makes browsers revalidate on every load so a redeployed
        // portal (new app.js / style.css) is not hidden behind a stale cache.
        let no_cache = || {
            tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-cache"),
            )
        };
        let index = dir.join("index.html");
        if index.is_file() {
            router = router.route("/", get_service(no_cache().layer(ServeFile::new(index))));
        } else {
            router = router.route("/", get(portal::default_index));
        }
        router = router.nest_service("/static", no_cache().layer(ServeDir::new(dir)));
    } else {
        router = router.route("/", get(portal::default_index));
    }

    // Axum's default request-body limit is 2 MiB; raise it to the configured
    // upload cap so large files are not rejected before the handlers run.
    let body_limit = usize::try_from(state.max_upload).unwrap_or(usize::MAX);
    router = router
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(TraceLayer::new_for_http());

    router.with_state(state)
}
