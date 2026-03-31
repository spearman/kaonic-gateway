mod handlers;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{Router, routing::get};

use kaonic_gateway::atak::BridgeMetrics;
use kaonic_gateway::settings::Settings;
use kaonic_gateway::radio::SharedRadioClient;

/// Shared settings handle passed to all API handlers.
pub type SharedSettings = Arc<Mutex<Settings>>;

/// Shared application state for all HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    pub settings: SharedSettings,
    pub atak_metrics: Vec<Arc<BridgeMetrics>>,
    pub vpn_hash: String,
    pub radio_client: SharedRadioClient,
}

/// Start the combined HTTP server (JSON API + dashboard UI). Runs until the process exits.
pub async fn serve(state: AppState, addr: SocketAddr) {
    let api = Router::new()
        .route("/api/settings",                get(handlers::get_settings).put(handlers::put_settings))
        .route("/api/settings/radio/:module",  get(handlers::get_radio).put(handlers::put_radio))
        .route("/api/status",                  get(handlers::get_status))
        .route("/api/mavlink/status",          get(handlers::get_mavlink_status))
        .route("/api/mavlink/control",         axum::routing::post(handlers::post_mavlink_control))
        .with_state(state);

    let app = api.merge(kaonic_dashboard::router());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind HTTP listener");

    log::info!("HTTP server listening on http://{addr}");
    axum::serve(listener, app).await.expect("HTTP server error");
}
