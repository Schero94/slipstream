#!/usr/bin/env bash
# Grammar-forced drafts without a draft model: ngram-simple tuned to never draft by itself,
# grammar drafts on, vs no speculation; rigid tool-call schema, streamed, lookahead on.
cd "$(dirname "$0")"
( cd ../../llama.cpp-slipstream && cmake --build build-static --target llama-server -j 10 2>&1 | grep -E "error:" | head )
G="--spec-type ngram-simple --spec-ngram-simple-size-n 64 --spec-ngram-simple-min-hits 1000000 --spec-draft-n-max 16 --spec-grammar-draft 1"
for c in 4 10; do
  for rep in 1 2; do
    SCHEMA_FILE=$PWD/schema_toolcall.json PROMPT_FILE=$PWD/prompt_tool.txt SPEC_ARGS=" " ./run_arm_server.sh none-tool "$c" PGRN_LOOKAHEAD=0; sleep 25
    SCHEMA_FILE=$PWD/schema_toolcall.json PROMPT_FILE=$PWD/prompt_tool.txt SPEC_ARGS="$G" ./run_arm_server.sh gdraft-tool "$c" PGRN_LOOKAHEAD=0; sleep 25
  done
done
echo GRAMMAR_DONE
