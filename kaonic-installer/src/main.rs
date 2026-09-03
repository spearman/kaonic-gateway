#![recursion_limit = "512"]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use clap::Parser;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;

mod installer;
mod plugins;

use installer::{validate_on_boot, Target};

const META_DIR: &str = "/etc/kaonic";
const BIN_DIR: &str = "/usr/bin";
const PLUGINS_DIR: &str = "/etc/kaonic/plugins";
const PLUGINS_DB: &str = "/etc/kaonic/plugins/kaonic-plugins.db";
const SYSTEMD_DIR: &str = "/etc/systemd/system";
const MAX_UPLOAD_BYTES: usize = 256 * 1024 * 1024; // 256 MiB
const SYSTEMD_REFRESH_INTERVAL: Duration = Duration::from_secs(15);

/// kaonic-installer: standalone installer server for kaonic packages and plugins.
#[derive(Parser)]
#[command(name = "kaonic-installer", version)]
struct Cmd {
    /// Address to listen on
    #[arg(long, default_value = "0.0.0.0:8682")]
    pub listen: SocketAddr,
}

#[derive(Clone)]
struct AppState {
    core_plugins: Vec<plugins::CorePluginSpec>,
    plugins_root: PathBuf,
    plugins_db: PathBuf,
    systemd_dir: PathBuf,
    cert_path: PathBuf,
    systemd_cache: Arc<RwLock<HashMap<String, plugins::PluginSystemdStatus>>>,
}

fn make_core_targets(meta_dir: &str, bin_dir: &str, plugins_dir: &str) -> Vec<Arc<Target>> {
    let meta = PathBuf::from(meta_dir);
    let bin = PathBuf::from(bin_dir);
    let plugins = PathBuf::from(plugins_dir);
    vec![
        Arc::new(Target {
            name: "commd",
            symlink_path: bin.join("kaonic-commd"),
            binary_path: plugins.join("kaonic-commd/current/kaonic-commd"),
            service: "kaonic-commd.service",
            meta_dir: meta.clone(),
        }),
        Arc::new(Target {
            name: "gateway",
            symlink_path: bin.join("kaonic-gateway"),
            binary_path: plugins.join("kaonic-gateway/current/kaonic-gateway"),
            service: "kaonic-gateway.service",
            meta_dir: meta.clone(),
        }),
        Arc::new(Target {
            name: "factory",
            symlink_path: bin.join("kaonic-factory"),
            binary_path: plugins.join("kaonic-factory/current/kaonic-factory"),
            service: "kaonic-factory.service",
            meta_dir: meta,
        }),
    ]
}

