//! Peer discovery: bootstrap list (primary) + optional mDNS (`mdns` feature: v0.6 LAN
//! discovery of sharing Macs, `_slipstream._udp`).

use std::net::SocketAddr;
use std::str::FromStr;

use crate::error::NetError;

/// A discovered or configured peer endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerAddr {
    pub label: Option<String>,
    pub addr: SocketAddr,
}

impl PeerAddr {
    pub fn new(addr: SocketAddr) -> Self {
        Self { label: None, addr }
    }

    pub fn with_label(addr: SocketAddr, label: impl Into<String>) -> Self {
        Self {
            label: Some(label.into()),
            addr,
        }
    }
}

impl FromStr for PeerAddr {
    type Err = NetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // "label@host:port" or "host:port"
        if let Some((label, rest)) = s.split_once('@') {
            let addr: SocketAddr = rest
                .parse()
                .map_err(|e| NetError::protocol(format!("bad peer addr '{s}': {e}")))?;
            Ok(Self::with_label(addr, label))
        } else {
            let addr: SocketAddr = s
                .parse()
                .map_err(|e| NetError::protocol(format!("bad peer addr '{s}': {e}")))?;
            Ok(Self::new(addr))
        }
    }
}

/// Explicit bootstrap peers — enough for LAN/dev and deterministic tests.
#[derive(Debug, Clone, Default)]
pub struct BootstrapList {
    peers: Vec<PeerAddr>,
}

impl BootstrapList {
    pub fn new(peers: impl IntoIterator<Item = PeerAddr>) -> Self {
        Self {
            peers: peers.into_iter().collect(),
        }
    }

    pub fn from_addrs(addrs: impl IntoIterator<Item = SocketAddr>) -> Self {
        Self::new(addrs.into_iter().map(PeerAddr::new))
    }

    /// Parse a comma-separated list: `a@127.0.0.1:9001,127.0.0.1:9002`.
    pub fn parse_csv(csv: &str) -> Result<Self, NetError> {
        let mut peers = Vec::new();
        for part in csv.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            peers.push(part.parse()?);
        }
        Ok(Self::new(peers))
    }

    pub fn peers(&self) -> &[PeerAddr] {
        &self.peers
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    pub fn push(&mut self, peer: PeerAddr) {
        self.peers.push(peer);
    }
}

