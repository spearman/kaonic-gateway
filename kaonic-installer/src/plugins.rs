use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use kaonic_gateway::local_https;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::installer::{apply_update, Target};

const MANIFEST_NAME: &str = "kaonic-plugin.toml";

#[derive(Debug, Clone, Copy)]
pub enum PluginAction {
    Start,
    Stop,
    Restart,
}

impl PluginAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PluginError {
    pub status: StatusCode,
    pub detail: String,
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for PluginError {}

impl PluginError {
    fn bad_request(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            detail: detail.into(),
        }
    }

    fn not_found(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            detail: detail.into(),
        }
    }

    fn internal(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            detail: detail.into(),
        }
    }
}

type PluginResult<T> = Result<T, PluginError>;

#[derive(Debug, Clone)]
pub struct CorePluginSpec {
    pub target: Arc<Target>,
    pub plugin_id: String,
    pub plugin_dir: PathBuf,
}

impl CorePluginSpec {
    pub fn new(target: Arc<Target>, plugins_root: &Path) -> Self {
        let plugin_id = target
            .symlink_path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(target.name)
            .to_string();
        Self {
            target,
            plugin_dir: plugins_root.join(&plugin_id),
            plugin_id,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PluginManifest {
    name: String,
    description: String,
    version: String,
    service: String,
    developer: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    github_url: Option<String>,
    #[serde(default)]
    webview: Option<u16>,
    #[serde(default)]
    tls: bool,
    /// Icon file shipped with the plugin, relative to the plugin runtime
    /// directory (`<install_dir>/current`) — i.e. `files/icon.png` in the
    /// package is declared here as `icon = "icon.png"`.
    #[serde(default)]
    icon: Option<String>,
    #[serde(default, alias = "bin_path")]
    symlink: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PluginManifestFile {
    #[serde(rename = "kaonic-plugin")]
    plugin: PluginManifest,
}

#[derive(Debug, Clone)]
struct PluginPackage {
    manifest: PluginManifest,
    manifest_raw: String,
    id: String,
    binary_name: String,
    service_name: String,
    bin_path: Option<String>,
    sha256: String,
    binary_bytes: Vec<u8>,
    service_bytes: Vec<u8>,
    custom_files: Vec<PluginPackageFile>,
    signature_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct PluginPackageFile {
    relative_path: PathBuf,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
struct PluginRecord {
    id: String,
    name: String,
    description: String,
    version: String,
    service: String,
    developer: String,
    channel: Option<String>,
    github_url: Option<String>,
    webview: Option<u16>,
    tls: bool,
    icon: Option<String>,
    binary_name: String,
    bin_path: Option<String>,
    sha256: String,
    install_dir: String,
    package_path: String,
    official: bool,
    enabled: bool,
    removable: bool,
    target_name: Option<String>,
    installed_at: u64,
    updated_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub service: String,
    pub developer: String,
    pub channel: Option<String>,
    pub github_url: Option<String>,
    pub webview: Option<u16>,
    pub tls: bool,
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
    pub systemd_status: PluginSystemdStatus,
    pub installed_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginSystemdStatus {
    pub active_state: String,
    pub sub_state: String,
    pub unit_file_state: String,
    pub main_pid: Option<u32>,
    pub tasks_current: Option<u64>,
    pub memory_current: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginMessage {
    pub detail: String,
}

pub fn initialize_store(
    plugins_root: &Path,
    db_path: &Path,
    systemd_dir: &Path,
    cert_path: Option<&Path>,
    core_plugins: &[CorePluginSpec],
) -> PluginResult<()> {
    log::debug!(
        "initializing plugin store root={} db={} core_plugins={}",
        plugins_root.display(),
        db_path.display(),
        core_plugins.len()
    );
    fs::create_dir_all(plugins_root)
        .map_err(|err| PluginError::internal(format!("create plugin root: {err}")))?;
    let conn = open_db(db_path)?;
    init_db(&conn)?;
    recover_plugins_after_interruption(plugins_root, &conn, systemd_dir, cert_path)?;
    discover_plugins(plugins_root, &conn, cert_path)?;
    sync_core_plugins(&conn, core_plugins)?;
    reconcile_plugin_bin_paths(&conn)?;
    Ok(())
}

/// Icon bytes for an installed plugin, read from its runtime directory.
#[derive(Debug, Clone)]
pub struct PluginIcon {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
}

pub fn read_plugin_icon(db_path: &Path, plugin_id: &str) -> PluginResult<PluginIcon> {
    let conn = open_db(db_path)?;
    let record = load_plugin(&conn, plugin_id)?
        .ok_or_else(|| PluginError::not_found(format!("unknown plugin {plugin_id}")))?;
    let relative = normalize_icon_path(record.icon.as_deref())?
        .ok_or_else(|| PluginError::not_found(format!("plugin {plugin_id} declares no icon")))?;
    let path = Path::new(&record.install_dir)
        .join("current")
        .join(&relative);
    let bytes = fs::read(&path).map_err(|err| {
        PluginError::not_found(format!("read plugin icon {}: {err}", path.display()))
    })?;

    Ok(PluginIcon {
        content_type: icon_content_type(&relative),
        bytes,
    })
}

pub fn list_plugins(
    plugins_root: &Path,
    db_path: &Path,
    systemd_dir: &Path,
    cert_path: Option<&Path>,
    core_plugins: &[CorePluginSpec],
) -> PluginResult<Vec<PluginSummary>> {
    initialize_store(plugins_root, db_path, systemd_dir, cert_path, core_plugins)?;
    log::debug!("loading plugin summaries from {}", db_path.display());
    let cached_status = refresh_systemd_status_cache(db_path)?;
    list_plugins_with_cached_status(db_path, &cached_status)
}

pub fn list_plugins_with_cached_status(
    db_path: &Path,
    systemd_status_by_service: &HashMap<String, PluginSystemdStatus>,
) -> PluginResult<Vec<PluginSummary>> {
    let conn = open_db(db_path)?;
    init_db(&conn)?;
    build_plugin_summaries(load_all_plugins(&conn)?, systemd_status_by_service)
}

pub fn refresh_systemd_status_cache(
    db_path: &Path,
) -> PluginResult<HashMap<String, PluginSystemdStatus>> {
    let conn = open_db(db_path)?;
    init_db(&conn)?;
    let mut statuses = HashMap::new();
    for record in load_all_plugins(&conn)? {
        statuses.insert(
            record.service.clone(),
            read_service_systemd_status(&record.service),
        );
    }
    Ok(statuses)
}

pub fn log_boot_inventory(plugins_root: &Path, db_path: &Path) -> PluginResult<()> {
    let conn = open_db(db_path)?;
    init_db(&conn)?;

    let mut stmt = conn
        .prepare(
            "SELECT id, name, version, service, removable, target_name, install_dir, package_path
             FROM plugins
             ORDER BY name COLLATE NOCASE, id",
        )
        .map_err(|err| PluginError::internal(format!("prepare boot inventory query: {err}")))?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)? != 0,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .map_err(|err| PluginError::internal(format!("query boot inventory: {err}")))?;

    let mut plugins = Vec::new();
    for row in rows {
        plugins.push(
            row.map_err(|err| PluginError::internal(format!("read boot inventory row: {err}")))?,
        );
    }

    log::info!("boot inventory: {} registered plugins", plugins.len());
    for (id, name, version, service, removable, target_name, install_dir, package_path) in plugins {
        log::debug!(
            "boot plugin id={} name={} version={} service={} removable={} target_name={:?} install_dir={} package_path={}",
            id,
            name,
            version,
            service,
            removable,
            target_name,
            install_dir,
            package_path
        );
    }

    let mut physical_files = Vec::new();
    collect_plugin_paths(plugins_root, plugins_root, &mut physical_files)?;
    physical_files.sort();

    log::info!(
        "boot inventory: {} physical entries under {}",
        physical_files.len(),
        plugins_root.display()
    );
    for path in physical_files {
        log::debug!("boot file {}", path);
    }

    Ok(())
}

pub fn install_plugin(
    plugins_root: &Path,
    db_path: &Path,
    systemd_dir: &Path,
    cert_path: &Path,
    core_plugins: &[CorePluginSpec],
    zip_bytes: &[u8],
) -> PluginResult<PluginMessage> {
    initialize_store(
        plugins_root,
        db_path,
        systemd_dir,
        Some(cert_path),
        core_plugins,
    )?;
    let normalized_zip =
        crate::installer::normalize_uploaded_zip(zip_bytes).map_err(PluginError::bad_request)?;
    let package = parse_plugin_package(normalized_zip.as_ref())?;
    log::info!(
        "installing plugin id={} version={} service={} payload_bytes={}",
        package.id,
        package.manifest.version,
        package.service_name,
        normalized_zip.len()
    );
    let conn = open_db(db_path)?;
    let existing = load_plugin(&conn, &package.id)?;
    log::debug!(
        "plugin install state id={} existing={} enabled_preference={}",
        package.id,
        existing.is_some(),
        existing
            .as_ref()
            .map(|record| record.enabled)
            .unwrap_or(true)
    );
    ensure_service_available(&conn, &package.service_name, Some(&package.id))?;
    ensure_bin_path_available(&conn, package.bin_path.as_deref(), Some(&package.id))?;

    let plugin_dir = plugins_root.join(&package.id);
    let current_dir = plugin_dir.join("current");
    let package_path = plugin_dir.join("package.zip");
    let manifest_path = plugin_dir.join(MANIFEST_NAME);
    let stored_service_path = plugin_dir.join(&package.service_name);
    let stored_hash_path = plugin_dir.join(format!("{}.sha256", package.binary_name));
    let stored_signature_path = plugin_dir.join(format!("{}.sig", package.binary_name));
    let legacy_signature_path = plugin_dir.join(format!("{}.sign", package.binary_name));
    let service_path = systemd_dir.join(&package.service_name);
    let binary_path = current_dir.join(&package.binary_name);
    let binary_target = binary_path.clone();
    let normalized_service_bytes =
        normalize_service_working_directory(&package.service_bytes, &current_dir)?;
    let now = now_secs();
    let official = evaluate_official(
        cert_path,
        &package.binary_bytes,
        package.signature_bytes.as_deref(),
    )?;
    let should_enable = existing
        .as_ref()
        .map(|record| record.enabled)
        .unwrap_or(true);
    let was_running = existing
        .as_ref()
        .map(|record| is_service_running(&record.service))
        .unwrap_or(false);
    let should_start = existing.is_none() || was_running;
    let rollback_dir = plugin_dir.join(".rollback");
    let had_existing_service = service_path.exists();

    fs::create_dir_all(&plugin_dir)
        .map_err(|err| PluginError::internal(format!("create plugin dir: {err}")))?;
    prepare_rollback(
        &plugin_dir,
        &rollback_dir,
        &service_path,
        &package.service_name,
    )?;
    prepare_bin_path_rollback(
        &rollback_dir,
        existing
            .as_ref()
            .and_then(|record| record.bin_path.as_deref()),
        existing.as_ref().map(|record| {
            plugin_binary_target(Path::new(&record.install_dir), &record.binary_name)
        }),
    )?;

    if existing.is_some() || had_existing_service {
        let _ = systemctl("stop", &package.service_name);
    }

    let install_result = (|| -> PluginResult<()> {
        if current_dir.exists() {
            log::debug!(
                "removing previous plugin files from {}",
                current_dir.display()
            );
            fs::remove_dir_all(&current_dir).map_err(|err| {
                PluginError::internal(format!("remove previous plugin files: {err}"))
            })?;
        }
        fs::create_dir_all(&current_dir)
            .map_err(|err| PluginError::internal(format!("create current plugin dir: {err}")))?;

        fs::write(&binary_path, &package.binary_bytes)
            .map_err(|err| PluginError::internal(format!("write plugin binary: {err}")))?;
        make_executable(&binary_path)
            .map_err(|err| PluginError::internal(format!("chmod plugin binary: {err}")))?;
        install_custom_files(&current_dir, &package.custom_files)?;
        if package.manifest.tls {
            local_https::ensure_plugin_tls_files(&current_dir, &package.id).map_err(|err| {
                PluginError::internal(format!("generate plugin TLS files: {err}"))
            })?;
        }
        fs::write(&package_path, normalized_zip.as_ref())
            .map_err(|err| PluginError::internal(format!("persist plugin package: {err}")))?;
        fs::write(&manifest_path, package.manifest_raw.as_bytes())
            .map_err(|err| PluginError::internal(format!("persist plugin manifest: {err}")))?;
        fs::write(&stored_service_path, &normalized_service_bytes)
            .map_err(|err| PluginError::internal(format!("persist plugin service: {err}")))?;
        fs::write(&stored_hash_path, format!("{}\n", package.sha256))
            .map_err(|err| PluginError::internal(format!("persist plugin checksum: {err}")))?;
        if let Some(signature) = &package.signature_bytes {
            fs::write(&stored_signature_path, signature)
                .map_err(|err| PluginError::internal(format!("persist plugin signature: {err}")))?;
            if legacy_signature_path.exists() {
                fs::remove_file(&legacy_signature_path).map_err(|err| {
                    PluginError::internal(format!("remove legacy plugin signature: {err}"))
                })?;
            }
        } else if stored_signature_path.exists() {
            fs::remove_file(&stored_signature_path).map_err(|err| {
                PluginError::internal(format!("remove stale plugin signature: {err}"))
            })?;
        } else if legacy_signature_path.exists() {
            fs::remove_file(&legacy_signature_path).map_err(|err| {
                PluginError::internal(format!("remove stale legacy plugin signature: {err}"))
            })?;
        }

        fs::create_dir_all(systemd_dir)
            .map_err(|err| PluginError::internal(format!("create systemd dir: {err}")))?;
        fs::write(&service_path, &normalized_service_bytes)
            .map_err(|err| PluginError::internal(format!("install systemd unit: {err}")))?;
        log::debug!(
            "installed plugin unit id={} unit_path={}",
            package.id,
            service_path.display()
        );
        daemon_reload()?;
        install_bin_path(
            package.bin_path.as_deref(),
            existing
                .as_ref()
                .and_then(|record| record.bin_path.as_deref()),
            &binary_target,
            &rollback_dir,
        )?;

        if should_enable {
            let _ = systemctl("enable", &package.service_name);
        } else {
            let _ = systemctl("disable", &package.service_name);
        }

        if should_start {
            log::debug!(
                "starting plugin service id={} service={}",
                package.id,
                package.service_name
            );
            systemctl("start", &package.service_name)
                .map_err(|err| PluginError::internal(format!("start plugin service: {err}")))?;
            if !poll_running(&package.service_name, 10, Duration::from_secs(1)) {
                return Err(PluginError::internal(
                    "plugin failed health check after install; rolled back",
                ));
            }
        }

        upsert_plugin(
            &conn,
            PluginRecord {
                id: package.id.clone(),
                name: package.manifest.name.clone(),
                description: package.manifest.description.clone(),
                version: package.manifest.version.clone(),
                service: package.service_name.clone(),
                developer: package.manifest.developer.clone(),
                channel: normalize_channel(package.manifest.channel.as_deref())?,
                github_url: normalize_github_url(package.manifest.github_url.as_deref()),
                webview: normalize_webview_port(package.manifest.webview)?,
                tls: package.manifest.tls,
                icon: normalize_icon_path(package.manifest.icon.as_deref())?,
                binary_name: package.binary_name.clone(),
                bin_path: package.bin_path.clone(),
                sha256: package.sha256.clone(),
                install_dir: plugin_dir.to_string_lossy().into_owned(),
                package_path: package_path.to_string_lossy().into_owned(),
                official,
                enabled: should_enable,
                removable: true,
                target_name: None,
                installed_at: existing
                    .as_ref()
                    .map(|record| record.installed_at)
                    .unwrap_or(now),
                updated_at: now,
            },
        )?;

        Ok(())
    })();

    if let Err(err) = install_result {
        log::error!("plugin install failed id={} detail={}", package.id, err);
        let _ = systemctl("stop", &package.service_name);
        let _ = rollback_install(
            &plugin_dir,
            &rollback_dir,
            &service_path,
            &package.service_name,
            package.bin_path.as_deref(),
            &binary_target,
            existing.as_ref(),
        );
        return Err(err);
    }

    cleanup_rollback(&rollback_dir);
    log::info!(
        "plugin install completed id={} version={} updated={}",
        package.id,
        package.manifest.version,
        existing.is_some()
    );
    Ok(PluginMessage {
        detail: if existing.is_some() {
            format!(
                "Updated plugin {} to {}",
                package.manifest.name, package.manifest.version
            )
        } else {
            format!(
                "Installed plugin {} {}",
                package.manifest.name, package.manifest.version
            )
        },
    })
}

pub fn control_plugin(
    db_path: &Path,
    plugin_id: &str,
    action: PluginAction,
) -> PluginResult<PluginMessage> {
    let conn = open_db(db_path)?;
    init_db(&conn)?;
    let plugin = load_plugin(&conn, plugin_id)?
        .ok_or_else(|| PluginError::not_found(format!("unknown plugin: {plugin_id}")))?;
    log::info!(
        "controlling plugin id={} name={} action={} service={}",
        plugin_id,
        plugin.name,
        action.as_str(),
        plugin.service
    );

    systemctl(action.as_str(), &plugin.service)
        .map_err(|err| PluginError::internal(format!("{} service: {err}", action.as_str())))?;

    let enabled = match action {
        PluginAction::Start | PluginAction::Restart => {
            let _ = systemctl("enable", &plugin.service);
            true
        }
        PluginAction::Stop => false,
    };
    if matches!(action, PluginAction::Stop) {
        let _ = systemctl("disable", &plugin.service);
    }

    update_plugin_enabled(&conn, plugin_id, enabled)?;
    log::info!(
        "plugin control completed id={} action={} enabled={}",
        plugin_id,
        action.as_str(),
        enabled
    );
    Ok(PluginMessage {
        detail: format!("{} {}", plugin.name, action.as_str()),
    })
}

pub fn delete_plugin(
    plugins_root: &Path,
    db_path: &Path,
    systemd_dir: &Path,
    plugin_id: &str,
) -> PluginResult<PluginMessage> {
    let conn = open_db(db_path)?;
    init_db(&conn)?;
    let plugin = load_plugin(&conn, plugin_id)?
        .ok_or_else(|| PluginError::not_found(format!("unknown plugin: {plugin_id}")))?;
    log::info!(
        "deleting plugin id={} name={} service={}",
        plugin_id,
        plugin.name,
        plugin.service
    );
    if plugin.target_name.is_some() || !plugin.removable {
        return Err(PluginError::bad_request(format!(
            "plugin {} is built-in and cannot be removed",
            plugin.name
        )));
    }
    let plugin_dir = plugins_root.join(plugin_id);
    let service_path = systemd_dir.join(&plugin.service);

    let _ = systemctl("stop", &plugin.service);
    let _ = systemctl("disable", &plugin.service);
    if let Some(bin_path) = plugin.bin_path.as_deref() {
        remove_managed_symlink(
            Path::new(bin_path),
            &plugin_binary_target(Path::new(&plugin.install_dir), &plugin.binary_name),
        )?;
    }
    if service_path.exists() {
        fs::remove_file(&service_path)
            .map_err(|err| PluginError::internal(format!("remove service file: {err}")))?;
    }
    daemon_reload()?;
    if plugin_dir.exists() {
        fs::remove_dir_all(&plugin_dir)
            .map_err(|err| PluginError::internal(format!("remove plugin files: {err}")))?;
    }
    conn.execute("DELETE FROM plugins WHERE id = ?1", params![plugin_id])
        .map_err(|err| PluginError::internal(format!("delete plugin record: {err}")))?;
    log::info!("deleted plugin id={} name={}", plugin_id, plugin.name);

    Ok(PluginMessage {
        detail: format!("Deleted plugin {}", plugin.name),
    })
}

pub fn upload_plugin_update(
    plugins_root: &Path,
    db_path: &Path,
    systemd_dir: &Path,
    cert_path: &Path,
    core_plugins: &[CorePluginSpec],
    plugin_id: &str,
    zip_bytes: &[u8],
) -> PluginResult<PluginMessage> {
    initialize_store(
        plugins_root,
        db_path,
        systemd_dir,
        Some(cert_path),
        core_plugins,
    )?;
    let conn = open_db(db_path)?;
    let plugin = load_plugin(&conn, plugin_id)?
        .ok_or_else(|| PluginError::not_found(format!("unknown plugin: {plugin_id}")))?;
    let normalized_zip =
        crate::installer::normalize_uploaded_zip(zip_bytes).map_err(PluginError::bad_request)?;
    log::info!(
        "uploading plugin update id={} built_in={} payload_bytes={}",
        plugin_id,
        plugin.target_name.is_some(),
        normalized_zip.len()
    );

    if let Some(target_name) = plugin.target_name.as_deref() {
        let spec = core_plugins
            .iter()
            .find(|spec| spec.target.name == target_name)
            .ok_or_else(|| {
                PluginError::internal(format!(
                    "missing built-in target mapping for plugin {plugin_id}"
                ))
            })?;
        log::info!(
            "dispatching built-in plugin update id={} target={}",
            plugin_id,
            target_name
        );
        let detail =
            apply_update(&spec.target, normalized_zip.as_ref()).map_err(PluginError::internal)?;
        sync_core_plugins(&conn, core_plugins)?;
        return Ok(PluginMessage { detail });
    }

    let package = parse_plugin_package(normalized_zip.as_ref())?;
    log::debug!(
        "parsed plugin update package id={} version={}",
        package.id,
        package.manifest.version
    );
    if package.id != plugin_id {
        return Err(PluginError::bad_request(format!(
            "plugin package targets {}, expected {plugin_id}",
            package.id
        )));
    }
    install_plugin(
        plugins_root,
        db_path,
        systemd_dir,
        cert_path,
        core_plugins,
        normalized_zip.as_ref(),
    )
}

fn parse_plugin_package(zip_bytes: &[u8]) -> PluginResult<PluginPackage> {
    let mut archive = ZipArchive::new(Cursor::new(zip_bytes))
        .map_err(|err| PluginError::bad_request(format!("bad plugin ZIP: {err}")))?;
    log::debug!(
        "parsing plugin ZIP payload_bytes={} entries={}",
        zip_bytes.len(),
        archive.len()
    );
    let manifest_raw = read_zip_entry_text(&mut archive, MANIFEST_NAME)?;
    let manifest: PluginManifest = toml::from_str::<PluginManifestFile>(&manifest_raw)
        .map(|file| file.plugin)
        .map_err(|err| PluginError::bad_request(format!("bad plugin manifest: {err}")))?;
    validate_manifest(&manifest)?;

    let service_name = manifest.service.trim().to_string();
    let binary_name = derive_binary_name(&service_name)?;
    let bin_path = normalize_bin_path(manifest.symlink.as_deref())?;
    let id = binary_name.clone();
    let sha256_name = format!("{binary_name}.sha256");
    let service_bytes = read_zip_entry_bytes(&mut archive, &service_name)?;
    let binary_bytes = read_zip_entry_bytes(&mut archive, &binary_name)?;
    let sha256 = read_zip_entry_text(&mut archive, &sha256_name)?
        .trim()
        .to_string();
    validate_sha256_text(&sha256)?;
    verify_sha256(&binary_bytes, &sha256, "plugin package binary")?;
    let custom_files = read_custom_files(&mut archive, &binary_name)?;
    if let Some(icon) = normalize_icon_path(manifest.icon.as_deref())? {
        let icon_path = PathBuf::from(&icon);
        if !custom_files
            .iter()
            .any(|file| file.relative_path == icon_path)
        {
            return Err(PluginError::bad_request(format!(
                "plugin manifest declares icon {icon} but the package has no files/{icon} entry"
            )));
        }
    }
    let signature_bytes = read_optional_signature_bytes(&mut archive, &binary_name)?;
    log::debug!(
        "parsed plugin manifest id={} version={} service={} has_signature={} has_symlink={} custom_files={}",
        id,
        manifest.version,
        service_name,
        signature_bytes.is_some(),
        bin_path.is_some(),
        custom_files.len()
    );

    Ok(PluginPackage {
        manifest,
        manifest_raw,
        id,
        binary_name,
        service_name,
        bin_path,
        sha256,
        binary_bytes,
        service_bytes,
        custom_files,
        signature_bytes,
    })
}

fn discover_plugins(
    plugins_root: &Path,
    conn: &Connection,
    cert_path: Option<&Path>,
) -> PluginResult<()> {
    log::debug!(
        "scanning plugin root for discovery path={}",
        plugins_root.display()
    );
    let entries = fs::read_dir(plugins_root)
        .map_err(|err| PluginError::internal(format!("scan plugin root: {err}")))?;

    for entry in entries {
        let entry = entry
            .map_err(|err| PluginError::internal(format!("read plugin directory entry: {err}")))?;
        let plugin_dir = entry.path();
        if !entry
            .file_type()
            .map_err(|err| PluginError::internal(format!("inspect plugin entry type: {err}")))?
            .is_dir()
        {
            continue;
        }

        let plugin_id = entry.file_name().to_string_lossy().into_owned();
        if plugin_id.starts_with('.') {
            continue;
        }
        if load_plugin(conn, &plugin_id)?.is_some() {
            continue;
        }

        match discover_plugin_record(&plugin_dir, cert_path) {
            Ok(record) => {
                if let Err(err) = ensure_service_available(conn, &record.service, Some(&record.id))
                {
                    log::warn!(
                        "skipping plugin discovery in {}: {}",
                        plugin_dir.display(),
                        err
                    );
                    continue;
                }
                if let Err(err) =
                    ensure_bin_path_available(conn, record.bin_path.as_deref(), Some(&record.id))
                {
                    log::warn!(
                        "skipping plugin discovery in {}: {}",
                        plugin_dir.display(),
                        err
                    );
                    continue;
                }
                upsert_plugin(conn, record.clone())?;
                log::info!("discovered preinstalled plugin {}", record.id);
            }
            Err(err) => {
                log::warn!(
                    "skipping plugin discovery in {}: {}",
                    plugin_dir.display(),
                    err
                );
            }
        }
    }

    Ok(())
}

fn recover_plugins_after_interruption(
    plugins_root: &Path,
    conn: &Connection,
    systemd_dir: &Path,
    cert_path: Option<&Path>,
) -> PluginResult<()> {
    let entries = fs::read_dir(plugins_root)
        .map_err(|err| PluginError::internal(format!("scan plugin root for recovery: {err}")))?;

    for entry in entries {
        let entry = entry.map_err(|err| {
            PluginError::internal(format!("read recovery directory entry: {err}"))
        })?;
        if !entry
            .file_type()
            .map_err(|err| PluginError::internal(format!("inspect recovery entry type: {err}")))?
            .is_dir()
        {
            continue;
        }

        let plugin_id = entry.file_name().to_string_lossy().into_owned();
        if plugin_id.starts_with('.') {
            continue;
        }

        let plugin_dir = entry.path();
        let rollback_dir = plugin_dir.join(".rollback");
        if !rollback_dir.exists() {
            continue;
        }

        let existing = load_plugin(conn, &plugin_id)?;
        match discover_plugin_record(&plugin_dir, cert_path) {
            Ok(_) => {
                log::warn!(
                    "cleaning stale rollback data for plugin id={} path={}",
                    plugin_id,
                    rollback_dir.display()
                );
                cleanup_rollback(&rollback_dir);
            }
            Err(current_err) => match discover_plugin_record(&rollback_dir, cert_path) {
                Ok(rollback_record) => {
                    log::warn!(
                        "recovering plugin id={} after interrupted update: {}",
                        plugin_id,
                        current_err
                    );
                    let attempted_target = existing
                        .as_ref()
                        .map(current_binary_path)
                        .unwrap_or_else(|| {
                            plugin_binary_target(&plugin_dir, &rollback_record.binary_name)
                        });
                    rollback_install(
                        &plugin_dir,
                        &rollback_dir,
                        &systemd_dir.join(&rollback_record.service),
                        &rollback_record.service,
                        existing
                            .as_ref()
                            .and_then(|record| record.bin_path.as_deref()),
                        &attempted_target,
                        existing.as_ref(),
                    )?;
                    log::info!("recovered plugin id={} from rollback state", plugin_id);
                }
                Err(rollback_err) => {
                    log::error!(
                        "plugin id={} has invalid current state after power loss and rollback is unusable: current={} rollback={}",
                        plugin_id,
                        current_err,
                        rollback_err
                    );
                }
            },
        }
    }

    Ok(())
}

fn discover_plugin_record(
    plugin_dir: &Path,
    cert_path: Option<&Path>,
) -> PluginResult<PluginRecord> {
    let manifest_path = plugin_dir.join(MANIFEST_NAME);
    let manifest_raw = fs::read_to_string(&manifest_path)
        .map_err(|err| PluginError::internal(format!("read plugin manifest: {err}")))?;
    let manifest: PluginManifest = toml::from_str::<PluginManifestFile>(&manifest_raw)
        .map(|file| file.plugin)
        .map_err(|err| PluginError::bad_request(format!("bad plugin manifest: {err}")))?;
    validate_manifest(&manifest)?;

    let service_name = manifest.service.trim().to_string();
    let binary_name = derive_binary_name(&service_name)?;
    let current_dir = plugin_dir.join("current");
    if manifest.tls && current_dir.is_dir() {
        local_https::ensure_plugin_tls_files(&current_dir, &binary_name).map_err(|err| {
            PluginError::internal(format!("generate discovered plugin TLS files: {err}"))
        })?;
    }
    let binary_path = current_dir.join(&binary_name);
    let service_path = plugin_dir.join(&service_name);
    let hash_path = plugin_dir.join(format!("{}.sha256", binary_name));
    let package_path = plugin_dir.join("package.zip");
    let signature_path = resolve_signature_file_path(plugin_dir, &binary_name);

    if !binary_path.is_file() {
        return Err(PluginError::bad_request(format!(
            "missing plugin binary {}",
            binary_path.display()
        )));
    }
    if !service_path.is_file() {
        return Err(PluginError::bad_request(format!(
            "missing plugin service {}",
            service_path.display()
        )));
    }
    if !hash_path.is_file() {
        return Err(PluginError::bad_request(format!(
            "missing plugin checksum {}",
            hash_path.display()
        )));
    }

    let binary_bytes = fs::read(&binary_path)
        .map_err(|err| PluginError::internal(format!("read plugin binary: {err}")))?;
    let sha256 = read_sha256_file(&hash_path)?;
    verify_sha256(&binary_bytes, &sha256, "installed plugin binary")?;
    let signature_bytes = if let Some(signature_path) = signature_path {
        Some(
            fs::read(&signature_path)
                .map_err(|err| PluginError::internal(format!("read plugin signature: {err}")))?,
        )
    } else {
        None
    };

    let official = match cert_path {
        Some(cert_path) => evaluate_official(cert_path, &binary_bytes, signature_bytes.as_deref())?,
        None => false,
    };

    let installed_at = first_available_timestamp(&[
        plugin_dir,
        &manifest_path,
        &service_path,
        &binary_path,
        &hash_path,
        &package_path,
    ])
    .unwrap_or_else(now_secs);
    let updated_at = latest_available_timestamp(&[
        plugin_dir,
        &manifest_path,
        &service_path,
        &binary_path,
        &hash_path,
        &package_path,
    ])
    .unwrap_or(installed_at);

    Ok(PluginRecord {
        id: binary_name.clone(),
        name: manifest.name,
        description: manifest.description,
        version: manifest.version,
        service: service_name.clone(),
        developer: manifest.developer,
        channel: normalize_channel(manifest.channel.as_deref())?,
        github_url: normalize_github_url(manifest.github_url.as_deref()),
        webview: normalize_webview_port(manifest.webview)?,
        tls: manifest.tls,
        icon: normalize_icon_path(manifest.icon.as_deref())?,
        binary_name,
        bin_path: normalize_bin_path(manifest.symlink.as_deref())?,
        sha256,
        install_dir: plugin_dir.to_string_lossy().into_owned(),
        package_path: package_path.to_string_lossy().into_owned(),
        official,
        enabled: read_service_enabled(&service_name),
        removable: true,
        target_name: None,
        installed_at,
        updated_at,
    })
}

fn validate_manifest(manifest: &PluginManifest) -> PluginResult<()> {
    if manifest.name.trim().is_empty() {
        return Err(PluginError::bad_request("plugin name must not be empty"));
    }
    if manifest.description.trim().is_empty() {
        return Err(PluginError::bad_request(
            "plugin description must not be empty",
        ));
    }
    if manifest.version.trim().is_empty() {
        return Err(PluginError::bad_request("plugin version must not be empty"));
    }
    if manifest.developer.trim().is_empty() {
        return Err(PluginError::bad_request(
            "plugin developer must not be empty",
        ));
    }
    let _ = normalize_channel(manifest.channel.as_deref())?;
    let _ = normalize_github_url(manifest.github_url.as_deref());
    let _ = normalize_webview_port(manifest.webview)?;
    let _ = normalize_icon_path(manifest.icon.as_deref())?;
    normalize_bin_path(manifest.symlink.as_deref())?;
    derive_binary_name(manifest.service.trim()).map(|_| ())
}

fn derive_binary_name(service_name: &str) -> PluginResult<String> {
    if !service_name.ends_with(".service") {
        return Err(PluginError::bad_request(
            "plugin service must end with .service",
        ));
    }
    let path = Path::new(service_name);
    if path.components().count() != 1 {
        return Err(PluginError::bad_request(
            "plugin service file must be a plain filename",
        ));
    }
    let stem = path
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or_else(|| PluginError::bad_request("plugin service name is invalid"))?;
    validate_file_name(stem)?;
    Ok(stem.to_string())
}

fn validate_file_name(value: &str) -> PluginResult<()> {
    if value.is_empty() {
        return Err(PluginError::bad_request(
            "plugin filename must not be empty",
        ));
    }
    if value.contains('/') || value.contains('\\') {
        return Err(PluginError::bad_request(
            "plugin filename must not contain path separators",
        ));
    }
    if value.starts_with('.') {
        return Err(PluginError::bad_request(
            "plugin filename must not start with a dot",
        ));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(PluginError::bad_request(
            "plugin filename must use only ASCII letters, numbers, ., _ or -",
        ));
    }
    Ok(())
}

fn read_zip_entry_text<R: io::Read + io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> PluginResult<String> {
    let bytes = read_zip_entry_bytes(archive, name)?;
    String::from_utf8(bytes)
        .map_err(|err| PluginError::bad_request(format!("plugin file {name} is not utf-8: {err}")))
}

fn read_zip_entry_bytes<R: io::Read + io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> PluginResult<Vec<u8>> {
    let mut file = archive
        .by_name(name)
        .map_err(|_| PluginError::bad_request(format!("missing required plugin file {name}")))?;
    validate_zip_name(file.name())?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|err| PluginError::bad_request(format!("read plugin file {name}: {err}")))?;
    Ok(buf)
}

fn read_optional_zip_entry_bytes<R: io::Read + io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> PluginResult<Option<Vec<u8>>> {
    match archive.by_name(name) {
        Ok(mut file) => {
            validate_zip_name(file.name())?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).map_err(|err| {
                PluginError::bad_request(format!("read optional plugin file {name}: {err}"))
            })?;
            Ok(Some(buf))
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(err) => Err(PluginError::bad_request(format!(
            "read optional plugin file {name}: {err}"
        ))),
    }
}

fn read_optional_signature_bytes<R: io::Read + io::Seek>(
    archive: &mut ZipArchive<R>,
    binary_name: &str,
) -> PluginResult<Option<Vec<u8>>> {
    if let Some(bytes) = read_optional_zip_entry_bytes(archive, &format!("{binary_name}.sig"))? {
        return Ok(Some(bytes));
    }
    read_optional_zip_entry_bytes(archive, &format!("{binary_name}.sign"))
}

fn resolve_signature_file_path(plugin_dir: &Path, binary_name: &str) -> Option<PathBuf> {
    let sig_path = plugin_dir.join(format!("{binary_name}.sig"));
    if sig_path.is_file() {
        return Some(sig_path);
    }
    let legacy_path = plugin_dir.join(format!("{binary_name}.sign"));
    if legacy_path.is_file() {
        return Some(legacy_path);
    }
    None
}

fn read_custom_files<R: io::Read + io::Seek>(
    archive: &mut ZipArchive<R>,
    binary_name: &str,
) -> PluginResult<Vec<PluginPackageFile>> {
    let mut files = Vec::new();
    let mut seen_paths = HashSet::new();

    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|err| PluginError::bad_request(format!("read plugin archive entry: {err}")))?;
        let entry_name = file.name().to_string();
        validate_zip_name(&entry_name)?;
        if !entry_name.starts_with("files/") {
            continue;
        }
        if file.is_dir() || entry_name == "files/" {
            continue;
        }

        let relative = Path::new(&entry_name).strip_prefix("files").map_err(|_| {
            PluginError::bad_request(format!("plugin package contains invalid path {entry_name}"))
        })?;
        let relative = normalize_custom_file_path(relative)?;
        if relative == Path::new(binary_name) {
            return Err(PluginError::bad_request(format!(
                "custom plugin file {} conflicts with the plugin binary",
                relative.display()
            )));
        }
        let relative_key = relative.to_string_lossy().into_owned();
        if !seen_paths.insert(relative_key.clone()) {
            return Err(PluginError::bad_request(format!(
                "plugin package contains duplicate custom file {}",
                relative.display()
            )));
        }

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|err| {
            PluginError::bad_request(format!(
                "read custom plugin file {}: {err}",
                relative.display()
            ))
        })?;
        files.push(PluginPackageFile {
            relative_path: PathBuf::from(relative_key),
            bytes,
        });
    }

    Ok(files)
}

fn validate_zip_name(name: &str) -> PluginResult<()> {
    let path = Path::new(name);
    for component in path.components() {
        if matches!(
            component,
            Component::CurDir | Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(PluginError::bad_request(format!(
                "plugin package contains invalid path {name}"
            )));
        }
    }
    Ok(())
}

fn normalize_custom_file_path(path: &Path) -> PluginResult<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let segment = value.to_str().ok_or_else(|| {
                    PluginError::bad_request("plugin custom file path must be valid utf-8")
                })?;
                if segment.is_empty() {
                    return Err(PluginError::bad_request(
                        "plugin custom file path must not contain empty segments",
                    ));
                }
                normalized.push(value);
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(PluginError::bad_request(format!(
                    "plugin custom file path {} is invalid",
                    path.display()
                )))
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(PluginError::bad_request(
            "plugin custom file path must not be empty",
        ));
    }

    Ok(normalized)
}

fn normalize_bin_path(bin_path: Option<&str>) -> PluginResult<Option<String>> {
    let Some(bin_path) = bin_path.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let path = Path::new(bin_path);
    if !path.is_absolute() {
        return Err(PluginError::bad_request(
            "plugin bin_path must be an absolute path",
        ));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(PluginError::bad_request(
            "plugin bin_path must not contain parent-directory segments",
        ));
    }
    Ok(Some(path.to_string_lossy().into_owned()))
}

fn normalize_github_url(github_url: Option<&str>) -> Option<String> {
    github_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn normalize_webview_port(webview: Option<u16>) -> PluginResult<Option<u16>> {
    match webview {
        Some(0) => Err(PluginError::bad_request(
            "plugin webview port must be between 1 and 65535",
        )),
        Some(port) => Ok(Some(port)),
        None => Ok(None),
    }
}

/// Manifest icon path, resolved against the plugin runtime directory
/// (`<install_dir>/current`) — the same place `files/` contents are installed.
/// Icons are expected to be 128x128 PNG or SVG.
fn normalize_icon_path(icon: Option<&str>) -> PluginResult<Option<String>> {
    let Some(icon) = icon.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let normalized = normalize_custom_file_path(Path::new(icon))
        .map_err(|_| PluginError::bad_request(format!("plugin icon path {icon} is invalid")))?;
    match normalized
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") | Some("svg") => {}
        _ => {
            return Err(PluginError::bad_request(format!(
                "plugin icon {icon} must be a .png or .svg file"
            )))
        }
    }

    Ok(Some(
        normalized
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    ))
}

fn icon_content_type(relative_path: &str) -> &'static str {
    match Path::new(relative_path)
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("svg") => "image/svg+xml",
        _ => "image/png",
    }
}

