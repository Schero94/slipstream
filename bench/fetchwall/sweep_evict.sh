#!/usr/bin/env bash
# A/B of the PGRN_EVICT eviction experiment in the app regime (run_arm_server.sh:
# llama-server, MTP draft 4, compact slots, io 8, lookahead on), interleaved arms,
# REPS reps per cache size, 25 s cooldown. Same binary for every arm.
# usage: scripts/heavy.sh bench/fetchwall/sweep_evict.sh   (env: REPS, CACHES, ARMS, NTOK)
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
REPS=${REPS:-2}; CACHES=${CACHES:-"4 10"}; ARMS=${ARMS:-"base lp decay"}
export NTOK=${NTOK:-400}
for C in $CACHES; do
  for r in $(seq 1 "$REPS"); do
    for a in $ARMS; do
      case $a in
        base) "$HERE/run_arm_server.sh" "evict-base-r$r${RTAG:-}" "$C" PGRN_EVICT_UNSET=1 ;;
        *)    "$HERE/run_arm_server.sh" "evict-$a-r$r${RTAG:-}" "$C" PGRN_EVICT="$a" ;;
      esac
      sleep 25
    done
  done
done
