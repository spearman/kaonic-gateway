use axum::extract::{Form, Path, State};
use axum::http::{header, StatusCode};
use axum::{response::IntoResponse, Json};
use kaonic_gateway::app_types::{
    FrameStatsDto, NetworkPortStatusDto, NetworkSnapshotDto, ReticulumSnapshotDto, RxFrameDto,
    ServiceStatusDto, SystemStatusDto, WsInterfacesDto, WsReticulumSnapshotDto,
};
use kaonic_gateway::audio::{
    AudioCardSnapshot, AudioControlSnapshot, AudioControlState, AudioError, AudioOutput,
};
use kaonic_gateway::config::GatewayConfig;
use kaonic_gateway::local_https;
use kaonic_gateway::network::{read_interface_ipv4, NetworkError, WifiAntenna, WifiMode};
use kaonic_gateway::radio::{transmit_test_frame, RadioModuleConfig};
use kaonic_gateway::settings::normalize_codename;
use kaonic_gateway::system_metrics::{
    is_gateway_service_unit, read_cpu_freq_mhz, read_cpu_percent_async, read_fs_mb,
    read_gateway_services, read_mem_mb, read_os_details,
};
use kaonic_vpn::VpnSnapshot;
use reticulum::hash::AddressHash;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
#[cfg(target_os = "linux")]
use std::process::Command;
use std::time::{Duration, Instant};

use super::AppState;

// ── /api/info ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct InfoResponse {
    pub serial: String,
}

pub async fn get_info(State(state): State<AppState>) -> Json<InfoResponse> {
    Json(InfoResponse {
        serial: state.serial.clone(),
    })
}

pub async fn get_serial(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; charset=utf-8".to_string(),
        )],
        state.serial.clone(),
    )
}

pub async fn get_system_rootca() -> Result<impl IntoResponse, (StatusCode, String)> {
    let path = local_https::root_ca_cert_path();
    let bytes = tokio::fs::read(&path).await.map_err(|err| {
        let status = if err.kind() == std::io::ErrorKind::NotFound {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (
            status,
            format!(
                "failed to read root CA certificate {}: {err}",
                path.display()
            ),
        )
    })?;

    Ok((
        [
            (
                header::CONTENT_TYPE,
                "application/x-x509-ca-cert".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"{}\"",
                    local_https::ROOT_CA_DOWNLOAD_NAME
                ),
            ),
        ],
        bytes,
    ))
}

