//! In-process "Use your other Mac" sessions for the app (v0.6 M4).
//!
//! The CLI (`slipstream-node share|use|pair|discover`) and the Slipstream app share this
//! code: [`Node`] (identity + trust store + file watcher for one data dir), [`ShareSession`]
//! (forwarder share side + pair protocol + control socket + optional mDNS announcement +
//! pairing windows opened in place), [`UseSession`] (use side with a supervisor that
//! re-dials while the big Mac is asleep or away and notices when it removed us),
//! [`LanBrowser`] (continuous mDNS browse after a user click) and [`pair_with_target`]
//! (client side of pairing by fingerprint or address).
//!
//! Nothing here touches system settings; mDNS announcements use `slipstream-<fp>.local.`
//! (see `p2p_net::discovery::mdns`).

use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use p2p_crypto::NodeKeypair;
use p2p_forward::client::LocalAnswer;
use p2p_forward::proto::Versions;
use p2p_forward::status::{Compat, StatusRequest};
use p2p_forward::{ModelControl, StatusReply, UseHandle};
use p2p_net::discovery::mdns::{self, DiscoveryEvent};
use serde::Serialize;
use tokio::task::JoinHandle;

use crate::pairing::{attach_pair_protocol, local_info, share_info, PairCtx, PairingWindow, SystemClock, WindowState};
use crate::trust::{NodeTrust, TrustStore, TrustedPeer, WATCH_INTERVAL};
use crate::wake::WakeTargets;

/// Default share listen address (fixed port so "Not listed? Enter its address" has a stable
/// answer); falls back to a free port when 9003 is taken.
pub const DEFAULT_SHARE_LISTEN: &str = "/ip4/0.0.0.0/udp/9003/quic-v1";
/// Supervisor retry interval while the paired Mac cannot be reached.
pub const USE_RETRY: Duration = Duration::from_secs(5);

/// One data dir's identity and trust store (one instance per process, so every session sees
/// the same trust changes at once; another process's changes arrive via the file watcher).
pub struct Node {
    pub dir: PathBuf,
    pub node: NodeKeypair,
    pub store: Arc<TrustStore>,
    pub peer_id: PeerId,
    watcher: JoinHandle<()>,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.watcher.abort();
    }
}

impl Node {
    /// Must run inside a tokio runtime (spawns the trust-file watcher).
    pub fn open(dir: &Path) -> io::Result<Arc<Self>> {
        let node = p2p_forward::load_or_create_node(dir)?;
        let store = TrustStore::load(dir, &node)?;
        let watcher = store.spawn_watcher(WATCH_INTERVAL);
        let peer_id = p2p_forward::transport_identity(&node).public().to_peer_id();
        Ok(Arc::new(Self {
            dir: dir.to_path_buf(),
            node,
            store,
            peer_id,
            watcher,
        }))
    }

    pub fn fingerprint(&self) -> String {
        crate::lan::fingerprint(&self.peer_id)
    }
}

// ------------------------------------------------------------------- share

pub struct ShareOpts {
    /// Listen multiaddr; `None` = [`DEFAULT_SHARE_LISTEN`] with a free-port fallback.
    pub listen: Option<String>,
    pub upstream: SocketAddr,
    pub name: Option<String>,
    /// Claimed at pairing ("Paired Macs may start and stop the model"); the live answer
    /// is [`ModelControl::allowed`].
    pub may_start_model: bool,
    pub announce: bool,
    pub model: Option<Arc<dyn ModelControl>>,
    pub paired: p2p_libp2p::paired::PairedConfig,
    pub versions: Versions,
    pub verbose: bool,
}

impl ShareOpts {
    pub fn new(upstream: SocketAddr) -> Self {
        Self {
            listen: None,
            upstream,
            name: None,
            may_start_model: false,
            announce: true,
            model: None,
            paired: p2p_libp2p::paired::PairedConfig::default(),
            versions: Versions::default(),
            verbose: false,
        }
    }
}

/// The pairing window as the window shows it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PairingView {
    pub code: String,
    pub remaining_s: u64,
    /// open | paired | too_many_failures | expired | cancelled
    pub state: String,
    pub paired_name: Option<String>,
    pub failed_attempts: u32,
}