#[tokio::main]
async fn main() {
    env_logger::Builder::new()
        .parse_filters("info,kaonic_installer=debug")
        .parse_default_env()
        .init();

    let cmd = Cmd::parse();

    let core_targets = make_core_targets(META_DIR, BIN_DIR, PLUGINS_DIR);

    // Validate installed binaries against stored hashes on boot
    for target in &core_targets {
        validate_on_boot(target);
    }

    let systemd_cache = Arc::new(RwLock::new(HashMap::new()));
    let plugins_root = PathBuf::from(PLUGINS_DIR);
    let state = AppState {
        core_plugins: core_targets
            .iter()
            .cloned()
            .map(|target| plugins::CorePluginSpec::new(target, &plugins_root))
            .collect(),
        plugins_root,
        plugins_db: PathBuf::from(PLUGINS_DB),
        systemd_dir: PathBuf::from(SYSTEMD_DIR),
        cert_path: PathBuf::from(META_DIR).join("beechat-ota.pub.pem"),
        systemd_cache,
    };

    if let Err(err) = plugins::initialize_store(
        &state.plugins_root,
        &state.plugins_db,
        &state.systemd_dir,
        Some(&state.cert_path),
        &state.core_plugins,
    ) {
        log::error!("failed to initialize plugin store: {err}");
    } else if let Err(err) = plugins::log_boot_inventory(&state.plugins_root, &state.plugins_db) {
        log::error!("failed to log plugin boot inventory: {err}");
    }
    if let Err(err) = refresh_systemd_cache(&state).await {
        log::error!("failed to refresh cached plugin systemd state: {err}");
    }
    tokio::spawn(run_systemd_refresh_loop(state.clone()));

    let app = Router::new()
        .route("/api/plugins", get(handle_plugins_list))
        .route("/api/services", get(handle_services_list))
        .route(
            "/api/plugins/installer-version",
            get(handle_installer_version),
        )
        .route("/api/plugins/install", post(handle_plugin_install))
        .route("/api/plugins/:plugin_id/icon", get(handle_plugin_icon))
        .route("/api/plugins/:plugin_id/upload", post(handle_plugin_upload))
        .route("/api/plugins/:plugin_id/start", post(handle_plugin_start))
        .route("/api/plugins/:plugin_id/stop", post(handle_plugin_stop))
        .route(
            "/api/plugins/:plugin_id/restart",
            post(handle_plugin_restart),
        )
        .route("/api/plugins/:plugin_id", delete(handle_plugin_delete))
        .layer(CorsLayer::permissive())
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .layer(RequestBodyLimitLayer::new(MAX_UPLOAD_BYTES))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(cmd.listen)
        .await
        .expect("failed to bind");
    log::info!("kaonic-installer listening on http://{}", cmd.listen);
    axum::serve(listener, app).await.expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request};
    use tower::util::ServiceExt;

    #[test]
    fn core_targets_include_factory() {
        let targets = make_core_targets("/etc/kaonic", "/usr/bin", "/etc/kaonic/plugins");
        let factory = targets
            .iter()
            .find(|target| target.name == "factory")
            .expect("factory target registered");

        assert_eq!(factory.service, "kaonic-factory.service");
        assert_eq!(
            factory.symlink_path,
            PathBuf::from("/usr/bin/kaonic-factory")
        );
        assert_eq!(
            factory.binary_path,
            PathBuf::from("/etc/kaonic/plugins/kaonic-factory/current/kaonic-factory")
        );
    }

    #[tokio::test]
    async fn multipart_uploads_above_default_limit_are_accepted() {
        async fn upload_echo(multipart: Multipart) -> impl IntoResponse {
            match read_upload_bytes(multipart).await {
                Ok(bytes) => (StatusCode::OK, bytes.len().to_string()).into_response(),
                Err(response) => response,
            }
        }

        let app = Router::new()
            .route("/upload", post(upload_echo))
            .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
            .layer(RequestBodyLimitLayer::new(MAX_UPLOAD_BYTES));

        let boundary = "X-BOUNDARY";
        let payload = vec![b'a'; (2 * 1024 * 1024) + 4096];
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"plugin.zip\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: application/zip\r\n\r\n");
        body.extend_from_slice(&payload);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .expect("multipart request"),
            )
            .await
            .expect("upload response");

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response bytes");
        assert_eq!(
            String::from_utf8(body.to_vec()).expect("utf8 body"),
            payload.len().to_string()
        );
    }
}

// ── handlers ─────────────────────────────────────────────────────────────────

async fn handle_plugins_list(State(state): State<AppState>) -> impl IntoResponse {
    log::debug!("listing plugins");
    let plugins_db = state.plugins_db.clone();
    let systemd_cache = state.systemd_cache.read().await.clone();
    match tokio::task::spawn_blocking(move || {
        plugins::list_plugins_with_cached_status(&plugins_db, &systemd_cache)
    })
    .await
    {
        Ok(Ok(records)) => {
            log::debug!("plugin list returned {} records", records.len());
            Json(records).into_response()
        }
        Ok(Err(err)) => plugin_error_response("list plugins", err),
        Err(err) => {
            log::error!("plugin list task panic: {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"detail": format!("plugin task panic: {err}")})),
            )
                .into_response()
        }
    }
}

/// Cached systemd status for every unit the installer tracks — plugin units and
/// the core gateway units. Served from the background refresh, so a caller never
/// causes a `systemctl` fork of its own.
async fn handle_services_list(State(state): State<AppState>) -> impl IntoResponse {
    let cache = state.systemd_cache.read().await.clone();
    Json(cache).into_response()
}

async fn handle_installer_version() -> impl IntoResponse {
    Json(serde_json::json!({ "version": env!("CARGO_PKG_VERSION") })).into_response()
}

