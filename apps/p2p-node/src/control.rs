//! Local control socket of a running `slipstream-node share` (v0.6 M3).
//!
//! Lets `slipstream-node pair --show-code` open a pairing window **inside** the running
//! share (M2 risk 1: both cannot listen on the same UDP port). The socket is
//! `<data_dir>/share.sock` — the data dir holds `node.key`, so whoever can reach the socket
//! can already read the identity; the socket is additionally chmod 0600 and every
//! connection's peer uid must equal the socket owner's uid.
//!
//! Protocol: one JSON request line `{"cmd":"open_pairing"}`; the share answers
//! `{"t":"opened","code":..,"dial":..,"fp":..,"remaining_s":..}` and, when the window
//! closes, `{"t":"closed","state":"paired|expired|too_many_failures|cancelled",
//! "name":..,"peer_id":..}`. If the client goes away first (Ctrl-C), the window is
//! cancelled. Errors: `{"t":"error","message":..}`. One window at a time.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use libp2p::Multiaddr;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;

use crate::pairing::{PairingWindow, SystemClock, WindowState};
use crate::trust::NodeTrust;

pub const CONTROL_SOCKET: &str = "share.sock";
/// `sun_path` is 104 bytes on macOS.
const MAX_SOCKET_PATH: usize = 100;
const MAX_LINE: u64 = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlRequest {
    OpenPairing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ControlReply {
    Opened {
        code: String,
        dial: String,
        fp: String,
        remaining_s: u64,
    },
    Closed {
        state: String,
        name: Option<String>,
        peer_id: Option<String>,
    },
    Error {
        message: String,
    },
}

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join(CONTROL_SOCKET)
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

/// The running server; dropping it stops accepting and removes the socket file.
pub struct ControlServer {
    pub path: PathBuf,
    task: JoinHandle<()>,
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn check_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("control socket path too long ({} bytes): {}", path.as_os_str().len(), path.display()),
        ));
    }
    Ok(())
}

/// Bind `<dir>/share.sock`. A stale socket (nobody answering) is replaced; a live one
/// (another share with this data dir) is an error.
pub async fn start_control_server(dir: &Path, trust: Arc<NodeTrust>, dial: Multiaddr, fp: String) -> io::Result<ControlServer> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let path = socket_path(dir);
    check_path(&path)?;
    if std::fs::symlink_metadata(&path).is_ok() {
        if UnixStream::connect(&path).await.is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another share is running with this data dir ({})", path.display()),
            ));
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let owner = std::fs::metadata(&path)?.uid();
    let dial = dial.to_string();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            match stream.peer_cred() {
                Ok(c) if c.uid() == owner => {}
                _ => {
                    eprintln!("control event=refused reason=foreign_uid");
                    continue;
                }
            }
            let trust = trust.clone();
            let (dial, fp) = (dial.clone(), fp.clone());
            tokio::spawn(async move {
                if let Err(e) = serve(stream, trust, dial, fp).await {
                    eprintln!("control event=error error={e}");
                }
            });
        }
    });
    Ok(ControlServer { path, task })
}

async fn send(w: &mut tokio::net::unix::OwnedWriteHalf, r: &ControlReply) -> io::Result<()> {
    let mut line = serde_json::to_vec(r).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

async fn serve(stream: UnixStream, trust: Arc<NodeTrust>, dial: String, fp: String) -> io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r.take(MAX_LINE));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no request"))??;
    let req: ControlRequest = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            return send(&mut w, &ControlReply::Error { message: format!("bad request: {e}") }).await;
        }
    };
    match req {
        ControlRequest::OpenPairing => {
            if trust.window().is_some_and(|w| w.is_open()) {
                return send(
                    &mut w,
                    &ControlReply::Error {
                        message: "a pairing window is already open on this Mac".into(),
                    },
                )
                .await;
            }
            let window = PairingWindow::open(Arc::new(SystemClock), trust.store.epoch());
            trust.set_window(Some(window.clone()));
            eprintln!("control event=pairing_window_opened");
            send(
                &mut w,
                &ControlReply::Opened {
                    code: window.code().as_str().to_string(),
                    dial,
                    fp,
                    remaining_s: window.remaining().as_secs(),
                },
            )
            .await?;
            // Wait for the window to close, or for the client to go away (→ cancel).
            let mut rest = reader.into_inner().into_inner();
            let mut buf = [0u8; 64];
            let state = tokio::select! {
                s = window.wait_closed() => s,
                _ = async { loop { match rest.read(&mut buf).await { Ok(0) | Err(_) => break, Ok(_) => {} } } } => {
                    window.cancel();
                    window.state()
                }
            };
            trust.set_window(None);
            eprintln!("control event=pairing_window_closed state={}", state_name(state));
            let p = window.paired_peer();
            let _ = send(
                &mut w,
                &ControlReply::Closed {
                    state: state_name(state).into(),
                    name: p.as_ref().map(|p| p.name.clone()),
                    peer_id: p.map(|p| p.peer_id),
                },
            )
            .await;
            Ok(())
        }
    }
}

/// Client: a connection to a running share's control socket.
pub struct ControlClient {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    _w: tokio::net::unix::OwnedWriteHalf,
}

impl ControlClient {
    /// Next reply (`None` = the share closed the connection).
    pub async fn next(&mut self) -> io::Result<Option<ControlReply>> {
        match self.lines.next_line().await? {
            None => Ok(None),
            Some(l) => serde_json::from_str(&l)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        }
    }
}

/// Ask a running share (same data dir) to open a pairing window. `Ok(None)` = no share
/// is running (no socket, or nobody answers).
pub async fn open_pairing_via_share(dir: &Path) -> io::Result<Option<ControlClient>> {
    let path = socket_path(dir);
    if check_path(&path).is_err() || std::fs::symlink_metadata(&path).is_err() {
        return Ok(None);
    }
    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let (r, mut w) = stream.into_split();
    let mut req = serde_json::to_vec(&ControlRequest::OpenPairing).map_err(io::Error::other)?;
    req.push(b'\n');
    w.write_all(&req).await?;
    Ok(Some(ControlClient {
        lines: BufReader::new(r).lines(),
        _w: w,
    }))
}
