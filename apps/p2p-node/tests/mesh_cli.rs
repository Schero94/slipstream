use std::process::Command;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_slipstream-node"))
}

#[test]
fn exposes_direct_quic_worker_and_client_commands() {
    let output = binary().arg("--help").output().expect("run help");
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("mesh-serve"), "{help}");
    assert!(help.contains("mesh-send-job"), "{help}");
}

#[test]
fn mesh_serve_help_discloses_worker_plaintext_boundary() {
    let output = binary()
        .args(["mesh-serve", "--help"])
        .output()
        .expect("run mesh-serve help");
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("selected worker sees plaintext"), "{help}");
    assert!(help.contains("--donate-capacity"), "{help}");
    assert!(help.contains("--mode"), "{help}");
}

#[test]
fn mesh_client_requires_an_explicit_peer_address() {
    let output = binary()
        .args(["mesh-send-job", "--help"])
        .output()
        .expect("run mesh-send-job help");
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--peer"), "{help}");
    assert!(help.contains("--expected-peer-id"), "{help}");
}