async fn handle_plugin_install(
    State(state): State<AppState>,
    multipart: Multipart,
) -> impl IntoResponse {
    log::info!("received plugin install upload");
    let zip_bytes = match read_upload_bytes(multipart).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    log::info!("starting plugin install payload_bytes={}", zip_bytes.len());
    let plugins_root = state.plugins_root.clone();
    let plugins_db = state.plugins_db.clone();
    let systemd_dir = state.systemd_dir.clone();
    let cert_path = state.cert_path.clone();
    let core_plugins = state.core_plugins.clone();

    match tokio::task::spawn_blocking(move || {
        plugins::install_plugin(
            &plugins_root,
            &plugins_db,
            &systemd_dir,
            &cert_path,
            &core_plugins,
            &zip_bytes,
        )
    })
    .await
    {
        Ok(Ok(response)) => {
            log::info!("plugin install completed detail={}", response.detail);
            if let Err(err) = refresh_systemd_cache(&state).await {
                log::warn!("failed to refresh cached plugin systemd state after install: {err}");
            }
            Json(response).into_response()
        }
        Ok(Err(err)) => plugin_error_response("install plugin", err),
        Err(err) => {
            log::error!("plugin install task panic: {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"detail": format!("plugin task panic: {err}")})),
            )
                .into_response()
        }
    }
}

async fn handle_plugin_upload(
    Path(plugin_id): Path<String>,
    State(state): State<AppState>,
    multipart: Multipart,
) -> impl IntoResponse {
    log::info!("received plugin update upload for plugin_id={plugin_id}");
    let zip_bytes = match read_upload_bytes(multipart).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    log::info!(
        "starting plugin update for plugin_id={} payload_bytes={}",
        plugin_id,
        zip_bytes.len()
    );
    let plugins_root = state.plugins_root.clone();
    let plugins_db = state.plugins_db.clone();
    let systemd_dir = state.systemd_dir.clone();
    let cert_path = state.cert_path.clone();
    let core_plugins = state.core_plugins.clone();
    let plugin_id_for_task = plugin_id.clone();

    match tokio::task::spawn_blocking(move || {
        plugins::upload_plugin_update(
            &plugins_root,
            &plugins_db,
            &systemd_dir,
            &cert_path,
            &core_plugins,
            &plugin_id_for_task,
            &zip_bytes,
        )
    })
    .await
    {
        Ok(Ok(response)) => {
            log::info!(
                "plugin update completed for plugin_id={} detail={}",
                plugin_id,
                response.detail
            );
            if let Err(err) = refresh_systemd_cache(&state).await {
                log::warn!("failed to refresh cached plugin systemd state after update: {err}");
            }
            Json(response).into_response()
        }
        Ok(Err(err)) => plugin_error_response(&format!("update plugin {plugin_id}"), err),
        Err(err) => {
            log::error!("plugin update task panic for plugin_id={plugin_id}: {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"detail": format!("plugin task panic: {err}")})),
            )
                .into_response()
        }
    }
}