/// `GET /api/settings` — return the full gateway config.
pub async fn get_settings(
    State(state): State<AppState>,
) -> Result<Json<GatewayConfig>, StatusCode> {
    let s = state
        .settings
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    s.load_config().map(Json).map_err(|err| {
        log::error!("failed to load settings: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

/// `PUT /api/settings` — replace the full gateway config.
pub async fn put_settings(
    State(state): State<AppState>,
    Json(config): Json<GatewayConfig>,
) -> StatusCode {
    {
        let s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(err) = s.save_config(&config) {
            log::error!("failed to save settings: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    }

    if let Some(vpn) = &state.vpn {
        if let Err(err) = vpn
            .replace_peer_policy(config.allow_all_peers, config.peers.clone())
            .await
        {
            log::error!("failed to apply VPN peer policy from settings: {err}");
            return StatusCode::BAD_REQUEST;
        }
        vpn.replace_advertised_routes(config.advertised_routes.clone())
            .await;
    }

    StatusCode::NO_CONTENT
}

/// `GET /api/settings/radio/:module` — return config for one RF module (0 or 1).
pub async fn get_radio(
    State(state): State<AppState>,
    Path(module): Path<usize>,
) -> Result<Json<RadioModuleConfig>, StatusCode> {
    let s = state
        .settings
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    s.load_config()
        .map_err(|err| {
            log::error!("failed to load radio settings: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
        .and_then(|c| {
            c.radio
                .module_configs
                .get(module)
                .cloned()
                .map(Json)
                .ok_or(StatusCode::NOT_FOUND)
        })
}

/// `PUT /api/settings/radio/:module` — save config for one RF module and apply to hardware.
pub async fn put_radio(
    State(state): State<AppState>,
    Path(module): Path<usize>,
    Json(cfg): Json<RadioModuleConfig>,
) -> StatusCode {
    log::info!(
        "put_radio: module={} radio_config={:?} modulation={:?}",
        module,
        cfg.radio_config,
        cfg.modulation
    );

    // Captured before `cfg.radio_config` is moved into the apply calls below.
    let band = cfg.band();

    let save_result = {
        let s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
        s.save_module_config(module, &cfg)
    };
    if let Err(err) = save_result {
        log::error!("failed to save radio settings for module {module}: {err}");
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    log::info!("put_radio: module={module} saved to DB");

    if let Some(client) = state.radio_client.clone() {
        let mut client = client.lock().await;
        match client.set_radio_config(module, cfg.radio_config).await {
            Ok(_) => log::info!("put_radio: radio_config applied to module {module}"),
            Err(e) => {
                log::error!("put_radio: set_radio_config failed for module {module}: {e:?}")
            }
        }
        match client.set_modulation(module, cfg.modulation).await {
            Ok(_) => log::info!("put_radio: modulation applied to module {module}"),
            Err(e) => log::error!("put_radio: set_modulation failed for module {module}: {e:?}"),
        }
        match client.set_accelerator(module, cfg.accelerator).await {
            Ok(_) => log::info!("put_radio: accelerator applied to module {module}"),
            Err(e) => log::error!("put_radio: set_accelerator failed for module {module}: {e:?}"),
        }
        // Sub-GHz has no antenna switch on this board — it is always external.
        if band == radio_common::RadioBand::Band24 {
            match client
                .set_antenna(module, radio_common::RadioBand::Band24, cfg.antenna)
                .await
            {
                Ok(_) => log::info!("put_radio: antenna applied to module {module}"),
                Err(e) => log::error!("put_radio: set_antenna failed for module {module}: {e:?}"),
            }
        }
    } else {
        log::info!("put_radio: running without radio backend, saved config only");
    }

    StatusCode::NO_CONTENT
}

pub async fn post_radio_test(
    State(state): State<AppState>,
    Path(module): Path<usize>,
    Json(request): Json<RadioTestRequest>,
) -> Result<Json<RadioTestResponse>, (StatusCode, String)> {
    if module > 1 {
        return Err((
            StatusCode::NOT_FOUND,
            format!("radio module {module} not found"),
        ));
    }

    let message = request.message.trim().to_string();
    if message.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "message is required".into()));
    }
    if message.chars().count() > 2047 {
        return Err((
            StatusCode::BAD_REQUEST,
            "message exceeds 2047 characters".into(),
        ));
    }

    transmit_test_frame(
        state.radio_client.clone(),
        state.radio_tx_observer.clone(),
        module,
        message.as_bytes(),
    )
    .await
    .map_err(|err| (StatusCode::SERVICE_UNAVAILABLE, err))?;

    Ok(Json(RadioTestResponse {
        status: format!(
            "Sent test frame on {}",
            if module == 0 { "Radio A" } else { "Radio B" }
        ),
    }))
}

pub async fn post_system_reboot() -> Result<Json<SystemActionResponse>, (StatusCode, String)> {
    let status = request_system_reboot().map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(Json(SystemActionResponse { status }))
}

pub async fn post_system_codename(
    State(state): State<AppState>,
    Json(request): Json<SetSystemCodenameRequest>,
) -> Result<Json<SystemCodenameResponse>, (StatusCode, String)> {
    let codename = normalize_codename(&request.codename)
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;

    {
        let settings = state.settings.lock().map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "settings lock poisoned".to_string(),
            )
        })?;
        settings.save_codename(&codename).map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to save codename: {err}"),
            )
        })?;
    }

    Ok(Json(SystemCodenameResponse {
        status: "Codename updated".into(),
        codename,
    }))
}

pub async fn post_system_service_restart(
    Json(request): Json<ServiceActionRequest>,
) -> Result<Json<SystemActionResponse>, (StatusCode, String)> {
    if !is_gateway_service_unit(&request.unit) {
        return Err((StatusCode::BAD_REQUEST, "unsupported service".into()));
    }

    let status = request_service_restart(&request.unit)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err))?;
    Ok(Json(SystemActionResponse { status }))
}

