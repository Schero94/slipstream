#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

systemd="$root/deploy/systemd/slipstream-node.service"
launchd="$root/deploy/launchd/com.slipstream.node.plist"
dockerfile="$root/deploy/Dockerfile.node"
readme="$root/deploy/README.md"

test -f "$systemd"
test -f "$launchd"
test -f "$dockerfile"
test -f "$readme"

rg -q '^User=slipstream$' "$systemd"
rg -q '^NoNewPrivileges=true$' "$systemd"
rg -q '^ProtectSystem=strict$' "$systemd"
rg -q -- '--mode private' "$systemd"
rg -q 'mesh-serve.*quic-v1' "$systemd"
if rg -q -- '--mode community|--donate-capacity' "$systemd"; then
  echo "systemd template must not enable public community donation" >&2
  exit 1
fi

rg -q '<key>RunAtLoad</key>' "$launchd"
rg -q '<key>KeepAlive</key>' "$launchd"
rg -q '<string>mesh-serve</string>' "$launchd"
rg -q 'quic-v1' "$launchd"
if rg -q -- '--mode community|--donate-capacity' "$launchd"; then
  echo "launchd template must not enable public community donation" >&2
  exit 1
fi

rg -q '^USER slipstream$' "$dockerfile"
rg -q 'ENTRYPOINT \["/usr/local/bin/slipstream-node"\]' "$dockerfile"
rg -q 'EXPOSE 9003/udp' "$dockerfile"
rg -q 'selected worker.*plaintext|Worker.*Klartext' "$readme"

echo "headless assets: PASS"
