//! Wireless debugging: discovery of phones that are waiting to be paired, pairing
//! (pairing code / QR code) and connection.
//!
//! Android only advertises the `_adb-tls-pairing._tcp` mDNS service while the
//! "Pair device with pairing code" dialog is open (or after scanning a pairing QR code).
//! The 6-digit code itself is never sent over the network (it is the password of a PAKE
//! handshake), so it cannot be detected: what we can detect is *that* a phone is waiting
//! and *where* (IP and port), which leaves the user with typing only the code.
//!
//! Discovery merges two sources so that it keeps working if one of them is blocked:
//! * a built-in mDNS browser (`mdns-sd`), event driven and independent of the adb version;
//! * `adb mdns services`, polled only while a dialog/QR session needs it.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use mdns_sd::{DaemonEvent, IfKind, ServiceDaemon, ServiceEvent};
use rand::{distr::Alphanumeric, RngExt};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::watch;

use crate::adb;
use crate::parsers::device_parser;

const TYPE_PAIRING: &str = "_adb-tls-pairing._tcp";
const TYPE_CONNECT: &str = "_adb-tls-connect._tcp";
const EVENT_SERVICES: &str = "wireless-services-changed";
const EVENT_PAIR: &str = "wireless-pair-status";

/// How often `adb mdns services` is polled while somebody is looking at the discovery list.
const ADB_POLL_INTERVAL: Duration = Duration::from_millis(1500);
const ADB_POLL_FAILURE_INTERVAL: Duration = Duration::from_secs(5);
/// Delay before the always-on detector starts, so any firewall prompt appears once the UI is visible.
const BACKGROUND_START_DELAY: Duration = Duration::from_millis(2500);
const QR_TIMEOUT: Duration = Duration::from_secs(300);
const PAIR_TIMEOUT: Duration = Duration::from_secs(45);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Time given to adb's own mDNS auto-connect before connecting explicitly (avoids duplicate transports).
const AUTO_CONNECT_GRACE: Duration = Duration::from_millis(2500);
const POST_PAIR_CONNECT_WINDOW: Duration = Duration::from_secs(15);
/// `adb connect` right after `adb tcpip` either works within a moment or has to be retried.
const TCPIP_CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(6);
/// A phone registers its mDNS name slightly before it listens for the pairing handshake, so a
/// quick "unreachable" right after the QR scan is retried a few times.
const QR_PAIR_ATTEMPTS: u32 = 4;
const QR_PAIR_RETRY_DELAY: Duration = Duration::from_millis(1200);
const QR_PAIR_RETRY_MAX_ATTEMPT_TIME: Duration = Duration::from_secs(10);

/// Stable error codes understood by the frontend (`src/context/wireless.svelte.ts`).
pub const ERR_INVALID_ENDPOINT: &str = "ERROR_WIRELESS_INVALID_ENDPOINT";
pub const ERR_INVALID_CODE: &str = "ERROR_WIRELESS_INVALID_CODE";
pub const ERR_WRONG_CODE: &str = "ERROR_WIRELESS_WRONG_CODE";
pub const ERR_UNREACHABLE: &str = "ERROR_WIRELESS_UNREACHABLE";
pub const ERR_NOT_PAIRED: &str = "ERROR_WIRELESS_NOT_PAIRED";
pub const ERR_QR_TIMEOUT: &str = "ERROR_WIRELESS_QR_TIMEOUT";

// ---------------------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    /// `_adb-tls-pairing._tcp`: the phone is showing a pairing code / waiting for a QR scan.
    Pairing,
    /// `_adb-tls-connect._tcp`: Wireless debugging is on and the phone accepts connections.
    Connect,
}

