# Slipstream Headless Node

`slipstream-node` runs the same sealed inference node on Linux and macOS without a GUI. The safe default is local-only. The supplied server templates use either `local` or manually connected `private` mode; neither template joins a public community network or donates capacity automatically.

## Privacy boundary

Requests and responses are encrypted while travelling between requester and worker. Relays and passive network observers cannot read them. The selected worker decrypts and sees the plaintext prompt to perform inference. Never send secrets, credentials, private code, or sensitive files to an unknown worker. Sensitive and Secret work stays local by default.

## Build

```bash
cargo build --release --bin slipstream-node
./target/release/slipstream-node --version
```

The node attaches to the local oMLX/llama-compatible HTTP inference endpoint selected by the engine adapter. Use `--spawn-engine` only with a build that includes the `launch` feature and only when no Slipstream/oMLX/llama server is already running.

## Linux / systemd

Create a locked service account, install the binary and unit, then review the bind address, model, and engine before enabling it:

```bash
sudo useradd --system --home /var/lib/slipstream --create-home slipstream
sudo install -m 0755 target/release/slipstream-node /usr/local/bin/slipstream-node
sudo install -m 0644 deploy/systemd/slipstream-node.service /etc/systemd/system/slipstream-node.service
sudo systemctl daemon-reload
sudo systemctl enable --now slipstream-node
sudo journalctl -u slipstream-node -f
```

The example binds private mode to port 9002. Restrict that port with the host firewall to intended peers until the authenticated libp2p WAN transport is enabled.

## Container

```bash
docker build -f deploy/Dockerfile.node -t slipstream-node .
docker run --rm -p 9002:9002 -v slipstream-data:/var/lib/slipstream slipstream-node
```

The final image runs as the unprivileged `slipstream` user. Connect the container to an inference endpoint explicitly; it does not bundle model weights.

## macOS / launchd

The plist is a reviewable template for the upcoming in-app installer. Replace `REPLACE_ME`, create the working directory, install the binary, then load it as the logged-in user. It binds only to loopback and does not donate capacity.

## Explicit community donation

Only use the following combination after reviewing the limits and plaintext-worker disclosure:

```bash
slipstream-node serve --listen 0.0.0.0:9002 --mode community --donate-capacity --models slipstream --engine auto
```

This currently enables the hardened direct protocol, not global discovery/NAT traversal. Public WAN operation remains disabled until the libp2p QUIC/Noise, bootstrap, AutoNAT/DCUtR, and relay validation gates pass.
