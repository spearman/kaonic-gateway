pub mod frames;
pub mod pages;

use std::sync::OnceLock;

use axum::{Router, routing::get};

static SERIAL: OnceLock<String> = OnceLock::new();

/// Call once at startup before serving requests.
pub fn set_serial(serial: String) {
    let _ = SERIAL.set(serial);
}

pub fn serial() -> &'static str {
    SERIAL.get().map(|s| s.as_str()).unwrap_or("unknown")
}

/// Returns the dashboard Axum router (stateless — all data ops go through the API).
pub fn router() -> Router {
    Router::new()
        .route("/", get(frames::get_dashboard))
        .route("/settings", get(pages::get_settings))
        .route("/update", get(pages::get_update))
        .route("/mavlink", get(pages::get_mavlink))
}
