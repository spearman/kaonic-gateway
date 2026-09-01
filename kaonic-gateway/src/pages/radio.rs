use leptos::prelude::*;

use super::PageTitle;
use crate::app_types::{GatewaySettingsDto, RadioModuleConfigDto};

// ── Server functions ──────────────────────────────────────────────────────────

#[server]
pub async fn load_settings() -> Result<GatewaySettingsDto, ServerFnError> {
    use crate::state::AppState;

    let state = leptos::context::use_context::<AppState>()
        .ok_or_else(|| ServerFnError::new("missing AppState context"))?;

    let config = state
        .settings
        .lock()
        .map_err(|_| ServerFnError::new("settings lock poisoned"))?
        .load_config()
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(GatewaySettingsDto::from(config))
}

#[server]
pub async fn save_settings(settings: GatewaySettingsDto) -> Result<(), ServerFnError> {
    use crate::config::GatewayConfig;
    use crate::state::AppState;

    let state = leptos::context::use_context::<AppState>()
        .ok_or_else(|| ServerFnError::new("missing AppState context"))?;

    let config = GatewayConfig::try_from(settings).map_err(|e| ServerFnError::new(e))?;

    state
        .settings
        .lock()
        .map_err(|_| ServerFnError::new("settings lock poisoned"))?
        .save_config(&config)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(())
}

#[server]
pub async fn save_radio_module(
    module: usize,
    cfg: RadioModuleConfigDto,
) -> Result<(), ServerFnError> {
    use crate::radio::RadioModuleConfig;
    use crate::state::AppState;

    let state = leptos::context::use_context::<AppState>()
        .ok_or_else(|| ServerFnError::new("missing AppState context"))?;

    let radio_cfg = RadioModuleConfig::from(cfg);
    let band = radio_cfg.band();

    state
        .settings
        .lock()
        .map_err(|_| ServerFnError::new("settings lock poisoned"))?
        .save_module_config(module, &radio_cfg)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let Some(client) = state.radio_client.clone() else {
        return Ok(());
    };

    let mut client = client.lock().await;

    client
        .set_radio_config(module, radio_cfg.radio_config)
        .await
        .map_err(|e| ServerFnError::new(format!("set_radio_config: {e:?}")))?;

    client
        .set_modulation(module, radio_cfg.modulation)
        .await
        .map_err(|e| ServerFnError::new(format!("set_modulation: {e:?}")))?;

    client
        .set_accelerator(module, radio_cfg.accelerator)
        .await
        .map_err(|e| ServerFnError::new(format!("set_accelerator: {e:?}")))?;

    // Sub-GHz has no antenna switch on this board — it is always external.
    if band == radio_common::RadioBand::Band24 {
        client
            .set_antenna(module, radio_common::RadioBand::Band24, radio_cfg.antenna)
            .await
            .map_err(|e| ServerFnError::new(format!("set_antenna: {e:?}")))?;
    }

    Ok(())
}

// ── Page component ────────────────────────────────────────────────────────────