fn normalize_channel(channel: Option<&str>) -> PluginResult<Option<String>> {
    let Some(channel) = channel.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let normalized = channel.to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "dev" | "experimental" | "unstable" | "stable"
    ) {
        Ok(Some(normalized))
    } else {
        Err(PluginError::bad_request(
            "plugin channel must be one of: dev, experimental, unstable, stable",
        ))
    }
}

fn collect_plugin_paths(root: &Path, current: &Path, out: &mut Vec<String>) -> PluginResult<()> {
    let entries = fs::read_dir(current)
        .map_err(|err| PluginError::internal(format!("scan plugin inventory path: {err}")))?;

    for entry in entries {
        let entry = entry
            .map_err(|err| PluginError::internal(format!("read plugin inventory entry: {err}")))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path.as_path())
            .display()
            .to_string();
        out.push(relative);

        if entry
            .file_type()
            .map_err(|err| PluginError::internal(format!("inspect plugin inventory entry: {err}")))?
            .is_dir()
        {
            collect_plugin_paths(root, &path, out)?;
        }
    }

    Ok(())
}

fn plugin_binary_target(install_dir: &Path, binary_name: &str) -> PathBuf {
    install_dir.join("current").join(binary_name)
}

fn current_binary_path(record: &PluginRecord) -> PathBuf {
    let install_dir = Path::new(&record.install_dir);
    if record.target_name.is_some() && !record.removable {
        record
            .bin_path
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| install_dir.join(&record.binary_name))
    } else {
        plugin_binary_target(install_dir, &record.binary_name)
    }
}

