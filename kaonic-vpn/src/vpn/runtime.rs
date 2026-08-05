//! VPN runtime orchestration.
//!
//! Owns the small set of tokio tasks that drive the VPN: announce TX/RX,
//! in/out link event streams, TUN read loop, and a periodic watchdog that
//! expires stale peer routes and re-requests dead outbound links.
//!
//! The hot path (tun→peer, peer→tun) goes through the lock-free `Router`
//! snapshot and per-peer `Arc<Peer>` handles, so the only unavoidable lock
//! is Reticulum's own `Mutex<Transport>` when we call `send_to_out_links`.

use std::collections::{BTreeSet, HashSet};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cidr::Ipv4Cidr;
use etherparse::IpSlice;
use if_addrs::{get_if_addrs, IfAddr};
use parking_lot::RwLock as PlRwLock;
use reticulum::destination::link::{LinkEvent, LinkStatus};
use reticulum::destination::DestinationName;
use reticulum::hash::AddressHash;
use reticulum::identity::PrivateIdentity;
use reticulum::transport::Transport;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::config::VpnConfig;

use super::codec::{
    decode_announce, decode_ctrl, encode_announce, encode_hello, is_announce, is_ctrl, Ctrl,
};
use super::links::LinkRegistry;
use super::metrics::{now_secs, Metrics};
use super::peer::{LinkState, Peer, PeerRegistry};
use super::platform::{self, LocalRouteTranslation};
use super::router::{RouteTable, Router};
use super::tun::{self, SharedTun, TUN_MTU};
use super::types::{VpnPeerSnapshot, VpnRouteMappingSnapshot, VpnRouteSnapshot, VpnSnapshot};

/// Peer routes are dropped this long after the last announce. Keeps a small
/// grace window so a single missed announce does not flap the kernel route.
const ROUTE_GRACE_SECS: u64 = 45;
/// Watchdog cadence.
const WATCHDOG_SECS: u64 = 10;
/// Give a link this long to finish its handshake before we tear it down and
/// try again. Reticulum keeps re-sending the link request every 6 s on its
/// own, but it never gives up on a stuck Pending link, so a lost proof reply
/// would leave us wedged forever without this timeout.
const LINK_PENDING_TIMEOUT_SECS: u64 = 10;
/// After a forced close, wait briefly before opening a fresh link so control
/// traffic can settle and duplicate reopen attempts collapse together.
const LINK_RECONNECT_COOLDOWN_SECS: u64 = 3;
/// When a peer is not currently Active, briefly pause TUN TX so the transport
/// is not hammered with packets it cannot deliver yet.
const TUN_TX_BACKOFF_MILLIS: u64 = 100;

// ── Status ───────────────────────────────────────────────────────────────────

const STATUS_RUNNING: u8 = 0;
const STATUS_MOCK: u8 = 1;
const STATUS_ERROR: u8 = 2;

fn status_str(code: u8) -> &'static str {
    match code {
        STATUS_MOCK => "mock",
        STATUS_ERROR => "error",
        _ => "running",
    }
}

// ── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum VpnRuntimeError {
    Config(String),
    Io(std::io::Error),
    Tun(String),
}

