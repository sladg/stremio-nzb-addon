use maud::{html, Markup, PreEscaped, DOCTYPE};

use crate::config::{AddonConfig, Indexer, NntpServer};
use crate::manifest::{config_fields, manifest, ConfigField, FieldKind};

pub fn render_configure(cfg: &AddonConfig, host: &str) -> Markup {
    let m = manifest();
    let install_url = format!("http://{host}/manifest.json");
    let fields = config_fields();

    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width,initial-scale=1";
                title { (m.name) " — Configure" }
            }
            body {
                h1 { (m.name) }
                p { (m.description) }

                form id="configure-form" onsubmit="return false" {
                    @for field in &fields {
                        (render_field(field, "", cfg))
                    }
                }

                p {
                    button type="button" onclick="saveConfig()" { "Save" }
                    " "
                    span id="save-status" {}
                }

                hr;

                h2 { "Install" }
                p {
                    "Add this URL in Stremio: "
                    code id="install-url" { (install_url) }
                    " "
                    button type="button" onclick=(format!("navigator.clipboard.writeText('{install_url}')")) { "copy" }
                }

                hr;

                h2 { "Status" }
                p {
                    "Active sessions: " span id="status-sessions" { "—" }
                    "  •  Cache: " span id="status-cache" { "—" }
                    "  •  Uptime: " span id="status-uptime" { "—" }
                }
                p style="font-size:90%;color:#888" {
                    "Cache dir: " code id="status-cache-dir" { "—" }
                }

                script {
                    (PreEscaped(SCRIPT))
                }
            }
        }
    }
}

fn render_field(field: &ConfigField, key_prefix: &str, cfg: &AddonConfig) -> Markup {
    let full_key = if key_prefix.is_empty() {
        field.key.to_string()
    } else {
        format!("{key_prefix}.{}", field.key)
    };

    match field.kind {
        FieldKind::Text | FieldKind::Password | FieldKind::Number => {
            let value = read_scalar(cfg, &full_key);
            html! {
                div style="margin:0.6em 0" {
                    label {
                        div { (field.title) @if field.required { " *" } }
                        input
                            type=(input_type(&field.kind))
                            data-key=(full_key)
                            data-required=(field.required.to_string())
                            placeholder=[field.placeholder]
                            value=(value)
                            style="width:100%;max-width:500px";
                    }
                    @if matches!(field.kind, FieldKind::Text) && full_key.ends_with(".url") {
                        button type="button" data-test="indexer" onclick="testIndexerForKey(this)" {
                            "Test indexer"
                        }
                    }
                    @if matches!(field.kind, FieldKind::Text) && full_key.ends_with(".server") {
                        button type="button" data-test="nntp" onclick="testNntpForKey(this)" {
                            "Test NNTP"
                        }
                    }
                }
            }
        }
        FieldKind::Checkbox => {
            let checked = read_bool(cfg, &full_key);
            html! {
                div style="margin:0.6em 0" {
                    label {
                        input
                            type="checkbox"
                            data-key=(full_key)
                            data-kind="checkbox"
                            checked[checked];
                        " " (field.title)
                    }
                }
            }
        }
        FieldKind::StringList => {
            let value = read_scalar(cfg, &full_key);
            html! {
                div style="margin:0.6em 0" {
                    label {
                        div { (field.title) @if field.required { " *" } }
                        input
                            type="text"
                            data-key=(full_key)
                            data-kind="string-list"
                            data-required=(field.required.to_string())
                            placeholder=[field.placeholder]
                            value=(value)
                            style="width:100%;max-width:500px";
                    }
                }
            }
        }
        FieldKind::Array => {
            let count = match field.key {
                "indexers" => cfg.indexers.len().max(1),
                "nntpServers" => cfg.nntp_servers.len().max(1),
                _ => 1,
            };
            html! {
                fieldset style="margin:0.6em 0;padding:0.5em" data-array=(full_key) {
                    legend { (field.title) @if field.required { " *" } }
                    div data-array-rows=(full_key) {
                        @for idx in 0..count {
                            (render_array_row(field, &full_key, idx, cfg))
                        }
                    }
                    button type="button" onclick=(format!("addRow('{full_key}')")) { "+ add" }
                }
            }
        }
    }
}

