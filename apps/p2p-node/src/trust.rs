//! Trust store of paired Macs (v0.6 M2, docs/plans/2026-11-28-use-your-other-mac.md §2).
//!
//! `<data_dir>/trusted_macs.json`, written atomically (new temp file opened with mode
//! 0600 + `create_new`, fsync, rename over the old file) and protected by an HMAC-SHA256
//! over the serialized body, keyed by `HKDF-SHA256(node secret, "trust-store")`. A file
//! whose MAC does not verify (edited by hand, by another user or by malware, or written
//! under another node key) is **treated as empty** and [`TrustStore::pairings_reset`] is
//! set, so the UI can say "pairings were reset" — never fall back to trusting it.
//!
//! The store is shared between processes (the running `share`/`use` and the
//! `trust revoke` command): a watcher ([`TrustStore::spawn_watcher`]) polls the file every
//! [`WATCH_INTERVAL`] and reloads on change, so a revoke in another process takes effect
//! in the running one within ≲ 0.25 s (connections closed, active streams aborted).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use libp2p::PeerId;
use p2p_crypto::NodeKeypair;
use p2p_forward::trust::{BoxFuture, PeerTrust, TrustEpoch};
use p2p_libp2p::paired::Control;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::watch;
use zeroize::Zeroizing;

use crate::pairing::PairingWindow;

pub const TRUST_FILE: &str = "trusted_macs.json";
pub const TRUST_FILE_VERSION: u32 = 1;
/// How often running processes check the file for changes made by other processes.
pub const WATCH_INTERVAL: Duration = Duration::from_millis(250);
const MAC_SALT: &[u8] = b"slipstream-trust-store-v1";
const MAC_INFO: &[u8] = b"trust-store";
const MAX_FILE: u64 = 1024 * 1024;

/// The other Mac's role relative to this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerRole {
    /// It shares its model with this Mac (we dial it: `slipstream-node use`).
    Share,
    /// It uses this Mac's model (it dials us: `slipstream-node share`).
    Use,
}

impl std::fmt::Display for PeerRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PeerRole::Share => "shares with this Mac",
            PeerRole::Use => "uses this Mac",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    /// libp2p PeerId (pinned for dialing and the QUIC TLS check).
    pub peer_id: String,
    /// The peer's Ed25519 identity public key (hex) — the key inside the PeerId.
    pub identity_hex: String,
    /// The peer's static X25519 public key (hex), confirmed during pairing; mixed into
    /// every SealedStream handshake.
    pub encryption_node_id: String,
    pub name: String,
    pub role: PeerRole,
    /// Unix seconds.
    pub paired_at: u64,
    pub last_addrs: Vec<String>,
    pub last_seen: Option<u64>,
    pub app_version: String,
    /// `role = share`: the other Mac lets us start/stop its model. `role = use`: we let it
    /// start/stop ours.
    pub may_start_model: bool,
}

impl TrustedPeer {
    pub fn peer(&self) -> Option<PeerId> {
        self.peer_id.parse().ok()
    }

    pub fn static_key(&self) -> Option<[u8; 32]> {
        let bytes = hex::decode(&self.encryption_node_id).ok()?;
        bytes.try_into().ok()
    }

