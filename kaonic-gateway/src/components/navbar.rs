use leptos::prelude::*;
use leptos_router::components::A;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct NavbarVersionInfo {
    version: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct TopbarInfo {
    serial: String,
    gateway_version: String,
}

async fn fetch_topbar_info() -> Result<TopbarInfo, ServerFnError> {
    use crate::state::AppState;

    let state = leptos::context::use_context::<AppState>()
        .ok_or_else(|| ServerFnError::new("missing AppState"))?;
    let gateway_version = match reqwest::Client::new()
        .get("http://127.0.0.1:8682/api/update/gateway/version")
        .send()
        .await
    {
        Ok(resp) => resp
            .json::<NavbarVersionInfo>()
            .await
            .ok()
            .and_then(|info| info.version)
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
        Err(_) => env!("CARGO_PKG_VERSION").to_string(),
    };

    Ok(TopbarInfo {
        serial: state.serial.clone(),
        gateway_version,
    })
}

#[component]
pub fn Navbar() -> impl IntoView {
    let topbar = Resource::new(|| (), |_| fetch_topbar_info());

    view! {
        <header class="topbar">
            <div class="topbar-brand">
                <img src="/kaonic-logo.svg" alt="Kaonic" class="topbar-logo-img"/>
            </div>
            <nav class="topbar-nav">
                <A href="/" exact=true attr:class="nav-link">"Dashboard"</A>
                <A href="/radio" attr:class="nav-link">"Radio"</A>
                <A href="/reticulum" attr:class="nav-link">"Reticulum"</A>
                <A href="/vpn" attr:class="nav-link">"VPN"</A>
                <A href="/mavlink" attr:class="nav-link">"MAVLink"</A>
                <A href="/network" attr:class="nav-link">"Network"</A>
                <A href="/media" attr:class="nav-link">"Media"</A>
                <A href="/system" attr:class="nav-link">"System"</A>
            </nav>
            <div class="topbar-serial">
                <Suspense fallback=|| ()>
                    {move || topbar.get().and_then(|r| r.ok()).map(|info| view! {
                        <span class="serial-label">"SN"</span>
                        <code class="serial-value">{info.serial}</code>
                        <span class="serial-label">"GW"</span>
                        <code class="serial-value">{info.gateway_version}</code>
                    })}
                </Suspense>
            </div>
        </header>
    }
}
