use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use super::PageTitle;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub active: bool,
    pub enabled: bool,
    pub status_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MavlinkSnapshot {
    pub fc: ServiceStatus,
    pub gc: ServiceStatus,
}

// ── Server Functions ──────────────────────────────────────────────────────────

#[server]
pub async fn load_mavlink_snapshot() -> Result<MavlinkSnapshot, ServerFnError> {
    let fc = get_service_status("rns-mavlink-fc.service").await?;
    let gc = get_service_status("rns-mavlink-gc.service").await?;

    Ok(MavlinkSnapshot { fc, gc })
}

#[server]
pub async fn control_mavlink_service(
    service: String,
    action: String,
) -> Result<(), ServerFnError> {
    // Validate service name
    if service != "rns-mavlink-fc.service" && service != "rns-mavlink-gc.service" {
        return Err(ServerFnError::new("Invalid service name"));
    }

    // Validate action
    let valid_actions = ["start", "stop", "restart", "enable", "disable"];
    if !valid_actions.contains(&action.as_str()) {
        return Err(ServerFnError::new("Invalid action"));
    }

    log::info!("MAVLink control: {} {}", action, service);

    let output = tokio::process::Command::new("systemctl")
        .args([&action, &service])
        .output()
        .await
        .map_err(|e| {
            log::error!("Failed to execute systemctl: {}", e);
            ServerFnError::new("Failed to execute systemctl")
        })?;

    if output.status.success() {
        Ok(())
    } else {
        let error = String::from_utf8_lossy(&output.stderr);
        log::error!("systemctl {} {} failed: {}", action, service, error);
        Err(ServerFnError::new(format!(
            "systemctl command failed: {}",
            error
        )))
    }
}

async fn get_service_status(service: &str) -> Result<ServiceStatus, ServerFnError> {
    let active = tokio::process::Command::new("systemctl")
        .args(["is-active", service])
        .output()
        .await
        .map_err(|e| ServerFnError::new(format!("Failed to check service status: {}", e)))?
        .status
        .success();

    let enabled = tokio::process::Command::new("systemctl")
        .args(["is-enabled", service])
        .output()
        .await
        .map_err(|e| ServerFnError::new(format!("Failed to check service enabled: {}", e)))?
        .status
        .success();

    let status_output = tokio::process::Command::new("systemctl")
        .args(["status", service])
        .output()
        .await
        .map_err(|e| ServerFnError::new(format!("Failed to get service status: {}", e)))?;

    let status_text = String::from_utf8_lossy(&status_output.stdout)
        .lines()
        .find(|line| line.trim().starts_with("Active:"))
        .map(|line| line.trim().to_string())
        .unwrap_or_else(|| "Active: unknown".to_string());

    Ok(ServiceStatus {
        active,
        enabled,
        status_text,
    })
}

// ── Page Component ────────────────────────────────────────────────────────────

#[component]
pub fn MavlinkPage() -> impl IntoView {
    let reload_trigger = RwSignal::new(0);

    let reload = move || {
        reload_trigger.update(|n| *n += 1);
    };

    let snapshot_with_reload = Resource::new(
        move || reload_trigger.get(),
        |_| load_mavlink_snapshot(),
    );

    view! {
        <div class="page">
            <PageTitle icon="✈️" title="MAVLink Services" />
            <Suspense fallback=|| view! { <p class="loading">"Loading…"</p> }>
                {move || match snapshot_with_reload.get() {
                    None => view! { <p class="loading">"Loading…"</p> }.into_any(),
                    Some(Err(e)) => view! {
                        <div class="error-banner">"Error: "{e.to_string()}</div>
                    }.into_any(),
                    Some(Ok(snap)) => view! {
                        <MavlinkContent snapshot=snap reload=reload/>
                    }.into_any(),
                }}
            </Suspense>
        </div>
    }
}

#[component]
fn MavlinkContent(snapshot: MavlinkSnapshot, reload: impl Fn() + 'static + Copy) -> impl IntoView {
    view! {
        <div class="mavlink-services">
            <ServiceCard
                name="Flight Controller (FC)"
                service="rns-mavlink-fc.service"
                status=snapshot.fc
                reload=reload
            />
            <ServiceCard
                name="Ground Control (GC)"
                service="rns-mavlink-gc.service"
                status=snapshot.gc
                reload=reload
            />
        </div>
        <style>{MAVLINK_CSS}</style>
    }
}

