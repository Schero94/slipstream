#!/usr/bin/env bash
# Draft-model-free speculation in the streamed regime: none vs ngram-simple (lookahead on),
# on fresh text and on a code-rewrite prompt (where context n-grams repeat).
cd "$(dirname "$0")"
for pf in prompt_code.txt prompt_edit.txt; do
  tag=${pf%.txt}; tag=${tag#prompt_}
  for rep in 1 2; do
    PROMPT_FILE=$PWD/$pf SPEC_ARGS=" " ./run_arm_server.sh "none-$tag" 4 PGRN_LOOKAHEAD=0; sleep 25
    PROMPT_FILE=$PWD/$pf SPEC_ARGS="--spec-type ngram-simple --spec-draft-n-max 8" ./run_arm_server.sh "ngram-$tag" 4 PGRN_LOOKAHEAD=0; sleep 25
  done
done
echo NGRAM_DONE
