use crate::app_types::ServiceStatusDto;

pub const GATEWAY_SERVICE_UNITS: [&str; 4] = [
    "kaonic-commd.service",
    "kaonic-factory.service",
    "kaonic-gateway.service",
    "kaonic-installer.service",
];

pub async fn read_cpu_percent_async() -> f32 {
    let Some((idle1, total1)) = parse_stat() else {
        return 0.0;
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let Some((idle2, total2)) = parse_stat() else {
        return 0.0;
    };
    let total_diff = total2.saturating_sub(total1) as f32;
    let idle_diff = idle2.saturating_sub(idle1) as f32;
    if total_diff == 0.0 {
        return 0.0;
    }
    ((total_diff - idle_diff) / total_diff * 100.0 * 10.0).round() / 10.0
}

pub fn read_mem_mb() -> (u64, u64) {
    let Ok(data) = std::fs::read_to_string("/proc/meminfo") else {
        return (0, 0);
    };
    let mut total = 0u64;
    let mut available = 0u64;
    for line in data.lines() {
        if let Some(r) = line.strip_prefix("MemTotal:") {
            total = r
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        } else if let Some(r) = line.strip_prefix("MemAvailable:") {
            available = r
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    (total.saturating_sub(available) / 1024, total / 1024)
}

pub fn read_fs_mb() -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::ffi::CString;

        let probe_path = user_filesystem_probe_path();
        let Ok(path) = CString::new(probe_path.to_string_lossy().into_owned()) else {
            return (0, 0);
        };
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        let rc = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
        if rc != 0 {
            return (0, 0);
        }
        let stats = unsafe { stats.assume_init() };
        let block_size = if stats.f_frsize > 0 {
            stats.f_frsize as u64
        } else {
            stats.f_bsize as u64
        };
        let total = (stats.f_blocks as u64).saturating_mul(block_size);
        let free = (stats.f_bavail as u64).saturating_mul(block_size);
        return (free / 1024 / 1024, total / 1024 / 1024);
    }

    #[cfg(not(unix))]
    {
        (0, 0)
    }
}

pub fn read_os_details() -> String {
    let os_name = read_os_name();
    let kernel = read_kernel_release();
    match (os_name.is_empty(), kernel.is_empty()) {
        (false, false) => format!("{os_name} / {kernel}"),
        (false, true) => os_name,
        (true, false) => kernel,
        (true, true) => "Unknown".into(),
    }
}

pub fn read_hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Unknown".into())
}

pub fn read_cpu_model() -> String {
    let Ok(data) = std::fs::read_to_string("/proc/cpuinfo") else {
        return "Unknown".into();
    };

    for key in ["model name", "Hardware", "Processor", "Model"] {
        for line in data.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.trim() == key {
                let value = value.trim();
                if !value.is_empty() {
                    return value.to_string();
                }
            }
        }
    }

    "Unknown".into()
}

pub fn read_architecture() -> String {
    #[cfg(unix)]
    {
        let mut uts = std::mem::MaybeUninit::<libc::utsname>::uninit();
        let rc = unsafe { libc::uname(uts.as_mut_ptr()) };
        if rc == 0 {
            let uts = unsafe { uts.assume_init() };
            let machine = unsafe { std::ffi::CStr::from_ptr(uts.machine.as_ptr()) };
            let value = machine.to_string_lossy().trim().to_string();
            if !value.is_empty() {
                return value;
            }
        }
    }

    std::env::consts::ARCH.into()
}

pub fn read_cpu_cores() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

/// Current CPU clock in MHz. Prefers the busiest core's live `scaling_cur_freq`
/// (reflects DVFS/throttling), falling back to `/proc/cpuinfo` "cpu MHz".
/// Returns 0 when no frequency source is available.
pub fn read_cpu_freq_mhz() -> u32 {
    // cpufreq exposes the live per-core frequency in kHz under sysfs.
    let mut max_khz: u64 = 0;
    if let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") {
        for entry in entries.flatten() {
            let path = entry.path().join("cpufreq/scaling_cur_freq");
            if let Ok(data) = std::fs::read_to_string(&path) {
                if let Ok(khz) = data.trim().parse::<u64>() {
                    max_khz = max_khz.max(khz);
                }
            }
        }
    }
    if max_khz > 0 {
        return (max_khz / 1000) as u32;
    }

    // Fallback for platforms without cpufreq sysfs (e.g. some x86 kernels).
    if let Ok(data) = std::fs::read_to_string("/proc/cpuinfo") {
        let mut max_mhz: f64 = 0.0;
        for line in data.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.trim() == "cpu MHz" {
                if let Ok(mhz) = value.trim().parse::<f64>() {
                    max_mhz = max_mhz.max(mhz);
                }
            }
        }
        if max_mhz > 0.0 {
            return max_mhz.round() as u32;
        }
    }

    0
}

