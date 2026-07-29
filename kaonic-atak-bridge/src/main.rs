use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use kaonic_gateway::radio::{attach_radio_interface, connect_radio_client};
use kaonic_gateway::settings::Settings;
use rand::rngs::OsRng;
use reticulum::destination::DestinationName;
use reticulum::identity::PrivateIdentity;
use reticulum::transport::{TimerConfig, Transport, TransportConfig};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const ATAK_PORTS: &[(u16, Ipv4Addr)] = &[
    (6969, Ipv4Addr::new(239, 2, 3, 1)),
    (17012, Ipv4Addr::new(224, 10, 10, 1)),
];
const DEFAULT_DB_PATH: &str = "kaonic-gateway.db";
const DEFAULT_CTRL_SERVER: &str = "192.168.10.1:9090";
const DEFAULT_SEED_KEY: &str = "atak_bridge_identity_seed";

#[derive(Parser)]
#[command(name = "kaonic-atak-bridge", version)]
struct Command {
    #[arg(short = 'a', long)]
    kaonic_ctrl_server: Option<std::net::SocketAddr>,
    #[arg(long, default_value_t = 0)]
    rns_module: usize,
    #[arg(long, default_value = DEFAULT_SEED_KEY)]
    seed_key: String,
}

#[derive(Default)]
struct BridgeMetrics {
    rx_packets: AtomicU64,
    tx_packets: AtomicU64,
}

#[tokio::main]
async fn main() -> Result<(), process::ExitCode> {
    let cmd = Command::parse();

    env_logger::Builder::new()
        .parse_filters("warn,kaonic_atak_bridge=debug,kaonic_gateway=warn,reticulum=warn")
        .parse_default_env()
        .init();

    let db_path =
        std::env::var("KAONIC_GATEWAY_DB_PATH").unwrap_or_else(|_| DEFAULT_DB_PATH.to_string());
    let settings = Settings::open(&db_path).unwrap_or_else(|err| {
        log::error!("failed to open database {db_path}: {err}");
        process::exit(1);
    });
    let config = settings.load_config().unwrap_or_else(|err| {
        log::error!("failed to load config from database: {err}");
        process::exit(1);
    });

    let seed = settings
        .load_or_create_named_seed(&cmd.seed_key)
        .unwrap_or_else(|err| {
            log::error!(
                "failed to load/create ATAK identity seed '{}': {err}",
                cmd.seed_key
            );
            process::exit(1);
        });
    let id = PrivateIdentity::new_from_name(&seed);

    let server_addr = cmd
        .kaonic_ctrl_server
        .unwrap_or_else(|| DEFAULT_CTRL_SERVER.parse().unwrap());
    let listen_addr: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
    let radio_client = connect_radio_client(listen_addr, server_addr)
        .await
        .map_err(|err| {
            log::error!("kaonic-ctrl connect error: {err:?}");
            process::ExitCode::FAILURE
        })?;

    let mut transport_cfg = TransportConfig::new("kaonic-atak-bridge", &id, true);
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
    transport_cfg.set_restart_outlinks(true);
    let transport = Arc::new(Mutex::new(Transport::new(transport_cfg)));
    attach_radio_interface(
        &transport,
        radio_client.clone(),
        &config.radio,
        cmd.rns_module.min(1),
        None,
        None,
        None,
    )
    .await
    .map_err(|err| {
        log::error!("radio interface attach error: {err}");
        process::ExitCode::FAILURE
    })?;

    let cancel = CancellationToken::new();
    spawn_keepalive(radio_client, cancel.clone());

    let mut tasks = Vec::new();
    for (port, group) in ATAK_PORTS {
        tasks.push(tokio::spawn(run_bridge(
            transport.clone(),
            id.clone(),
            *port,
            *group,
            cancel.clone(),
        )));
    }

    shutdown_signal(cancel.clone()).await;
    for task in tasks {
        let _ = task.await;
    }

    Ok(())
}