fn state_name(s: WindowState) -> &'static str {
    match s {
        WindowState::Open => "open",
        WindowState::Paired => "paired",
        WindowState::TooManyFailures => "too_many_failures",
        WindowState::Expired => "expired",
        WindowState::Cancelled => "cancelled",
    }
}

pub struct ShareSession {
    pub node: Arc<Node>,
    pub trust: Arc<NodeTrust>,
    pub handle: p2p_forward::ShareHandle,
    pub announced: bool,
    window: Mutex<Option<Arc<PairingWindow>>>,
    pair_task: JoinHandle<()>,
    _control: Option<crate::control::ControlServer>,
    _announcer: Option<crate::lan::ShareAnnouncer>,
}

impl Drop for ShareSession {
    fn drop(&mut self) {
        if let Some(w) = self.window.lock().unwrap().take() {
            w.cancel();
        }
        self.trust.set_window(None);
        self.pair_task.abort();
    }
}

impl ShareSession {
    /// Start sharing (works with no paired Mac yet: the app opens a pairing window next).
    pub async fn start(node: Arc<Node>, opts: ShareOpts) -> io::Result<Self> {
        let trust = NodeTrust::new(node.store.clone(), node.node.clone());
        let name = opts.name.unwrap_or_else(p2p_forward::status::default_name);
        let explicit = opts.listen.is_some();
        let listen: Multiaddr = opts
            .listen
            .as_deref()
            .unwrap_or(DEFAULT_SHARE_LISTEN)
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("bad listen address: {e}")))?;
        let cfg = |listen: Multiaddr| p2p_forward::ShareConfig {
            listen,
            upstream: opts.upstream,
            trust: trust.clone(),
            node: node.node.clone(),
            paired: opts.paired.clone(),
            admission: p2p_forward::admission::AdmissionLimits::default(),
            limits: p2p_forward::ShareLimits::default(),
            name: name.clone(),
            model: opts.model.clone(),
            versions: opts.versions,
            verbose: opts.verbose,
        };
        let identity = || p2p_forward::transport_identity(&node.node);
        let handle = match p2p_forward::start_share(identity(), cfg(listen.clone())).await {
            Ok(h) => h,
            Err(e) if !explicit => {
                eprintln!("share event=default_port_unavailable error={e}; using a free port");
                let any: Multiaddr = "/ip4/0.0.0.0/udp/0/quic-v1".parse().expect("static");
                p2p_forward::start_share(identity(), cfg(any)).await?
            }
            Err(e) => return Err(e),
        };
        let pair_task = attach_pair_protocol(
            handle.control(),
            PairCtx::new(trust.clone(), share_info(Some(name), opts.may_start_model)),
        )?;
        let fp = crate::lan::fingerprint(&handle.peer_id);
        let control = match crate::control::start_control_server(&node.dir, trust.clone(), handle.dial_addr(), fp).await
        {
            Ok(c) => Some(c),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                pair_task.abort();
                return Err(e);
            }
            Err(e) => {
                eprintln!("control socket unavailable ({e})");
                None
            }
        };
        let announcer = match crate::lan::udp_port(&handle.listen_addr) {
            Some(port) if opts.announce && crate::lan::should_announce(&listen) => {
                match crate::lan::start_share_announcer(handle.peer_id, port, trust.clone()) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        eprintln!("not announced on the local network: {e}");
                        None
                    }
                }
            }
            _ => None,
        };
        Ok(Self {
            announced: announcer.is_some(),
            node,
            trust,
            handle,
            window: Mutex::new(None),
            pair_task,
            _control: control,
            _announcer: announcer,
        })
    }

    /// `<listen>/p2p/<id>` (the manual-address fallback shows this with the LAN IP).
    pub fn dial_addr(&self) -> String {
        self.handle.dial_addr().to_string()
    }

    pub fn port(&self) -> Option<u16> {
        crate::lan::udp_port(&self.handle.listen_addr)
    }

    /// Open a new pairing window (replaces an open one) and return its code.
    pub fn open_pairing(&self) -> PairingView {
        let window = PairingWindow::open(Arc::new(SystemClock), self.node.store.epoch());
        if let Some(old) = self.window.lock().unwrap().replace(window.clone()) {
            old.cancel();
        }
        self.trust.set_window(Some(window));
        self.pairing().expect("just opened")
    }

    pub fn pairing(&self) -> Option<PairingView> {
        let w = self.window.lock().unwrap().clone()?;
        let state = w.state();
        Some(PairingView {
            code: w.code().as_str().to_string(),
            remaining_s: if state == WindowState::Open {
                w.remaining().as_secs()
            } else {
                0
            },
            state: state_name(state).into(),
            paired_name: w.paired_peer().map(|p| p.name),
            failed_attempts: w.failed_attempts(),
        })
    }

    pub fn cancel_pairing(&self) {
        if let Some(w) = self.window.lock().unwrap().take() {
            w.cancel();
        }
        self.trust.set_window(None);
    }

    /// (requests in flight, queued) from paired Macs.
    pub fn load(&self) -> (usize, usize) {
        self.handle.admission.load()
    }

    /// Forwarded requests holding an upstream connection (M5 busy accounting).
    pub fn forwarded(&self) -> usize {
        self.handle.forwarded()
    }
}