pub async fn put_vpn_routes(
    State(state): State<AppState>,
    Json(request): Json<PutVpnRoutesRequest>,
) -> Result<Json<SystemActionResponse>, (StatusCode, String)> {
    let routes = request
        .routes
        .iter()
        .map(|route| {
            route.trim().parse::<cidr::Ipv4Cidr>().map_err(|err| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("invalid route '{route}': {err}"),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    {
        let settings = state.settings.lock().map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "settings lock poisoned".into(),
            )
        })?;
        let mut config = settings.load_config().map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load config: {err}"),
            )
        })?;
        config.advertised_routes = routes.clone();
        settings.save_config(&config).map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to save config: {err}"),
            )
        })?;
    }

    if let Some(vpn) = &state.vpn {
        vpn.replace_advertised_routes(routes).await;
    }

    Ok(Json(SystemActionResponse {
        status: "VPN advertised routes updated".into(),
    }))
}

pub async fn put_vpn_access(
    State(state): State<AppState>,
    Json(request): Json<PutVpnAccessRequest>,
) -> Result<Json<VpnAccessResponse>, (StatusCode, String)> {
    let peers = normalize_vpn_peer_hashes(&request.peers)?;

    {
        let settings = state.settings.lock().map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "settings lock poisoned".into(),
            )
        })?;
        let mut config = settings.load_config().map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load config: {err}"),
            )
        })?;
        config.allow_all_peers = request.allow_all_peers;
        config.peers = peers.clone();
        settings.save_config(&config).map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to save config: {err}"),
            )
        })?;
    }

    if let Some(vpn) = &state.vpn {
        vpn.replace_peer_policy(request.allow_all_peers, peers.clone())
            .await
            .map_err(|err| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("failed to apply VPN peer policy: {err}"),
                )
            })?;
    }

    Ok(Json(VpnAccessResponse {
        status: if request.allow_all_peers {
            "VPN access updated (allow all peers)".into()
        } else {
            "VPN access updated (allowlist only)".into()
        },
        allow_all_peers: request.allow_all_peers,
        peers,
    }))
}

pub async fn post_vpn_ping(
    Json(request): Json<VpnPingRequest>,
) -> Result<Json<VpnPingResponse>, (StatusCode, String)> {
    let address = request.address.trim().parse::<Ipv4Addr>().map_err(|err| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid IPv4 address '{}': {err}", request.address.trim()),
        )
    })?;

    let result = request_vpn_ping(address)
        .await
        .map_err(|err| (StatusCode::BAD_GATEWAY, err))?;
    Ok(Json(VpnPingResponse {
        ok: result.ok,
        latency: result.latency,
    }))
}