#[component]
fn ServiceCard(
    name: &'static str,
    service: &'static str,
    status: ServiceStatus,
    reload: impl Fn() + 'static + Copy,
) -> impl IntoView {
    let is_active = status.active;
    let is_enabled = status.enabled;
    let status_text = status.status_text.clone();

    let action_pending = RwSignal::new(false);
    let action_error = RwSignal::new(None::<String>);

    let execute_action = move |action: &'static str| {
        let service = service.to_string();
        let action_str = action.to_string();

        leptos::task::spawn_local(async move {
            action_pending.set(true);
            action_error.set(None);

            match control_mavlink_service(service, action_str.clone()).await {
                Ok(_) => {
                    log::info!("Action {} completed successfully", action_str);
                    reload();
                }
                Err(e) => {
                    log::error!("Action {} failed: {}", action_str, e);
                    action_error.set(Some(e.to_string()));
                }
            }

            action_pending.set(false);
        });
    };

    let status_class = if is_active {
        "service-status service-status--active"
    } else {
        "service-status service-status--inactive"
    };

    let enabled_badge_class = if is_enabled {
        "badge badge-ok"
    } else {
        "badge badge-warn"
    };

    view! {
        <div class="card service-card">
            <div class="card-header">
                <span class="card-title">{name}</span>
                <span class=enabled_badge_class>
                    {if is_enabled { "Enabled" } else { "Disabled" }}
                </span>
            </div>

            <div class=status_class>
                <div class="service-status-indicator">
                    {if is_active { "●" } else { "○" }}
                </div>
                <div class="service-status-text">{status_text}</div>
            </div>

            {move || action_error.get().map(|err| view! {
                <div class="error-banner" style="margin: 12px 0;">
                    "Error: "{err}
                </div>
            })}

            <div class="service-actions">
                <div class="service-actions-group">
                    <button
                        type="button"
                        class="btn-primary"
                        disabled=move || action_pending.get()
                        on:click=move |_| execute_action("start")
                    >
                        {move || if action_pending.get() { "Working…" } else { "Start" }}
                    </button>
                    <button
                        type="button"
                        class="btn-secondary"
                        disabled=move || action_pending.get()
                        on:click=move |_| execute_action("stop")
                    >
                        "Stop"
                    </button>
                    <button
                        type="button"
                        class="btn-secondary"
                        disabled=move || action_pending.get()
                        on:click=move |_| execute_action("restart")
                    >
                        "Restart"
                    </button>
                </div>
                <div class="service-actions-group">
                    <button
                        type="button"
                        class="btn-secondary"
                        disabled=move || action_pending.get()
                        on:click=move |_| execute_action("enable")
                    >
                        "Enable"
                    </button>
                    <button
                        type="button"
                        class="btn-secondary"
                        disabled=move || action_pending.get()
                        on:click=move |_| execute_action("disable")
                    >
                        "Disable"
                    </button>
                </div>
            </div>
        </div>
    }
}

// ── Styles ────────────────────────────────────────────────────────────────────

const MAVLINK_CSS: &str = r#"
.mavlink-services {
    display: grid;
    gap: 1.5rem;
    margin-bottom: 2rem;
}

.service-card {
    background: var(--bg-elevated);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 1.5rem;
}

.service-status {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding: 1rem;
    margin: 1rem 0;
    background: var(--bg-base);
    border-radius: 6px;
    font-family: var(--font-mono);
    font-size: 0.875rem;
}

.service-status-indicator {
    font-size: 1.5rem;
    line-height: 1;
}

.service-status--active .service-status-indicator {
    color: var(--status-ok);
}

.service-status--inactive .service-status-indicator {
    color: var(--text-muted);
}

.service-status-text {
    flex: 1;
    color: var(--text-secondary);
}

.service-actions {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
}

.service-actions-group {
    display: flex;
    gap: 0.75rem;
    flex-wrap: wrap;
}

.service-actions button {
    padding: 0.5rem 1rem;
    font-size: 0.875rem;
    border-radius: 4px;
    border: none;
    cursor: pointer;
    font-weight: 600;
    transition: all 0.15s ease;
}

.service-actions button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
}

.btn-primary {
    background: var(--accent-primary);
    color: white;
}

.btn-primary:hover:not(:disabled) {
    background: var(--accent-primary-hover);
}

.btn-secondary {
    background: var(--bg-base);
    color: var(--text-primary);
    border: 1px solid var(--border);
}

.btn-secondary:hover:not(:disabled) {
    background: var(--bg-elevated);
    border-color: var(--accent-primary);
}

@media (min-width: 768px) {
    .mavlink-services {
        grid-template-columns: repeat(2, 1fr);
    }
}
"#;