    fn valid(&self) -> bool {
        self.peer().is_some() && self.static_key().is_some()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Body {
    version: u32,
    peers: Vec<TrustedPeer>,
}

#[derive(Serialize, Deserialize)]
struct FileFormat {
    body: Body,
    mac: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    mtime_ns: i128,
    ino: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let m = fs::symlink_metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(Stamp {
            len: m.len(),
            mtime_ns: m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128,
            ino: m.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let mtime_ns = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0);
        Some(Stamp { len: m.len(), mtime_ns, ino: 0 })
    }
}

struct State {
    peers: Vec<TrustedPeer>,
    pairings_reset: bool,
    stamp: Option<Stamp>,
}

/// Outcome of reading the file.
enum Loaded {
    Missing,
    Valid(Vec<TrustedPeer>),
    Tampered,
}

pub struct TrustStore {
    path: PathBuf,
    key: Zeroizing<[u8; 32]>,
    state: Mutex<State>,
    epoch: TrustEpoch,
}

type HmacSha256 = Hmac<Sha256>;

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl TrustStore {
    /// Load `<dir>/trusted_macs.json` with the MAC key derived from `node`'s secret.
    /// Missing file → empty; tampered / unparseable → empty + `pairings_reset`.
    pub fn load(dir: &Path, node: &NodeKeypair) -> io::Result<Arc<Self>> {
        fs::create_dir_all(dir)?;
        let hk = Hkdf::<Sha256>::new(Some(MAC_SALT), &Zeroizing::new(node.secret_bytes())[..]);
        let mut key = Zeroizing::new([0u8; 32]);
        hk.expand(MAC_INFO, &mut key[..]).expect("32-byte HKDF output");
        let store = Arc::new(Self {
            path: dir.join(TRUST_FILE),
            key,
            state: Mutex::new(State {
                peers: Vec::new(),
                pairings_reset: false,
                stamp: None,
            }),
            epoch: TrustEpoch::default(),
        });
        store.reload()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn epoch(&self) -> TrustEpoch {
        self.epoch.clone()
    }

    fn mac(&self, body: &Body) -> Vec<u8> {
        let bytes = serde_json::to_vec(body).expect("body serializes");
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.key[..]).expect("any key length");
        mac.update(&bytes);
        mac.finalize().into_bytes().to_vec()
    }

    fn read_file(&self) -> io::Result<(Loaded, Option<Stamp>)> {
        let st = stamp(&self.path);
        let bytes = match fs::File::open(&self.path) {
            Ok(f) => {
                use std::io::Read;
                let mut buf = Vec::new();
                f.take(MAX_FILE + 1).read_to_end(&mut buf)?;
                buf
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Loaded::Missing, None)),
            Err(e) => return Err(e),
        };
        if bytes.len() as u64 > MAX_FILE {
            return Ok((Loaded::Tampered, st));
        }
        let Ok(file) = serde_json::from_slice::<FileFormat>(&bytes) else {
            return Ok((Loaded::Tampered, st));
        };
        let Ok(tag) = hex::decode(&file.mac) else {
            return Ok((Loaded::Tampered, st));
        };
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.key[..]).expect("any key length");
        mac.update(&serde_json::to_vec(&file.body).expect("body serializes"));
        if mac.verify_slice(&tag).is_err()
            || file.body.version != TRUST_FILE_VERSION
            || !file.body.peers.iter().all(TrustedPeer::valid)
        {
            return Ok((Loaded::Tampered, st));
        }
        Ok((Loaded::Valid(file.body.peers), st))
    }

    /// Re-read the file. Returns whether the trusted set (or the reset flag) changed;
    /// bumps the trust epoch if so.
    pub fn reload(&self) -> io::Result<bool> {
        let (loaded, st) = self.read_file()?;
        let mut state = self.state.lock().unwrap();
        state.stamp = st;
        let (peers, tampered) = match loaded {
            Loaded::Missing => (Vec::new(), false),
            Loaded::Valid(p) => (p, false),
            Loaded::Tampered => (Vec::new(), true),
        };
        let mut changed = state.peers != peers;
        if tampered && !state.pairings_reset {
            state.pairings_reset = true;
            changed = true;
            eprintln!(
                "trust event=pairings_reset file={} reason=integrity_check_failed (treated as empty; pair again)",
                self.path.display()
            );
        }
        state.peers = peers;
        drop(state);
        if changed {
            self.epoch.bump();
        }
        Ok(changed)
    }

    /// Reload if the file changed on disk since the last read/write.
    pub fn reload_if_changed(&self) -> io::Result<bool> {
        let now = stamp(&self.path);
        if now == self.state.lock().unwrap().stamp {
            return Ok(false);
        }
        self.reload()
    }

