#!/usr/bin/env bash
cd "$(dirname "$0")"
./e2_context.sh cram512 512; sleep 25
./e2_context.sh cram2048 2048; sleep 25
./e2_context.sh cram4096 4096
echo E2_DONE