fn render_array_row(field: &ConfigField, full_key: &str, idx: usize, cfg: &AddonConfig) -> Markup {
    let Some(opts) = &field.array_options else {
        return html!();
    };
    html! {
        div data-array-row=(full_key) data-row-idx=(idx.to_string())
            style="border-left:2px solid #888;padding:0.4em 0.6em;margin:0.4em 0" {
            @for sub in opts {
                (render_field(sub, &format!("{full_key}[{idx}]"), cfg))
            }
            button type="button" onclick="this.parentElement.remove()" { "− remove" }
        }
    }
}

fn input_type(kind: &FieldKind) -> &'static str {
    match kind {
        FieldKind::Password => "password",
        FieldKind::Number => "number",
        _ => "text",
    }
}

fn read_scalar(cfg: &AddonConfig, key: &str) -> String {
    // Resolve a dot/bracket path against the live config to prefill the form.
    if let Some((arr_name, rest)) = key.split_once('[') {
        let (idx_str, sub) = rest.split_once(']').unwrap_or((rest, ""));
        let idx: usize = idx_str.parse().unwrap_or(0);
        let sub = sub.strip_prefix('.').unwrap_or(sub);
        match arr_name {
            "indexers" => match cfg.indexers.get(idx) {
                Some(Indexer { url, api_key, .. }) => match sub {
                    "url" => url.clone(),
                    "apiKey" => api_key.clone(),
                    _ => String::new(),
                },
                None => String::new(),
            },
            "nntpServers" => match cfg.nntp_servers.get(idx) {
                Some(NntpServer { server, .. }) => match sub {
                    "server" => server.clone(),
                    _ => String::new(),
                },
                None => String::new(),
            },
            _ => String::new(),
        }
    } else {
        match key {
            "minGbitPerHour" => format_optional_f64(cfg.min_gbit_per_hour),
            "maxGbitPerHour" => format_optional_f64(cfg.max_gbit_per_hour),
            "excludeRegex" => cfg.exclude_regex.clone().unwrap_or_default(),
            "streamsPerResolution" => cfg
                .streams_per_resolution
                .map(|n| n.to_string())
                .unwrap_or_default(),
            "preferredLanguages" => cfg.preferred_languages.join(", "),
            _ => String::new(),
        }
    }
}

fn format_optional_f64(v: Option<f64>) -> String {
    v.map(|f| {
        if f == f.trunc() {
            format!("{}", f as i64)
        } else {
            format!("{}", f)
        }
    })
    .unwrap_or_default()
}

fn read_bool(cfg: &AddonConfig, key: &str) -> bool {
    match key {
        "validateNzbStructure" => cfg.validate_nzb_structure.unwrap_or(false),
        "validateNzbAvailability" => cfg.validate_nzb_availability.unwrap_or(false),
        _ => false,
    }
}

const SCRIPT: &str = r##"
function setVal(obj, path, value) {
    const tokens = [];
    const re = /([^.\[\]]+)|\[(\d+)\]/g;
    let m;
    while ((m = re.exec(path))) tokens.push(m[1] !== undefined ? {k: m[1]} : {i: parseInt(m[2])});
    let cur = obj;
    for (let i = 0; i < tokens.length; i++) {
        const t = tokens[i];
        const last = i === tokens.length - 1;
        const next = tokens[i+1];
        const wantArray = next && next.i !== undefined;
        if (t.k !== undefined) {
            if (last) cur[t.k] = value;
            else { if (cur[t.k] === undefined) cur[t.k] = wantArray ? [] : {}; cur = cur[t.k]; }
        } else {
            if (last) cur[t.i] = value;
            else { if (cur[t.i] === undefined) cur[t.i] = wantArray ? [] : {}; cur = cur[t.i]; }
        }
    }
}

function collectConfig() {
    const cfg = {};
    document.querySelectorAll("#configure-form [data-key]").forEach(el => {
        const key = el.dataset.key;
        let val;
        if (el.dataset.kind === "checkbox") val = el.checked;
        else if (el.dataset.kind === "string-list") {
            const arr = el.value.split(",").map(s => s.trim()).filter(s => s.length > 0);
            if (arr.length === 0) return;
            setVal(cfg, key, arr);
            return;
        }
        else if (el.type === "number") val = el.value === "" ? undefined : Number(el.value);
        else val = el.value;
        if (val === undefined || val === "" || val === false) return;
        setVal(cfg, key, val);
    });
    // Compact arrays: drop holes that come from removed rows.
    if (Array.isArray(cfg.indexers)) cfg.indexers = cfg.indexers.filter(Boolean);
    if (Array.isArray(cfg.nntpServers)) cfg.nntpServers = cfg.nntpServers.filter(Boolean);
    return cfg;
}

