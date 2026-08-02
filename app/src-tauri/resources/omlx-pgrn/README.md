# oMLX + PGRN bundle staging

The complete oMLX fork and `libpgrn_host.dylib` are staged here at bundle time
and remain ignored because they are generated/vendor build inputs.

Three release-critical overlays are intentionally versioned even though their
parent tree is ignored:

- `omlx/pgrn/profile.py` defines Slipstream's bounded `contract` profile.
- `omlx/pgrn/store.py` preserves active-bank experts during cache eviction.
- `omlx/api/forced_tool_choice.py` enforces OpenAI required/specific tool calls.

The launcher also resolves oMLX's `auto` prefix-cache limit against current
free space. It preserves an explicit cache limit, otherwise keeps a 3 GiB disk
reserve (`SLIPSTREAM_OMLX_SSD_RESERVE_GIB` overrides it) and disables the SSD
prefix cache if the reserve is already exhausted.

After refreshing the staged oMLX fork, restore these three tracked files before
testing or building. The launcher, bootstrap, lock, dependency manifests, and
runtime helpers beside this README are also versioned. Never stage private
machine-specific helper scripts in a release bundle.

After copying the pinned oMLX fork, run `app/scripts/apply_omlx_overlays.sh` to
apply the small tracked server seam and byte-compile both server and overlay.

The release build must run `app/scripts/stage_uv_runtime.sh`. It accepts only
the manifest-pinned arm64 `uv` version and atomically stages it as `./uv`;
the executable is intentionally a generated release input rather than a large
binary committed to Git.
