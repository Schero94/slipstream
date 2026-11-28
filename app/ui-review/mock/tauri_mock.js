// Stateful __TAURI__ stub for UI journey review (fresh Mac → download → prepare → start → chat).
// Knobs (set before or during the walk via window.__M):
//   __M.freeRam (GiB), __M.diskFree (GiB), __M.totalRam, __M.speed (sim multiplier)
//   __M.startErr = "[model_missing] The model file (.gguf) is missing. Download it first." to force a
//                  start failure (main.rs ui_err format; try "[low_memory free=5.0 need=9.0 fit=2] ...")
//   __M.prefillTokens = 40000  → chat request simulates a long prompt read
//   __M.startCrash = true      → engine exits while loading (RAM admission refusal in the log)
//   __M.editorRequest({ total: 39517, cached: 0, slots: true })  → a request from an editor
//        (Kilo/OpenCode): /slots phase + prompt_progress (live_stats.slot_activity) and the
//        engine log lines prompt_start / prompt_progress / print_timing tail / prompt_done /
//        n_gen / release. slots:false = log-only fallback. Ticks at 1 s / __M.speed.
//   Your Macs (v0.6, remote_* commands): window.__M.remote knobs
//     .found = [{fp, pairing_open, addrs, paired_peer, paired_name}]  (what "Find my Macs" sees)
//     .code = "482913"  the code the big Mac shows (remote_pair checks it)
//     .useState = "connected" | "connecting" | "reconnecting" | "unreachable" | "port_busy"
//     .modelRunning, .busy, .queue, .activity ({phase, total, processed}), .canStart, .onBattery
//     .removedBy = "Mac Studio" ("this Mac removed you"), .pairingsReset = true
//     .shareRequests = 1 (big Mac badge), .peerPairs() = the other Mac types the code (share side)
//     .connectMs = 1200 (connecting → connected)
//   M5: .useState also "waking" | "asleep" | "version_mismatch" (.mismatchSide "peer"|"self",
//     .remoteVersion), .wakeKnown, .nCtx (the used Mac's context; ≠ the last editor patch →
//     re-patch button), .localBusy (big Mac busy with its own work), .selfBattery (this Mac on
//     battery: saving hint), .shareBattery (sharing on battery: warning), .wakeMs = 20000
//     (waking → asleep after this long, like the 20 s window)
(() => {
  const GiB = 1073741824;
  const M = window.__M = Object.assign({
    phase: "fresh", srv: "down", freeRam: 21, totalRam: 36, diskFree: 412, speed: 1,
    gguf: 0, pgrn: 0, remote: 21 * GiB, pgrnTotal: 19 * GiB, convDone: 0, startErr: null,
    prefillTokens: 0, log: "", calls: [], lastChatBody: null, slotAct: null, lastAuth: null,
  }, window.__M || {});
  const R = M.remote = Object.assign({
    mode: "off", paired: [], found: [{ fp: "3f2a9c41d07b5e88", pairing_open: true, addrs: ["/ip4/192.168.1.20/udp/9003/quic-v1"], paired_peer: null, paired_name: null }],
    code: "482913", useState: "connected", modelRunning: true, busy: false, queue: 0, activity: null, canStart: true,
    onBattery: false, removedBy: null, pairingsReset: false, shareRequests: 0, mayStart: true, lanStarted: false,
    usePeer: null, pairing: null, connectMs: 1200, shareOn: false,
    mismatchSide: "peer", remoteVersion: "0.5.3", wakeKnown: true, nCtx: 65536, localBusy: false,
    selfBattery: false, shareBattery: false, wakeMs: 20000, wakeAt: null,
  }, M.remote || {});
  const STUDIO = "12D3KooWStudioMockPeer000000000000000000000000000";
  const BOOK = "12D3KooWBookMockPeer00000000000000000000000000000";
  const remoteStatus = () => {
    // waking → asleep after the window (like the backend's 20 s)
    if (R.useState === "waking") { R.wakeAt = R.wakeAt || Date.now(); if (Date.now() - R.wakeAt >= R.wakeMs) R.useState = "asleep"; }
    else R.wakeAt = null;
    const u = R.mode === "using" ? {
      peer_id: R.usePeer, name: (R.paired.find((p) => p.peer_id === R.usePeer) || {}).name || "Mac Studio",
      state: R.useState, error: R.useState === "unreachable" ? "dial timed out" : null, bind: "127.0.0.1:8080",
      model_running: R.useState === "connected" ? R.modelRunning : null, model_id: "slipstream", n_ctx: R.nCtx,
      busy: R.busy, queue_len: R.queue, on_battery: R.onBattery, can_start_model: R.canStart, remote_version: R.remoteVersion,
      activity: R.activity, local_busy: R.localBusy,
      wake_remaining_s: R.useState === "waking" ? Math.max(0, Math.ceil((R.wakeMs - (Date.now() - R.wakeAt)) / 1000)) : null,
      wol_sent: R.useState === "waking" && R.wakeKnown, wake_known: R.wakeKnown,
      mismatch_side: R.useState === "version_mismatch" ? R.mismatchSide : null,
      mismatch_version: R.useState === "version_mismatch" ? R.remoteVersion : null, min_version: "0.6",
    } : null;
    let pairing = null;
    if (R.pairing) {
      const left = Math.max(0, 300 - Math.floor((Date.now() - R.pairing.at) / 1000));
      if (R.pairing.state === "open" && left === 0) R.pairing.state = "expired";
      pairing = { code: R.code, remaining_s: R.pairing.state === "open" ? left : 0, state: R.pairing.state,
        paired_name: R.pairing.name || null, failed_attempts: 0 };
    }
    const sh = R.shareOn ? { on: true, announced: true, pairing, requests: R.shareRequests, queued: 0, port: 9003,
      dial: "/ip4/0.0.0.0/udp/9003/quic-v1/p2p/" + STUDIO, lan_ips: ["192.168.1.20"], caffeinate: !R.shareBattery || R.shareRequests > 0,
      on_battery: R.shareBattery, remote_started: false } : null;
    return { mode: R.mode === "using" ? "using" : (R.shareOn ? "sharing" : "off"), use: u, share: sh, paired: R.paired,
      may_start_model: R.mayStart, removed_by: R.removedBy, pairings_reset: R.pairingsReset, lan_started: R.lanStarted, fp: "a1b2c3d4e5f60718",
      on_battery: R.selfBattery };
  };
  R.peerPairs = (name = "MacBook Air") => {
    R.paired.push({ peer_id: BOOK, name, role: "use", paired_at: 1764300000, last_seen: 1764300000, app_version: "0.5.3", fp: "77aa00bb11cc22dd" });
    if (R.pairing) { R.pairing.state = "paired"; R.pairing.name = name; }
  };
  let timer = null, tick = 0, misses = 120000;
  const run = () => M.srv === "ready";
  const step = () => {
    if (M.phase === "downloading") {
      M.gguf = Math.min(M.remote, M.gguf + M.remote / (8 / M.speed));
      if (M.gguf >= M.remote) M.phase = "downloaded";
    } else if (M.phase === "converting") {
      M.convDone = Math.min(M.pgrnTotal, M.convDone + M.pgrnTotal / (8 / M.speed));
      M.pgrn = M.convDone;
      if (M.convDone >= M.pgrnTotal) M.phase = "ready";
    }
  };
  timer = setInterval(step, 1000);
  const mem = () => ({
    total_bytes: M.totalRam * GiB, free_bytes: (run() ? Math.max(1, M.freeRam - 12) : M.freeRam) * GiB,
    wired_bytes: 6 * GiB, active_bytes: 14 * GiB, compressed_bytes: 1.2 * GiB, swap_used_bytes: 0,
  });
  const live = () => {
    tick++;
    if (run()) misses += 900;
    const sess = { total_tokens: run() ? 1834 : 0, cached_tokens: 0, cache_efficiency: 0,
      requests: run() ? 2 : 0, avg_prefill_tps: run() ? 212.5 : 0, avg_decode_tps: run() ? 11.8 : 0 };
    return {
      serving_available: run(), session: sess, alltime: sess,
      last_tps: run() ? 12.4 : null,
      experts: run() ? { hit_rate: 78, misses } : null,
      slot_activity: run() ? M.slotAct : null,
      rss_bytes: run() ? 17.2 * GiB : null, pgrn_high_water_bytes: run() ? 10.1 * GiB : null,
      system: { memory: mem(), e_core_usage: 0.18, p_core_usage: run() ? 0.42 : 0.08, gpu_usage: run() ? 0.61 : 0.02,
        gpu_memory_bytes: run() ? 15 * GiB : null, thermal: "Nominal", load_average: [3.1, 2.8, 2.4], uptime_seconds: 93000 },
    };
  };
  const h = {
    defaults: () => ({ model_dir: "/Users/demo/Modelle", home: "/Users/demo", server_bin: "/Applications/Slipstream.app/llama-server",
      embed_file: "nomic.gguf", embed_url: "https://example.invalid/nomic.gguf" }),
    server_state: () => (R.mode === "using" ? "remote" : M.srv),
    remote_status: () => remoteStatus(),
    remote_api_key: () => "mock0local0token0000000000000000",
    remote_discover: () => { R.lanStarted = true; return R.found; },
    remote_pair: ({ target, code }) => {
      if (String(code).replace(/\D/g, "") !== R.code) throw "[pair_wrong_code] Wrong code. Check the code on the other Mac.";
      const p = { peer_id: STUDIO, name: "Mac Studio", role: "share", paired_at: 1764300000, last_seen: 1764300000, app_version: "0.5.3", fp: "3f2a9c41d07b5e88" };
      R.paired = R.paired.filter((x) => x.peer_id !== STUDIO).concat([p]);
      R.found = R.found.map((f) => (f.fp === "3f2a9c41d07b5e88" || f.fp === target ? { ...f, paired_peer: STUDIO, paired_name: "Mac Studio", pairing_open: false } : f));
      R.removedBy = null;
      return p;
    },
    remote_use: ({ peer, confirm }) => {
      if (R.shareOn) throw "[sharing_active] This Mac is sharing its model. Turn sharing off before using another Mac.";
      if (!R.paired.some((p) => p.peer_id === peer)) throw "[not_paired] This Mac is not paired. Pair it first.";
      if (M.srv !== "down" && !confirm) throw "[local_running name=Mac_Studio] This Mac's model is running. Stop it to use the other Mac?";
      if (M.srv !== "down") { M.srv = "down"; M.log = ""; }
      R.mode = "using"; R.usePeer = peer; R.removedBy = null;
      const target = R.useState === "connecting" ? "connected" : R.useState;
      R.useState = "connecting";
      setTimeout(() => { if (R.mode === "using" && R.useState === "connecting") R.useState = target; }, R.connectMs / M.speed);
      return remoteStatus();
    },
    remote_stop_using: () => { R.mode = "off"; R.usePeer = null; return remoteStatus(); },
    remote_trusted: () => R.paired,
    remote_revoke: ({ peer }) => { R.paired = R.paired.filter((p) => p.peer_id !== peer); if (R.usePeer === peer) { R.mode = "off"; R.usePeer = null; } return remoteStatus(); },
    remote_share_start: () => { if (R.mode === "using") throw "[using_active] This Mac is using another Mac."; R.shareOn = true; return remoteStatus(); },
    remote_share_stop: () => { R.shareOn = false; R.pairing = null; return remoteStatus(); },
    remote_pair_code: () => { R.shareOn = true; R.pairing = { at: Date.now(), state: "open" }; return remoteStatus().share.pairing; },
    remote_pair_cancel: () => { if (R.pairing) R.pairing.state = "cancelled"; return remoteStatus(); },
    remote_start_model: () => { if (!R.canStart) throw "[remote_model_control_off] no"; R.modelRunning = true; return { model_running: true }; },
    remote_stop_model: () => { R.modelRunning = false; return { model_running: false }; },
    remote_set_model_control: ({ allowed }) => { R.mayStart = !!allowed; return remoteStatus(); },
    remote_dismiss_removed: () => { R.removedBy = null; return remoteStatus(); },
    read_log: () => M.log, tail_file: () => "",
    live_stats: live, clear_stats: () => null,
    system_stats: () => ({ cores: 12, perf_cores: 6 }),
    model_status: () => ({ downloading: M.phase === "downloading", converting: M.phase === "converting",
      gguf_bytes: M.gguf, pgrn_bytes: M.phase === "converting" ? M.pgrn * 0.5 : M.pgrn, ready: M.phase === "ready",
      disk_free_gib: M.diskFree, resume: null }),
    remote_size: () => M.remote,
    start_download: () => { if (M.phase === "downloading") throw "[download_running] A download is already running."; M.phase = "downloading"; return null; },
    cancel_download: () => { if (M.phase === "downloading") M.phase = "fresh-partial"; return null; },
    cancel_convert: () => { if (M.phase === "converting") { M.phase = "downloaded"; M.pgrn = 0; } return null; },
    start_convert: () => { M.phase = "converting"; M.convDone = 0; return null; },
    discard_convert: () => null,
    convert_progress: () => M.phase === "converting"
      ? { phase: "write", done_bytes: M.convDone, total_bytes: M.pgrnTotal, mb_s: 1900, eta_s: (M.pgrnTotal - M.convDone) / (1900 * 1048576) }
      : (M.pgrn >= M.pgrnTotal ? { phase: "done" } : { phase: "idle" }),
    start_server: ({ cfg }) => {
      M.lastCfg = cfg;
      // main.rs: port 8080 belongs to the forwarder while using another Mac.
      if (R.mode === "using") throw "[remote_active] This Mac is using another Mac's model. Stop using it first (Your Macs).";
      if (M.startErr) throw M.startErr;
      if (M.startCrash) {
        // Engine exits during load: HTTP never answers (server_state stays "down").
        M.log = "llama_model_load: loading model\n[peregrine] runtime error: the plan fits the machine but not the moment: 5120 MiB is free now and 9800 MiB resident is planned, which would break the 3072 MiB reserve; close something or lower --pgrn-cache-gb\nllama_init_from_model: failed to initialize the context\nmain: error: failed to load model\n";
        return "Server gestartet - lädt Modell (~60s)...";
      }
      M.srv = "loading";
      M.log = "llama_model_load: loading model\n";
      setTimeout(() => { M.srv = "ready"; M.log += "main: server is listening on http://127.0.0.1:8080\nmain loop\n"; }, 4000 / M.speed);
      return "Server startet – Modell wird geladen…";
    },
    stop_server: () => { M.srv = "down"; M.log = ""; return null; },
    runtime_preflight: () => ({ ready: true, schema: 3, mlx_packages: {}, components: [
      { name: "llama_server", applicable: true, required: true, ready: true, detail: "ok" },
      { name: "pgrn_convert", applicable: true, required: true, ready: true, detail: "ok" }] }),
    inspect_storage: () => ({ admitted: true, placement_ok: true, internal: true, available_bytes: M.diskFree * GiB, device: "APPLE SSD", kind: "nvme" }),
    mlx_capability: () => ({ mode: "unavailable", models_with_experts_pgrn: 0 }),
    index_status: () => ({ emb_state: "stopped", qdrant_installed: false, qdrant_running: false, emb_bytes: 0 }),
    p2p_status: () => ({ state: "stopped", mode: "local", donate_capacity: false }),
    p2p_recent_peers: () => [], path_is_dir: () => true, list_ext_model_bases: () => [],
    mlx_runtime_status: () => "{}",
    patch_kilo_config: ({ cfg }) => { M.lastPatch = cfg; return `/Users/demo/.config/${cfg.target || "kilo"}/config.json`; },
  };
  // Editor request simulation: layer-major prompt read (~290 tok/s, progress every tick on
  // /slots, every 3rd tick in the log), then the final checkpoint chunk (no /slots answer, one
  // print_timing line, ~4 ticks), prompt_done, answer, release.
  let editorTask = 100;
  M.editorRequest = ({ total = 39517, cached = 0, slots = true, rate = 2900 } = {}) => {
    const task = ++editorTask, lmEnd = Math.floor(total * 0.93);
    let processed = cached, tms = 0, n = 0, tail = 0, gen = 0;
    const line = (l) => { M.log += l + "\n"; };
    M.log = "";
    line(`I slot launch_slot_: id  0 | task ${task} | processing task, is_child = 0`);
    line(`I slot prompt_start: id  0 | task ${task} | n_prompt = ${total}, n_cached = ${cached}, n_process = ${total - cached}`);
    const pp = () => ({ phase: "prompt", slot: 0, task, total, cache: cached, processed, time_ms: tms, n_decoded: null });
    M.slotAct = slots ? pp() : null;
    const iv = setInterval(() => {
      n++;
      if (processed < lmEnd) {
        tms += 1000 * 10; processed = Math.min(lmEnd, processed + rate);   // one tick = 10 s of engine time
        if (slots) M.slotAct = pp();
        line(`I slot prompt_progress: id  0 | task ${task} | n_processed = ${processed}, n_prompt = ${total}, progress = ${(processed / total).toFixed(3)}, t = ${tms} ms`);
        if (processed >= lmEnd) {
          line(`I slot print_timing: id  0 | task ${task} | prompt processing, n_tokens = ${processed - cached}, progress = ${(processed / total).toFixed(2)}, t = ${(tms / 1000).toFixed(2)} s / ${((processed - cached) / (tms / 1000)).toFixed(2)} tokens per second`);
          M.slotAct = null;   // the server does not answer /slots during a normal batch
        }
      } else if (tail < 4) {
        tail++;
      } else if (gen === 0) {
        processed = total; gen = 1;
        line(`I slot prompt_done: id  0 | task ${task} | n_prompt = ${total}, n_cached = ${cached}, n_processed = ${total - cached}, t = ${tms + 15000} ms, tps = 290.0`);
        if (slots) M.slotAct = { phase: "generating", slot: 0, task, total: null, cache: null, processed: null, time_ms: null, n_decoded: 0 };
      } else if (gen < 6) {
        gen++;
        line(`I slot print_timing: id  0 | task ${task} | n_gen = ${gen * 25}, tg = 13.10 t/s, tg_3s = 12.80 t/s`);
        if (slots) M.slotAct = { ...M.slotAct, n_decoded: gen * 25 };
      } else {
        line(`I slot      release: id  0 | task ${task} | stop processing: n_tokens = ${total + 150}, truncated = 0`);
        M.slotAct = slots ? { phase: "idle", slot: 0, task: -1 } : null;
        clearInterval(iv);
      }
    }, 1000 / M.speed);
    return task;
  };

  window.__TAURI__ = {
    core: { invoke: async (cmd, args) => { M.calls.push(cmd); const f = h[cmd]; if (!f) { if (/^p2p_/.test(cmd)) return null; throw new Error("mock: " + cmd); } return f(args || {}); } },
    dialog: { open: async () => null },
    event: { listen: async () => () => {} },
  };

  // llama-server HTTP mock (127.0.0.1:8080).
  const realFetch = window.fetch.bind(window);
  const enc = new TextEncoder();
  const sse = (o) => enc.encode("data: " + JSON.stringify(o) + "\n\n");
  window.fetch = async (url, opts) => {
    const u = String(url);
    if (!u.startsWith("http://127.0.0.1:8080")) return realFetch(url, opts);
    const hdrs = (opts && opts.headers) || {};
    M.lastAuth = hdrs.Authorization || hdrs.authorization || null;
    if (R.mode === "using") {
      // The forwarder: the app window must send this install's key; the remote model must run.
      if (R.useState !== "connected") throw new TypeError("Failed to fetch");
      if (M.lastAuth !== "Bearer mock0local0token0000000000000000") return new Response(JSON.stringify({ error: { code: 401, message: "bad_local_token" } }), { status: 401 });
      if (!R.modelRunning) return new Response(JSON.stringify({ error: { code: 503, message: "model_not_running" } }), { status: 503 });
    } else if (!run()) throw new TypeError("Failed to fetch");
    if (u.endsWith("/v1/models")) return new Response(JSON.stringify({ data: [{ id: "slipstream" }] }), { headers: { "Content-Type": "application/json" } });
    if (u.endsWith("/v1/chat/completions")) {
      const body = JSON.parse(opts.body || "{}");
      M.lastChatBody = body;
      if (!body.stream) return new Response(JSON.stringify({ choices: [{ message: { content: "ok" } }] }));
      const total = M.prefillTokens || 30;
      const signal = opts.signal;
      const stream = new ReadableStream({
        async start(ctrl) {
          const wait = (ms) => new Promise((r) => setTimeout(r, ms));
          const t0 = Date.now();
          if (total > 100) {
            // ~260 tok/s prompt read, in 4096-token batches, compressed in time by __M.speed.
            for (let done = 0; done < total; done = Math.min(total, done + 4096)) {
              if (signal && signal.aborted) { ctrl.close(); return; }
              const ms = Date.now() - t0;
              M.log = `slot update_slots: id 0 | task 1 | prompt processing, n_tokens = ${done}, progress = ${(done / total).toFixed(2)}, t = ${(ms / 1000).toFixed(2)} s / ${(done / Math.max(0.001, ms / 1000)).toFixed(2)} tokens per second\n`;
              if (body.return_progress) ctrl.enqueue(sse({ choices: [{ index: 0, delta: {} }], prompt_progress: { total, cache: 0, processed: done, time_ms: ms } }));
              await wait(1200 / M.speed);
            }
            if (body.return_progress) ctrl.enqueue(sse({ choices: [{ index: 0, delta: {} }], prompt_progress: { total, cache: 0, processed: total, time_ms: Date.now() - t0 } }));
          }
          const words = "Hallo! Ich laufe lokal auf deinem Mac. Frag mich etwas zu deinem Code.".split(" ");
          let n = 0;
          for (const w of words) {
            if (signal && signal.aborted) { ctrl.close(); return; }
            n++;
            M.log = `slot print_timing: id 0 | task 1 | n_gen = ${n * 20}, tg = 12.40 t/s, tg_3s = 12.10 t/s\n`;
            ctrl.enqueue(sse({ choices: [{ index: 0, delta: { content: w + " " } }] }));
            await wait(150);
          }
          ctrl.enqueue(sse({ choices: [{ index: 0, delta: {}, finish_reason: "stop" }], usage: { completion_tokens: words.length * 2 } }));
          ctrl.enqueue(enc.encode("data: [DONE]\n\n"));
          M.log = "slot release: id 0 | task 1 | stop processing: n_tokens = 120\n";
          ctrl.close();
        },
      });
      return new Response(stream, { headers: { "Content-Type": "text/event-stream" } });
    }
    return new Response("{}", { status: 404 });
  };
})();