const RADIO_WS_JS: &str = r#"
(function() {
    function shouldPauseLiveUpdates() {
        if (document.body.classList.contains('modal-open')) { return true; }
        var active = document.activeElement;
        if (active && (
            active.tagName === 'INPUT' ||
            active.tagName === 'TEXTAREA' ||
            active.tagName === 'SELECT' ||
            active.isContentEditable
        )) {
            return true;
        }
        var selection = window.getSelection ? window.getSelection() : null;
        return !!(selection && !selection.isCollapsed && String(selection).trim().length > 0);
    }
    function formatKBytes(bytes) {
        return (bytes / 1024).toFixed(1);
    }
    function formatRate(bytesPerSecond) {
        return (bytesPerSecond / 1024).toFixed(1);
    }
    function formatFrameTime(ts) {
        return new Date(ts * 1000).toLocaleTimeString([], {
            hour: '2-digit',
            minute: '2-digit',
            second: '2-digit'
        });
    }
    function rssiClass(frame) {
        if (frame.direction === 'tx') {
            return 'td-rssi';
        }
        var value = Number(frame.rssi);
        if (!Number.isFinite(value)) {
            return 'td-rssi';
        }
        if (value <= -100) {
            return 'td-rssi td-rssi-bad';
        }
        if (value <= -80) {
            return 'td-rssi td-rssi-warn';
        }
        return 'td-rssi td-rssi-good';
    }
    function connect() {
        var proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
        var ws = new WebSocket(proto + '//' + location.host + '/api/ws/status');
        ws.onmessage = function(ev) {
            try {
                if (shouldPauseLiveUpdates()) { return; }
                var msg = JSON.parse(ev.data) || {};
                if (msg.type !== 'radio_frames') { return; }
                var d = msg.data || {};
                var i = Number(d.module) || 0;
                var tbody = document.getElementById('rx-frames-' + i);
                var stats = document.getElementById('rx-stats-summary-' + i);
                var rssi = document.getElementById('rx-stats-rssi-' + i);
                if (!tbody || !stats || !rssi) return;
                var list = d.frames || [];
                var totals = d.stats || {};
                var rxBps = Number(totals.rx_bps) || 0;
                var txBps = Number(totals.tx_bps) || 0;
                stats.textContent = 'RX: ' + formatRate(rxBps) + ' KB/s | TX: '
                    + formatRate(txBps) + ' KB/s | Total: '
                    + formatRate(rxBps + txBps) + ' KB/s';
                    rssi.textContent = totals.last_rssi !== null && totals.last_rssi !== undefined
                        ? 'Last RSSI: ' + totals.last_rssi + ' dBm'
                        : 'Last RSSI: —';
                if (list.length === 0) {
                    tbody.innerHTML = '<tr><td colspan="7" class="frames-empty">No frames observed</td></tr>';
                    return;
                }
                tbody.innerHTML = list.map(function(f) {
                    var t = formatFrameTime(f.ts);
                    var dir = f.direction === 'tx' ? '\u2191' : '\u2193';
                    var trxClass = f.direction === 'tx' ? 'td-trx td-trx-tx' : 'td-trx td-trx-rx';
                    return '<tr><td class="' + trxClass + '">' + dir + '</td>'
                         + '<td class="td-time">' + t + '</td>'
                            + '<td class="' + rssiClass(f) + '">' + f.rssi + ' dBm</td>'
                            + '<td class="td-len">' + f.len + ' B</td>'
                            + '<td class="td-hex td-preview">' + (f.hex || '—') + '</td>'
                            + '<td class="td-hex td-preview">' + (f.ascii || '—') + '</td>'
                            + '<td class="td-hex td-preview">' + (f.crc32 || '—') + '</td></tr>';
                }).join('');
            } catch(e) {}
        };
        ws.onclose = function() {
            setTimeout(connect, 3000);
        };
        ws.onerror = function() {
            ws.close();
        };
    }
    connect();
})();
"#;

#[component]
pub fn RadioPage() -> impl IntoView {
    let settings_res = Resource::new(|| (), |_| load_settings());

    view! {
        <div class="page page--fill">
            <PageTitle icon="📡" title="Radio" />
            <Suspense fallback=|| view! { <p class="loading">"Loading…"</p> }>
                {move || match settings_res.get() {
                    None => view! { <p class="loading">"Loading…"</p> }.into_any(),
                    Some(Err(e)) => view! {
                        <div class="error-banner">"Error: "{e.to_string()}</div>
                    }.into_any(),
                    Some(Ok(s)) => view! { <RadioSplitView settings=s/> }.into_any(),
                }}
            </Suspense>
            <script>{RADIO_WS_JS}</script>
        </div>
    }
}

#[component]
fn RadioSplitView(settings: GatewaySettingsDto) -> impl IntoView {
    let mod_a = settings.radio_modules[0].clone();
    let mod_b = settings.radio_modules[1].clone();
    view! {
        <div class="radio-split">
            <RadioPanel index=0 module=mod_a />
            <RadioPanel index=1 module=mod_b />
        </div>
    }
    .into_any()
}

#[component]
fn RadioPanel(index: usize, module: RadioModuleConfigDto) -> impl IntoView {
    view! {
        <div class="radio-panel">
            <div class="frames-section">
                <div class="frames-table-wrap">
                    <table class="frames-table">
                        <thead>
                            <tr>
                                <th>"TRX"</th>
                                <th>"Time"</th>
                                <th>"RSSI"</th>
                                <th>"Len"</th>
                                <th>"Preview"</th>
                                <th>"ASCII"</th>
                                <th>"CRC32"</th>
                            </tr>
                        </thead>
                        <tbody id=format!("rx-frames-{index}")>
                            <tr>
                                <td colspan="7" class="frames-empty">"Waiting for frames…"</td>
                            </tr>
                        </tbody>
                    </table>
                </div>
                <div class="frames-stats">
                    <span id=format!("rx-stats-summary-{index}") class="frames-stats-summary">
                        "RX: 0.0 KB/s | TX: 0.0 KB/s | Total: 0.0 KB/s"
                    </span>
                    <span id=format!("rx-stats-rssi-{index}") class="frames-stats-rssi">
                        "Last RSSI: —"
                    </span>
                </div>
            </div>
            <RadioModuleForm index=index module=module />
        </div>
    }
    .into_any()
}

