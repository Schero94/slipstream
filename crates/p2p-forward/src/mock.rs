//! Deterministic SSE mock of llama-server's streaming endpoint (M0 measurements).
//!
//! `POST /mock?n=<events>&gap_us=<pause between events>&first_ms=<delay before first>`
//! → `HTTP/1.1 200`, `text/event-stream`, `n` OpenAI-style chunk events + `data: [DONE]`,
//! one `write` + flush per event (like llama-server), then close. The body is a pure
//! function of `n`, so its SHA-256 is identical across runs and arms.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The deterministic event for index `i`.
pub fn event(i: usize) -> String {
    format!(
        "data: {{\"id\":\"chatcmpl-mock\",\"object\":\"chat.completion.chunk\",\"model\":\"mock\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"tok{i} \"}},\"finish_reason\":null}}]}}\n\n"
    )
}

pub const DONE: &str = "data: [DONE]\n\n";
pub const HEADER: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";

/// Expected full body for `n` events.
pub fn expected_body(n: usize) -> Vec<u8> {
    let mut body = String::new();
    for i in 0..n {
        body.push_str(&event(i));
    }
    body.push_str(DONE);
    body.into_bytes()
}

#[derive(Debug, Clone, Copy)]
struct Params {
    n: usize,
    gap_us: u64,
    first_ms: u64,
}

fn parse_params(head: &str) -> Params {
    let mut p = Params {
        n: 512,
        gap_us: 0,
        first_ms: 0,
    };
    let target = head.split_whitespace().nth(1).unwrap_or("");
    if let Some((_, query)) = target.split_once('?') {
        for kv in query.split('&') {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            match k {
                "n" => p.n = v.parse().unwrap_or(p.n),
                "gap_us" => p.gap_us = v.parse().unwrap_or(0),
                "first_ms" => p.first_ms = v.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    p
}

/// Read one HTTP request head (+ Content-Length body, discarded).
pub async fn read_request(sock: &mut TcpStream) -> io::Result<String> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 64 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "head too large"));
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let content_length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut have = buf.len() - head_end;
    while have < content_length {
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        have += n;
    }
    Ok(head)
}

async fn serve_one(mut sock: TcpStream) -> io::Result<()> {
    sock.set_nodelay(true)?;
    let head = read_request(&mut sock).await?;
    let p = parse_params(&head);
    sock.write_all(HEADER.as_bytes()).await?;
    sock.flush().await?;
    if p.first_ms > 0 {
        tokio::time::sleep(Duration::from_millis(p.first_ms)).await;
    }
    for i in 0..p.n {
        if i > 0 && p.gap_us > 0 {
            tokio::time::sleep(Duration::from_micros(p.gap_us)).await;
        }
        sock.write_all(event(i).as_bytes()).await?;
        sock.flush().await?;
    }
    sock.write_all(DONE.as_bytes()).await?;
    sock.shutdown().await?;
    Ok(())
}

/// Bind and serve forever; returns the bound address via the listener.
pub async fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

pub async fn serve(listener: TcpListener) -> io::Result<()> {
    loop {
        let (sock, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = serve_one(sock).await;
        });
    }
}