pub async fn post_vpn_speed_test(
    State(state): State<AppState>,
    Json(request): Json<VpnSpeedTestRequest>,
) -> Result<Json<VpnSpeedTestResponse>, (StatusCode, String)> {
    let address = request.address.trim().parse::<Ipv4Addr>().map_err(|err| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid IPv4 address '{}': {err}", request.address.trim()),
        )
    })?;

    let vpn = state
        .vpn
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "vpn unavailable".into()))?;
    let peer_ip = address.to_string();
    let snapshot = vpn.snapshot().await;
    if !snapshot
        .peers
        .iter()
        .any(|peer| peer.tunnel_ip.as_deref() == Some(peer_ip.as_str()))
    {
        return Err((StatusCode::NOT_FOUND, "peer tunnel IP not found".into()));
    }

    let url = format!("https://{peer_ip}/");
    let root_ca_pem = std::fs::read(local_https::root_ca_cert_path()).map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read local Root CA certificate: {err}"),
        )
    })?;
    let root_ca = reqwest::Certificate::from_pem(&root_ca_pem).map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to parse local Root CA certificate: {err}"),
        )
    })?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(15))
        .add_root_certificate(root_ca)
        .build()
        .map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to build speed-test client: {err}"),
            )
        })?;

    let started = Instant::now();
    let response = client
        .get(&url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|err| {
            (
                StatusCode::BAD_GATEWAY,
                format!("speed-test request failed: {err}"),
            )
        })?;

    if !response.status().is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("speed-test returned {}", response.status()),
        ));
    }

    let bytes = response.bytes().await.map_err(|err| {
        (
            StatusCode::BAD_GATEWAY,
            format!("failed to read speed-test body: {err}"),
        )
    })?;
    let duration_ms = started.elapsed().as_millis().max(1) as u64;
    let bytes_len = bytes.len() as u64;
    let bps = ((bytes_len as u128) * 8 * 1000 / duration_ms as u128) as u64;

    log::info!(
        "vpn speed-test peer={} bytes={} duration_ms={} bps={}",
        peer_ip,
        bytes_len,
        duration_ms,
        bps
    );

    Ok(Json(VpnSpeedTestResponse {
        ok: true,
        bytes: bytes_len,
        duration_ms,
        bps,
    }))
}

#[derive(Serialize)]
pub struct VpnRoutesResponse {
    pub tunnel_ip: Option<String>,
    /// Routes exported to peers. Entries use "exported/prefix -> local/prefix" when
    /// NAT aliasing is active, or just "net/prefix" when no aliasing is needed.
    pub exported_routes: Vec<String>,
    /// Alias subnets announced by remote peers that are currently installed as kernel routes.
    /// Add these on laptops/hosts behind this device:
    ///   ip route add <network> via <kaonic-lan-ip>
    pub remote_installed: Vec<String>,
}

pub async fn get_vpn_routes(State(state): State<AppState>) -> Json<VpnRoutesResponse> {
    let vpn = match &state.vpn {
        Some(vpn) => vpn.snapshot().await,
        None => kaonic_vpn::VpnSnapshot::default(),
    };
    let remote_installed = vpn
        .remote_routes
        .iter()
        .filter(|r| r.installed)
        .map(|r| r.network.clone())
        .collect();
    Json(VpnRoutesResponse {
        tunnel_ip: vpn.local_tunnel_ip,
        exported_routes: vpn.local_routes,
        remote_installed,
    })
}

// ── /api/audio ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PutAudioRequest {
    pub volume: u8,
    pub muted: bool,
}

#[derive(Deserialize)]
pub struct RadioTestRequest {
    pub message: String,
}

#[derive(Deserialize)]
pub struct PutVpnRoutesRequest {
    pub routes: Vec<String>,
}

#[derive(Deserialize)]
pub struct PutVpnAccessRequest {
    pub allow_all_peers: bool,
    pub peers: Vec<String>,
}

#[derive(Deserialize)]
pub struct VpnPingRequest {
    pub address: String,
}

#[derive(Deserialize)]
pub struct VpnSpeedTestRequest {
    pub address: String,
}

#[derive(Deserialize)]
pub struct ServiceActionRequest {
    pub unit: String,
}

#[derive(Deserialize)]
pub struct SetSystemCodenameRequest {
    pub codename: String,
}

#[derive(Serialize)]
pub struct AudioSaveResponse {
    pub status: String,
}

#[derive(Serialize)]
pub struct RadioTestResponse {
    pub status: String,
}

#[derive(Serialize)]
pub struct SystemActionResponse {
    pub status: String,
}

#[derive(Serialize)]
pub struct SystemCodenameResponse {
    pub status: String,
    pub codename: String,
}

#[derive(Serialize)]
pub struct VpnAccessResponse {
    pub status: String,
    pub allow_all_peers: bool,
    pub peers: Vec<String>,
}

