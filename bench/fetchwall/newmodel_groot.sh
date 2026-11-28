#!/usr/bin/env bash
# New model test: Groot-19B-A3B-261023 (GrootTeam, 2026-10-23; expert-pruned Qwen3.6-35B-A3B,
# 128 routed experts, qwen35moe arch) in the app regime (run_arm_server.sh: llama-server, compact
# slots, lookahead on by default, NO MTP). Run inside scripts/heavy.sh.
# Phases: convert · lookahead on/off A/B at 2/4/8 GiB (interleaved, 2 reps, 25 s cooldown) ·
# 12 GiB (all experts cached) parity reference · decay eviction A/B at 2/4 GiB · MTP at 12 GiB ·
# tool-choice smoke. Usage: newmodel_groot.sh [all|convert|ab|decay|mtp|tool]
set -uo pipefail
cd "$(dirname "$0")"
D=$HOME/Modelle/groot-19b-a3b-q4
G=$D/Groot-19B-A3B-261023-Q4_K_M.gguf; P=${G%.gguf}.pgrn
B=../../llama.cpp-slipstream/build-static/bin
PHASE=${1:-all}
if [ "$PHASE" = all ] || [ "$PHASE" = convert ]; then
  [ -s "$P" ] || /usr/bin/time -l "$B/llama-pgrn-convert" --input "$G" --output "$P" --io-threads 4 2>&1 | tail -25
  ls -la "$G" "$P"; df -h "$HOME" | tail -1
fi
if [ "$PHASE" = all ] || [ "$PHASE" = ab ]; then
  for c in 2 4 8; do
    for rep in 1 2; do
      GGUF=$G PGRN=$P ./run_arm_server.sh groot-la "$c"; sleep 25
      GGUF=$G PGRN=$P ./run_arm_server.sh groot-laoff "$c" PGRN_LOOKAHEAD=-1; sleep 25
    done
  done
  GGUF=$G PGRN=$P ./run_arm_server.sh groot-la 12; sleep 25
  GGUF=$G PGRN=$P ./run_arm_server.sh groot-laoff 12 PGRN_LOOKAHEAD=-1; sleep 25
fi
if [ "$PHASE" = all ] || [ "$PHASE" = decay ]; then   # app sets PGRN_EVICT=decay below 0.45 x PGRN (~4.2 GiB here)
  for c in 4 2; do for rep in 1 2; do
    GGUF=$G PGRN=$P ./run_arm_server.sh groot-la "$c"; sleep 25
    GGUF=$G PGRN=$P ./run_arm_server.sh groot-decay "$c" PGRN_EVICT=decay; sleep 25
  done; done
fi
if [ "$PHASE" = all ] || [ "$PHASE" = mtp ]; then     # MTP regime (app uses it only when ~all experts fit)
  for rep in 1 2; do
    GGUF=$G PGRN=$P SPEC_ARGS="--spec-type draft-mtp --spec-draft-n-max 4" ./run_arm_server.sh groot-mtp 12; sleep 25
  done
fi
if [ "$PHASE" = all ] || [ "$PHASE" = tool ]; then
  GGUF=$G PGRN=$P ./toolchoice_smoke.sh; sleep 25
fi
echo GROOT_DONE
