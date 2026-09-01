use std::str::FromStr;

use rand::RngCore;
use rusqlite::{params, Connection, Result};
use serde_json;

use crate::config::GatewayConfig;
use crate::radio::{HardwareRadioConfig, RadioModuleConfig};

const DEFAULT_NETWORK: &str = "10.20.0.0/16";
const DEFAULT_ANNOUNCE_FREQ_SECS: u32 = 5;
const DEFAULT_ADVERTISED_ROUTES: &str = "[\"192.168.10.0/24\"]";
const DEFAULT_ALLOW_ALL_PEERS: bool = true;
const CODENAME_KEY: &str = "system_codename";
const CODENAME_LEN: usize = 8;
const CODENAME_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS peers (
                destination_hash TEXT PRIMARY KEY
            );",
        )
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT value FROM settings WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        Ok(if let Some(row) = rows.next()? {
            Some(row.get(0)?)
        } else {
            None
        })
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn load_or_create_seed(&self) -> Result<String> {
        self.load_or_create_named_seed("identity_seed")
    }

    pub fn load_or_create_named_seed(&self, key: &str) -> Result<String> {
        if let Some(seed) = self.get(key)? {
            return Ok(seed);
        }
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let seed = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        self.set(key, &seed)?;
        log::info!("generated new seed for '{key}'");
        Ok(seed)
    }

    pub fn load_or_create_codename(&self) -> Result<String> {
        if let Some(codename) = self.get(CODENAME_KEY)? {
            return Ok(codename);
        }
        let codename = generate_codename();
        self.set(CODENAME_KEY, &codename)?;
        log::info!("generated system codename '{codename}'");
        Ok(codename)
    }

    pub fn save_codename(&self, codename: &str) -> Result<()> {
        self.set(CODENAME_KEY, codename)
    }

    pub fn load_config(&self) -> Result<GatewayConfig> {
        let network_str = self
            .get("network")?
            .unwrap_or_else(|| DEFAULT_NETWORK.to_string());
        let network = cidr::Ipv4Cidr::from_str(&network_str).map_err(|e| {
            rusqlite::Error::InvalidParameterName(format!("invalid network '{network_str}': {e}"))
        })?;

        let announce_freq_secs = self
            .get("announce_freq_secs")?
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(DEFAULT_ANNOUNCE_FREQ_SECS);
        let allow_all_peers = self
            .get("allow_all_peers")?
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(DEFAULT_ALLOW_ALL_PEERS);

        let peers = {
            let mut stmt = self
                .conn
                .prepare("SELECT destination_hash FROM peers ORDER BY destination_hash")?;
            let rows = stmt.query_map([], |row| row.get(0))?;
            rows.collect::<Result<Vec<String>>>()?
        };

        let advertised_routes = self
            .get("advertised_routes")?
            .unwrap_or_else(|| DEFAULT_ADVERTISED_ROUTES.to_string());
        let advertised_routes = serde_json::from_str(&advertised_routes).unwrap_or_default();

        let defaults = HardwareRadioConfig::default();
        let module_configs = std::array::from_fn(|i| {
            let suffix = format!("_{i}");
            let radio_config = self
                .get(&format!("kaonic_ctrl_radio_config{suffix}"))
                .ok()
                .flatten()
                .or_else(|| {
                    if i == 0 {
                        self.get("kaonic_ctrl_radio_config").ok()?
                    } else {
                        None
                    }
                })
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or_else(|| defaults.module_configs[i].radio_config.clone());
            let modulation = self
                .get(&format!("kaonic_ctrl_modulation{suffix}"))
                .ok()
                .flatten()
                .or_else(|| {
                    if i == 0 {
                        self.get("kaonic_ctrl_modulation").ok()?
                    } else {
                        None
                    }
                })
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or_else(|| defaults.module_configs[i].modulation.clone());
            let accelerator = self
                .get(&format!("kaonic_ctrl_accelerator{suffix}"))
                .ok()
                .flatten()
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or_else(|| defaults.module_configs[i].accelerator);
            let antenna = self
                .get(&format!("kaonic_ctrl_antenna{suffix}"))
                .ok()
                .flatten()
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or_else(|| defaults.module_configs[i].antenna);
            RadioModuleConfig {
                radio_config,
                modulation,
                accelerator,
                antenna,
            }
        });

        Ok(GatewayConfig {
            network,
            allow_all_peers,
            peers,
            advertised_routes,
            announce_freq_secs,
            radio: HardwareRadioConfig { module_configs },
        })
    }

    pub fn save_config(&self, config: &GatewayConfig) -> Result<()> {
        self.set("network", &config.network.to_string())?;
        self.set(
            "advertised_routes",
            &serde_json::to_string(&config.advertised_routes).unwrap(),
        )?;
        self.set("allow_all_peers", &config.allow_all_peers.to_string())?;
        self.set("announce_freq_secs", &config.announce_freq_secs.to_string())?;

        self.conn.execute("DELETE FROM peers", [])?;
        for peer in &config.peers {
            self.conn.execute(
                "INSERT OR IGNORE INTO peers (destination_hash) VALUES (?1)",
                params![peer],
            )?;
        }

        for (i, module_cfg) in config.radio.module_configs.iter().enumerate() {
            self.save_module_config(i, module_cfg)?;
        }

        Ok(())
    }

    /// Save a single module's radio+modulation config by module index.
    pub fn save_module_config(&self, module: usize, cfg: &RadioModuleConfig) -> Result<()> {
        let suffix = format!("_{module}");
        self.set(
            &format!("kaonic_ctrl_radio_config{suffix}"),
            &serde_json::to_string(&cfg.radio_config).unwrap(),
        )?;
        self.set(
            &format!("kaonic_ctrl_modulation{suffix}"),
            &serde_json::to_string(&cfg.modulation).unwrap(),
        )?;
        self.set(
            &format!("kaonic_ctrl_accelerator{suffix}"),
            &serde_json::to_string(&cfg.accelerator).unwrap(),
        )?;
        self.set(
            &format!("kaonic_ctrl_antenna{suffix}"),
            &serde_json::to_string(&cfg.antenna).unwrap(),
        )?;
        Ok(())
    }
}

fn generate_codename() -> String {
    let mut bytes = [0u8; CODENAME_LEN];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
        .into_iter()
        .map(|byte| CODENAME_ALPHABET[(byte as usize) % CODENAME_ALPHABET.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::Database;

    #[test]
    fn codename_is_created_once_and_persisted() {
        let db = Database::open(":memory:").expect("open db");

        let first = db.load_or_create_codename().expect("create codename");
        let second = db.load_or_create_codename().expect("load codename");

        assert_eq!(first, second);
        assert_eq!(first.len(), 8);
        assert!(first
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn codename_can_be_overridden() {
        let db = Database::open(":memory:").expect("open db");

        db.save_codename("abcd1234").expect("save codename");

        assert_eq!(
            db.load_or_create_codename().expect("load codename"),
            "abcd1234"
        );
    }
}
