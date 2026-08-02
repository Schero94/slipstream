use std::process::Command;

#[test]
fn slipstream_node_binary_has_stable_name_and_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_slipstream-node"))
        .arg("--version")
        .output()
        .expect("run slipstream-node");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.trim(), format!("slipstream-node {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn invalid_local_public_bind_fails_before_key_creation() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("must-not-exist.key");
    let output = Command::new(env!("CARGO_BIN_EXE_slipstream-node"))
        .args([
            "serve",
            "--mode",
            "local",
            "--listen",
            "0.0.0.0:9002",
            "--key",
        ])
        .arg(&key)
        .output()
        .expect("run slipstream-node");
    assert!(!output.status.success());
    assert!(!key.exists());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.to_ascii_lowercase().contains("loopback"), "{stderr}");
}
