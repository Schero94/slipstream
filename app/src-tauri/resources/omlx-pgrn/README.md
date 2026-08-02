# oMLX + PGRN bundle staging

The complete oMLX fork and `libpgrn_host.dylib` are staged here at bundle time
and remain ignored because they are generated/vendor build inputs.

Two release-critical overlays are intentionally versioned even though their
parent tree is ignored:

- `omlx/pgrn/profile.py` defines Slipstream's bounded `contract` profile.
- `omlx/pgrn/store.py` preserves active-bank experts during cache eviction.

After refreshing the staged oMLX fork, restore these two tracked files before
testing or building. The launcher, bootstrap, lock, dependency manifests, and
runtime helpers beside this README are also versioned. Never stage private
machine-specific helper scripts in a release bundle.

The release build must run `app/scripts/stage_uv_runtime.sh`. It accepts only
the manifest-pinned arm64 `uv` version and atomically stages it as `./uv`;
the executable is intentionally a generated release input rather than a large
binary committed to Git.
