#!/usr/bin/env bash
# Diagnose why lookahead is inert in llama-server: spec off vs MTP on, PGRN_DEBUG logs.
cd "$(dirname "$0")"
( cd ../../llama.cpp-slipstream && cmake --build build-static --target llama-server llama-completion -j 10 2>&1 | grep -E " error|Built target llama-server" )
BIN=../../llama.cpp-slipstream/build-static/bin/llama-server
G=$HOME/Modelle/qwen3.6-35b-a3b-q4/Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf; P=${G%.gguf}.pgrn
for spec in none mtp; do
  extra=(); [ $spec = mtp ] && extra=(--spec-type draft-mtp --spec-draft-n-max 4)
  PGRN_DEBUG=1 PGRN_LOOKAHEAD=0 $BIN -m $G --pgrn $P --pgrn-cache-gb 4 --pgrn-io-threads 8 --pgrn-compact-slots \
     --gpu-layers 99 -c 4096 --parallel 1 -b 2048 -ub 2048 "${extra[@]}" --port 18081 > results/debug-$spec.log 2>&1 &
  PID=$!
  for i in $(seq 1 300); do curl -sf http://127.0.0.1:18081/health >/dev/null && break; sleep 1; done
  curl -s http://127.0.0.1:18081/completion -d '{"prompt":"def fib(n):","n_predict":24,"temperature":0}' >/dev/null
  kill $PID; wait $PID 2>/dev/null
  echo "== spec=$spec"; grep -E "PGRN_DEBUG|PGRN cache" results/debug-$spec.log | head -20
done
echo DEBUG_DONE
