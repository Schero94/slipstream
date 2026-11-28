//! Property-based fuzzing of the pairing wire (v0.6 security review §8: "PairMsg decode").
//! The pair protocol is the only one an *untrusted* peer reaches (while a code is shown), so
//! its decoder must take any bytes: length-prefixed JSON, ≤ 4 KiB, never a panic, never an
//! allocation beyond the cap. `PROPTEST_CASES=<n>` for a long run.

use p2p_node::pairing::{read_msg, refusal_from_peer, write_msg, PairError, PairMsg, MAX_MSG};
use proptest::prelude::*;

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

fn msg() -> impl Strategy<Value = PairMsg> {
    let s = || any::<String>();
    prop_oneof![
        (s(), s()).prop_map(|(spake, x25519)| PairMsg::Start { spake, x25519 }),
        (s(), s()).prop_map(|(spake, x25519)| PairMsg::Reply { spake, x25519 }),
        (s(), s()).prop_map(|(info, confirm)| PairMsg::Finish { info, confirm }),
        (s(), s(), s()).prop_map(|(info, confirm, ack)| PairMsg::Done { info, confirm, ack }),
        (s(), s()).prop_map(|(code, message)| PairMsg::Refused { code, message }),
        Just(PairMsg::Unpair),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), failure_persistence: None, ..ProptestConfig::default() })]

    /// Raw bytes (random length prefix + random body): never panics; a decoded message came
    /// from at most MAX_MSG body bytes.
    #[test]
    fn read_msg_total(len in prop_oneof![0u32..64, 0u32..5000, Just(u32::MAX), Just(MAX_MSG as u32 + 1)], body in proptest::collection::vec(any::<u8>(), 0..600)) {
        let mut wire = len.to_be_bytes().to_vec();
        wire.extend_from_slice(&body);
        let r = rt().block_on(async { read_msg(&mut wire.as_slice()).await });
        if r.is_ok() {
            prop_assert!(len as usize <= MAX_MSG && len as usize <= body.len());
        }
    }

    /// JSON-shaped bodies near the real ones (tag + fields): never panics.
    #[test]
    fn read_msg_json_shaped(t in prop_oneof![Just("start".to_string()), Just("reply".to_string()), Just("finish".to_string()), Just("done".to_string()), Just("refused".to_string()), Just("unpair".to_string()), "[a-z]{0,8}"], extra in any::<String>()) {
        let body = format!(r#"{{"t":"{t}","spake":{extra:?},"x25519":"00","info":1,"confirm":null,"code":"{t}"}}"#);
        let mut wire = (body.len() as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(body.as_bytes());
        let _ = rt().block_on(async { read_msg(&mut wire.as_slice()).await });
    }

    /// Every message we can write, we read back identically (or refuse to write it: too big).
    #[test]
    fn write_read_roundtrip(m in msg()) {
        let mut wire = Vec::new();
        let wrote = rt().block_on(async { write_msg(&mut wire, &m).await });
        if wrote.is_ok() {
            let back = rt().block_on(async { read_msg(&mut wire.as_slice()).await }).unwrap();
            prop_assert_eq!(back, m);
        }
    }

    /// L6: whatever code an unauthenticated peer sends, the error text is ours.
    #[test]
    fn refusal_text_is_local(code in any::<String>()) {
        match refusal_from_peer(&code) {
            PairError::Refused { code: c, message } => {
                prop_assert!(["expired", "too_many_attempts", "already_used", "not_pairing", "busy", "refused"].contains(&c.as_str()));
                // fixed local texts only
                let local = ["The pairing code expired. Show a new code on the other Mac.",
                    "Too many wrong codes. Show a new code on the other Mac.",
                    "This code was already used. Show a new code on the other Mac.",
                    "The other Mac is not showing a pairing code.",
                    "Another Mac is pairing right now. Try again in a moment.",
                    "The other Mac refused pairing."];
                prop_assert!(local.contains(&message.as_str()), "{}", message);
            }
            PairError::WrongCode => prop_assert_eq!(code, "wrong_code"),
            e => prop_assert!(false, "unexpected {e:?}"),
        }
    }
}