#[derive(Serialize)]
pub struct VpnPingResponse {
    pub ok: bool,
    pub latency: Option<String>,
}

#[derive(Serialize)]
pub struct VpnSpeedTestResponse {
    pub ok: bool,
    pub bytes: u64,
    pub duration_ms: u64,
    pub bps: u64,
}

struct PingAttempt {
    ok: bool,
    latency: Option<String>,
}

fn normalize_vpn_peer_hashes(peers: &[String]) -> Result<Vec<String>, (StatusCode, String)> {
    let mut peers = peers
        .iter()
        .map(|peer| {
            let raw = peer.trim();
            if raw.is_empty() {
                return Err((StatusCode::BAD_REQUEST, "peer hash is required".into()));
            }
            AddressHash::new_from_hex_string(raw)
                .map(|hash| hash.to_hex_string())
                .map_err(|err| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("invalid peer hash '{raw}': {err:?}"),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    peers.sort();
    peers.dedup();
    Ok(peers)
}

pub async fn get_audio(
    State(state): State<AppState>,
    Path(output): Path<String>,
) -> Result<Json<AudioControlSnapshot>, StatusCode> {
    let output = AudioOutput::parse(&output).ok_or(StatusCode::NOT_FOUND)?;

    state.audio.read(output).await.map(Json).map_err(|err| {
        log::error!("failed to read {output:?} audio state: {err}");
        map_audio_error(&err)
    })
}

pub async fn get_audio_cards(
    State(state): State<AppState>,
) -> Result<Json<Vec<AudioCardSnapshot>>, StatusCode> {
    state.audio.list_cards().await.map(Json).map_err(|err| {
        log::error!("failed to list audio cards: {err}");
        map_audio_error(&err)
    })
}

pub async fn get_audio_control(
    State(state): State<AppState>,
    Path((card_id, output)): Path<(usize, String)>,
) -> Result<Json<AudioControlSnapshot>, StatusCode> {
    let output = AudioOutput::parse(&output).ok_or(StatusCode::NOT_FOUND)?;

    state
        .audio
        .read_control(card_id, output)
        .await
        .map(Json)
        .map_err(|err| {
            log::error!("failed to read card {card_id} {output:?} audio state: {err}");
            map_audio_error(&err)
        })
}

pub async fn put_audio(
    State(state): State<AppState>,
    Path(output): Path<String>,
    Json(request): Json<PutAudioRequest>,
) -> Result<Json<AudioControlSnapshot>, StatusCode> {
    let output = AudioOutput::parse(&output).ok_or(StatusCode::NOT_FOUND)?;
    let next = AudioControlState {
        volume: request.volume,
        muted: request.muted,
    };

    state
        .audio
        .write(output, next)
        .await
        .map(Json)
        .map_err(|err| {
            log::error!("failed to update {output:?} audio state: {err}");
            map_audio_error(&err)
        })
}

pub async fn put_audio_control(
    State(state): State<AppState>,
    Path((card_id, output)): Path<(usize, String)>,
    Json(request): Json<PutAudioRequest>,
) -> Result<Json<AudioControlSnapshot>, StatusCode> {
    let output = AudioOutput::parse(&output).ok_or(StatusCode::NOT_FOUND)?;
    let next = AudioControlState {
        volume: request.volume,
        muted: request.muted,
    };

    state
        .audio
        .write_control(card_id, output, next)
        .await
        .map(Json)
        .map_err(|err| {
            log::error!("failed to update card {card_id} {output:?} audio state: {err}");
            map_audio_error(&err)
        })
}

pub async fn post_audio_control_test(
    State(state): State<AppState>,
    Path((card_id, output)): Path<(usize, String)>,
) -> Result<Json<AudioControlSnapshot>, StatusCode> {
    let output = AudioOutput::parse(&output).ok_or(StatusCode::NOT_FOUND)?;

    state
        .audio
        .test_control(card_id, output)
        .await
        .map(Json)
        .map_err(|err| {
            log::error!("failed to play test sample on card {card_id} {output:?}: {err}");
            map_audio_error(&err)
        })
}

pub async fn post_audio_card_save(
    State(state): State<AppState>,
    Path(card_id): Path<usize>,
) -> Result<Json<AudioSaveResponse>, StatusCode> {
    state
        .audio
        .save_card(card_id)
        .await
        .map(|status| Json(AudioSaveResponse { status }))
        .map_err(|err| {
            log::error!("failed to persist audio settings for card {card_id}: {err}");
            map_audio_error(&err)
        })
}

fn map_audio_error(err: &AudioError) -> StatusCode {
    match err {
        AudioError::InvalidVolume(_) => StatusCode::BAD_REQUEST,
        AudioError::NotFound(_) => StatusCode::NOT_FOUND,
        AudioError::StatePoisoned(_)
        | AudioError::TaskJoin(_)
        | AudioError::CommandIo { .. }
        | AudioError::CommandFailed { .. }
        | AudioError::UnexpectedOutput(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(target_os = "linux")]
fn request_system_reboot() -> Result<String, String> {
    let output = Command::new("systemctl")
        .args(["--no-block", "reboot"])
        .output()
        .map_err(|err| format!("failed to execute systemctl reboot: {err}"))?;

    if output.status.success() {
        return Ok("Reboot requested".into());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    Err(if message.is_empty() {
        "systemctl reboot failed".into()
    } else {
        message
    })
}

#[cfg(not(target_os = "linux"))]
fn request_system_reboot() -> Result<String, String> {
    Ok("Mock reboot requested".into())
}

#[cfg(target_os = "linux")]
fn request_service_restart(unit: &str) -> Result<String, String> {
    let output = Command::new("systemctl")
        .args(["--no-block", "restart", unit])
        .output()
        .map_err(|err| format!("failed to execute systemctl restart {unit}: {err}"))?;

    if output.status.success() {
        return Ok(format!("Restart requested for {unit}"));
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    Err(if message.is_empty() {
        format!("systemctl restart {unit} failed")
    } else {
        message
    })
}

#[cfg(not(target_os = "linux"))]
fn request_service_restart(unit: &str) -> Result<String, String> {
    Ok(format!("Mock restart requested for {unit}"))
}

#[cfg(target_os = "linux")]
async fn request_vpn_ping(address: Ipv4Addr) -> Result<PingAttempt, String> {
    let output = tokio::process::Command::new("ping")
        .args(["-n", "-c", "1", "-W", "3", &address.to_string()])
        .output()
        .await
        .map_err(|err| format!("failed to execute ping {address}: {err}"))?;

    if output.status.success() {
        return Ok(PingAttempt {
            ok: true,
            latency: parse_ping_latency(&output.stdout),
        });
    }

    Ok(PingAttempt {
        ok: false,
        latency: None,
    })
}

#[cfg(not(target_os = "linux"))]
async fn request_vpn_ping(_address: Ipv4Addr) -> Result<PingAttempt, String> {
    Ok(PingAttempt {
        ok: true,
        latency: Some("1.0 ms".into()),
    })
}

fn parse_ping_latency(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .find(|line| line.contains("time=") || line.contains("time<"))
        .and_then(|line| {
            let start = line
                .find("time=")
                .map(|idx| idx + "time=".len())
                .or_else(|| line.find("time").map(|idx| idx + "time".len()))?;
            let tail = &line[start..];
            let end = tail.find(" ms").or_else(|| tail.find("ms"))?;
            let value = tail[..end].trim();
            if value.is_empty() {
                return None;
            }
            Some(format!("{value} ms"))
        })
}

#[cfg(test)]
mod tests {
    use super::parse_ping_latency;

    #[test]
    fn parses_ping_latency_from_output() {
        let output = b"64 bytes from 10.20.78.77: icmp_seq=1 ttl=64 time=12.34 ms\n";
        assert_eq!(parse_ping_latency(output), Some("12.34 ms".into()));
    }

    #[test]
    fn missing_ping_latency_returns_none() {
        let output = b"1 packets transmitted, 0 packets received, 100% packet loss\n";
        assert_eq!(parse_ping_latency(output), None);
    }

    #[test]
    fn parses_busybox_sub_millisecond_latency() {
        let output = b"64 bytes from 10.20.78.77: seq=0 ttl=64 time<1 ms\n";
        assert_eq!(parse_ping_latency(output), Some("<1 ms".into()));
    }
}

// ── /network/wifi actions ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct WifiModeForm {
    pub mode: String,
}

#[derive(Deserialize)]
pub struct WifiAntennaForm {
    pub antenna: String,
}

#[derive(Deserialize)]
pub struct WifiConnectForm {
    pub ssid: String,
    pub psk: String,
}

pub async fn post_wifi_mode(
    State(state): State<AppState>,
    Form(form): Form<WifiModeForm>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mode = WifiMode::parse(&form.mode).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            NetworkError::InvalidMode(form.mode).to_string(),
        )
    })?;

    state
        .network
        .set_wifi_mode(mode)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_network_error)
}

pub async fn post_wifi_antenna(
    State(state): State<AppState>,
    Form(form): Form<WifiAntennaForm>,
) -> Result<StatusCode, (StatusCode, String)> {
    let antenna = WifiAntenna::parse(&form.antenna).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            NetworkError::InvalidAntenna(form.antenna).to_string(),
        )
    })?;

    state
        .network
        .set_wifi_antenna(antenna)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_network_error)
}