fn normalize_service_working_directory(
    service_bytes: &[u8],
    current_dir: &Path,
) -> PluginResult<Vec<u8>> {
    let service_text = std::str::from_utf8(service_bytes).map_err(|err| {
        PluginError::bad_request(format!("plugin service file is not utf-8: {err}"))
    })?;
    let working_directory = format!("WorkingDirectory={}", current_dir.display());
    let mut output = Vec::new();
    let mut saw_service_section = false;
    let mut in_service_section = false;
    let mut replaced_working_directory = false;

    for line in service_text.lines() {
        let trimmed = line.trim();
        let is_section = trimmed.starts_with('[') && trimmed.ends_with(']');
        if is_section {
            if in_service_section && !replaced_working_directory {
                output.push(working_directory.clone());
                replaced_working_directory = true;
            }
            in_service_section = trimmed.eq_ignore_ascii_case("[Service]");
            if in_service_section {
                saw_service_section = true;
            }
            output.push(line.to_string());
            continue;
        }

        if in_service_section && trimmed.starts_with("WorkingDirectory=") {
            if !replaced_working_directory {
                output.push(working_directory.clone());
                replaced_working_directory = true;
            }
            continue;
        }

        output.push(line.to_string());
    }

    if !saw_service_section {
        return Err(PluginError::bad_request(
            "plugin service file must contain a [Service] section",
        ));
    }
    if in_service_section && !replaced_working_directory {
        output.push(working_directory);
    }

    let mut normalized = output.join("\n");
    if normalized.is_empty() || !normalized.ends_with('\n') {
        normalized.push('\n');
    }
    Ok(normalized.into_bytes())
}