async function saveConfig() {
    const status = document.getElementById("save-status");
    status.textContent = "saving…";
    status.style.color = "";
    const body = JSON.stringify(collectConfig());
    try {
        const res = await fetch("/api/config", {
            method: "POST",
            headers: {"content-type": "application/json"},
            body
        });
        const json = await res.json().catch(() => ({}));
        if (res.ok && json.ok) {
            status.style.color = "green";
            status.textContent = "✓ saved";
        } else {
            status.style.color = "crimson";
            status.textContent = "✗ " + (json.error || ("HTTP " + res.status));
        }
    } catch (e) {
        status.style.color = "crimson";
        status.textContent = "✗ " + e.message;
    }
}

function addRow(key) {
    const wrap = document.querySelector("[data-array-rows='" + key + "']");
    if (!wrap.firstElementChild) return;
    const sample = wrap.firstElementChild;
    const clone = sample.cloneNode(true);
    const newIdx = wrap.children.length;
    clone.dataset.rowIdx = newIdx;
    clone.querySelectorAll("[data-key]").forEach(el => {
        el.value = "";
        el.checked = false;
        el.dataset.key = el.dataset.key.replace(/\[\d+\]/, "[" + newIdx + "]");
    });
    // Strip any leftover ✓/✗ healthcheck status text.
    clone.querySelectorAll("button + *").forEach(n => { if (n.nodeType === 3 || n.tagName === undefined) n.remove(); });
    wrap.appendChild(clone);
}

async function testIndexerForKey(btn) {
    const row = btn.closest("[data-array-row], div");
    const url = row.querySelector("[data-key$='.url']")?.value;
    const apiKey = row.querySelector("[data-key$='.apiKey']")?.value;
    if (!url || !apiKey) { btn.insertAdjacentText("afterend", " ⚠ fill url + apiKey first"); return; }
    btn.disabled = true; btn.textContent = "Testing…";
    const res = await fetch("/api/healthcheck/indexer", {
        method: "POST", headers: {"content-type": "application/json"},
        body: JSON.stringify({url, apiKey})
    }).then(r => r.json()).catch(e => ({ok: false, error: e.message}));
    btn.disabled = false; btn.textContent = "Test indexer";
    btn.insertAdjacentHTML("afterend", " " + (res.ok ? "✓" : "✗ " + (res.error || "")));
}

function fmtBytes(n) {
    if (!n) return "0 B";
    const u = ["B","kB","MB","GB","TB"];
    let i = 0; let v = n;
    while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
    return v.toFixed(2) + " " + u[i];
}

function fmtSecs(s) {
    if (s < 60) return s + "s";
    if (s < 3600) return Math.floor(s/60) + "m " + (s%60) + "s";
    return Math.floor(s/3600) + "h " + Math.floor((s%3600)/60) + "m";
}

async function pollStatus() {
    try {
        const r = await fetch("/api/status");
        if (!r.ok) return;
        const j = await r.json();
        document.getElementById("status-sessions").textContent = j.sessions;
        document.getElementById("status-cache").textContent =
            fmtBytes(j.cache_bytes) + " / " + fmtBytes(j.cache_max_bytes);
        document.getElementById("status-uptime").textContent = fmtSecs(j.uptime_secs);
        document.getElementById("status-cache-dir").textContent = j.cache_dir;
    } catch (_) {}
}
pollStatus();
setInterval(pollStatus, 10_000);

async function testNntpForKey(btn) {
    const row = btn.closest("[data-array-row]");
    const server = row.querySelector("[data-key$='.server']")?.value;
    if (!server) { btn.insertAdjacentText("afterend", " ⚠ fill server first"); return; }
    btn.disabled = true; btn.textContent = "Testing…";
    const res = await fetch("/api/healthcheck/nntp", {
        method: "POST", headers: {"content-type": "application/json"},
        body: JSON.stringify({server})
    }).then(r => r.json()).catch(e => ({ok: false, error: e.message}));
    btn.disabled = false; btn.textContent = "Test NNTP";
    btn.insertAdjacentHTML("afterend", " " + (res.ok ? "✓" : "✗ " + (res.error || "")));
}
"##;