// --------------------------------------------------------------------- use

pub struct UseOpts {
    /// PeerId of the paired Mac to use.
    pub peer: String,
    /// A pinned address (`.../p2p/<PeerId>`) to dial besides the stored `last_addrs`
    /// (CLI `--peer /ip4/...`).
    pub addr: Option<Multiaddr>,
    pub bind: SocketAddr,
    /// Per-install bearer token (the app always sets it; the CLI with `--token-file`).
    pub token: Option<String>,
    pub app_origin: Option<String>,
    /// LAN discovery (mDNS browse) for the pinned Mac's current address.
    pub discovery: bool,
    pub paired: p2p_libp2p::paired::PairedConfig,
    pub spare_stream: bool,
    pub verbose: bool,
    /// Where Wake-on-LAN packets go (tests: a loopback socket; never the LAN in tests).
    pub wake: WakeTargets,
    /// "Waking…" phase length before "asleep or off" ([`crate::wake::WAKE_WINDOW`]).
    pub wake_window: Duration,
    /// Quiet re-dial time after a drop / at start before the waking phase begins.
    pub wake_grace: Duration,
    pub versions: Versions,
}

impl UseOpts {
    pub fn new(peer: impl Into<String>, bind: SocketAddr) -> Self {
        Self {
            peer: peer.into(),
            addr: None,
            bind,
            token: None,
            app_origin: None,
            discovery: true,
            paired: p2p_libp2p::paired::PairedConfig::default(),
            spare_stream: true,
            verbose: false,
            wake: WakeTargets::Broadcast,
            wake_window: crate::wake::WAKE_WINDOW,
            wake_grace: WAKE_GRACE,
            versions: Versions::default(),
        }
    }
}

/// Default [`UseOpts::wake_grace`].
pub const WAKE_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UseState {
    /// First dial in progress (quiet grace period at start).
    Connecting,
    /// The forwarder runs and the paired connection is up (versions compatible).
    Connected,
    /// The connection dropped; quiet re-dial for [`UseOpts::wake_grace`].
    Reconnecting,
    /// M5: not reachable — magic packets sent (if its MAC is known), re-dialing; editors get
    /// a 503 "waking" answer. Ends after [`UseOpts::wake_window`].
    Waking { remaining_s: u64, wol_sent: bool },
    /// M5: still unreachable after the waking phase: asleep, off or not on this network.
    /// Re-dialing continues; the window offers "use this Mac's model".
    Asleep { error: String },
    /// No address is known for this Mac (no LAN discovery, nothing stored).
    Unreachable { error: String },
    /// The editor port is taken by something else.
    PortBusy { error: String },
    /// M5: the two Macs speak different protocol versions. `side` = "peer" (the other Mac
    /// must update) or "self" (this Mac must update); `version` = the other Mac's app version.
    VersionMismatch { side: String, version: String },
    /// The other Mac removed this one (Unpair) — nothing more is tried.
    Removed,
}

