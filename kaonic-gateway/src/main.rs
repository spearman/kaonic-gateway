#![recursion_limit = "512"]

/// kaonic-gateway: Reticulum VPN gateway using kaonic radio hardware
mod http;

use std::path::Path;
use std::process;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use crc32fast::hash as crc32_hash;
use env_logger;
use http::{AppState, SharedSettings};
use kaonic_gateway::gateway_reticulum::GatewayReticulum;
use kaonic_gateway::local_https;
use kaonic_gateway::radio::{
    attach_radio_interface, connect_radio_client, SharedErrorObserver, SharedRadioClient,
    SharedTxObserver,
};
use kaonic_gateway::settings::Settings;
use kaonic_vpn::{VpnConfig, VpnRuntime};
use log;
use reticulum::identity::PrivateIdentity;
use reticulum::transport::{TimerConfig, Transport, TransportConfig};
use std::sync::Mutex;
use tokio;
use tokio_util::sync::CancellationToken;

const DEFAULT_DB_PATH: &str = "kaonic-gateway.db";

fn frame_preview(data: &[u8]) -> String {
    data.iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn frame_ascii_preview(data: &[u8]) -> String {
    data.iter()
        .take(16)
        .map(|b| match b {
            0x20..=0x7e => char::from(*b),
            _ => '.',
        })
        .collect()
}

fn frame_crc32(data: &[u8]) -> String {
    format!("{:08x}", crc32_hash(data))
}

fn unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// kaonic-gateway: VPN over Reticulum using the kaonic radio hardware.
#[derive(Parser)]
#[command(name = "kaonic-gateway", version)]
pub struct Command {
    /// kaonic-ctrl server UDP address (overrides config / default 192.168.10.1:9090)
    #[arg(short = 'a', long)]
    pub kaonic_ctrl_server: Option<std::net::SocketAddr>,
    /// Address to bind the HTTP server — dashboard + API (default: 0.0.0.0:80)
    #[arg(long, default_value = "0.0.0.0:80")]
    pub http_addr: std::net::SocketAddr,
    /// Address to bind the HTTPS server (default: same IP as --http-addr, port 443)
    #[arg(long)]
    pub https_addr: Option<std::net::SocketAddr>,
}

fn main() -> Result<(), process::ExitCode> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024) // 8 MB — Leptos SSR view trees are deep
        .build()
        .expect("tokio runtime")
        .block_on(async_main())
}

