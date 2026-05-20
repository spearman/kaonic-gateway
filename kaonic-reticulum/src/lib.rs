use std::net::SocketAddr;
use std::sync::Arc;

use kaonic_ctrl::client::Client;
use kaonic_ctrl::error::ControllerError;
use kaonic_ctrl::protocol::{Message, MessageCoder, RADIO_FRAME_SIZE};
use kaonic_frame::frame::{Frame, FrameSegment};
use kaonic_net::{
    coder::LdpcPacketCoder, error::NetworkError as KaonicNetError,
    network::Network as KaonicNetNetwork,
};
use rand::rngs::OsRng;
use reticulum::buffer::{InputBuffer, OutputBuffer};
use reticulum::iface::{Interface, InterfaceContext, RxMessage, TxMessage};
use reticulum::packet::Packet;
use reticulum::serde::Serialize;
use tokio::sync::Mutex;
use tokio::time;
use tokio_util::sync::CancellationToken;

pub use kaonic_ctrl::radio::RadioClient;

pub type TxObserver = Arc<dyn Fn(usize, &[u8]) + Send + Sync>;
pub type ErrorObserver = Arc<dyn Fn(usize, InterfaceErrorKind) + Send + Sync>;
const LDPC_SEGMENTS_PER_PACKET: usize = 3;
const LDPC_REASSEMBLY_QUEUE: usize = 32;

type RadioPacketCoder = LdpcPacketCoder<RADIO_FRAME_SIZE>;
type RadioNetwork = KaonicNetNetwork<
    RADIO_FRAME_SIZE,
    LDPC_SEGMENTS_PER_PACKET,
    LDPC_REASSEMBLY_QUEUE,
    RadioPacketCoder,
>;
type RadioSegmentBuffer = FrameSegment<RADIO_FRAME_SIZE, LDPC_SEGMENTS_PER_PACKET>;

/// Reticulum interface that forwards packets through the kaonic radio hardware
/// via the kaonic-ctrl UDP control protocol.
///
/// A single `RadioClient` connection handles all hardware modules; the module
/// index is passed as a parameter on every call. Create one `RadioClient` via
/// `connect_client`, then build one `KaonicCtrlInterface` per module with `new`.
pub struct KaonicCtrlInterface {
    radio_client: Arc<Mutex<RadioClient>>,
    module: usize,
    tx_observer: Option<TxObserver>,
    error_observer: Option<ErrorObserver>,
}

#[derive(Clone, Copy, Debug)]
pub enum InterfaceErrorKind {
    RxLdpcDecode,
    RxReassembly,
    RxDeserialize,
    TxLdpcEncode,
    TxTransmit,
    TxSerialize,
}

impl KaonicCtrlInterface {
    /// Connect to the kaonic-ctrl daemon and return the shared client.
    /// One connection is sufficient for all hardware modules.
    pub async fn connect_client<const MTU: usize, const R: usize>(
        listen_addr: SocketAddr,
        server_addr: SocketAddr,
        cancel: CancellationToken,
    ) -> Result<Arc<Mutex<RadioClient>>, ControllerError> {
        let client = Client::<Message>::connect::<MTU, R, MessageCoder<MTU, R>>(
            listen_addr,
            server_addr,
            MessageCoder::new(),
            cancel.clone(),
        )
        .await?;
        Ok(Arc::new(Mutex::new(
            RadioClient::new(client, cancel).await?,
        )))
    }

    /// Create an interface for `module` using an already-connected `RadioClient`.
    pub fn new(
        radio_client: Arc<Mutex<RadioClient>>,
        module: usize,
        tx_observer: Option<TxObserver>,
        error_observer: Option<ErrorObserver>,
    ) -> Self {
        Self {
            radio_client,
            module,
            tx_observer,
            error_observer,
        }
    }

