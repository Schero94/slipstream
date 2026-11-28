//! v0.6 M3: live mDNS on this host (announce + browse in one process, two responders).
//! Skips with a clear message when multicast is unavailable (`SLIPSTREAM_NO_MULTICAST=1`,
//! or the mDNS socket cannot be opened).
#![cfg(feature = "mdns")]

use std::time::{Duration, Instant};

use p2p_net::discovery::mdns::{announce_share, browse_continuous, DiscoveryEvent, ShareAdvert, ShareProto};

fn skip_reason() -> Option<String> {
    if std::env::var_os("SLIPSTREAM_NO_MULTICAST").is_some() {
        return Some("SLIPSTREAM_NO_MULTICAST is set".into());
    }
    if let Err(e) = mdns_sd::ServiceDaemon::new() {
        return Some(format!("cannot open the mDNS socket: {e}"));
    }
    None
}

fn random_fp() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("{:016x}", (t as u64) ^ ((std::process::id() as u64) << 40))
}

fn wait_for(
    d: &mut p2p_net::discovery::mdns::Discovery,
    within: Duration,
    mut pred: impl FnMut(&DiscoveryEvent) -> bool,
) -> Option<(DiscoveryEvent, Duration)> {
    let t0 = Instant::now();
    while t0.elapsed() < within {
        if let Some(e) = d.recv_timeout(within.saturating_sub(t0.elapsed())) {
            if pred(&e) {
                return Some((e, t0.elapsed()));
            }
        }
    }
    None
}

#[test]
fn announce_found_pair_flag_and_lost() {
    if let Some(why) = skip_reason() {
        eprintln!("SKIP mdns_live: {why}");
        return;
    }
    let fp = random_fp();
    let mut d = browse_continuous().unwrap();
    let t0 = Instant::now();
    let share = announce_share(&ShareAdvert {
        fp: fp.clone(),
        proto: ShareProto::QuicV1,
        pairing_open: false,
        port: 41234,
    })
    .unwrap();
    let mine = |e: &DiscoveryEvent| matches!(e, DiscoveryEvent::Found { fp: f, .. } if *f == fp);
    let (found, _) = wait_for(&mut d, Duration::from_secs(3), mine).expect("not found within 3 s");
    println!("found_ms={:.0} event={found:?}", t0.elapsed().as_secs_f64() * 1e3);
    let DiscoveryEvent::Found { addrs, port, pairing_open, .. } = found else { unreachable!() };
    assert_eq!(port, 41234);
    assert!(!pairing_open);
    assert!(!addrs.is_empty());
    assert_eq!(share.hostname(), format!("slipstream-{fp}.local."));

    // RFC 6762 §10.2: a cache-flush only replaces records older than 1 s, so a real
    // window (minutes) is modelled with ≥ 1 s between the flag changes.
    std::thread::sleep(Duration::from_millis(1100));
    let t1 = Instant::now();
    share.set_pairing_open(true).unwrap();
    wait_for(&mut d, Duration::from_secs(3), |e| {
        matches!(e, DiscoveryEvent::Found { fp: f, pairing_open: true, .. } if *f == fp)
    })
    .expect("pair=1 never appeared");
    println!("pair_on_ms={:.0}", t1.elapsed().as_secs_f64() * 1e3);
    std::thread::sleep(Duration::from_millis(1100));
    let t2 = Instant::now();
    share.set_pairing_open(false).unwrap();
    wait_for(&mut d, Duration::from_secs(3), |e| {
        matches!(e, DiscoveryEvent::Found { fp: f, pairing_open: false, .. } if *f == fp)
    })
    .expect("pair=1 never disappeared");
    println!("pair_off_ms={:.0}", t2.elapsed().as_secs_f64() * 1e3);

    // more window cycles: the flag must follow every time (no stale TXT, no rename)
    for round in 0..3 {
        for open in [true, false] {
            std::thread::sleep(Duration::from_millis(1100));
            let t = Instant::now();
            share.set_pairing_open(open).unwrap();
            wait_for(&mut d, Duration::from_secs(3), |e| {
                matches!(e, DiscoveryEvent::Found { fp: f, pairing_open, .. } if *f == fp && *pairing_open == open)
            })
            .unwrap_or_else(|| panic!("round {round}: pair={open} not seen within 3 s"));
            println!("round{round}_pair_{}_ms={:.0}", u8::from(open), t.elapsed().as_secs_f64() * 1e3);
        }
    }

    let t3 = Instant::now();
    drop(share);
    wait_for(&mut d, Duration::from_secs(3), |e| matches!(e, DiscoveryEvent::Lost { fp: f } if *f == fp))
        .expect("no Lost after the share stopped");
    println!("lost_ms={:.0}", t3.elapsed().as_secs_f64() * 1e3);
}