    /// Poll the file every `interval` and reload on change (another process revoked or
    /// paired). The task ends when the store is dropped.
    pub fn spawn_watcher(self: &Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(store) = weak.upgrade() else { return };
                if let Err(e) = store.reload_if_changed() {
                    eprintln!("trust event=reload_error error={e}");
                }
            }
        })
    }

    /// Atomic write: temp file (0600, create_new) → fsync → rename → fsync dir.
    fn write(&self, peers: &[TrustedPeer]) -> io::Result<()> {
        let body = Body {
            version: TRUST_FILE_VERSION,
            peers: peers.to_vec(),
        };
        let file = FileFormat {
            mac: hex::encode(self.mac(&body)),
            body,
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let mut rnd = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut rnd);
        let tmp = dir.join(format!(".{TRUST_FILE}.tmp-{}-{}", std::process::id(), hex::encode(rnd)));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let result = (|| {
            let mut f = opts.open(&tmp)?;
            f.write_all(&bytes)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            fs::rename(&tmp, &self.path)?;
            if let Ok(d) = fs::File::open(dir) {
                let _ = d.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Read-modify-write under the state lock, starting from the file on disk (so a
    /// concurrent change by another process is not overwritten with a stale view).
    fn modify<T>(&self, f: impl FnOnce(&mut Vec<TrustedPeer>) -> io::Result<T>) -> io::Result<T> {
        let (loaded, _) = self.read_file()?;
        let mut state = self.state.lock().unwrap();
        let mut peers = match loaded {
            Loaded::Valid(p) => p,
            // a tampered file is replaced by what we trust in memory (empty after a reset)
            Loaded::Missing | Loaded::Tampered => state.peers.clone(),
        };
        let out = f(&mut peers)?;
        self.write(&peers)?;
        state.stamp = stamp(&self.path);
        let changed = state.peers != peers;
        state.peers = peers;
        drop(state);
        if changed {
            self.epoch.bump();
        }
        Ok(out)
    }

    /// Add or replace (same PeerId) a paired Mac.
    pub fn add(&self, peer: TrustedPeer) -> io::Result<()> {
        if !peer.valid() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid peer id or static key"));
        }
        self.modify(|peers| {
            peers.retain(|p| p.peer_id != peer.peer_id);
            peers.push(peer);
            Ok(())
        })
    }

    /// Remove by exact PeerId or by name (case-insensitive). Ambiguous names are refused.
    pub fn revoke(&self, selector: &str) -> io::Result<TrustedPeer> {
        let selector = selector.trim();
        self.modify(|peers| {
            let by_id: Vec<usize> = (0..peers.len()).filter(|&i| peers[i].peer_id == selector).collect();
            let hits = if by_id.is_empty() {
                (0..peers.len())
                    .filter(|&i| peers[i].name.eq_ignore_ascii_case(selector))
                    .collect()
            } else {
                by_id
            };
            match hits.as_slice() {
                [] => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no paired Mac named or with PeerId \"{selector}\""),
                )),
                [i] => Ok(peers.remove(*i)),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("several paired Macs are named \"{selector}\"; use the PeerId"),
                )),
            }
        })
    }

    /// Remove `peer` if present (the peer sent `Unpair`).
    pub fn remove_peer(&self, peer: &PeerId) -> io::Result<Option<TrustedPeer>> {
        let id = peer.to_string();
        self.modify(|peers| {
            let pos = peers.iter().position(|p| p.peer_id == id);
            Ok(pos.map(|i| peers.remove(i)))
        })
    }

    pub fn list(&self) -> Vec<TrustedPeer> {
        self.state.lock().unwrap().peers.clone()
    }

    pub fn get(&self, peer: &PeerId) -> Option<TrustedPeer> {
        let id = peer.to_string();
        self.state.lock().unwrap().peers.iter().find(|p| p.peer_id == id).cloned()
    }

    pub fn is_trusted(&self, peer: &PeerId) -> bool {
        self.get(peer).is_some()
    }

    pub fn trusted_key(&self, peer: &PeerId) -> Option<[u8; 32]> {
        self.get(peer).and_then(|p| p.static_key())
    }

    /// The file failed its integrity check at some point in this process: every pairing
    /// was dropped ("pairings were reset").
    pub fn pairings_reset(&self) -> bool {
        self.state.lock().unwrap().pairings_reset
    }
}

