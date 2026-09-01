use axum::{
    body::Body,
    extract::Path,
    http::{Request, StatusCode},
    response::IntoResponse,
};
use reqwest::Method;
use serde_json::json;

#[cfg(target_os = "linux")]
const INSTALLER_SERVICE: &str = "kaonic-installer.service";
#[cfg(target_os = "linux")]
const INSTALLER_BINARY_PATH: &str = "/usr/bin/kaonic-installer";
const MAX_INSTALLER_BINARY_BYTES: usize = 64 * 1024 * 1024;

const UPDATE_BASE: &str = "http://127.0.0.1:8682";

pub async fn list_plugins() -> impl IntoResponse {
    proxy_get(format!("{UPDATE_BASE}/api/plugins")).await
}

pub async fn installer_version() -> impl IntoResponse {
    proxy_get(format!("{UPDATE_BASE}/api/plugins/installer-version")).await
}

pub async fn plugin_icon(Path(plugin_id): Path<String>) -> impl IntoResponse {
    proxy_bytes(format!("{UPDATE_BASE}/api/plugins/{plugin_id}/icon")).await
}

pub async fn install_plugin(req: Request<Body>) -> impl IntoResponse {
    proxy_request(
        Method::POST,
        format!("{UPDATE_BASE}/api/plugins/install"),
        Some(req),
    )
    .await
}

pub async fn upload_plugin(Path(plugin_id): Path<String>, req: Request<Body>) -> impl IntoResponse {
    proxy_request(
        Method::POST,
        format!("{UPDATE_BASE}/api/plugins/{plugin_id}/upload"),
        Some(req),
    )
    .await
}

pub async fn upgrade_installer_binary(req: Request<Body>) -> impl IntoResponse {
    let binary_bytes = match axum::body::to_bytes(req.into_body(), MAX_INSTALLER_BINARY_BYTES).await
    {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => {
            return (
                StatusCode::BAD_REQUEST,
                json_detail("replacement binary is empty"),
            )
                .into_response()
        }
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                json_detail(&format!("failed to read replacement binary: {err}")),
            )
                .into_response()
        }
    };

    match replace_installer_binary(binary_bytes.as_ref()) {
        Ok(detail) => (StatusCode::OK, json_detail(&detail)).into_response(),
        Err(detail) => (StatusCode::INTERNAL_SERVER_ERROR, json_detail(&detail)).into_response(),
    }
}

pub async fn start_plugin(Path(plugin_id): Path<String>) -> impl IntoResponse {
    proxy_request(
        Method::POST,
        format!("{UPDATE_BASE}/api/plugins/{plugin_id}/start"),
        None,
    )
    .await
}

pub async fn stop_plugin(Path(plugin_id): Path<String>) -> impl IntoResponse {
    proxy_request(
        Method::POST,
        format!("{UPDATE_BASE}/api/plugins/{plugin_id}/stop"),
        None,
    )
    .await
}

pub async fn restart_plugin(Path(plugin_id): Path<String>) -> impl IntoResponse {
    proxy_request(
        Method::POST,
        format!("{UPDATE_BASE}/api/plugins/{plugin_id}/restart"),
        None,
    )
    .await
}

pub async fn delete_plugin(Path(plugin_id): Path<String>) -> impl IntoResponse {
    proxy_request(
        Method::DELETE,
        format!("{UPDATE_BASE}/api/plugins/{plugin_id}"),
        None,
    )
    .await
}

/// Byte-preserving proxy — used for binary payloads such as plugin icons,
/// where `proxy_get`'s lossy `text()` conversion would corrupt the body.
async fn proxy_bytes(url: String) -> axum::response::Response {
    match reqwest::Client::new().get(&url).send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            match resp.bytes().await {
                Ok(body) => (
                    status,
                    [(axum::http::header::CONTENT_TYPE, content_type)],
                    body,
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::BAD_GATEWAY,
                    json_detail(&format!("read plugin icon body: {e}")),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            json_detail(&format!("kaonic-installer unreachable: {e}")),
        )
            .into_response(),
    }
}

async fn proxy_get(url: String) -> impl IntoResponse {
    match reqwest::Client::new().get(&url).send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let body = resp.text().await.unwrap_or_default();
            (status, body).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("{{\"detail\":\"kaonic-installer unreachable: {e}\"}}"),
        )
            .into_response(),
    }
}

fn json_detail(detail: &str) -> String {
    json!({ "detail": detail }).to_string()
}

#[cfg(target_os = "linux")]
fn replace_installer_binary(binary_bytes: &[u8]) -> Result<String, String> {
    let target_path = std::fs::canonicalize(INSTALLER_BINARY_PATH)
        .unwrap_or_else(|_| std::path::PathBuf::from(INSTALLER_BINARY_PATH));
    let parent = target_path.parent().ok_or_else(|| {
        format!(
            "installer binary path has no parent: {}",
            target_path.display()
        )
    })?;
    let temp_path = parent.join(".kaonic-installer.upgrade.tmp");
    std::fs::write(&temp_path, binary_bytes)
        .map_err(|err| format!("write replacement installer binary: {err}"))?;
    if let Err(err) = make_executable(&temp_path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    if let Err(err) = run_systemctl("stop", INSTALLER_SERVICE) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&temp_path, &target_path).map_err(|rename_err| {
        format!(
            "replace installer binary {}: {rename_err}",
            target_path.display()
        )
    }) {
        let _ = run_systemctl("start", INSTALLER_SERVICE);
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    if let Err(err) = run_systemctl("start", INSTALLER_SERVICE) {
        return Err(err);
    }
    Ok("kaonic-installer binary upgraded.".to_string())
}

#[cfg(not(target_os = "linux"))]
fn replace_installer_binary(_binary_bytes: &[u8]) -> Result<String, String> {
    Ok("Mock kaonic-installer binary upgrade completed.".to_string())
}

#[cfg(target_os = "linux")]
fn run_systemctl(action: &str, unit: &str) -> Result<(), String> {
    let output = std::process::Command::new("systemctl")
        .args([action, unit])
        .output()
        .map_err(|err| format!("failed to execute systemctl {action} {unit}: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    Err(if message.is_empty() {
        format!("systemctl {action} {unit} failed")
    } else {
        message
    })
}

#[cfg(target_os = "linux")]
fn make_executable(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)
        .map_err(|err| format!("read permissions for {}: {err}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
        .map_err(|err| format!("set executable permissions for {}: {err}", path.display()))
}

async fn proxy_request(
    method: Method,
    url: String,
    req: Option<Request<Body>>,
) -> axum::response::Response {
    let client = reqwest::Client::new();
    let mut builder = client.request(method, &url);

    if let Some(req) = req {
        let content_type = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let body_bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
            Ok(b) => b,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("failed to read body: {err}"),
                )
                    .into_response()
            }
        };

        if let Some(content_type) = content_type {
            builder = builder.header(reqwest::header::CONTENT_TYPE, content_type);
        }
        builder = builder.body(body_bytes.to_vec());
    }

    match builder.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let body = resp.text().await.unwrap_or_default();
            (status, body).into_response()
        }
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            format!("{{\"detail\":\"kaonic-installer unreachable: {err}\"}}"),
        )
            .into_response(),
    }
}