async fn handle_plugin_icon(
    Path(plugin_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let plugins_db = state.plugins_db.clone();
    let lookup_id = plugin_id.clone();
    match tokio::task::spawn_blocking(move || plugins::read_plugin_icon(&plugins_db, &lookup_id))
        .await
    {
        Ok(Ok(icon)) => (
            [
                (header::CONTENT_TYPE, icon.content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            icon.bytes,
        )
            .into_response(),
        Ok(Err(err)) => plugin_error_response("read plugin icon", err),
        Err(err) => {
            log::error!("plugin icon task panic: {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"detail": format!("plugin task panic: {err}")})),
            )
                .into_response()
        }
    }
}

async fn handle_plugin_start(
    Path(plugin_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    handle_plugin_action(plugin_id, state, plugins::PluginAction::Start).await
}

async fn handle_plugin_stop(
    Path(plugin_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    handle_plugin_action(plugin_id, state, plugins::PluginAction::Stop).await
}

async fn handle_plugin_restart(
    Path(plugin_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    handle_plugin_action(plugin_id, state, plugins::PluginAction::Restart).await
}

async fn handle_plugin_delete(
    Path(plugin_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    log::info!("received delete request for plugin_id={plugin_id}");
    let plugins_root = state.plugins_root.clone();
    let plugins_db = state.plugins_db.clone();
    let systemd_dir = state.systemd_dir.clone();
    let plugin_id_for_task = plugin_id.clone();
    match tokio::task::spawn_blocking(move || {
        plugins::delete_plugin(
            &plugins_root,
            &plugins_db,
            &systemd_dir,
            &plugin_id_for_task,
        )
    })
    .await
    {
        Ok(Ok(response)) => {
            log::info!(
                "delete completed for plugin_id={} detail={}",
                plugin_id,
                response.detail
            );
            if let Err(err) = refresh_systemd_cache(&state).await {
                log::warn!("failed to refresh cached plugin systemd state after delete: {err}");
            }
            Json(response).into_response()
        }
        Ok(Err(err)) => plugin_error_response(&format!("delete plugin {plugin_id}"), err),
        Err(err) => {
            log::error!("plugin delete task panic for plugin_id={plugin_id}: {err}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"detail": format!("plugin task panic: {err}")})),
            )
                .into_response()
        }
    }
}

async fn handle_plugin_action(
    plugin_id: String,
    state: AppState,
    action: plugins::PluginAction,
) -> axum::response::Response {
    let plugins_db = state.plugins_db.clone();
    let action_name = action.as_str().to_string();
    log::info!(
        "received plugin action request action={} plugin_id={}",
        action_name,
        plugin_id
    );
    let plugin_id_for_task = plugin_id.clone();
    match tokio::task::spawn_blocking(move || {
        plugins::control_plugin(&plugins_db, &plugin_id_for_task, action)
    })
    .await
    {
        Ok(Ok(response)) => {
            log::info!(
                "plugin action completed action={} detail={}",
                action_name,
                response.detail
            );
            if let Err(err) = refresh_systemd_cache(&state).await {
                log::warn!(
                    "failed to refresh cached plugin systemd state after action={} plugin_id={}: {err}",
                    action_name,
                    plugin_id
                );
            }
            Json(response).into_response()
        }
        Ok(Err(err)) => {
            plugin_error_response(&format!("{} plugin {}", action_name, plugin_id), err)
        }
        Err(err) => {
            log::error!(
                "plugin action task panic action={} plugin_id={}: {err}",
                action_name,
                plugin_id
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"detail": format!("plugin task panic: {err}")})),
            )
                .into_response()
        }
    }
}

async fn run_systemd_refresh_loop(state: AppState) {
    let mut interval = tokio::time::interval(SYSTEMD_REFRESH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    loop {
        interval.tick().await;
        if let Err(err) = refresh_systemd_cache(&state).await {
            log::warn!("background plugin systemd refresh failed: {err}");
        }
    }
}

async fn refresh_systemd_cache(state: &AppState) -> Result<(), plugins::PluginError> {
    let plugins_db = state.plugins_db.clone();

    let core_services = state
        .core_plugins
        .iter()
        .map(|spec| spec.target.service.to_string())
        .collect::<Vec<_>>();

    let cache = tokio::task::spawn_blocking(move || {
        plugins::refresh_systemd_status_cache(&plugins_db, &core_services)
    })
            .await
            .map_err(|err| plugins::PluginError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                detail: format!("plugin cache task panic: {err}"),
            })??;

    *state.systemd_cache.write().await = cache;
    Ok(())
}

async fn read_upload_bytes(mut multipart: Multipart) -> Result<Bytes, axum::response::Response> {
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let field_name = field.name().map(str::to_string);
                let file_name = field.file_name().map(str::to_string);
                match field.bytes().await {
                    Ok(bytes) => {
                        log::debug!(
                            "received multipart field name={:?} file_name={:?} bytes={}",
                            field_name,
                            file_name,
                            bytes.len()
                        );
                        return Ok(bytes);
                    }
                    Err(err) => {
                        log::warn!(
                            "failed reading multipart field name={:?} file_name={:?}: {}",
                            field_name,
                            file_name,
                            err
                        );
                        return Err((
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({"detail": format!("read error: {err}")})),
                        )
                            .into_response());
                    }
                }
            }
            Ok(None) => {
                log::warn!("multipart upload missing file field");
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"detail": "no file uploaded"})),
                )
                    .into_response());
            }
            Err(err) => {
                log::warn!("multipart parsing error: {}", err);
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"detail": format!("multipart error: {err}")})),
                )
                    .into_response());
            }
        }
    }
}

fn plugin_error_response(context: &str, err: plugins::PluginError) -> axum::response::Response {
    if err.status.is_server_error() {
        log::error!(
            "{} failed status={} detail={}",
            context,
            err.status,
            err.detail
        );
    } else {
        log::warn!(
            "{} failed status={} detail={}",
            context,
            err.status,
            err.detail
        );
    }
    (
        err.status,
        Json(serde_json::json!({
            "detail": err.detail
        })),
    )
        .into_response()
}
