use kaonic_vpn::VpnSnapshot;
use radio_common::{
    modulation::{Modulation, OfdmModulation},
    Accelerator, Antenna, RadioConfig,
};
use serde::{Deserialize, Serialize};

// ── Radio ────────────────────────────────────────────────────────────────────

/// WASM-safe mirror of `crate::radio::RadioModuleConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadioModuleConfigDto {
    pub radio_config: RadioConfig,
    pub modulation: Modulation,
    #[serde(default = "crate::radio::default_accelerator")]
    pub accelerator: Accelerator,
    #[serde(default = "crate::radio::default_antenna")]
    pub antenna: Antenna,
}

impl Default for RadioModuleConfigDto {
    fn default() -> Self {
        Self {
            radio_config: radio_common::RadioConfigBuilder::new().build(),
            modulation: Modulation::Ofdm(OfdmModulation::default()),
            accelerator: crate::radio::default_accelerator(),
            antenna: crate::radio::default_antenna(),
        }
    }
}

// ── Received frames ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RxFrameDto {
    pub module: usize,
    pub direction: String,
    pub rssi: i8,
    pub len: u16,
    pub hex: String, // hex preview of first 16 bytes
    pub ascii: String,
    pub crc32: String,
    pub ts: u64, // unix timestamp (seconds)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FrameStatsDto {
    pub rx_frames: u64,
    pub rx_bytes: u64,
    pub rx_bps: u64,
    pub tx_frames: u64,
    pub tx_bytes: u64,
    pub tx_bps: u64,
    pub last_rssi: Option<i8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemStatusDto {
    pub cpu_percent: f32,
    /// Current CPU clock in MHz (0 if unavailable).
    pub cpu_freq_mhz: u32,
    pub ram_used_mb: u64,
    pub ram_total_mb: u64,
    pub fs_free_mb: u64,
    pub fs_total_mb: u64,
    pub os_details: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceStatusDto {
    pub unit: String,
    pub brief_name: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub status: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginSummaryDto {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub service: String,
    pub developer: String,
    pub channel: Option<String>,
    pub webview: Option<u16>,
    pub tls: bool,
    #[serde(default)]
    pub icon: Option<String>,
    pub binary_name: String,
    pub bin_path: Option<String>,
    pub sha256: String,
    pub install_dir: String,
    pub package_path: String,
    pub official: bool,
    pub enabled: bool,
    pub removable: bool,
    pub target_name: Option<String>,
    pub status: String,
    pub installed_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginMessageDto {
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkPortStatusDto {
    pub name: String,
    pub protocol: String,
    pub port: u16,
    pub service: String,
    pub status: String,
    pub details: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReticulumLinkDto {
    pub id: String,
    pub destination: String,
    pub status: String,
    pub last_event: String,
    pub packets: u64,
    pub bytes: u64,
    pub rtt_ms: Option<u64>,
    pub last_seen_ts: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReticulumInterfaceStatsDto {
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_ldpc_errors: u64,
    pub rx_reassembly_errors: u64,
    pub rx_deserialize_errors: u64,
    pub tx_ldpc_errors: u64,
    pub tx_transmit_errors: u64,
    pub tx_serialize_errors: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReticulumEventDto {
    pub ts: u64,
    pub direction: String,
    pub kind: String,
    pub link_id: String,
    pub destination: String,
    pub details: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReticulumSnapshotDto {
    pub interface_stats: ReticulumInterfaceStatsDto,
    pub incoming_links: Vec<ReticulumLinkDto>,
    pub outgoing_links: Vec<ReticulumLinkDto>,
    pub events: Vec<ReticulumEventDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WsReticulumSnapshotDto {
    pub interface_stats: ReticulumInterfaceStatsDto,
    pub incoming_links: Vec<ReticulumLinkDto>,
    pub outgoing_links: Vec<ReticulumLinkDto>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WsInterfacesDto {
    pub wlan0_ip: Option<String>,
    pub usb0_ip: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WsRadioFramesDto {
    pub module: usize,
    pub frames: Vec<RxFrameDto>,
    pub stats: FrameStatsDto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WsStatusEvent {
    Interfaces(WsInterfacesDto),
    NetworkPorts(Vec<NetworkPortStatusDto>),
    System(SystemStatusDto),
    Services(Vec<ServiceStatusDto>),
    Vpn(VpnSnapshot),
    Reticulum(WsReticulumSnapshotDto),
    RadioFrames(WsRadioFramesDto),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayStatusDto {
    pub serial: String,
    pub vpn_hash: String,
    pub network_ports: Vec<NetworkPortStatusDto>,
    pub system: SystemStatusDto,
    pub services: Vec<ServiceStatusDto>,
    pub radio_modules: Vec<RadioModuleConfigDto>,
    pub reticulum: ReticulumSnapshotDto,
    pub vpn: VpnSnapshot,
}

impl Default for GatewayStatusDto {
    fn default() -> Self {
        Self {
            serial: String::new(),
            vpn_hash: String::new(),
            network_ports: vec![],
            system: SystemStatusDto::default(),
            services: vec![],
            radio_modules: vec![],
            reticulum: ReticulumSnapshotDto::default(),
            vpn: VpnSnapshot::default(),
        }
    }
}

// ── Network ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WifiStatusDto {
    pub mode: String,
    pub antenna: String,
    pub antenna_supported: bool,
    pub configured_ssid: Option<String>,
    pub connected_ssid: Option<String>,
    pub wlan0_ip: Option<String>,
    pub hostapd_status: String,
    pub wpa_supplicant_status: String,
    pub link_details: String,
}

impl Default for WifiStatusDto {
    fn default() -> Self {
        Self {
            mode: "ap".into(),
            antenna: "internal".into(),
            antenna_supported: false,
            configured_ssid: None,
            connected_ssid: None,
            wlan0_ip: None,
            hostapd_status: String::new(),
            wpa_supplicant_status: String::new(),
            link_details: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkSnapshotDto {
    pub backend: String,
    pub interface_source: String,
    pub interface_details: String,
    pub wifi: WifiStatusDto,
}

impl Default for NetworkSnapshotDto {
    fn default() -> Self {
        Self {
            backend: String::new(),
            interface_source: String::new(),
            interface_details: String::new(),
            wifi: WifiStatusDto::default(),
        }
    }
}

// ── Settings ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewaySettingsDto {
    /// CIDR network string, e.g. `"10.0.0.0/24"`.
    pub network: String,
    pub allow_all_peers: bool,
    pub peers: Vec<String>,
    pub advertised_routes: Vec<String>,
    pub announce_freq_secs: u32,
    /// Exactly 2 radio modules.
    pub radio_modules: [RadioModuleConfigDto; 2],
}

impl Default for GatewaySettingsDto {
    fn default() -> Self {
        Self {
            network: "10.0.0.0/24".into(),
            allow_all_peers: true,
            peers: vec![],
            advertised_routes: vec!["192.168.10.0/24".into()],
            announce_freq_secs: 5,
            radio_modules: [
                RadioModuleConfigDto::default(),
                RadioModuleConfigDto::default(),
            ],
        }
    }
}

use crate::radio::{HardwareRadioConfig, RadioModuleConfig};

impl From<RadioModuleConfig> for RadioModuleConfigDto {
    fn from(c: RadioModuleConfig) -> Self {
        Self {
            radio_config: c.radio_config,
            modulation: c.modulation,
            accelerator: c.accelerator,
            antenna: c.antenna,
        }
    }
}

impl From<RadioModuleConfigDto> for RadioModuleConfig {
    fn from(d: RadioModuleConfigDto) -> Self {
        Self {
            radio_config: d.radio_config,
            modulation: d.modulation,
            accelerator: d.accelerator,
            antenna: d.antenna,
        }
    }
}

impl From<crate::config::GatewayConfig> for GatewaySettingsDto {
    fn from(c: crate::config::GatewayConfig) -> Self {
        Self {
            network: c.network.to_string(),
            allow_all_peers: c.allow_all_peers,
            peers: c.peers,
            advertised_routes: c
                .advertised_routes
                .into_iter()
                .map(|route| route.to_string())
                .collect(),
            announce_freq_secs: c.announce_freq_secs,
            radio_modules: [
                c.radio.module_configs[0].clone().into(),
                c.radio.module_configs[1].clone().into(),
            ],
        }
    }
}

impl TryFrom<GatewaySettingsDto> for crate::config::GatewayConfig {
    type Error = String;

    fn try_from(d: GatewaySettingsDto) -> Result<Self, Self::Error> {
        let network = d
            .network
            .parse()
            .map_err(|e| format!("invalid network CIDR '{}: {e}", d.network))?;
        let advertised_routes = d
            .advertised_routes
            .into_iter()
            .map(|route| {
                route
                    .parse()
                    .map_err(|e| format!("invalid advertised route '{route}': {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(crate::config::GatewayConfig {
            network,
            allow_all_peers: d.allow_all_peers,
            peers: d.peers,
            advertised_routes,
            announce_freq_secs: d.announce_freq_secs,
            radio: HardwareRadioConfig {
                module_configs: [
                    RadioModuleConfig::from(d.radio_modules[0].clone()),
                    RadioModuleConfig::from(d.radio_modules[1].clone()),
                ],
            },
        })
    }
}
