use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use kaonic_reticulum::InterfaceErrorKind;
use reticulum::destination::link::{Link, LinkEvent, LinkEventData, LinkStatus};
use reticulum::transport::{AnnounceEvent, ReceivedData, Transport};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::Mutex;

use crate::app_types::{
    ReticulumEventDto, ReticulumInterfaceStatsDto, ReticulumLinkDto, ReticulumSnapshotDto,
};

const RETICULUM_EVENT_BUF_SIZE: usize = 256;

pub type SharedGatewayReticulum = Arc<GatewayReticulum>;

#[derive(Default)]
struct GatewayReticulumState {
    interface_stats: ReticulumInterfaceStatsDto,
    incoming_links: HashMap<String, ReticulumLinkDto>,
    outgoing_links: HashMap<String, ReticulumLinkDto>,
    events: VecDeque<ReticulumEventDto>,
}

#[derive(Default)]
pub struct GatewayReticulum {
    state: Mutex<GatewayReticulumState>,
}

impl GatewayReticulum {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn snapshot(&self) -> ReticulumSnapshotDto {
        let state = self.state.lock().await;

        let mut incoming_links = state.incoming_links.values().cloned().collect::<Vec<_>>();
        incoming_links.sort_by(|a, b| {
            b.last_seen_ts
                .cmp(&a.last_seen_ts)
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut outgoing_links = state.outgoing_links.values().cloned().collect::<Vec<_>>();
        outgoing_links.sort_by(|a, b| {
            b.last_seen_ts
                .cmp(&a.last_seen_ts)
                .then_with(|| a.id.cmp(&b.id))
        });

        ReticulumSnapshotDto {
            interface_stats: state.interface_stats.clone(),
            incoming_links,
            outgoing_links,
            events: state.events.iter().cloned().collect(),
        }
    }

    pub async fn record_interface_error(&self, module: usize, kind: InterfaceErrorKind) {
        let ts = unix_timestamp_secs();
        let mut state = self.state.lock().await;
        let stats = &mut state.interface_stats;
        match kind {
            InterfaceErrorKind::RxLdpcDecode => {
                stats.rx_errors += 1;
                stats.rx_ldpc_errors += 1;
            }
            InterfaceErrorKind::RxReassembly => {
                stats.rx_errors += 1;
                stats.rx_reassembly_errors += 1;
            }
            InterfaceErrorKind::RxDeserialize => {
                stats.rx_errors += 1;
                stats.rx_deserialize_errors += 1;
            }
            InterfaceErrorKind::TxLdpcEncode => {
                stats.tx_errors += 1;
                stats.tx_ldpc_errors += 1;
            }
            InterfaceErrorKind::TxTransmit => {
                stats.tx_errors += 1;
                stats.tx_transmit_errors += 1;
            }
            InterfaceErrorKind::TxSerialize => {
                stats.tx_errors += 1;
                stats.tx_serialize_errors += 1;
            }
        }

        push_event(
            &mut state.events,
            ReticulumEventDto {
                ts,
                direction: "interface".into(),
                kind: "error".into(),
                link_id: String::new(),
                destination: String::new(),
                details: format!("module {module}: {}", interface_error_label(kind)),
            },
        );
    }

    pub async fn attach(self: &Arc<Self>, transport: Arc<Mutex<Transport>>) {
        let mut in_rx = transport.lock().await.in_link_events();
        let mut out_rx = transport.lock().await.out_link_events();
        let mut data_rx = transport.lock().await.received_data_events();
        let mut ann_rx = transport.lock().await.recv_announces().await;

        {
            let tracker = self.clone();
            let transport = transport.clone();
            tokio::spawn(async move {
                loop {
                    match in_rx.recv().await {
                        Ok(event) => tracker.handle_link_event(&transport, true, event).await,
                        Err(RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }

        {
            let tracker = self.clone();
            let transport = transport.clone();
            tokio::spawn(async move {
                loop {
                    match out_rx.recv().await {
                        Ok(event) => tracker.handle_link_event(&transport, false, event).await,
                        Err(RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }

        {
            let tracker = self.clone();
            tokio::spawn(async move {
                loop {
                    match data_rx.recv().await {
                        Ok(event) => tracker.handle_received_data(event).await,
                        Err(RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }

        {
            let tracker = self.clone();
            tokio::spawn(async move {
                loop {
                    match ann_rx.recv().await {
                        Ok(event) => tracker.handle_announce(event).await,
                        Err(RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }
    }

    async fn handle_link_event(
        &self,
        transport: &Arc<Mutex<Transport>>,
        incoming: bool,
        event: LinkEventData,
    ) {
        let ts = unix_timestamp_secs();
        let kind = link_event_kind(&event.event);
        let details = link_event_details(&event.event);
        let packet_len = link_event_packet_len(&event.event);
        let status = link_event_status(&event.event);

        let mut snapshot = self
            .resolve_link_snapshot(transport, incoming, &event, &kind, ts)
            .await;
        snapshot.status = status.into();
        snapshot.last_event = kind.clone();
        snapshot.last_seen_ts = ts;

        let mut state = self.state.lock().await;
        let links = if incoming {
            &mut state.incoming_links
        } else {
            &mut state.outgoing_links
        };

        let entry = links
            .entry(snapshot.id.clone())
            .or_insert_with(|| snapshot.clone());
        entry.destination = snapshot.destination;
        entry.status = snapshot.status;
        entry.rtt_ms = snapshot.rtt_ms;
        entry.last_event = snapshot.last_event;
        entry.last_seen_ts = snapshot.last_seen_ts;
        if let Some(packet_len) = packet_len {
            entry.packets += 1;
            entry.bytes += packet_len as u64;
        }

        push_event(
            &mut state.events,
            ReticulumEventDto {
                ts,
                direction: if incoming { "incoming" } else { "outgoing" }.into(),
                kind,
                link_id: event.id.to_hex_string(),
                destination: event.address_hash.to_hex_string(),
                details,
            },
        );
    }

    async fn handle_received_data(&self, event: ReceivedData) {
        let mut state = self.state.lock().await;
        push_event(
            &mut state.events,
            ReticulumEventDto {
                ts: unix_timestamp_secs(),
                direction: "transport".into(),
                kind: "received-data".into(),
                link_id: String::new(),
                destination: event.destination.to_hex_string(),
                details: format!("{} B", event.data.as_slice().len()),
            },
        );
    }

    async fn handle_announce(&self, event: AnnounceEvent) {
        let destination = event
            .destination
            .lock()
            .await
            .desc
            .address_hash
            .to_hex_string();
        let mut state = self.state.lock().await;
        push_event(
            &mut state.events,
            ReticulumEventDto {
                ts: unix_timestamp_secs(),
                direction: "transport".into(),
                kind: "announce".into(),
                link_id: String::new(),
                destination,
                details: format!("app data {} B", event.app_data.as_slice().len()),
            },
        );
    }

    async fn resolve_link_snapshot(
        &self,
        transport: &Arc<Mutex<Transport>>,
        incoming: bool,
        event: &LinkEventData,
        last_event: &str,
        ts: u64,
    ) -> ReticulumLinkDto {
        let link = if incoming {
            transport.lock().await.find_in_link(&event.id).await
        } else {
            transport
                .lock()
                .await
                .find_out_link(&event.address_hash)
                .await
        };

        if let Some(link) = link {
            let link = link.lock().await;
            return link_snapshot(&link, last_event, ts);
        }

        ReticulumLinkDto {
            id: event.id.to_hex_string(),
            destination: event.address_hash.to_hex_string(),
            status: link_event_status(&event.event).into(),
            last_event: last_event.into(),
            packets: 0,
            bytes: 0,
            rtt_ms: None,
            last_seen_ts: ts,
        }
    }
}

fn link_snapshot(link: &Link, last_event: &str, ts: u64) -> ReticulumLinkDto {
    ReticulumLinkDto {
        id: link.id().to_hex_string(),
        destination: link.destination().address_hash.to_hex_string(),
        status: link_status_label(link.status()).into(),
        last_event: last_event.into(),
        packets: 0,
        bytes: 0,
        rtt_ms: Some(link.rtt().as_millis() as u64),
        last_seen_ts: ts,
    }
}

fn link_status_label(status: LinkStatus) -> &'static str {
    match status {
        LinkStatus::Pending => "pending",
        LinkStatus::Handshake => "handshake",
        LinkStatus::Active => "active",
        LinkStatus::Stale => "stale",
        LinkStatus::Closed => "closed",
    }
}

fn link_event_kind(event: &LinkEvent) -> String {
    match event {
        LinkEvent::Activated => "activated".into(),
        LinkEvent::Data(_) => "data".into(),
        LinkEvent::Proof(_) => "proof".into(),
        LinkEvent::Closed => "closed".into(),
        LinkEvent::RemoteIdentified(_) => "identified".into(),
    }
}

fn link_event_details(event: &LinkEvent) -> String {
    match event {
        LinkEvent::Activated => "Link activated".into(),
        LinkEvent::Data(payload) => format!("{} B", payload.as_slice().len()),
        LinkEvent::Proof(hash) => format!("{hash}"),
        LinkEvent::Closed => "Link closed".into(),
        LinkEvent::RemoteIdentified(identity) => format!("Remote identity {}", identity.address_hash)
    }
}

fn link_event_packet_len(event: &LinkEvent) -> Option<usize> {
    match event {
        LinkEvent::Data(payload) => Some(payload.as_slice().len()),
        _ => None,
    }
}

fn link_event_status(event: &LinkEvent) -> &'static str {
    match event {
        LinkEvent::Activated | LinkEvent::Data(_) | LinkEvent::Proof(_) |
            LinkEvent::RemoteIdentified(_) => "active",
        LinkEvent::Closed => "closed",
    }
}

fn push_event(events: &mut VecDeque<ReticulumEventDto>, event: ReticulumEventDto) {
    events.push_front(event);
    events.truncate(RETICULUM_EVENT_BUF_SIZE);
}

fn interface_error_label(kind: InterfaceErrorKind) -> &'static str {
    match kind {
        InterfaceErrorKind::RxLdpcDecode => "rx ldpc decode",
        InterfaceErrorKind::RxReassembly => "rx reassembly",
        InterfaceErrorKind::RxDeserialize => "rx deserialize",
        InterfaceErrorKind::TxLdpcEncode => "tx ldpc encode",
        InterfaceErrorKind::TxTransmit => "tx transmit",
        InterfaceErrorKind::TxSerialize => "tx serialize",
    }
}

fn unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
