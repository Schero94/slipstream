#!/usr/bin/env bash
# Port 33aeb17 -> 8e23278: find the upstream change behind the decode slowdown. One port binary with temporary env
# toggles that restore the old behaviour of single upstream commits (perf-bisect branch in upstream-port2):
#   PORT_NOCHUNK=1      metal: skip the chunked commit path (#32947)
#   PORT_FUSE_LAST=1    metal: concurrency check on the last fused node only (#33175 reverted)
#   PORT_RS_NOINPLACE=1 graph: recurrent states via gather, not in place (#30139)
#   PORT_CHUNK_FIRST=1  metal: chunked path, but the first chunk is only n_main nodes (commit early like the old path)
# ROUND=2: old / new / nochunk / chunkfirst only
# usage: scripts/heavy.sh bench/fetchwall/port2_perfbisect.sh   (env CACHE, REPS)
set -uo pipefail
cd "$(dirname "$0")"
P2="/Volumes/Crucial X10/SLipstream clone/upstream-port2"
OLD="$P2/bin-old/llama-server"; NEW="$P2/llama.cpp/build-static/bin/llama-server"
C=${CACHE:-10}
for rep in $(seq 1 "${REPS:-2}"); do
  if [ "${ROUND:-1}" = 2 ]; then
    BIN=$OLD SPEC_ARGS=" " ./run_arm_server.sh pb2-old "$C"; sleep 25
    BIN=$NEW SPEC_ARGS=" " ./run_arm_server.sh pb2-new "$C"; sleep 25
    BIN=$NEW SPEC_ARGS=" " ./run_arm_server.sh pb2-nochunk "$C" PORT_NOCHUNK=1; sleep 25
    BIN=$NEW SPEC_ARGS=" " ./run_arm_server.sh pb2-chunkfirst "$C" PORT_CHUNK_FIRST=1; sleep 25
    continue
  fi
  BIN=$OLD SPEC_ARGS=" " ./run_arm_server.sh pb-old "$C"; sleep 25
  BIN=$NEW SPEC_ARGS=" " ./run_arm_server.sh pb-new "$C"; sleep 25
  BIN=$NEW SPEC_ARGS=" " ./run_arm_server.sh pb-nochunk "$C" PORT_NOCHUNK=1; sleep 25
  BIN=$NEW SPEC_ARGS=" " ./run_arm_server.sh pb-fuselast "$C" PORT_FUSE_LAST=1; sleep 25
  BIN=$NEW SPEC_ARGS=" " ./run_arm_server.sh pb-rsgather "$C" PORT_RS_NOINPLACE=1; sleep 25
  BIN=$NEW SPEC_ARGS=" " ./run_arm_server.sh pb-all "$C" PORT_NOCHUNK=1 PORT_FUSE_LAST=1 PORT_RS_NOINPLACE=1; sleep 25
done
echo PB_DONE
