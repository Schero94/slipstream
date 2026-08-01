# Slipstream 0.2.32

**Canonical product repo.** Releases ship here (`Schero94/slipstream`), not LLM-BOOM.

## Highlights
- **Metal Apply Best bands:** free≥22 → cache 14; ≥17 → 10; else conservative — always **io=4**, headroom **3** (qualified PEREGRINE recipe).
- **Metal Peak** button + Start-time gate for cache≥14 (refuse &lt;17 GiB free; confirm 17–22).
- **I/O default 4** (was 8). Backend serde default **Auto**.
- **Obs strip `cfg`:** shows configured `cache/io/headroom` so path smokes are not misread as peak.
- **Good tokens** preset: thinking off (temp 0 on send).
- **Smoke:** `PROFILE=path|warm|peak` + `--pgrn-io-threads` when io&gt;1.
- **P2P:** CLI `--spawn-engine` refuses live serve lock / healthy endpoint; **JobResult sealed** on wire; Cluster freeze tip; prefer-local Chat contract test.

## Not claimed this release
- Live re-measure of ~18.9 tok/s peak (needs quiet ≥22 GiB free+inactive). Product path now *exposes* the qualified knobs safely.

## Build
`cargo tauri build --bundles app` from `app/src-tauri` (stage `resources/llama-server` / omlx-pgrn as needed), then `ditto` into `/Applications/Slipstream.app`.
