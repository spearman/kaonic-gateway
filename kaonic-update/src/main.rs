use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use tower_http::cors::{Any, CorsLayer};

mod update;

use update::{apply_update, get_version, validate_on_boot, Target};

const META_DIR: &str = "/etc/kaonic";
const BIN_DIR: &str = "/usr/bin";
const MAX_UPLOAD_BYTES: usize = 256 * 1024 * 1024; // 256 MiB

/// kaonic-update: standalone OTA update server for kaonic packages.
#[derive(Parser)]
#[command(name = "kaonic-update", version)]
struct Cmd {
    /// Address to listen on
    #[arg(long, default_value = "0.0.0.0:8682")]
    pub listen: SocketAddr,
}

#[derive(Clone)]
struct AppState {
    commd: Arc<Target>,
    gateway: Arc<Target>,
}

// Safety: Target only holds PathBuf / &'static str — no interior mutability.
unsafe impl Send for AppState {}
unsafe impl Sync for AppState {}

fn make_targets(meta_dir: &str, bin_dir: &str) -> (Arc<Target>, Arc<Target>) {
    let meta = PathBuf::from(meta_dir);
    let bin = PathBuf::from(bin_dir);
    (
        Arc::new(Target {
            name: "commd",
            bin_path: bin.join("kaonic-commd"),
            service: "kaonic-commd.service",
            meta_dir: meta.clone(),
        }),
        Arc::new(Target {
            name: "gateway",
            bin_path: bin.join("kaonic-gateway"),
            service: "kaonic-gateway.service",
            meta_dir: meta,
        }),
    )
}

#[tokio::main]
async fn main() {
    env_logger::Builder::new()
        .parse_filters("info,kaonic_update=debug")
        .parse_default_env()
        .init();

    let cmd = Cmd::parse();

    let (commd, gateway) = make_targets(META_DIR, BIN_DIR);

    // Validate installed binaries against stored hashes on boot
    validate_on_boot(&commd);
    validate_on_boot(&gateway);

    let state = AppState { commd, gateway };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
        .allow_credentials(false);

    let app = Router::new()
        .route("/api/update/:target/version", get(handle_version))
        .route("/api/update/:target/upload", post(handle_upload))
        .layer(cors)
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(cmd.listen)
        .await
        .expect("failed to bind");
    log::info!("kaonic-update listening on http://{}", cmd.listen);
    axum::serve(listener, app).await.expect("server error");
}

// ── handlers ─────────────────────────────────────────────────────────────────

async fn handle_version(
    Path(target): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match resolve_target(&state, &target) {
        Some(t) => Json(get_version(&t)).into_response(),
        None => (StatusCode::NOT_FOUND, "unknown target").into_response(),
    }
}

async fn handle_upload(
    Path(target): Path<String>,
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let t = match resolve_target(&state, &target) {
        Some(t) => t,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"detail":"unknown target"})),
            )
                .into_response()
        }
    };

    // Read the first multipart field as the ZIP bytes
    let zip_bytes: Bytes = loop {
        match multipart.next_field().await {
            Ok(Some(field)) => match field.bytes().await {
                Ok(b) => break b,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"detail": format!("read error: {e}")})),
                    )
                        .into_response()
                }
            },
            Ok(None) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"detail": "no file uploaded"})),
                )
                    .into_response()
            }
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"detail": format!("multipart error: {e}")})),
                )
                    .into_response()
            }
        }
    };

    // Run the blocking OTA logic on a threadpool thread so we don't block Tokio
    let result = tokio::task::spawn_blocking(move || apply_update(&t, &zip_bytes))
        .await
        .unwrap_or_else(|e| Err(format!("task panic: {e}")));

    match result {
        Ok(msg) => Json(serde_json::json!({"detail": msg})).into_response(),
        Err(msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"detail": msg})),
        )
            .into_response(),
    }
}

fn resolve_target(state: &AppState, name: &str) -> Option<Arc<Target>> {
    match name {
        "commd" => Some(state.commd.clone()),
        "gateway" => Some(state.gateway.clone()),
        _ => None,
    }
}