impl ServiceKind {
    fn service_type(self) -> &'static str {
        match self {
            Self::Pairing => TYPE_PAIRING,
            Self::Connect => TYPE_CONNECT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct WirelessService {
    pub kind: ServiceKind,
    /// mDNS instance name, e.g. `adb-R58M12ABC-AbCdEf` (this is the `guid` printed by `adb pair`).
    pub instance: String,
    /// Human friendly name derived from the instance name.
    pub label: String,
    pub host: String,
    pub port: u16,
    /// `host:port` (IPv6 hosts are bracketed), ready for `adb pair` / `adb connect`.
    pub endpoint: String,
}

impl WirelessService {
    fn new(kind: ServiceKind, instance: &str, host: &str, port: u16) -> Self {
        Self {
            kind,
            instance: instance.to_string(),
            label: friendly_label(instance),
            host: host.to_string(),
            port,
            endpoint: format_endpoint(host, port),
        }
    }

    fn is_instance(&self, name: &str) -> bool {
        strip_conflict_suffix(&self.instance).eq_ignore_ascii_case(strip_conflict_suffix(name))
    }

    /// Identity used to decide whether two discoveries describe the same advertisement.
    fn dedupe_key(&self) -> (ServiceKind, String, u16) {
        (
            self.kind,
            strip_conflict_suffix(&self.instance).to_ascii_lowercase(),
            self.port,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WirelessSnapshot {
    pub services: Vec<WirelessService>,
    /// True while the built-in mDNS browser is running.
    pub builtin_active: bool,
    /// Last error reported by the built-in browser (diagnostics only, it may be benign).
    pub builtin_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairOutcome {
    /// Raw adb output of the pairing step.
    pub message: String,
    /// Instance name of the paired device (`[guid=...]`), when adb reported it.
    pub guid: Option<String>,
    /// Serial of the connected device, when the automatic connection succeeded.
    pub serial: Option<String>,
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WirelessQr {
    pub session_id: String,
    pub service_name: String,
    pub qr_data: String,
}

/// Progress of a pairing attempt (QR session or pairing-code flow), sent as `wireless-pair-status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PairStatus {
    session_id: String,
    /// `waiting` | `found` | `pairing` | `connecting` | `done` | `error`
    status: &'static str,
    label: Option<String>,
    /// `host:port` of the phone's pairing service (known once it was found).
    endpoint: Option<String>,
    serial: Option<String>,
    connected: bool,
    error: Option<String>,
}

// ---------------------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------------------

#[derive(Default)]
struct Registry {
    /// Services seen by the built-in mDNS browser, keyed by lowercase full name.
    builtin: HashMap<String, WirelessService>,
    /// Services listed by the last `adb mdns services` poll.
    adb: Vec<WirelessService>,
}

impl Registry {
    /// Union of both sources. The built-in browser wins when both describe the same advertisement.
    fn merged(&self) -> Vec<WirelessService> {
        let mut seen = HashSet::new();
        let mut merged = Vec::new();
        // HashMap order is random: sort first so the winner of a duplicate is always the same.
        let mut builtin: Vec<&WirelessService> = self.builtin.values().collect();
        builtin.sort_by(|a, b| {
            (a.dedupe_key(), &a.instance, &a.host).cmp(&(b.dedupe_key(), &b.instance, &b.host))
        });
        for service in builtin.into_iter().chain(self.adb.iter()) {
            if seen.insert(service.dedupe_key()) {
                merged.push(service.clone());
            }
        }
        merged.sort_by(|a, b| {
            (a.kind, &a.label, &a.endpoint).cmp(&(b.kind, &b.label, &b.endpoint))
        });
        merged
    }
}

#[derive(Default)]
struct Inner {
    registry: Registry,
    published: Vec<WirelessService>,
    daemon: Option<ServiceDaemon>,
    builtin_error: Option<String>,
    /// The "detect pairing requests" setting: keeps the built-in browser alive.
    background: bool,
    /// Dialogs / QR sessions currently interested in discovery (also enables the adb poller).
    foreground: usize,
    poller: Option<tauri::async_runtime::JoinHandle<()>>,
    qr: Option<QrHandle>,
    /// Bumped every time the built-in browser starts or stops; events carry the generation of the
    /// daemon that produced them so late events of an old daemon are ignored.
    generation: u64,
}

struct QrHandle {
    session_id: String,
    task: tauri::async_runtime::JoinHandle<()>,
}

pub struct WirelessState {
    inner: Mutex<Inner>,
    /// Bumped whenever the published service list changes; lets async waiters re-check.
    version: watch::Sender<u64>,
}

impl WirelessState {
    pub fn new() -> Self {
        let (version, _) = watch::channel(0);
        Self {
            inner: Mutex::new(Inner::default()),
            version,
        }
    }

    /// Locks the state, recovering from poisoning (a panic elsewhere must not disable pairing).
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn snapshot(&self) -> WirelessSnapshot {
        let inner = self.lock();
        WirelessSnapshot {
            services: inner.published.clone(),
            builtin_active: inner.daemon.is_some(),
            builtin_error: inner.builtin_error.clone(),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Pure helpers (parsing / formatting) - unit tested below
// ---------------------------------------------------------------------------------------

/// `adb-XYZ (2)` -> `adb-XYZ`. mDNS name conflicts append ` (N)` to the instance name.
fn strip_conflict_suffix(name: &str) -> &str {
    if let Some(stripped) = name.strip_suffix(')') {
        if let Some((base, number)) = stripped.rsplit_once(" (") {
            if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
                return base;
            }
        }
    }
    name
}

/// `adb-<serial>-<suffix>` -> `<serial>`; any other name is returned untouched.
fn friendly_label(instance: &str) -> String {
    let name = strip_conflict_suffix(instance);
    let Some(rest) = name.strip_prefix("adb-") else {
        return name.to_string();
    };
    match rest.rsplit_once('-') {
        Some((serial, suffix))
            if !serial.is_empty()
                && !suffix.is_empty()
                && suffix.len() <= 8
                && suffix.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            serial.to_string()
        }
        _ => rest.to_string(),
    }
}

fn format_endpoint(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Parses `host:port` / `[v6]:port`. Rejects anything that is not a plain network endpoint,
/// which also keeps option-like values (`-x:1`) away from adb's argument parser.
fn parse_endpoint(raw: &str) -> Option<(String, u16)> {
    let raw = raw.trim();
    let (host, port, bracketed) = if let Some(rest) = raw.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        (host, tail.strip_prefix(':')?, true)
    } else {
        let (host, port) = raw.rsplit_once(':')?;
        (host, port, false)
    };
    if host.is_empty() || (host.contains(':') && !bracketed) {
        return None;
    }
    let valid_host = if bracketed {
        host.chars().all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.' | '%'))
    } else {
        host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            && !host.starts_with('-')
    };
    if !valid_host || port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let port: u16 = port.parse().ok().filter(|port| *port != 0)?;
    Some((host.to_string(), port))
}

fn normalize_endpoint(raw: &str) -> Result<String, String> {
    parse_endpoint(raw)
        .map(|(host, port)| format_endpoint(&host, port))
        .ok_or_else(|| ERR_INVALID_ENDPOINT.to_string())
}

/// Instance name of a browse result: `adb-XYZ._adb-tls-pairing._tcp.local.` -> `adb-XYZ`.
fn instance_name(fullname: &str, ty_domain: &str) -> Option<String> {
    let instance = fullname.strip_suffix(ty_domain)?.strip_suffix('.')?;
    if instance.is_empty() {
        None
    } else {
        Some(instance.replace("\\.", ".").replace("\\ ", " "))
    }
}

/// Picks the address most likely to be reachable from this computer: private LAN ranges first,
/// then CGNAT (VPN overlays), then anything routable, link-local last.
fn best_ipv4<'a>(addresses: impl IntoIterator<Item = &'a Ipv4Addr>) -> Option<Ipv4Addr> {
    let rank = |ip: &Ipv4Addr| {
        let octets = ip.octets();
        if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
            4
        } else if ip.is_link_local() {
            3
        } else if ip.is_private() {
            0
        } else if octets[0] == 100 && (64..=127).contains(&octets[1]) {
            1
        } else {
            2
        }
    };
    addresses
        .into_iter()
        .filter(|ip| rank(ip) < 4)
        .min_by_key(|ip| (rank(ip), **ip))
        .copied()
}

fn service_kind_from_type(raw: &str) -> Option<ServiceKind> {
    let normalized = raw
        .trim()
        .trim_end_matches('.')
        .trim_end_matches(".local")
        .trim_end_matches('.');
    match normalized {
        TYPE_PAIRING => Some(ServiceKind::Pairing),
        TYPE_CONNECT => Some(ServiceKind::Connect),
        _ => None,
    }
}

/// Parses one line of `adb mdns services`:
/// `<instance>\t<type>\t<host:port>` (tab separated; instance names may contain spaces, e.g. `adb-X (2)`).
fn parse_mdns_line(line: &str) -> Option<WirelessService> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() || line.starts_with("List of discovered") {
        return None;
    }

    let tab_fields: Vec<&str> = line.split('\t').map(str::trim).collect();
    let (instance, service_type, endpoint) = if tab_fields.len() >= 3 {
        (
            tab_fields[0].to_string(),
            tab_fields[1],
            tab_fields[2],
        )
    } else {
        // Space separated fallback: the type token always starts with `_adb`.
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let type_index = tokens.iter().position(|token| token.starts_with("_adb"))?;
        if type_index == 0 || type_index + 1 >= tokens.len() {
            return None;
        }
        (
            tokens[..type_index].join(" "),
            tokens[type_index],
            tokens[type_index + 1],
        )
    };

    let kind = service_kind_from_type(service_type)?;
    let (host, port) = parse_endpoint(endpoint)?;
    if instance.is_empty() {
        return None;
    }
    Some(WirelessService::new(kind, &instance, &host, port))
}

fn parse_mdns_services(output: &str) -> Vec<WirelessService> {
    output.lines().filter_map(parse_mdns_line).collect()
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let to = text[from..].find(end)? + from;
    Some(&text[from..to])
}

/// Interprets the output of `adb pair`. Returns the guid (may be empty) on success.
fn classify_pair_output(output: &str) -> Result<Option<String>, String> {
    let lower = output.to_ascii_lowercase();
    if lower.contains("successfully paired") {
        let guid = between(output, "[guid=", "]")
            .map(str::trim)
            .filter(|guid| !guid.is_empty())
            .map(str::to_string);
        return Ok(guid);
    }
    if lower.contains("wrong password") {
        return Err(ERR_WRONG_CODE.to_string());
    }
    // adb 37 answers `protocol fault (couldn't read status message)` when nothing listens on the
    // pairing port (the dialog was closed on the phone, or the port changed).
    if is_unreachable_message(&lower)
        || lower.contains("pairing client")
        || lower.contains("protocol fault")
    {
        return Err(ERR_UNREACHABLE.to_string());
    }
    Err(output.trim().to_string())
}

/// Interprets the output of `adb connect`.
fn classify_connect_output(output: &str) -> Result<String, String> {
    let lower = output.to_ascii_lowercase();
    if lower.contains("failed to authenticate") {
        return Err(ERR_NOT_PAIRED.to_string());
    }
    let failed = lower.contains("failed") || lower.contains("cannot") || lower.contains("unable");
    if !failed && lower.contains("connected to") {
        return Ok(output.trim().to_string());
    }
    if is_unreachable_message(&lower) {
        return Err(ERR_UNREACHABLE.to_string());
    }
    Err(output.trim().to_string())
}

/// Windows returns localized socket errors, but always with the numeric WSA code in parentheses.
fn is_unreachable_message(lower: &str) -> bool {
    [
        "connection refused",
        "no route to host",
        "timed out",
        "timeout",
        "unreachable",
        "host is down",
        "actively refused",
        "(10060)",
        "(10061)",
        "(10064)",
        "(10065)",
        "(10051)",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn escape_wifi_qr(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | ';' | ',' | ':' | '"') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn random_token(length: usize) -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect::<String>()
        .to_ascii_uppercase()
}

/// True for serials that come from wireless connections (`ip:port` or mDNS names).
pub fn is_wireless_serial(serial: &str) -> bool {
    serial.contains(':') || serial.contains("._tcp") || serial.starts_with("adb-")
}

// ---------------------------------------------------------------------------------------
// Publishing
// ---------------------------------------------------------------------------------------

fn pairing_key(service: &WirelessService) -> (String, u16) {
    (
        strip_conflict_suffix(&service.instance).to_ascii_lowercase(),
        service.port,
    )
}

/// Recomputes the merged list and, if it changed, notifies the frontend and async waiters.
fn publish(app: &AppHandle) {
    let state = app.state::<WirelessState>();
    let (snapshot, request_attention) = {
        let mut inner = state.lock();
        let merged = inner.registry.merged();
        if merged == inner.published {
            return;
        }
        let known: HashSet<(String, u16)> = inner
            .published
            .iter()
            .filter(|service| service.kind == ServiceKind::Pairing)
            .map(pairing_key)
            .collect();
        let new_request = merged
            .iter()
            .filter(|service| service.kind == ServiceKind::Pairing)
            .any(|service| !known.contains(&pairing_key(service)));
        inner.published = merged;
        let request_attention = new_request && inner.background && inner.foreground == 0;
        (
            WirelessSnapshot {
                services: inner.published.clone(),
                builtin_active: inner.daemon.is_some(),
                builtin_error: inner.builtin_error.clone(),
            },
            request_attention,
        )
    };
    state.version.send_modify(|version| *version = version.wrapping_add(1));
    let _ = app.emit(EVENT_SERVICES, &snapshot);
    if request_attention {
        flash_window(app);
    }
}

/// Emits the current snapshot even when the service list did not change (status updates).
fn emit_snapshot(app: &AppHandle) {
    let snapshot = app.state::<WirelessState>().snapshot();
    let _ = app.emit(EVENT_SERVICES, &snapshot);
}

/// Draws attention (taskbar flash) when a phone asks to pair while the window is in the background.
fn flash_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if !window.is_focused().unwrap_or(true) {
            let _ = window.request_user_attention(Some(tauri::UserAttentionType::Informational));
        }
    }
}

// ---------------------------------------------------------------------------------------
// Built-in mDNS browser
// ---------------------------------------------------------------------------------------

fn start_builtin(app: &AppHandle) {
    let state = app.state::<WirelessState>();
    let mut inner = state.lock();
    if inner.daemon.is_some() {
        return;
    }

    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(error) => {
            inner.builtin_error = Some(error.to_string());
            drop(inner);
            emit_snapshot(app);
            return;
        }
    };
    // adb pairing/connection is IPv4 in practice; skipping IPv6 halves the traffic and avoids
    // link-local scope ids that adb cannot use anyway.
    let _ = daemon.disable_interface(IfKind::IPv6);
    inner.builtin_error = None;
    inner.generation = inner.generation.wrapping_add(1);
    let generation = inner.generation;
    inner.registry.builtin.clear();

    if let Ok(monitor) = daemon.monitor() {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            while let Ok(event) = monitor.recv_async().await {
                if let DaemonEvent::Error(error) = event {
                    app.state::<WirelessState>().lock().builtin_error = Some(error.to_string());
                }
            }
        });
    }