/// Service state changes rarely, so cache the result for a short window to avoid
/// forking `systemctl` on every 1 s status tick and on every new WebSocket
/// connection — the fork/exec storm was the source of periodic CPU spikes.
const SERVICE_STATUS_TTL: std::time::Duration = std::time::Duration::from_secs(3);

fn service_cache(
) -> &'static std::sync::Mutex<Option<(std::time::Instant, Vec<ServiceStatusDto>)>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<Option<(std::time::Instant, Vec<ServiceStatusDto>)>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

pub async fn read_gateway_services() -> Vec<ServiceStatusDto> {
    // Serve from cache while fresh (lock is released before any await).
    {
        let guard = service_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((fetched_at, services)) = guard.as_ref() {
            if fetched_at.elapsed() < SERVICE_STATUS_TTL {
                return services.clone();
            }
        }
    }

    // Prefer the installer's shared snapshot — it already polls systemd for every
    // unit on the device, so reading it costs no fork here. Fall back to our own
    // `systemctl show` when the installer is unreachable, since that is exactly
    // when its own unit state matters most.
    let services = match query_services_from_installer().await {
        Some(services) => services,
        None => query_gateway_services().await,
    };

    let mut guard = service_cache().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some((std::time::Instant::now(), services.clone()));
    services
}

pub fn is_gateway_service_unit(unit: &str) -> bool {
    GATEWAY_SERVICE_UNITS.contains(&unit)
}

const INSTALLER_SERVICES_URL: &str = "http://127.0.0.1:8682/api/services";

#[derive(serde::Deserialize)]
struct InstallerServiceStatus {
    #[serde(default)]
    active_state: String,
    #[serde(default)]
    sub_state: String,
    #[serde(default)]
    load_state: String,
}

/// Read the installer's cached systemd snapshot. `None` means "not usable" —
/// unreachable, malformed, or missing one of the units we render — and the
/// caller then falls back to querying systemd directly.
async fn query_services_from_installer() -> Option<Vec<ServiceStatusDto>> {
    let response = reqwest::Client::new()
        .get(INSTALLER_SERVICES_URL)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let statuses = response
        .json::<std::collections::HashMap<String, InstallerServiceStatus>>()
        .await
        .ok()?;

    if !GATEWAY_SERVICE_UNITS
        .iter()
        .all(|unit| statuses.contains_key(*unit))
    {
        return None;
    }

    Some(
        GATEWAY_SERVICE_UNITS
            .iter()
            .map(|&unit| {
                let status = &statuses[unit];
                ServiceStatusDto {
                    unit: unit.into(),
                    brief_name: service_brief_name(unit).into(),
                    status: format_service_status(
                        &status.load_state,
                        &status.active_state,
                        &status.sub_state,
                    ),
                    load_state: status.load_state.clone(),
                    active_state: status.active_state.clone(),
                    sub_state: status.sub_state.clone(),
                }
            })
            .collect(),
    )
}

/// Query all gateway units with a single `systemctl show` invocation (one fork
/// instead of one per unit) and without blocking the async runtime.
#[cfg(target_os = "linux")]
async fn query_gateway_services() -> Vec<ServiceStatusDto> {
    let output = tokio::process::Command::new("systemctl")
        .args([
            "show",
            "--property=Id",
            "--property=LoadState",
            "--property=ActiveState",
            "--property=SubState",
        ])
        .args(GATEWAY_SERVICE_UNITS)
        .output()
        .await;

    match output {
        Ok(output) if output.status.success() => {
            parse_show_output(&String::from_utf8_lossy(&output.stdout))
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let message = if stderr.is_empty() { stdout } else { stderr };
            GATEWAY_SERVICE_UNITS
                .iter()
                .map(|unit| service_error_dto(unit, &message))
                .collect()
        }
        Err(err) => {
            let message = format!("systemctl unavailable: {err}");
            GATEWAY_SERVICE_UNITS
                .iter()
                .map(|unit| service_error_dto(unit, &message))
                .collect()
        }
    }
}

