//! Who is listening on the upstream port? (v0.6 security review M3)
//!
//! The share side forwards paired Macs' requests to `127.0.0.1:<port>`. If the model is not
//! running, *any* local process (another user's, a leftover dev server, malware) could
//! listen there and receive the paired Mac's prompts. Before a request (and a status
//! collection) touches the upstream, [`UpstreamGuard::verify`] checks the listening
//! socket's owner with `lsof -Fpu` (read-only, no privileges):
//!
//! - [`UpstreamOwner::Pid`] (the app): the listener must be the app's own llama-server /
//!   oMLX child (the PID from the app's `Child` handle) or one of its descendants (oMLX's
//!   launcher starts the real server as a child);
//! - [`UpstreamOwner::SameUid`] (headless `slipstream-node share` without `--forward-pid`):
//!   at least the listener must belong to this user.
//!
//! Every listener on the address must pass (a second `SO_REUSEPORT` listener fails it).
//! Results are cached for [`OWNER_CACHE_OK`] (positive) / [`OWNER_CACHE_FAIL`] (negative);
//! `lsof` missing or failing counts as "not verified" (fail closed). A failed check answers
//! 503 `model_not_running`.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A verified owner is trusted this long before `lsof` runs again.
pub const OWNER_CACHE_OK: Duration = Duration::from_secs(5);
/// A failed check is retried after this (the model may just have started).
pub const OWNER_CACHE_FAIL: Duration = Duration::from_secs(1);

pub type PidSource = Arc<dyn Fn() -> Option<u32> + Send + Sync>;

#[derive(Clone)]
pub enum UpstreamOwner {
    /// The listener must be this process (`None` = no model process → not verified) or a
    /// descendant of it.
    Pid(PidSource),
    /// The listener must run under this process's user id.
    SameUid,
}

impl UpstreamOwner {
    /// A fixed PID (`slipstream-node share --forward-pid`).
    pub fn fixed_pid(pid: u32) -> Self {
        Self::Pid(Arc::new(move || Some(pid)))
    }
}

impl Default for UpstreamOwner {
    fn default() -> Self {
        Self::SameUid
    }
}

impl std::fmt::Debug for UpstreamOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pid(p) => write!(f, "Pid({:?})", p()),
            Self::SameUid => f.write_str("SameUid"),
        }
    }
}

/// One listening socket: (pid, uid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listener {
    pub pid: u32,
    pub uid: u32,
}

