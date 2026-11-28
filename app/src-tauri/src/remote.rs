//! "Your Macs" — use another Mac's model, or share this one (v0.6 M4,
//! docs/plans/2026-11-28-use-your-other-mac.md §4–§6).
//!
//! Runs the p2p-forward share/use runtimes **in-process** (`p2p_node::session`), on a small
//! tokio runtime owned by [`RemoteState`]:
//!
//! - **Use another Mac** (MacBook): the forwarder listens on `127.0.0.1:8080` + `[::1]:8080`
//!   with this install's bearer token, so editors keep `http://127.0.0.1:8080/v1`; the local
//!   model is stopped first (after the window asked). The choice persists: at the next app start
//!   the forwarder comes back by itself (0 clicks), and re-dials while the big Mac sleeps.
//! - **Share this Mac** (big Mac): paired Macs reach this Mac's llama-server through the
//!   allowlisted forwarder; a 6-digit code (5 minutes) pairs a new Mac; "Paired Macs may start
//!   and stop the model" lets them start the model through the app's own `start_server` path
//!   (same RAM admission) and stop only a model they started, never while a local request runs.
//!   A `caffeinate -i -w <pid>` child keeps the Mac awake while sharing on AC power (and during
//!   a remote request on battery).
//! - mDNS browse/announce start only after a user click (macOS Local Network prompt); later
//!   app starts resume what the user turned on.
//!
//! API key decision: the Connect-editor patch always writes this install's local token
//! (`<p2p dir>/local.token`) as the provider's API key. llama-server ignores the key, so the
//! same editor config works locally and through the forwarder — switching never needs a
//! re-patch.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use p2p_forward::{ModelControl, PeerId, ReplyFuture, Status, StatusReply, StatusRequest};
use p2p_node::session::{LanBrowser, Node, ShareOpts, ShareSession, UseOpts, UseSession, UseState};
use p2p_node::trust::PeerRole;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::State;

use crate::slotactivity::{self, SlotActivity};
use crate::ui_err;

/// True while this Mac uses another Mac (the forwarder owns port 8080). Read by
/// `server_state`, `is_running`, `stop_server_impl` and the tray without any lock.
pub static USE_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn using() -> bool {
    USE_ACTIVE.load(Ordering::SeqCst)
}

/// The app window's origin (WKWebView, Tauri 2 on macOS).
pub const APP_ORIGIN: &str = "tauri://localhost";

/// `SLIPSTREAM_P2P_DIR`, else `~/Library/Application Support/Slipstream/p2p`.
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SLIPSTREAM_P2P_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/var/empty".into());
    PathBuf::from(home).join("Library/Application Support/Slipstream/p2p")
}

/// This install's local API key (created 0600 on first use).
pub fn api_key_in(dir: &std::path::Path) -> Result<String, String> {
    p2p_forward::load_or_create_token(&dir.join("local.token")).map_err(|e| e.to_string())
}

pub fn api_key() -> Result<String, String> {
    api_key_in(&data_dir())
}

/// What the big Mac's app does for a paired Mac's StartModel / StopModel (main.rs implements
/// it with `start_server_impl` / `stop_server_impl`; tests use a fake).
pub trait AppModel: Send + Sync + 'static {
    /// PID of this app's model child (llama-server / oMLX launcher), if one runs. The share
    /// side forwards only to a listener owned by it or a descendant (security review M3).
    fn server_pid(&self) -> Option<u32> {
        None
    }
    /// The local model process runs (loading or ready).
    fn running(&self) -> bool;
    /// Start with the last configuration (through the RAM admission of `start_server`).
    fn start(&self) -> Result<String, String>;
    fn stop(&self);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    /// PeerId of the Mac this one uses (persists: 0 clicks at the next start).
    #[serde(default)]
    use_peer: Option<String>,
    #[serde(default)]
    share: bool,
    #[serde(default = "yes")]
    may_start_model: bool,
    /// Security review L3: a paired Mac may start this Mac's model while it runs on battery
    /// only if the owner opted in (default off).
    #[serde(default)]
    start_on_battery: bool,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            use_peer: None,
            share: false,
            may_start_model: true,
            start_on_battery: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteConfig {
    pub dir: PathBuf,
    /// Share listen multiaddr; `None` = /ip4/0.0.0.0/udp/9003/quic-v1 (free-port fallback).
    pub share_listen: Option<String>,
    /// Where the forwarder listens for editors.
    pub use_bind: SocketAddr,
    /// This Mac's llama-server (what a share forwards to).
    pub upstream: SocketAddr,
    pub announce: bool,
    pub discovery: bool,
    pub app_origin: Option<String>,
    /// Keep the Mac awake while sharing (off in tests).
    pub caffeinate: bool,
    /// Wake-on-LAN targets (tests: a loopback socket, never the LAN).
    pub wake: p2p_node::wake::WakeTargets,
    /// Shorter waking phase (tests); `None` = 20 s.
    pub wake_window: Option<Duration>,
    /// Power source probe (tests fake "on battery"; L3).
    pub on_battery: fn() -> bool,
}

impl RemoteConfig {
    pub fn from_env() -> Self {
        let port = crate::SERVER_PORT;
        Self {
            dir: data_dir(),
            share_listen: std::env::var("SLIPSTREAM_SHARE_LISTEN").ok().filter(|s| !s.is_empty()),
            use_bind: std::env::var("SLIPSTREAM_REMOTE_BIND")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], port))),
            upstream: SocketAddr::from(([127, 0, 0, 1], port)),
            announce: true,
            discovery: true,
            app_origin: Some(APP_ORIGIN.into()),
            caffeinate: true,
            wake: p2p_node::wake::WakeTargets::Broadcast,
            wake_window: None,
            on_battery,
        }
    }
}

#[derive(Default)]
struct Inner {
    node: Option<Arc<Node>>,
    share: Option<Arc<ShareSession>>,
    use_: Option<Arc<UseSession>>,
    browser: Option<LanBrowser>,
    caffeinate: Option<Child>,
    /// Name of the Mac that removed this one (shown until the user dismisses it).
    removed_by: Option<String>,
    /// Last status of the used Mac (status RPC every 3 s) and its slot activity.
    use_status: Option<Status>,
    use_activity: Option<SlotActivity>,
    monitors: Vec<tokio::task::JoinHandle<()>>,
    /// M5 (item 8): interface / power info for remote_status, refreshed at most every
    /// [`NET_CACHE_TTL`] or when the use state changes (a network change shows up there).
    net: Option<NetInfo>,
    /// Use state the cache was taken in.
    net_state: Option<String>,
}

/// Cached interface / power facts (ifconfig + pmset are not run per poll).
#[derive(Debug, Clone)]
struct NetInfo {
    at: std::time::Instant,
    lan_ips: Vec<String>,
    on_battery: bool,
}

pub const NET_CACHE_TTL: Duration = Duration::from_secs(10);

/// Keep the Mac awake? (M5 battery rules, plan §4): while sharing on AC power always; on
/// battery only while a paired Mac's request is in flight or queued (sharing on battery is
/// allowed — the window shows a warning — but the Mac may sleep when idle).
pub fn want_awake(on_battery: bool, active_requests: usize) -> bool {
    !on_battery || active_requests > 0
}

pub struct Core {
    cfg: RemoteConfig,
    inner: Mutex<Inner>,
    settings: Mutex<Settings>,
    model: Mutex<Option<Arc<dyn AppModel>>>,
    /// The paired Mac that started the running model (L3: only that one may stop it).
    remote_started: Mutex<Option<PeerId>>,
}