/// Parse the property blocks emitted by `systemctl show` for multiple units.
/// Each unit's properties form a block; blocks are separated by a blank line.
/// We match blocks back to units by their `Id` so ordering is irrelevant.
#[cfg(target_os = "linux")]
fn parse_show_output(stdout: &str) -> Vec<ServiceStatusDto> {
    use std::collections::HashMap;

    let mut by_id: HashMap<String, (String, String, String)> = HashMap::new();
    for block in stdout.split("\n\n") {
        let (mut id, mut load_state, mut active_state, mut sub_state) =
            (String::new(), String::new(), String::new(), String::new());
        for line in block.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().to_string();
            match key.trim() {
                "Id" => id = value,
                "LoadState" => load_state = value,
                "ActiveState" => active_state = value,
                "SubState" => sub_state = value,
                _ => {}
            }
        }
        if !id.is_empty() {
            by_id.insert(id, (load_state, active_state, sub_state));
        }
    }

    GATEWAY_SERVICE_UNITS
        .iter()
        .map(|&unit| match by_id.get(unit) {
            Some((load_state, active_state, sub_state)) => ServiceStatusDto {
                unit: unit.into(),
                brief_name: service_brief_name(unit).into(),
                status: format_service_status(load_state, active_state, sub_state),
                load_state: load_state.clone(),
                active_state: active_state.clone(),
                sub_state: sub_state.clone(),
            },
            None => service_error_dto(unit, "not reported by systemctl"),
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn service_error_dto(unit: &str, message: &str) -> ServiceStatusDto {
    ServiceStatusDto {
        unit: unit.into(),
        brief_name: service_brief_name(unit).into(),
        load_state: "unknown".into(),
        active_state: "error".into(),
        sub_state: String::new(),
        status: if message.is_empty() {
            "systemctl error".into()
        } else {
            message.into()
        },
    }
}

#[cfg(not(target_os = "linux"))]
async fn query_gateway_services() -> Vec<ServiceStatusDto> {
    GATEWAY_SERVICE_UNITS
        .iter()
        .map(|&unit| ServiceStatusDto {
            unit: unit.into(),
            brief_name: service_brief_name(unit).into(),
            load_state: "mock".into(),
            active_state: "unknown".into(),
            sub_state: String::new(),
            status: "Unavailable on this host".into(),
        })
        .collect()
}

fn service_brief_name(unit: &str) -> &'static str {
    match unit {
        "kaonic-commd.service" => "Radio control",
        "kaonic-factory.service" => "Factory setup",
        "kaonic-gateway.service" => "Web gateway",
        "kaonic-installer.service" => "Installer agent",
        _ => "Service",
    }
}

fn format_service_status(load_state: &str, active_state: &str, sub_state: &str) -> String {
    if load_state != "loaded" && !load_state.is_empty() {
        return load_state.to_string();
    }
    if active_state.is_empty() && sub_state.is_empty() {
        return "unknown".into();
    }
    if sub_state.is_empty() || sub_state == active_state {
        return active_state.to_string();
    }
    format!("{active_state} ({sub_state})")
}

fn parse_stat() -> Option<(u64, u64)> {
    let data = std::fs::read_to_string("/proc/stat").ok()?;
    let line = data.lines().next()?;
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if vals.len() < 4 {
        return None;
    }
    Some((vals[3], vals.iter().sum()))
}

#[cfg(unix)]
fn user_filesystem_probe_path() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.exists())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
}

fn read_os_name() -> String {
    if let Ok(data) = std::fs::read_to_string("/etc/os-release") {
        for line in data.lines() {
            if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                return value.trim_matches('"').to_string();
            }
        }
    }
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

fn read_kernel_release() -> String {
    #[cfg(unix)]
    {
        let mut uts = std::mem::MaybeUninit::<libc::utsname>::uninit();
        let rc = unsafe { libc::uname(uts.as_mut_ptr()) };
        if rc == 0 {
            let uts = unsafe { uts.assume_init() };
            let release = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) };
            return format!("Kernel {}", release.to_string_lossy());
        }
    }

    String::new()
}