/// Parse `lsof -F pu` output (`p<pid>`, `u<uid>`, `f<fd>` lines).
pub fn parse_lsof(text: &str) -> Vec<Listener> {
    let mut out: Vec<Listener> = Vec::new();
    let mut pid = None;
    for line in text.lines() {
        let (tag, val) = line.split_at(line.len().min(1));
        match tag {
            "p" => pid = val.parse::<u32>().ok(),
            "u" => {
                if let (Some(p), Ok(u)) = (pid, val.parse::<u32>()) {
                    let l = Listener { pid: p, uid: u };
                    if !out.contains(&l) {
                        out.push(l);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Processes listening on `addr` (TCP). `None` if `lsof` could not be run.
pub fn listeners(addr: SocketAddr) -> Option<Vec<Listener>> {
    let spec = match addr {
        SocketAddr::V4(a) => format!("-iTCP@{}:{}", a.ip(), a.port()),
        SocketAddr::V6(a) => format!("-iTCP@[{}]:{}", a.ip(), a.port()),
    };
    let out = Command::new("/usr/sbin/lsof")
        .args(["-nP", &spec, "-sTCP:LISTEN", "-Fpu"])
        .output()
        .ok()?;
    // lsof exits 1 when nothing matches: that is a valid, empty answer
    Some(parse_lsof(&String::from_utf8_lossy(&out.stdout)))
}

fn parent_of(pid: u32) -> Option<u32> {
    let out = Command::new("/bin/ps").args(["-o", "ppid=", "-p", &pid.to_string()]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// `pid` is `ancestor` or a descendant of it (≤ 8 levels).
pub fn is_same_or_descendant(pid: u32, ancestor: u32, parent: impl Fn(u32) -> Option<u32>) -> bool {
    let mut cur = pid;
    for _ in 0..8 {
        if cur == ancestor {
            return true;
        }
        match parent(cur) {
            Some(p) if p > 1 && p != cur => cur = p,
            _ => return false,
        }
    }
    false
}

/// Pure decision (tests): do these listeners belong to `owner`?
pub fn owned_by(found: &[Listener], owner: &UpstreamOwner, my_uid: u32, parent: impl Fn(u32) -> Option<u32>) -> bool {
    if found.is_empty() {
        return false;
    }
    match owner {
        UpstreamOwner::SameUid => found.iter().all(|l| l.uid == my_uid),
        UpstreamOwner::Pid(src) => match src() {
            Some(want) => found.iter().all(|l| is_same_or_descendant(l.pid, want, &parent)),
            None => false,
        },
    }
}

/// This process's user id (`id -u`, once; the crate forbids `unsafe`). `u32::MAX` if
/// unknown, which matches no listener (fail closed).
fn my_uid() -> u32 {
    static UID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *UID.get_or_init(|| {
        Command::new("/usr/bin/id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(u32::MAX)
    })
}

/// [`UpstreamOwner`] check with a small cache.
pub struct UpstreamGuard {
    owner: UpstreamOwner,
    cache: Mutex<Option<(Instant, Option<u32>, bool)>>,
}

impl UpstreamGuard {
    pub fn new(owner: UpstreamOwner) -> Self {
        Self {
            owner,
            cache: Mutex::new(None),
        }
    }

    pub fn owner(&self) -> &UpstreamOwner {
        &self.owner
    }

    /// Whether the process listening on `upstream` is the expected one.
    pub async fn verify(&self, upstream: SocketAddr) -> bool {
        let want = match &self.owner {
            UpstreamOwner::Pid(src) => src(),
            UpstreamOwner::SameUid => None,
        };
        if let Some((t, w, ok)) = *self.cache.lock().unwrap() {
            let ttl = if ok { OWNER_CACHE_OK } else { OWNER_CACHE_FAIL };
            if w == want && t.elapsed() < ttl {
                return ok;
            }
        }
        let owner = self.owner.clone();
        let ok = tokio::task::spawn_blocking(move || match listeners(upstream) {
            Some(found) => owned_by(&found, &owner, my_uid(), parent_of),
            None => false,
        })
        .await
        .unwrap_or(false);
        *self.cache.lock().unwrap() = Some((Instant::now(), want, ok));
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsof_parsing_and_ownership_rules() {
        let text = "p4242\nu501\nf7\np777\nu502\nf3\n";
        let found = parse_lsof(text);
        assert_eq!(found, vec![Listener { pid: 4242, uid: 501 }, Listener { pid: 777, uid: 502 }]);
        let parents = |p: u32| match p {
            4242 => Some(4000), // launcher child of the app's child 4000
            4000 => Some(1),
            777 => Some(1),
            _ => None,
        };
        let one = [Listener { pid: 4242, uid: 501 }];
        assert!(owned_by(&one, &UpstreamOwner::fixed_pid(4242), 501, parents));
        assert!(owned_by(&one, &UpstreamOwner::fixed_pid(4000), 501, parents), "descendant of the child");
        assert!(!owned_by(&one, &UpstreamOwner::fixed_pid(9999), 501, parents));
        assert!(!owned_by(&one, &UpstreamOwner::Pid(Arc::new(|| None)), 501, parents), "no model process");
        assert!(owned_by(&one, &UpstreamOwner::SameUid, 501, parents));
        assert!(!owned_by(&one, &UpstreamOwner::SameUid, 502, parents), "another user's listener");
        // a second listener (SO_REUSEPORT squatter) fails the check
        assert!(!owned_by(&found, &UpstreamOwner::fixed_pid(4000), 501, parents));
        assert!(!owned_by(&found, &UpstreamOwner::SameUid, 501, parents));
        assert!(!owned_by(&[], &UpstreamOwner::SameUid, 501, parents), "nobody listening");
    }

    /// Real lsof against a listener of this process: our PID and uid pass, another PID fails.
    #[tokio::test]
    async fn verify_against_a_real_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        assert!(UpstreamGuard::new(UpstreamOwner::SameUid).verify(addr).await);
        assert!(UpstreamGuard::new(UpstreamOwner::fixed_pid(std::process::id())).verify(addr).await);
        assert!(!UpstreamGuard::new(UpstreamOwner::fixed_pid(1)).verify(addr).await);
        drop(l);
        // nobody listening any more → not verified
        assert!(!UpstreamGuard::new(UpstreamOwner::SameUid).verify(addr).await);
    }
}