struct UseInner {
    state: Mutex<UseState>,
    handle: Mutex<Option<Arc<UseHandle>>>,
    /// Last status of the other Mac (from the version check / the app's monitor).
    status: Mutex<Option<p2p_forward::Status>>,
}

pub struct UseSession {
    pub peer: PeerId,
    pub name: String,
    inner: Arc<UseInner>,
    task: JoinHandle<()>,
}

impl Drop for UseSession {
    fn drop(&mut self) {
        self.task.abort();
        // Release the editor port now (the supervisor's Arc goes with the aborted task).
        self.inner.handle.lock().unwrap().take();
    }
}

impl UseSession {
    /// Start the supervisor; returns at once (state `Connecting`).
    pub fn start(node: Arc<Node>, opts: UseOpts) -> Result<Self, String> {
        let peer: PeerId = opts.peer.parse().map_err(|_| "bad PeerId".to_string())?;
        let entry = node.store.get(&peer).ok_or("that Mac is not paired")?;
        let inner = Arc::new(UseInner {
            state: Mutex::new(UseState::Connecting),
            handle: Mutex::new(None),
            status: Mutex::new(None),
        });
        let sup = inner.clone();
        let task = tokio::spawn(async move { supervise(node, peer, opts, sup).await });
        Ok(Self {
            peer,
            name: entry.name,
            inner,
            task,
        })
    }

    pub fn state(&self) -> UseState {
        self.inner.state.lock().unwrap().clone()
    }

    pub fn handle(&self) -> Option<Arc<UseHandle>> {
        self.inner.handle.lock().unwrap().clone()
    }

    /// The last status the version check saw (the app refreshes it with [`rpc`](Self::rpc)).
    pub fn last_status(&self) -> Option<p2p_forward::Status> {
        self.inner.status.lock().unwrap().clone()
    }

    pub async fn rpc(&self, req: StatusRequest) -> io::Result<StatusReply> {
        let h = self
            .handle()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "not connected to the other Mac"))?;
        if !h.connected() {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "not connected to the other Mac"));
        }
        let r = h.rpc(req).await?;
        if let StatusReply::Status(s) = &r {
            *self.inner.status.lock().unwrap() = Some(s.clone());
        }
        Ok(r)
    }
}

/// The 503 answers editors get while the paired Mac is away (EN; the window shows its
/// own EN/DE text).
pub fn waking_answer(name: &str) -> LocalAnswer {
    LocalAnswer::new(
        503,
        "peer_waking",
        format!("{name} is not answering right now; Slipstream is trying to wake it. Try again in a few seconds."),
    )
}

pub fn asleep_answer(name: &str) -> LocalAnswer {
    LocalAnswer::new(
        503,
        "peer_asleep",
        format!(
            "{name} is asleep, turned off or not on this network. Wake it, or switch back to this Mac's own model in Slipstream (Your Macs)."
        ),
    )
}

/// The local answer (426) while the versions do not match.
pub fn version_answer(name: &str, side: &str, version: &str) -> LocalAnswer {
    let msg = if side == "self" {
        format!(
            "{name} runs Slipstream {version}, which needs a newer Slipstream on this Mac. Update this Mac to {} or newer.",
            p2p_forward::proto::PROTO_FIRST_APP
        )
    } else {
        format!(
            "{name} runs Slipstream {version}. Update it to {} or newer.",
            p2p_forward::proto::PROTO_FIRST_APP
        )
    };
    LocalAnswer::new(426, p2p_forward::status::VERSION_MISMATCH, msg)
}