pub struct RemoteState {
    rt: tokio::runtime::Runtime,
    pub core: Arc<Core>,
}

impl Drop for Core {
    fn drop(&mut self) {
        if let Ok(mut i) = self.inner.lock() {
            if let Some(mut c) = i.caffeinate.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

fn settings_path(dir: &std::path::Path) -> PathBuf {
    dir.join("app_remote.json")
}

fn err_text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl RemoteState {
    pub fn new(cfg: RemoteConfig) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("slipstream-remote")
            .enable_all()
            .build()
            .expect("tokio runtime for Your Macs");
        let settings = std::fs::read(settings_path(&cfg.dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            rt,
            core: Arc::new(Core {
                cfg,
                inner: Mutex::default(),
                settings: Mutex::new(settings),
                model: Mutex::new(None),
                remote_started: Mutex::new(None),
            }),
        }
    }

    pub fn from_env() -> Self {
        Self::new(RemoteConfig::from_env())
    }

    /// Run `f(core)` on the remote runtime and wait for it (async commands).
    pub async fn run<T, F, Fut>(&self, f: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Core>) -> Fut,
        Fut: std::future::Future<Output = Result<T, String>> + Send + 'static,
    {
        self.rt.spawn(f(self.core.clone())).await.map_err(err_text)?
    }

    /// Blocking variant (setup, tests).
    pub fn block<T, F, Fut>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce(Arc<Core>) -> Fut,
        Fut: std::future::Future<Output = Result<T, String>>,
    {
        self.rt.block_on(f(self.core.clone()))
    }

    pub fn set_model(&self, model: Arc<dyn AppModel>) {
        *self.core.model.lock().unwrap() = Some(model);
    }

    /// A local Start / Stop from the window: the model is no longer "remotely started".
    pub fn local_lifecycle(&self) {
        *self.core.remote_started.lock().unwrap() = None;
    }

    /// "Regenerate key" (review M2): a new local token; a running use session is restarted
    /// so the forwarder requires the new one at once. Returns the new key.
    pub async fn regenerate_key(&self) -> Result<String, String> {
        self.run(|c| async move {
            let path = c.cfg.dir.join("local.token");
            let key = tokio::task::spawn_blocking(move || p2p_forward::regenerate_token(&path))
                .await
                .map_err(err_text)?
                .map_err(|e| ui_err("key_failed", &[], format!("Could not write a new key: {e}")))?;
            let using = c.inner.lock().unwrap().use_.as_ref().map(|u| u.peer.to_string());
            if let Some(peer) = using {
                c.use_start(peer, true).await?;
            }
            Ok(key)
        })
        .await
    }

    /// App start: bring back what the user turned on before (sharing, using a Mac). The
    /// first browse/announce of a fresh install always follows a click instead.
    pub fn resume(&self) {
        let s = self.core.settings.lock().unwrap().clone();
        if s.share {
            if let Err(e) = self.block(|c| async move { c.share_start().await.map(|_| ()) }) {
                eprintln!("remote event=resume_share_failed error={e}");
            }
        }
        if let Some(peer) = s.use_peer {
            if let Err(e) = self.block(|c| async move { c.use_start(peer, true).await.map(|_| ()) }) {
                eprintln!("remote event=resume_use_failed error={e}");
            }
        }
    }

    /// Menubar status line while using another Mac (German, like the rest of the menu).
    pub fn tray_line(&self) -> Option<String> {
        let inner = self.core.inner.lock().unwrap();
        if let Some(name) = &inner.removed_by {
            if inner.use_.is_none() {
                return Some(format!("{name} hat diesen Mac entfernt"));
            }
        }
        let u = inner.use_.as_ref()?;
        let what = match u.state() {
            UseState::Connected => match inner.use_activity.as_ref().and_then(slotactivity::status_text) {
                Some(t) => t.trim_start_matches("Slipstream · ").to_string(),
                None => match &inner.use_status {
                    Some(s) if s.model_running => "bereit".into(),
                    Some(_) => "Modell aus".into(),
                    None => "verbunden".into(),
                },
            },
            UseState::Connecting | UseState::Reconnecting => "verbindet…".into(),
            UseState::Waking { .. } => "wird geweckt…".into(),
            UseState::Asleep { .. } => "schläft oder ist aus".into(),
            UseState::Unreachable { .. } => "nicht erreichbar".into(),
            UseState::PortBusy { .. } => "Port 8080 belegt".into(),
            UseState::VersionMismatch { .. } => "andere Slipstream-Version".into(),
            UseState::Removed => "hat diesen Mac entfernt".into(),
        };
        Some(format!("Nutzt {} · {what}", u.name))
    }

    /// Menubar badge while sharing: "geteilt" / "geteilt · 1 Anfrage".
    pub fn share_badge(&self) -> Option<String> {
        let inner = self.core.inner.lock().unwrap();
        let s = inner.share.as_ref()?;
        let (n, q) = s.load();
        Some(match n + q {
            0 => "geteilt".into(),
            1 => "geteilt · 1 Anfrage".into(),
            k => format!("geteilt · {k} Anfragen"),
        })
    }
}

fn save_settings(dir: &std::path::Path, s: &Settings) {
    let _ = std::fs::create_dir_all(dir);
    let path = settings_path(dir);
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(s).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn on_battery() -> bool {
    Command::new("/usr/bin/pmset")
        .args(["-g", "ps"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("'Battery Power'"))
        .unwrap_or(false)
}

/// Small authenticated GET against the local forwarder (status line activity).
async fn local_get(addr: SocketAddr, path: &str, token: &str) -> Option<(u16, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fut = async {
        let mut s = tokio::net::TcpStream::connect(addr).await.ok()?;
        s.write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .ok()?;
        let mut raw = Vec::new();
        (&mut s).take(1 << 20).read_to_end(&mut raw).await.ok()?;
        let end = p2p_forward::http::find_head_end(&raw)?;
        let head = p2p_forward::http::parse_response_head(&raw[..end]).ok()?;
        let body = if head.is_chunked() {
            p2p_forward::http::dechunk_strict(&raw[end..], 1 << 20)?
        } else {
            raw[end..].to_vec()
        };
        Some((head.status, body))
    };
    tokio::time::timeout(Duration::from_secs(2), fut).await.ok().flatten()
}

fn peer_view(p: &p2p_node::trust::TrustedPeer) -> Value {
    json!({
        "peer_id": p.peer_id,
        "name": p.name,
        "role": match p.role { PeerRole::Share => "share", PeerRole::Use => "use" },
        "paired_at": p.paired_at,
        "last_seen": p.last_seen,
        "app_version": p.app_version,
        "fp": p.peer().map(|id| p2p_node::lan::fingerprint(&id)),
    })
}

fn use_state_view(s: &UseState) -> (&'static str, Option<String>) {
    match s {
        UseState::Connecting => ("connecting", None),
        UseState::Connected => ("connected", None),
        UseState::Reconnecting => ("reconnecting", None),
        UseState::Waking { .. } => ("waking", None),
        UseState::Asleep { error } => ("asleep", Some(error.clone())),
        UseState::Unreachable { error } => ("unreachable", Some(error.clone())),
        UseState::PortBusy { error } => ("port_busy", Some(error.clone())),
        UseState::VersionMismatch { .. } => ("version_mismatch", None),
        UseState::Removed => ("removed", None),
    }
}

/// Non-loopback IPv4 addresses of this Mac (for the "enter its address" fallback text).
fn lan_ipv4() -> Vec<String> {
    let out = Command::new("/sbin/ifconfig").output().ok();
    let text = out
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut v: Vec<String> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("inet "))
        .filter_map(|r| r.split_whitespace().next())
        .filter(|ip| !ip.starts_with("127.") && !ip.starts_with("169.254."))
        .map(str::to_string)
        .collect();
    v.dedup();
    v
}

impl Core {
    /// Interface / power facts, from the cache unless it is older than [`NET_CACHE_TTL`]
    /// or the use state changed since (re-read after a network change).
    async fn net_info(&self) -> NetInfo {
        let state_key = self.inner.lock().unwrap().use_.as_ref().map(|u| use_state_view(&u.state()).0.to_string());
        {
            let inner = self.inner.lock().unwrap();
            if let Some(n) = &inner.net {
                if n.at.elapsed() < NET_CACHE_TTL && inner.net_state == state_key {
                    return n.clone();
                }
            }
        }
        let (lan_ips, on_battery) = tokio::task::spawn_blocking(|| (lan_ipv4(), on_battery()))
            .await
            .unwrap_or_default();
        let n = NetInfo {
            at: std::time::Instant::now(),
            lan_ips,
            on_battery,
        };
        let mut inner = self.inner.lock().unwrap();
        inner.net = Some(n.clone());
        inner.net_state = state_key;
        n
    }

    fn node(&self) -> Result<Arc<Node>, String> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(n) = &inner.node {
            return Ok(n.clone());
        }
        let n = Node::open(&self.cfg.dir)
            .map_err(|e| ui_err("p2p_dir", &[], format!("Could not open the Your Macs data folder: {e}")))?;
        inner.node = Some(n.clone());
        Ok(n)
    }

    fn token(&self) -> Result<String, String> {
        api_key_in(&self.cfg.dir)
            .map_err(|e| ui_err("p2p_dir", &[], format!("Could not create the local API key: {e}")))
    }

    fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        let mut s = self.settings.lock().unwrap();
        f(&mut s);
        save_settings(&self.cfg.dir, &s);
    }

    // ------------------------------------------------------------ status

    pub async fn status(self: &Arc<Self>) -> Result<Value, String> {
        let node = self.node().ok();
        let settings = self.settings.lock().unwrap().clone();
        let net = self.net_info().await;
        let inner = self.inner.lock().unwrap();
        let paired: Vec<Value> = node
            .as_ref()
            .map(|n| n.store.list().iter().map(peer_view).collect())
            .unwrap_or_default();
        let use_v = inner.use_.as_ref().map(|u| {
            let ustate = u.state();
            let (state, error) = use_state_view(&ustate);
            let st = inner.use_status.as_ref();
            let entry = node.as_ref().and_then(|n| n.store.get(&u.peer));
            let (wake_remaining_s, wol_sent) = match &ustate {
                UseState::Waking { remaining_s, wol_sent } => (Some(*remaining_s), *wol_sent),
                _ => (None, false),
            };
            let (mismatch_side, mismatch_version) = match &ustate {
                UseState::VersionMismatch { side, version } => (Some(side.clone()), Some(version.clone())),
                _ => (None, None),
            };
            json!({
                "peer_id": u.peer.to_string(),
                "name": u.name,
                "state": state,
                // M3: the forwarder owns the editor port (never true in port_busy) — the
                // window sends the local key to 8080 only then.
                "bound": u.handle().is_some(),
                "error": error,
                "bind": self.cfg.use_bind.to_string(),
                "model_running": st.map(|s| s.model_running),
                "model_id": st.and_then(|s| s.model_id.clone()),
                "n_ctx": st.and_then(|s| s.n_ctx),
                "busy": st.map(|s| s.busy).unwrap_or(false),
                "queue_len": st.map(|s| s.queue_len).unwrap_or(0),
                "on_battery": st.map(|s| s.on_battery).unwrap_or(false),
                "can_start_model": st.map(|s| s.can_start_model).unwrap_or(false),
                "remote_version": st.map(|s| s.app_version.clone())
                    .or_else(|| entry.as_ref().map(|e| e.app_version.clone())),
                "local_busy": st.map(|s| s.local_busy).unwrap_or(false),
                "activity": inner.use_activity,
                // M5
                "wake_remaining_s": wake_remaining_s,
                "wol_sent": wol_sent,
                "wake_known": entry.as_ref().is_some_and(|e| !e.wake_macs.is_empty()),
                "mismatch_side": mismatch_side,
                "mismatch_version": mismatch_version,
                "min_version": p2p_forward::proto::PROTO_FIRST_APP,
            })
        });
        let share_v = inner.share.as_ref().map(|s| {
            let (n, q) = s.load();
            json!({
                "on": true,
                "announced": s.announced,
                "pairing": s.pairing(),
                "requests": n,
                "queued": q,
                "port": s.port(),
                "dial": s.dial_addr(),
                "lan_ips": net.lan_ips,
                "on_battery": net.on_battery,
                "caffeinate": inner.caffeinate.is_some(),
                "remote_started": self.remote_started.lock().unwrap().is_some(),
            })
        });
        let mode = if inner.use_.is_some() {
            "using"
        } else if inner.share.is_some() {
            "sharing"
        } else {
            "off"
        };
        Ok(json!({
            "mode": mode,
            "use": use_v,
            "share": share_v,
            "paired": paired,
            "may_start_model": settings.may_start_model,
            "start_on_battery": settings.start_on_battery,
            "removed_by": inner.removed_by,
            "pairings_reset": node.as_ref().map(|n| n.store.pairings_reset()).unwrap_or(false),
            "lan_started": inner.browser.is_some(),
            // this Mac on battery (M5: a MacBook using another Mac gets a battery-saving hint)
            "on_battery": net.on_battery,
            "fp": node.as_ref().map(|n| n.fingerprint()),
        }))
    }

    // ------------------------------------------------------------- share

    pub async fn share_start(self: &Arc<Self>) -> Result<Value, String> {
        if self.inner.lock().unwrap().use_.is_some() {
            return Err(ui_err(
                "using_active",
                &[],
                "This Mac is using another Mac. Stop using it before sharing this one.",
            ));
        }
        if self.inner.lock().unwrap().share.is_none() {
            let node = self.node()?;
            let may = self.settings.lock().unwrap().may_start_model;
            let ctl: Arc<dyn ModelControl> = Arc::new(Control {
                core: Arc::downgrade(self),
            });
            let mut opts = ShareOpts::new(self.cfg.upstream);
            opts.listen = self.cfg.share_listen.clone();
            opts.may_start_model = may;
            opts.announce = self.cfg.announce;
            opts.model = Some(ctl);
            // M3: forward only to this app's own model process (or its children).
            let weak = Arc::downgrade(self);
            opts.upstream_owner = p2p_forward::upstream::UpstreamOwner::Pid(Arc::new(move || {
                let c = weak.upgrade()?;
                let m = c.model.lock().unwrap().clone()?;
                m.server_pid()
            }));
            let session = ShareSession::start(node, opts)
            .await
            .map_err(|e| ui_err("share_failed", &[], format!("Sharing could not start: {e}")))?;
            let session = Arc::new(session);
            let mut inner = self.inner.lock().unwrap();
            inner.share = Some(session);
            if self.cfg.caffeinate {
                let me = Arc::downgrade(self);
                inner
                    .monitors
                    .push(tokio::spawn(async move { awake_monitor(me).await }));
            }
        }
        self.update_settings(|s| s.share = true);
        self.status().await
    }

    pub async fn share_stop(self: &Arc<Self>) -> Result<Value, String> {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.share = None;
            if let Some(mut c) = inner.caffeinate.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        self.update_settings(|s| s.share = false);
        self.status().await
    }

    /// Open a pairing window inside the running share (starting the share first: one click
    /// on the big Mac shows the code).
    pub async fn pair_code(self: &Arc<Self>) -> Result<Value, String> {
        if self.inner.lock().unwrap().share.is_none() {
            self.share_start().await?;
        }
        let share = self.inner.lock().unwrap().share.clone().ok_or("sharing is off")?;
        Ok(json!(share.open_pairing()))
    }

    pub async fn pair_cancel(self: &Arc<Self>) -> Result<Value, String> {
        if let Some(s) = self.inner.lock().unwrap().share.clone() {
            s.cancel_pairing();
        }
        self.status().await
    }

    pub fn set_battery_start(&self, allowed: bool) {
        self.update_settings(|s| s.start_on_battery = allowed);
    }

    pub fn set_may_start(&self, allowed: bool) {
        self.update_settings(|s| s.may_start_model = allowed);
    }

    // --------------------------------------------------------- discovery

    /// Sharing Macs on this network. The first call starts browsing (the user clicked).
    pub async fn discover(self: &Arc<Self>) -> Result<Value, String> {
        let node = self.node()?;
        let mut inner = self.inner.lock().unwrap();
        if inner.browser.is_none() && self.cfg.discovery {
            inner.browser = Some(LanBrowser::start().map_err(|e| {
                ui_err(
                    "lan_unavailable",
                    &[],
                    format!("The local network cannot be searched: {e}"),
                )
            })?);
        }
        let own = node.fingerprint();
        let list = inner
            .browser
            .as_ref()
            .map(|b| b.list(&node.store, Some(&own)))
            .unwrap_or_default();
        Ok(json!(list))
    }

    pub async fn pair(self: &Arc<Self>, target: String, addrs: Vec<String>, code: String) -> Result<Value, String> {
        let node = self.node()?;
        let digits: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() != 6 {
            return Err(ui_err("pair_code_format", &[], "The code has 6 digits."));
        }
        match p2p_node::session::pair_with_target(&node, &target, &addrs, &digits, None).await {
            Ok(p) => {
                self.inner.lock().unwrap().removed_by = None;
                Ok(peer_view(&p))
            }
            Err(p2p_node::pairing::PairError::WrongCode) => Err(ui_err(
                "pair_wrong_code",
                &[],
                "Wrong code. Check the code on the other Mac.",
            )),
            Err(p2p_node::pairing::PairError::Unreachable(e)) => Err(ui_err(
                "pair_unreachable",
                &[],
                format!("The other Mac could not be reached: {e}"),
            )),
            Err(p2p_node::pairing::PairError::Refused { code, message }) => {
                let key = match code.as_str() {
                    "expired" => "pair_expired",
                    "too_many_attempts" => "pair_too_many",
                    "already_used" => "pair_used",
                    "busy" => "pair_busy",
                    _ => "pair_not_open",
                };
                Err(ui_err(key, &[], message))
            }
            Err(e) => Err(ui_err("pair_failed", &[], format!("Pairing failed: {e}"))),
        }
    }

    // --------------------------------------------------------------- use

    pub async fn use_start(self: &Arc<Self>, peer: String, confirm_stop_local: bool) -> Result<Value, String> {
        let node = self.node()?;
        let id: PeerId = peer.parse().map_err(|_| ui_err("not_paired", &[], "Unknown Mac."))?;
        let entry = node
            .store
            .get(&id)
            .ok_or_else(|| ui_err("not_paired", &[], "This Mac is not paired. Pair it first."))?;
        if self.inner.lock().unwrap().share.is_some() {
            return Err(ui_err(
                "sharing_active",
                &[],
                "This Mac is sharing its model. Turn sharing off before using another Mac.",
            ));
        }
        let model = self.model.lock().unwrap().clone();
        if let Some(m) = &model {
            if m.running() {
                if !confirm_stop_local {
                    return Err(ui_err(
                        "local_running",
                        &[("name", entry.name.clone())],
                        "This Mac's model is running. Stop it to use the other Mac?",
                    ));
                }
                let m = m.clone();
                tokio::task::spawn_blocking(move || m.stop()).await.map_err(err_text)?;
            }
        }
        // Replace a running session (another Mac chosen).
        let old = self.inner.lock().unwrap().use_.take();
        drop(old);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut opts = UseOpts::new(peer.clone(), self.cfg.use_bind);
        opts.token = Some(self.token()?);
        opts.app_origin = self.cfg.app_origin.clone();
        opts.discovery = self.cfg.discovery;
        opts.wake = self.cfg.wake.clone();
        if let Some(w) = self.cfg.wake_window {
            opts.wake_window = w;
            opts.wake_grace = Duration::from_millis(300);
        }
        let session = UseSession::start(node, opts)
        .map_err(|e| ui_err("not_paired", &[], e))?;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.use_ = Some(Arc::new(session));
            inner.removed_by = None;
            inner.use_status = None;
            inner.use_activity = None;
            let me = Arc::downgrade(self);
            inner.monitors.push(tokio::spawn(async move { use_monitor(me).await }));
        }
        USE_ACTIVE.store(true, Ordering::SeqCst);
        self.update_settings(|s| s.use_peer = Some(peer));
        self.status().await
    }

    pub async fn use_stop(self: &Arc<Self>) -> Result<Value, String> {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.use_ = None;
            inner.use_status = None;
            inner.use_activity = None;
            inner.removed_by = None;
        }
        USE_ACTIVE.store(false, Ordering::SeqCst);
        self.update_settings(|s| s.use_peer = None);
        self.status().await
    }