/// Optional mDNS discovery (feature `mdns`).
///
/// v0.6 "Use your other Mac" (docs/plans/2026-11-28-use-your-other-mac.md §1, M3):
/// [`SERVICE_TYPE`] `_slipstream._udp.local.` (QUIC is UDP). A sharing Mac announces
/// itself with [`announce_share`]; a using Mac listens with [`browse_continuous`].
///
/// Privacy: the instance is `Slipstream-<fp4>` and the TXT record is **exactly**
/// `v=1`, `fp=<16 hex>` and — only while a pairing window is open — `pair=1`. No names,
/// models, RAM or versions; those travel only over the authenticated channel.
///
/// Trust: everything learned here is **untrusted** (anyone on the LAN can announce any
/// fingerprint). Callers match `fp` against their pinned peers and dial the announced
/// addresses only with the pinned `/p2p/<PeerId>` suffix, so the QUIC TLS check refuses
/// an impostor.
///
/// The older one-shot helpers ([`announce`], [`browse`]) for the TCP runtime are kept on
/// [`LEGACY_SERVICE_TYPE`].
#[cfg(feature = "mdns")]
pub mod mdns {
    use std::collections::{BTreeSet, HashMap};
    use std::net::{IpAddr, SocketAddr};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo, UnregisterStatus};
    use tokio::sync::mpsc;

    use super::PeerAddr;
    use crate::error::NetError;

    /// v0.6 share announcement (UDP: QUIC).
    pub const SERVICE_TYPE: &str = "_slipstream._udp.local.";
    /// The `NSBonjourServices` entry for macOS (Info.plist).
    pub const BONJOUR_SERVICE: &str = "_slipstream._udp";
    /// Old TCP runtime service type (one-shot [`announce`] / [`browse`]).
    pub const LEGACY_SERVICE_TYPE: &str = "_slipstream-p2p._tcp.local.";
    /// TXT `v` value this build announces and accepts.
    pub const TXT_VERSION: &str = "1";
    /// Interface poll period for [`browse_continuous`] (plan §5 network change).
    pub const IFACE_POLL: Duration = Duration::from_secs(5);
    /// After a re-browse, instances that did not answer again within this time are lost.
    pub const REBROWSE_GRACE: Duration = Duration::from_secs(3);
    /// Periodic re-verification: re-browse this often while any share is known, so an
    /// instance whose goodbye was missed (observed ~1 in 8 same-host CLI runs with
    /// mdns-sd 0.22: the using process never saw the `ServiceRemoved`) is dropped within
    /// `REVERIFY_EVERY + REBROWSE_GRACE` instead of its 75-minute PTR TTL.
    pub const REVERIFY_EVERY: Duration = Duration::from_secs(10);
    /// Re-browse at most this often (dial failures can come in bursts).
    pub const REBROWSE_MIN_GAP: Duration = Duration::from_secs(1);

    fn err(what: &str, e: impl std::fmt::Display) -> NetError {
        NetError::protocol(format!("mdns {what}: {e}"))
    }

    /// Transport the share listens on. Only QUIC v1 exists; it selects [`SERVICE_TYPE`]
    /// and is deliberately **not** put into the TXT record.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum ShareProto {
        #[default]
        QuicV1,
    }

    /// What a sharing Mac announces.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ShareAdvert {
        /// 16 lowercase hex chars: the first 8 bytes of SHA-256(PeerId bytes).
        pub fp: String,
        pub proto: ShareProto,
        /// `pair=1` is added only while this is true.
        pub pairing_open: bool,
        /// UDP port of the QUIC listener.
        pub port: u16,
    }

    /// `fp` must be exactly 16 lowercase hex chars.
    pub fn valid_fp(fp: &str) -> bool {
        fp.len() == 16 && fp.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }

    /// `Slipstream-<first 4 hex of fp>`.
    pub fn instance_name(fp: &str) -> String {
        format!("Slipstream-{}", &fp[..4.min(fp.len())])
    }

    /// The exact TXT key/value set for `advert` (sorted by key).
    pub fn txt_properties(advert: &ShareAdvert) -> Vec<(String, String)> {
        let mut props = vec![
            ("fp".to_string(), advert.fp.clone()),
            ("v".to_string(), TXT_VERSION.to_string()),
        ];
        if advert.pairing_open {
            props.insert(1, ("pair".to_string(), "1".to_string()));
        }
        props
    }

    /// Whether this host has an up, non-loopback IPv4 interface — mDNS announcements are
    /// sent on those (mdns-sd does not announce loopback addresses), so same-host tests
    /// skip without one.
    pub fn lan_ipv4_available() -> bool {
        if_addrs::get_if_addrs()
            .map(|v| v.iter().any(|i| !i.is_loopback() && i.ip().is_ipv4()))
            .unwrap_or(false)
    }

    /// The mDNS host name the share's SRV record points at: `slipstream-<fp>.local.`.
    ///
    /// Not the system host name: mdns-sd publishes its own A/AAAA records for the SRV
    /// target (incl. loopback / interfaces mDNSResponder does not announce), and on the
    /// macOS test host (2026-11-28) announcing the system `LocalHostName` made
    /// mDNSResponder see a conflict and **rename the Mac** (`<name>` → `<name>-2`). A
    /// per-identity name cannot collide with the system responder; it leaks nothing beyond
    /// the fingerprint that is already in the TXT record.
    pub fn share_hostname(fp: &str) -> String {
        format!("slipstream-{fp}.local.")
    }

    fn new_daemon() -> Result<ServiceDaemon, NetError> {
        let daemon = ServiceDaemon::new().map_err(|e| err("daemon", e))?;
        Ok(daemon)
    }

    fn service_info(advert: &ShareAdvert, hostname: &str) -> Result<ServiceInfo, NetError> {
        let props: HashMap<String, String> = txt_properties(advert).into_iter().collect();
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name(&advert.fp),
            hostname,
            "",
            advert.port,
            props,
        )
        .map_err(|e| err("service info", e))?
        .enable_addr_auto();
        Ok(info)
    }

    /// A live announcement. Change the pairing flag with [`ShareHandle::set_pairing_open`]
    /// (re-registers with the new TXT); dropping it unregisters (mDNS goodbye) and stops
    /// the responder.
    pub struct ShareHandle {
        daemon: ServiceDaemon,
        hostname: String,
        fullname: String,
        advert: Mutex<ShareAdvert>,
    }

    impl std::fmt::Debug for ShareHandle {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ShareHandle")
                .field("fullname", &self.fullname)
                .field("hostname", &self.hostname)
                .finish()
        }
    }

    impl ShareHandle {
        pub fn fullname(&self) -> &str {
            &self.fullname
        }

        pub fn hostname(&self) -> &str {
            &self.hostname
        }

        pub fn advert(&self) -> ShareAdvert {
            self.advert.lock().unwrap().clone()
        }

        /// Replace the announcement (same instance name; re-registered if anything changed).
        pub fn update(&self, advert: ShareAdvert) -> Result<(), NetError> {
            if !valid_fp(&advert.fp) {
                return Err(NetError::protocol("mdns: fp must be 16 lowercase hex chars"));
            }
            let mut cur = self.advert.lock().unwrap();
            if *cur == advert {
                return Ok(());
            }
            if cur.fp != advert.fp {
                return Err(NetError::protocol("mdns: fp of a running announcement cannot change"));
            }
            let info = service_info(&advert, &self.hostname)?;
            self.daemon.register(info).map_err(|e| err("re-register", e))?;
            *cur = advert;
            Ok(())
        }

        /// Add (`true`) or remove (`false`) `pair=1`.
        pub fn set_pairing_open(&self, open: bool) -> Result<(), NetError> {
            let mut next = self.advert();
            next.pairing_open = open;
            self.update(next)
        }
    }

    impl Drop for ShareHandle {
        fn drop(&mut self) {
            // Goodbye packets (TTL 0) so browsers see `Lost` at once, then stop.
            if let Ok(rx) = self.daemon.unregister(&self.fullname) {
                let _: Option<UnregisterStatus> = rx.recv_timeout(Duration::from_secs(1)).ok();
                // mdns-sd repeats the goodbye after 120 ms; let it go out before shutdown.
                std::thread::sleep(Duration::from_millis(200));
            }
            if let Ok(rx) = self.daemon.shutdown() {
                let _ = rx.recv_timeout(Duration::from_secs(1));
            }
        }
    }

    /// Announce a sharing Mac on the local network (all interfaces, addresses follow
    /// interface changes), host name = [`share_hostname`].
    pub fn announce_share(advert: &ShareAdvert) -> Result<ShareHandle, NetError> {
        if !valid_fp(&advert.fp) {
            return Err(NetError::protocol("mdns: fp must be 16 lowercase hex chars"));
        }
        let hostname = share_hostname(&advert.fp);
        let daemon = new_daemon()?;
        let info = service_info(advert, &hostname)?;
        let fullname = info.get_fullname().to_string();
        daemon.register(info).map_err(|e| err("register", e))?;
        Ok(ShareHandle {
            daemon,
            hostname,
            fullname,
            advert: Mutex::new(advert.clone()),
        })
    }

    /// What [`browse_continuous`] reports. Untrusted data.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum DiscoveryEvent {
        /// A share with this fingerprint is announced (or its addresses / port / pairing
        /// flag changed). `addrs` is the union over every instance claiming `fp` (an
        /// impostor cannot hide the real Mac's addresses), IPv6 link-local excluded
        /// (a multiaddr cannot carry the scope).
        Found {
            fp: String,
            addrs: Vec<IpAddr>,
            port: u16,
            pairing_open: bool,
        },
        /// No instance announces `fp` any more (goodbye, or not answering a re-browse).
        Lost { fp: String },
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Instance {
        fp: String,
        addrs: BTreeSet<IpAddr>,
        port: u16,
        pairing_open: bool,
        /// Last time it (re-)resolved.
        seen: Instant,
    }

    /// Parse a resolved service: `None` if the TXT is not a valid v1 Slipstream record.
    fn parse_resolved(info: &mdns_sd::ResolvedService) -> Option<(String, Instance)> {
        let props = info.get_properties();
        if props.get_property_val_str("v") != Some(TXT_VERSION) {
            return None;
        }
        let fp = props.get_property_val_str("fp")?.to_string();
        if !valid_fp(&fp) {
            return None;
        }
        let pairing_open = props.get_property_val_str("pair") == Some("1");
        let addrs = info
            .get_addresses()
            .iter()
            .map(|ip| ip.to_ip_addr())
            .filter(|ip| match ip {
                IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
                IpAddr::V4(v4) => !v4.is_unspecified(),
            })
            .collect();
        Some((
            info.get_fullname().to_string(),
            Instance {
                fp,
                addrs,
                port: info.get_port(),
                pairing_open,
                seen: Instant::now(),
            },
        ))
    }

    /// Per-fp view: union of addresses over all instances, newest port, any pairing flag.
    fn aggregate(instances: &HashMap<String, Instance>, fp: &str) -> Option<DiscoveryEvent> {
        let mine: Vec<&Instance> = instances.values().filter(|i| i.fp == fp).collect();
        let newest = mine.iter().max_by_key(|i| i.seen)?;
        let mut addrs: BTreeSet<IpAddr> = BTreeSet::new();
        for i in &mine {
            addrs.extend(i.addrs.iter().copied());
        }
        Some(DiscoveryEvent::Found {
            fp: fp.to_string(),
            addrs: addrs.into_iter().collect(),
            port: newest.port,
            pairing_open: mine.iter().any(|i| i.pairing_open),
        })
    }

    struct BrowseCtl {
        stop: AtomicBool,
        rebrowse: AtomicBool,
    }

    /// Continuous browse: events via [`Discovery::recv`] / [`Discovery::try_recv`];
    /// [`Discovery::rebrowse`] forces a fresh query (call it on any dial failure). The
    /// browser also re-queries by itself when the set of interface addresses changes
    /// (polled every [`IFACE_POLL`]). Dropping it stops the browser.
    pub struct Discovery {
        events: mpsc::UnboundedReceiver<DiscoveryEvent>,
        ctl: Arc<BrowseCtl>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    /// A cloneable handle that can ask a running [`Discovery`] to re-browse.
    #[derive(Clone)]
    pub struct Rebrowse(Arc<BrowseCtl>);

    impl Rebrowse {
        pub fn rebrowse(&self) {
            self.0.rebrowse.store(true, Ordering::SeqCst);
        }
    }

    impl Discovery {
        pub async fn recv(&mut self) -> Option<DiscoveryEvent> {
            self.events.recv().await
        }

        pub fn try_recv(&mut self) -> Option<DiscoveryEvent> {
            self.events.try_recv().ok()
        }

        /// Blocking receive with a timeout (no tokio runtime needed).
        pub fn recv_timeout(&mut self, timeout: Duration) -> Option<DiscoveryEvent> {
            let deadline = Instant::now() + timeout;
            loop {
                match self.events.try_recv() {
                    Ok(e) => return Some(e),
                    Err(mpsc::error::TryRecvError::Disconnected) => return None,
                    Err(mpsc::error::TryRecvError::Empty) => {}
                }
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        pub fn rebrowse(&self) {
            self.ctl.rebrowse.store(true, Ordering::SeqCst);
        }

        pub fn rebrowse_handle(&self) -> Rebrowse {
            Rebrowse(self.ctl.clone())
        }
    }

    impl Drop for Discovery {
        fn drop(&mut self) {
            self.ctl.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn iface_snapshot() -> BTreeSet<(String, IpAddr)> {
        if_addrs::get_if_addrs()
            .map(|v| v.into_iter().map(|i| (i.name.clone(), i.ip())).collect())
            .unwrap_or_default()
    }

    /// Start browsing [`SERVICE_TYPE`] until the returned [`Discovery`] is dropped.
    pub fn browse_continuous() -> Result<Discovery, NetError> {
        let daemon = new_daemon()?;
        let mut receiver = daemon.browse(SERVICE_TYPE).map_err(|e| err("browse", e))?;
        let (tx, rx) = mpsc::unbounded_channel();
        let ctl = Arc::new(BrowseCtl {
            stop: AtomicBool::new(false),
            rebrowse: AtomicBool::new(false),
        });
        let thread_ctl = ctl.clone();
        let thread = std::thread::Builder::new()
            .name("slipstream-mdns-browse".into())
            .spawn(move || {
                // SLIPSTREAM_MDNS_DEBUG=1 prints every raw browse event (diagnostics).
                let debug = std::env::var_os("SLIPSTREAM_MDNS_DEBUG").is_some();
                let mut instances: HashMap<String, Instance> = HashMap::new();
                let mut last_sent: HashMap<String, DiscoveryEvent> = HashMap::new();
                let mut ifaces = iface_snapshot();
                let mut next_poll = Instant::now() + IFACE_POLL;
                let mut last_rebrowse = Instant::now();
                let mut next_reverify = Instant::now() + REVERIFY_EVERY;
                // set after a re-browse: drop instances not seen since this instant
                let mut sweep_at: Option<(Instant, Instant)> = None;
                let emit = |fp: &str,
                            instances: &HashMap<String, Instance>,
                            last_sent: &mut HashMap<String, DiscoveryEvent>|
                 -> bool {
                    let ev = aggregate(instances, fp).unwrap_or(DiscoveryEvent::Lost { fp: fp.to_string() });
                    let changed = match (&ev, last_sent.get(fp)) {
                        (DiscoveryEvent::Lost { .. }, None) => false,
                        (e, Some(prev)) => e != prev,
                        (_, None) => true,
                    };
                    if !changed {
                        return true;
                    }
                    if matches!(ev, DiscoveryEvent::Lost { .. }) {
                        last_sent.remove(fp);
                    } else {
                        last_sent.insert(fp.to_string(), ev.clone());
                    }
                    tx.send(ev).is_ok()
                };
                while !thread_ctl.stop.load(Ordering::SeqCst) {
                    let now = Instant::now();
                    if now >= next_poll {
                        next_poll = now + IFACE_POLL;
                        let snap = iface_snapshot();
                        if snap != ifaces {
                            ifaces = snap;
                            thread_ctl.rebrowse.store(true, Ordering::SeqCst);
                        }
                    }
                    if now >= next_reverify {
                        next_reverify = now + REVERIFY_EVERY;
                        if !instances.is_empty() {
                            thread_ctl.rebrowse.store(true, Ordering::SeqCst);
                        }
                    }
                    if thread_ctl.rebrowse.load(Ordering::SeqCst)
                        && now.duration_since(last_rebrowse) >= REBROWSE_MIN_GAP
                    {
                        thread_ctl.rebrowse.store(false, Ordering::SeqCst);
                        last_rebrowse = now;
                        let _ = daemon.stop_browse(SERVICE_TYPE);
                        // drain the old receiver (SearchStopped)
                        while receiver.try_recv().is_ok() {}
                        match daemon.browse(SERVICE_TYPE) {
                            Ok(r) => receiver = r,
                            Err(_) => break,
                        }
                        sweep_at = Some((now + REBROWSE_GRACE, now));
                        next_reverify = now + REVERIFY_EVERY;
                    }
                    if let Some((at, since)) = sweep_at {
                        if Instant::now() >= at {
                            sweep_at = None;
                            let stale: Vec<String> = instances
                                .iter()
                                .filter(|(_, i)| i.seen < since)
                                .map(|(k, _)| k.clone())
                                .collect();
                            let mut fps = BTreeSet::new();
                            for k in stale {
                                if let Some(i) = instances.remove(&k) {
                                    fps.insert(i.fp);
                                }
                            }
                            for fp in fps {
                                if !emit(&fp, &instances, &mut last_sent) {
                                    return;
                                }
                            }
                        }
                    }
                    let event = match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(e) => e,
                        Err(flume_err) => {
                            if receiver.is_disconnected() {
                                let _ = flume_err;
                                break;
                            }
                            continue;
                        }
                    };
                    if debug {
                        eprintln!("mdns debug event={event:?}");
                    }
                    match event {
                        ServiceEvent::ServiceResolved(info) => {
                            let Some((name, inst)) = parse_resolved(&info) else { continue };
                            let mut fps = BTreeSet::from([inst.fp.clone()]);
                            if let Some(old) = instances.insert(name, inst) {
                                fps.insert(old.fp);
                            }
                            for fp in fps {
                                if !emit(&fp, &instances, &mut last_sent) {
                                    return;
                                }
                            }
                        }
                        ServiceEvent::ServiceRemoved(_, name) => {
                            if let Some(old) = instances.remove(&name) {
                                if !emit(&old.fp, &instances, &mut last_sent) {
                                    return;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                let _ = daemon.stop_browse(SERVICE_TYPE);
                if let Ok(rx) = daemon.shutdown() {
                    let _ = rx.recv_timeout(Duration::from_secs(1));
                }
            })
            .map_err(|e| err("browse thread", e))?;
        Ok(Discovery {
            events: rx,
            ctl,
            thread: Some(thread),
        })
    }

    /// Old TCP runtime: announce this node on [`LEGACY_SERVICE_TYPE`].
    pub fn announce(instance: &str, port: u16, host_ipv4: &str) -> Result<ServiceDaemon, NetError> {
        let daemon = new_daemon()?;
        let props: HashMap<String, String> = HashMap::new();
        let info = ServiceInfo::new(
            LEGACY_SERVICE_TYPE,
            instance,
            &format!("{host_ipv4}.local."),
            host_ipv4,
            port,
            props,
        )
        .map_err(|e| err("service info", e))?;
        daemon.register(info).map_err(|e| err("register", e))?;
        Ok(daemon)
    }

    /// Old TCP runtime: browse [`LEGACY_SERVICE_TYPE`] briefly; returns whatever answers
    /// within `timeout`.
    pub fn browse(timeout: Duration) -> Result<Vec<PeerAddr>, NetError> {
        let daemon = new_daemon()?;
        let receiver = daemon.browse(LEGACY_SERVICE_TYPE).map_err(|e| err("browse", e))?;
        let deadline = Instant::now() + timeout;
        let mut found = Vec::new();
        while Instant::now() < deadline {
            let remain = deadline.saturating_duration_since(Instant::now());
            match receiver.recv_timeout(remain.min(Duration::from_millis(200))) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let port = info.get_port();
                    // mdns-sd 0.22 yields scoped IPs; drop the interface scope.
                    for ip in info.get_addresses() {
                        let addr = SocketAddr::new(ip.to_ip_addr(), port);
                        found.push(PeerAddr::with_label(addr, info.get_fullname()));
                    }
                }
                Ok(_) => {}
                Err(_) if receiver.is_disconnected() => break,
                Err(_) => {}
            }
        }
        let _ = daemon.shutdown();
        Ok(found)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn advert(pair: bool) -> ShareAdvert {
            ShareAdvert {
                fp: "0123456789abcdef".into(),
                proto: ShareProto::QuicV1,
                pairing_open: pair,
                port: 9003,
            }
        }

        #[test]
        fn txt_is_exactly_v_fp_and_pair_only_while_open() {
            let keys = |a: &ShareAdvert| txt_properties(a).into_iter().map(|(k, _)| k).collect::<Vec<_>>();
            assert_eq!(keys(&advert(false)), ["fp", "v"]);
            assert_eq!(keys(&advert(true)), ["fp", "pair", "v"]);
            assert_eq!(
                txt_properties(&advert(true)),
                vec![
                    ("fp".into(), "0123456789abcdef".into()),
                    ("pair".into(), "1".into()),
                    ("v".into(), "1".into())
                ]
            );
            assert_eq!(instance_name("0123456789abcdef"), "Slipstream-0123");
            // the ServiceInfo carries the same set and nothing else
            let info = service_info(&advert(true), "host.local.").unwrap();
            let mut got: Vec<(String, String)> = info
                .get_properties()
                .iter()
                .map(|p| (p.key().to_string(), p.val_str().to_string()))
                .collect();
            got.sort();
            assert_eq!(got, txt_properties(&advert(true)));
            assert_eq!(info.get_type(), SERVICE_TYPE);
            assert_eq!(info.get_hostname(), "host.local.");
        }

        #[test]
        fn fp_validation_and_hostnames() {
            assert!(valid_fp("0123456789abcdef"));
            assert!(!valid_fp("0123456789ABCDEF"));
            assert!(!valid_fp("0123456789abcde"));
            assert!(!valid_fp("0123456789abcdeg"));
            assert_eq!(share_hostname("0123456789abcdef"), "slipstream-0123456789abcdef.local.");
        }

        #[test]
        fn aggregate_unions_addresses_over_instances() {
            let mut m = HashMap::new();
            let t0 = Instant::now();
            m.insert(
                "a".to_string(),
                Instance {
                    fp: "0123456789abcdef".into(),
                    addrs: BTreeSet::from(["10.0.0.2".parse().unwrap()]),
                    port: 1,
                    pairing_open: false,
                    seen: t0,
                },
            );
            m.insert(
                "b".to_string(),
                Instance {
                    fp: "0123456789abcdef".into(),
                    addrs: BTreeSet::from(["10.0.0.9".parse().unwrap()]),
                    port: 2,
                    pairing_open: true,
                    seen: t0 + Duration::from_millis(1),
                },
            );
            assert_eq!(
                aggregate(&m, "0123456789abcdef"),
                Some(DiscoveryEvent::Found {
                    fp: "0123456789abcdef".into(),
                    addrs: vec!["10.0.0.2".parse().unwrap(), "10.0.0.9".parse().unwrap()],
                    port: 2,
                    pairing_open: true
                })
            );
            assert_eq!(aggregate(&m, "ffffffffffffffff"), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn parse_csv_with_labels() {
        let list = BootstrapList::parse_csv("alice@127.0.0.1:9001, 127.0.0.1:9002").unwrap();
        assert_eq!(list.peers().len(), 2);
        assert_eq!(list.peers()[0].label.as_deref(), Some("alice"));
        assert_eq!(
            list.peers()[0].addr,
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9001))
        );
        assert!(list.peers()[1].label.is_none());
    }

    #[test]
    fn empty_csv() {
        assert!(BootstrapList::parse_csv("  , ").unwrap().is_empty());
    }

    #[test]
    fn bad_peer_addr_is_actionable() {
        let err = BootstrapList::parse_csv("not-a-peer").unwrap_err();
        let s = err.to_string();
        assert!(s.contains("bad peer addr"), "display={s}");
        assert!(s.contains("not-a-peer"), "display={s}");
    }
}