impl std::fmt::Display for VpnRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(m) => f.write_str(m),
            Self::Io(e) => e.fmt(f),
            Self::Tun(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for VpnRuntimeError {}

impl From<std::io::Error> for VpnRuntimeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ── Runtime ──────────────────────────────────────────────────────────────────

pub struct VpnRuntime {
    destination: AddressHash,
    destination_hex: String,
    network: Ipv4Cidr,
    local_tunnel_ip: Ipv4Addr,
    interface_name: Option<String>,
    backend: &'static str,
    route_aliasing_enabled: bool,
    status: AtomicU8,

    metrics: Metrics,
    peers: PeerRegistry,
    router: Router,
    out_links: LinkRegistry,

    slow: PlRwLock<SlowState>,
}

/// Mutable bookkeeping that only changes on user action or periodic sync.
struct SlowState {
    allow_all_peers: bool,
    allowed_peers: HashSet<AddressHash>,
    advertised_routes: Vec<Ipv4Cidr>,
    local_routes: Vec<Ipv4Cidr>,
    installed_routes: BTreeSet<String>,
    conflicted_routes: BTreeSet<String>,
    last_error: Option<String>,
}

impl VpnRuntime {
    pub async fn start(
        config: VpnConfig,
        transport: Arc<AsyncMutex<Transport>>,
        id: PrivateIdentity,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>, VpnRuntimeError> {
        validate_network(config.network)?;
        let configured_peers = parse_configured_peers(&config.peers)?;

        let destination = transport
            .lock()
            .await
            .add_destination(id, DestinationName::new("kaonic", "vpn"))
            .await;
        let destination_hash = destination.lock().await.desc.address_hash;
        let local_tunnel_ip = derive_tunnel_ip(config.network, &destination_hash)?;

        let tun = tun::open_platform()?;
        let interface_name = tun.as_ref().map(|t| t.name().to_string());
        let route_aliasing_enabled = platform::supports_route_aliasing();

        if let Some(name) = interface_name.as_deref() {
            platform::configure_tun_address(
                name,
                local_tunnel_ip,
                config.network.network_length(),
            )?;
            platform::enable_forwarding()?;
        }

        let discovered_routes = discover_local_routes(interface_name.as_deref());
        let local_routes = merge_routes(&discovered_routes, &config.advertised_routes);

        let status = if interface_name.is_some() {
            STATUS_RUNNING
        } else {
            STATUS_MOCK
        };

        let runtime = Arc::new(Self {
            destination: destination_hash,
            destination_hex: destination_hash.to_hex_string(),
            network: config.network,
            local_tunnel_ip,
            interface_name: interface_name.clone(),
            backend: platform::backend_name(),
            route_aliasing_enabled,
            status: AtomicU8::new(status),
            metrics: Metrics::default(),
            peers: PeerRegistry::new(),
            router: Router::new(),
            out_links: LinkRegistry::new(),
            slow: PlRwLock::new(SlowState {
                allow_all_peers: config.allow_all_peers,
                allowed_peers: configured_peers.clone(),
                advertised_routes: config.advertised_routes.clone(),
                local_routes,
                installed_routes: BTreeSet::new(),
                conflicted_routes: BTreeSet::new(),
                last_error: None,
            }),
        });

        // Seed configured peers (they get a tunnel IP slot, but no routes yet).
        for hash in configured_peers {
            if hash == destination_hash {
                continue;
            }
            let tunnel_ip = derive_tunnel_ip(config.network, &hash)?;
            runtime
                .peers
                .insert(Peer::new(hash, tunnel_ip, LinkState::Configured));
        }
        runtime.rebuild_router();

        log::info!(
            "vpn start dest={} tunnel_ip={} network={} iface={} backend={} aliasing={}",
            runtime.destination_hex,
            local_tunnel_ip,
            config.network,
            interface_name.as_deref().unwrap_or("mock"),
            runtime.backend,
            route_aliasing_enabled
        );

        // Initial route sync so the kernel sees our advertised routes on disk.
        runtime.sync_routes();

        spawn_announce_tx(
            runtime.clone(),
            transport.clone(),
            destination.clone(),
            config.announce_freq_secs,
            cancel.clone(),
        );
        spawn_announce_rx(runtime.clone(), transport.clone(), cancel.clone());
        spawn_out_link_events(
            runtime.clone(),
            transport.clone(),
            tun.clone(),
            cancel.clone(),
        );
        spawn_in_link_events(
            runtime.clone(),
            transport.clone(),
            tun.clone(),
            cancel.clone(),
        );
        spawn_tun_rx(
            runtime.clone(),
            transport.clone(),
            tun.clone(),
            cancel.clone(),
        );
        spawn_watchdog(runtime.clone(), transport, cancel);

        Ok(runtime)
    }

    pub async fn snapshot(&self) -> VpnSnapshot {
        self.snapshot_blocking()
    }

    pub async fn replace_peer_policy(
        &self,
        allow_all_peers: bool,
        peers: Vec<String>,
    ) -> Result<(), VpnRuntimeError> {
        let allowed_peers = parse_configured_peers(&peers)?;
        {
            let mut slow = self.slow.write();
            slow.allow_all_peers = allow_all_peers;
            slow.allowed_peers = allowed_peers.clone();
            slow.last_error = None;
        }

        for hash in &allowed_peers {
            if *hash == self.destination {
                continue;
            }
            if self.peers.get(hash).is_some() {
                continue;
            }
            let tunnel_ip = derive_tunnel_ip(self.network, hash)?;
            self.peers
                .insert(Peer::new(*hash, tunnel_ip, LinkState::Configured));
        }

        if !allow_all_peers {
            for peer in self.peers.all() {
                if peer.hash == self.destination || allowed_peers.contains(&peer.hash) {
                    continue;
                }
                self.peers.remove(&peer.hash);
                self.out_links.remove(&peer.hash);
            }
        }

        self.rebuild_router();
        self.sync_routes();
        Ok(())
    }

    pub async fn replace_advertised_routes(&self, routes: Vec<Ipv4Cidr>) {
        {
            let discovered = discover_local_routes(self.interface_name.as_deref());
            let mut slow = self.slow.write();
            slow.advertised_routes = routes;
            slow.local_routes = merge_routes(&discovered, &slow.advertised_routes);
            slow.last_error = None;
        }
        self.sync_routes();
    }

    // ── Snapshot assembly ────────────────────────────────────────────────────

    fn snapshot_blocking(&self) -> VpnSnapshot {
        let now = now_secs();
        let slow = self.slow.read();
        let metrics = self.metrics.snapshot();
        let table = self.router.snapshot();

        let mut peers_snap = Vec::new();
        let mut remote_routes = Vec::new();
        let mut owners = BTreeSet::new();

        for peer in self.peers.all() {
            if peer.hash == self.destination {
                continue;
            }
            let state = peer.state();
            let last_seen = peer.last_seen_ts.load(Ordering::Relaxed);
            let last_err = peer.last_error.read().clone();
            let routes = peer.routes_clone();
            let route_strings = if routes_live(&peer, now) {
                routes.iter().map(ToString::to_string).collect()
            } else {
                Vec::new()
            };
            let m = peer.metrics.snapshot();
            peers_snap.push(VpnPeerSnapshot {
                destination: peer.hash.to_hex_string(),
                tunnel_ip: Some(peer.tunnel_ip.to_string()),
                link_state: state.as_str().into(),
                announced_routes: route_strings,
                last_seen_ts: last_seen,
                last_error: last_err,
                tx_packets: m.tx_packets,
                tx_bytes: m.tx_bytes,
                rx_packets: m.rx_packets,
                rx_bytes: m.rx_bytes,
                tx_bps: m.tx_bps,
                rx_bps: m.rx_bps,
                last_tx_ts: m.last_tx_ts,
                last_rx_ts: m.last_rx_ts,
            });
            for route in &routes {
                let key = route.to_string();
                if !owners.insert((key.clone(), peer.hash.to_hex_string())) {
                    continue;
                }
                let installed = slow.installed_routes.contains(&key);
                let conflict = slow.conflicted_routes.contains(&key);
                remote_routes.push(VpnRouteSnapshot {
                    network: key,
                    owner: peer.hash.to_hex_string(),
                    status: if conflict {
                        "conflict".into()
                    } else {
                        "active".into()
                    },
                    last_seen_ts: last_seen,
                    installed: installed && !conflict,
                });
            }
        }
        peers_snap.sort_by(|a, b| a.destination.cmp(&b.destination));
        remote_routes.sort_by(|a, b| a.network.cmp(&b.network));

        // Local route mappings (for the exported/local subnet table in the UI).
        let translations = local_route_translations(
            &slow.local_routes,
            &self.destination,
            self.route_aliasing_enabled,
        );
        let mut local_routes: Vec<String> = translations
            .iter()
            .map(|t| {
                if t.local == t.exported {
                    t.local.to_string()
                } else {
                    format!("{} -> {}", t.exported, t.local)
                }
            })
            .collect();
        local_routes.sort();

        let mut route_mappings: Vec<VpnRouteMappingSnapshot> = translations
            .iter()
            .map(|t| VpnRouteMappingSnapshot {
                subnet: t.local.to_string(),
                tunnel: self.local_tunnel_ip.to_string(),
                mapped_subnet: t.exported.to_string(),
            })
            .collect();
        route_mappings.sort_by(|a, b| {
            a.subnet
                .cmp(&b.subnet)
                .then(a.mapped_subnet.cmp(&b.mapped_subnet))
        });

        let mut advertised: Vec<String> = slow
            .advertised_routes
            .iter()
            .map(ToString::to_string)
            .collect();
        advertised.sort();

        // subnets from the router snapshot give us the full "active route set"
        // even when the corresponding peer state has gone cold; not used here
        // but kept as the authoritative forwarding view.
        let _ = table.subnets();

        VpnSnapshot {
            destination_hash: self.destination_hex.clone(),
            network: self.network.to_string(),
            local_tunnel_ip: Some(self.local_tunnel_ip.to_string()),
            backend: self.backend.into(),
            interface_name: self.interface_name.clone(),
            status: status_str(self.status.load(Ordering::Relaxed)).into(),
            advertised_routes: advertised,
            local_routes,
            tx_packets: metrics.tx_packets,
            tx_bytes: metrics.tx_bytes,
            tx_bps: metrics.tx_bps,
            rx_packets: metrics.rx_packets,
            rx_bytes: metrics.rx_bytes,
            rx_bps: metrics.rx_bps,
            drop_packets: metrics.drop_packets,
            last_tx_ts: metrics.last_tx_ts,
            last_rx_ts: metrics.last_rx_ts,
            peers: peers_snap,
            remote_routes,
            route_mappings,
            last_error: slow.last_error.clone(),
        }
    }

    // ── Router ───────────────────────────────────────────────────────────────

    fn rebuild_router(&self) {
        let table = RouteTable::build(
            self.peers.all().into_iter(),
            now_secs(),
            self.destination,
            self.local_tunnel_ip,
        );
        self.router.swap(table);
    }

    // ── Route kernel sync ────────────────────────────────────────────────────

    /// Re-compute the kernel route table and iptables NETMAP translations.
    /// Called on startup, on user `replace_advertised_routes`, and from the
    /// watchdog after peer routes change.
    fn sync_routes(&self) {
        let table = self.router.snapshot();
        let (interface, local_routes, aliasing) = {
            let slow = self.slow.read();
            (
                self.interface_name.clone(),
                slow.local_routes.clone(),
                self.route_aliasing_enabled,
            )
        };
        let translations = local_route_translations(&local_routes, &self.destination, aliasing);
        let local_conflicts =
            conflicting_local_routes(&translations, table.subnets().iter().map(|(c, _)| *c));

        let desired: BTreeSet<String> = table
            .subnets()
            .iter()
            .filter(|(route, _)| !local_conflicts.contains(&route.to_string()))
            .map(|(route, _)| route.to_string())
            .collect();

        let (installed_before, iface) = {
            let slow = self.slow.read();
            (slow.installed_routes.clone(), interface.clone())
        };

        if let Some(name) = iface.as_deref() {
            for route in installed_before.difference(&desired) {
                platform::delete_route(name, route);
            }
            for route in &desired {
                if let Err(err) = platform::replace_route(name, route) {
                    self.set_error(err.to_string());
                    return;
                }
            }
            if let Err(err) = platform::sync_route_translations(name, &translations) {
                self.set_error(err.to_string());
                return;
            }
        }

        let mut slow = self.slow.write();
        slow.installed_routes = desired;
        slow.conflicted_routes = local_conflicts;
        slow.last_error = None;
        if self.status.load(Ordering::Relaxed) != STATUS_MOCK {
            self.status.store(STATUS_RUNNING, Ordering::Relaxed);
        }
    }

    fn set_error(&self, msg: String) {
        self.slow.write().last_error = Some(msg);
        self.status.store(STATUS_ERROR, Ordering::Relaxed);
    }

    // ── Announces ────────────────────────────────────────────────────────────

    fn exported_routes(&self) -> Vec<Ipv4Cidr> {
        let slow = self.slow.read();
        local_route_translations(
            &slow.local_routes,
            &self.destination,
            self.route_aliasing_enabled,
        )
        .into_iter()
        .map(|t| t.exported)
        .collect()
    }

    fn refresh_local_routes(&self) {
        let discovered = discover_local_routes(self.interface_name.as_deref());
        let mut slow = self.slow.write();
        slow.local_routes = merge_routes(&discovered, &slow.advertised_routes);
    }

    fn allow_all_peers(&self) -> bool {
        self.slow.read().allow_all_peers
    }

    fn peer_allowed(&self, hash: &AddressHash) -> bool {
        if *hash == self.destination {
            return false;
        }
        let slow = self.slow.read();
        slow.allow_all_peers || slow.allowed_peers.contains(hash)
    }

    fn handle_peer_announce(
        &self,
        desc: reticulum::destination::DestinationDesc,
        routes: Vec<Ipv4Cidr>,
    ) -> bool {
        let hash = desc.address_hash;
        if hash == self.destination {
            return false;
        }
        if !self.peer_allowed(&hash) {
            log::debug!("vpn peer={} announce ignored by allowlist", hash);
            return false;
        }
        let tunnel_ip = match derive_tunnel_ip(self.network, &hash) {
            Ok(ip) => ip,
            Err(_) => return false,
        };
        let peer = self
            .peers
            .get_or_create(hash, || Peer::new(hash, tunnel_ip, LinkState::Discovered));
        peer.set_desc(desc);
        peer.mark_seen();
        let now = now_secs();
        peer.route_expires_ts
            .store(now + ROUTE_GRACE_SECS, Ordering::Relaxed);
        peer.set_routes(routes);
        peer.clear_error();
        self.rebuild_router();
        self.sync_routes();
        true
    }

    // ── Peer state updates ───────────────────────────────────────────────────

    fn set_link_state(&self, hash: AddressHash, state: LinkState) {
        if let Some(peer) = self.peers.get(&hash) {
            peer.set_state(state);
        }
    }

    fn record_peer_error(&self, hash: AddressHash, msg: String) {
        if let Some(peer) = self.peers.get(&hash) {
            peer.set_error(msg);
        }
    }

    fn record_drop(&self) {
        self.metrics.record_drop();
    }
}

// ── Background tasks ─────────────────────────────────────────────────────────

fn spawn_announce_tx(
    runtime: Arc<VpnRuntime>,
    transport: Arc<AsyncMutex<Transport>>,
    destination: Arc<AsyncMutex<reticulum::destination::SingleInputDestination>>,
    announce_freq_secs: u32,
    cancel: CancellationToken,
) {
    let freq = Duration::from_secs(announce_freq_secs.max(1) as u64);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(freq);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    runtime.refresh_local_routes();
                    let routes = runtime.exported_routes();
                    match encode_announce(&routes) {
                        Ok(app_data) => {
                            transport.lock().await.send_announce(&destination, Some(&app_data)).await;
                        }
                        Err(err) => runtime.set_error(err.to_string()),
                    }
                }
            }
        }
    });
}

