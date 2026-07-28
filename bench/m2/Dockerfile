# Linux build environment for the R2 parity gate.
#
# Debian trixie because its GCC 14 accepts the fp16 NEON intrinsics in
# ggml-cpu/vec.cpp; GCC 12 (bookworm) rejects them with "target specific option
# mismatch" on aarch64, which is a toolchain limitation upstream of us.
FROM debian:trixie-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends \
      build-essential \
      cmake \
      python3 \
      python3-numpy \
      ca-certificates \
      time \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /work
