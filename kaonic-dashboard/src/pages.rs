use axum::response::IntoResponse;
use maud::{html, Markup, PreEscaped, DOCTYPE};

use crate::serial;

const CSS: &str = r#"
*{box-sizing:border-box;margin:0;padding:0}
body{font-family:system-ui,sans-serif;background:#0f1117;color:#e2e8f0;min-height:100vh}
a{color:#7c85f5;text-decoration:none}a:hover{text-decoration:underline}
header{background:#1a1d27;border-bottom:1px solid #2d3147;padding:.85rem 2rem;display:flex;align-items:center;gap:2rem}
header h1{font-size:1.1rem;font-weight:700;color:#7c85f5;letter-spacing:.02em}
header nav{flex:1}
header nav a{color:#94a3b8;font-size:.88rem;margin-right:1.2rem}
header nav a:hover{color:#e2e8f0}
.serial-badge{font-size:.78rem;color:#64748b;background:#111320;border:1px solid #2d3147;border-radius:6px;padding:.2rem .7rem;white-space:nowrap}
main{max-width:740px;margin:2rem auto;padding:0 1.25rem}
.card{background:#1a1d27;border:1px solid #2d3147;border-radius:8px;padding:1.5rem;margin-bottom:1.5rem}
.card-title{font-size:.78rem;font-weight:700;color:#7c85f5;text-transform:uppercase;letter-spacing:.08em;margin-bottom:1.1rem}
.field{margin-bottom:1rem}
.field label{display:block;font-size:.83rem;color:#94a3b8;margin-bottom:.3rem;font-weight:500}
.field input,.field textarea,.field select{width:100%;background:#0f1117;border:1px solid #2d3147;border-radius:4px;padding:.45rem .7rem;color:#e2e8f0;font-size:.9rem;font-family:inherit}
.field input:focus,.field textarea:focus,.field select:focus{outline:none;border-color:#7c85f5;box-shadow:0 0 0 2px #7c85f520}
.field textarea{resize:vertical;min-height:90px;line-height:1.5;font-family:monospace}
.field small{display:block;color:#64748b;font-size:.76rem;margin-top:.25rem}
.row{display:grid;grid-template-columns:1fr 1fr;gap:1rem}
.actions{display:flex;justify-content:flex-end;gap:.75rem;margin-top:.5rem}
.btn{padding:.45rem 1.2rem;border-radius:4px;border:none;cursor:pointer;font-size:.9rem;font-weight:600}
.btn-primary{background:#7c85f5;color:#fff}.btn-primary:hover{background:#6470f0}
.btn-sm{padding:.3rem .85rem;font-size:.82rem}
.btn-outline{background:transparent;border:1px solid #3d4262;color:#94a3b8}.btn-outline:hover{border-color:#7c85f5;color:#e2e8f0}
.flash{border-radius:6px;padding:.7rem 1rem;margin-bottom:1.5rem;font-size:.88rem;display:none}
.flash.show{display:block}
.flash-ok{background:#14532d;border:1px solid #16a34a;color:#86efac}
.flash-err{background:#450a0a;border:1px solid #dc2626;color:#fca5a5}
.flash-info{background:#1e2235;border:1px solid #3d4262;color:#94a3b8}
.hint{font-size:.78rem;color:#64748b;margin-top:-.5rem;margin-bottom:.75rem}
.mod-section{display:none}.mod-section.active{display:block}
.slider-row{display:flex;align-items:center;gap:.75rem}
.slider-row input[type=range]{flex:1;cursor:pointer}
.tx-label{font-size:.88rem;font-weight:700;color:#94a3b8;min-width:58px;text-align:right;transition:color .2s}
.tx-label.hot{color:#f97316}
select option{background:#1a1d27}
/* band toggle */
.band-toggle{display:flex;border:1px solid #2d3147;border-radius:6px;overflow:hidden;margin-bottom:1.1rem}
.band-btn{flex:1;padding:.38rem .6rem;background:#0f1117;color:#64748b;border:none;cursor:pointer;font-size:.84rem;font-weight:500;transition:background .15s,color .15s}
.band-btn.active{background:#7c85f5;color:#fff}
.band-btn:not(.active):hover{background:#1e2235;color:#e2e8f0}
/* module rows */
.module-row{display:flex;align-items:center;justify-content:space-between;padding:.75rem 0;border-bottom:1px solid #2d3147}
.module-row:last-child{border-bottom:none}
.module-label{font-weight:600;font-size:.95rem}
.module-sub{font-size:.78rem;color:#64748b;margin-top:.2rem}
.module-sub.ok{color:#4ade80}
/* modal */
.modal-backdrop{display:none;position:fixed;inset:0;background:#00000099;z-index:100;align-items:center;justify-content:center}
.modal-backdrop.open{display:flex}
.modal{background:#1a1d27;border:1px solid #2d3147;border-radius:10px;padding:1.75rem;width:100%;max-width:560px;max-height:90vh;overflow-y:auto;position:relative}
.modal-title{font-size:1rem;font-weight:700;color:#e2e8f0;margin-bottom:1.5rem}
.modal-close{position:absolute;top:1rem;right:1rem;background:none;border:none;color:#94a3b8;font-size:1.2rem;cursor:pointer;line-height:1}
.modal-close:hover{color:#e2e8f0}
.modal-flash{border-radius:4px;padding:.5rem .8rem;margin-bottom:1rem;font-size:.85rem;display:none}
.modal-flash.show{display:block}
"#;

const JS: &str = r#"
var OFDM_MCS  = ['BpskC1_2_4x','BpskC1_2_2x','QpskC1_2_2x','QpskC1_2','QpskC3_4','QamC1_2','QamC3_4'];
var OFDM_OPT  = ['Option1','Option2','Option3','Option4'];
var QPSK_FCHIP = ['Fchip100','Fchip200','Fchip1000','Fchip2000'];
var QPSK_MODE  = ['RateMode0','RateMode1','RateMode2','RateMode3','RateMode4'];
var MODULE_NAMES = ['A', 'B'];
var currentModule = 0;
var currentBand = 'subghz';

function v(id) { return document.getElementById(id); }

// ── band toggle ──────────────────────────────────────────────────────────────
function setBand(band, userInitiated) {
  currentBand = band;
  document.querySelectorAll('.band-btn').forEach(function(b) {
    b.classList.toggle('active', b.dataset.band === band);
  });
  var fi = v('radio_freq_khz');
  if (band === 'subghz') {
    fi.min = 100000; fi.max = 1100000; fi.placeholder = 'e.g. 869535';
    if (userInitiated) fi.value = 869535;
  } else {
    fi.min = 2400000; fi.max = 2500000; fi.placeholder = 'e.g. 2450000';
    if (userInitiated) fi.value = 2450000;
  }
}

// ── modulation type ──────────────────────────────────────────────────────────
function updateMod() {
  var t = v('modulation_type').value;
  document.querySelectorAll('.mod-section').forEach(function(el) {
    el.classList.toggle('active', el.dataset.mod === t);
  });
}

// ── tx power label ───────────────────────────────────────────────────────────
function updateTxLabel(prefix) {
  var val = parseInt(v(prefix + '_tx_power_range').value);
  var label = v(prefix + '_tx_label');
  label.textContent = val + ' dBm';
  label.className = 'tx-label' + (val > 20 ? ' hot' : '');
}

// ── flash helpers ────────────────────────────────────────────────────────────
function showFlash(ok, msg) {
  var el = v('flash');
  el.className = 'flash show ' + (ok ? 'flash-ok' : 'flash-err');
  el.textContent = ok ? ('✓ ' + msg) : ('✗ ' + msg);
}
function showModalFlash(ok, msg) {
  var el = v('modal-flash');
  el.className = 'modal-flash show ' + (ok ? 'flash-ok' : 'flash-err');
  el.textContent = ok ? ('✓ ' + msg) : ('✗ ' + msg);
}

// ── module summary rows ──────────────────────────────────────────────────────
function fmtFreqHz(hz) {
  if (!hz) return null;
  var mhz = hz / 1e6;
  return (mhz % 1 === 0 ? mhz : mhz.toFixed(3)) + ' MHz';
}
function moduleSummary(radio) {
  if (!radio) return 'Not configured';
  var parts = [];
  if (radio.radio_config) {
    var f = fmtFreqHz(radio.radio_config.freq);
    if (f) parts.push(f);
    if (radio.radio_config.channel != null) parts.push('ch ' + radio.radio_config.channel);
    if (radio.radio_config.bandwidth_filter) parts.push(radio.radio_config.bandwidth_filter);
  }
  if (radio.modulation) {
    if (radio.modulation === 'Off') parts.push('Off');
    else if (radio.modulation === 'Fsk') parts.push('FSK');
    else if (radio.modulation.Ofdm) parts.push('OFDM mcs:' + radio.modulation.Ofdm.mcs);
    else if (radio.modulation.Qpsk) parts.push('QPSK');
  }
  return parts.join('  ·  ') || 'Configured';
}

var moduleData = [null, null];

// Default values shown when a module has no saved config yet.
var MODULE_DEFAULTS = [
  // Module 0 — Sub-GHz
  { radio_config: { freq: 869535000, channel_spacing: 200000, channel: 10, bandwidth_filter: 'Narrow' },
    modulation: { Ofdm: { mcs: 'BpskC1_2_4x', opt: 'Option1', pdt: 3, tx_power: 14 } } },
  // Module 1 — 2.4 GHz
  { radio_config: { freq: 2450000000, channel_spacing: 200000, channel: 0, bandwidth_filter: 'Narrow' },
    modulation: { Ofdm: { mcs: 'BpskC1_2_4x', opt: 'Option1', pdt: 3, tx_power: 14 } } }
];

function loadAllModules(cfg) {
  var configs = (cfg && cfg.radio && cfg.radio.module_configs) || [];
  moduleData = [null, null];
  configs.forEach(function(m, idx) { if (idx < 2) moduleData[idx] = m; });
  updateModuleRows();
}
function updateModuleRows() {
  for (var i = 0; i < 2; i++) {
    var sub = v('module-sub-' + i);
    if (!sub) continue;
    sub.textContent = moduleSummary(moduleData[i]);
    sub.className = 'module-sub' + (moduleData[i] ? ' ok' : '');
  }
}

// ── modal ────────────────────────────────────────────────────────────────────
function openModal(idx) {
  currentModule = idx;
  v('modal-title').textContent = 'RF215 Module ' + MODULE_NAMES[idx] + ' Configuration';
  v('modal-flash').className = 'modal-flash';
  populateModalFields(moduleData[idx] || MODULE_DEFAULTS[idx]);
  v('modal-backdrop').classList.add('open');
}
function closeModal() {
  v('modal-backdrop').classList.remove('open');
}

function populateModalFields(radio) {
  var rf = radio && radio.radio_config;
  var freq_hz = rf ? rf.freq : 0;
  var freq_khz = freq_hz ? Math.round(freq_hz / 1000) : '';

  // detect band from frequency
  var band = (freq_hz && freq_hz >= 2000000000) ? '2400' : 'subghz';
  setBand(band);

  v('radio_freq_khz').value            = freq_khz;
  v('radio_channel_spacing_khz').value = rf ? Math.round(rf.channel_spacing / 1000) : '';
  v('radio_channel').value             = rf ? rf.channel : 0;
  v('radio_bandwidth_filter').value    = (rf && rf.bandwidth_filter === 'Wide') ? 'wide' : 'narrow';

  var mod = radio && radio.modulation;
  if (!mod || mod === 'Off') {
    v('modulation_type').value = 'off';
  } else if (mod === 'Fsk') {
    v('modulation_type').value = 'fsk';
  } else if (mod.Ofdm) {
    v('modulation_type').value = 'ofdm';
    var o = mod.Ofdm;
    v('ofdm_mcs').value             = OFDM_MCS.indexOf(o.mcs);
    v('ofdm_opt').value             = OFDM_OPT.indexOf(o.opt);
    v('ofdm_pdt').value             = o.pdt;
    v('ofdm_tx_power_range').value  = o.tx_power;
    updateTxLabel('ofdm');
  } else if (mod.Qpsk) {
    v('modulation_type').value = 'qpsk';
    var q = mod.Qpsk;
    v('qpsk_fchip').value           = QPSK_FCHIP.indexOf(q.fchip);
    v('qpsk_mode').value            = QPSK_MODE.indexOf(q.mode);
    v('qpsk_tx_power_range').value  = q.tx_power;
    updateTxLabel('qpsk');
  }
  updateMod();
}

function buildModuleConfig() {
  var existing = moduleData[currentModule] || {};
  var freq_khz = parseInt(v('radio_freq_khz').value) || 0;
  var ch_khz   = parseInt(v('radio_channel_spacing_khz').value) || 0;
  var channel  = parseInt(v('radio_channel').value) || 0;
  var bw       = v('radio_bandwidth_filter').value === 'wide' ? 'Wide' : 'Narrow';

  var default_rc = (MODULE_DEFAULTS[currentModule] || {}).radio_config;
  var existing_rc = (existing && existing.radio_config) || default_rc;
  var radio_config = freq_khz > 0 ? {
    freq: freq_khz * 1000,
    channel_spacing: ch_khz * 1000,
    channel: channel,
    bandwidth_filter: bw
  } : existing_rc;

  var mod_type = v('modulation_type').value;
  var modulation = 'Off';
  if (mod_type === 'fsk') {
    modulation = 'Fsk';
  } else if (mod_type === 'ofdm') {
    modulation = { Ofdm: {
      mcs: OFDM_MCS[parseInt(v('ofdm_mcs').value)],
      opt: OFDM_OPT[parseInt(v('ofdm_opt').value)],
      pdt: parseInt(v('ofdm_pdt').value) || 0,
      tx_power: parseInt(v('ofdm_tx_power_range').value) || 0
    }};
  } else if (mod_type === 'qpsk') {
    modulation = { Qpsk: {
      fchip: QPSK_FCHIP[parseInt(v('qpsk_fchip').value)],
      mode:  QPSK_MODE[parseInt(v('qpsk_mode').value)],
      tx_power: parseInt(v('qpsk_tx_power_range').value) || 0
    }};
  }

  return {
    radio_config: radio_config,
    modulation: modulation
  };
}

function saveModule(e) {
  e.preventDefault();
  var cfg = buildModuleConfig();
  fetch('/api/settings/radio/' + currentModule, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(cfg)
  }).then(function(r) {
    if (r.ok) {
      moduleData[currentModule] = cfg;
      updateModuleRows();
      showModalFlash(true, 'Module ' + MODULE_NAMES[currentModule] + ' saved.');
    } else {
      showModalFlash(false, 'Save failed (HTTP ' + r.status + ').');
    }
  }).catch(function(err) { showModalFlash(false, 'Error: ' + err.message); });
}

// ── VPN + peer settings ──────────────────────────────────────────────────────
function loadSettings() {
  fetch('/api/settings')
    .then(function(r) { return r.ok ? r.json() : null; })
    .then(function(cfg) {
      if (!cfg) return;
      v('network').value = cfg.network || '';
      v('announce_freq_secs').value = cfg.announce_freq_secs || 1;
      v('peers').value = (cfg.peers || []).join('\n');
      loadAllModules(cfg);
    })
    .catch(function() {});
}

function saveSettings(e) {
  e.preventDefault();
  fetch('/api/settings')
    .then(function(r) { return r.ok ? r.json() : {}; })
    .then(function(existing) {
      return fetch('/api/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          network: v('network').value.trim(),
          announce_freq_secs: parseInt(v('announce_freq_secs').value) || 1,
          peers: v('peers').value.split('\n').map(function(s){return s.trim();}).filter(Boolean),
          radio: { module_configs: [
            moduleData[0] || MODULE_DEFAULTS[0],
            moduleData[1] || MODULE_DEFAULTS[1]
          ]}
        })
      });
    })
    .then(function(r) {
      if (r.ok) showFlash(true, 'Settings saved.');
      else showFlash(false, 'Failed (HTTP ' + r.status + ').');
    })
    .catch(function(err) { showFlash(false, 'Error: ' + err.message); });
}

document.addEventListener('DOMContentLoaded', function() {
  loadSettings();
  v('settings-form').addEventListener('submit', saveSettings);
  v('module-form').addEventListener('submit', saveModule);
  setBand('subghz');
  updateTxLabel('ofdm');
  updateTxLabel('qpsk');
  updateMod();
  v('modal-backdrop').addEventListener('click', function(e) {
    if (e.target === this) closeModal();
  });
});
"#;


fn layout(body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width,initial-scale=1";
                title { "Settings — Kaonic Gateway" }
                style { (PreEscaped(CSS)) }
            }
            body {
                header {
                    h1 { "⚡ Kaonic Gateway" }
                    nav {
                        a href="/" { "Dashboard" }
                        a href="/settings" { "Settings" }
                        a href="/update" { "Update" }
                        a href="/mavlink" { "MAVLink" }
                    }
                    span .serial-badge { "S/N: " (serial()) }
                }
                main { (body) }
                script { (PreEscaped(JS)) }
            }
        }
    }
}

/// `GET /settings` — serve the settings page (data loaded client-side via `/api/settings`).
pub async fn get_settings() -> impl IntoResponse {
    let content = html! {
        h2 style="font-size:1.25rem;font-weight:700;margin-bottom:1.5rem" { "Configuration" }

        div #"flash" .flash {}

        form #"settings-form" {

            // ── VPN ─────────────────────────────────────────────────────────
            div .card {
                p .card-title { "VPN" }
                div .field {
                    label for="network" { "Network (CIDR)" }
                    input type="text" id="network" name="network" placeholder="10.20.0.0/16" required;
                    small { "IPv4 subnet shared by all VPN peers." }
                }
                div .field {
                    label for="announce_freq_secs" { "Announce interval (s)" }
                    input type="number" id="announce_freq_secs" name="announce_freq_secs" min="1" value="1";
                }
                div .field {
                    label for="peers" { "Peers" }
                    textarea id="peers" name="peers" placeholder="fb08aff16ec6f5ccf0d3eb179028e9c3\n..." {}
                    small { "One Reticulum destination hash per line." }
                }
            }

            // ── RF215 Modules ────────────────────────────────────────────────
            div .card {
                p .card-title { "RF215 Modules" }
                div .module-row {
                    div {
                        div .module-label { "Module A" }
                        div .module-sub id="module-sub-0" { "Not configured" }
                    }
                    button type="button" .btn.btn-outline.btn-sm onclick="openModal(0)" { "Configure" }
                }
                div .module-row {
                    div {
                        div .module-label { "Module B" }
                        div .module-sub id="module-sub-1" { "Not configured" }
                    }
                    button type="button" .btn.btn-outline.btn-sm onclick="openModal(1)" { "Configure" }
                }
            }

            div .actions {
                button .btn.btn-primary type="submit" { "Save VPN settings" }
            }
        }

        // ── Module config modal ──────────────────────────────────────────────
        div #"modal-backdrop" .modal-backdrop {
            div .modal {
                button .modal-close type="button" onclick="closeModal()" { "×" }
                p .modal-title id="modal-title" { "Module Configuration" }
                div #"modal-flash" .modal-flash {}

                form #"module-form" {

                    // ── Band toggle ──────────────────────────────────────────
                    div .band-toggle {
                        button type="button" .band-btn data-band="subghz" onclick="setBand('subghz',true)" { "Sub-GHz" }
                        button type="button" .band-btn data-band="2400"   onclick="setBand('2400',true)"   { "2.4 GHz" }
                    }

                    p .card-title style="margin-bottom:.9rem" { "RF Configuration" }
                    div .row {
                        div .field {
                            label for="radio_freq_khz" { "Frequency (kHz)" }
                            input type="number" id="radio_freq_khz" min="100000" max="1100000" placeholder="e.g. 869000";
                        }
                        div .field {
                            label for="radio_channel_spacing_khz" { "Channel spacing (kHz)" }
                            input type="number" id="radio_channel_spacing_khz" min="0" placeholder="200";
                        }
                    }
                    div .row {
                        div .field {
                            label for="radio_channel" { "Channel" }
                            input type="number" id="radio_channel" min="0" value="0";
                        }
                        div .field {
                            label for="radio_bandwidth_filter" { "Bandwidth filter" }
                            select id="radio_bandwidth_filter" {
                                option value="narrow" { "Narrow" }
                                option value="wide"   { "Wide" }
                            }
                        }
                    }

                    p .card-title style="margin-top:1.2rem;margin-bottom:.9rem" { "Modulation" }
                    div .field {
                        label for="modulation_type" { "Type" }
                        select id="modulation_type" onchange="updateMod()" {
                            option value="off"  { "Off" }
                            option value="ofdm" { "OFDM" }
                            option value="qpsk" { "QPSK" }
                            option value="fsk"  { "FSK" }
                        }
                    }

                    // OFDM
                    div .mod-section data-mod="ofdm" {
                        div .row {
                            div .field {
                                label for="ofdm_mcs" { "MCS" }
                                select id="ofdm_mcs" {
                                    option value="0" { "BPSK 1/2 4x (slowest)" }
                                    option value="1" { "BPSK 1/2 2x" }
                                    option value="2" { "QPSK 1/2 2x" }
                                    option value="3" { "QPSK 1/2" }
                                    option value="4" { "QPSK 3/4" }
                                    option value="5" { "16-QAM 1/2" }
                                    option value="6" { "16-QAM 3/4 (fastest)" }
                                }
                            }
                            div .field {
                                label for="ofdm_opt" { "Bandwidth option" }
                                select id="ofdm_opt" {
                                    option value="0" { "Option 1" }
                                    option value="1" { "Option 2" }
                                    option value="2" { "Option 3" }
                                    option value="3" { "Option 4" }
                                }
                            }
                        }
                        div .row {
                            div .field {
                                label for="ofdm_pdt" { "Preamble detection threshold" }
                                input type="number" id="ofdm_pdt" min="0" max="255" value="3";
                            }
                            div .field {
                                label { "TX power" }
                                div .slider-row {
                                    input type="range" id="ofdm_tx_power_range" min="0" max="30" value="10"
                                        style="accent-color:#7c85f5"
                                        oninput="updateTxLabel('ofdm')";
                                    span .tx-label id="ofdm_tx_label" { "10 dBm" }
                                }
                            }
                        }
                    }

                    // QPSK
                    div .mod-section data-mod="qpsk" {
                        div .row {
                            div .field {
                                label for="qpsk_fchip" { "Chip frequency" }
                                select id="qpsk_fchip" {
                                    option value="0" { "100 kchip/s" }
                                    option value="1" { "200 kchip/s" }
                                    option value="2" { "1000 kchip/s" }
                                    option value="3" { "2000 kchip/s" }
                                }
                            }
                            div .field {
                                label for="qpsk_mode" { "Rate mode" }
                                select id="qpsk_mode" {
                                    option value="0" { "Mode 0" }
                                    option value="1" { "Mode 1" }
                                    option value="2" { "Mode 2" }
                                    option value="3" { "Mode 3" }
                                    option value="4" { "Mode 4" }
                                }
                            }
                        }
                        div .field {
                            label { "TX power" }
                            div .slider-row {
                                input type="range" id="qpsk_tx_power_range" min="0" max="30" value="10"
                                    style="accent-color:#7c85f5"
                                    oninput="updateTxLabel('qpsk')";
                                span .tx-label id="qpsk_tx_label" { "10 dBm" }
                            }
                        }
                    }

                    div .actions style="margin-top:1.2rem" {
                        button type="button" .btn.btn-outline onclick="closeModal()" { "Cancel" }
                        button type="submit" .btn.btn-primary { "Save module" }
                    }
                }
            }
        }
    };

    layout(content)
}

const UPDATE_JS: &str = r#"
const UPDATE_API = 'http://' + location.hostname + ':8682';

async function loadVersions() {
    for (const target of ['commd', 'gateway']) {
        const el = document.getElementById(target + '_version');
        if (!el) continue;
        try {
            const r = await fetch(UPDATE_API + '/api/update/' + target + '/version');
            if (!r.ok) { el.textContent = 'unavailable (HTTP ' + r.status + ')'; continue; }
            const d = await r.json();
            const hashStr = d.hash ? ' (' + d.hash.slice(0,12) + '...)' : '';
            el.textContent = d.version ? d.version + hashStr : 'not installed';
        } catch(e) {
            el.textContent = 'unavailable';
        }
    }
}

async function doUpload(target) {
    const input = document.getElementById(target + '_file');
    const status = document.getElementById(target + '_status');
    if (!input.files.length) {
        showStatus(status, 'No file selected.', false);
        return;
    }
    const file = input.files[0];
    if (!file.name.endsWith('.zip')) {
        showStatus(status, 'Only .zip files are accepted.', false);
        return;
    }
    const btn = document.getElementById(target + '_btn');
    btn.disabled = true;
    showStatus(status, 'Uploading…', null);

    const form = new FormData();
    form.append('file', file);
    try {
        const r = await fetch(UPDATE_API + '/api/update/' + target + '/upload', {
            method: 'POST',
            body: form,
        });
        const d = await r.json();
        showStatus(status, d.detail || (r.ok ? 'Success' : 'Error'), r.ok);
        if (r.ok) { input.value = ''; loadVersions(); }
    } catch(e) {
        showStatus(status, 'Request failed: ' + e.message, false);
    } finally {
        btn.disabled = false;
    }
}

function showStatus(el, msg, ok) {
    el.textContent = msg;
    el.className = 'flash show ' + (ok === null ? 'flash-info' : (ok ? 'flash-ok' : 'flash-err'));
}

window.addEventListener('DOMContentLoaded', loadVersions);
"#;

/// `GET /update` — OTA update page.
pub async fn get_update() -> impl IntoResponse {
    let content = html! {
        h2 style="font-size:1.25rem;font-weight:700;margin-bottom:1.5rem" { "Software Update" }

        // kaonic-commd card
        div .card {
            div .card-title { "kaonic-commd" }
            p style="font-size:.85rem;color:#94a3b8;margin-bottom:1rem" {
                "Radio control daemon. Installed: "
                span id="commd_version" style="color:#e2e8f0" { "loading…" }
            }
            div .field {
                label { "Update package (.zip)" }
                input type="file" id="commd_file" accept=".zip"
                    style="background:#0f1117;border:1px solid #2d3147;border-radius:4px;padding:.45rem .7rem;color:#e2e8f0;width:100%";
                small { "ZIP must contain: kaonic-commd, kaonic-commd.sha256, kaonic-commd.version, kaonic-commd.sig" }
            }
            div .actions {
                button id="commd_btn" .btn.btn-primary type="button"
                    onclick="doUpload('commd')" { "Upload & Install" }
            }
            div id="commd_status" .flash {}
        }

        // kaonic-gateway card
        div .card {
            div .card-title { "kaonic-gateway" }
            p style="font-size:.85rem;color:#94a3b8;margin-bottom:1rem" {
                "Gateway service. Installed: "
                span id="gateway_version" style="color:#e2e8f0" { "loading…" }
            }
            div .field {
                label { "Update package (.zip)" }
                input type="file" id="gateway_file" accept=".zip"
                    style="background:#0f1117;border:1px solid #2d3147;border-radius:4px;padding:.45rem .7rem;color:#e2e8f0;width:100%";
                small { "ZIP must contain: kaonic-gateway, kaonic-gateway.sha256, kaonic-gateway.version, kaonic-gateway.sig" }
            }
            div .hint style="color:#f97316;font-size:.82rem;margin-bottom:.75rem" {
                "⚠ The gateway service will restart after update. Dashboard will be briefly unavailable."
            }
            div .actions {
                button id="gateway_btn" .btn.btn-primary type="button"
                    onclick="doUpload('gateway')" { "Upload & Install" }
            }
            div id="gateway_status" .flash {}
        }

        script { (PreEscaped(UPDATE_JS)) }
    };

    layout(content)
}

/// `GET /mavlink` — serve the mavlink page
pub async fn get_mavlink() -> impl IntoResponse {
    let content = html! {
        h2 style="font-size:1.25rem;font-weight:700;margin-bottom:1.5rem" { "MAVLink" }
    };

    layout(content)
}