fn spawn_announce_rx(
    runtime: Arc<VpnRuntime>,
    transport: Arc<AsyncMutex<Transport>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut rx = transport.lock().await.recv_announces().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                recv = rx.recv() => match recv {
                    Ok(announce) => {
                        let desc = announce.destination.lock().await.desc.clone();
                        let app_data = announce.app_data.as_slice();
                        if !is_announce(app_data) {
                            continue;
                        }
                        match decode_announce(app_data) {
                            Ok(routes) => {
                                if !runtime.handle_peer_announce(desc.clone(), routes) {
                                    continue;
                                }
                                // Announces are the primary trigger for opening
                                // a fresh outbound link after a disconnect. If a
                                // link is already active/pending, request_out_link
                                // reuses it without restarting the attempt.
                                let transport = transport.clone();
                                let runtime = runtime.clone();
                                tokio::spawn(async move {
                                    request_out_link(&runtime, &transport, desc).await;
                                });
                            }
                            Err(err) => {
                                runtime.record_peer_error(desc.address_hash, err.to_string());
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    });
}

fn spawn_out_link_events(
    runtime: Arc<VpnRuntime>,
    transport: Arc<AsyncMutex<Transport>>,
    tun: Option<SharedTun>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut rx = transport.lock().await.out_link_events();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                recv = rx.recv() => match recv {
                    Ok(event) => {
                        let peer_hash = event.address_hash;
                        match event.event {
                            LinkEvent::Activated => {
                                runtime.set_link_state(peer_hash, LinkState::Active);
                                if let Some(peer) = runtime.peers.get(&peer_hash) {
                                    peer.clear_link_attempt();
                                    peer.finish_link_request();
                                    peer.clear_reconnect_cooldown();
                                    peer.reconnect_attempts.store(0, Ordering::Relaxed);
                                }
                                if let Some(link) = transport.lock().await.find_out_link(&peer_hash).await {
                                    runtime.out_links.insert(peer_hash, link);
                                }
                                let routes = runtime.exported_routes();
                                if let Ok(hello) = encode_hello(&routes) {
                                    let _ = transport.lock().await.send_to_out_links(&peer_hash, &hello).await;
                                }
                                log::info!("vpn peer={} out-link activated", peer_hash);
                            }
                            LinkEvent::Closed => {
                                runtime.set_link_state(peer_hash, LinkState::Closed);
                                runtime.out_links.remove(&peer_hash);
                                if let Some(peer) = runtime.peers.get(&peer_hash) {
                                    peer.clear_link_attempt();
                                    peer.finish_link_request();
                                    peer.set_reconnect_cooldown(LINK_RECONNECT_COOLDOWN_SECS);
                                }
                                log::warn!("vpn peer={} out-link closed", peer_hash);
                            }
                            LinkEvent::Data(payload) => {
                                if let Some(t) = tun.as_ref() {
                                    handle_link_data(&runtime, t, Some(peer_hash), payload.as_slice()).await;
                                }
                            }
                            LinkEvent::Proof(_) | LinkEvent::RemoteIdentified(_) => {}
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    });
}

fn spawn_in_link_events(
    runtime: Arc<VpnRuntime>,
    transport: Arc<AsyncMutex<Transport>>,
    tun: Option<SharedTun>,
    cancel: CancellationToken,
) {
    let local = runtime.destination;
    tokio::spawn(async move {
        let mut rx = transport.lock().await.in_link_events();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                recv = rx.recv() => match recv {
                    Ok(event) => {
                        if event.address_hash != local {
                            continue;
                        }
                        if let LinkEvent::Data(payload) = event.event {
                            if let Some(t) = tun.as_ref() {
                                // No single owning peer on the in-link stream —
                                // let handle_link_data resolve the credit target
                                // from the packet source IP.
                                handle_link_data(&runtime, t, None, payload.as_slice()).await;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    });
}

fn spawn_tun_rx(
    runtime: Arc<VpnRuntime>,
    transport: Arc<AsyncMutex<Transport>>,
    tun: Option<SharedTun>,
    cancel: CancellationToken,
) {
    let Some(tun) = tun else {
        return;
    };
    tokio::spawn(async move {
        let mut buf = vec![0u8; TUN_MTU];
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                read = tun.recv(&mut buf) => match read {
                    Ok(n) => {
                        let packet = &buf[..n];
                        let Some(dst_ip) = packet_destination(packet) else { continue; };
                        if dst_ip == runtime.local_tunnel_ip {
                            continue;
                        }
                        let Some(peer_hash) = runtime.router.snapshot().resolve(dst_ip) else {
                            runtime.record_drop();
                            continue;
                        };
                        if let Some(peer) = runtime.peers.get(&peer_hash) {
                            if tun_tx_requires_backoff(peer.state()) {
                                runtime.record_drop();
                                tokio::time::sleep(Duration::from_millis(TUN_TX_BACKOFF_MILLIS)).await;
                                tokio::task::yield_now().await;
                                continue;
                            }
                        }
                        let sent = transport.lock().await.send_to_out_links(&peer_hash, packet).await;
                        if sent.is_empty() {
                            runtime.record_drop();
                            log::warn!(
                                "vpn tx drop peer={} src={} dst={} len={} no-active-link",
                                peer_hash,
                                packet_source(packet)
                                    .map(|ip| ip.to_string())
                                    .unwrap_or_else(|| "unknown".into()),
                                dst_ip,
                                packet.len()
                            );
                            tokio::time::sleep(Duration::from_millis(TUN_TX_BACKOFF_MILLIS)).await;
                        } else {
                            runtime.metrics.record_tx(packet.len());
                            if let Some(peer) = runtime.peers.get(&peer_hash) {
                                peer.mark_tx();
                                peer.metrics.record_tx(packet.len());
                            }
                            log::debug!(
                                "vpn tx peer={} src={} dst={} len={} packets={}",
                                peer_hash,
                                packet_source(packet)
                                    .map(|ip| ip.to_string())
                                    .unwrap_or_else(|| "unknown".into()),
                                dst_ip,
                                packet.len(),
                                sent.len()
                            );
                        }
                        tokio::task::yield_now().await;
                    }
                    Err(err) => {
                        runtime.set_error(format!("vpn tun recv: {err}"));
                        break;
                    }
                }
            }
        }
    });
}

fn spawn_watchdog(
    runtime: Arc<VpnRuntime>,
    transport: Arc<AsyncMutex<Transport>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(WATCHDOG_SECS));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let now = now_secs();
                    let mut routes_changed = false;
                    for peer in runtime.peers.all() {
                        if peer.hash == runtime.destination {
                            continue;
                        }
                        let expires = peer.route_expires_ts.load(Ordering::Relaxed);
                        if expires != 0 && expires <= now && !peer.routes_clone().is_empty() {
                            peer.set_routes(Vec::new());
                            routes_changed = true;
                        }
                    }
                    if routes_changed {
                        runtime.rebuild_router();
                        runtime.sync_routes();
                    }

                    // Rescue peers whose current transport link has been stuck
                    // too long by closing it locally and waiting for a fresh
                    // announce before starting the next outbound attempt.
                    for peer in runtime.peers.all() {
                        if peer.hash == runtime.destination {
                            continue;
                        }
                        let existing = transport.lock().await.find_out_link(&peer.hash).await;
                        let Some(link) = existing else {
                            continue;
                        };
                        let (status, link_age_secs) = {
                            let link = link.lock().await;
                            (link.status(), link.elapsed().as_secs())
                        };
                        sync_peer_link_state(&peer, status);
                        if !out_link_needs_reset(status, link_age_secs) {
                            continue;
                        }
                        log::warn!(
                            "vpn peer={} out-link {status:?} for {link_age_secs}s; closing until next announce",
                            peer.hash
                        );
                        drop(link);
                        if let Err(err) = transport.lock().await.link_close(peer.hash).await {
                            log::warn!(
                                "vpn peer={} failed to close stuck out-link: {err:?}",
                                peer.hash
                            );
                            continue;
                        }
                        runtime.out_links.remove(&peer.hash);
                        peer.set_state(LinkState::Closed);
                        peer.clear_link_attempt();
                        peer.finish_link_request();
                        peer.set_reconnect_cooldown(LINK_RECONNECT_COOLDOWN_SECS);
                    }
                }
            }
        }
    });
}

