use axum::response::IntoResponse;
use maud::{html, Markup, PreEscaped, DOCTYPE};

use crate::serial;

const CSS: &str = r#"
*{box-sizing:border-box;margin:0;padding:0}
body{font-family:system-ui,sans-serif;background:#0a0c14;color:#e2e8f0;min-height:100vh}
a{color:#6ee7f7;text-decoration:none}a:hover{text-decoration:underline}
header{background:#111320;border-bottom:1px solid #1e2235;padding:.85rem 2rem;display:flex;align-items:center;gap:2rem}
header h1{font-size:1.1rem;font-weight:700;color:#6ee7f7;letter-spacing:.04em}
header nav{flex:1}
header nav a{color:#64748b;font-size:.88rem;margin-right:1.4rem;transition:color .15s}
header nav a:hover,header nav a.active{color:#e2e8f0}
.serial-badge{font-size:.78rem;color:#64748b;background:#1a1d2e;border:1px solid #2d3147;border-radius:6px;padding:.2rem .7rem;white-space:nowrap}
main{max-width:900px;margin:2.5rem auto;padding:0 1.5rem}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(200px,1fr));gap:1rem;margin-bottom:1.5rem}
.card{background:#111320;border:1px solid #1e2235;border-radius:10px;padding:1.4rem 1.6rem}
.card-title{font-size:.7rem;font-weight:700;color:#6ee7f7;text-transform:uppercase;letter-spacing:.1em;margin-bottom:.75rem}
.stat{font-size:1.9rem;font-weight:700;color:#f1f5f9;font-variant-numeric:tabular-nums}
.stat-sub{font-size:.78rem;color:#64748b;margin-top:.3rem}
.hash{font-family:monospace;font-size:.82rem;color:#94a3b8;word-break:break-all;line-height:1.6}
.bar-bg{background:#1e2235;border-radius:4px;height:8px;margin-top:.7rem;overflow:hidden}
.bar-fill{height:100%;border-radius:4px;background:linear-gradient(90deg,#6ee7f7,#6366f1);transition:width .4s}
.bridge-row{display:flex;justify-content:space-between;align-items:center;padding:.55rem 0;border-bottom:1px solid #1e2235;font-size:.88rem}
.bridge-row:last-child{border-bottom:none}
.bridge-port{color:#6ee7f7;font-weight:600;font-family:monospace}
.badge{display:inline-block;padding:.18rem .55rem;border-radius:4px;font-size:.72rem;font-weight:700;background:#1e2235;color:#94a3b8}
.dot{display:inline-block;width:8px;height:8px;border-radius:50%;background:#22c55e;margin-right:.4rem;box-shadow:0 0 6px #22c55e}
.section-title{font-size:.95rem;font-weight:700;color:#94a3b8;margin-bottom:.9rem;text-transform:uppercase;letter-spacing:.06em}
"#;

const JS: &str = r#"
function fmtFreq(hz) {
  if (!hz) return '—';
  const mhz = hz / 1e6;
  return mhz % 1 === 0 ? mhz + ' MHz' : mhz.toFixed(3) + ' MHz';
}
function fmtMod(mod) {
  if (!mod) return '—';
  if (mod === 'Off' || mod === 'Fsk') return mod;
  if (mod.Ofdm) {
    const o = mod.Ofdm;
    return `OFDM  MCS:${o.mcs}  BW:${o.opt}  TX:${o.tx_power} dBm`;
  }
  if (mod.Qpsk) {
    const q = mod.Qpsk;
    return `QPSK  Fchip:${q.fchip}  Mode:${q.mode}  TX:${q.tx_power} dBm`;
  }
  return JSON.stringify(mod);
}
function fmtOneRadio(r, idx) {
  if (!r) return '';
  const names = ['A', 'B'];
  const label = names[idx] ?? idx;
  const rc = r.radio_config;
  const freq = rc ? fmtFreq(rc.freq) : '—';
  const ch   = rc ? (rc.channel ?? '—') : '—';
  const bw   = rc ? (rc.bandwidth_filter ?? '—') : '—';
  const mod  = fmtMod(r.modulation);
  const freqHz = rc ? rc.freq : 0;
  const band = freqHz >= 2_000_000_000 ? '2.4 GHz' : 'Sub-GHz';
  const bandColor = freqHz >= 2_000_000_000 ? '#7c85f5' : '#4ade80';
  return `<div class="bridge-row" style="align-items:center;padding:.75rem 0">
    <div style="flex:1;display:flex;flex-direction:column;gap:.4rem">
      <span class="bridge-port">Module ${label}</span>
      <span style="font-size:.82rem;color:#94a3b8">${freq}  ·  ch ${ch}  ·  ${bw}  ·  ${mod}</span>
    </div>
    <span style="font-size:1.15rem;font-weight:700;color:${bandColor};letter-spacing:.01em">${band}</span>
  </div>`;
}
function fmtRadio(modules) {
  if (!modules || modules.length === 0) return '<span style="color:#64748b">No modules configured</span>';
  return modules.map(fmtOneRadio).join('');
}

async function refresh() {
  try {
    const r = await fetch('/api/status');
    const d = await r.json();

    document.getElementById('vpn-hash').textContent = d.vpn_hash || '—';

    const sys = d.system || {};
    const cpu = sys.cpu_percent ?? 0;
    const ramUsed = sys.ram_used_mb ?? 0;
    const ramTotal = sys.ram_total_mb ?? 1;
    const ramPct = ramTotal > 0 ? Math.round(ramUsed / ramTotal * 100) : 0;

    document.getElementById('cpu-val').textContent  = cpu.toFixed(1) + ' %';
    document.getElementById('cpu-bar').style.width  = Math.min(cpu, 100) + '%';
    document.getElementById('ram-val').textContent  = ramUsed + ' / ' + ramTotal + ' MB';
    document.getElementById('ram-pct').textContent  = ramPct + '%';
    document.getElementById('ram-bar').style.width  = ramPct + '%';

    const bridges = d.atak_bridges || [];
    const tbody = document.getElementById('bridges');
    tbody.innerHTML = bridges.map(b =>
      `<div class="bridge-row">
        <span>
          <span class="bridge-port">UDP:${b.port}</span>
          <span class="hash" style="font-size:.72rem;margin-left:.6rem">${b.dest_hash || '…'}</span>
        </span>
        <span><span class="badge">RX ${b.rx_packets}</span> &nbsp; <span class="badge">TX ${b.tx_packets}</span></span>
      </div>`
    ).join('') || '<div class="bridge-row" style="color:#64748b">No bridges active</div>';

    document.getElementById('radio-info').innerHTML = fmtRadio(d.radio_modules);
  } catch(e) { /* ignore */ }
}
refresh();
setInterval(refresh, 2000);
"#;

fn layout(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width,initial-scale=1";
                title { (title) " — Kaonic Gateway" }
                style { (PreEscaped(CSS)) }
            }
            body {
                header {
                    h1 { "⚡ Kaonic Gateway" }
                    nav {
                        a href="/" class="active" { "Dashboard" }
                        a href="/settings" { "Settings" }
                        a href="/update" { "Update" }
                        a href="/mavlink" { "MAVLink" }
                    }
                    span .serial-badge { "S/N: " (serial()) }
                }
                main { (body) }
            }
        }
    }
}

/// `GET /` — dashboard home page.
pub async fn get_dashboard() -> impl IntoResponse {
    let content = html! {
        div style="display:flex;align-items:center;gap:.6rem;margin-bottom:1.8rem" {
            span .dot {}
            span style="color:#94a3b8;font-size:.9rem" { "Live — refreshing every 2 s" }
        }

        // VPN identity
        div .card style="margin-bottom:1rem" {
            p .card-title { "VPN Identity Hash" }
            p .hash id="vpn-hash" { "loading…" }
        }

        // CPU + RAM
        p .section-title style="margin-top:1.5rem" { "System" }
        div .grid {
            div .card {
                p .card-title { "CPU Usage" }
                p .stat id="cpu-val" { "…" }
                div .bar-bg { div .bar-fill id="cpu-bar" style="width:0%" {} }
            }
            div .card {
                p .card-title { "Memory" }
                p .stat id="ram-val" { "…" }
                p .stat-sub id="ram-pct" {}
                div .bar-bg { div .bar-fill id="ram-bar" style="width:0%" {} }
            }
        }

        // ATAK bridges
        p .section-title style="margin-top:1.5rem" { "ATAK Bridges" }
        div .card {
            div id="bridges" { "loading…" }
        }

        // Radio config
        p .section-title style="margin-top:1.5rem" { "Radio" }
        div .card {
            div id="radio-info" { "loading…" }
        }

        script { (PreEscaped(JS)) }
    };
    layout("Dashboard", content)
}