pub async fn post_wifi_connect(
    State(state): State<AppState>,
    Form(form): Form<WifiConnectForm>,
) -> Result<StatusCode, (StatusCode, String)> {
    state
        .network
        .connect_wifi(&form.ssid, &form.psk)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_network_error)
}

pub async fn get_network_snapshot(
    State(state): State<AppState>,
) -> Result<Json<NetworkSnapshotDto>, (StatusCode, String)> {
    state
        .network
        .snapshot()
        .await
        .map(Json)
        .map_err(map_network_error)
}

fn map_network_error(err: NetworkError) -> (StatusCode, String) {
    let status = match err {
        NetworkError::InvalidMode(_)
        | NetworkError::InvalidAntenna(_)
        | NetworkError::InvalidSsid
        | NetworkError::InvalidPsk
        | NetworkError::MissingStaConfig => StatusCode::BAD_REQUEST,
        NetworkError::StatePoisoned
        | NetworkError::TaskJoin(_)
        | NetworkError::ModeFileWrite(_)
        | NetworkError::CommandIo { .. }
        | NetworkError::CommandFailed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, err.to_string())
}

// ── /api/status ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct StatusResponse {
    vpn_hash: String,
    wlan0_ip: Option<String>,
    usb0_ip: Option<String>,
    network_ports: Vec<NetworkPortStatusDto>,
    system: SystemStatusDto,
    services: Vec<ServiceStatusDto>,
    radio_modules: Vec<RadioModuleConfig>,
    reticulum: ReticulumSnapshotDto,
    vpn: VpnSnapshot,
    rx_frames: [Vec<RxFrameDto>; 2],
    frame_stats: [FrameStatsDto; 2],
}