async fn run_bridge(
    transport: Arc<Mutex<Transport>>,
    identity: PrivateIdentity,
    port: u16,
    multicast_group: Ipv4Addr,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let metrics = Arc::new(BridgeMetrics::default());
    let port_tag = port.to_be_bytes();
    let dest_name = format!("atak.{port}");

    let destination = transport
        .lock()
        .await
        .add_destination(identity, DestinationName::new("kaonic", &dest_name))
        .await;
    let dest_hash = destination.lock().await.desc.address_hash;

    if let Ok(pkt) = destination.lock().await.announce(OsRng, Some(&port_tag)) {
        transport.lock().await.send_packet(pkt).await;
    }
    log::info!("atak-bridge:{port}: starting, dest={dest_hash}");

    let rx_sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    rx_sock.set_reuse_address(true)?;
    rx_sock.set_nonblocking(true)?;
    rx_sock.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;

    let local_addr = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .find_map(|iface| match iface.addr.ip() {
            IpAddr::V4(addr)
                if addr.octets()[0] == 192 && addr.octets()[1] == 168 && addr.octets()[2] == 10 =>
            {
                Some(addr)
            }
            _ => None,
        })
        .unwrap_or(Ipv4Addr::UNSPECIFIED);
    match rx_sock.join_multicast_v4(&multicast_group, &local_addr) {
        Ok(_) => {
            log::info!("atak-bridge:{port}: joined multicast {multicast_group} via {local_addr}")
        }
        Err(err) => log::warn!("atak-bridge:{port}: multicast join on {local_addr} failed: {err}"),
    }
    let udp_rx = Arc::new(UdpSocket::from_std(rx_sock.into())?);

    let mcast_target: std::net::SocketAddr = SocketAddrV4::new(multicast_group, port).into();
    let udp_tx_sockets: Arc<Vec<UdpSocket>> = Arc::new(
        if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|iface| match iface.addr.ip() {
                IpAddr::V4(addr) if !addr.is_loopback() => Some(addr),
                _ => None,
            })
            .filter_map(|local_ip| {
                let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
                socket.set_nonblocking(true).ok()?;
                socket.set_multicast_loop_v4(false).ok()?;
                socket.set_multicast_if_v4(&local_ip).ok()?;
                socket.bind(&SocketAddrV4::new(local_ip, 0).into()).ok()?;
                UdpSocket::from_std(socket.into()).ok()
            })
            .collect(),
    );

    let udp_to_rns = {
        let transport = transport.clone();
        let metrics = metrics.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    Ok((len, src)) = udp_rx.recv_from(&mut buf) => {
                        let data = &buf[..len];
                        log::info!("atak-bridge:{port}: udp -> rns {len}B from {src}");
                        metrics.rx_packets.fetch_add(1, Ordering::Relaxed);
                        let _ = transport.lock().await.send_to_in_links(&dest_hash, data).await;
                    }
                }
            }
        })
    };

    let rns_to_udp = {
        let transport = transport.clone();
        let metrics = metrics.clone();
        let cancel = cancel.clone();
        let sockets = udp_tx_sockets.clone();
        tokio::spawn(async move {
            let mut events = transport.lock().await.out_link_events();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    Ok(ev) = events.recv() => {
                        if let reticulum::destination::link::LinkEvent::Data(payload) = ev.event {
                            let data = payload.as_slice();
                            for socket in sockets.iter() {
                                if let Err(err) = socket.send_to(data, mcast_target).await {
                                    log::warn!("atak-bridge:{port}: udp send error: {err}");
                                }
                            }
                            metrics.tx_packets.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        })
    };

    let auto_link = {
        let transport = transport.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut announces = transport.lock().await.recv_announces().await;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    Ok(ev) = announces.recv() => {
                        if ev.app_data.as_slice() != port_tag {
                            continue;
                        }
                        let peer = ev.destination.lock().await.desc.clone();
                        if peer.address_hash == dest_hash {
                            continue;
                        }
                        let transport = transport.lock().await;
                        if transport.find_out_link(&peer.address_hash).await.is_some() {
                            continue;
                        }
                        log::info!("atak-bridge:{port}: auto-link -> {}", peer.address_hash);
                        transport.link(peer).await;
                    }
                }
            }
        })
    };

    let reannounce = {
        let transport = transport.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        if let Ok(pkt) = destination.lock().await.announce(OsRng, Some(&port_tag)) {
                            transport.lock().await.send_packet(pkt).await;
                        }
                    }
                }
            }
        })
    };

    let _ = tokio::join!(udp_to_rns, rns_to_udp, auto_link, reannounce);
    log::info!("atak-bridge:{port}: stopped");
    Ok(())
}

fn spawn_keepalive(
    radio_client: kaonic_gateway::radio::SharedRadioClient,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.tick().await;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(err) = radio_client.lock().await.ping().await {
                        log::warn!("keepalive ping failed: {err:?}");
                    }
                }
                _ = cancel.cancelled() => break,
            }
        }
    });
}

async fn shutdown_signal(cancel: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => log::info!("received Ctrl-C"),
            _ = sigterm.recv() => log::info!("received SIGTERM"),
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