/// What the version check found.
enum Check {
    Ok(p2p_forward::Status),
    Mismatch { side: &'static str, version: String },
    /// Could not ask (try again later).
    Unknown,
}

async fn version_check(handle: &UseHandle, versions: Versions, stored_version: &str) -> Check {
    match tokio::time::timeout(Duration::from_secs(10), handle.rpc(StatusRequest::Status)).await {
        Ok(Ok(StatusReply::Status(s))) => match s.compat(versions) {
            Compat::Ok => Check::Ok(s),
            Compat::PeerTooOld => Check::Mismatch {
                side: "peer",
                version: s.app_version.clone(),
            },
            Compat::SelfTooOld => Check::Mismatch {
                side: "self",
                version: s.app_version.clone(),
            },
        },
        Ok(Ok(StatusReply::Refused { code, .. })) if code == p2p_forward::status::VERSION_MISMATCH => Check::Mismatch {
            side: "self",
            version: stored_version.to_string(),
        },
        // The other Mac does not speak our status protocol at all: another version.
        Ok(Err(e)) if e.kind() == io::ErrorKind::Unsupported => Check::Mismatch {
            side: "peer",
            version: stored_version.to_string(),
        },
        _ => Check::Unknown,
    }
}

async fn supervise(node: Arc<Node>, peer: PeerId, opts: UseOpts, inner: Arc<UseInner>) {
    let set = |s: UseState| *inner.state.lock().unwrap() = s;
    let entry_name = node.store.get(&peer).map(|p| p.name).unwrap_or_else(|| "The other Mac".into());
    let (lan_inputs, _lan) = if opts.discovery {
        match crate::lan::start_use_discovery(node.store.clone(), peer, opts.verbose) {
            Ok((d, l)) => (Some(d), Some(l)),
            Err(e) => {
                eprintln!("use event=no_lan_discovery error={e}");
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    let trust = NodeTrust::new(node.store.clone(), node.node.clone());
    // `Unpair` from the big Mac (also its answer when a removed Mac reconnects) arrives on
    // the pair protocol: registered before the first dial.
    let pair_task: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::default();
    let hook: p2p_forward::client::BeforeDial = {
        let (slot, trust) = (pair_task.clone(), trust.clone());
        Arc::new(move |control| {
            if let Ok(t) = attach_pair_protocol(control, PairCtx::new(trust.clone(), local_info(None, false))) {
                if let Some(old) = slot.lock().unwrap().replace(t) {
                    old.abort();
                }
            }
        })
    };
    // 1. The forwarder: bound at once (persistent), dials in the background.
    let (handle, pair) = loop {
        if !node.store.is_trusted(&peer) {
            set(UseState::Removed);
            return;
        }
        let target = match &opts.addr {
            Some(a) => a.clone(),
            None => match crate::trust::resolve_use_peer(&node.store, Some(&peer.to_string())) {
                Ok((_, Some(addr))) => addr,
                Ok((id, None)) => Multiaddr::empty().with(Protocol::P2p(id)),
                Err(e) => {
                    set(UseState::Unreachable { error: e });
                    tokio::time::sleep(USE_RETRY).await;
                    continue;
                }
            },
        };
        if lan_inputs.is_none() && target.iter().count() <= 1 {
            set(UseState::Unreachable {
                error: "no known address for this Mac".into(),
            });
            tokio::time::sleep(USE_RETRY).await;
            continue;
        }
        let started = p2p_forward::start_use(
            p2p_forward::transport_identity(&node.node),
            p2p_forward::UseConfig {
                peer: target,
                trust: trust.clone(),
                node: node.node.clone(),
                bind: opts.bind,
                paired: opts.paired.clone(),
                spare_stream: opts.spare_stream,
                token: opts.token.clone(),
                local: p2p_forward::LocalLimits::default(),
                discovery: lan_inputs.clone(),
                app_origin: opts.app_origin.clone(),
                persistent: true,
                connect_wait: p2p_forward::client::CONNECT_WAIT,
                versions: opts.versions,
                before_dial: Some(hook.clone()),
                verbose: opts.verbose,
            },
        )
        .await;
        match started {
            Ok(h) => {
                let h = Arc::new(h);
                let pair = pair_task.lock().unwrap().take();
                h.set_offline(waking_answer(&entry_name));
                break (h, pair);
            }
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                set(UseState::PortBusy { error: e.to_string() });
                tokio::time::sleep(USE_RETRY).await;
            }
            Err(e) => {
                set(UseState::Unreachable { error: e.to_string() });
                tokio::time::sleep(USE_RETRY).await;
            }
        }
    };
    *inner.handle.lock().unwrap() = Some(handle.clone());

    // 2. Watch: connection, version, waking, removal.
    let t_start = tokio::time::Instant::now();
    let mut ever_connected = false;
    let mut checked = false;
    let mut mismatch: Option<(String, String)> = None;
    let mut down_since: Option<tokio::time::Instant> = Some(t_start);
    let mut last_wol: Option<tokio::time::Instant> = None;
    let mut wol_sent = false;
    loop {
        if !node.store.is_trusted(&peer) {
            inner.handle.lock().unwrap().take();
            if let Some(p) = pair {
                p.abort();
            }
            set(UseState::Removed);
            return;
        }
        if handle.connected() {
            ever_connected = true;
            down_since = None;
            last_wol = None;
            wol_sent = false;
            if !checked {
                let stored = node.store.get(&peer).map(|p| p.app_version).unwrap_or_default();
                match version_check(&handle, opts.versions, &stored).await {
                    Check::Ok(s) => {
                        checked = true;
                        mismatch = None;
                        handle.set_blocked(None);
                        *inner.status.lock().unwrap() = Some(s);
                    }
                    Check::Mismatch { side, version } => {
                        checked = true;
                        eprintln!("use event=version_mismatch peer={peer} side={side} version={version}");
                        handle.set_blocked(Some(version_answer(&entry_name, side, &version)));
                        mismatch = Some((side.to_string(), version));
                    }
                    Check::Unknown => {}
                }
            }
            set(match &mismatch {
                Some((side, version)) => UseState::VersionMismatch {
                    side: side.clone(),
                    version: version.clone(),
                },
                None => UseState::Connected,
            });
        } else {
            checked = false;
            let now = tokio::time::Instant::now();
            let since = *down_since.get_or_insert(now);
            let down = now.duration_since(since);
            if down < opts.wake_grace {
                handle.set_offline(waking_answer(&entry_name));
                set(if ever_connected { UseState::Reconnecting } else { UseState::Connecting });
            } else {
                let waking_for = down - opts.wake_grace;
                let resend = if waking_for < opts.wake_window {
                    crate::wake::WAKE_RESEND
                } else {
                    Duration::from_secs(60)
                };
                if last_wol.is_none_or(|t| now.duration_since(t) >= resend) {
                    last_wol = Some(now);
                    let macs = node.store.get(&peer).map(|p| p.wake_macs).unwrap_or_default();
                    let targets = opts.wake.clone();
                    let sent = tokio::task::spawn_blocking(move || {
                        crate::wake::send_wake(&macs, &targets.resolve()).unwrap_or(0)
                    })
                    .await
                    .unwrap_or(0);
                    if sent > 0 {
                        wol_sent = true;
                        eprintln!("use event=wake_sent peer={peer} datagrams={sent} unix_ms={}", p2p_forward::unix_ms());
                    }
                }
                if waking_for < opts.wake_window {
                    handle.set_offline(waking_answer(&entry_name));
                    set(UseState::Waking {
                        remaining_s: (opts.wake_window - waking_for).as_secs(),
                        wol_sent,
                    });
                } else {
                    handle.set_offline(asleep_answer(&entry_name));
                    set(UseState::Asleep {
                        error: handle
                            .last_error()
                            .unwrap_or_else(|| "no answer".into()),
                    });
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ------------------------------------------------------------------ browse

/// A sharing Mac seen on the LAN (announcement data is untrusted until a pinned dial or a
/// pairing proves it).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LanMac {
    pub fp: String,
    pub pairing_open: bool,
    /// `/ip4/<ip>/udp/<port>/quic-v1` (no `/p2p`) for pairing.
    pub addrs: Vec<String>,
    /// PeerId of the paired Mac with this fingerprint, if any.
    pub paired_peer: Option<String>,
    pub paired_name: Option<String>,
}

pub fn quic_addrs(ips: &[IpAddr], port: u16) -> Vec<Multiaddr> {
    let mut out: Vec<Multiaddr> = ips
        .iter()
        .map(|ip| {
            Multiaddr::empty()
                .with(match ip {
                    IpAddr::V4(v) => Protocol::Ip4(*v),
                    IpAddr::V6(v) => Protocol::Ip6(*v),
                })
                .with(Protocol::Udp(port))
                .with(Protocol::QuicV1)
        })
        .collect();
    out.sort_by_key(|a| !matches!(a.iter().next(), Some(Protocol::Ip4(_))));
    out
}

/// Continuous browse; started only after a user click (macOS Local Network prompt).
pub struct LanBrowser {
    seen: Arc<Mutex<BTreeMap<String, (bool, Vec<Multiaddr>)>>>,
    task: JoinHandle<()>,
}

impl Drop for LanBrowser {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl LanBrowser {
    pub fn start() -> Result<Self, String> {
        let mut d = mdns::browse_continuous().map_err(|e| e.to_string())?;
        let seen: Arc<Mutex<BTreeMap<String, (bool, Vec<Multiaddr>)>>> = Arc::default();
        let map = seen.clone();
        let task = tokio::spawn(async move {
            while let Some(ev) = d.recv().await {
                match ev {
                    DiscoveryEvent::Found {
                        fp,
                        addrs,
                        port,
                        pairing_open,
                    } => {
                        map.lock().unwrap().insert(fp, (pairing_open, quic_addrs(&addrs, port)));
                    }
                    DiscoveryEvent::Lost { fp } => {
                        map.lock().unwrap().remove(&fp);
                    }
                }
            }
        });
        Ok(Self { seen, task })
    }

    /// Current view, with the paired Macs of `store` matched by fingerprint. `own_fp` (this
    /// Mac's own share) is left out.
    pub fn list(&self, store: &TrustStore, own_fp: Option<&str>) -> Vec<LanMac> {
        let paired: Vec<(String, TrustedPeer)> = store
            .list()
            .into_iter()
            .filter_map(|p| p.peer().map(|id| (crate::lan::fingerprint(&id), p)))
            .collect();
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(fp, _)| Some(fp.as_str()) != own_fp)
            .map(|(fp, (open, addrs))| {
                let p = paired.iter().find(|(f, _)| f == fp).map(|(_, p)| p);
                LanMac {
                    fp: fp.clone(),
                    pairing_open: *open,
                    addrs: addrs.iter().map(|a| a.to_string()).collect(),
                    paired_peer: p.map(|p| p.peer_id.clone()),
                    paired_name: p.map(|p| p.name.clone()),
                }
            })
            .collect()
    }
}

// -------------------------------------------------------------------- pair

/// Client side of pairing from the app: `target` is a fingerprint (prefix) from the LAN
/// list — `known` carries its announced addresses, else it is looked up for ≤ 5 s — or an
/// address `/ip4/<ip>/udp/<port>/quic-v1[/p2p/<id>]` (manual fallback).
pub async fn pair_with_target(
    node: &Node,
    target: &str,
    known: &[String],
    code: &str,
    name: Option<String>,
) -> Result<TrustedPeer, crate::pairing::PairError> {
    use crate::pairing::PairError;
    let code = p2p_crypto::PairingCode::parse(code).map_err(|e| PairError::Protocol(e.to_string()))?;
    let target = target.trim();
    let (addrs, expect_fp): (Vec<Multiaddr>, Option<String>) = if target.starts_with('/') {
        let a: Multiaddr = target
            .parse()
            .map_err(|e| PairError::Protocol(format!("bad address: {e}")))?;
        (vec![a], None)
    } else if crate::lan::looks_like_fp(target) {
        let parsed: Vec<Multiaddr> = known.iter().filter_map(|a| a.parse().ok()).collect();
        if parsed.is_empty() {
            let (fp, addrs) = crate::lan::resolve_fp(target, Duration::from_secs(5))
                .await
                .map_err(PairError::Unreachable)?;
            (addrs, Some(fp))
        } else {
            (parsed, Some(target.to_string()))
        }
    } else {
        return Err(PairError::Protocol("not a fingerprint or an address".into()));
    };
    crate::pairing::pair_with_checked(
        &node.node,
        &node.store,
        &addrs,
        &code,
        local_info(name, false),
        &p2p_libp2p::paired::PairedConfig::default(),
        expect_fp.as_deref(),
    )
    .await
}