/// `GET /api/status` — live gateway status: system resources, VPN hash, radio config.
pub async fn get_status(State(state): State<AppState>) -> Json<StatusResponse> {
    Json(build_status(&state).await)
}

/// Build a `StatusResponse` from shared application state. Used by both the REST handler
/// and the WebSocket streamer.
pub async fn build_status(state: &AppState) -> StatusResponse {
    let radio_modules = state
        .settings
        .lock()
        .ok()
        .and_then(|s| s.load_config().ok())
        .map(|c| c.radio.module_configs.to_vec())
        .unwrap_or_default();
    let services = build_services().await;
    let network_ports = build_network_ports(state, &services);
    let interfaces = build_ws_interfaces();
    let system = build_system_status().await;
    let rx_frames = build_all_radio_frames(state).await;
    let frame_stats = build_all_frame_stats(state);
    let reticulum = build_reticulum_snapshot(state).await;
    let vpn = build_vpn_snapshot(state).await;

    StatusResponse {
        vpn_hash: state.vpn_hash.clone(),
        wlan0_ip: interfaces.wlan0_ip,
        usb0_ip: interfaces.usb0_ip,
        network_ports,
        system,
        services,
        radio_modules,
        reticulum,
        vpn,
        rx_frames,
        frame_stats,
    }
}

