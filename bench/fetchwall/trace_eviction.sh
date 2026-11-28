#!/usr/bin/env bash
# Record PGRN cache-event traces (PGRN_TRACE) for the offline eviction replay
# (bench/fetchwall/sim_eviction.py). App regime via run_arm_server.sh (MTP draft 4,
# compact slots, io 8, lookahead default on), greedy, 1000 decode tokens.
# usage: scripts/heavy.sh bench/fetchwall/trace_eviction.sh OUTDIR
set -uo pipefail
OUTDIR=$1; mkdir -p "$OUTDIR"
HERE=$(cd "$(dirname "$0")" && pwd)
for C in 4 10; do
  NTOK=1000 "$HERE/run_arm_server.sh" trace-code "$C" PGRN_TRACE="$OUTDIR/code_c$C.trace"
  sleep 25
  # agent-style: continue after the ~30k-token agent prompt (no warm-up, long context)
  NTOK=1000 CTX=40960 WARMUP=0 PROMPT_FILE="$HERE/prompt_30k.txt" \
    "$HERE/run_arm_server.sh" trace-agent30k "$C" PGRN_TRACE="$OUTDIR/agent_c$C.trace"
  sleep 25
done
