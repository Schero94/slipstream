#!/usr/bin/env bash
# Free internal disk by deleting the LFM2.5 test model (re-downloadable; user allowed model
# deletion), then convert and test Mellum2.1 with the widened PGRN type whitelist.
cd "$(dirname "$0")"
rm -f "$HOME/Modelle/lfm2.5-24b-a2b-q4/LFM2.5-24B-A2B-UD-Q4_K_XL.gguf" "$HOME/Modelle/lfm2.5-24b-a2b-q4/LFM2.5-24B-A2B-UD-Q4_K_XL.pgrn"
rmdir "$HOME/Modelle/lfm2.5-24b-a2b-q4" 2>/dev/null
df -h / | tail -1
./newmodel_mellum.sh
