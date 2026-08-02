use std::net::SocketAddr;
use std::str::FromStr;

use p2p_node::{NodeMode, NodePolicy};

#[test]
fn node_modes_parse_with_stable_names() {
    assert_eq!(NodeMode::from_str("local").unwrap(), NodeMode::Local);
    assert_eq!(NodeMode::from_str("private").unwrap(), NodeMode::Private);
    assert_eq!(
        NodeMode::from_str("community").unwrap(),
        NodeMode::Community
    );
    assert!(NodeMode::from_str("public-ish").is_err());
}

#[test]
fn local_mode_refuses_non_loopback_bind() {
    let public: SocketAddr = "0.0.0.0:9002".parse().unwrap();
    let loopback: SocketAddr = "127.0.0.1:9002".parse().unwrap();

    assert!(NodeMode::Local.validate_listen(loopback).is_ok());
    let error = NodeMode::Local.validate_listen(public).unwrap_err();
    assert!(error.to_string().contains("loopback"), "{error}");
}

#[test]
fn community_donation_is_explicit_opt_in() {
    let defaults = NodePolicy::default();
    assert_eq!(defaults.mode, NodeMode::Local);
    assert!(!defaults.donate_capacity);

    let community = NodePolicy::for_mode(NodeMode::Community);
    assert_eq!(community.mode, NodeMode::Community);
    assert!(!community.donate_capacity);
}

#[test]
fn defaults_are_bounded_and_conservative() {
    let policy = NodePolicy::default();
    assert_eq!(policy.limits.max_concurrent_jobs, 4);
    assert_eq!(policy.limits.max_tokens_per_job, 4096);
    assert!(policy.max_queue_jobs > 0);
    assert!(policy.max_queue_jobs <= 64);
    assert!(policy.limits.max_frame_bytes <= 4 * 1024 * 1024);
    assert!(policy.max_replay_entries <= 100_000);
}

#[test]
fn non_community_mode_cannot_donate_publicly() {
    let mut policy = NodePolicy::for_mode(NodeMode::Private);
    policy.donate_capacity = true;
    let error = policy.validate().unwrap_err();
    assert!(error.to_string().contains("community"), "{error}");
}