    for kind in [ServiceKind::Pairing, ServiceKind::Connect] {
        match daemon.browse(&format!("{}.local.", kind.service_type())) {
            Ok(events) => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    while let Ok(event) = events.recv_async().await {
                        handle_service_event(&app, kind, generation, event);
                    }
                });
            }
            Err(error) => inner.builtin_error = Some(error.to_string()),
        }
    }
    inner.daemon = Some(daemon);
    drop(inner);
    emit_snapshot(app);
}

fn stop_builtin(app: &AppHandle) {
    let state = app.state::<WirelessState>();
    let daemon = {
        let mut inner = state.lock();
        inner.registry.builtin.clear();
        inner.generation = inner.generation.wrapping_add(1);
        inner.daemon.take()
    };
    if let Some(daemon) = daemon {
        let _ = daemon.shutdown();
    }
    publish(app);
    emit_snapshot(app);
}

fn handle_service_event(app: &AppHandle, kind: ServiceKind, generation: u64, event: ServiceEvent) {
    let state = app.state::<WirelessState>();
    match event {
        ServiceEvent::ServiceResolved(info) => {
            let Some(instance) = instance_name(&info.fullname, &info.ty_domain) else {
                return;
            };
            let Some(host) = best_ipv4(&info.get_addresses_v4()) else {
                return;
            };
            let service = WirelessService::new(kind, &instance, &host.to_string(), info.port);
            {
                let mut inner = state.lock();
                if inner.generation != generation {
                    return;
                }
                inner
                    .registry
                    .builtin
                    .insert(info.fullname.to_ascii_lowercase(), service);
            }
            publish(app);
        }
        ServiceEvent::ServiceRemoved(_, fullname) => {
            let removed = {
                let mut inner = state.lock();
                inner.generation == generation
                    && inner
                        .registry
                        .builtin
                        .remove(&fullname.to_ascii_lowercase())
                        .is_some()
            };
            if removed {
                publish(app);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------
// adb based discovery (fallback / complement)
// ---------------------------------------------------------------------------------------

fn start_poller(app: &AppHandle, inner: &mut Inner) {
    if inner.poller.is_some() {
        return;
    }
    let app = app.clone();
    inner.poller = Some(tauri::async_runtime::spawn(async move {
        loop {
            let delay = poll_adb_once(&app).await;
            tokio::time::sleep(delay).await;
        }
    }));
}

async fn poll_adb_once(app: &AppHandle) -> Duration {
    let listed = tokio::time::timeout(
        Duration::from_secs(6),
        adb::run_adb(&["mdns", "services"]),
    )
    .await;
    let (services, delay) = match listed {
        Ok(Ok(result)) if result.ok() => (parse_mdns_services(&result.output), ADB_POLL_INTERVAL),
        // adb missing, mDNS unavailable in this adb build, or the call timed out.
        _ => (Vec::new(), ADB_POLL_FAILURE_INTERVAL),
    };
    app.state::<WirelessState>().lock().registry.adb = services;
    publish(app);
    delay
}

// ---------------------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------------------

/// Registers the state and, when the "detect pairing requests" setting is on, starts the
/// always-on detector shortly after startup.
pub fn init(app: &AppHandle, background: bool) {
    app.manage(WirelessState::new());
    if background {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(BACKGROUND_START_DELAY).await;
            set_background(&app, true);
        });
    }
}

pub fn set_background(app: &AppHandle, enabled: bool) {
    let state = app.state::<WirelessState>();
    let stop = {
        let mut inner = state.lock();
        inner.background = enabled;
        !enabled && inner.foreground == 0
    };
    if enabled {
        start_builtin(app);
    } else if stop {
        stop_builtin(app);
    }
}

fn acquire(app: &AppHandle) {
    {
        let state = app.state::<WirelessState>();
        let mut inner = state.lock();
        inner.foreground += 1;
        start_poller(app, &mut inner);
    }
    start_builtin(app);
}

fn release(app: &AppHandle) {
    let state = app.state::<WirelessState>();
    let stop = {
        let mut inner = state.lock();
        inner.foreground = inner.foreground.saturating_sub(1);
        if inner.foreground == 0 {
            if let Some(poller) = inner.poller.take() {
                poller.abort();
            }
            inner.registry.adb.clear();
        }
        inner.foreground == 0 && !inner.background
    };
    if stop {
        stop_builtin(app);
    } else {
        publish(app);
    }
}

/// Keeps discovery running for as long as it lives (also when the owning task is aborted).
struct ForegroundGuard(AppHandle);

impl ForegroundGuard {
    fn new(app: &AppHandle) -> Self {
        acquire(app);
        Self(app.clone())
    }
}

impl Drop for ForegroundGuard {
    fn drop(&mut self) {
        release(&self.0);
    }
}

// ---------------------------------------------------------------------------------------
// Discovery commands
// ---------------------------------------------------------------------------------------

#[tauri::command]
pub async fn wireless_discovery_acquire(
    app: AppHandle,
    state: State<'_, WirelessState>,
) -> Result<WirelessSnapshot, String> {
    acquire(&app);
    Ok(state.snapshot())
}

#[tauri::command]
pub async fn wireless_discovery_release(app: AppHandle) -> Result<(), String> {
    release(&app);
    Ok(())
}

#[tauri::command]
pub async fn wireless_discovery_snapshot(
    state: State<'_, WirelessState>,
) -> Result<WirelessSnapshot, String> {
    Ok(state.snapshot())
}

// ---------------------------------------------------------------------------------------
// Connect / disconnect
// ---------------------------------------------------------------------------------------

async fn run_adb_timeout(args: &[&str], timeout: Duration) -> Result<adb::AdbResult, String> {
    match tokio::time::timeout(timeout, adb::run_adb(args)).await {
        Ok(result) => result,
        Err(_) => Err(ERR_UNREACHABLE.to_string()),
    }
}

async fn connect_endpoint(endpoint: &str) -> Result<String, String> {
    connect_endpoint_within(endpoint, CONNECT_TIMEOUT).await
}

async fn connect_endpoint_within(endpoint: &str, timeout: Duration) -> Result<String, String> {
    let result = run_adb_timeout(&["connect", endpoint], timeout).await?;
    classify_connect_output(&result.output)
}

#[tauri::command]
pub async fn connect_wireless_device(endpoint: String) -> Result<String, String> {
    let endpoint = normalize_endpoint(&endpoint)?;
    connect_endpoint(&endpoint).await
}

#[tauri::command]
pub async fn disconnect_wireless_device(
    state: State<'_, WirelessState>,
    endpoint: String,
) -> Result<String, String> {
    let target = endpoint.trim();
    if target.is_empty() || target.starts_with('-') {
        return Err("Please enter a valid device serial".to_string());
    }
    let first = adb::run_adb(&["disconnect", target]).await?;
    if first.ok() && !first.output.to_ascii_lowercase().contains("error") {
        return Ok(first.output.trim().to_string());
    }

    // Devices connected through mDNS are named `<instance>._adb-tls-connect._tcp`, which
    // `adb disconnect` cannot resolve: retry with the `ip:port` of that advertisement.
    if let Some(instance) = target.strip_suffix(&format!(".{TYPE_CONNECT}")) {
        let endpoint = {
            let inner = state.lock();
            inner
                .registry
                .merged()
                .into_iter()
                .find(|service| service.kind == ServiceKind::Connect && service.is_instance(instance))
                .map(|service| service.endpoint)
        };
        if let Some(endpoint) = endpoint {
            let retry = adb::run_adb(&["disconnect", &endpoint]).await?;
            if retry.ok() && !retry.output.to_ascii_lowercase().contains("error") {
                return Ok(retry.output.trim().to_string());
            }
        }
    }
    Err(first.output.trim().to_string())
}

// ---------------------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------------------

/// Serial of the connected device that belongs to the phone that was just paired.
async fn connected_serial_for(host: &str, guid: Option<&str>) -> Option<String> {
    let listing = adb::run_adb(&["devices", "-l"]).await.ok()?;
    if !listing.ok() {
        return None;
    }
    // `offline`/`connecting` transports (stale `adb tcpip` entries, a TLS handshake in progress)
    // are not a finished connection.
    device_parser::parse_devices(&listing.stdout)
        .into_iter()
        .filter(|device| device.state == "device")
        .map(|device| device.serial)
        .find(|serial| {
            let mdns_match = guid.is_some_and(|guid| {
                serial
                    .strip_suffix(&format!(".{TYPE_CONNECT}"))
                    .is_some_and(|instance| strip_conflict_suffix(instance).eq_ignore_ascii_case(guid))
            });
            mdns_match || serial.starts_with(&format!("{host}:"))
        })
}

fn connect_service_for(app: &AppHandle, host: &str, guid: Option<&str>) -> Option<WirelessService> {
    let services = app.state::<WirelessState>().lock().registry.merged();
    let connect = || services.iter().filter(|service| service.kind == ServiceKind::Connect);
    guid.and_then(|guid| connect().find(|service| service.is_instance(guid)))
        .or_else(|| connect().find(|service| service.host == host))
        .cloned()
}

/// After a successful pairing: wait for adb's own auto-connect and, if it does not happen,
/// connect to the phone's `_adb-tls-connect._tcp` port. Returns the serial when connected.
async fn connect_after_pairing(app: &AppHandle, host: &str, guid: Option<&str>) -> Option<String> {
    let started = tokio::time::Instant::now();
    let mut attempted: HashMap<String, u8> = HashMap::new();
    let mut last_attempt: Option<tokio::time::Instant> = None;

    while started.elapsed() < POST_PAIR_CONNECT_WINDOW {
        if let Some(serial) = connected_serial_for(host, guid).await {
            return Some(serial);
        }
        if started.elapsed() >= AUTO_CONNECT_GRACE {
            let retry_ready = last_attempt.map_or(true, |at| at.elapsed() >= Duration::from_secs(2));
            if let (true, Some(service)) = (retry_ready, connect_service_for(app, host, guid)) {
                let tries = attempted.entry(service.endpoint.clone()).or_insert(0);
                if *tries < 4 {
                    *tries += 1;
                    last_attempt = Some(tokio::time::Instant::now());
                    if connect_endpoint(&service.endpoint).await.is_ok() {
                        return Some(service.endpoint);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    None
}

/// Shared by the manual pairing code flow and the QR flow.
async fn pair_and_connect(
    app: &AppHandle,
    endpoint: &str,
    code: &str,
    progress: impl Fn(&'static str),
) -> Result<PairOutcome, String> {
    let endpoint = normalize_endpoint(endpoint)?;
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.is_empty()
        || code.len() > 64
        || code.starts_with('-')
        || !code.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Err(ERR_INVALID_CODE.to_string());
    }

    progress("pairing");
    let result = run_adb_timeout(&["pair", &endpoint, &code], PAIR_TIMEOUT).await?;
    let guid = classify_pair_output(&result.output)?;

    progress("connecting");
    let host = parse_endpoint(&endpoint).map(|(host, _)| host).unwrap_or_default();
    let serial = connect_after_pairing(app, &host, guid.as_deref()).await;
    Ok(PairOutcome {
        message: result.output.trim().to_string(),
        guid,
        connected: serial.is_some(),
        serial,
    })
}

/// Pairs with the phone that is showing `code` at `endpoint`, then connects to it.
/// When `session_id` is given, progress is reported through `wireless-pair-status` events.
#[tauri::command]
pub async fn pair_wireless_device(
    app: AppHandle,
    endpoint: String,
    code: String,
    session_id: Option<String>,
) -> Result<PairOutcome, String> {
    let report = |status: &'static str| {
        if let Some(session_id) = &session_id {
            emit_pair(&app, pair_status(session_id, status));
        }
    };
    pair_and_connect(&app, &endpoint, &code, report).await
}

// ---------------------------------------------------------------------------------------
// QR pairing
// ---------------------------------------------------------------------------------------

fn emit_pair(app: &AppHandle, status: PairStatus) {
    let _ = app.emit(EVENT_PAIR, status);
}

fn pair_status(session_id: &str, status: &'static str) -> PairStatus {
    PairStatus {
        session_id: session_id.to_string(),
        status,
        label: None,
        endpoint: None,
        serial: None,
        connected: false,
        error: None,
    }
}

async fn run_qr_session(app: AppHandle, session_id: String, service_name: String, password: String) {
    let _guard = ForegroundGuard::new(&app);
    emit_pair(&app, pair_status(&session_id, "waiting"));

    // Subscribe before looking so a service that appears in between is not missed.
    let mut changes = app.state::<WirelessState>().version.subscribe();
    let deadline = tokio::time::Instant::now() + QR_TIMEOUT;
    let service = loop {
        let found = app
            .state::<WirelessState>()
            .lock()
            .registry
            .merged()
            .into_iter()
            .find(|service| service.kind == ServiceKind::Pairing && service.is_instance(&service_name));
        if let Some(service) = found {
            break service;
        }
        match tokio::time::timeout_at(deadline, changes.changed()).await {
            Ok(Ok(())) => {}
            _ => {
                let mut status = pair_status(&session_id, "error");
                status.error = Some(ERR_QR_TIMEOUT.to_string());
                emit_pair(&app, status);
                return;
            }
        }
    };

    let mut found = pair_status(&session_id, "found");
    found.label = Some(service.label.clone());
    found.endpoint = Some(service.endpoint.clone());
    emit_pair(&app, found);

    let report = |status: &'static str| emit_pair(&app, pair_status(&session_id, status));
    let mut attempt = 0;
    let outcome = loop {
        attempt += 1;
        let attempt_started = tokio::time::Instant::now();
        match pair_and_connect(&app, &service.endpoint, &password, &report).await {
            Err(error)
                if error == ERR_UNREACHABLE
                    && attempt < QR_PAIR_ATTEMPTS
                    && attempt_started.elapsed() < QR_PAIR_RETRY_MAX_ATTEMPT_TIME =>
            {
                tokio::time::sleep(QR_PAIR_RETRY_DELAY).await;
            }
            other => break other,
        }
    };
    match outcome {
        Ok(outcome) => {
            let mut done = pair_status(&session_id, "done");
            done.label = Some(service.label);
            done.endpoint = Some(service.endpoint);
            done.connected = outcome.connected;
            done.serial = outcome.serial;
            emit_pair(&app, done);
        }
        Err(error) => {
            let mut failed = pair_status(&session_id, "error");
            failed.error = Some(error);
            emit_pair(&app, failed);
        }
    }
}

/// Starts a QR pairing session: returns the payload to render as a QR code and waits in the
/// background for a phone to scan it, reporting progress through `wireless-qr-status` events.
#[tauri::command]
pub async fn start_wireless_qr(
    app: AppHandle,
    state: State<'_, WirelessState>,
) -> Result<WirelessQr, String> {
    // Same layout as Android Studio: `WIFI:T:ADB;S:<service name>;P:<password>;;`
    let service_name = format!("adb-{}", random_token(8));
    let password = random_token(12);
    let session_id = random_token(10);
    let qr_data = format!(
        "WIFI:T:ADB;S:{};P:{};;",
        escape_wifi_qr(&service_name),
        escape_wifi_qr(&password)
    );

    let task = tauri::async_runtime::spawn(run_qr_session(
        app.clone(),
        session_id.clone(),
        service_name.clone(),
        password,
    ));
    let previous = state.lock().qr.replace(QrHandle {
        session_id: session_id.clone(),
        task,
    });
    if let Some(previous) = previous {
        previous.task.abort();
    }

    Ok(WirelessQr {
        session_id,
        service_name,
        qr_data,
    })
}

#[tauri::command]
pub async fn cancel_wireless_qr(
    state: State<'_, WirelessState>,
    session_id: String,
) -> Result<(), String> {
    let handle = {
        let mut inner = state.lock();
        match inner.qr.take() {
            Some(handle) if handle.session_id == session_id => Some(handle),
            other => {
                inner.qr = other;
                None
            }
        }
    };
    if let Some(handle) = handle {
        handle.task.abort();
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// USB -> Wi-Fi (adb tcpip)
// ---------------------------------------------------------------------------------------

async fn wireless_host_for_serial(serial: &str) -> Result<String, String> {
    let queries = [
        (vec!["shell", "ip", "route", "show", "dev", "wlan0"], "src "),
        (vec!["shell", "ip", "route"], "src "),
        (
            vec!["shell", "ip", "-f", "inet", "addr", "show", "wlan0"],
            "inet ",
        ),
    ];

    for (args, keyword) in queries {
        let result = adb::run_adb_for_serial(serial, &args).await?;
        if result.ok() {
            for line in result.output.lines() {
                if let Some(idx) = line.find(keyword) {
                    let rest = line[idx + keyword.len()..].trim_start();
                    let ip_candidate = rest.split_whitespace().next().unwrap_or("");
                    let ip_candidate = ip_candidate.split('/').next().unwrap_or("");
                    if ip_candidate.parse::<Ipv4Addr>().is_ok() {
                        return Ok(ip_candidate.to_string());
                    }
                }
            }
        }
    }
    let property =
        adb::run_adb_for_serial(serial, &["shell", "getprop", "dhcp.wlan0.ipaddress"]).await?;
    let host = property.output.trim();
    if property.ok() && host.parse::<Ipv4Addr>().is_ok() {
        Ok(host.to_string())
    } else {
        Err("Could not detect the Wi-Fi IP of the device".to_string())
    }
}

#[tauri::command]
pub async fn connect_usb_over_tcpip(serial: String) -> Result<String, String> {
    if is_wireless_serial(&serial) || serial.starts_with("emulator-") {
        return Err("Select a physically connected USB device".to_string());
    }
    let host = wireless_host_for_serial(&serial).await?;
    let tcpip = adb::run_adb_for_serial(&serial, &["tcpip", "5555"]).await?;
    if !tcpip.ok() || tcpip.output.to_ascii_lowercase().contains("failed") {
        return Err(tcpip.output.trim().to_string());
    }

    // adbd restarts in TCP mode; it needs a moment before it accepts connections.
    let endpoint = format!("{host}:5555");
    let mut last_error = String::new();
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(900)).await;
        match connect_endpoint_within(&endpoint, TCPIP_CONNECT_ATTEMPT_TIMEOUT).await {
            Ok(_) => return Ok(endpoint),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_payload_matches_the_android_wifi_adb_format() {
        let name = format!("adb-{}", random_token(8));
        let password = random_token(12);
        let payload = format!(
            "WIFI:T:ADB;S:{};P:{};;",
            escape_wifi_qr(&name),
            escape_wifi_qr(&password)
        );
        assert!(payload.starts_with("WIFI:T:ADB;S:adb-"));
        assert!(payload.ends_with(";;"));
        assert_eq!(password.len(), 12);
        assert!(password.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(payload.matches(';').count(), 4);
    }

    #[test]
    fn qr_values_escape_wifi_special_characters() {
        assert_eq!(escape_wifi_qr(r#"a;b,c:d\e"f"#), r#"a\;b\,c\:d\\e\"f"#);
    }

    #[test]
    fn parses_tab_separated_mdns_services() {
        let output = "List of discovered mdns services\n\
            adb-R58M12ABC-AbCdEf\t_adb-tls-connect._tcp\t192.168.15.20:37411\n\
            adb-R58M12ABC-AbCdEf\t_adb-tls-pairing._tcp\t192.168.15.20:41123\n\
            studio-ABCDEFGHIJ\t_adb._tcp\t192.168.15.30:5555\n";
        let services = parse_mdns_services(output);
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].kind, ServiceKind::Connect);
        assert_eq!(services[0].endpoint, "192.168.15.20:37411");
        assert_eq!(services[1].kind, ServiceKind::Pairing);
        assert_eq!(services[1].label, "R58M12ABC");
        assert_eq!(services[1].port, 41123);
    }

    #[test]
    fn parses_mdns_services_with_trailing_dot_types_and_conflict_suffix() {
        let output = "adb-XYZ-abcdef (2)\t_adb-tls-pairing._tcp.\t10.0.0.5:4000\r\n\
            adb-XYZ-abcdef  _adb-tls-connect._tcp.local.  10.0.0.5:5000\r\n";
        let services = parse_mdns_services(output);
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].instance, "adb-XYZ-abcdef (2)");
        assert_eq!(services[0].label, "XYZ");
        assert_eq!(services[0].kind, ServiceKind::Pairing);
        assert_eq!(services[1].kind, ServiceKind::Connect);
        assert_eq!(services[1].endpoint, "10.0.0.5:5000");
    }

    #[test]
    fn space_separated_fallback_keeps_names_with_spaces() {
        let service = parse_mdns_line("adb-XYZ-abcdef (2) _adb-tls-pairing._tcp 10.0.0.5:4000")
            .expect("line should parse");
        assert_eq!(service.instance, "adb-XYZ-abcdef (2)");
        assert_eq!(service.port, 4000);
    }

    #[test]
    fn ignores_garbage_mdns_lines() {
        assert!(parse_mdns_line("").is_none());
        assert!(parse_mdns_line("List of discovered mdns services").is_none());
        assert!(parse_mdns_line("ERROR: mdns discovery unavailable").is_none());
        assert!(parse_mdns_line("name\t_adb-tls-pairing._tcp\tnot-an-endpoint").is_none());
    }

    #[test]
    fn validates_endpoints() {
        assert_eq!(parse_endpoint(" 192.168.1.5:41123 "), Some(("192.168.1.5".into(), 41123)));
        assert_eq!(parse_endpoint("[fe80::1]:5555"), Some(("fe80::1".into(), 5555)));
        assert_eq!(parse_endpoint("phone.local:5555"), Some(("phone.local".into(), 5555)));
        assert!(parse_endpoint("192.168.1.5").is_none());
        assert!(parse_endpoint("192.168.1.5:0").is_none());
        assert!(parse_endpoint("192.168.1.5:70000").is_none());
        assert!(parse_endpoint("fe80::1:5555").is_none());
        assert!(parse_endpoint("-x:5555").is_none());
        assert!(parse_endpoint(":5555").is_none());
        assert_eq!(normalize_endpoint("nonsense"), Err(ERR_INVALID_ENDPOINT.to_string()));
    }

    #[test]
    fn formats_ipv6_endpoints_with_brackets() {
        assert_eq!(format_endpoint("fe80::1", 5555), "[fe80::1]:5555");
        assert_eq!(format_endpoint("10.0.0.2", 5555), "10.0.0.2:5555");
    }

    #[test]
    fn derives_friendly_labels() {
        assert_eq!(friendly_label("adb-R58M12ABC-AbCdEf"), "R58M12ABC");
        assert_eq!(friendly_label("adb-R58M12ABC-AbCdEf (3)"), "R58M12ABC");
        assert_eq!(friendly_label("adb-ABCD1234"), "ABCD1234");
        assert_eq!(friendly_label("studio-XYZ"), "studio-XYZ");
    }

    #[test]
    fn extracts_instance_from_full_name() {
        assert_eq!(
            instance_name("adb-XYZ._adb-tls-pairing._tcp.local.", "_adb-tls-pairing._tcp.local.").as_deref(),
            Some("adb-XYZ")
        );
        assert_eq!(
            instance_name("adb-X\\.Y._adb-tls-pairing._tcp.local.", "_adb-tls-pairing._tcp.local.").as_deref(),
            Some("adb-X.Y")
        );
        assert_eq!(instance_name("._adb-tls-pairing._tcp.local.", "_adb-tls-pairing._tcp.local."), None);
    }

    #[test]
    fn prefers_lan_addresses_over_vpn_and_link_local() {
        let addresses = [
            Ipv4Addr::new(169, 254, 3, 3),
            Ipv4Addr::new(100, 100, 1, 1),
            Ipv4Addr::new(192, 168, 15, 20),
            Ipv4Addr::new(8, 8, 8, 8),
        ];
        assert_eq!(best_ipv4(&addresses), Some(Ipv4Addr::new(192, 168, 15, 20)));
        assert_eq!(best_ipv4(&[Ipv4Addr::new(127, 0, 0, 1)]), None);
        assert_eq!(best_ipv4(&[Ipv4Addr::new(169, 254, 3, 3)]), Some(Ipv4Addr::new(169, 254, 3, 3)));
    }

    #[test]
    fn classifies_pair_output() {
        let ok = "Successfully paired to 192.168.15.20:41123 [guid=adb-R58M12ABC-AbCdEf]";
        assert_eq!(classify_pair_output(ok), Ok(Some("adb-R58M12ABC-AbCdEf".to_string())));
        assert_eq!(classify_pair_output("Successfully paired to 1.2.3.4:5 "), Ok(None));
        assert_eq!(
            classify_pair_output("Failed: Wrong password or connection was dropped."),
            Err(ERR_WRONG_CODE.to_string())
        );
        assert_eq!(
            classify_pair_output("Failed: Unable to start pairing client."),
            Err(ERR_UNREACHABLE.to_string())
        );
        assert_eq!(
            classify_pair_output("error: protocol fault (couldn't read status message): No error"),
            Err(ERR_UNREACHABLE.to_string())
        );
        assert_eq!(classify_pair_output("something odd"), Err("something odd".to_string()));
    }

    #[test]
    fn classifies_connect_output() {
        assert!(classify_connect_output("connected to 192.168.1.5:5555").is_ok());
        assert!(classify_connect_output("already connected to 192.168.1.5:5555").is_ok());
        assert_eq!(
            classify_connect_output("failed to authenticate to 192.168.1.5:37000"),
            Err(ERR_NOT_PAIRED.to_string())
        );
        assert_eq!(
            classify_connect_output("cannot connect to 192.168.1.5:5555: Connection refused (10061)"),
            Err(ERR_UNREACHABLE.to_string())
        );
        assert_eq!(
            classify_connect_output("failed to connect to '192.168.1.5:5555': Connection timed out"),
            Err(ERR_UNREACHABLE.to_string())
        );
    }

    #[test]
    fn registry_prefers_builtin_and_dedupes_conflict_names() {
        let mut registry = Registry::default();
        registry.builtin.insert(
            "a".into(),
            WirelessService::new(ServiceKind::Pairing, "adb-X-abcdef", "192.168.1.5", 4000),
        );
        registry.adb = vec![
            WirelessService::new(ServiceKind::Pairing, "adb-X-abcdef (2)", "10.0.0.9", 4000),
            WirelessService::new(ServiceKind::Connect, "adb-X-abcdef", "192.168.1.5", 5000),
        ];
        let merged = registry.merged();
        assert_eq!(merged.len(), 2);
        // Pairing requests are listed first; the built-in browser wins over the adb poll.
        assert_eq!(merged[0].kind, ServiceKind::Pairing);
        assert_eq!(merged[0].endpoint, "192.168.1.5:4000");
        assert_eq!(merged[1].kind, ServiceKind::Connect);
    }

    #[test]
    fn detects_wireless_serials() {
        assert!(is_wireless_serial("192.168.1.5:5555"));
        assert!(is_wireless_serial("adb-R58M12ABC-AbCdEf._adb-tls-connect._tcp"));
        assert!(!is_wireless_serial("R58M12ABC"));
        assert!(!is_wireless_serial("emulator-5554"));
    }
}
