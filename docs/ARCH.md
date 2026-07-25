# Peregrine Architecture Facts

Status: M0a feasibility measurement only. These values were verified from the pinned
model metadata on 2026-07-16 and replace earlier blueprint estimates where they
differ.

## Colibrì-first rule

[`JustVugg/colibri`](https://github.com/JustVugg/colibri) is Peregrine's upstream
reference implementation. Before designing or implementing any M0-M6 component,
inspect `vendor/colibri` and current upstream `dev`: adapt merged code first, port an
open PR second, implement a published paper third, and write Peregrine-only code last.
Do not copy blindly across model or hardware boundaries. Preserve Apache-2.0
attribution and validate every port against Peregrine's own oracle and performance
gates. The maintained component map is in [COLIBRI_REFERENCE.md](COLIBRI_REFERENCE.md).

## Target host

- MacBook Pro `Mac15,6`, Apple M3 Pro (11 CPU cores)
- 38,654,705,664 bytes unified memory
- macOS 26.5.2 (build 25F84)
- `iogpu.wired_limit_mb = 0` at baseline

## Development model

- Metadata revision: `Qwen/Qwen3.6-35B-A3B@995ad96eacd98c81ed38be0c5b274b04031597b0`
- GGUF revision: `unsloth/Qwen3.6-35B-A3B-GGUF@a483e9e6cbd595906af30beda3187c2663a1118c`
- 40 transformer layers: 30 linear-attention and 10 full-attention layers
- 256 routed experts per layer; top-8 routing
- Hidden size 2048; MoE intermediate size 512
- Vocabulary size 248,320
- One configured MTP layer

## Flagship projection

- Metadata revision: `0xSero/Qwen3-Coder-64B@46c090c12bf8de1d9032572c6b05684f7ac37caa`
- 48 transformer layers: 36 linear-attention and 12 full-attention layers
- 410 routed experts per layer after pruning; top-10 routing
- Hidden size 2048; MoE intermediate size 512
- Vocabulary size 151,936
- The published checkpoint has no MTP/NEXTN configuration and no MTP/NEXTN weights.

M0a therefore tests the locality thesis with the development router; it is not proof
of the exact flagship router. The exact flagship requires a second routing gate
before M4. MTP is excluded from the flagship speed budget unless a compatible draft
head is identified or trained and independently validated.