pub fn build_ws_interfaces() -> WsInterfacesDto {
    WsInterfacesDto {
        wlan0_ip: read_interface_ipv4("wlan0"),
        usb0_ip: read_interface_ipv4("usb0"),
    }
}

pub async fn build_services() -> Vec<ServiceStatusDto> {
    read_gateway_services().await
}

pub fn build_network_ports(
    state: &AppState,
    services: &[ServiceStatusDto],
) -> Vec<NetworkPortStatusDto> {
    state.network_ports(services)
}

pub async fn build_system_status() -> SystemStatusDto {
    let (ram_used_mb, ram_total_mb) = read_mem_mb();
    let (fs_free_mb, fs_total_mb) = read_fs_mb();
    SystemStatusDto {
        cpu_percent: read_cpu_percent_async().await,
        cpu_freq_mhz: read_cpu_freq_mhz(),
        ram_used_mb,
        ram_total_mb,
        fs_free_mb,
        fs_total_mb,
        os_details: read_os_details(),
    }
}

pub fn build_frame_stats(state: &AppState, module: usize) -> FrameStatsDto {
    use std::sync::atomic::Ordering;

    let module = module.min(1);
    let rx_bytes = state.frame_stats[module].rx_bytes.load(Ordering::Relaxed);
    let tx_bytes = state.frame_stats[module].tx_bytes.load(Ordering::Relaxed);
    let (rx_bps, tx_bps) = state.frame_stats[module].rates(rx_bytes, tx_bytes);
    FrameStatsDto {
        rx_frames: state.frame_stats[module].rx_frames.load(Ordering::Relaxed),
        rx_bytes,
        rx_bps,
        tx_frames: state.frame_stats[module].tx_frames.load(Ordering::Relaxed),
        tx_bytes,
        tx_bps,
        last_rssi: if state.frame_stats[module].rx_frames.load(Ordering::Relaxed) > 0 {
            Some(state.frame_stats[module].last_rssi.load(Ordering::Relaxed) as i8)
        } else {
            None
        },
    }
}

pub fn build_all_frame_stats(state: &AppState) -> [FrameStatsDto; 2] {
    [build_frame_stats(state, 0), build_frame_stats(state, 1)]
}

pub async fn build_radio_frames(state: &AppState, module: usize) -> Vec<RxFrameDto> {
    let module = module.min(1);
    state.rx_buffers[module]
        .lock()
        .await
        .iter()
        .cloned()
        .collect()
}

pub async fn build_all_radio_frames(state: &AppState) -> [Vec<RxFrameDto>; 2] {
    [
        build_radio_frames(state, 0).await,
        build_radio_frames(state, 1).await,
    ]
}

pub async fn build_reticulum_snapshot(state: &AppState) -> ReticulumSnapshotDto {
    state.reticulum.snapshot().await
}

pub async fn build_ws_reticulum_snapshot(state: &AppState) -> WsReticulumSnapshotDto {
    let snapshot = state.reticulum.snapshot().await;
    WsReticulumSnapshotDto {
        interface_stats: snapshot.interface_stats,
        incoming_links: snapshot.incoming_links,
        outgoing_links: snapshot.outgoing_links,
    }
}

pub async fn build_vpn_snapshot(state: &AppState) -> VpnSnapshot {
    match &state.vpn {
        Some(vpn) => vpn.snapshot().await,
        None => VpnSnapshot::default(),
    }
}
