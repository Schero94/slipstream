#!/usr/bin/env bash
# Serialize CPU/SSD-heavy jobs (model runs, benchmarks, engine builds) across agents and
# sessions so a build never lands in the middle of a measurement.
# usage: scripts/heavy.sh <command> [args...]   (blocks until the lock is free)
LOCK=${SLIPSTREAM_HEAVY_LOCK:-"$HOME/.slipstream-heavy.lock"}
exec /usr/bin/lockf -k "$LOCK" "$@"