// ── Helpers ──────────────────────────────────────────────────────────────────

async fn handle_link_data(
    runtime: &Arc<VpnRuntime>,
    tun: &SharedTun,
    sender_hint: Option<AddressHash>,
    data: &[u8],
) {
    if is_ctrl(data) {
        // CTRL frames require a known sender. Out-link events carry the peer
        // hash; in-link events do not (they are filtered on our local
        // destination and we cannot attribute them from the frame alone), so
        // we just drop unattributed CTRL frames.
        let Some(hash) = sender_hint else {
            return;
        };
        match decode_ctrl(data) {
            Ok(Ctrl::Hello { routes }) | Ok(Ctrl::Routes { routes }) => {
                let cidrs: Vec<Ipv4Cidr> = routes.iter().filter_map(|s| s.to_cidr().ok()).collect();
                if let Some(peer) = runtime.peers.get(&hash) {
                    peer.mark_seen();
                    peer.route_expires_ts
                        .store(now_secs() + ROUTE_GRACE_SECS, Ordering::Relaxed);
                    peer.set_routes(cidrs);
                }
                runtime.rebuild_router();
                runtime.sync_routes();
            }
            Ok(Ctrl::Ping) => {
                if let Some(peer) = runtime.peers.get(&hash) {
                    peer.mark_seen();
                }
            }
            Err(err) => runtime.record_peer_error(hash, err.to_string()),
        }
        return;
    }
    let src_ip = packet_source(data);
    let dst_ip = packet_destination(data);
    let credited = sender_hint.and_then(|h| runtime.peers.get(&h)).or_else(|| {
        let src = src_ip?;
        let owner = runtime.router.snapshot().resolve(src)?;
        runtime.peers.get(&owner)
    });
    if credited.is_none() && (sender_hint.is_some() || !runtime.allow_all_peers()) {
        log::debug!(
            "vpn drop peer=unknown src={} dst={} len={} reason=peer-filter",
            src_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "unknown".into()),
            dst_ip
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "unknown".into()),
            data.len()
        );
        runtime.record_drop();
        return;
    }
    // Forward to TUN and credit the peer that sent it. Prefer the explicit
    // hint from the out-link event; fall back to resolving the packet source
    // IP through the router (the same lookup used on the TX path).
    match tun.send(data).await {
        Ok(_) => {
            runtime.metrics.record_rx(data.len());
            if let Some(peer) = credited {
                peer.metrics.record_rx(data.len());
                peer.mark_seen();
                log::debug!(
                    "vpn rx peer={} src={} dst={} len={}",
                    peer.hash,
                    src_ip
                        .map(|ip| ip.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                    dst_ip
                        .map(|ip| ip.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                    data.len()
                );
            } else {
                log::warn!(
                    "vpn rx peer=unknown src={} dst={} len={}",
                    src_ip
                        .map(|ip| ip.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                    dst_ip
                        .map(|ip| ip.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                    data.len()
                );
            }
        }
        Err(err) => runtime.set_error(format!("vpn tun send: {err}")),
    }
}

async fn request_out_link(
    runtime: &Arc<VpnRuntime>,
    transport: &Arc<AsyncMutex<Transport>>,
    desc: reticulum::destination::DestinationDesc,
) {
    let hash = desc.address_hash;
    if !runtime.peer_allowed(&hash) {
        runtime.out_links.remove(&hash);
        return;
    }
    let peer = runtime.peers.get(&hash);
    let existing = transport.lock().await.find_out_link(&hash).await;
    let existing_status = if let Some(link) = existing.as_ref() {
        Some(link.lock().await.status())
    } else {
        None
    };

    if should_reuse_existing_out_link(existing_status) {
        if let Some(link) = existing {
            runtime.out_links.insert(hash, link);
        }
        return;
    }

    if let Some(peer) = peer.as_ref() {
        let now = now_secs();
        if link_request_on_cooldown(peer, now) {
            log::debug!(
                "vpn peer={} reconnect cooldown {}s active; skipping new out-link",
                hash,
                peer.reconnect_cooldown_until().saturating_sub(now)
            );
            return;
        }
        if !peer.try_begin_link_request() {
            log::debug!("vpn peer={} link request already in flight", hash);
            return;
        }
        peer.set_state(LinkState::Pending);
        peer.reconnect_attempts.fetch_add(1, Ordering::Relaxed);
        peer.mark_link_attempt();
    }
    let link = transport.lock().await.link(desc).await;
    runtime.out_links.insert(hash, link);
    if let Some(peer) = peer.as_ref() {
        peer.finish_link_request();
    }
}

fn should_reuse_existing_out_link(existing_status: Option<LinkStatus>) -> bool {
    matches!(
        existing_status,
        Some(LinkStatus::Pending | LinkStatus::Handshake | LinkStatus::Active | LinkStatus::Stale)
    )
}

fn out_link_needs_reset(status: LinkStatus, link_age_secs: u64) -> bool {
    match status {
        LinkStatus::Pending | LinkStatus::Handshake => link_age_secs >= LINK_PENDING_TIMEOUT_SECS,
        LinkStatus::Stale | LinkStatus::Closed | LinkStatus::Active => false,
    }
}

fn link_request_on_cooldown(peer: &Peer, now: u64) -> bool {
    peer.reconnect_cooldown_until() > now
}

fn tun_tx_requires_backoff(state: LinkState) -> bool {
    !matches!(state, LinkState::Active)
}

fn sync_peer_link_state(peer: &Peer, status: LinkStatus) {
    match status {
        LinkStatus::Active => {
            peer.set_state(LinkState::Active);
            peer.clear_link_attempt();
            peer.finish_link_request();
            peer.clear_reconnect_cooldown();
            peer.reconnect_attempts.store(0, Ordering::Relaxed);
        }
        LinkStatus::Pending | LinkStatus::Handshake | LinkStatus::Stale => {
            peer.set_state(LinkState::Pending);
        }
        LinkStatus::Closed => {
            peer.set_state(LinkState::Closed);
            peer.finish_link_request();
        }
    }
}

// ── Packet parsing ───────────────────────────────────────────────────────────

fn packet_destination(packet: &[u8]) -> Option<Ipv4Addr> {
    match IpSlice::from_slice(packet).ok()?.destination_addr() {
        std::net::IpAddr::V4(ip) => Some(ip),
        std::net::IpAddr::V6(_) => None,
    }
}

fn packet_source(packet: &[u8]) -> Option<Ipv4Addr> {
    match IpSlice::from_slice(packet).ok()?.source_addr() {
        std::net::IpAddr::V4(ip) => Some(ip),
        std::net::IpAddr::V6(_) => None,
    }
}

// ── Route helpers ────────────────────────────────────────────────────────────

fn routes_live(peer: &Peer, now: u64) -> bool {
    let exp = peer.route_expires_ts.load(Ordering::Relaxed);
    !peer.routes_clone().is_empty() && (exp == 0 || exp > now)
}

fn merge_routes(discovered: &[Ipv4Cidr], advertised: &[Ipv4Cidr]) -> Vec<Ipv4Cidr> {
    let mut out = BTreeSet::new();
    out.extend(discovered.iter().copied());
    out.extend(advertised.iter().copied());
    out.into_iter().collect()
}

fn local_route_translations(
    routes: &[Ipv4Cidr],
    destination: &AddressHash,
    aliasing: bool,
) -> Vec<LocalRouteTranslation> {
    routes
        .iter()
        .map(|r| LocalRouteTranslation {
            local: *r,
            exported: if aliasing {
                export_local_route(destination, *r)
            } else {
                *r
            },
        })
        .collect()
}

/// Exports /24 LANs as 192.168.{100..254}.0/24 aliases so peers see distinct
/// subnets. Larger prefixes are left untouched — they are wire-routable as-is.
fn export_local_route(destination: &AddressHash, route: Ipv4Cidr) -> Ipv4Cidr {
    if route.network_length() != 24 {
        return route;
    }
    let mut seed = 0u32;
    for byte in destination.as_slice() {
        seed = seed.wrapping_mul(167).wrapping_add(u32::from(*byte));
    }
    for byte in route.first_address().octets() {
        seed = seed.wrapping_mul(131).wrapping_add(u32::from(byte));
    }
    seed = seed
        .wrapping_mul(31)
        .wrapping_add(u32::from(route.network_length()));

    let mut third = 100 + (seed % 155) as u8;
    let oct = route.first_address().octets();
    if oct[0] == 192 && oct[1] == 168 && oct[2] == third {
        third = if third == 254 { 100 } else { third + 1 };
    }
    Ipv4Cidr::new(Ipv4Addr::new(192, 168, third, 0), 24).unwrap_or(route)
}

fn conflicting_local_routes(
    local: &[LocalRouteTranslation],
    routes: impl IntoIterator<Item = Ipv4Cidr>,
) -> BTreeSet<String> {
    routes
        .into_iter()
        .filter(|route| {
            local
                .iter()
                .any(|t| routes_overlap(*route, t.local) || routes_overlap(*route, t.exported))
        })
        .map(|r| r.to_string())
        .collect()
}

fn routes_overlap(a: Ipv4Cidr, b: Ipv4Cidr) -> bool {
    let (as_, ae) = cidr_bounds(a);
    let (bs, be) = cidr_bounds(b);
    as_ <= be && bs <= ae
}

fn cidr_bounds(route: Ipv4Cidr) -> (u64, u64) {
    let start = u64::from(u32::from(route.first_address()));
    let host_bits = 32u32.saturating_sub(route.network_length() as u32);
    let size = match host_bits {
        0 => 1,
        32 => u64::from(u32::MAX) + 1,
        bits => 1u64 << bits,
    };
    (start, start + size - 1)
}

fn discover_local_routes(exclude: Option<&str>) -> Vec<Ipv4Cidr> {
    let mut routes = BTreeSet::new();
    for iface in get_if_addrs().unwrap_or_default() {
        if iface.is_loopback() || iface.is_link_local() {
            continue;
        }
        if should_skip_interface(&iface.name, exclude) {
            continue;
        }
        let IfAddr::V4(addr) = iface.addr else {
            continue;
        };
        if addr.prefixlen == 0 {
            continue;
        }
        if let Ok(cidr) = Ipv4Cidr::new(addr.ip, addr.prefixlen) {
            routes.insert(cidr);
        }
    }
    routes.into_iter().collect()
}

fn should_skip_interface(name: &str, exclude: Option<&str>) -> bool {
    if exclude.is_some_and(|e| e == name) {
        return true;
    }
    matches!(
        name,
        "lo" | "docker0" | "tailscale0" | "zt0" | "utun0" | "utun1" | "utun2" | "utun3"
    ) || name.starts_with("tun")
        || name.starts_with("tap")
        || name.starts_with("docker")
        || name.starts_with("veth")
        || name.starts_with("br-")
}

// ── Config parsing ───────────────────────────────────────────────────────────

fn parse_configured_peers(peers: &[String]) -> Result<HashSet<AddressHash>, VpnRuntimeError> {
    peers
        .iter()
        .map(|p| {
            AddressHash::new_from_hex_string(p)
                .map_err(|err| VpnRuntimeError::Config(format!("invalid peer '{p}': {err:?}")))
        })
        .collect()
}

fn validate_network(network: Ipv4Cidr) -> Result<(), VpnRuntimeError> {
    if network.network_length() > 30 {
        return Err(VpnRuntimeError::Config(format!(
            "vpn network {network} must have at least 2 host bits"
        )));
    }
    Ok(())
}

fn derive_tunnel_ip(
    network: Ipv4Cidr,
    destination: &AddressHash,
) -> Result<Ipv4Addr, VpnRuntimeError> {
    validate_network(network)?;
    let host_bits = 32 - u32::from(network.network_length());
    let usable = (1u64 << host_bits) - 2;
    let mut seed = 0u64;
    for byte in destination.as_slice() {
        seed = seed.wrapping_mul(131).wrapping_add(u64::from(*byte));
    }
    let offset = (seed % usable) + 1;
    let base = u32::from(network.first_address());
    Ok(Ipv4Addr::from(base.wrapping_add(offset as u32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(hex: &str) -> AddressHash {
        AddressHash::new_from_hex_string(hex).unwrap()
    }

    #[test]
    fn derive_tunnel_ip_stable_and_in_network() {
        let net: Ipv4Cidr = "10.20.0.0/16".parse().unwrap();
        let h = hash("fb08aff16ec6f5ccf0d3eb179028e9c3");
        let a = derive_tunnel_ip(net, &h).unwrap();
        let b = derive_tunnel_ip(net, &h).unwrap();
        assert_eq!(a, b);
        assert!(net.contains(&a));
    }

    #[test]
    fn export_local_route_lands_in_alias_pool() {
        let dest = hash("971a7ac9b42ce6e0faa131bb3c2e7852");
        let r: Ipv4Cidr = "192.168.10.0/24".parse().unwrap();
        let e = export_local_route(&dest, r);
        let oct = e.first_address().octets();
        assert_eq!(oct[0], 192);
        assert_eq!(oct[1], 168);
        assert!((100..=254).contains(&oct[2]));
        assert_ne!(e, r);
    }

    #[test]
    fn export_local_route_preserves_non_24_routes() {
        let dest = hash("971a7ac9b42ce6e0faa131bb3c2e7852");
        let r: Ipv4Cidr = "10.42.0.0/16".parse().unwrap();
        assert_eq!(export_local_route(&dest, r), r);
    }

    #[test]
    fn merge_routes_unions_inputs() {
        let d = vec![
            "192.168.10.0/24".parse().unwrap(),
            "10.50.0.0/24".parse().unwrap(),
        ];
        let a = vec![
            "10.50.0.0/24".parse().unwrap(),
            "172.16.1.0/24".parse().unwrap(),
        ];
        let m = merge_routes(&d, &a);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn local_conflicts_block_overlapping_remotes() {
        let locals = vec![LocalRouteTranslation {
            local: "192.168.10.0/24".parse().unwrap(),
            exported: "192.168.142.0/24".parse().unwrap(),
        }];
        let conflicts = conflicting_local_routes(
            &locals,
            [
                "192.168.10.0/24".parse().unwrap(),
                "192.168.142.0/24".parse().unwrap(),
                "192.168.177.0/24".parse().unwrap(),
            ],
        );
        assert!(conflicts.contains("192.168.10.0/24"));
        assert!(conflicts.contains("192.168.142.0/24"));
        assert!(!conflicts.contains("192.168.177.0/24"));
    }

    #[test]
    fn fresh_attempts_start_when_no_reusable_link_exists() {
        assert!(!should_reuse_existing_out_link(None));
        assert!(!should_reuse_existing_out_link(Some(LinkStatus::Closed)));
    }

    #[test]
    fn reusable_link_states_do_not_restart_attempts() {
        assert!(should_reuse_existing_out_link(Some(LinkStatus::Pending)));
        assert!(should_reuse_existing_out_link(Some(LinkStatus::Handshake)));
        assert!(should_reuse_existing_out_link(Some(LinkStatus::Active)));
        assert!(should_reuse_existing_out_link(Some(LinkStatus::Stale)));
    }

    #[test]
    fn stale_and_closed_links_are_left_to_transport_recovery() {
        assert!(!out_link_needs_reset(LinkStatus::Stale, 0));
        assert!(!out_link_needs_reset(LinkStatus::Closed, 0));
    }

    #[test]
    fn pending_links_only_reset_after_timeout() {
        assert!(!out_link_needs_reset(
            LinkStatus::Pending,
            LINK_PENDING_TIMEOUT_SECS - 1
        ));
        assert!(out_link_needs_reset(
            LinkStatus::Pending,
            LINK_PENDING_TIMEOUT_SECS
        ));
        assert!(out_link_needs_reset(
            LinkStatus::Handshake,
            LINK_PENDING_TIMEOUT_SECS
        ));
    }

    #[test]
    fn tun_tx_only_runs_without_backoff_when_link_is_active() {
        assert!(tun_tx_requires_backoff(LinkState::Configured));
        assert!(tun_tx_requires_backoff(LinkState::Pending));
        assert!(tun_tx_requires_backoff(LinkState::Closed));
        assert!(!tun_tx_requires_backoff(LinkState::Active));
    }

    #[test]
    fn reconnect_cooldown_blocks_new_attempts_until_expiry() {
        let peer = Peer::new(
            hash("971a7ac9b42ce6e0faa131bb3c2e7852"),
            "10.20.0.2".parse().unwrap(),
            LinkState::Closed,
        );
        peer.set_reconnect_cooldown(LINK_RECONNECT_COOLDOWN_SECS);
        let cooldown_until = peer.reconnect_cooldown_until();
        assert!(link_request_on_cooldown(
            &peer,
            cooldown_until.saturating_sub(1)
        ));
        assert!(!link_request_on_cooldown(&peer, cooldown_until));
    }

    #[test]
    fn only_one_link_request_can_be_started_at_a_time() {
        let peer = Peer::new(
            hash("fb08aff16ec6f5ccf0d3eb179028e9c3"),
            "10.20.0.3".parse().unwrap(),
            LinkState::Closed,
        );
        assert!(peer.try_begin_link_request());
        assert!(peer.link_request_in_flight());
        assert!(!peer.try_begin_link_request());
        peer.finish_link_request();
        assert!(!peer.link_request_in_flight());
        assert!(peer.try_begin_link_request());
    }
}