fn install_custom_files(current_dir: &Path, files: &[PluginPackageFile]) -> PluginResult<()> {
    for file in files {
        let target_path = current_dir.join(&file.relative_path);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                PluginError::internal(format!("create custom file parent: {err}"))
            })?;
        }
        fs::write(&target_path, &file.bytes)
            .map_err(|err| PluginError::internal(format!("write custom plugin file: {err}")))?;
    }
    Ok(())
}

fn rollback_symlink_info_path(rollback_dir: &Path) -> PathBuf {
    rollback_dir.join("bin_path.rollback")
}

fn prepare_bin_path_rollback(
    rollback_dir: &Path,
    bin_path: Option<&str>,
    target: Option<PathBuf>,
) -> PluginResult<()> {
    let info_path = rollback_symlink_info_path(rollback_dir);
    let _ = fs::remove_file(&info_path);
    let Some(bin_path) = bin_path else {
        return Ok(());
    };
    let Some(target) = target else {
        return Ok(());
    };
    let link_path = Path::new(bin_path);
    if !link_path.exists() && fs::symlink_metadata(link_path).is_err() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(link_path)
        .map_err(|err| PluginError::internal(format!("inspect plugin symlink: {err}")))?;
    if !metadata.file_type().is_symlink() {
        return Err(PluginError::bad_request(format!(
            "plugin bin_path {} conflicts with an existing non-symlink entry",
            link_path.display()
        )));
    }
    let current_target = fs::read_link(link_path)
        .map_err(|err| PluginError::internal(format!("read plugin symlink: {err}")))?;
    if current_target != target {
        return Err(PluginError::bad_request(format!(
            "plugin bin_path {} is already used by another target",
            link_path.display()
        )));
    }
    fs::write(
        &info_path,
        format!("{}\n{}\n", link_path.display(), current_target.display()),
    )
    .map_err(|err| PluginError::internal(format!("persist symlink rollback metadata: {err}")))?;
    Ok(())
}

fn install_bin_path(
    new_bin_path: Option<&str>,
    previous_bin_path: Option<&str>,
    target: &Path,
    rollback_dir: &Path,
) -> PluginResult<()> {
    if let Some(previous) = previous_bin_path {
        remove_managed_symlink(Path::new(previous), target)?;
    }

    let Some(new_bin_path) = new_bin_path else {
        return Ok(());
    };
    let link_path = Path::new(new_bin_path);
    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| PluginError::internal(format!("create bin_path parent: {err}")))?;
    }
    match fs::symlink_metadata(link_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                let current_target = fs::read_link(link_path)
                    .map_err(|err| PluginError::internal(format!("read plugin symlink: {err}")))?;
                if current_target != target {
                    return Err(PluginError::bad_request(format!(
                        "plugin bin_path {} conflicts with an existing symlink",
                        link_path.display()
                    )));
                }
                fs::remove_file(link_path).map_err(|err| {
                    PluginError::internal(format!("replace plugin symlink: {err}"))
                })?;
            } else {
                return Err(PluginError::bad_request(format!(
                    "plugin bin_path {} conflicts with an existing file",
                    link_path.display()
                )));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(PluginError::internal(format!(
                "inspect plugin bin_path {}: {err}",
                link_path.display()
            )))
        }
    }

    std::os::unix::fs::symlink(target, link_path)
        .map_err(|err| PluginError::internal(format!("create plugin symlink: {err}")))?;
    let _ = rollback_dir;
    Ok(())
}

fn remove_managed_symlink(link_path: &Path, expected_target: &Path) -> PluginResult<()> {
    match fs::symlink_metadata(link_path) {
        Ok(metadata) => {
            if !metadata.file_type().is_symlink() {
                return Err(PluginError::bad_request(format!(
                    "plugin bin_path {} conflicts with an existing non-symlink entry",
                    link_path.display()
                )));
            }
            let current_target = fs::read_link(link_path)
                .map_err(|err| PluginError::internal(format!("read plugin symlink: {err}")))?;
            if current_target != expected_target {
                return Err(PluginError::bad_request(format!(
                    "plugin bin_path {} is owned by another target",
                    link_path.display()
                )));
            }
            fs::remove_file(link_path)
                .map_err(|err| PluginError::internal(format!("remove plugin symlink: {err}")))?;
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(PluginError::internal(format!(
            "inspect plugin symlink {}: {err}",
            link_path.display()
        ))),
    }
}

fn restore_bin_path(rollback_dir: &Path) -> PluginResult<()> {
    let info_path = rollback_symlink_info_path(rollback_dir);
    if !info_path.exists() {
        return Ok(());
    }
    let data = fs::read_to_string(&info_path)
        .map_err(|err| PluginError::internal(format!("read symlink rollback metadata: {err}")))?;
    let mut lines = data.lines();
    let Some(link_path) = lines.next() else {
        return Ok(());
    };
    let Some(target_path) = lines.next() else {
        return Ok(());
    };
    let link_path = PathBuf::from(link_path);
    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| PluginError::internal(format!("create symlink parent: {err}")))?;
    }
    if fs::symlink_metadata(&link_path).is_ok() {
        let _ = fs::remove_file(&link_path);
    }
    std::os::unix::fs::symlink(PathBuf::from(target_path), &link_path)
        .map_err(|err| PluginError::internal(format!("restore plugin symlink: {err}")))?;
    cleanup_bin_path_rollback(rollback_dir);
    Ok(())
}

fn cleanup_bin_path_rollback(rollback_dir: &Path) {
    let _ = fs::remove_file(rollback_symlink_info_path(rollback_dir));
}

fn evaluate_official(
    cert_path: &Path,
    binary_bytes: &[u8],
    signature_bytes: Option<&[u8]>,
) -> PluginResult<bool> {
    let Some(signature) = signature_bytes else {
        log::debug!("plugin signature missing; marking package as community");
        return Ok(false);
    };
    if !cert_path.exists() {
        log::debug!(
            "official plugin cert missing at {}; treating signature as community",
            cert_path.display()
        );
        return Ok(false);
    }

    let tmp = tempfile::tempdir()
        .map_err(|err| PluginError::internal(format!("create signature tempdir: {err}")))?;
    let bin_path = tmp.path().join("plugin.bin");
    let sig_path = tmp.path().join("plugin.sig");
    fs::write(&bin_path, binary_bytes)
        .map_err(|err| PluginError::internal(format!("write temp plugin binary: {err}")))?;
    fs::write(&sig_path, signature)
        .map_err(|err| PluginError::internal(format!("write temp plugin signature: {err}")))?;
    log::debug!(
        "verifying plugin signature using cert={}",
        cert_path.display()
    );
    verify_signature(&bin_path, &sig_path, cert_path)?;
    Ok(true)
}

fn prepare_rollback(
    plugin_dir: &Path,
    rollback_dir: &Path,
    service_path: &Path,
    service_name: &str,
) -> PluginResult<()> {
    cleanup_rollback(rollback_dir);
    fs::create_dir_all(rollback_dir)
        .map_err(|err| PluginError::internal(format!("create rollback dir: {err}")))?;

    move_if_exists(&plugin_dir.join("current"), &rollback_dir.join("current"))?;
    move_if_exists(
        &plugin_dir.join("package.zip"),
        &rollback_dir.join("package.zip"),
    )?;
    move_if_exists(
        &plugin_dir.join(MANIFEST_NAME),
        &rollback_dir.join(MANIFEST_NAME),
    )?;
    move_if_exists(
        &plugin_dir.join(service_name),
        &rollback_dir.join(service_name),
    )?;
    move_if_exists(
        &plugin_dir.join(format!(
            "{}.sha256",
            Path::new(service_name)
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or(service_name)
        )),
        &rollback_dir.join(format!(
            "{}.sha256",
            Path::new(service_name)
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or(service_name)
        )),
    )?;

    let binary_name = Path::new(service_name)
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or(service_name);
    for signature_name in [format!("{binary_name}.sig"), format!("{binary_name}.sign")] {
        let signature_path = plugin_dir.join(&signature_name);
        move_if_exists(
            &signature_path,
            &rollback_dir.join(signature_path.file_name().unwrap()),
        )?;
    }

    if service_path.exists() {
        fs::copy(service_path, rollback_dir.join(service_name))
            .map_err(|err| PluginError::internal(format!("backup service file: {err}")))?;
    }

    Ok(())
}

fn rollback_install(
    plugin_dir: &Path,
    rollback_dir: &Path,
    service_path: &Path,
    service_name: &str,
    attempted_bin_path: Option<&str>,
    attempted_target: &Path,
    existing: Option<&PluginRecord>,
) -> PluginResult<()> {
    if let Some(bin_path) = attempted_bin_path {
        let _ = remove_managed_symlink(Path::new(bin_path), attempted_target);
    }
    let current_dir = plugin_dir.join("current");
    if current_dir.exists() {
        let _ = fs::remove_dir_all(&current_dir);
    }
    let _ = fs::remove_file(plugin_dir.join("package.zip"));
    let _ = fs::remove_file(plugin_dir.join(MANIFEST_NAME));
    let _ = fs::remove_file(plugin_dir.join(service_name));
    let _ = fs::remove_file(plugin_dir.join(format!(
        "{}.sha256",
        Path::new(service_name)
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or(service_name)
    )));
    let binary_name = Path::new(service_name)
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or(service_name);
    let _ = fs::remove_file(plugin_dir.join(format!("{binary_name}.sig")));
    let _ = fs::remove_file(plugin_dir.join(format!("{binary_name}.sign")));

    restore_if_exists(&rollback_dir.join("current"), &current_dir)?;
    restore_if_exists(
        &rollback_dir.join("package.zip"),
        &plugin_dir.join("package.zip"),
    )?;
    restore_if_exists(
        &rollback_dir.join(MANIFEST_NAME),
        &plugin_dir.join(MANIFEST_NAME),
    )?;
    restore_if_exists(
        &rollback_dir.join(format!("{binary_name}.sig")),
        &plugin_dir.join(format!("{binary_name}.sig")),
    )?;
    restore_if_exists(
        &rollback_dir.join(format!("{binary_name}.sign")),
        &plugin_dir.join(format!("{binary_name}.sign")),
    )?;
    restore_if_exists(
        &rollback_dir.join(service_name),
        &plugin_dir.join(service_name),
    )?;
    restore_if_exists(
        &rollback_dir.join(format!(
            "{}.sha256",
            Path::new(service_name)
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or(service_name)
        )),
        &plugin_dir.join(format!(
            "{}.sha256",
            Path::new(service_name)
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or(service_name)
        )),
    )?;

    let restored_service = rollback_dir.join(service_name);
    if restored_service.exists() {
        fs::copy(&restored_service, service_path)
            .map_err(|err| PluginError::internal(format!("restore service file: {err}")))?;
    } else if service_path.exists() {
        fs::remove_file(service_path)
            .map_err(|err| PluginError::internal(format!("remove failed service file: {err}")))?;
    }
    daemon_reload()?;
    restore_bin_path(rollback_dir)?;

    if let Some(existing) = existing {
        if existing.enabled {
            let _ = systemctl("enable", &existing.service);
        } else {
            let _ = systemctl("disable", &existing.service);
        }
        if is_service_running(&existing.service) || existing.enabled {
            let _ = systemctl("start", &existing.service);
        }
    } else {
        let _ = systemctl("disable", service_name);
        let _ = fs::remove_file(service_path);
        daemon_reload()?;
    }

    cleanup_rollback(rollback_dir);
    Ok(())
}