    /// Spawn the interface tasks. Matches the pattern used by other Reticulum interfaces.
    pub async fn spawn(context: InterfaceContext<Self>) {
        let (radio_client, module, tx_observer, error_observer) = {
            let inner = context.inner.lock().unwrap();
            (
                inner.radio_client.clone(),
                inner.module,
                inner.tx_observer.clone(),
                inner.error_observer.clone(),
            )
        };

        let iface_address = context.channel.address;
        let (rx_channel, mut tx_channel) = context.channel.split();
        let cancel = context.cancel;

        let mut rx_recv = radio_client.lock().await.module_receive();

        let rx_task = {
            let cancel = cancel.clone();
            let rx_channel = rx_channel.clone();
            let error_observer = error_observer.clone();

            tokio::spawn(async move {
                let mut rx_network = build_radio_network();
                let mut rx_frame = RadioSegmentBuffer::new();
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        Ok(recv_module) = rx_recv.recv() => {
                            if recv_module.module == module {
                                let current_time = network_time_now();
                                let frame_bytes = recv_module.frame.as_slice();
                                let mut frame = Frame::<RADIO_FRAME_SIZE>::new();
                                frame.copy_from_slice(frame_bytes);
                                let start = time::Instant::now();
                                if let Err(err) = rx_network.receive(current_time, &frame) {
                                    notify_error(&error_observer, module, InterfaceErrorKind::RxLdpcDecode);
                                    log::warn!(
                                        "kaonic_ctrl: rx ldpc decode failed module={} len={} preview={} err={err:?}",
                                        module,
                                        frame_bytes.len(),
                                        frame_preview(frame_bytes)
                                    );
                                    continue;
                                }
                                log::info!("rx_network.receive {}", start.elapsed().as_nanos());

                                let start = time::Instant::now();
                                loop {
                                    match rx_network.process(current_time, &mut rx_frame) {
                                        Ok(assembled) => {
                                            let bytes = assembled.as_slice();
                                            let mut input = InputBuffer::new(bytes);
                                            match Packet::deserialize(&mut input) {
                                                Ok(packet) => {
                                                    log::trace!(
                                                        "kaonic_ctrl: rx module={} rssi={} packet_id={} {}",
                                                        module,
                                                        recv_module.rssi,
                                                        assembled.id(),
                                                        packet_log_summary(&packet)
                                                    );
                                                    let _ = rx_channel
                                                        .send(RxMessage { address: iface_address, packet })
                                                        .await;
                                                }
                                                Err(err) => {
                                                    notify_error(&error_observer, module, InterfaceErrorKind::RxDeserialize);
                                                    log::warn!(
                                                        "kaonic_ctrl: rx deserialize failed module={} len={} preview={} err={err:?}",
                                                        module,
                                                        bytes.len(),
                                                        frame_preview(bytes)
                                                    );
                                                }
                                            }
                                        }
                                        Err(KaonicNetError::TryAgain) => break,
                                        Err(err) => {
                                            notify_error(&error_observer, module, InterfaceErrorKind::RxReassembly);
                                            log::warn!(
                                                "kaonic_ctrl: rx ldpc reassembly failed module={} len={} preview={} err={err:?}",
                                                module,
                                                frame_bytes.len(),
                                                frame_preview(frame_bytes)
                                            );
                                            break;
                                        }
                                    }
                                }
                                log::info!("rx_network.proces {}", start.elapsed().as_nanos());
                            }
                        }
                    }
                }
            })
        };

        let tx_task = {
            let cancel = cancel.clone();
            let radio_client = radio_client.clone();
            let tx_observer = tx_observer.clone();
            let error_observer = error_observer.clone();

            tokio::spawn(async move {
                const BUF_SIZE: usize = reticulum::packet::PACKET_MDU * 2;
                let mut tx_buffer = [0u8; BUF_SIZE];
                let mut tx_network = build_radio_network();
                let mut tx_frames = [Frame::<RADIO_FRAME_SIZE>::new(); LDPC_SEGMENTS_PER_PACKET];

                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        Some(message) = tx_channel.recv() => {
                            let start = time::Instant::now();
                            transmit_message(
                                &radio_client,
                                module,
                                &tx_observer,
                                &error_observer,
                                &mut tx_network,
                                &mut tx_frames,
                                &mut tx_buffer,
                                message,
                            ).await;
                            log::info!("transmit_message {}", start.elapsed().as_nanos());
                        }
                        else => break,
                    }
                }
            })
        };

        let _ = tokio::join!(rx_task, tx_task);
    }
}