async fn async_main() -> Result<(), process::ExitCode> {
    let cmd = Command::parse();
    let https_addr = cmd
        .https_addr
        .unwrap_or_else(|| std::net::SocketAddr::new(cmd.http_addr.ip(), 443));

    let db_path =
        std::env::var("KAONIC_GATEWAY_DB_PATH").unwrap_or_else(|_| DEFAULT_DB_PATH.to_string());

    let settings: SharedSettings =
        Arc::new(Mutex::new(Settings::open(&db_path).unwrap_or_else(|err| {
            eprintln!("failed to open database {db_path}: {err}");
            process::exit(1);
        })));

    let config = settings
        .lock()
        .unwrap()
        .load_config()
        .unwrap_or_else(|err| {
            eprintln!("failed to load config from database: {err}");
            process::exit(1);
        });

    let codename = settings
        .lock()
        .unwrap()
        .load_or_create_codename()
        .unwrap_or_else(|err| {
            eprintln!("failed to load codename from database: {err}");
            process::exit(1);
        });

    env_logger::Builder::new()
        .parse_filters(
            "warn,kaonic_gateway=trace,kaonic_vpn=debug,kaonic_reticulum=warn,reticulum=warn",
        )
        .parse_default_env()
        .init();

    let webapp_only = should_run_webapp_only();

    let serial = std::fs::read_to_string("/etc/kaonic/kaonic_serial")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    log::info!("device serial: {serial}");
    log::info!("system codename: {codename}");
    local_https::install_rustls_crypto_provider();
    if let Err(err) = local_https::ensure_root_ca_files() {
        log::warn!("failed to prepare local Root CA files: {err}");
    }
    if let Ok(current_dir) = std::env::current_dir() {
        if let Err(err) = local_https::ensure_plugin_tls_files(&current_dir, "kaonic-gateway") {
            log::warn!("failed to prepare gateway plugin TLS files: {err}");
        }
    }

    if webapp_only {
        log::info!("starting in webapp-only mode; skipping radio and transport initialization");
        let reticulum = Arc::new(GatewayReticulum::new());
        let app_state = AppState::new(
            settings.clone(),
            "webapp-only".into(),
            None,
            cmd.kaonic_ctrl_server
                .unwrap_or_else(|| "192.168.10.1:9090".parse().unwrap()),
            cmd.http_addr,
            None,
            None,
            reticulum,
            serial,
        );

        tokio::spawn(http::serve(app_state, cmd.http_addr, https_addr));
        shutdown_signal(CancellationToken::new()).await;
        return Ok(());
    }

    let seed = settings
        .lock()
        .unwrap()
        .load_or_create_seed()
        .unwrap_or_else(|err| {
            log::error!("failed to load/create identity seed: {err}");
            process::exit(1);
        });
    let id = PrivateIdentity::new_from_name(&seed);
    let vpn_hash = id.address_hash().to_hex_string();
    log::info!("Reticulum identity ready: {vpn_hash}");

    let default_server: std::net::SocketAddr = "192.168.10.1:9090".parse().unwrap();
    let default_listen: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
    let server_addr = cmd.kaonic_ctrl_server.unwrap_or(default_server);
    log::info!("connecting to kaonic-ctrl at {server_addr}");
    let radio_client: SharedRadioClient = connect_radio_client(default_listen, server_addr)
        .await
        .map_err(|e| {
        log::error!("kaonic-ctrl connect error: {e:?}");
        process::ExitCode::FAILURE
    })?;

    let (tx_events_tx, mut tx_events_rx) =
        tokio::sync::mpsc::unbounded_channel::<(usize, Vec<u8>)>();
    let radio_tx_observer: SharedTxObserver = Arc::new(move |module, payload| {
        let _ = tx_events_tx.send((module, payload.to_vec()));
    });

    let mut transport_cfg = TransportConfig::new("kaonic-gateway", &id, true);
    transport_cfg.set_retransmit(true);
    transport_cfg.set_timer_config(TimerConfig {
        in_link_stale: Duration::from_secs(30),
        in_link_close: Duration::from_secs(15),
        out_link_restart: Duration::from_secs(45),
        out_link_stale: Duration::from_secs(30),
        out_link_close: Duration::from_secs(15),
        out_link_repeat: Duration::from_secs(10),
        out_link_keep: Duration::from_secs(5),
        ..TimerConfig::default()
    });
    // Lossy radio interfaces can miss several keep-alive round-trips under
    // load, so keep links alive longer before marking them stale and restart
    // stale out-links after 45 s instead of forcing a full re-handshake sooner.
    transport_cfg.set_restart_outlinks(true);
    let transport = Arc::new(tokio::sync::Mutex::new(Transport::new(transport_cfg)));
    let reticulum = Arc::new(GatewayReticulum::new());
    reticulum.attach(transport.clone()).await;
    let reticulum_error_observer: SharedErrorObserver = Arc::new({
        let reticulum = reticulum.clone();
        move |module, kind| {
            let reticulum = reticulum.clone();
            tokio::spawn(async move {
                reticulum.record_interface_error(module, kind).await;
            });
        }
    });

    attach_radio_interface(
        &transport,
        radio_client.clone(),
        &config.radio,
        0,
        Some(radio_tx_observer.clone()),
        Some(reticulum_error_observer),
    )
    .await
    .map_err(|err| {
        log::error!("radio interface attach error: {err:?}");
        process::ExitCode::FAILURE
    })?;

    // Shared cancellation token — cancelled on Ctrl-C / SIGTERM.
    let cancel = CancellationToken::new();
    let vpn = match VpnRuntime::start(
        VpnConfig {
            network: config.network,
            allow_all_peers: config.allow_all_peers,
            peers: config.peers.clone(),
            advertised_routes: config.advertised_routes.clone(),
            announce_freq_secs: config.announce_freq_secs,
        },
        transport.clone(),
        id.clone(),
        cancel.clone(),
    )
    .await
    {
        Ok(vpn) => Some(vpn),
        Err(err) => {
            log::error!("vpn runtime start failed: {err}");
            None
        }
    };

    let app_state = AppState::new(
        settings.clone(),
        vpn_hash,
        vpn,
        server_addr,
        cmd.http_addr,
        Some(radio_tx_observer.clone()),
        Some(radio_client.clone()),
        reticulum,
        serial,
    );

    // Spawn frame listener — fills per-module ring buffers used by the WS feed.
    {
        use http::ws::publish_radio_frames;
        use kaonic_gateway::app_types::RxFrameDto;
        use kaonic_gateway::state::RX_BUF_SIZE;
        use std::sync::atomic::Ordering;
        use tokio::sync::broadcast::error::RecvError;

        let ws_state = app_state.clone();
        let rx_bufs = app_state.rx_buffers.clone();
        let frame_stats = app_state.frame_stats.clone();
        let mut rx = radio_client.lock().await.module_receive();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    recv = rx.recv() => match recv {
                        Ok(recv) => {
                            let module = recv.module.min(1);
                            let entry = RxFrameDto {
                                module,
                                direction: "rx".into(),
                                rssi: recv.rssi,
                                len: recv.frame.len,
                                hex: frame_preview(recv.frame.as_slice()),
                                ascii: frame_ascii_preview(recv.frame.as_slice()),
                                crc32: frame_crc32(recv.frame.as_slice()),
                                ts: unix_timestamp_secs(),
                            };
                            frame_stats[module]
                                .rx_frames
                                .fetch_add(1, Ordering::Relaxed);
                            frame_stats[module]
                                .rx_bytes
                                .fetch_add(recv.frame.len as u64, Ordering::Relaxed);
                            frame_stats[module]
                                .last_rssi
                                .store(recv.rssi as i32, Ordering::Relaxed);
                            let mut buf = rx_bufs[module].lock().await;
                            buf.push_front(entry);
                            buf.truncate(RX_BUF_SIZE);
                            let frames = buf.iter().cloned().collect();
                            drop(buf);
                            publish_radio_frames(
                                &ws_state,
                                module,
                                frames,
                                http::handlers::build_frame_stats(&ws_state, module),
                            );
                        }
                        Err(RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    },
                    sent = tx_events_rx.recv() => match sent {
                        Some((module, payload)) => {
                            let module = module.min(1);
                            let entry = RxFrameDto {
                                module,
                                direction: "tx".into(),
                                rssi: 0,
                                len: payload.len() as u16,
                                hex: frame_preview(payload.as_slice()),
                                ascii: frame_ascii_preview(payload.as_slice()),
                                crc32: frame_crc32(payload.as_slice()),
                                ts: unix_timestamp_secs(),
                            };
                            frame_stats[module]
                                .tx_frames
                                .fetch_add(1, Ordering::Relaxed);
                            frame_stats[module]
                                .tx_bytes
                                .fetch_add(payload.len() as u64, Ordering::Relaxed);
                            let mut buf = rx_bufs[module].lock().await;
                            buf.push_front(entry);
                            buf.truncate(RX_BUF_SIZE);
                            let frames = buf.iter().cloned().collect();
                            drop(buf);
                            publish_radio_frames(
                                &ws_state,
                                module,
                                frames,
                                http::handlers::build_frame_stats(&ws_state, module),
                            );
                        }
                        None => break,
                    }
                }
            }
        });
    }

    // Keepalive — ping kaonic-commd every 30 s so the UDP server never removes us as a
    // known client (kaonic-commd expires idle clients after 120 s). Without this we
    // stop receiving ReceiveModule frames after 2 minutes.
    {
        let rc = radio_client.clone();
        let c = cancel.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
            interval.tick().await; // skip the immediate first tick
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(e) = rc.lock().await.ping().await {
                            log::warn!("keepalive ping failed: {e:?}");
                        }
                    }
                    _ = c.cancelled() => break,
                }
            }
        });
    }

    tokio::spawn(http::serve(app_state, cmd.http_addr, https_addr));

    shutdown_signal(cancel.clone()).await;

    // kaonic_vpn::run_vpn(
    //     transport,
    //     &config.network.to_string(),
    //     config.peers,
    //     config.announce_freq_secs,
    //     id,
    //     cancel.clone(),
    // ).await.map_err(|err| {
    //     log::error!("gateway error: {err:?}");
    //     process::ExitCode::FAILURE
    // })
    //
    Ok(())
}

fn should_run_webapp_only() -> bool {
    if let Ok(value) = std::env::var("KAONIC_GATEWAY_WEBAPP_ONLY") {
        let value = value.trim().to_ascii_lowercase();
        return matches!(value.as_str(), "1" | "true" | "yes" | "on");
    }

    !cfg!(target_os = "linux") || !Path::new("/etc/kaonic/kaonic_serial").exists()
}

/// Wait for Ctrl-C or SIGTERM, then cancel the token.
async fn shutdown_signal(cancel: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => log::info!("received Ctrl-C"),
            _ = sigterm.recv()          => log::info!("received SIGTERM"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        log::info!("received Ctrl-C");
    }
    log::info!("shutting down…");
    cancel.cancel();
}
