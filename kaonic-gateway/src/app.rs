use leptos::prelude::*;
use leptos_meta::{provide_meta_context, MetaTags, Title};
use leptos_router::{
    components::{Route, Router, Routes},
    StaticSegment,
};

use crate::components::navbar::Navbar;
use crate::pages::{
    dashboard::DashboardPage, mavlink::MavlinkPage, media::MediaPage, network::NetworkPage,
    radio::RadioPage, reticulum::ReticulumPage, update::SystemPage, vpn::VpnPage,
};

pub fn shell(options: leptos::config::LeptosOptions) -> impl IntoView {
    let _ = options;
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <MetaTags/>
                <link rel="stylesheet" href="/style.css"/>
            </head>
            <body>
                <App/>
            </body>
        </html>
    }
}

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

    view! {
        <Title text="Kaonic Gateway"/>
        <Router>
            <Navbar/>
            <main class="main-content">
                <Routes fallback=|| view! { <p class="not-found">"Page not found."</p> }>
                    <Route path=StaticSegment("") view=DashboardPage/>
                    <Route path=StaticSegment("radio") view=RadioPage/>
                    <Route path=StaticSegment("reticulum") view=ReticulumPage/>
                    <Route path=StaticSegment("vpn") view=VpnPage/>
                    <Route path=StaticSegment("mavlink") view=MavlinkPage/>
                    <Route path=StaticSegment("settings") view=RadioPage/>
                    <Route path=StaticSegment("network") view=NetworkPage/>
                    <Route path=StaticSegment("media") view=MediaPage/>
                    <Route path=StaticSegment("system") view=SystemPage/>
                    <Route path=StaticSegment("update") view=SystemPage/>
                </Routes>
            </main>
        </Router>
    }
}
