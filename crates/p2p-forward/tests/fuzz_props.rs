//! Property-based fuzzing of the forwarder's parsers (plan §8 "fuzz frame decoder and
//! allowlist parser"; v0.6 security review §8 gap). cargo-fuzz needs nightly + network, so
//! these are proptest targets: random and structured inputs, every case must neither panic
//! nor violate the invariant. Default 2,000 cases each (fast enough for `cargo test`); a long
//! run uses `PROPTEST_CASES=<n>` (recorded in bench/RESULTS.md "v0.6 security review fixes").

use p2p_forward::allowlist::{self, Route};
use p2p_forward::http;
use proptest::prelude::*;

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000)
}

/// Request targets built from path-like pieces, so the allowlist's interesting region
/// (real paths with odd suffixes, encodings, queries, control bytes) is actually reached.
fn target() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("/v1/chat/completions".to_string()),
        Just("/v1/completions".to_string()),
        Just("/completion".to_string()),
        Just("/slots".to_string()),
        Just("/slots/0".to_string()),
        Just("/props".to_string()),
        Just("/health".to_string()),
        Just("?".to_string()),
        Just("#".to_string()),
        Just("%2F".to_string()),
        Just("fail_on_no_slot=1".to_string()),
        Just("action=save".to_string()),
        Just("\r\n".to_string()),
        Just(" HTTP/1.1".to_string()),
        Just("\u{85}".to_string()),
        Just("..".to_string()),
        "[ -~]{0,8}".prop_map(|s| s),
        any::<String>().prop_map(|s| s.chars().take(6).collect()),
    ];
    proptest::collection::vec(piece, 0..6).prop_map(|v| v.concat())
}

fn method() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("GET".to_string()),
        Just("POST".to_string()),
        Just("PUT".to_string()),
        Just("get".to_string()),
        "[A-Z]{0,7}",
        any::<String>(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), failure_persistence: None, ..ProptestConfig::default() })]

    /// The share side writes only constants upstream: whatever comes in, a `Some` carries a
    /// path from the route table, the query is one of the two allowed constants, the target
    /// is visible ASCII, and `POST /slots/*` / `POST /props` never pass.
    #[test]
    fn canonical_only_emits_constants(m in method(), t in target()) {
        if let Some(c) = allowlist::canonical(&m, &t) {
            prop_assert!(c.target().bytes().all(|b| (0x21..=0x7E).contains(&b)));
            prop_assert_eq!(c.method, m.as_str());
            prop_assert_eq!(allowlist::path_of(&t), c.path);
            prop_assert!(matches!(c.query, None | Some("fail_on_no_slot=0") | Some("fail_on_no_slot=1")));
            if c.query.is_some() { prop_assert_eq!(c.route, Route::SlotsSanitized); }
            prop_assert!(!c.path.starts_with("/slots/"));
            prop_assert!(!(c.method == "POST" && c.path == "/props"));
            prop_assert!(allowlist::target_bytes_ok(&t));
            prop_assert!(!t.contains('#'));
        }
    }

    /// Arbitrary bytes: the request-head parser never panics, and anything it accepts is
    /// HTTP/1.1 origin-form.
    #[test]
    fn parse_request_head_total(buf in proptest::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(h) = http::parse_request_head(&buf) {
            prop_assert!(h.target.starts_with('/'));
        }
        let _ = http::parse_response_head(&buf);
        let _ = http::find_head_end(&buf);
    }

    /// Structured heads (valid line + random headers / garbage): never panics; a parsed head
    /// round-trips its method and target.
    #[test]
    fn parse_request_head_structured(m in "[A-Z]{1,7}", t in target(), hs in proptest::collection::vec(("[!-~]{1,12}", any::<String>()), 0..8)) {
        let mut s = format!("{m} {t} HTTP/1.1\r\n");
        for (k, v) in &hs { s.push_str(&format!("{k}: {v}\r\n")); }
        s.push_str("\r\n");
        if let Ok(h) = http::parse_request_head(s.as_bytes()) {
            prop_assert_eq!(h.method, m);
            prop_assert!(h.target.starts_with('/'));
            let _ = allowlist::filter_request_headers(&h.headers);
            let _ = allowlist::browser_origin_refused(&h.headers);
        }
    }

    /// Chunked decoding: never panics (incl. huge sizes), never exceeds the limit, and a body
    /// we encode decodes back exactly.
    #[test]
    fn dechunk_total_and_bounded(buf in proptest::collection::vec(any::<u8>(), 0..512), limit in 0usize..4096) {
        if let Some(out) = http::dechunk_strict(&buf, limit) {
            prop_assert!(out.len() <= limit);
        }
    }

    #[test]
    fn dechunk_roundtrip(chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 1..64), 0..8)) {
        let mut enc = Vec::new();
        for c in &chunks {
            enc.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
            enc.extend_from_slice(c);
            enc.extend_from_slice(b"\r\n");
        }
        enc.extend_from_slice(b"0\r\n\r\n");
        let want: Vec<u8> = chunks.concat();
        prop_assert_eq!(http::dechunk_strict(&enc, 1 << 20), Some(want));
    }

    /// The streaming response framer (use side, upstream bytes split at random points):
    /// never panics, and it only ever delays bytes — the output is always a prefix of the input.
    #[test]
    fn chunk_framer_total(buf in proptest::collection::vec(any::<u8>(), 0..512), cuts in proptest::collection::vec(0usize..512, 0..6)) {
        let mut f = p2p_forward::client::ChunkFramer::default();
        let mut out = Vec::new();
        let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c.min(buf.len())).collect();
        cuts.push(0);
        cuts.push(buf.len());
        cuts.sort_unstable();
        for w in cuts.windows(2) {
            out.extend(f.push(&buf[w[0]..w[1]]));
        }
        prop_assert!(buf.starts_with(&out));
    }

    /// The frame decoder (what a SealedStream yields after decryption) and the status request
    /// parser take arbitrary bytes without panicking; encode → decode round-trips.
    #[test]
    fn frame_and_status_decoders_total(buf in proptest::collection::vec(any::<u8>(), 0..512), kind in 0u8..16, payload in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = p2p_forward::proto::decode(&buf);
        let _ = p2p_forward::status::decode_request(&buf);
        if let Some(k) = p2p_forward::proto::FrameType::from_u8(kind) {
            let mut out = Vec::new();
            if p2p_forward::proto::encode(k, &payload, &mut out).is_ok() {
                let f = p2p_forward::proto::decode(&out).unwrap();
                prop_assert_eq!(f.kind, k);
                prop_assert_eq!(f.payload, payload);
            }
        }
    }

    /// The sanitizers see untrusted upstream bytes too.
    #[test]
    fn sanitizers_total(buf in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = allowlist::sanitize_slots(&buf);
        let _ = allowlist::sanitize_props(&buf);
    }
}

/// Found while writing the dechunk target (an explicit edge case; the random generator did not
/// hit it): a chunk size near `usize::MAX` made `size + 2` overflow (panic in debug builds,
/// wrap in release). Upstream-only input (llama-server's `/slots` / `/props` / status bodies).
#[test]
fn dechunk_huge_chunk_size_is_rejected() {
    assert_eq!(http::dechunk_strict(b"ffffffffffffffff\r\nab\r\n0\r\n\r\n", 1 << 20), None);
    assert_eq!(http::dechunk_strict(b"fffffffffffffffe\r\n", 1 << 20), None);
}