fn cleanup_rollback(path: &Path) {
    if path.exists() {
        let _ = fs::remove_dir_all(path);
    }
}

fn move_if_exists(src: &Path, dst: &Path) -> PluginResult<()> {
    if !src.exists() {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| PluginError::internal(format!("create rollback parent: {err}")))?;
    }
    fs::rename(src, dst)
        .map_err(|err| PluginError::internal(format!("move plugin state into rollback: {err}")))
}

fn restore_if_exists(src: &Path, dst: &Path) -> PluginResult<()> {
    if !src.exists() {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| PluginError::internal(format!("create restore parent: {err}")))?;
    }
    fs::rename(src, dst)
        .map_err(|err| PluginError::internal(format!("restore plugin state: {err}")))
}

fn open_db(db_path: &Path) -> PluginResult<Connection> {
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| PluginError::internal(format!("create plugin db dir: {err}")))?;
    }
    let conn = Connection::open(db_path)
        .map_err(|err| PluginError::internal(format!("open plugin db: {err}")))?;
    init_db(&conn)?;
    Ok(conn)
}

fn init_db(conn: &Connection) -> PluginResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS plugins (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            description TEXT NOT NULL,
            version TEXT NOT NULL,
            service TEXT NOT NULL UNIQUE,
            developer TEXT NOT NULL,
            channel TEXT,
            github_url TEXT,
            webview INTEGER,
            tls INTEGER NOT NULL DEFAULT 0,
            icon TEXT,
            binary_name TEXT NOT NULL,
            bin_path TEXT,
            sha256 TEXT NOT NULL DEFAULT '',
            install_dir TEXT NOT NULL,
            package_path TEXT NOT NULL,
            official INTEGER NOT NULL DEFAULT 0,
            enabled INTEGER NOT NULL DEFAULT 1,
            removable INTEGER NOT NULL DEFAULT 1,
            target_name TEXT,
            installed_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );",
    )
    .map_err(|err| PluginError::internal(format!("init plugin db: {err}")))?;
    ensure_column_exists(
        conn,
        "plugins",
        "bin_path",
        "ALTER TABLE plugins ADD COLUMN bin_path TEXT",
    )?;
    ensure_column_exists(
        conn,
        "plugins",
        "removable",
        "ALTER TABLE plugins ADD COLUMN removable INTEGER NOT NULL DEFAULT 1",
    )?;
    ensure_column_exists(
        conn,
        "plugins",
        "github_url",
        "ALTER TABLE plugins ADD COLUMN github_url TEXT",
    )?;
    ensure_column_exists(
        conn,
        "plugins",
        "channel",
        "ALTER TABLE plugins ADD COLUMN channel TEXT",
    )?;
    ensure_column_exists(
        conn,
        "plugins",
        "webview",
        "ALTER TABLE plugins ADD COLUMN webview INTEGER",
    )?;
    ensure_column_exists(
        conn,
        "plugins",
        "tls",
        "ALTER TABLE plugins ADD COLUMN tls INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column_exists(
        conn,
        "plugins",
        "icon",
        "ALTER TABLE plugins ADD COLUMN icon TEXT",
    )?;
    ensure_column_exists(
        conn,
        "plugins",
        "target_name",
        "ALTER TABLE plugins ADD COLUMN target_name TEXT",
    )?;
    ensure_column_exists(
        conn,
        "plugins",
        "sha256",
        "ALTER TABLE plugins ADD COLUMN sha256 TEXT NOT NULL DEFAULT ''",
    )?;
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_plugins_service ON plugins(service);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_plugins_bin_path
         ON plugins(bin_path)
         WHERE bin_path IS NOT NULL AND bin_path != '';
         CREATE UNIQUE INDEX IF NOT EXISTS idx_plugins_target_name
         ON plugins(target_name)
         WHERE target_name IS NOT NULL AND target_name != '';",
    )
    .map_err(|err| PluginError::internal(format!("init plugin indexes: {err}")))?;
    Ok(())
}

fn load_plugin(conn: &Connection, plugin_id: &str) -> PluginResult<Option<PluginRecord>> {
    conn.query_row(
        "SELECT id, name, description, version, service, developer, channel, github_url, webview, tls, icon, binary_name, bin_path, sha256, install_dir, package_path, official, enabled, removable, target_name, installed_at, updated_at
         FROM plugins
         WHERE id = ?1",
        params![plugin_id],
        |row| {
            Ok(PluginRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                version: row.get(3)?,
                service: row.get(4)?,
                developer: row.get(5)?,
                channel: row.get(6)?,
                github_url: row.get(7)?,
                webview: row.get(8)?,
                tls: row.get::<_, i64>(9)? != 0,
                icon: row.get(10)?,
                binary_name: row.get(11)?,
                bin_path: row.get(12)?,
                sha256: row.get(13)?,
                install_dir: row.get(14)?,
                package_path: row.get(15)?,
                official: row.get::<_, i64>(16)? != 0,
                enabled: row.get::<_, i64>(17)? != 0,
                removable: row.get::<_, i64>(18)? != 0,
                target_name: row.get(19)?,
                installed_at: row.get(20)?,
                updated_at: row.get(21)?,
            })
        },
    )
    .optional()
    .map_err(|err| PluginError::internal(format!("load plugin {plugin_id}: {err}")))
}

fn load_all_plugins(conn: &Connection) -> PluginResult<Vec<PluginRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, description, version, service, developer, channel, github_url, webview, tls, icon, binary_name, bin_path, sha256, install_dir, package_path, official, enabled, removable, target_name, installed_at, updated_at
             FROM plugins
             ORDER BY name COLLATE NOCASE, id",
        )
        .map_err(|err| PluginError::internal(format!("prepare plugin query: {err}")))?;

    let rows = stmt
        .query_map([], |row| {
            Ok(PluginRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                version: row.get(3)?,
                service: row.get(4)?,
                developer: row.get(5)?,
                channel: row.get(6)?,
                github_url: row.get(7)?,
                webview: row.get(8)?,
                tls: row.get::<_, i64>(9)? != 0,
                icon: row.get(10)?,
                binary_name: row.get(11)?,
                bin_path: row.get(12)?,
                sha256: row.get(13)?,
                install_dir: row.get(14)?,
                package_path: row.get(15)?,
                official: row.get::<_, i64>(16)? != 0,
                enabled: row.get::<_, i64>(17)? != 0,
                removable: row.get::<_, i64>(18)? != 0,
                target_name: row.get(19)?,
                installed_at: row.get(20)?,
                updated_at: row.get(21)?,
            })
        })
        .map_err(|err| PluginError::internal(format!("query plugins: {err}")))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|err| PluginError::internal(format!("read plugins: {err}")))
}

fn build_plugin_summaries(
    records: Vec<PluginRecord>,
    systemd_status_by_service: &HashMap<String, PluginSystemdStatus>,
) -> PluginResult<Vec<PluginSummary>> {
    let mut summaries = Vec::with_capacity(records.len());
    for record in records {
        let systemd_status = systemd_status_by_service
            .get(&record.service)
            .cloned()
            .unwrap_or_else(unknown_systemd_status);
        summaries.push(PluginSummary {
            status: summarize_service_status(&systemd_status),
            id: record.id,
            name: record.name,
            description: record.description,
            version: record.version,
            service: record.service,
            developer: record.developer,
            channel: record.channel,
            github_url: record.github_url,
            webview: record.webview,
            tls: record.tls,
            icon: record.icon,
            binary_name: record.binary_name,
            bin_path: record.bin_path,
            sha256: record.sha256,
            install_dir: record.install_dir,
            package_path: record.package_path,
            official: record.official,
            enabled: record.enabled,
            removable: record.removable,
            target_name: record.target_name,
            systemd_status,
            installed_at: record.installed_at,
            updated_at: record.updated_at,
        });
    }
    Ok(summaries)
}

fn reconcile_plugin_bin_paths(conn: &Connection) -> PluginResult<()> {
    for record in load_all_plugins(conn)? {
        if record.target_name.is_some() || !record.removable {
            continue;
        }
        let Some(_) = record.bin_path.as_deref() else {
            continue;
        };
        let target = current_binary_path(&record);
        if !target.is_file() {
            continue;
        }
        install_bin_path(record.bin_path.as_deref(), None, &target, Path::new(""))?;
    }
    Ok(())
}

fn ensure_service_available(
    conn: &Connection,
    service_name: &str,
    current_plugin_id: Option<&str>,
) -> PluginResult<()> {
    let owner = conn
        .query_row(
            "SELECT id FROM plugins WHERE service = ?1",
            params![service_name],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|err| PluginError::internal(format!("query plugin service owner: {err}")))?;

    if let Some(owner) = owner {
        if current_plugin_id != Some(owner.as_str()) {
            return Err(PluginError::bad_request(format!(
                "service {service_name} is already owned by plugin {owner}"
            )));
        }
    }
    Ok(())
}

fn ensure_bin_path_available(
    conn: &Connection,
    bin_path: Option<&str>,
    current_plugin_id: Option<&str>,
) -> PluginResult<()> {
    let Some(bin_path) = bin_path else {
        return Ok(());
    };
    let owner = conn
        .query_row(
            "SELECT id FROM plugins WHERE bin_path = ?1",
            params![bin_path],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|err| PluginError::internal(format!("query plugin bin_path owner: {err}")))?;

    if let Some(owner) = owner {
        if current_plugin_id != Some(owner.as_str()) {
            return Err(PluginError::bad_request(format!(
                "bin_path {bin_path} is already owned by plugin {owner}"
            )));
        }
    }
    Ok(())
}

fn sync_core_plugins(conn: &Connection, core_plugins: &[CorePluginSpec]) -> PluginResult<()> {
    for spec in core_plugins {
        let manifest_path = spec.plugin_dir.join(MANIFEST_NAME);
        if !manifest_path.is_file() {
            log::info!(
                "skipping built-in plugin id={} target={} missing manifest at {}",
                spec.plugin_id,
                spec.target.name,
                manifest_path.display()
            );
            continue;
        }
        let manifest_raw = fs::read_to_string(&manifest_path).map_err(|err| {
            PluginError::internal(format!("read built-in plugin manifest: {err}"))
        })?;
        let manifest: PluginManifest = toml::from_str::<PluginManifestFile>(&manifest_raw)
            .map(|file| file.plugin)
            .map_err(|err| {
                PluginError::bad_request(format!("bad built-in plugin manifest: {err}"))
            })?;
        validate_manifest(&manifest)?;

        let service_name = manifest.service.trim().to_string();
        let binary_name = spec
            .target
            .symlink_path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(spec.target.name)
            .to_string();
        let manifest_binary_name = derive_binary_name(&service_name)?;
        if service_name != spec.target.service {
            return Err(PluginError::bad_request(format!(
                "built-in plugin manifest service {} does not match target service {}",
                service_name, spec.target.service
            )));
        }
        if manifest_binary_name != binary_name {
            return Err(PluginError::bad_request(format!(
                "built-in plugin manifest binary {} does not match target binary {}",
                manifest_binary_name, binary_name
            )));
        }
        let manifest_bin_path = normalize_bin_path(manifest.symlink.as_deref())?;
        if manifest_bin_path.as_deref() != Some(spec.target.symlink_path.to_string_lossy().as_ref())
        {
            return Err(PluginError::bad_request(format!(
                "built-in plugin manifest bin_path {:?} does not match target path {}",
                manifest_bin_path,
                spec.target.symlink_path.display()
            )));
        }

        let version_path = spec.target.meta_dir.join(format!("{binary_name}.version"));
        let hash_path = spec.target.meta_dir.join(format!("{binary_name}.sha256"));
        let version = read_trimmed(&version_path).unwrap_or_else(|| manifest.version.clone());
        let sha256 = read_trimmed(&hash_path)
            .unwrap_or_else(|| sha256_file(&spec.target.binary_path).unwrap_or_default());
        let current_binary_path = plugin_binary_target(&spec.plugin_dir, &binary_name);
        let current_dir = spec.plugin_dir.join("current");
        if manifest.tls && current_dir.is_dir() {
            local_https::ensure_plugin_tls_files(&current_dir, &spec.plugin_id).map_err(|err| {
                PluginError::internal(format!("generate built-in plugin TLS files: {err}"))
            })?;
        }
        let installed_at = first_available_timestamp(&[
            spec.plugin_dir.as_path(),
            manifest_path.as_path(),
            current_binary_path.as_path(),
            spec.target.binary_path.as_path(),
            spec.target.symlink_path.as_path(),
            version_path.as_path(),
            hash_path.as_path(),
        ])
        .unwrap_or_else(now_secs);
        let updated_at = latest_available_timestamp(&[
            spec.plugin_dir.as_path(),
            manifest_path.as_path(),
            current_binary_path.as_path(),
            spec.target.binary_path.as_path(),
            spec.target.symlink_path.as_path(),
            version_path.as_path(),
            hash_path.as_path(),
        ])
        .unwrap_or(installed_at);
        log::debug!(
            "syncing built-in plugin id={} target={} service={}",
            spec.plugin_id,
            spec.target.name,
            spec.target.service
        );

        upsert_plugin(
            conn,
            PluginRecord {
                id: spec.plugin_id.clone(),
                name: manifest.name,
                description: manifest.description,
                version,
                service: service_name,
                developer: manifest.developer,
                channel: normalize_channel(manifest.channel.as_deref())?,
                github_url: normalize_github_url(manifest.github_url.as_deref()),
                webview: normalize_webview_port(manifest.webview)?,
                tls: manifest.tls,
                icon: normalize_icon_path(manifest.icon.as_deref())?,
                binary_name,
                bin_path: manifest_bin_path,
                sha256,
                install_dir: spec.plugin_dir.to_string_lossy().into_owned(),
                package_path: spec
                    .plugin_dir
                    .join("package.zip")
                    .to_string_lossy()
                    .into_owned(),
                official: true,
                enabled: read_service_enabled(spec.target.service),
                removable: false,
                target_name: Some(spec.target.name.to_string()),
                installed_at,
                updated_at,
            },
        )?;
    }
    Ok(())
}

