use std::sync::Arc;

use kaonic_ctrl::protocol::RADIO_FRAME_SIZE;
use kaonic_ctrl::radio::RadioClient;
use kaonic_frame::frame::Frame;
use kaonic_reticulum::{ErrorObserver, KaonicCtrlInterface, RxObserver, TxObserver};
use radio_common::{
    Accelerator, Antenna, Hertz, Modulation, RadioBand, RadioConfig, RadioConfigBuilder,
};
use reticulum::transport::Transport;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub type SharedRadioClient = Arc<Mutex<RadioClient>>;
pub type SharedTxObserver = TxObserver;
pub type SharedErrorObserver = ErrorObserver;

/// Default baseband acceleration mode for a radio module.
pub fn default_accelerator() -> Accelerator {
    Accelerator::Native
}

/// Default antenna for the 2.4 GHz band (the only band with an antenna switch).
pub fn default_antenna() -> Antenna {
    Antenna::Internal
}

/// Lowest frequency the 2.4 GHz transceiver (RF24) covers.
const BAND_24_MIN_HZ: u64 = 2_400_000_000;

/// Band a module operates in, derived from its tuned frequency.
pub fn band_for_config(radio_config: &RadioConfig) -> RadioBand {
    if radio_config.freq.as_hz() >= BAND_24_MIN_HZ {
        RadioBand::Band24
    } else {
        RadioBand::Band09
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RadioModuleConfig {
    pub radio_config: RadioConfig,
    pub modulation: Modulation,
    /// Baseband acceleration: on-chip (`Native`) or external FPGA (`Hardware`).
    #[serde(default = "default_accelerator")]
    pub accelerator: Accelerator,
    /// Antenna selection for the 2.4 GHz band. The sub-GHz path has no antenna
    /// switch on this board — it is always wired to the external connector — so
    /// this is only applied when the module is tuned to 2.4 GHz.
    #[serde(default = "default_antenna")]
    pub antenna: Antenna,
}

impl RadioModuleConfig {
    /// Band this module currently operates in.
    pub fn band(&self) -> RadioBand {
        band_for_config(&self.radio_config)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HardwareRadioConfig {
    pub module_configs: [RadioModuleConfig; 2],
}

impl Default for HardwareRadioConfig {
    fn default() -> Self {
        Self {
            module_configs: [
                RadioModuleConfig {
                    radio_config: RadioConfigBuilder::new()
                        .freq(Hertz::new(869_535_000))
                        .channel_spacing(Hertz::new(200_000))
                        .channel(3)
                        .build(),
                    modulation: Modulation::Ofdm(
                        radio_common::modulation::OfdmModulation::default(),
                    ),
                    accelerator: default_accelerator(),
                    antenna: default_antenna(),
                },
                RadioModuleConfig {
                    radio_config: RadioConfigBuilder::new()
                        .freq(Hertz::new(869_535_000))
                        .channel_spacing(Hertz::new(200_000))
                        .channel(11)
                        .build(),
                    modulation: Modulation::Ofdm(
                        radio_common::modulation::OfdmModulation::default(),
                    ),
                    accelerator: default_accelerator(),
                    antenna: default_antenna(),
                },
            ],
        }
    }
}

/// Connect to the kaonic-ctrl daemon. One connection serves all hardware modules.
pub async fn connect_radio_client(
    listen_addr: std::net::SocketAddr,
    server_addr: std::net::SocketAddr,
) -> Result<SharedRadioClient, kaonic_ctrl::error::ControllerError> {
    KaonicCtrlInterface::connect_client::<1400, 5>(
        listen_addr,
        server_addr,
        CancellationToken::new(),
    )
    .await
}

/// Apply saved per-module hardware settings and wire `rns_module` into the transport.
pub async fn attach_radio_interface(
    transport: &Arc<Mutex<Transport>>,
    radio_client: SharedRadioClient,
    radio: &HardwareRadioConfig,
    rns_module: usize,
    rx_observer: Option<RxObserver>,
    tx_observer: Option<SharedTxObserver>,
    error_observer: Option<SharedErrorObserver>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for (module, cfg) in radio.module_configs.iter().enumerate() {
        log::info!("applying saved radio config on boot (module {module})");
        if let Err(e) = radio_client
            .lock()
            .await
            .set_radio_config(module, cfg.radio_config.clone())
            .await
        {
            log::warn!("boot radio config error for module {module}: {e:?}");
        }
        if let Err(e) = radio_client
            .lock()
            .await
            .set_modulation(module, cfg.modulation.clone())
            .await
        {
            log::warn!("boot modulation error for module {module}: {e:?}");
        }
        if let Err(e) = radio_client
            .lock()
            .await
            .set_accelerator(module, cfg.accelerator)
            .await
        {
            log::warn!("boot accelerator error for module {module}: {e:?}");
        }
        // Only the 2.4 GHz front-end has an antenna switch; sub-GHz is hard-wired
        // to the external connector, so there is nothing to select for Band09.
        if cfg.band() == RadioBand::Band24 {
            if let Err(e) = radio_client
                .lock()
                .await
                .set_antenna(module, RadioBand::Band24, cfg.antenna)
                .await
            {
                log::warn!("boot antenna error for module {module}: {e:?}");
            }
        }
    }

    let iface = KaonicCtrlInterface::new(
        radio_client,
        rns_module,
        rx_observer,
        tx_observer,
        error_observer
    );
    let iface_mgr = transport.lock().await.iface_manager();
    iface_mgr
        .lock()
        .await
        .spawn(iface, KaonicCtrlInterface::spawn);

    Ok(())
}

pub async fn transmit_test_frame(
    radio_client: Option<SharedRadioClient>,
    tx_observer: Option<SharedTxObserver>,
    module: usize,
    payload: &[u8],
) -> Result<(), String> {
    if module > 1 {
        return Err(format!("radio module {module} not found"));
    }

    if payload.is_empty() {
        return Err("message is required".into());
    }

    let max_len = RADIO_FRAME_SIZE.min(2047);
    if payload.len() > max_len {
        return Err(format!("message exceeds {max_len} bytes"));
    }

    let Some(radio_client) = radio_client else {
        return Err("radio backend unavailable".into());
    };

    let mut frame = Frame::<RADIO_FRAME_SIZE>::new();
    frame.copy_from_slice(payload);

    let transmit_result = radio_client
        .lock()
        .await
        .transmit(module, &frame)
        .await
        .map_err(|err| format!("transmit: {err:?}"));

    if transmit_result.is_ok() {
        if let Some(observer) = tx_observer {
            observer(module, payload);
        }
    }

    transmit_result
}