    pub async fn trusted(self: &Arc<Self>) -> Result<Value, String> {
        let node = self.node()?;
        Ok(json!(node.store.list().iter().map(peer_view).collect::<Vec<_>>()))
    }

    /// Remove a paired Mac (it is told via `Unpair` while connected).
    pub async fn revoke(self: &Arc<Self>, peer: String) -> Result<Value, String> {
        let node = self.node()?;
        let using_it = self
            .inner
            .lock()
            .unwrap()
            .use_
            .as_ref()
            .is_some_and(|u| u.peer.to_string() == peer);
        let store = node.store.clone();
        tokio::task::spawn_blocking(move || store.revoke(&peer))
            .await
            .map_err(err_text)?
            .map_err(|e| ui_err("revoke_failed", &[], e.to_string()))?;
        // Let the running share / use connection send Unpair (the other Mac then removes this
        // one too) before the window re-reads the list; only then drop the use session — it
        // would otherwise be gone before it could say goodbye.
        tokio::time::sleep(Duration::from_millis(500)).await;
        if using_it {
            // clears the "removed you" note the supervisor may have set for our own revoke
            self.use_stop().await?;
        }
        self.status().await
    }

    pub async fn rpc(self: &Arc<Self>, req: StatusRequest) -> Result<Value, String> {
        let u = self
            .inner
            .lock()
            .unwrap()
            .use_
            .clone()
            .ok_or_else(|| ui_err("not_using", &[], "Not using another Mac."))?;
        let reply = u
            .rpc(req)
            .await
            .map_err(|e| ui_err("not_connected", &[], format!("The other Mac is not reachable: {e}")))?;
        match reply {
            StatusReply::Status(s) => {
                self.inner.lock().unwrap().use_status = Some(s.clone());
                Ok(json!(s))
            }
            StatusReply::Refused { code, message } => Err(ui_err(&format!("remote_{code}"), &[], message)),
        }
    }