fn upsert_plugin(conn: &Connection, record: PluginRecord) -> PluginResult<()> {
    conn.execute(
        "INSERT INTO plugins (
            id, name, description, version, service, developer, channel, github_url, webview, tls, icon, binary_name, bin_path, sha256, install_dir, package_path, official, enabled, removable, target_name, installed_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
        ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            description = excluded.description,
            version = excluded.version,
            service = excluded.service,
            developer = excluded.developer,
            channel = excluded.channel,
            github_url = excluded.github_url,
            webview = excluded.webview,
            tls = excluded.tls,
            icon = excluded.icon,
            binary_name = excluded.binary_name,
            bin_path = excluded.bin_path,
            sha256 = excluded.sha256,
            install_dir = excluded.install_dir,
            package_path = excluded.package_path,
            official = excluded.official,
            enabled = excluded.enabled,
            removable = excluded.removable,
            target_name = excluded.target_name,
            installed_at = excluded.installed_at,
            updated_at = excluded.updated_at",
        params![
            record.id,
            record.name,
            record.description,
            record.version,
            record.service,
            record.developer,
            record.channel,
            record.github_url,
            record.webview,
            if record.tls { 1 } else { 0 },
            record.icon,
            record.binary_name,
            record.bin_path,
            record.sha256,
            record.install_dir,
            record.package_path,
            if record.official { 1 } else { 0 },
            if record.enabled { 1 } else { 0 },
            if record.removable { 1 } else { 0 },
            record.target_name,
            record.installed_at,
            record.updated_at,
        ],
    )
    .map_err(|err| PluginError::internal(format!("upsert plugin record: {err}")))?;
    Ok(())
}

fn ensure_column_exists(
    conn: &Connection,
    table: &str,
    column: &str,
    alter_sql: &str,
) -> PluginResult<()> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn
        .prepare(&pragma)
        .map_err(|err| PluginError::internal(format!("prepare table info: {err}")))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|err| PluginError::internal(format!("query table info: {err}")))?
        .filter_map(Result::ok)
        .any(|name| name == column);
    if !exists {
        conn.execute(alter_sql, [])
            .map_err(|err| PluginError::internal(format!("migrate plugin db: {err}")))?;
    }
    Ok(())
}

fn update_plugin_enabled(conn: &Connection, plugin_id: &str, enabled: bool) -> PluginResult<()> {
    conn.execute(
        "UPDATE plugins SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
        params![plugin_id, if enabled { 1 } else { 0 }, now_secs()],
    )
    .map_err(|err| PluginError::internal(format!("update plugin state: {err}")))?;
    Ok(())
}

fn daemon_reload() -> PluginResult<()> {
    #[cfg(test)]
    {
        return Ok(());
    }

    #[cfg(not(test))]
    {
        let status = Command::new("systemctl")
            .arg("daemon-reload")
            .status()
            .map_err(|err| PluginError::internal(format!("systemctl daemon-reload: {err}")))?;
        if status.success() {
            log::debug!("systemctl daemon-reload succeeded");
        } else {
            log::warn!("systemctl daemon-reload exited with status {}", status);
        }
        Ok(())
    }
}

fn systemctl(action: &str, service: &str) -> io::Result<()> {
    #[cfg(test)]
    {
        let _ = (action, service);
        return Ok(());
    }

    #[cfg(not(test))]
    {
        let status = Command::new("systemctl").args([action, service]).status()?;
        if status.success() {
            log::debug!("systemctl {} {} succeeded", action, service);
        } else {
            log::warn!(
                "systemctl {} {} exited with status {}",
                action,
                service,
                status
            );
        }
        Ok(())
    }
}

fn is_service_running(service: &str) -> bool {
    #[cfg(test)]
    {
        let _ = service;
        return false;
    }

    #[cfg(not(test))]
    {
        Command::new("systemctl")
            .args(["is-active", "--quiet", service])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

fn poll_running(service: &str, tries: u32, interval: Duration) -> bool {
    for _ in 0..tries {
        std::thread::sleep(interval);
        if is_service_running(service) {
            return true;
        }
    }
    false
}

fn summarize_service_status(status: &PluginSystemdStatus) -> String {
    if status.active_state == "unknown" && status.sub_state == "unknown" {
        return "unknown".into();
    }
    if status.sub_state.is_empty()
        || status.sub_state == "unknown"
        || status.sub_state == status.active_state
    {
        status.active_state.clone()
    } else {
        format!("{}/{}", status.active_state, status.sub_state)
    }
}

fn unknown_systemd_status() -> PluginSystemdStatus {
    PluginSystemdStatus {
        active_state: "unknown".into(),
        sub_state: "unknown".into(),
        unit_file_state: "unknown".into(),
        main_pid: None,
        tasks_current: None,
        memory_current: None,
    }
}

fn read_service_systemd_status(service: &str) -> PluginSystemdStatus {
    #[cfg(target_os = "linux")]
    {
        let mut snapshot = unknown_systemd_status();
        match Command::new("systemctl")
            .args([
                "show",
                service,
                "--property=ActiveState",
                "--property=SubState",
                "--property=UnitFileState",
                "--property=MainPID",
                "--property=TasksCurrent",
                "--property=MemoryCurrent",
            ])
            .output()
        {
            Ok(output) if output.status.success() => {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    if let Some(value) = line.strip_prefix("ActiveState=") {
                        if !value.trim().is_empty() {
                            snapshot.active_state = value.trim().to_string();
                        }
                    } else if let Some(value) = line.strip_prefix("SubState=") {
                        if !value.trim().is_empty() {
                            snapshot.sub_state = value.trim().to_string();
                        }
                    } else if let Some(value) = line.strip_prefix("UnitFileState=") {
                        if !value.trim().is_empty() {
                            snapshot.unit_file_state = value.trim().to_string();
                        }
                    } else if let Some(value) = line.strip_prefix("MainPID=") {
                        snapshot.main_pid = parse_systemd_number(value).and_then(|value| {
                            if value == 0 {
                                None
                            } else {
                                u32::try_from(value).ok()
                            }
                        });
                    } else if let Some(value) = line.strip_prefix("TasksCurrent=") {
                        snapshot.tasks_current = parse_systemd_number(value);
                    } else if let Some(value) = line.strip_prefix("MemoryCurrent=") {
                        snapshot.memory_current = parse_systemd_number(value);
                    }
                }
                snapshot
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let error_text = if stderr.is_empty() {
                    "error".to_string()
                } else {
                    stderr
                };
                snapshot.active_state = error_text.clone();
                snapshot.sub_state = error_text;
                snapshot
            }
            Err(err) => {
                snapshot.active_state = format!("systemctl unavailable: {err}");
                snapshot.sub_state = "unknown".into();
                snapshot
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let mut snapshot = unknown_systemd_status();
        snapshot.active_state = format!("Unavailable on this host ({service})");
        snapshot
    }
}

fn parse_systemd_number(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "[not set]" {
        None
    } else {
        trimmed.parse::<u64>().ok()
    }
}

fn read_service_enabled(service: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        Command::new("systemctl")
            .args(["is-enabled", "--quiet", service])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = service;
        false
    }
}

fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o111);
    fs::set_permissions(path, perms)
}

fn read_sha256_file(path: &Path) -> PluginResult<String> {
    let sha256 = fs::read_to_string(path)
        .map_err(|err| PluginError::internal(format!("read plugin checksum: {err}")))?;
    let sha256 = sha256.trim().to_string();
    validate_sha256_text(&sha256)?;
    Ok(sha256)
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let data = fs::read(path)?;
    Ok(hex::encode(Sha256::digest(data)))
}

fn validate_sha256_text(value: &str) -> PluginResult<()> {
    if value.len() != 64 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(PluginError::bad_request(
            "plugin sha256 file must contain a 64-character hex digest",
        ));
    }
    Ok(())
}

