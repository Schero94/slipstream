//! Trust store of paired Macs (v0.6 M2, docs/plans/2026-11-28-use-your-other-mac.md §2).
//!
//! `<data_dir>/trusted_macs.json`, written atomically (new temp file opened with mode
//! 0600 + `create_new`, fsync, rename over the old file) and protected by an HMAC-SHA256
//! over the serialized body, keyed by `HKDF-SHA256(node secret, "trust-store")`. A file
//! whose MAC does not verify (edited by hand, by another user or by malware, or written
//! under another node key) is **treated as empty** and [`TrustStore::pairings_reset`] is
//! set, so the UI can say "pairings were reset" — never fall back to trusting it.
//!
//! The store is shared between processes (the running `share`/`use`, the app and the
//! `trust revoke` command). Every read-modify-write holds an exclusive `flock` on
//! `trusted_macs.json.lock` and reads the file **inside** the lock (security review M4: no
//! lost updates, a revoke is never undone by a concurrent `add` / `record_seen`); readers
//! hold a shared lock. The MAC'd body carries a monotonic `generation`; a file with a lower
//! generation than this process has already seen (a rolled-back copy) is treated like a
//! tampered one. (Rollback across restarts is not detected: whoever can write the data dir
//! can also read `node.key`.) A watcher ([`TrustStore::spawn_watcher`]) polls the file every
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
use p2p_forward::trust::{BoxFuture, PeerTrust, Role, TrustEpoch};
use p2p_libp2p::paired::Control;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::watch;
use zeroize::Zeroizing;

use crate::pairing::PairingWindow;

pub const TRUST_FILE: &str = "trusted_macs.json";
/// Inter-process lock file (exclusive for writers, shared for readers; review M4).
pub const TRUST_LOCK_FILE: &str = "trusted_macs.json.lock";
pub const TRUST_FILE_VERSION: u32 = 1;
/// How often running processes check the file for changes made by other processes.
pub const WATCH_INTERVAL: Duration = Duration::from_millis(250);
const MAC_SALT: &[u8] = b"slipstream-trust-store-v1";
const MAC_INFO: &[u8] = b"trust-store";
const MAX_FILE: u64 = 1024 * 1024;
/// `last_addrs` kept per peer (newest first).
pub const MAX_LAST_ADDRS: usize = 4;

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
    /// M5: the other Mac's Ethernet / Wi-Fi hardware addresses (sent at pairing, for
    /// Wake-on-LAN). Omitted when empty, so pre-M5 files keep their exact bytes (and MAC).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wake_macs: Vec<String>,
}

/// A Mac this one removed (M5 tombstone): its static key lets the share side answer a
/// later connection attempt with an authenticated `Unpair` ("removed while offline").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormerPeer {
    pub peer_id: String,
    pub encryption_node_id: String,
    /// Unix seconds.
    pub removed_at: u64,
}

/// Tombstones are kept this long and at most [`MAX_FORMER`] of them.
pub const FORMER_TTL_SECS: u64 = 30 * 24 * 3600;
pub const MAX_FORMER: usize = 32;

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
    /// Monotonic write counter (review M4 rollback check). Omitted when 0, so files written
    /// before it keep their MAC.
    #[serde(default, skip_serializing_if = "is_zero")]
    generation: u64,
    peers: Vec<TrustedPeer>,
    /// M5 tombstones; omitted when empty (pre-M5 files keep their MAC).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    removed: Vec<FormerPeer>,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Data {
    peers: Vec<TrustedPeer>,
    removed: Vec<FormerPeer>,
    generation: u64,
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
    removed: Vec<FormerPeer>,
    pairings_reset: bool,
    stamp: Option<Stamp>,
    /// Highest file generation this process has read or written (rollback check).
    generation: u64,
}

/// Outcome of reading the file.
enum Loaded {
    Missing,
    Valid(Data),
    Tampered,
    /// MAC-valid, but older than a generation this process already saw.
    RolledBack,
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
                removed: Vec::new(),
                pairings_reset: false,
                stamp: None,
                generation: 0,
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