fn packet_log_summary(packet: &Packet) -> String {
    format!(
        "type={:?} ctx={:?} dst={} len={}",
        packet.header.packet_type,
        packet.context,
        packet.destination,
        packet.data.len()
    )
}

fn frame_preview(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

async fn transmit_message(
    radio_client: &Arc<Mutex<RadioClient>>,
    module: usize,
    tx_observer: &Option<TxObserver>,
    error_observer: &Option<ErrorObserver>,
    tx_network: &mut RadioNetwork,
    tx_frames: &mut [Frame<RADIO_FRAME_SIZE>; LDPC_SEGMENTS_PER_PACKET],
    tx_buffer: &mut [u8],
    message: TxMessage,
) {
    let mut output = OutputBuffer::new(tx_buffer);
    if let Ok(_) = message.packet.serialize(&mut output) {
        let bytes = output.as_slice();
        match tx_network.transmit(bytes, OsRng, tx_frames) {
            Ok(frames) => {
                log::trace!(
                    "kaonic_ctrl: tx module={} {} payload_len={} encoded_frames={}",
                    module,
                    packet_log_summary(&message.packet),
                    bytes.len(),
                    frames.len()
                );

                let mut radio_client = radio_client.lock().await;
                for frame in frames {
                    let frame_bytes = frame.as_slice();
                    if let Err(err) = radio_client.transmit(module, frame).await {
                        notify_error(error_observer, module, InterfaceErrorKind::TxTransmit);
                        log::warn!(
                            "kaonic_ctrl: tx failed module={} {} payload_len={} frame_len={} err={err:?}",
                            module,
                            packet_log_summary(&message.packet),
                            bytes.len(),
                            frame_bytes.len()
                        );
                        return;
                    }
                    if let Some(observer) = tx_observer {
                        observer(module, frame_bytes);
                    }
                }
            }
            Err(err) => {
                notify_error(error_observer, module, InterfaceErrorKind::TxLdpcEncode);
                log::warn!(
                    "kaonic_ctrl: tx ldpc encode failed module={} {} payload_len={} err={err:?}",
                    module,
                    packet_log_summary(&message.packet),
                    bytes.len()
                );
            }
        }
    } else {
        notify_error(error_observer, module, InterfaceErrorKind::TxSerialize);
        log::warn!(
            "kaonic_ctrl: packet serialize failed module={} {}",
            module,
            packet_log_summary(&message.packet)
        );
    }
    // Under sustained transmit load, explicitly yield so Reticulum
    // maintenance tasks get time to refresh links and process control traffic.
    tokio::task::yield_now().await;
}

fn notify_error(observer: &Option<ErrorObserver>, module: usize, kind: InterfaceErrorKind) {
    if let Some(observer) = observer {
        observer(module, kind);
    }
}

fn build_radio_network() -> RadioNetwork {
    KaonicNetNetwork::new(LdpcPacketCoder::new())
}

fn network_time_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

impl Interface for KaonicCtrlInterface {
    fn mtu() -> usize {
        RADIO_FRAME_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ldpc_radio_network_round_trips_full_reticulum_payload() {
        let original = [0x5a; reticulum::packet::PACKET_MDU];
        let mut tx_network = build_radio_network();
        let mut rx_network = build_radio_network();
        let mut tx_frames = [Frame::<RADIO_FRAME_SIZE>::new(); LDPC_SEGMENTS_PER_PACKET];
        let mut rx_frame = RadioSegmentBuffer::new();

        let frames = tx_network
            .transmit(&original, OsRng, &mut tx_frames)
            .expect("encoded ldpc frames");

        let mut recovered = None;
        for (idx, frame) in frames.iter().enumerate() {
            let ts = idx as u128;
            rx_network.receive(ts, frame).expect("accepted ldpc frame");
            if let Ok(packet) = rx_network.process(ts, &mut rx_frame) {
                recovered = Some(packet.as_slice().to_vec());
            }
        }

        assert_eq!(recovered.as_deref(), Some(original.as_slice()));
    }
}