fn radio_label(index: usize) -> &'static str {
    match index {
        0 => "Radio A",
        1 => "Radio B",
        _ => "Radio",
    }
}

// ── Sub-components (keep each view! small to avoid SSR stack overflow) ────────

#[component]
fn OfdmFields(div_id: String, ofdm: radio_common::modulation::OfdmModulation) -> impl IntoView {
    let mcs_val = ofdm.mcs as u8;
    let opt_val = ofdm.opt as u8;
    let tx_val = ofdm.tx_power;
    let tx_init_style = if tx_val > 21 {
        "accent-color:#f97316"
    } else {
        ""
    };
    let tx_init_class = if tx_val > 21 {
        "tx-value tx-high"
    } else {
        "tx-value"
    };
    view! {
        <div id=div_id>
            <div class="field-row">
                <label class="field-label">"MCS"</label>
                <select id=format!("ofdm-mcs-{div_id}") class="field-input">
                    <option value="0" selected=mcs_val==0>"0 · BPSK ½ 4×"</option>
                    <option value="1" selected=mcs_val==1>"1 · BPSK ½ 2×"</option>
                    <option value="2" selected=mcs_val==2>"2 · QPSK ½ 2×"</option>
                    <option value="3" selected=mcs_val==3>"3 · QPSK ½"</option>
                    <option value="4" selected=mcs_val==4>"4 · QPSK ¾"</option>
                    <option value="5" selected=mcs_val==5>"5 · 16-QAM ½"</option>
                    <option value="6" selected=mcs_val==6>"6 · 16-QAM ¾"</option>
                </select>
            </div>
            <div class="field-row">
                <label class="field-label">"BW Option"</label>
                <select id=format!("ofdm-opt-{div_id}") class="field-input">
                    <option value="0" selected=opt_val==0>"Option 1"</option>
                    <option value="1" selected=opt_val==1>"Option 2"</option>
                    <option value="2" selected=opt_val==2>"Option 3"</option>
                    <option value="3" selected=opt_val==3>"Option 4"</option>
                </select>
            </div>
            <div class="field-row">
                <label class="field-label">"TX Power"</label>
                <div class="tx-slider-wrap">
                    <input id=format!("otx-{div_id}") type="range" class="tx-slider" min="0" max="31" step="1"
                        style=tx_init_style
                        value=tx_val.to_string()
                    />
                    <span id=format!("otxv-{div_id}") class=tx_init_class>{format!("{tx_val} dBm")}</span>
                </div>
            </div>
        </div>
    }.into_any()
}

#[component]
fn QpskFields(div_id: String, qpsk: radio_common::modulation::QpskModulation) -> impl IntoView {
    let fchip_val = qpsk.fchip as u8;
    let mode_val = qpsk.mode as u8;
    let tx_val = qpsk.tx_power;
    let tx_init_style = if tx_val > 21 {
        "accent-color:#f97316"
    } else {
        ""
    };
    let tx_init_class = if tx_val > 21 {
        "tx-value tx-high"
    } else {
        "tx-value"
    };
    view! {
        <div id=div_id>
            <div class="field-row">
                <label class="field-label">"Chip Rate"</label>
                <select id=format!("qpsk-fchip-{div_id}") class="field-input">
                    <option value="0" selected=fchip_val==0>"100 kchip/s"</option>
                    <option value="1" selected=fchip_val==1>"200 kchip/s"</option>
                    <option value="2" selected=fchip_val==2>"1000 kchip/s"</option>
                    <option value="3" selected=fchip_val==3>"2000 kchip/s"</option>
                </select>
            </div>
            <div class="field-row">
                <label class="field-label">"Rate Mode"</label>
                <select id=format!("qpsk-mode-{div_id}") class="field-input">
                    <option value="0" selected=mode_val==0>"Mode 0"</option>
                    <option value="1" selected=mode_val==1>"Mode 1"</option>
                    <option value="2" selected=mode_val==2>"Mode 2"</option>
                    <option value="3" selected=mode_val==3>"Mode 3"</option>
                    <option value="4" selected=mode_val==4>"Mode 4"</option>
                </select>
            </div>
            <div class="field-row">
                <label class="field-label">"TX Power"</label>
                <div class="tx-slider-wrap">
                    <input id=format!("qtx-{div_id}") type="range" class="tx-slider" min="0" max="31" step="1"
                        style=tx_init_style
                        value=tx_val.to_string()
                    />
                    <span id=format!("qtxv-{div_id}") class=tx_init_class>{format!("{tx_val} dBm")}</span>
                </div>
            </div>
        </div>
    }.into_any()
}