    /// Hold `trusted_macs.json.lock` (exclusive for read-modify-write, shared for reads).
    /// `flock` locks per open file description, so this also orders threads of one process.
    fn lock(&self, exclusive: bool) -> io::Result<fs::File> {
        let path = self.path.with_file_name(TRUST_LOCK_FILE);
        let mut opts = fs::OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let f = opts.open(&path)?;
        if exclusive {
            f.lock()?;
        } else {
            f.lock_shared()?;
        }
        Ok(f)
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
        if file.body.generation < self.state.lock().unwrap().generation {
            return Ok((Loaded::RolledBack, st));
        }
        Ok((
            Loaded::Valid(Data {
                peers: file.body.peers,
                removed: file.body.removed,
                generation: file.body.generation,
            }),
            st,
        ))
    }

    /// Re-read the file. Returns whether the trusted set (or the reset flag) changed;
    /// bumps the trust epoch if so.
    pub fn reload(&self) -> io::Result<bool> {
        // Shared lock: never read between a writer's read and its state update (M4).
        let _lock = self.lock(false)?;
        let (loaded, st) = self.read_file()?;
        let mut state = self.state.lock().unwrap();
        state.stamp = st;
        let (data, tampered) = match loaded {
            Loaded::Missing => (Data::default(), None),
            Loaded::Valid(d) => (d, None),
            Loaded::Tampered => (Data::default(), Some("integrity_check_failed")),
            Loaded::RolledBack => (Data::default(), Some("rolled_back")),
        };
        let Data { peers, removed, generation } = data;
        state.generation = state.generation.max(generation);
        state.removed = removed;
        let mut changed = state.peers != peers;
        if let Some(reason) = tampered.filter(|_| !state.pairings_reset) {
            state.pairings_reset = true;
            changed = true;
            eprintln!(
                "trust event=pairings_reset file={} reason={reason} (treated as empty; pair again)",
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
    fn write(&self, data: &Data) -> io::Result<()> {
        let body = Body {
            version: TRUST_FILE_VERSION,
            generation: data.generation,
            peers: data.peers.clone(),
            removed: data.removed.clone(),
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
        self.modify_all(|d| f(&mut d.peers))
    }

    fn modify_all<T>(&self, f: impl FnOnce(&mut Data) -> io::Result<T>) -> io::Result<T> {
        // Exclusive inter-process lock around read → modify → write (M4); the file is read
        // inside it, so a concurrent writer's change is never overwritten with a stale view.
        let _lock = self.lock(true)?;
        let (loaded, _) = self.read_file()?;
        let mut state = self.state.lock().unwrap();
        let mut data = match loaded {
            Loaded::Valid(d) => d,
            // a tampered / rolled-back file is replaced by what we trust in memory (empty
            // after a reset)
            Loaded::Missing | Loaded::Tampered | Loaded::RolledBack => Data {
                peers: state.peers.clone(),
                removed: state.removed.clone(),
                generation: 0,
            },
        };
        data.generation = data.generation.max(state.generation) + 1;
        let out = f(&mut data)?;
        // tombstones: re-paired peers drop theirs; old / excess ones expire
        let now = unix_now();
        let live: Vec<String> = data.peers.iter().map(|p| p.peer_id.clone()).collect();
        data.removed
            .retain(|r| !live.contains(&r.peer_id) && now.saturating_sub(r.removed_at) < FORMER_TTL_SECS);
        if data.removed.len() > MAX_FORMER {
            let cut = data.removed.len() - MAX_FORMER;
            data.removed.drain(..cut);
        }
        self.write(&data)?;
        state.stamp = stamp(&self.path);
        state.generation = data.generation;
        state.removed = data.removed;
        let peers = data.peers;
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
        self.modify_all(|data| {
            let removed = Self::revoke_in(&mut data.peers, selector)?;
            // M5: remember the key, so a later connection attempt of the removed Mac (it
            // may be offline now) is answered with an authenticated Unpair.
            data.removed.retain(|r| r.peer_id != removed.peer_id);
            data.removed.push(FormerPeer {
                peer_id: removed.peer_id.clone(),
                encryption_node_id: removed.encryption_node_id.clone(),
                removed_at: unix_now(),
            });
            Ok(removed)
        })
    }

    fn revoke_in(peers: &mut Vec<TrustedPeer>, selector: &str) -> io::Result<TrustedPeer> {
        {
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
        }
    }

    /// The static key of a Mac this one removed (tombstone), if still kept.
    pub fn former_key(&self, peer: &PeerId) -> Option<[u8; 32]> {
        let id = peer.to_string();
        let state = self.state.lock().unwrap();
        let r = state.removed.iter().find(|r| r.peer_id == id)?;
        hex::decode(&r.encryption_node_id).ok()?.try_into().ok()
    }

    /// Remove `peer` if present (the peer sent `Unpair`).
    pub fn remove_peer(&self, peer: &PeerId) -> io::Result<Option<TrustedPeer>> {
        let id = peer.to_string();
        self.modify(|peers| {
            let pos = peers.iter().position(|p| p.peer_id == id);
            Ok(pos.map(|i| peers.remove(i)))
        })
    }

    /// A pinned dial to `peer` succeeded at `addr` (M3): put `<addr>/p2p/<peer>` first in
    /// `last_addrs` (at most [`MAX_LAST_ADDRS`]) and refresh `last_seen`. Skips the write
    /// when nothing changed and `last_seen` is younger than a minute. Returns whether it
    /// wrote.
    pub fn record_seen(&self, peer: &PeerId, addr: &libp2p::Multiaddr) -> io::Result<bool> {
        use libp2p::multiaddr::Protocol;
        let bare: libp2p::Multiaddr = addr.iter().filter(|p| !matches!(p, Protocol::P2p(_))).collect();
        if bare.is_empty() {
            return Ok(false);
        }
        let pinned = bare.with(Protocol::P2p(*peer)).to_string();
        let now = unix_now();
        let Some(cur) = self.get(peer) else { return Ok(false) };
        if cur.last_addrs.first() == Some(&pinned) && cur.last_seen.is_some_and(|t| now.saturating_sub(t) < 60) {
            return Ok(false);
        }
        let id = peer.to_string();
        self.modify(|peers| {
            let Some(p) = peers.iter_mut().find(|p| p.peer_id == id) else {
                return Ok(false);
            };
            p.last_addrs.retain(|a| *a != pinned);
            p.last_addrs.insert(0, pinned.clone());
            p.last_addrs.truncate(MAX_LAST_ADDRS);
            p.last_seen = Some(now);
            Ok(true)
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

impl PeerRole {
    /// The forwarder's view of this role.
    pub fn as_role(self) -> Role {
        match self {
            PeerRole::Share => Role::Share,
            PeerRole::Use => Role::Use,
        }
    }
}

impl PeerTrust for NodeTrust {
    fn trusted_key(&self, peer: &PeerId) -> Option<[u8; 32]> {
        self.store.trusted_key(peer)
    }

    fn trusted_key_for(&self, peer: &PeerId, role: Role) -> Option<[u8; 32]> {
        self.store.get(peer).filter(|p| p.role.as_role() == role).and_then(|p| p.static_key())
    }

    fn pairing_open(&self) -> bool {
        self.window().is_some_and(|w| w.is_open())
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.store.epoch().subscribe()
    }

    fn former_key(&self, peer: &PeerId) -> Option<[u8; 32]> {
        self.store.former_key(peer)
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
    let (peer, addr) = resolve_use_peer(store, selector)?;
    match addr {
        Some(a) => Ok((peer, a)),
        None => {
            let name = store.get(&peer).map(|p| p.name).unwrap_or_default();
            Err(format!("no known address for \"{name}\"; pass --peer <multiaddr>"))
        }
    }
}

/// Like [`resolve_use_target`], but a paired Mac without a stored address is fine
/// (`None`): LAN discovery (M3) finds it by its fingerprint.
pub fn resolve_use_peer(store: &TrustStore, selector: Option<&str>) -> Result<(PeerId, Option<libp2p::Multiaddr>), String> {
    use libp2p::multiaddr::Protocol;
    let pinned = |entry: &TrustedPeer, addr: Option<libp2p::Multiaddr>| -> Result<(PeerId, Option<libp2p::Multiaddr>), String> {
        let peer = entry.peer().ok_or("bad PeerId in the trust store")?;
        let addr = match addr {
            Some(a) => a,
            None => match entry.last_addrs.first() {
                None => return Ok((peer, None)),
                Some(a) => a.parse::<libp2p::Multiaddr>().map_err(|e| format!("bad stored address: {e}"))?,
            },
        };
        match addr.iter().last() {
            Some(Protocol::P2p(id)) if id == peer => Ok((peer, Some(addr))),
            Some(Protocol::P2p(id)) => Err(format!(
                "{addr} pins {id}, but \"{}\" is {peer}: refusing to dial a different Mac",
                entry.name
            )),
            _ => Ok((peer, Some(addr.with(Protocol::P2p(peer))))),
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
        not_a_user(entry)?;
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
                [one] => {
                    not_a_user(one)?;
                    pinned(one, None)
                }
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

/// Security review M1: pairing is directional. A Mac paired as a *user* of this one cannot be
/// used from here (it never offered its model to this Mac); pair the other way round.
fn not_a_user(entry: &TrustedPeer) -> Result<(), String> {
    if entry.role == PeerRole::Use {
        return Err(format!(
            "\"{}\" was paired to use this Mac, not to share with it. To use its model, show a code on \"{}\" and pair again from here.",
            entry.name, entry.name
        ));
    }
    Ok(())
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
            wake_macs: vec![],
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
        let mut names: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, vec![std::ffi::OsString::from(TRUST_FILE), std::ffi::OsString::from(TRUST_LOCK_FILE)]);
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
    fn record_seen_puts_the_working_address_first() {
        let dir = tempfile::tempdir().unwrap();
        let me = NodeKeypair::generate();
        let store = TrustStore::load(dir.path(), &me).unwrap();
        let a = NodeKeypair::generate();
        store.add(sample(&a, "Mac Studio", PeerRole::Share)).unwrap();
        let pa = p2p_forward::transport_identity(&a).public().to_peer_id();
        // no address yet: resolvable without one (discovery finds it)
        assert_eq!(resolve_use_peer(&store, None).unwrap(), (pa, None));
        assert!(resolve_use_target(&store, None).is_err());
        let addr: libp2p::Multiaddr = "/ip4/10.0.0.7/udp/9003/quic-v1".parse().unwrap();
        assert!(store.record_seen(&pa, &addr).unwrap());
        assert!(!store.record_seen(&pa, &addr).unwrap(), "unchanged + fresh: no write");
        let e = store.get(&pa).unwrap();
        assert_eq!(e.last_addrs, vec![format!("/ip4/10.0.0.7/udp/9003/quic-v1/p2p/{pa}")]);
        assert!(e.last_seen.is_some());
        for port in 1..=5u16 {
            let a2: libp2p::Multiaddr = format!("/ip4/10.0.0.8/udp/{port}/quic-v1").parse().unwrap();
            store.record_seen(&pa, &a2).unwrap();
        }
        let e = store.get(&pa).unwrap();
        assert_eq!(e.last_addrs.len(), MAX_LAST_ADDRS);
        assert!(e.last_addrs[0].contains("/udp/5/"));
        assert_eq!(resolve_use_target(&store, None).unwrap().1.to_string(), e.last_addrs[0]);
        // persisted and still MAC-valid
        assert_eq!(TrustStore::load(dir.path(), &me).unwrap().get(&pa).unwrap().last_addrs, e.last_addrs);
    }

    /// Review M4: a MAC-valid but older copy of the file (rollback) is refused like a
    /// tampered one; generations only grow.
    #[test]
    fn rolled_back_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let me = NodeKeypair::generate();
        let store = TrustStore::load(dir.path(), &me).unwrap();
        let a = NodeKeypair::generate();
        store.add(sample(&a, "Mac Studio", PeerRole::Use)).unwrap();
        let old = fs::read(store.path()).unwrap(); // generation 1, Mac Studio trusted
        store.revoke("Mac Studio").unwrap(); // generation 2
        let text = fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("\"generation\": 2"), "{text}");
        // an attacker (or a stale backup) puts the old, validly MAC'd file back
        let tmp = dir.path().join("old.json");
        fs::write(&tmp, &old).unwrap();
        fs::rename(&tmp, store.path()).unwrap();
        store.reload().unwrap();
        let pa = p2p_forward::transport_identity(&a).public().to_peer_id();
        assert!(!store.is_trusted(&pa), "rollback resurrected a revoked Mac");
        assert!(store.pairings_reset());
        // a fresh process cannot tell (documented): it loads the old file
        assert!(TrustStore::load(dir.path(), &me).unwrap().is_trusted(&pa));
    }

    /// Child mode of [`concurrent_processes_never_resurrect_a_revoked_peer`]: runs only when
    /// the parent sets `SLIPSTREAM_TRUST_CHILD` (otherwise a no-op).
    #[test]
    fn trust_child_writer() {
        let Ok(spec) = std::env::var("SLIPSTREAM_TRUST_CHILD") else { return };
        let mut it = spec.split('|');
        let (dir, secret, target, tag) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        let me = NodeKeypair::from_secret_bytes(hex::decode(secret).unwrap().try_into().unwrap());
        let store = TrustStore::load(Path::new(dir), &me).unwrap();
        let target: PeerId = target.parse().unwrap();
        for i in 0..40u16 {
            let n = NodeKeypair::generate();
            store.add(sample(&n, &format!("{tag}-{i}"), PeerRole::Use)).unwrap();
            let addr: libp2p::Multiaddr = format!("/ip4/10.0.0.{}/udp/{}/quic-v1", i % 200 + 1, 9000 + i).parse().unwrap();
            let _ = store.record_seen(&target, &addr).unwrap();
        }
    }

    /// Review M4: four other processes add Macs and record addresses of the target while this
    /// one revokes it — the revoked Mac never comes back and no add is lost.
    #[test]
    fn concurrent_processes_never_resurrect_a_revoked_peer() {
        let dir = tempfile::tempdir().unwrap();
        let me = NodeKeypair::generate();
        let store = TrustStore::load(dir.path(), &me).unwrap();
        let t = NodeKeypair::generate();
        let target = p2p_forward::transport_identity(&t).public().to_peer_id();
        store.add(sample(&t, "Target", PeerRole::Use)).unwrap();
        let exe = std::env::current_exe().unwrap();
        let spawn = |tag: &str| {
            std::process::Command::new(&exe)
                .args(["--exact", "trust::tests::trust_child_writer", "--test-threads=1", "--quiet"])
                .env(
                    "SLIPSTREAM_TRUST_CHILD",
                    format!("{}|{}|{}|{tag}", dir.path().display(), hex::encode(me.secret_bytes()), target),
                )
                .spawn()
                .unwrap()
        };
        let mut kids = vec![spawn("a"), spawn("b"), spawn("c"), spawn("d")];
        // revoke while the children write (wait until they started)
        let t0 = std::time::Instant::now();
        while store.list().len() < 3 && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(5));
            store.reload().unwrap();
        }
        store.revoke("Target").unwrap();
        for k in &mut kids {
            assert!(k.wait().unwrap().success());
        }
        let after = TrustStore::load(dir.path(), &me).unwrap();
        assert!(!after.is_trusted(&target), "a concurrent writer resurrected the revoked Mac");
        assert!(after.former_key(&target).is_some(), "tombstone lost");
        assert_eq!(after.list().len(), 4 * 40, "lost updates: {}", after.list().len());
        assert!(!after.pairings_reset());
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
