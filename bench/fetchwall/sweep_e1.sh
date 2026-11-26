#!/usr/bin/env bash
# E1: realistic append-only agent turn after an interleaved request, app settings (cache-ram 2048).
cd "$(dirname "$0")"
E1_APPEND=1 ./e2_context.sh e1-append-cram2048 2048; sleep 25
E1_APPEND=1 ./e2_context.sh e1-append-cram512 512
echo E1_DONE