#[component]
fn RadioModuleForm(index: usize, module: RadioModuleConfigDto) -> impl IntoView {
    use crate::radio::HardwareRadioConfig;
    use radio_common::modulation::{Modulation, OfdmModulation, QpskModulation};

    // Initial values for SSR render only — browser reads live from DOM via JS
    let freq_frac = {
        let hz = module.radio_config.freq.as_hz();
        let mhz_exact = hz as f64 / 1_000_000.0;
        format!("{:.3}", mhz_exact)
    };
    let channel = module.radio_config.channel.to_string();
    let spacing_khz = {
        let hz = module.radio_config.channel_spacing.as_hz();
        format!("{:.3}", hz as f64 / 1_000.0)
    };

    let init_mod = match &module.modulation {
        Modulation::Off => "Off",
        Modulation::Ofdm(_) => "OFDM",
        Modulation::Qpsk(_) => "QPSK",
        Modulation::Fsk => "FSK",
    };

    let init_accel = match module.accelerator {
        radio_common::Accelerator::Native => "Native",
        radio_common::Accelerator::Hardware => "Hardware",
    };

    let init_antenna = match module.antenna {
        radio_common::Antenna::Internal => "Internal",
        radio_common::Antenna::External => "External",
    };

    let ofdm = match &module.modulation {
        Modulation::Ofdm(o) => o.clone(),
        _ => OfdmModulation::default(),
    };
    let qpsk = match &module.modulation {
        Modulation::Qpsk(q) => q.clone(),
        _ => QpskModulation::default(),
    };
    let default_module = HardwareRadioConfig::default().module_configs[index].clone();
    let default_freq_frac = {
        let hz = default_module.radio_config.freq.as_hz();
        format!("{:.3}", hz as f64 / 1_000_000.0)
    };
    let default_spacing_khz = {
        let hz = default_module.radio_config.channel_spacing.as_hz();
        format!("{:.3}", hz as f64 / 1_000.0)
    };
    let default_channel = default_module.radio_config.channel.to_string();
    let default_ofdm = match default_module.modulation {
        Modulation::Ofdm(o) => o,
        _ => OfdmModulation::default(),
    };
    let default_qpsk = QpskModulation::default();
    let default_accel = match default_module.accelerator {
        radio_common::Accelerator::Native => "Native",
        radio_common::Accelerator::Hardware => "Hardware",
    };
    let default_antenna = match default_module.antenna {
        radio_common::Antenna::Internal => "Internal",
        radio_common::Antenna::External => "External",
    };

    let js = format!(
        r#"(function(){{
        var fi=document.getElementById('freq-{index}'),si=document.getElementById('spacing-{index}'),ci=document.getElementById('channel-{index}');
        var bs=document.getElementById('band-sub-{index}'),b2=document.getElementById('band-24-{index}');
        var ms=document.getElementById('ms{index}'),mo=document.getElementById('mo{index}'),mq=document.getElementById('mq{index}');
        var accel=document.getElementById('accel-{index}');
        var ant=document.getElementById('ant-{index}');
        var applyBtn=document.getElementById('apply-btn-{index}'),resetBtn=document.getElementById('reset-btn-{index}');
        var testBtn=document.getElementById('test-btn-{index}');
        var status=document.getElementById('apply-status-{index}');
        var testModal=document.getElementById('radio-test-modal-{index}');
        var testForm=document.getElementById('radio-test-form-{index}');
        var testInput=document.getElementById('radio-test-message-{index}');
        var testStatus=document.getElementById('radio-test-status-{index}');
        var testSendBtn=document.getElementById('radio-test-send-{index}');
        var testCounter=document.getElementById('radio-test-count-{index}');
        var closeTestButtons=document.querySelectorAll('[data-close-radio-test="{index}"]');
        var bands={{
            sub:{{label:'sub-GHz',freqMin:389.5,freqMax:1020.0,channelMax:255}},
            b24:{{label:'2.4 GHz',freqMin:2400.0,freqMax:2483.5,channelMax:511}}
        }};
        var defaults={{
            freq:'{default_freq_frac}',
            spacing:'{default_spacing_khz}',
            channel:'{default_channel}',
            modulation:'OFDM',
            accel:'{default_accel}',
            antenna:'{default_antenna}',
            ofdm:{{mcs:{},opt:{},tx:{}}},
            qpsk:{{fchip:{},mode:{},tx:{}}}
        }};
        function formatFixed(input, digits){{
            var value=parseFloat(input.value);
            if(Number.isFinite(value)){{input.value=value.toFixed(digits);}}
        }}
        function clampFreq(){{
            var f=parseFloat(fi.value);
            if(!Number.isFinite(f)) return false;
            var corrected=f;
            if(f<bands.sub.freqMin) corrected=bands.sub.freqMin;
            else if(f>bands.b24.freqMax) corrected=bands.b24.freqMax;
            else if(f>bands.sub.freqMax && f<bands.b24.freqMin){{
                corrected=(f-bands.sub.freqMax) <= (bands.b24.freqMin-f)
                    ? bands.sub.freqMax
                    : bands.b24.freqMin;
            }}
            if(corrected!==f){{
                fi.value=corrected.toFixed(3);
                return true;
            }}
            return false;
        }}
        function clampSpacing(){{
            var s=parseFloat(si.value);
            if(!Number.isFinite(s)) return false;
            var corrected=Math.min(10000,Math.max(0.001,s));
            if(corrected!==s){{
                si.value=corrected.toFixed(3);
                return true;
            }}
            return false;
        }}
        function bandForFreq(f){{
            if(Number.isFinite(f)){{
                if(f>=bands.sub.freqMin && f<=bands.sub.freqMax) return 'sub';
                if(f>=bands.b24.freqMin && f<=bands.b24.freqMax) return 'b24';
            }}
            return f>=bands.b24.freqMin ? 'b24' : 'sub';
        }}
        function validateFreq(){{
            var f=parseFloat(fi.value);
            var valid=Number.isFinite(f) && (
                (f>=bands.sub.freqMin && f<=bands.sub.freqMax) ||
                (f>=bands.b24.freqMin && f<=bands.b24.freqMax)
            );
            fi.setCustomValidity(
                valid
                    ? ''
                    : 'Use 389.500-1020.000 MHz for sub-GHz or 2400.000-2483.500 MHz for 2.4 GHz.'
            );
            return valid;
        }}
        function validateChannel(){{
            var channel=parseInt(ci.value,10);
            var band=bands[bandForFreq(parseFloat(fi.value))];
            ci.max=String(band.channelMax);
            var valid=Number.isInteger(channel) && channel>=0 && channel<=band.channelMax;
            ci.setCustomValidity(valid ? '' : 'Use a channel between 0 and ' + band.channelMax + ' for ' + band.label + '.');
            return valid;
        }}
        function validateSpacing(){{
            var s=parseFloat(si.value);
            var valid=Number.isFinite(s) && s>=0.001 && s<=10000;
            si.setCustomValidity(valid ? '' : 'Use a spacing between 0.001 and 10000.000 kHz.');
            return valid;
        }}
        function syncBand(){{
            var band=bandForFreq(parseFloat(fi.value));
            bs.classList.toggle('active',band==='sub');
            b2.classList.toggle('active',band==='b24');
            validateChannel();
        }}
        syncBand();
        validateFreq();
        validateChannel();
        validateSpacing();
        fi.addEventListener('input',function(){{validateFreq();syncBand();}});
        ci.addEventListener('input',validateChannel);
        si.addEventListener('input',validateSpacing);
        fi.addEventListener('blur',function(){{clampFreq();validateFreq();formatFixed(fi,3);syncBand();}});
        ci.addEventListener('blur',validateChannel);
        si.addEventListener('blur',function(){{clampSpacing();validateSpacing();formatFixed(si,3);}});
        bs.addEventListener('click',function(){{fi.value='915.000';si.value='200.000';validateFreq();validateSpacing();syncBand();}});
        b2.addEventListener('click',function(){{fi.value='2450.000';si.value='5000.000';validateFreq();validateSpacing();syncBand();}});
        function syncMod(){{mo.style.display=ms.value==='OFDM'?'':'none';mq.style.display=ms.value==='QPSK'?'':'none';}}
        syncMod();ms.addEventListener('change',syncMod);
        function syncTxValue(sl,sp){{
            var v=parseInt(sl.value,10)||0;
            sp.textContent=v+' dBm';
            if(v>21){{sl.style.accentColor='#f97316';sp.className='tx-value tx-high';}}
            else{{sl.style.accentColor='';sp.className='tx-value';}}
        }}
        var otx=document.getElementById('otx-mo{index}'),otxv=document.getElementById('otxv-mo{index}');
        var qtx=document.getElementById('qtx-mq{index}'),qtxv=document.getElementById('qtxv-mq{index}');
        if(otx){{otx.addEventListener('input',function(){{syncTxValue(otx,otxv);}});syncTxValue(otx,otxv);}}
        if(qtx){{qtx.addEventListener('input',function(){{syncTxValue(qtx,qtxv);}});syncTxValue(qtx,qtxv);}}
        function setTestStatus(text, className){{
            if(!testStatus) return;
            testStatus.textContent=text;
            testStatus.className=className || '';
        }}
        function updateTestCount(){{
            if(!testInput || !testCounter) return;
            testCounter.textContent=String(testInput.value.length) + ' / 2047';
        }}
        function openTestModal(){{
            if(!testModal) return;
            testModal.hidden=false;
            document.body.classList.add('modal-open');
            setTestStatus('', '');
            updateTestCount();
            if(testInput) testInput.focus();
        }}
        function closeTestModal(){{
            if(!testModal) return;
            testModal.hidden=true;
            document.body.classList.remove('modal-open');
            setTestStatus('', '');
        }}
        function sendTestMessage(){{
            if(!testInput || !testSendBtn) return;
            var message=testInput.value || '';
            if(!message.trim()) {{
                setTestStatus('Message is required.', 'flash-err');
                return;
            }}
            if(message.length > 2047) {{
                setTestStatus('Message exceeds 2047 characters.', 'flash-err');
                return;
            }}
            testSendBtn.disabled=true;
            setTestStatus('Sending…', 'flash-ok');
            fetch('/api/radio/{index}/test', {{
                method:'POST',
                headers:{{'Content-Type':'application/json'}},
                body:JSON.stringify({{message:message}})
            }}).then(function(r){{
                if(!r.ok){{
                    return r.text().then(function(t){{ throw new Error(t || ('HTTP ' + r.status)); }});
                }}
                return r.json();
            }}).then(function(resp){{
                status.textContent=resp.status || '✓ Test frame sent';
                status.className='flash-ok';
                closeTestModal();
                testInput.value='';
                updateTestCount();
            }}).catch(function(err){{
                setTestStatus('Error: ' + (err.message || err), 'flash-err');
            }}).finally(function(){{
                testSendBtn.disabled=false;
            }});
        }}
        if(testBtn){{testBtn.addEventListener('click',openTestModal);}}
        if(testInput){{testInput.addEventListener('input',updateTestCount); updateTestCount();}}
        closeTestButtons.forEach(function(btn){{ btn.addEventListener('click', closeTestModal); }});
        if(testModal){{
            testModal.addEventListener('click', function(ev){{ if(ev.target===testModal) closeTestModal(); }});
        }}
        if(testForm){{
            testForm.addEventListener('submit', function(ev){{ ev.preventDefault(); sendTestMessage(); }});
        }}
        window.addEventListener('keydown', function(ev){{ if(ev.key==='Escape' && testModal && !testModal.hidden) closeTestModal(); }});
        // Apply button
        var MCS=['BpskC1_2_4x','BpskC1_2_2x','QpskC1_2_2x','QpskC1_2','QpskC3_4','QamC1_2','QamC3_4'];
        var BWOPT=['Option1','Option2','Option3','Option4'];
        var FCHIP=['Fchip100','Fchip200','Fchip1000','Fchip2000'];
        var MODE=['RateMode0','RateMode1','RateMode2','RateMode3','RateMode4'];
        function applyConfig(){{
            var modVal=ms.value;
            var modulation;
            if(modVal==='OFDM'){{
                modulation={{Ofdm:{{
                    mcs:MCS[parseInt(document.getElementById('ofdm-mcs-mo{index}').value)||0],
                    opt:BWOPT[parseInt(document.getElementById('ofdm-opt-mo{index}').value)||0],
                    tx_power:parseInt(document.getElementById('otx-mo{index}').value)||10
                }}}};
            }}else if(modVal==='QPSK'){{
                modulation={{Qpsk:{{
                    fchip:FCHIP[parseInt(document.getElementById('qpsk-fchip-mq{index}').value)||0],
                    mode:MODE[parseInt(document.getElementById('qpsk-mode-mq{index}').value)||0],
                    tx_power:parseInt(document.getElementById('qtx-mq{index}').value)||10
                }}}};
            }}else if(modVal==='FSK'){{
                modulation='Fsk';
            }}else{{
                modulation='Off';
            }}
            var freqHz=Math.round((parseFloat(fi.value)||869.535)*1000000);
            var spacingHz=Math.round((parseFloat(si.value)||200)*1000);
            var payload={{
                radio_config:{{
                    freq:freqHz,
                    channel_spacing:spacingHz,
                    channel:parseInt(ci.value)||0,
                    bandwidth_filter:'Narrow'
                }},
                modulation:modulation,
                accelerator:accel?accel.value:'Native',
                // Always sent so the 2.4 GHz choice survives a retune to sub-GHz;
                // the server only applies it when the module is on 2.4 GHz.
                antenna:ant?ant.value:'Internal'
            }};
            validateFreq();
            validateChannel();
            validateSpacing();
            if(!fi.reportValidity() || !ci.reportValidity() || !si.reportValidity()){{
                status.textContent='Fix invalid values.';
                status.className='flash-err';
                return;
            }}
            formatFixed(fi,3);
            formatFixed(si,3);
            applyBtn.disabled=true;
            if(resetBtn){{resetBtn.disabled=true;}}
            applyBtn.textContent='Applying…';status.textContent='';
            fetch('/api/settings/radio/{index}',{{
                method:'PUT',
                headers:{{'Content-Type':'application/json'}},
                body:JSON.stringify(payload)
            }}).then(function(r){{
                if(r.ok){{status.textContent='✓ Applied';status.className='flash-ok';}}
                else{{r.text().then(function(t){{status.textContent='Error: '+t;status.className='flash-err';}});}}
            }}).catch(function(e){{
                status.textContent='Error: '+e;status.className='flash-err';
            }}).finally(function(){{
                applyBtn.disabled=false;
                if(resetBtn){{resetBtn.disabled=false;}}
                applyBtn.textContent='Apply';
            }});
        }}
        applyBtn.addEventListener('click',applyConfig);
        if(resetBtn){{
            resetBtn.addEventListener('click',function(){{
                fi.value=defaults.freq;
                si.value=defaults.spacing;
                ci.value=defaults.channel;
                ms.value=defaults.modulation;
                if(accel){{accel.value=defaults.accel;}}
                if(ant){{ant.value=defaults.antenna;}}
                document.getElementById('ofdm-mcs-mo{index}').value=String(defaults.ofdm.mcs);
                document.getElementById('ofdm-opt-mo{index}').value=String(defaults.ofdm.opt);
                if(otx){{otx.value=String(defaults.ofdm.tx);syncTxValue(otx,otxv);}}
                document.getElementById('qpsk-fchip-mq{index}').value=String(defaults.qpsk.fchip);
                document.getElementById('qpsk-mode-mq{index}').value=String(defaults.qpsk.mode);
                if(qtx){{qtx.value=String(defaults.qpsk.tx);syncTxValue(qtx,qtxv);}}
                validateFreq();
                validateChannel();
                validateSpacing();
                syncBand();
                syncMod();
                status.textContent='Resetting to defaults…';
                status.className='flash-ok';
                applyConfig();
            }});
        }}
    }})();"#,
        default_ofdm.mcs as u8,
        default_ofdm.opt as u8,
        default_ofdm.tx_power,
        default_qpsk.fchip as u8,
        default_qpsk.mode as u8,
        default_qpsk.tx_power
    );

    view! {
        <div class="card radio-settings-card">
            <div class="card-header">
                <span class="card-title">{radio_label(index)}</span>
                <div class="band-toggle-wrap">
                    <button type="button" id=format!("band-sub-{index}") class="band-btn">"Sub-GHz"</button>
                    <button type="button" id=format!("band-24-{index}") class="band-btn">"2.4 GHz"</button>
                </div>
            </div>

            <div class="settings-grid">
                // Left column: RF fields
                <div class="settings-col">
                    <div class="field-row">
                        <label class="field-label">"Freq (MHz)"</label>
                        <input id=format!("freq-{index}") type="number" class="field-input" step="0.001" min="389.500" max="2483.500" required
                            value=freq_frac
                        />
                    </div>
                    <div class="field-row">
                        <label class="field-label">"Channel"</label>
                        <input id=format!("channel-{index}") type="number" class="field-input" min="0" max="511" step="1" required
                            value=channel
                        />
                    </div>
                    <div class="field-row">
                        <label class="field-label">"Spacing (kHz)"</label>
                        <input id=format!("spacing-{index}") type="number" class="field-input" step="0.001" min="0.001" max="10000" required
                            value=spacing_khz
                        />
                    </div>
                    <div class="field-row">
                        <label class="field-label">"Modulation"</label>
                        <select class="field-input" id=format!("ms{index}")>
                            <option value="Off"  selected=init_mod=="Off">"Off"</option>
                            <option value="OFDM" selected=init_mod=="OFDM">"OFDM"</option>
                            <option value="QPSK" selected=init_mod=="QPSK">"QPSK"</option>
                            <option value="FSK"  selected=init_mod=="FSK">"FSK"</option>
                        </select>
                    </div>
                    <div class="field-row">
                        <label class="field-label">"Acceleration"</label>
                        <select class="field-input" id=format!("accel-{index}")>
                            <option value="Native" selected=init_accel=="Native">"Native (on-chip)"</option>
                            <option value="Hardware" selected=init_accel=="Hardware">"Hardware (FPGA)"</option>
                        </select>
                    </div>
                    <div class="field-row">
                        <label class="field-label">"Antenna"</label>
                        <select class="field-input" id=format!("ant-{index}")>
                            <option value="Internal" selected=init_antenna=="Internal">"Internal (on-board)"</option>
                            <option value="External" selected=init_antenna=="External">"External (connector)"</option>
                        </select>
                    </div>
                </div>

                // Right column: modulation-specific fields
                <div class="settings-col">
                    <OfdmFields div_id=format!("mo{index}") ofdm=ofdm />
                    <QpskFields div_id=format!("mq{index}") qpsk=qpsk />
                </div>
            </div>

            <div class="form-actions">
                <div class="radio-form-left">
                    <button type="button" id=format!("test-btn-{index}") class="btn-secondary">
                        "Test"
                    </button>
                </div>
                <span id=format!("apply-status-{index}")></span>
                <button type="button" id=format!("reset-btn-{index}") class="btn-secondary">
                    "Reset"
                </button>
                <button type="button" id=format!("apply-btn-{index}") class="btn-apply">
                    "Apply"
                </button>
            </div>
            <div class="modal-backdrop" id=format!("radio-test-modal-{index}") hidden>
                <div class="modal-card">
                    <div class="modal-header">
                        <h2 class="modal-title">{format!("Test {}", radio_label(index))}</h2>
                        <button type="button" class="modal-close" data-close-radio-test=index.to_string()>"×"</button>
                    </div>
                    <form id=format!("radio-test-form-{index}") class="modal-form">
                        <label class="form-label" for=format!("radio-test-message-{index}")>
                            "Message"
                        </label>
                        <textarea
                            id=format!("radio-test-message-{index}")
                            class="form-textarea radio-test-textarea"
                            rows="7"
                            maxlength="2047"
                            placeholder="Type a test message to transmit on this radio module"
                            required
                        ></textarea>
                        <div class="radio-test-meta">
                            <span class="card-body-text">"Max 2047 characters"</span>
                            <span class="card-body-text" id=format!("radio-test-count-{index}")>"0 / 2047"</span>
                        </div>
                        <div id=format!("radio-test-status-{index}")></div>
                        <div class="modal-actions">
                            <button type="button" class="btn-secondary" data-close-radio-test=index.to_string()>
                                "Cancel"
                            </button>
                            <button type="submit" class="btn-primary" id=format!("radio-test-send-{index}")>
                                "Send"
                            </button>
                        </div>
                    </form>
                </div>
            </div>
            <script>{js}</script>
        </div>
    }.into_any()
}
