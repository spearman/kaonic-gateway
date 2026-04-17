use leptos::prelude::*;

pub mod dashboard;
pub mod mavlink;
pub mod media;
pub mod network;
pub mod radio;
pub mod reticulum;
pub mod update;
pub mod vpn;

#[component]
pub fn PageTitle(icon: &'static str, title: &'static str) -> impl IntoView {
    view! {
        <h1 class="page-title">
            <span class="page-title-icon" aria-hidden="true">{icon}</span>
            <span>{title}</span>
        </h1>
    }
}