/// "Reset this Mac's identity" (plan §2, Advanced): a new node key (new PeerId, new
/// static key), and every pairing removed (the trust file is deleted; it would no longer
/// verify under the new key anyway). Other Macs must pair again. Atomic per file.
pub fn reset_identity(dir: &Path) -> io::Result<NodeKeypair> {
    fs::create_dir_all(dir)?;
    let key = dir.join("node.key");
    let tmp = dir.join(format!(".node.key.new-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let node = NodeKeypair::generate();
    node.save(&tmp).map_err(io::Error::other)?;
    match fs::remove_file(dir.join(TRUST_FILE)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    }
    fs::rename(&tmp, &key)?;
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(node)
}

/// [`PeerTrust`] for the forwarder: the trust store plus an optional pairing window, and
/// the best-effort `Unpair` goodbye on revoke.
pub struct NodeTrust {
    pub store: Arc<TrustStore>,
    window: Mutex<Option<Arc<PairingWindow>>>,
    node: NodeKeypair,
    local: PeerId,
}

impl NodeTrust {
    pub fn new(store: Arc<TrustStore>, node: NodeKeypair) -> Arc<Self> {
        let local = p2p_forward::transport_identity(&node).public().to_peer_id();
        Arc::new(Self {
            store,
            window: Mutex::new(None),
            node,
            local,
        })
    }

    pub fn local_peer(&self) -> PeerId {
        self.local
    }

    pub fn node(&self) -> &NodeKeypair {
        &self.node
    }

    pub fn set_window(&self, window: Option<Arc<PairingWindow>>) {
        *self.window.lock().unwrap() = window;
        self.store.epoch().bump();
    }

    pub fn window(&self) -> Option<Arc<PairingWindow>> {
        self.window.lock().unwrap().clone()
    }
}

impl PeerTrust for NodeTrust {
    fn trusted_key(&self, peer: &PeerId) -> Option<[u8; 32]> {
        self.store.trusted_key(peer)
    }

    fn pairing_open(&self) -> bool {
        self.window().is_some_and(|w| w.is_open())
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.store.epoch().subscribe()
    }

    fn notify_revoked(&self, peer: PeerId, old_key: [u8; 32], control: Control) -> BoxFuture {
        let node = self.node.clone();
        let local = self.local;
        Box::pin(async move {
            match crate::pairing::send_unpair(control, &node, local, peer, old_key).await {
                Ok(()) => eprintln!("trust event=unpair_sent peer={peer}"),
                Err(e) => eprintln!("trust event=unpair_not_sent peer={peer} reason={e}"),
            }
        })
    }
}

/// Hint appended to "nothing paired" errors when the trust file failed its check.
fn reset_hint(store: &TrustStore) -> &'static str {
    if store.pairings_reset() {
        " (pairings were reset: trusted_macs.json failed its integrity check — pair again)"
    } else {
        ""
    }
}

/// Which paired Mac `use` dials: `selector` = name, PeerId, or a multiaddr ending in
/// `/p2p/<PeerId>` of a paired Mac; `None` = the only Mac that shares with this one.
/// Returns the pinned dial address. Never returns an address for an unpaired PeerId.
pub fn resolve_use_target(store: &TrustStore, selector: Option<&str>) -> Result<(PeerId, libp2p::Multiaddr), String> {
    use libp2p::multiaddr::Protocol;
    let pinned = |entry: &TrustedPeer, addr: Option<libp2p::Multiaddr>| -> Result<(PeerId, libp2p::Multiaddr), String> {
        let peer = entry.peer().ok_or("bad PeerId in the trust store")?;
        let addr = match addr {
            Some(a) => a,
            None => entry
                .last_addrs
                .first()
                .ok_or_else(|| format!("no known address for \"{}\"; pass --peer <multiaddr>", entry.name))?
                .parse::<libp2p::Multiaddr>()
                .map_err(|e| format!("bad stored address: {e}"))?,
        };
        match addr.iter().last() {
            Some(Protocol::P2p(id)) if id == peer => Ok((peer, addr)),
            Some(Protocol::P2p(id)) => Err(format!(
                "{addr} pins {id}, but \"{}\" is {peer}: refusing to dial a different Mac",
                entry.name
            )),
            _ => Ok((peer, addr.with(Protocol::P2p(peer)))),
        }
    };
    let all = store.list();
    if let Some(sel) = selector.map(str::trim).filter(|s| s.starts_with('/')) {
        let addr: libp2p::Multiaddr = sel.parse().map_err(|e| format!("bad --peer address: {e}"))?;
        let Some(Protocol::P2p(id)) = addr.iter().last() else {
            return Err("the address must end in /p2p/<PeerId> of a paired Mac (pinned dialing only)".into());
        };
        let entry = all
            .iter()
            .find(|p| p.peer() == Some(id))
            .ok_or_else(|| format!("{id} is not a paired Mac. Pair first: slipstream-node pair --peer <addr> --code <code>"))?;
        return pinned(entry, Some(addr));
    }
    let shares: Vec<&TrustedPeer> = all.iter().filter(|p| p.role == PeerRole::Share).collect();
    match selector.map(str::trim) {
        Some(sel) => {
            let hits: Vec<&TrustedPeer> = all
                .iter()
                .filter(|p| p.peer_id == sel || p.name.eq_ignore_ascii_case(sel))
                .collect();
            match hits.as_slice() {
                [one] => pinned(one, None),
                [] => Err(format!("no paired Mac named or with PeerId \"{sel}\"{}", reset_hint(store))),
                _ => Err(format!("several paired Macs are named \"{sel}\"; use the PeerId")),
            }
        }
        None => match shares.as_slice() {
            [one] => pinned(one, None),
            [] => Err(format!(
                "No paired Mac to use yet{}. On the big Mac run `slipstream-node pair --show-code`, then here: `slipstream-node pair --peer <addr> --code <code>`",
                reset_hint(store)
            )),
            many => Err(format!(
                "{} paired Macs share with this one ({}); choose one with --peer <name>",
                many.len(),
                many.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
            )),
        },
    }
}

/// Error for `share` when nothing is paired.
pub fn share_requires_pairing(store: &TrustStore) -> Result<(), String> {
    if store.list().is_empty() {
        return Err(format!(
            "No paired Macs yet{}. Pair first: on this Mac `slipstream-node pair --show-code`, on the other Mac `slipstream-node pair --peer <addr> --code <code>`",
            reset_hint(store)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample(node: &NodeKeypair, name: &str, role: PeerRole) -> TrustedPeer {
        let peer = p2p_forward::transport_identity(node).public().to_peer_id();
        TrustedPeer {
            peer_id: peer.to_string(),
            identity_hex: String::new(),
            encryption_node_id: node.public_hex(),
            name: name.into(),
            role,
            paired_at: unix_now(),
            last_addrs: vec![],
            last_seen: None,
            app_version: "0.6.0".into(),
            may_start_model: true,
        }
    }

    #[test]
    fn add_list_revoke_roundtrip_0600_and_persistent() {
        let dir = tempfile::tempdir().unwrap();
        let me = NodeKeypair::generate();
        let store = TrustStore::load(dir.path(), &me).unwrap();
        assert!(store.list().is_empty() && !store.pairings_reset());
        let a = NodeKeypair::generate();
        let b = NodeKeypair::generate();
        store.add(sample(&a, "Mac Studio", PeerRole::Share)).unwrap();
        store.add(sample(&b, "MacBook", PeerRole::Use)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let pa = p2p_forward::transport_identity(&a).public().to_peer_id();
        assert!(store.is_trusted(&pa));
        assert_eq!(store.trusted_key(&pa), Some(a.public_bytes()));
        let again = TrustStore::load(dir.path(), &me).unwrap();
        assert_eq!(again.list().len(), 2);
        assert_eq!(again.revoke("mac studio").unwrap().name, "Mac Studio");
        assert!(again.revoke("Mac Studio").is_err());
        // the first instance sees the other process's revoke on reload
        assert!(store.reload_if_changed().unwrap());
        assert!(!store.is_trusted(&pa));
        // no temp files left behind
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from(TRUST_FILE)]);
    }

    #[test]
    fn tampered_or_foreign_key_file_is_empty_with_flag() {
        let dir = tempfile::tempdir().unwrap();
        let me = NodeKeypair::generate();
        let store = TrustStore::load(dir.path(), &me).unwrap();
        store.add(sample(&NodeKeypair::generate(), "Mac Studio", PeerRole::Share)).unwrap();
        // another node key cannot verify it
        let other = TrustStore::load(dir.path(), &NodeKeypair::generate()).unwrap();
        assert!(other.list().is_empty() && other.pairings_reset());
        // one changed byte
        let text = fs::read_to_string(store.path()).unwrap().replace("Mac Studio", "Mac Studix");
        fs::write(store.path(), text).unwrap();
        let t = TrustStore::load(dir.path(), &me).unwrap();
        assert!(t.list().is_empty() && t.pairings_reset());
        // garbage
        fs::write(store.path(), b"{not json").unwrap();
        assert!(TrustStore::load(dir.path(), &me).unwrap().pairings_reset());
        // a fresh add replaces the bad file with a valid one
        t.add(sample(&NodeKeypair::generate(), "New", PeerRole::Use)).unwrap();
        let ok = TrustStore::load(dir.path(), &me).unwrap();
        assert_eq!(ok.list().len(), 1);
        assert!(!ok.pairings_reset());
    }

    #[test]
    fn reset_identity_new_key_and_no_pairings() {
        let dir = tempfile::tempdir().unwrap();
        let me = p2p_forward::load_or_create_node(dir.path()).unwrap();
        let store = TrustStore::load(dir.path(), &me).unwrap();
        store.add(sample(&NodeKeypair::generate(), "Mac Studio", PeerRole::Share)).unwrap();
        let fresh = reset_identity(dir.path()).unwrap();
        assert_ne!(fresh.public_bytes(), me.public_bytes());
        let loaded = p2p_forward::load_or_create_node(dir.path()).unwrap();
        assert_eq!(loaded.public_bytes(), fresh.public_bytes());
        let s = TrustStore::load(dir.path(), &loaded).unwrap();
        assert!(s.list().is_empty());
        assert!(!s.pairings_reset(), "an intentional reset is not a tamper");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join("node.key")).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
