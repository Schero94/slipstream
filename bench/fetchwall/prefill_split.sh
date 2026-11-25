#!/usr/bin/env bash
# Feasibility: how much of a 35k-token prefill is SSD fetch vs compute? (llama-completion
# prints PGRN fetch/stage/settle ms next to the prompt eval time.)
cd "$(dirname "$0")"
export PROMPT_FILE=$PWD/prompt_30k.txt CTX=40960 NTOK=4
for c in 4 10; do
  ./run_arm.sh split-ub2048 "$c"; sleep 25
  f=$(ls -t results/*-split-ub2048-c$c.log | head -1)
  grep -E "common_perf_print: prompt eval time|PGRN cache =" "$f"
done
echo SPLIT_DONE
