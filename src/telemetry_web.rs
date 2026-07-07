//! Fleet telemetry web dashboard — issue #84.
//!
//! Serves a self-contained HTML dashboard (`web/fleet.html`).
//!
//! Intended to be nested into the gateway's router at integration time:
//!
//! ```ignore
//! let fleet = crate::telemetry_web::routes();
//! gateway_router = gateway_router.merge(fleet); // or .nest("/", fleet)
//! ```
//!
//! `GET /fleet/data.json` is served from `gateway.rs` because it needs access
//! to Docker container labels as well as telemetry state.

use axum::{Router, response::Html, routing::get};

/// The HTML page, compiled in via `include_str!` — one self-contained file,
/// inline CSS + vanilla JS, no external requests (offline box).
const FLEET_HTML: &str = include_str!("../web/fleet.html");

/// Build a standalone axum router serving the fleet dashboard HTML.
///
/// `GET /`                → redirect to /fleet (a bare localhost:9801 in a
///                          browser should land somewhere, not 404).
/// `GET /fleet`           → the HTML dashboard.
pub fn routes() -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { axum::response::Redirect::to("/fleet") }),
        )
        .route("/fleet", get(fleet_html))
}

async fn fleet_html() -> Html<&'static str> {
    Html(FLEET_HTML)
}