    pub fn dismiss_removed(&self) {
        self.inner.lock().unwrap().removed_by = None;
    }
}

/// Status of the used Mac every 3 s (+ slot activity for the status line).
async fn use_monitor(core: std::sync::Weak<Core>) {
    loop {
        let Some(c) = core.upgrade() else { return };
        let Some(u) = c.inner.lock().unwrap().use_.clone() else {
            return;
        };
        match u.state() {
            UseState::Removed => {
                let mut inner = c.inner.lock().unwrap();
                if inner.use_.as_ref().is_some_and(|x| Arc::ptr_eq(x, &u)) {
                    inner.removed_by = Some(u.name.clone());
                    inner.use_ = None;
                    inner.use_status = None;
                    inner.use_activity = None;
                    drop(inner);
                    USE_ACTIVE.store(false, Ordering::SeqCst);
                    c.update_settings(|s| s.use_peer = None);
                }
                return;
            }
            UseState::Connected => {
                if let Ok(StatusReply::Status(s)) = u.rpc(StatusRequest::Status).await {
                    let running = s.model_running;
                    c.inner.lock().unwrap().use_status = Some(s);
                    let act = if running {
                        match c.token() {
                            Ok(tok) => local_get(c.cfg.use_bind, "/slots", &tok)
                                .await
                                .filter(|(st, _)| *st == 200)
                                .and_then(|(_, b)| slotactivity::parse_slots(&String::from_utf8_lossy(&b))),
                            Err(_) => None,
                        }
                    } else {
                        None
                    };
                    c.inner.lock().unwrap().use_activity = act;
                }
            }
            _ => {}
        }
        drop(u);
        drop(c);
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// `caffeinate -i -w <app pid>` while sharing on AC power, and during a remote request on
/// battery. Checked every 5 s; ends with the share.
async fn awake_monitor(core: std::sync::Weak<Core>) {
    loop {
        let Some(c) = core.upgrade() else { return };
        let share = c.inner.lock().unwrap().share.clone();
        let Some(share) = share else {
            if let Some(mut ch) = c.inner.lock().unwrap().caffeinate.take() {
                let _ = ch.kill();
                let _ = ch.wait();
            }
            return;
        };
        let (n, q) = share.load();
        let battery = tokio::task::spawn_blocking(on_battery).await.unwrap_or(false);
        let want = want_awake(battery, n + q);
        {
            let mut inner = c.inner.lock().unwrap();
            if let Some(ch) = inner.caffeinate.as_mut() {
                if !matches!(ch.try_wait(), Ok(None)) {
                    inner.caffeinate = None;
                }
            }
            match (want, inner.caffeinate.is_some()) {
                (true, false) => {
                    let pid = std::process::id().to_string();
                    match Command::new("/usr/bin/caffeinate").args(["-i", "-w", &pid]).spawn() {
                        Ok(ch) => inner.caffeinate = Some(ch),
                        Err(e) => eprintln!("remote event=caffeinate_failed error={e}"),
                    }
                }
                (false, true) => {
                    if let Some(mut ch) = inner.caffeinate.take() {
                        let _ = ch.kill();
                        let _ = ch.wait();
                    }
                }
                _ => {}
            }
        }
        drop(share);
        drop(c);
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// The share side's StartModel / StopModel (through the app).
struct Control {
    core: std::sync::Weak<Core>,
}

fn refused(code: &str, message: &str) -> StatusReply {
    StatusReply::Refused {
        code: code.into(),
        message: message.into(),
    }
}

/// English text of a `[code ...] text` error.
fn plain(e: &str) -> String {
    match (e.starts_with('['), e.find("] ")) {
        (true, Some(i)) => e[i + 2..].to_string(),
        _ => e.to_string(),
    }
}

impl Control {
    fn status(core: &Core, model_running: bool) -> StatusReply {
        let load = core
            .inner
            .lock()
            .unwrap()
            .share
            .as_ref()
            .map(|s| s.load())
            .unwrap_or((0, 0));
        StatusReply::Status(Status {
            name: p2p_forward::status::default_name(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            model_running,
            busy: load.0 > 0,
            queue_len: load.1,
            can_start_model: true,
            ..Status::default()
        })
    }
}

impl ModelControl for Control {
    fn allowed(&self, peer: &PeerId) -> bool {
        let Some(c) = self.core.upgrade() else { return false };
        let on = c.settings.lock().unwrap().may_start_model;
        let is_user = c
            .inner
            .lock()
            .unwrap()
            .node
            .as_ref()
            .and_then(|n| n.store.get(peer))
            .is_some_and(|p| p.role == PeerRole::Use);
        on && is_user
    }

    fn start(&self, peer: PeerId) -> ReplyFuture {
        let core = self.core.clone();
        Box::pin(async move {
            let Some(c) = core.upgrade() else {
                return refused("gone", "The app is quitting.");
            };
            if c.inner.lock().unwrap().use_.is_some() {
                return refused("remote_active", "That Mac is itself using another Mac.");
            }
            // L3: on battery only if this Mac's owner allowed it.
            let battery = c.cfg.on_battery;
            let on_battery = tokio::task::spawn_blocking(battery).await.unwrap_or(false);
            if !c.settings.lock().unwrap().start_on_battery && on_battery {
                return refused(
                    "on_battery",
                    "That Mac runs on battery; its owner has not allowed starting the model on battery.",
                );
            }
            let Some(model) = c.model.lock().unwrap().clone() else {
                return refused("no_model", "The model cannot be started from here.");
            };
            let m = model.clone();
            let out =
                tokio::task::spawn_blocking(move || if m.running() { Ok(None) } else { m.start().map(Some) }).await;
            match out {
                Ok(Ok(None)) => Control::status(&c, true),
                Ok(Ok(Some(_))) => {
                    *c.remote_started.lock().unwrap() = Some(peer);
                    Control::status(&c, false)
                }
                Ok(Err(e)) => refused("start_refused", &plain(&e)),
                Err(e) => refused("start_refused", &e.to_string()),
            }
        })
    }

    fn stop(&self, peer: PeerId) -> ReplyFuture {
        let core = self.core.clone();
        Box::pin(async move {
            let Some(c) = core.upgrade() else {
                return refused("gone", "The app is quitting.");
            };
            let starter = *c.remote_started.lock().unwrap();
            match starter {
                None => {
                    return refused(
                        "not_started_remotely",
                        "The model was started on that Mac itself; only it can stop it.",
                    )
                }
                // L3: only the paired Mac that started it may stop it.
                Some(p) if p != peer => {
                    return refused("not_started_by_you", "Another paired Mac started the model; only it can stop it.")
                }
                Some(_) => {}
            }
            // M5 busy accounting: llama-server's requests (processing + deferred from
            // /metrics, else busy /slots) beyond the forwarded requests that hold an upstream
            // connection are this Mac's own — never stop the model under them.
            let forwarded = c.inner.lock().unwrap().share.as_ref().map(|s| s.forwarded()).unwrap_or(0);
            if local_requests_in_flight(c.cfg.upstream, forwarded).await {
                return refused("local_request", "That Mac is busy with its own work right now.");
            }
            let Some(model) = c.model.lock().unwrap().clone() else {
                return refused("no_model", "The model cannot be stopped from here.");
            };
            let _ = tokio::task::spawn_blocking(move || model.stop()).await;
            *c.remote_started.lock().unwrap() = None;
            Control::status(&c, false)
        })
    }
}

/// A request that did not come through the forwarder is in flight on the local
/// llama-server (`forwarded` = paired requests holding an upstream connection).
async fn local_requests_in_flight(upstream: SocketAddr, forwarded: usize) -> bool {
    p2p_forward::status::local_requests(upstream, forwarded)
        .await
        .is_some_and(|n| n > 0)
}

// ------------------------------------------------------------ commands

fn req_of(s: &str) -> StatusRequest {
    match s {
        "start" => StatusRequest::StartModel,
        "stop" => StatusRequest::StopModel,
        _ => StatusRequest::Status,
    }
}

#[tauri::command]
pub async fn remote_status(rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(|c| async move { c.status().await }).await
}

#[tauri::command]
pub async fn remote_share_start(rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(|c| async move { c.share_start().await }).await
}

#[tauri::command]
pub async fn remote_share_stop(rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(|c| async move { c.share_stop().await }).await
}

#[tauri::command]
pub async fn remote_pair_code(rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(|c| async move { c.pair_code().await }).await
}

#[tauri::command]
pub async fn remote_pair_cancel(rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(|c| async move { c.pair_cancel().await }).await
}

#[tauri::command]
pub async fn remote_discover(rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(|c| async move { c.discover().await }).await
}

#[tauri::command]
pub async fn remote_pair(
    target: String,
    addrs: Option<Vec<String>>,
    code: String,
    rs: State<'_, RemoteState>,
) -> Result<Value, String> {
    rs.run(move |c| async move { c.pair(target, addrs.unwrap_or_default(), code).await })
        .await
}

#[tauri::command]
pub async fn remote_use(peer: String, confirm: Option<bool>, rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(move |c| async move { c.use_start(peer, confirm.unwrap_or(false)).await })
        .await
}

#[tauri::command]
pub async fn remote_stop_using(rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(|c| async move { c.use_stop().await }).await
}

#[tauri::command]
pub async fn remote_trusted(rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(|c| async move { c.trusted().await }).await
}

#[tauri::command]
pub async fn remote_revoke(peer: String, rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(move |c| async move { c.revoke(peer).await }).await
}

#[tauri::command]
pub async fn remote_start_model(rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(|c| async move { c.rpc(req_of("start")).await }).await
}

#[tauri::command]
pub async fn remote_stop_model(rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(|c| async move { c.rpc(req_of("stop")).await }).await
}

#[tauri::command]
pub async fn remote_set_model_control(allowed: bool, rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(move |c| async move {
        c.set_may_start(allowed);
        c.status().await
    })
    .await
}

#[tauri::command]
pub async fn remote_set_battery_start(allowed: bool, rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(move |c| async move {
        c.set_battery_start(allowed);
        c.status().await
    })
    .await
}

#[tauri::command]
pub async fn remote_dismiss_removed(rs: State<'_, RemoteState>) -> Result<Value, String> {
    rs.run(|c| async move {
        c.dismiss_removed();
        c.status().await
    })
    .await
}

/// The local API key for editor snippets (same value the Connect-editor patch writes).
#[tauri::command]
pub fn remote_api_key(rs: State<'_, RemoteState>) -> Result<String, String> {
    api_key_in(&rs.core.cfg.dir)
}

// --------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    /// Fake big-Mac model: "starts" by flipping a flag (no process).
    #[derive(Default)]
    struct FakeModel {
        running: AtomicBool,
        starts: AtomicU32,
        stops: AtomicU32,
    }

    impl AppModel for FakeModel {
        /// The mock llama-server runs inside the test process (M3 owner check).
        fn server_pid(&self) -> Option<u32> {
            Some(std::process::id())
        }
        fn running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }
        fn start(&self) -> Result<String, String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.running.store(true, Ordering::SeqCst);
            Ok("started".into())
        }
        fn stop(&self) {
            self.stops.fetch_add(1, Ordering::SeqCst);
            self.running.store(false, Ordering::SeqCst);
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn cfg(dir: &std::path::Path, upstream: SocketAddr, use_port: u16) -> RemoteConfig {
        RemoteConfig {
            dir: dir.to_path_buf(),
            share_listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
            use_bind: SocketAddr::from(([127, 0, 0, 1], use_port)),
            upstream,
            // No mDNS in tests (pairing and use go by address; nothing is announced).
            announce: false,
            discovery: false,
            app_origin: Some(APP_ORIGIN.into()),
            caffeinate: false,
            wake: p2p_node::wake::WakeTargets::Off,
            wake_window: None,
            on_battery: || false,
        }
    }

    fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let t0 = Instant::now();
        while !f() {
            assert!(t0.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn http(addr: SocketAddr, req: &str) -> String {
        use std::io::{Read, Write};
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    /// Two app instances in one process (data dirs a/b, different ports): pairing through
    /// the command layer, use through the forwarder against the mock llama-server, remote
    /// StartModel/StopModel, revoke → "this Mac removed you".
    #[test]
    fn share_and_use_in_process_against_mock_llama() {
        let tmp = tempfile::tempdir().unwrap();
        let mock_rt = tokio::runtime::Runtime::new().unwrap();
        let mock = mock_rt
            .block_on(p2p_forward::mock_llama::start("127.0.0.1:0".parse().unwrap()))
            .unwrap();
        let use_port = free_port();
        let big = RemoteState::new(cfg(&tmp.path().join("a"), mock.addr, free_port()));
        let book = RemoteState::new(cfg(
            &tmp.path().join("b"),
            SocketAddr::from(([127, 0, 0, 1], free_port())),
            use_port,
        ));
        let big_model = Arc::new(FakeModel::default());
        big.set_model(big_model.clone());
        let book_model = Arc::new(FakeModel::default());
        book_model.running.store(true, Ordering::SeqCst);
        book.set_model(book_model.clone());

        // Big Mac: one click → code (sharing starts by itself).
        let code = big.block(|c| async move { c.pair_code().await }).unwrap();
        let code = code["code"].as_str().unwrap().to_string();
        assert_eq!(code.len(), 6);
        let st = big.block(|c| async move { c.status().await }).unwrap();
        assert_eq!(st["mode"], "sharing");
        let dial = st["share"]["dial"].as_str().unwrap().to_string();
        assert_eq!(st["share"]["pairing"]["state"], "open");

        // MacBook: wrong code first (counts as one failure), then the right one.
        let wrong = if code == "000000" { "111111" } else { "000000" };
        let e = book
            .block(|c| {
                let d = dial.clone();
                async move { c.pair(d, vec![], wrong.into()).await }
            })
            .unwrap_err();
        assert!(e.starts_with("[pair_wrong_code]"), "{e}");
        let p = book
            .block(|c| {
                let (d, code) = (dial.clone(), code.clone());
                async move { c.pair(d, vec![], code).await }
            })
            .unwrap();
        assert_eq!(p["role"], "share");
        let big_peer = p["peer_id"].as_str().unwrap().to_string();
        wait_for("big Mac lists the MacBook", 5, || {
            let st = big.block(|c| async move { c.status().await }).unwrap();
            st["paired"].as_array().unwrap().len() == 1 && st["share"]["pairing"]["state"] == "paired"
        });

        // Use: the local model runs → the window must ask first.
        let e = book
            .block(|c| {
                let p = big_peer.clone();
                async move { c.use_start(p, false).await }
            })
            .unwrap_err();
        assert!(e.starts_with("[local_running"), "{e}");
        assert!(book_model.running());
        book.block(|c| {
            let p = big_peer.clone();
            async move { c.use_start(p, true).await }
        })
        .unwrap();
        assert!(!book_model.running(), "local model stopped after confirm");
        assert!(book.core.inner.lock().unwrap().use_.is_some()); // (USE_ACTIVE is process-wide)
        wait_for("connected", 15, || {
            book.block(|c| async move { c.status().await }).unwrap()["use"]["state"] == "connected"
        });

        // Editor request through the forwarder with the local token.
        let token = api_key_in(&tmp.path().join("b")).unwrap();
        let bind = SocketAddr::from(([127, 0, 0, 1], use_port));
        let body = r#"{"model":"x","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
        let resp = http(
            bind,
            &format!(
                "POST /v1/chat/completions HTTP/1.1\r\nHost: {bind}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("data: "), "{resp}");
        let no_key = http(
            bind,
            &format!("GET /v1/models HTTP/1.1\r\nHost: {bind}\r\nConnection: close\r\n\r\n"),
        );
        assert!(no_key.starts_with("HTTP/1.1 401"), "{no_key}");
        // A local Start is refused while using another Mac (start_server checks USE_ACTIVE).
        assert!(book.core.inner.lock().unwrap().use_.is_some()); // (USE_ACTIVE is process-wide)

        // Remote model control: start through the big Mac's app, stop only that one.
        let s = book
            .block(|c| async move { c.rpc(StatusRequest::StartModel).await })
            .unwrap();
        assert_eq!(s["can_start_model"], true);
        assert_eq!(big_model.starts.load(Ordering::SeqCst), 1);
        assert!(big.core.remote_started.lock().unwrap().is_some());
        // The mock's slot is busy and no paired request is in flight = a local request: refused.
        let e = book.block(|c| async move { c.rpc(StatusRequest::StopModel).await }).unwrap_err();
        assert!(e.starts_with("[remote_local_request]"), "{e}");
        assert_eq!(big_model.stops.load(Ordering::SeqCst), 0);
        // A model started on the big Mac itself is not stoppable remotely.
        big.local_lifecycle();
        let e = book.block(|c| async move { c.rpc(StatusRequest::StopModel).await }).unwrap_err();
        assert!(e.starts_with("[remote_not_started_remotely]"), "{e}");
        // "Paired Macs may start and stop the model" off → refused.
        big.core.set_may_start(false);
        let e = book
            .block(|c| async move { c.rpc(StatusRequest::StartModel).await })
            .unwrap_err();
        assert!(e.starts_with("[remote_model_control_off]"), "{e}");

        // The big Mac removes the MacBook → MacBook shows "removed you", port 8080 freed.
        let book_peer = big.block(|c| async move { c.trusted().await }).unwrap()[0]["peer_id"]
            .as_str()
            .unwrap()
            .to_string();
        big.block(|c| async move { c.revoke(book_peer).await }).unwrap();
        wait_for("MacBook notices it was removed", 10, || {
            let st = book.block(|c| async move { c.status().await }).unwrap();
            st["removed_by"].is_string() && st["mode"] == "off"
        });
        assert!(book.core.inner.lock().unwrap().use_.is_none());
        assert!(std::net::TcpListener::bind(bind).is_ok(), "editor port released");
        let st = book.block(|c| async move { c.status().await }).unwrap();
        assert_eq!(st["paired"].as_array().unwrap().len(), 0);
        assert!(book.tray_line().unwrap().contains("hat diesen Mac entfernt"));
        // The persisted choice is cleared (no auto-use at the next start).
        let saved: Settings =
            serde_json::from_slice(&std::fs::read(settings_path(&tmp.path().join("b"))).unwrap()).unwrap();
        assert!(saved.use_peer.is_none());
        let saved: Settings =
            serde_json::from_slice(&std::fs::read(settings_path(&tmp.path().join("a"))).unwrap()).unwrap();
        assert!(saved.share && !saved.may_start_model);

        // Sharing and using exclude each other.
        let e = big
            .block(|c| async move {
                c.use_start("12D3KooWQYhTNQdmr3ArTeUHRYzFg94BKyTkoWBDWez9kSCVe2Xo".into(), true)
                    .await
            })
            .unwrap_err();
        assert!(
            e.starts_with("[not_paired]") || e.starts_with("[sharing_active]"),
            "{e}"
        );
        big.block(|c| async move { c.share_stop().await }).unwrap();
        drop(book);
        drop(big);
        drop(mock);
        drop(mock_rt);
    }

    /// StopModel succeeds for a remotely started model when the big Mac's server is idle.
    #[test]
    fn remote_stop_of_a_remotely_started_model() {
        let tmp = tempfile::tempdir().unwrap();
        let idle = SocketAddr::from(([127, 0, 0, 1], free_port())); // nothing listens: not busy
        let rs = RemoteState::new(cfg(tmp.path(), idle, free_port()));
        let model = Arc::new(FakeModel::default());
        rs.set_model(model.clone());
        let ctl = Control { core: Arc::downgrade(&rs.core) };
        let peer = PeerId::random();
        let r = rs.rt.block_on(ctl.start(peer));
        assert!(matches!(r, StatusReply::Status(_)), "{r:?}");
        assert!(model.running() && rs.core.remote_started.lock().unwrap().is_some());
        let r = rs.rt.block_on(ctl.stop(peer));
        assert!(matches!(r, StatusReply::Status(ref s) if !s.model_running), "{r:?}");
        assert_eq!(model.stops.load(Ordering::SeqCst), 1);
        // already running locally: start answers "running" and does not mark it remote
        model.running.store(true, Ordering::SeqCst);
        let r = rs.rt.block_on(ctl.start(peer));
        assert!(matches!(r, StatusReply::Status(ref s) if s.model_running), "{r:?}");
        assert!(!rs.core.remote_started.lock().unwrap().is_some());
    }

    /// Security review L3: StartModel on battery only with the owner's opt-in; only the
    /// paired Mac that started the model may stop it.
    #[test]
    fn remote_start_battery_opt_in_and_stop_only_by_the_starter() {
        let tmp = tempfile::tempdir().unwrap();
        let idle = SocketAddr::from(([127, 0, 0, 1], free_port()));
        let mut c = cfg(tmp.path(), idle, free_port());
        c.on_battery = || true;
        let rs = RemoteState::new(c);
        let model = Arc::new(FakeModel::default());
        rs.set_model(model.clone());
        let ctl = Control { core: Arc::downgrade(&rs.core) };
        let (a, b) = (PeerId::random(), PeerId::random());
        let r = rs.rt.block_on(ctl.start(a));
        assert!(matches!(&r, StatusReply::Refused { code, .. } if code == "on_battery"), "{r:?}");
        assert_eq!(model.starts.load(Ordering::SeqCst), 0);
        rs.core.set_battery_start(true);
        let r = rs.rt.block_on(ctl.start(a));
        assert!(matches!(r, StatusReply::Status(_)), "{r:?}");
        assert_eq!(*rs.core.remote_started.lock().unwrap(), Some(a));
        let r = rs.rt.block_on(ctl.stop(b));
        assert!(matches!(&r, StatusReply::Refused { code, .. } if code == "not_started_by_you"), "{r:?}");
        assert!(model.running());
        let r = rs.rt.block_on(ctl.stop(a));
        assert!(matches!(r, StatusReply::Status(ref s) if !s.model_running), "{r:?}");
        let st = rs.block(|c| async move { c.status().await }).unwrap();
        assert_eq!(st["start_on_battery"], true);
    }

    /// Pair a fresh big Mac (sharing, upstream = `upstream`) with a fresh MacBook.
    fn pair_two(tmp: &std::path::Path, upstream: SocketAddr, book_cfg: impl FnOnce(&mut RemoteConfig)) -> (RemoteState, RemoteState, String) {
        let big = RemoteState::new(cfg(&tmp.join("a"), upstream, free_port()));
        let mut bc = cfg(&tmp.join("b"), SocketAddr::from(([127, 0, 0, 1], free_port())), free_port());
        book_cfg(&mut bc);
        let book = RemoteState::new(bc);
        let code = big.block(|c| async move { c.pair_code().await }).unwrap()["code"].as_str().unwrap().to_string();
        let dial = big.block(|c| async move { c.status().await }).unwrap()["share"]["dial"].as_str().unwrap().to_string();
        let p = book.block(|c| async move { c.pair(dial, vec![], code).await }).unwrap();
        let peer = p["peer_id"].as_str().unwrap().to_string();
        (big, book, peer)
    }

    /// M5 battery rules: awake on AC while sharing; on battery only during a remote request.
    #[test]
    fn battery_rules_for_the_power_assertion() {
        assert!(want_awake(false, 0), "AC, idle: stay awake while sharing");
        assert!(want_awake(false, 2));
        assert!(!want_awake(true, 0), "battery, idle: may sleep (sharing allowed, with a warning)");
        assert!(want_awake(true, 1), "battery, a remote request runs: stay awake for it");
    }

    /// M5 waking: the big Mac stops sharing (asleep) → the MacBook keeps 8080 bound, shows
    /// waking, then asleep, and editors get the 503 answers; the big Mac comes back → connected.
    #[test]
    fn waking_then_asleep_then_back() {
        let tmp = tempfile::tempdir().unwrap();
        let mock_rt = tokio::runtime::Runtime::new().unwrap();
        let mock = mock_rt.block_on(p2p_forward::mock_llama::start("127.0.0.1:0".parse().unwrap())).unwrap();
        let (big, book, big_peer) = pair_two(tmp.path(), mock.addr, |c| c.wake_window = Some(Duration::from_secs(1)));
        let port = big.block(|c| async move { c.status().await }).unwrap()["share"]["port"].as_u64().unwrap();
        big.block(|c| async move { c.share_stop().await }).unwrap();
        book.block(|c| {
            let p = big_peer.clone();
            async move { c.use_start(p, true).await }
        })
        .unwrap();
        let bind = book.core.cfg.use_bind;
        wait_for("waking", 10, || book.block(|c| async move { c.status().await }).unwrap()["use"]["state"] == "waking");
        let token = api_key_in(&tmp.path().join("b")).unwrap();
        let req = format!("GET /v1/models HTTP/1.1\r\nHost: {bind}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n");
        let r = http(bind, &req);
        assert!(r.starts_with("HTTP/1.1 503") && (r.contains("peer_waking") || r.contains("peer_asleep")), "{r}");
        wait_for("asleep", 10, || book.block(|c| async move { c.status().await }).unwrap()["use"]["state"] == "asleep");
        let r = http(bind, &req);
        assert!(r.starts_with("HTTP/1.1 503") && r.contains("peer_asleep"), "{r}");
        assert!(book.tray_line().unwrap().contains("schläft"), "{:?}", book.tray_line());
        // the editor port stays with the forwarder while the big Mac sleeps (USE_ACTIVE is a
        // process-wide flag shared with the other tests, so check the port itself)
        assert!(std::net::TcpListener::bind(bind).is_err(), "editor port still bound");
        // the big Mac wakes and shares again on the same port
        let mut c2 = cfg(&tmp.path().join("a"), mock.addr, free_port());
        c2.share_listen = Some(format!("/ip4/127.0.0.1/udp/{port}/quic-v1"));
        drop(big);
        let big = RemoteState::new(c2);
        big.set_model(Arc::new(FakeModel::default())); // M3: the app's model owns the upstream
        big.block(|c| async move { c.share_start().await }).unwrap();
        wait_for("connected", 15, || book.block(|c| async move { c.status().await }).unwrap()["use"]["state"] == "connected");
        assert!(http(bind, &req).starts_with("HTTP/1.1 200"));
        book.block(|c| async move { c.use_stop().await }).unwrap();
        drop(book);
        drop(big);
        drop(mock_rt);
    }

    /// M5 busy accounting: StopModel is refused while llama-server holds a request that did
    /// not come through the forwarder (live /metrics), allowed when it holds none.
    #[test]
    fn stop_model_uses_metrics_minus_forwarded_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let mock_rt = tokio::runtime::Runtime::new().unwrap();
        let mock = mock_rt.block_on(p2p_forward::mock_llama::start("127.0.0.1:0".parse().unwrap())).unwrap();
        mock.set_local_requests(1);
        let rs = RemoteState::new(cfg(tmp.path(), mock.addr, free_port()));
        let model = Arc::new(FakeModel::default());
        rs.set_model(model.clone());
        rs.block(|c| async move { c.share_start().await }).unwrap();
        let ctl = Control { core: Arc::downgrade(&rs.core) };
        let peer = PeerId::random();
        assert!(matches!(rs.rt.block_on(ctl.start(peer)), StatusReply::Status(_)));
        let r = rs.rt.block_on(ctl.stop(peer));
        assert!(matches!(&r, StatusReply::Refused { code, .. } if code == "local_request"), "{r:?}");
        assert_eq!(model.stops.load(Ordering::SeqCst), 0);
        mock.set_local_requests(0);
        let r = rs.rt.block_on(ctl.stop(peer));
        assert!(matches!(r, StatusReply::Status(_)), "{r:?}");
        assert_eq!(model.stops.load(Ordering::SeqCst), 1);
        drop(rs);
        drop(mock_rt);
    }

    /// M5 item 8: interface / power info is cached (no ifconfig per poll).
    #[test]
    fn status_caches_interface_info() {
        let tmp = tempfile::tempdir().unwrap();
        let rs = RemoteState::new(cfg(tmp.path(), SocketAddr::from(([127, 0, 0, 1], free_port())), free_port()));
        rs.block(|c| async move { c.status().await }).unwrap();
        let at = rs.core.inner.lock().unwrap().net.as_ref().unwrap().at;
        for _ in 0..5 {
            let st = rs.block(|c| async move { c.status().await }).unwrap();
            assert!(st["on_battery"].is_boolean());
        }
        assert_eq!(rs.core.inner.lock().unwrap().net.as_ref().unwrap().at, at, "re-read within the TTL");
        // a state change (e.g. after a network change) forces a re-read
        rs.core.inner.lock().unwrap().net_state = Some("other".into());
        rs.block(|c| async move { c.status().await }).unwrap();
        assert_ne!(rs.core.inner.lock().unwrap().net.as_ref().unwrap().at, at);
    }

    #[test]
    fn plain_strips_the_code() {
        assert_eq!(plain("[low_memory free=5] Not enough"), "Not enough");
        assert_eq!(plain("no code"), "no code");
    }
}
