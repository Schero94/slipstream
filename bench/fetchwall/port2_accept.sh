#!/usr/bin/env bash
# Acceptance of the upstream port 33aeb17f8586 -> 8e23278c7c36 (separate port tree, old binaries snapshotted).
# Run the whole thing under scripts/heavy.sh. Phases: parity (llama-completion, 4 GiB), E4 chat reuse (LM=1),
# decode speed (llama-server app regime, 4 + 10 GiB, interleaved old/new, 2 reps), layer-major progress + cancel.
# usage: scripts/heavy.sh bench/fetchwall/port2_accept.sh [phase...]   (phases: parity e4 speed lm; default all)
set -uo pipefail
cd "$(dirname "$0")"
P2="/Volumes/Crucial X10/SLipstream clone/upstream-port2"
OLD=${OLD:-"$P2/bin-old"}
NEW=${NEW:-"$P2/llama.cpp/build-static/bin"}
PHASES=${*:-parity e4 speed lm}
for ph in $PHASES; do
  case $ph in
  parity)
    for rep in 1 2; do
      BIN=$OLD/llama-completion ./run_arm.sh p2old-default 4; sleep 25
      BIN=$NEW/llama-completion ./run_arm.sh p2new-default 4; sleep 25
    done ;;
  e4)
    BIN=$NEW/llama-server LM=1 ./e4_newconv.sh p2new-lm; sleep 25 ;;
  speed)
    for c in 4 10; do
      for rep in 1 2; do
        BIN=$OLD/llama-server SPEC_ARGS=" " ./run_arm_server.sh p2old-srv "$c"; sleep 25
        BIN=$NEW/llama-server SPEC_ARGS=" " ./run_arm_server.sh p2new-srv "$c"; sleep 25
      done
    done ;;
  lm)
    BIN=$NEW/llama-server ./lm_progress.sh p2new; sleep 25
    BIN=$NEW/llama-server ./lm_abort.sh p2new; sleep 25 ;;
  esac
done
echo PORT2_DONE