fn verify_sha256(bytes: &[u8], expected: &str, context: &str) -> PluginResult<()> {
    let normalized = expected.to_ascii_lowercase();
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != normalized {
        return Err(PluginError::bad_request(format!(
            "{context} SHA-256 mismatch: expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

fn verify_signature(bin: &Path, sig: &Path, cert: &Path) -> PluginResult<()> {
    let status = Command::new("openssl")
        .args(["dgst", "-sha256", "-verify"])
        .arg(cert)
        .arg("-signature")
        .arg(sig)
        .arg(bin)
        .status()
        .map_err(|err| PluginError::internal(format!("openssl exec error: {err}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(PluginError::bad_request(
            "plugin signature did not match the official public key",
        ))
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default()
}

fn latest_available_timestamp(paths: &[&Path]) -> Option<u64> {
    paths.iter().filter_map(|path| file_timestamp(path)).max()
}

fn first_available_timestamp(paths: &[&Path]) -> Option<u64> {
    paths.iter().filter_map(|path| file_timestamp(path)).min()
}

fn file_timestamp(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use zip::{write::SimpleFileOptions, ZipWriter};

    #[test]
    fn derives_binary_name_from_service() {
        assert_eq!(
            derive_binary_name("kaonic-plugin-sample.service").unwrap(),
            "kaonic-plugin-sample"
        );
    }

    #[test]
    fn rejects_nested_service_path() {
        let err = derive_binary_name("plugins/sample.service").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_relative_bin_path() {
        let err = normalize_bin_path(Some("usr/bin/sample")).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parses_plugin_package_without_icon() {
        // The manifest key is optional: a package that never mentions an icon
        // installs exactly as before.
        let zip_bytes = build_test_plugin_zip();
        let package = parse_plugin_package(&zip_bytes).unwrap();
        assert!(package.manifest.icon.is_none());
        assert!(normalize_icon_path(package.manifest.icon.as_deref())
            .unwrap()
            .is_none());
    }

    #[test]
    fn stores_and_serves_plugin_without_icon() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("plugins.db");
        let conn = open_db(&db_path).unwrap();
        upsert_plugin(
            &conn,
            PluginRecord {
                id: "kaonic-plugin-sample".into(),
                name: "Sample".into(),
                description: "Sample plugin".into(),
                version: "0.1.0".into(),
                service: "kaonic-plugin-sample.service".into(),
                developer: "Beechat".into(),
                channel: Some("stable".into()),
                github_url: None,
                webview: None,
                tls: false,
                icon: None,
                binary_name: "kaonic-plugin-sample".into(),
                bin_path: None,
                sha256: "ab".repeat(32),
                install_dir: tmp.path().to_string_lossy().into_owned(),
                package_path: tmp
                    .path()
                    .join("package.zip")
                    .to_string_lossy()
                    .into_owned(),
                official: false,
                enabled: true,
                removable: true,
                target_name: None,
                installed_at: 1,
                updated_at: 1,
            },
        )
        .unwrap();

        let record = load_plugin(&conn, "kaonic-plugin-sample").unwrap().unwrap();
        assert!(record.icon.is_none());

        // The icon endpoint 404s for such a plugin rather than failing the list.
        let err = read_plugin_icon(&db_path, "kaonic-plugin-sample").unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn adds_icon_column_to_legacy_database() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("plugins.db");
        {
            // A database created before icons existed.
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE plugins (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    description TEXT NOT NULL,
                    version TEXT NOT NULL,
                    service TEXT NOT NULL UNIQUE,
                    developer TEXT NOT NULL,
                    channel TEXT,
                    github_url TEXT,
                    webview INTEGER,
                    tls INTEGER NOT NULL DEFAULT 0,
                    binary_name TEXT NOT NULL,
                    bin_path TEXT,
                    sha256 TEXT NOT NULL DEFAULT '',
                    install_dir TEXT NOT NULL,
                    package_path TEXT NOT NULL,
                    official INTEGER NOT NULL DEFAULT 0,
                    enabled INTEGER NOT NULL DEFAULT 1,
                    removable INTEGER NOT NULL DEFAULT 1,
                    target_name TEXT,
                    installed_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                INSERT INTO plugins (
                    id, name, description, version, service, developer, binary_name,
                    install_dir, package_path, installed_at, updated_at
                ) VALUES (
                    'kaonic-plugin-sample', 'Sample', 'Sample plugin', '0.1.0',
                    'kaonic-plugin-sample.service', 'Beechat', 'kaonic-plugin-sample',
                    '/etc/kaonic/plugins/kaonic-plugin-sample',
                    '/etc/kaonic/plugins/kaonic-plugin-sample/package.zip', 1, 1
                );",
            )
            .unwrap();
        }

        let conn = open_db(&db_path).unwrap();
        let has_icon = conn
            .prepare("PRAGMA table_info(plugins)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .any(|column| column == "icon");
        assert!(has_icon, "icon column added to legacy database");

        // Rows written before icons existed read back with no icon.
        let records = load_all_plugins(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].icon.is_none());
    }

    #[test]
    fn parses_plugin_package_with_icon() {
        let zip_bytes = build_test_plugin_zip_with_icon("icon.png", Some("files/icon.png"));
        let package = parse_plugin_package(&zip_bytes).unwrap();
        assert_eq!(package.manifest.icon.as_deref(), Some("icon.png"));
    }

    #[test]
    fn rejects_icon_missing_from_package() {
        let zip_bytes = build_test_plugin_zip_with_icon("icon.png", None);
        let err = parse_plugin_package(&zip_bytes).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.detail.contains("icon"), "unexpected detail {}", err.detail);
    }

    #[test]
    fn rejects_icon_with_unsupported_extension() {
        let err = normalize_icon_path(Some("icon.bmp")).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_icon_escaping_plugin_directory() {
        let err = normalize_icon_path(Some("../../etc/shadow.png")).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn normalizes_nested_icon_path() {
        let icon = normalize_icon_path(Some("  assets/icon.svg  "))
            .unwrap()
            .unwrap();
        assert_eq!(icon, "assets/icon.svg");
        assert_eq!(icon_content_type(&icon), "image/svg+xml");
        assert_eq!(icon_content_type("icon.PNG"), "image/png");
    }

    #[test]
    fn treats_blank_icon_as_absent() {
        assert!(normalize_icon_path(Some("   ")).unwrap().is_none());
        assert!(normalize_icon_path(None).unwrap().is_none());
    }

    #[test]
    fn rejects_zero_webview_port() {
        let err = normalize_webview_port(Some(0)).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parses_plugin_package() {
        let zip_bytes = build_test_plugin_zip();
        let package = parse_plugin_package(&zip_bytes).unwrap();
        assert_eq!(package.id, "kaonic-plugin-sample");
        assert_eq!(package.manifest.name, "Sample");
        assert_eq!(package.manifest.channel.as_deref(), Some("experimental"));
        assert_eq!(package.manifest.webview, Some(8780));
        assert_eq!(package.sha256.len(), 64);
        assert_eq!(package.custom_files.len(), 2);
        assert_eq!(
            package.custom_files[0].relative_path,
            PathBuf::from("config/settings.toml")
        );
        assert_eq!(package.custom_files[1].relative_path, PathBuf::from(".env"));
        assert!(package.signature_bytes.is_none());
    }

    #[test]
    fn parses_plugin_package_sig_signature() {
        let zip_bytes = build_test_plugin_zip_with_signature("kaonic-plugin-sample.sig");
        let package = parse_plugin_package(&zip_bytes).unwrap();
        assert_eq!(
            package.signature_bytes.as_deref(),
            Some(b"signature-bytes".as_slice())
        );
    }

    #[test]
    fn parses_plugin_package_legacy_sign_signature() {
        let zip_bytes = build_test_plugin_zip_with_signature("kaonic-plugin-sample.sign");
        let package = parse_plugin_package(&zip_bytes).unwrap();
        assert_eq!(
            package.signature_bytes.as_deref(),
            Some(b"signature-bytes".as_slice())
        );
    }

    #[test]
    fn install_path_accepts_github_artifact_wrapper_zip() {
        let inner_zip = build_test_plugin_zip();
        let cursor = io::Cursor::new(Vec::new());
        let mut wrapper = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default();
        wrapper
            .start_file("kaonic-plugin-sample.zip", options)
            .unwrap();
        wrapper.write_all(&inner_zip).unwrap();
        let wrapped_zip = wrapper.finish().unwrap().into_inner();

        let normalized = crate::installer::normalize_uploaded_zip(&wrapped_zip).unwrap();
        let package = parse_plugin_package(normalized.as_ref()).unwrap();

        assert_eq!(package.id, "kaonic-plugin-sample");
        assert_eq!(package.binary_name, "kaonic-plugin-sample");
    }

    #[test]
    fn rejects_plugin_package_with_bad_sha256() {
        let zip_bytes = build_test_plugin_zip_with_hash("deadbeef");
        let err = parse_plugin_package(&zip_bytes).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_invalid_plugin_channel() {
        let zip_bytes = build_test_plugin_zip_with_channel("beta");
        let err = parse_plugin_package(&zip_bytes).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.detail.contains("plugin channel"));
    }

    #[test]
    fn adds_working_directory_to_service_section() {
        let normalized = normalize_service_working_directory(
            b"[Unit]\nDescription=Sample\n\n[Service]\nExecStart=/bin/true\n",
            Path::new("/etc/kaonic/plugins/sample/current"),
        )
        .unwrap();
        let text = String::from_utf8(normalized).unwrap();
        assert!(text.contains(
            "[Service]\nExecStart=/bin/true\nWorkingDirectory=/etc/kaonic/plugins/sample/current\n"
        ));
    }

    #[test]
    fn replaces_existing_working_directory_in_service_section() {
        let normalized = normalize_service_working_directory(
            b"[Service]\nWorkingDirectory=/tmp\nExecStart=/bin/true\n",
            Path::new("/etc/kaonic/plugins/sample/current"),
        )
        .unwrap();
        let text = String::from_utf8(normalized).unwrap();
        assert!(!text.contains("WorkingDirectory=/tmp"));
        assert!(text.contains("WorkingDirectory=/etc/kaonic/plugins/sample/current"));
    }

    #[test]
    fn installs_custom_files_into_current_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let current_dir = tmp.path().join("current");
        fs::create_dir_all(&current_dir).unwrap();
        let files = vec![
            PluginPackageFile {
                relative_path: PathBuf::from("config/settings.toml"),
                bytes: b"mode = \"demo\"\n".to_vec(),
            },
            PluginPackageFile {
                relative_path: PathBuf::from(".env"),
                bytes: b"NAME=sample\n".to_vec(),
            },
        ];

        install_custom_files(&current_dir, &files).unwrap();

        assert_eq!(
            fs::read_to_string(current_dir.join("config/settings.toml")).unwrap(),
            "mode = \"demo\"\n"
        );
        assert_eq!(
            fs::read_to_string(current_dir.join(".env")).unwrap(),
            "NAME=sample\n"
        );
    }

    #[test]
    fn discovers_untracked_plugin_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_root = tmp.path().join("plugins");
        let plugin_dir = plugins_root.join("kaonic-plugin-sample");
        let current_dir = plugin_dir.join("current");
        fs::create_dir_all(&current_dir).unwrap();
        fs::write(
            plugin_dir.join(MANIFEST_NAME),
            r#"[kaonic-plugin]
name = "Sample"
description = "Sample plugin"
version = "0.1.0"
service = "kaonic-plugin-sample.service"
developer = "Beechat"
channel = "stable"
github_url = "https://github.com/BeechatNetworkSystemsLtd/kaonic-gateway"
webview = 8780
"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("kaonic-plugin-sample.service"),
            "[Service]\nExecStart=/etc/kaonic/plugins/kaonic-plugin-sample/current/kaonic-plugin-sample\n",
        )
        .unwrap();
        fs::write(
            current_dir.join("kaonic-plugin-sample"),
            b"#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        let binary_hash = hex::encode(Sha256::digest(b"#!/bin/sh\nexit 0\n"));
        fs::write(
            plugin_dir.join("kaonic-plugin-sample.sha256"),
            format!("{binary_hash}\n"),
        )
        .unwrap();

        let db_path = plugins_root.join("kaonic-plugins.db");
        initialize_store(&plugins_root, &db_path, tmp.path(), None, &[]).unwrap();

        let plugins = list_plugins(&plugins_root, &db_path, tmp.path(), None, &[]).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "kaonic-plugin-sample");
        assert_eq!(plugins[0].service, "kaonic-plugin-sample.service");
        assert_eq!(plugins[0].channel.as_deref(), Some("stable"));
        assert_eq!(plugins[0].webview, Some(8780));
        assert_eq!(plugins[0].sha256, binary_hash);
    }

    #[test]
    fn initialize_store_recreates_bin_path_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_root = tmp.path().join("plugins");
        let plugin_dir = plugins_root.join("kaonic-plugin-sample");
        let current_dir = plugin_dir.join("current");
        let bin_link = tmp.path().join("usr/bin/kaonic-plugin-sample");
        fs::create_dir_all(&current_dir).unwrap();
        fs::write(
            plugin_dir.join(MANIFEST_NAME),
            format!(
                r#"[kaonic-plugin]
name = "Sample"
description = "Sample plugin"
version = "0.1.0"
service = "kaonic-plugin-sample.service"
developer = "Beechat"
channel = "stable"
symlink = "{}"
"#,
                bin_link.display()
            ),
        )
        .unwrap();
        fs::write(
            plugin_dir.join("kaonic-plugin-sample.service"),
            "[Service]\nExecStart=/etc/kaonic/plugins/kaonic-plugin-sample/current/kaonic-plugin-sample\n",
        )
        .unwrap();
        fs::write(
            current_dir.join("kaonic-plugin-sample"),
            b"#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        let binary_hash = hex::encode(Sha256::digest(b"#!/bin/sh\nexit 0\n"));
        fs::write(
            plugin_dir.join("kaonic-plugin-sample.sha256"),
            format!("{binary_hash}\n"),
        )
        .unwrap();

        let db_path = plugins_root.join("kaonic-plugins.db");
        initialize_store(&plugins_root, &db_path, tmp.path(), None, &[]).unwrap();
        assert_eq!(
            fs::read_link(&bin_link).unwrap(),
            current_dir.join("kaonic-plugin-sample")
        );

        fs::remove_file(&bin_link).unwrap();
        initialize_store(&plugins_root, &db_path, tmp.path(), None, &[]).unwrap();
        assert_eq!(
            fs::read_link(&bin_link).unwrap(),
            current_dir.join("kaonic-plugin-sample")
        );
    }

    #[test]
    fn lists_plugins_with_stored_sha256_from_database() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_root = tmp.path().join("plugins");
        let plugin_dir = plugins_root.join("kaonic-plugin-sample");
        let current_dir = plugin_dir.join("current");
        fs::create_dir_all(&current_dir).unwrap();
        fs::write(
            plugin_dir.join(MANIFEST_NAME),
            r#"[kaonic-plugin]
name = "Sample"
description = "Sample plugin"
version = "0.1.0"
service = "kaonic-plugin-sample.service"
developer = "Beechat"
channel = "experimental"
"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("kaonic-plugin-sample.service"),
            "[Service]\nExecStart=/etc/kaonic/plugins/kaonic-plugin-sample/current/kaonic-plugin-sample\n",
        )
        .unwrap();
        fs::write(
            current_dir.join("kaonic-plugin-sample"),
            b"#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        let original_hash = hex::encode(Sha256::digest(b"#!/bin/sh\nexit 0\n"));
        fs::write(
            plugin_dir.join("kaonic-plugin-sample.sha256"),
            format!("{original_hash}\n"),
        )
        .unwrap();

        let db_path = plugins_root.join("kaonic-plugins.db");
        initialize_store(&plugins_root, &db_path, tmp.path(), None, &[]).unwrap();
        fs::write(
            current_dir.join("kaonic-plugin-sample"),
            b"#!/bin/sh\necho live\n",
        )
        .unwrap();

        let plugins = list_plugins(&plugins_root, &db_path, tmp.path(), None, &[]).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].sha256, original_hash);
    }

    #[test]
    fn manages_plugin_symlink_safely() {
        let tmp = tempfile::tempdir().unwrap();
        let rollback_dir = tmp.path().join("rollback");
        let install_dir = tmp.path().join("plugin");
        let current_dir = install_dir.join("current");
        let link_path = tmp.path().join("bin").join("sample");
        fs::create_dir_all(&rollback_dir).unwrap();
        fs::create_dir_all(&current_dir).unwrap();
        let binary_path = current_dir.join("sample");
        fs::write(&binary_path, b"sample").unwrap();

        install_bin_path(
            Some(link_path.to_str().unwrap()),
            None,
            &binary_path,
            &rollback_dir,
        )
        .unwrap();
        assert_eq!(fs::read_link(&link_path).unwrap(), binary_path);

        remove_managed_symlink(&link_path, &binary_path).unwrap();
        assert!(fs::symlink_metadata(&link_path).is_err());
    }

    #[test]
    fn rejects_deleting_built_in_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_root = tmp.path().join("plugins");
        let db_path = plugins_root.join("kaonic-plugins.db");
        let systemd_dir = tmp.path().join("systemd");
        let target = Arc::new(Target {
            name: "gateway",
            symlink_path: tmp.path().join("usr/bin/kaonic-gateway"),
            binary_path: tmp
                .path()
                .join("plugins/kaonic-gateway/current/kaonic-gateway"),
            service: "kaonic-gateway.service",
            meta_dir: tmp.path().join("meta"),
        });

        fs::create_dir_all(target.symlink_path.parent().unwrap()).unwrap();
        fs::create_dir_all(target.binary_path.parent().unwrap()).unwrap();
        fs::create_dir_all(&target.meta_dir).unwrap();
        fs::write(&target.binary_path, b"gateway").unwrap();
        std::os::unix::fs::symlink(&target.binary_path, &target.symlink_path).unwrap();
        fs::write(target.meta_dir.join("kaonic-gateway.version"), "1.0.0\n").unwrap();
        fs::write(target.meta_dir.join("kaonic-gateway.sha256"), "abcd\n").unwrap();
        let plugin_dir = plugins_root.join("kaonic-gateway");
        fs::create_dir_all(plugin_dir.join("current")).unwrap();
        fs::write(
            plugin_dir.join(MANIFEST_NAME),
            format!(
                "[kaonic-plugin]\nname = \"Kaonic Gateway\"\ndescription = \"Built-in gateway service.\"\nversion = \"0.1.0\"\nservice = \"kaonic-gateway.service\"\ndeveloper = \"Beechat\"\nchannel = \"stable\"\nsymlink = \"{}\"\n",
                target.symlink_path.display()
            ),
        )
        .unwrap();
        fs::write(
            plugin_dir.join("kaonic-gateway.service"),
            "[Service]\nExecStart=/usr/bin/kaonic-gateway\n",
        )
        .unwrap();
        fs::write(plugin_dir.join("current/kaonic-gateway"), b"gateway").unwrap();

        initialize_store(
            &plugins_root,
            &db_path,
            &systemd_dir,
            None,
            &[CorePluginSpec::new(target.clone(), &plugins_root)],
        )
        .unwrap();

        let err =
            delete_plugin(&plugins_root, &db_path, &systemd_dir, "kaonic-gateway").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.detail.contains("built-in"));
    }

    #[test]
    fn syncs_built_in_plugin_metadata_from_manifest_file() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_root = tmp.path().join("plugins");
        let db_path = plugins_root.join("kaonic-plugins.db");
        let systemd_dir = tmp.path().join("systemd");
        let target = Arc::new(Target {
            name: "gateway",
            symlink_path: tmp.path().join("usr/bin/kaonic-gateway"),
            binary_path: tmp
                .path()
                .join("plugins/kaonic-gateway/current/kaonic-gateway"),
            service: "kaonic-gateway.service",
            meta_dir: tmp.path().join("meta"),
        });

        fs::create_dir_all(target.symlink_path.parent().unwrap()).unwrap();
        fs::create_dir_all(target.binary_path.parent().unwrap()).unwrap();
        fs::create_dir_all(&target.meta_dir).unwrap();
        fs::write(&target.binary_path, b"gateway").unwrap();
        std::os::unix::fs::symlink(&target.binary_path, &target.symlink_path).unwrap();
        fs::write(target.meta_dir.join("kaonic-gateway.version"), "1.2.3\n").unwrap();
        fs::write(target.meta_dir.join("kaonic-gateway.sha256"), "abcd\n").unwrap();

        let plugin_dir = plugins_root.join("kaonic-gateway");
        fs::create_dir_all(plugin_dir.join("current")).unwrap();
        fs::write(
            plugin_dir.join(MANIFEST_NAME),
            format!(
                "[kaonic-plugin]\nname = \"Gateway From Manifest\"\ndescription = \"Loaded from kaonic-plugin.toml\"\nversion = \"0.9.0\"\nservice = \"kaonic-gateway.service\"\ndeveloper = \"Manifest Dev\"\nchannel = \"stable\"\ngithub_url = \"https://example.invalid/gateway\"\nwebview = 8085\nsymlink = \"{}\"\n",
                target.symlink_path.display()
            ),
        )
        .unwrap();
        fs::write(
            plugin_dir.join("kaonic-gateway.service"),
            "[Service]\nExecStart=/usr/bin/kaonic-gateway\n",
        )
        .unwrap();
        fs::write(plugin_dir.join("current/kaonic-gateway"), b"gateway").unwrap();

        initialize_store(
            &plugins_root,
            &db_path,
            &systemd_dir,
            None,
            &[CorePluginSpec::new(target.clone(), &plugins_root)],
        )
        .unwrap();

        let plugins = list_plugins(&plugins_root, &db_path, &systemd_dir, None, &[]).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "Gateway From Manifest");
        assert_eq!(plugins[0].description, "Loaded from kaonic-plugin.toml");
        assert_eq!(plugins[0].developer, "Manifest Dev");
        assert_eq!(plugins[0].channel.as_deref(), Some("stable"));
        assert_eq!(
            plugins[0].github_url.as_deref(),
            Some("https://example.invalid/gateway")
        );
        assert_eq!(
            plugins[0].bin_path.as_deref(),
            Some(target.symlink_path.to_string_lossy().as_ref())
        );
        assert_eq!(plugins[0].webview, Some(8085));
        assert_eq!(plugins[0].version, "1.2.3");
    }

    #[test]
    fn recovers_plugin_from_rollback_after_interrupted_update() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_root = tmp.path().join("plugins");
        let plugin_dir = plugins_root.join("kaonic-plugin-sample");
        let current_dir = plugin_dir.join("current");
        let rollback_dir = plugin_dir.join(".rollback");
        let db_path = plugins_root.join("kaonic-plugins.db");
        let systemd_dir = tmp.path().join("systemd");
        fs::create_dir_all(&current_dir).unwrap();
        fs::create_dir_all(&rollback_dir).unwrap();
        fs::create_dir_all(&systemd_dir).unwrap();

        let previous_binary = b"#!/bin/sh\nexit 0\n";
        let previous_hash = hex::encode(Sha256::digest(previous_binary));
        fs::write(
            plugin_dir.join(MANIFEST_NAME),
            r#"[kaonic-plugin]
name = "Sample"
description = "Sample plugin"
version = "0.2.0"
service = "kaonic-plugin-sample.service"
developer = "Beechat"
"#,
        )
        .unwrap();
        fs::write(
            plugin_dir.join("kaonic-plugin-sample.service"),
            "[Service]\nExecStart=/etc/kaonic/plugins/kaonic-plugin-sample/current/kaonic-plugin-sample\n",
        )
        .unwrap();
        fs::write(current_dir.join("kaonic-plugin-sample"), b"broken").unwrap();
        fs::write(
            plugin_dir.join("kaonic-plugin-sample.sha256"),
            "0000000000000000000000000000000000000000000000000000000000000000\n",
        )
        .unwrap();
        fs::write(
            rollback_dir.join(MANIFEST_NAME),
            r#"[kaonic-plugin]
name = "Sample"
description = "Sample plugin"
version = "0.1.0"
service = "kaonic-plugin-sample.service"
developer = "Beechat"
"#,
        )
        .unwrap();
        fs::write(
            rollback_dir.join("kaonic-plugin-sample.service"),
            "[Service]\nExecStart=/etc/kaonic/plugins/kaonic-plugin-sample/current/kaonic-plugin-sample\n",
        )
        .unwrap();
        fs::create_dir_all(rollback_dir.join("current")).unwrap();
        fs::write(
            rollback_dir.join("current").join("kaonic-plugin-sample"),
            previous_binary,
        )
        .unwrap();
        fs::write(
            rollback_dir.join("kaonic-plugin-sample.sha256"),
            format!("{previous_hash}\n"),
        )
        .unwrap();
        fs::write(rollback_dir.join("package.zip"), b"rollback-zip").unwrap();

        {
            let conn = open_db(&db_path).unwrap();
            upsert_plugin(
                &conn,
                PluginRecord {
                    id: "kaonic-plugin-sample".into(),
                    name: "Sample".into(),
                    description: "Sample plugin".into(),
                    version: "0.1.0".into(),
                    service: "kaonic-plugin-sample.service".into(),
                    developer: "Beechat".into(),
                    channel: Some("stable".into()),
                    github_url: None,
                    webview: None,
                    tls: false,
                    icon: None,
                    binary_name: "kaonic-plugin-sample".into(),
                    bin_path: None,
                    sha256: previous_hash.clone(),
                    install_dir: plugin_dir.to_string_lossy().into_owned(),
                    package_path: plugin_dir
                        .join("package.zip")
                        .to_string_lossy()
                        .into_owned(),
                    official: false,
                    enabled: false,
                    removable: true,
                    target_name: None,
                    installed_at: 1,
                    updated_at: 1,
                },
            )
            .unwrap();
        }

        initialize_store(&plugins_root, &db_path, &systemd_dir, None, &[]).unwrap();

        assert_eq!(
            fs::read(plugin_dir.join("current").join("kaonic-plugin-sample")).unwrap(),
            previous_binary
        );
        assert_eq!(
            read_sha256_file(&plugin_dir.join("kaonic-plugin-sample.sha256")).unwrap(),
            previous_hash
        );
        assert!(!rollback_dir.exists());
    }

    fn build_test_plugin_zip() -> Vec<u8> {
        let binary = b"#!/bin/sh\nexit 0\n";
        let sha256 = hex::encode(Sha256::digest(binary));
        build_test_plugin_zip_with_hash_and_channel(&sha256, "experimental")
    }

    fn build_test_plugin_zip_with_channel(channel: &str) -> Vec<u8> {
        let binary = b"#!/bin/sh\nexit 0\n";
        let sha256 = hex::encode(Sha256::digest(binary));
        build_test_plugin_zip_with_hash_and_channel(&sha256, channel)
    }

    fn build_test_plugin_zip_with_hash(sha256: &str) -> Vec<u8> {
        build_test_plugin_zip_with_hash_and_channel(sha256, "experimental")
    }

    fn build_test_plugin_zip_with_signature(signature_name: &str) -> Vec<u8> {
        let mut cursor = Cursor::new(build_test_plugin_zip());
        let mut archive = ZipArchive::new(&mut cursor).unwrap();
        let mut out = Cursor::new(Vec::<u8>::new());
        {
            let mut writer = ZipWriter::new(&mut out);
            let options = SimpleFileOptions::default();
            for index in 0..archive.len() {
                let mut file = archive.by_index(index).unwrap();
                writer.start_file(file.name(), options).unwrap();
                io::copy(&mut file, &mut writer).unwrap();
            }
            writer.start_file(signature_name, options).unwrap();
            writer.write_all(b"signature-bytes").unwrap();
            writer.finish().unwrap();
        }
        out.into_inner()
    }

    fn build_test_plugin_zip_with_icon(icon_value: &str, icon_entry: Option<&str>) -> Vec<u8> {
        let mut cursor = Cursor::new(build_test_plugin_zip());
        let mut archive = ZipArchive::new(&mut cursor).unwrap();
        let mut out = Cursor::new(Vec::<u8>::new());
        {
            let mut writer = ZipWriter::new(&mut out);
            let options = SimpleFileOptions::default();
            for index in 0..archive.len() {
                let mut file = archive.by_index(index).unwrap();
                let name = file.name().to_string();
                writer.start_file(&name, options).unwrap();
                if name == MANIFEST_NAME {
                    let mut manifest = Vec::new();
                    io::copy(&mut file, &mut manifest).unwrap();
                    let mut manifest = String::from_utf8(manifest).unwrap();
                    manifest.push_str(&format!("icon = \"{icon_value}\"\n"));
                    writer.write_all(manifest.as_bytes()).unwrap();
                } else {
                    io::copy(&mut file, &mut writer).unwrap();
                }
            }
            if let Some(entry) = icon_entry {
                writer.start_file(entry, options).unwrap();
                writer.write_all(b"icon-bytes").unwrap();
            }
            writer.finish().unwrap();
        }
        out.into_inner()
    }

    fn build_test_plugin_zip_with_hash_and_channel(sha256: &str, channel: &str) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        {
            let mut writer = ZipWriter::new(&mut cursor);
            let options = SimpleFileOptions::default();
            writer.start_file(MANIFEST_NAME, options).unwrap();
            let manifest = format!(
                "[kaonic-plugin]\nname = \"Sample\"\ndescription = \"Sample plugin\"\nversion = \"0.1.0\"\nservice = \"kaonic-plugin-sample.service\"\ndeveloper = \"Beechat\"\nchannel = \"{channel}\"\ngithub_url = \"https://github.com/BeechatNetworkSystemsLtd/kaonic-gateway\"\nwebview = 8780\n"
            );
            writer.write_all(manifest.as_bytes()).unwrap();
            writer
                .start_file("kaonic-plugin-sample.service", options)
                .unwrap();
            writer
                .write_all(b"[Service]\nExecStart=/bin/true\n")
                .unwrap();
            writer
                .start_file("files/config/settings.toml", options)
                .unwrap();
            writer.write_all(b"mode = \"demo\"\n").unwrap();
            writer.start_file("files/.env", options).unwrap();
            writer.write_all(b"NAME=sample\n").unwrap();
            writer
                .start_file("kaonic-plugin-sample.sha256", options)
                .unwrap();
            writer.write_all(sha256.as_bytes()).unwrap();
            writer.start_file("kaonic-plugin-sample", options).unwrap();
            writer.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
            writer.finish().unwrap();
        }
        cursor.into_inner()
    }
}
