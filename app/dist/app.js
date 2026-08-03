"use strict";
const T = window.__TAURI__;
const invoke = T.core.invoke;
const dialog = T.dialog;
const $ = (id) => document.getElementById(id);
const PORT = 8080;
const EXPERT_MIB = 1.83; // avg bytes per streamed expert (35B geometry)

// Product path: one primary Models folder on the internal SSD (~/Modelle).
// MLX resolves to <root>/mlx. External overflow (e.g. Crucial) is Advanced-only —
// never the sticky Start/stream default. Move models to external only when
// internal would fill; keep Start pointed at real trees under ~/Modelle.
let extBase = localStorage.getItem("pgrn.extBase") || "";
function normalizeRoot(p) {
  return (p || "").trim().replace(/\/+$/, "");
}
/** External/overflow volumes must not hijack the calm primary Models path. */
function isExternalOverflowPath(p) {
  const s = normalizeRoot(p);
  if (!s) return false;
  return /^\/Volumes\//i.test(s) || /Crucial/i.test(s);
}
/** Drop sticky Crucial/Volumes paths so Start uses ~/Modelle. Keeps Advanced overflow opt-in. */
function unstickExternalPrimaryPaths() {
  const root = normalizeRoot(localStorage.getItem("slipstream.modelsRoot") || "");
  if (isExternalOverflowPath(root)) {
    localStorage.removeItem("slipstream.modelsRoot");
  }
  const mlx = normalizeRoot(localStorage.getItem("slipstream.mlxDir") || "");
  if (isExternalOverflowPath(mlx)) {
    localStorage.removeItem("slipstream.mlxDir");
  }
  const ext = normalizeRoot(localStorage.getItem("pgrn.extBase") || "");
  if (isExternalOverflowPath(ext)) {
    // Overflow stays valid in Advanced — but do not auto-bind Start to it.
    // Clear sticky only when it was accidentally the sole catalog root.
    // Keep value if user explicitly set Advanced overflow (extBase); just ensure
    // modelsRoot/mlxDir are internal. No-op for extBase itself.
  }
  extBase = localStorage.getItem("pgrn.extBase") || "";
}
unstickExternalPrimaryPaths();
/** Primary Models folder — default ~/Modelle (internal). */
function defaultModelsRoot() {
  const saved = normalizeRoot(localStorage.getItem("slipstream.modelsRoot") || "");
  if (saved && !isExternalOverflowPath(saved)) return saved;
  if (state.def && state.def.model_dir && !isExternalOverflowPath(state.def.model_dir)) {
    return normalizeRoot(state.def.model_dir);
  }
  if (state.def && state.def.home) return `${state.def.home}/Modelle`;
  return "";
}
function mlxDirFromRoot(root) {
  const r = normalizeRoot(root);
  return r ? `${r}/mlx` : "";
}
/**
 * oMLX --model-dir must be the catalog parent (…/mlx) with one subdir per model.
 * Pickers often select the leaf (…/mlx/Qwen…-4bit); coerce that back to the parent.
 */
function coerceMlxCatalogDir(dir) {
  const d = normalizeRoot(dir);
  if (!d) return d;
  // Generic: …/mlx/<leaf> → …/mlx (one segment after /mlx).
  const m = d.match(/^(.*\/mlx)\/[^/]+$/);
  if (m) return m[1];
  if (typeof MODELS !== "undefined") {
    for (const mod of MODELS) {
      const id = mod.mlx && mod.mlx.id;
      if (id && d.endsWith("/" + id)) return normalizeRoot(d.slice(0, -(id.length + 1)));
    }
  }
  return d;
}
function ggufBase() { return extBase || defaultModelsRoot() || (state.def ? state.def.model_dir : ""); }
/** Prefer derived <Models>/mlx; Advanced override (slipstream.mlxDir) wins when set. */
function defaultMlxDir() {
  const saved = coerceMlxCatalogDir(localStorage.getItem("slipstream.mlxDir") || "");
  if (saved && !isExternalOverflowPath(saved)) {
    const raw = normalizeRoot(localStorage.getItem("slipstream.mlxDir") || "");
    if (raw && saved !== raw) localStorage.setItem("slipstream.mlxDir", saved);
    return saved;
  }
  if (saved && isExternalOverflowPath(saved)) {
    localStorage.removeItem("slipstream.mlxDir");
  }
  return mlxDirFromRoot(defaultModelsRoot())
    || (state.def && state.def.home ? `${state.def.home}/Modelle/mlx` : "");
}
/** Metal catalog root = primary Models folder. */
function preferredMetalModelDir() {
  return defaultModelsRoot();
}
/** MLX catalog root: persisted override, else <Models folder>/mlx. */
function preferredMlxModelDir() {
  return defaultMlxDir();
}
/** True when MLX path should follow Models folder (not a custom Advanced override). */
function shouldSyncMlxFromRoot(root) {
  const r = normalizeRoot(root);
  if (!r) return false;
  const saved = normalizeRoot(localStorage.getItem("slipstream.mlxDir") || "");
  if (!saved) return true;
  if (saved === mlxDirFromRoot(r)) return true;
  if (state.def && state.def.home && saved === `${state.def.home}/Modelle/mlx`) return true;
  // Previously synced from an older models root (…/mlx whose parent was the saved root).
  const prevRoot = normalizeRoot(localStorage.getItem("slipstream.modelsRoot") || "");
  if (saved.endsWith("/mlx")) {
    const parent = saved.slice(0, -4);
    if (!prevRoot || parent === prevRoot || parent === r) return true;
  }
  return false;
}
/** Apply primary Models folder; optionally sync MLX to <root>/mlx. */
function applyModelsRoot(root, opts) {
  const r = normalizeRoot(root);
  if (!r) return "";
  // Probe sync policy before writing modelsRoot (uses previous root as baseline).
  const doSync = opts && opts.syncMlx === false
    ? false
    : (opts && opts.syncMlx === true) || shouldSyncMlxFromRoot(r);
  localStorage.setItem("slipstream.modelsRoot", r);
  if (state.def) state.def.model_dir = r;
  if ($("pModelsRoot") && $("pModelsRoot").value.trim() !== r) $("pModelsRoot").value = r;
  if (doSync) {
    const mlx = mlxDirFromRoot(r);
    localStorage.setItem("slipstream.mlxDir", mlx);
    if ($("pMlx")) $("pMlx").value = mlx;
    if (state.model && state.model.mlx) state.model.mlx.dir = mlx;
    MODELS.forEach((m) => { if (m.mlx) m.mlx.dir = mlx; });
  }
  return r;
}
async function pathIsDir(p) {
  if (!p) return false;
  try { return !!(await invoke("path_is_dir", { path: p })); }
  catch { return false; }
}
/** Prefer ~/Modelle (Metal) and ~/Modelle/mlx (MLX) when those dirs exist. */
async function preferDefaultModelPaths() {
  const metal = preferredMetalModelDir();
  const mlx = preferredMlxModelDir();
  const metalOk = metal ? await pathIsDir(metal) : false;
  const mlxOk = mlx ? await pathIsDir(mlx) : false;
  state.preferMetalDir = metalOk ? metal : "";
  state.preferMlxDir = mlxOk ? mlx : "";
  // Soft fallback for picker defaultPath when probe unavailable: still suggest home defaults.
  if (!state.preferMetalDir && metal) state.preferMetalDir = metal;
  if (!state.preferMlxDir && mlx) state.preferMlxDir = mlx;
  if (metal && $("pModelsRoot") && !$("pModelsRoot").value.trim()) {
    $("pModelsRoot").value = metal;
  }
  if (mlxOk && $("pMlx") && !$("pMlx").value.trim()) {
    $("pMlx").value = mlx;
    if (state.model && state.model.mlx) state.model.mlx.dir = mlx;
  }
  return { metal: state.preferMetalDir, mlx: state.preferMlxDir, metalOk, mlxOk };
}
function ensureMlxCatalogDirs() {
  const dir = defaultMlxDir();
  if (!dir) return;
  MODELS.forEach((m) => {
    if (m.mlx && !m.mlx.dir) m.mlx.dir = dir;
  });
}

// Which scope the Status panel shows. Declared up here because the initial
// showTab() call may already want to draw that panel.
let statsScope = localStorage.getItem("slipstream.statsScope") === "alltime" ? "alltime" : "session";

// ---- compatible models (seed the dropdown) --------------------------------
// url: HF repo "owner/name" -> resolve URL is built; or a full https URL.
const MODELS = [
  // kv: "f16" — Qwen3.6 ist hybrid (Linear-Attention, Mini-KV): S1 maß q8-KV mit
  // −12…−28 % Decode am residenten 35B; q8 kauft hier nichts. Full-Attention-Modelle
  // bleiben beim q8_0-Default (KV-RAM wird Cache-Headroom).
  // quants: Qualität↔Speed-Stufen (unsloth UD-Namensschema). Q4 = Default (Speed),
  // Q5/Q6 = mehr Qualität, mehr Disk/RAM, etwas langsamer. Der Nutzer kann jederzeit
  // Speed für Qualität tauschen (ausdrücklich erlaubt).
  { id: "qwen3.6-35b", name: "Qwen3.6-35B-A3B (MTP)", subdir: "qwen3.6-35b-a3b-q4",
    repo: "unsloth/Qwen3.6-35B-A3B-Instruct-GGUF", file: "Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf",
    gb: 21, mtp: true, activeB: 3, spec: "draft-mtp", draft: "", extGguf: true, kv: "f16",
    // MLX twin. Catalog: ~/Modelle/mlx/<id> on internal SSD (external = Advanced overflow).
    mlx: { dir: "", id: "Qwen3.6-35B-A3B-4bit", gb: 20 },
    quants: [
      { label: "Q4_K_XL", tier: "qual.fast", file: "Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf", gb: 21, subdir: "qwen3.6-35b-a3b-q4", mtp: true, spec: "draft-mtp" },
      // UD-Q5 has no MTP layers — draft-max/spec must stay off or load aborts (failed to create MTP context).
      { label: "Q5_K_XL", tier: "qual.more", file: "Qwen3.6-35B-A3B-UD-Q5_K_XL.gguf", gb: 25, subdir: "qwen3.6-35b-a3b-q5", mtp: false, spec: "none" },
      { label: "Q6_K_XL", tier: "qual.best", file: "Qwen3.6-35B-A3B-UD-Q6_K_XL.gguf", gb: 29, subdir: "qwen3.6-35b-a3b-q6", mtp: false, spec: "none" },
    ],
    note: "note.qwen36" },
  { id: "qwen3-30b", name: "Qwen3-30B-A3B", subdir: "qwen3-30b-a3b-q4",
    repo: "unsloth/Qwen3-30B-A3B-GGUF", file: "Qwen3-30B-A3B-UD-Q4_K_XL.gguf",
    gb: 18, mtp: false, activeB: 3, spec: "none", draft: "",
    quants: [
      { label: "Q4_K_XL", tier: "qual.fast", file: "Qwen3-30B-A3B-UD-Q4_K_XL.gguf", gb: 18 },
      { label: "Q5_K_XL", tier: "qual.more", file: "Qwen3-30B-A3B-UD-Q5_K_XL.gguf", gb: 22 },
      { label: "Q6_K_XL", tier: "qual.best", file: "Qwen3-30B-A3B-UD-Q6_K_XL.gguf", gb: 25 },
    ],
    note: "note.qwen30" },
  { id: "deepseek-v2-lite", name: "DeepSeek-V2-Lite (16B-A2.4B)", subdir: "deepseek-v2-lite-q4",
    repo: "unsloth/DeepSeek-V2-Lite-GGUF", file: "DeepSeek-V2-Lite.Q4_K_M.gguf",
    gb: 11, mtp: false, activeB: 2.4, spec: "none", draft: "",
    note: "note.deepseek" },
  { id: "glm-4.5-air", name: "GLM-4.5-Air (106B-A12B)", subdir: "glm-4.5-air-q4",
    repo: "unsloth/GLM-4.5-Air-GGUF", file: "GLM-4.5-Air-UD-Q4_K_XL.gguf",
    gb: 63, mtp: false, activeB: 12, spec: "none", draft: "",
    note: "note.glm" },
  { id: "laguna-s-2.1", name: "Laguna S 2.1 (118B-A8B, DFlash)", subdir: "laguna-s-2.1-q4",
    repo: "", file: "laguna-s-2.1-Q4_K_M.gguf",
    gb: 68, mtp: true, activeB: 8, spec: "draft-dflash", draft: "laguna-s-2.1-DFlash-BF16.gguf", extGguf: true,
    note: "note.laguna" },
  // --- XL streaming tier: verified mainline-llama.cpp MoE, 240-466 GB @ Q4 (multi-shard).
  //     GGUF on a big fast SSD + a generated PGRN sidecar; pick paths via the file picker. ---
  { id: "qwen3-coder-480b", name: "Qwen3-Coder-480B-A35B (Coding, XL)", subdir: "qwen3-coder-480b-q4",
    repo: "unsloth/Qwen3-Coder-480B-A35B-Instruct-GGUF", file: "Qwen3-Coder-480B-A35B-Instruct-Q4_K_M-00001-of-00006.gguf",
    gb: 290, mtp: false, activeB: 35, spec: "none", draft: "", extGguf: true, xl: true,
    note: "note.qwencoder" },
  { id: "llama4-maverick", name: "Llama 4 Maverick (400B-A17B, XL)", subdir: "llama4-maverick-q4",
    repo: "unsloth/Llama-4-Maverick-17B-128E-Instruct-GGUF", file: "Llama-4-Maverick-17B-128E-Instruct-Q4_K_M-00001-of-00005.gguf",
    gb: 243, mtp: false, activeB: 17, spec: "none", draft: "", extGguf: true, xl: true,
    note: "note.maverick" },
  { id: "deepseek-v3", name: "DeepSeek V3 (671B-A37B, XL)", subdir: "deepseek-v3-q4",
    repo: "unsloth/DeepSeek-V3-GGUF", file: "DeepSeek-V3-Q4_K_M-00001-of-00009.gguf",
    gb: 340, mtp: false, activeB: 37, spec: "none", draft: "", extGguf: true, xl: true,
    note: "note.dsv3" },
  { id: "deepseek-r1", name: "DeepSeek R1 (671B-A37B, Reasoning, XL)", subdir: "deepseek-r1-q4",
    repo: "unsloth/DeepSeek-R1-GGUF", file: "DeepSeek-R1-Q4_K_M-00001-of-00009.gguf",
    gb: 404, mtp: false, activeB: 37, spec: "none", draft: "", extGguf: true, xl: true,
    note: "note.dsr1" },
  { id: "glm-5.2", name: "GLM-5.2 (744B-A40B, XL)", subdir: "glm-5.2-q4",
    repo: "unsloth/GLM-5.2-GGUF", file: "GLM-5.2-Q4_K_M-00001-of-00010.gguf",
    gb: 466, mtp: false, activeB: 40, spec: "none", draft: "", extGguf: true, xl: true,
    note: "note.glm52" },
  // --- coming soon: real MoE, but not yet merged into mainline llama.cpp (fork/PR only) ---
  { id: "minimax-m3", name: "MiniMax M3 (428B-A23B)", subdir: "minimax-m3-q4",
    repo: "unsloth/MiniMax-M3-GGUF", file: "", gb: 264, mtp: false, activeB: 23,
    spec: "none", draft: "", extGguf: true, soon: true, xl: true, note: "note.minimax" },
  { id: "deepseek-v4-flash", name: "DeepSeek V4 Flash (284B-A13B)", subdir: "deepseek-v4-flash-q4",
    repo: "", file: "", gb: 146, mtp: false, activeB: 13,
    spec: "none", draft: "", extGguf: true, soon: true, xl: true, note: "note.v4flash" },
];

/** First-launch / reset default — coding MoE, not XL giants (PRODUCT_CLICK_JOURNEY). */
const DEFAULT_MODEL_ID = "qwen3.6-35b";
function defaultModel() {
  return MODELS.find((m) => m.id === DEFAULT_MODEL_ID) || MODELS[0];
}

const state = {
  def: null,          // defaults() from backend
  model: defaultModel(),
  running: false,
  lastMisses: null, lastT: null,
  ssd: [], hit: [], tps: [],  // rolling buffers
  remoteBytes: 0,
  kvMiB: null,        // KV allocation as reported by the engine at load
  bench: null,        // last benchmark result set
  // "metal" | "mlx" | "auto" (heuristic alias persisted as auto). Explicit stays sticky.
  backend: (() => {
    const v = localStorage.getItem("slipstream.backend");
    if (v === "metal" || v === "mlx" || v === "auto" || v === "heuristic") {
      return v === "heuristic" ? "auto" : v;
    }
    return "auto";
  })(),
  // Last Auto/heuristic resolution at Start ("metal"|"mlx"); null when stopped / explicit.
  resolvedBackend: null,
  // Actual MLX profile chosen at Start; contract/fast are safe for expanded
  // tool and JSON-schema prompts under the Apple Metal working-set cap.
  runningPgrnProfile: null,
  nativeRuntime: null,
  nativeStorage: { model: null, pgrn: null },
  // Slipstream P2P experimental mesh (default OFF). Independent of Metal/MLX.
  p2p: localStorage.getItem("slipstream.p2p") === "1",
  p2pRemoteChat: localStorage.getItem("slipstream.p2p.remoteChat") === "1",
  chatModel: "slipstream",
  chatModelsLive: false,   // true after a successful GET /v1/models
  chatModels: [],          // [{id, vlm?}]
  chatAttach: null,        // { path, dataUrl } for OpenAI image_url (VLM)
  chatDoc: null,           // { path, dataUrl, filename, mime } for oMLX file parts (MLX)
  toolPrimeStatus: "idle", // llama only: idle | warming | ready | failed
  toolPrimeController: null,
  toolPrimePromise: null,
};

/** Matches Rust `mlx::AUTO_PREFILL_CHARS` — long prefill prefers Metal. */
const AUTO_PREFILL_CHARS = 8000;

function parseBackendPref(v) {
  if (v === "mlx" || v === "auto" || v === "heuristic") return v === "heuristic" ? "auto" : v;
  return "metal";
}

function isAutoBackend(pref) {
  const p = pref || state.backend;
  return p === "auto" || p === "heuristic";
}

/** Same contract as Rust `mlx::resolve_backend` (explicit sticky; auto uses length + experts.pgrn). */
function resolveAutoBackend(promptChars, hasExpertsPgrn) {
  if (promptChars >= AUTO_PREFILL_CHARS) return "metal";
  if (hasExpertsPgrn) return "mlx";
  return "metal";
}

function estimatePromptChars(extraText) {
  let n = (extraText || "").length;
  const hist = (typeof chat !== "undefined" && chat.history) ? chat.history : [];
  for (const m of hist) {
    const c = m && m.content;
    if (typeof c === "string") n += c.length;
    else if (Array.isArray(c)) {
      for (const part of c) {
        if (part && typeof part.text === "string") n += part.text.length;
      }
    }
  }
  return n;
}

/** Effective engine for UI/chat: preference if explicit, else last Start resolution. */
function effectiveBackend() {
  if (state.backend === "mlx") return "mlx";
  if (state.backend === "metal") return "metal";
  return state.resolvedBackend || "metal";
}

// ---- i18n ------------------------------------------------------------------
const I18N = {
  en: {
    "header.sub": "Large coding models, local on your Mac — streamed from SSD",
    "nav.chat": "Chat", "nav.models": "Models", "nav.downloads": "Downloads", "nav.benchmarks": "Benchmarks", "nav.cluster": "Cluster",
    "nav.logs": "Logs", "nav.settings": "Settings",
    "nav.regions": "Main areas",
    "chat.empty": "Start a conversation with the local model.", "chat.placeholder": "Message the local model…",
    "journey.title": "Models larger than RAM stream from SSD",
    "journey.step1": "Choose model folder",
    "journey.step2": "Start server",
    "journey.step3": "Send code prompt",
    "journey.codingTip": "Coding: Thinking off, greedy (temp 0).",
    "journey.optional": "optional",
    "journey.promptSample": "Write a Python function is_prime(n) with a short docstring and two tests.",
    "journey.needModel": "Choose a model folder first (Models tab).",
    "hint.ssdStream": "Models larger than RAM stream from SSD.",
    "hint.codingPreset": "Coding preset: Thinking off, greedy (temp 0).",
    "hint.startPath": "Models folder → Backend Auto → Start",
    "runtime.title": "Native runtime & SSD",
    "runtime.subtitle": "Verified Slipstream components and real storage devices.",
    "runtime.llama": "llama.cpp/PGRN", "runtime.convert": "PGRN converter",
    "runtime.omlx": "oMLX/PGRN", "runtime.version": "Runtime",
    "runtime.modelDevice": "Model device", "runtime.pgrnDevice": "PGRN device",
    "runtime.diskFree": "Free on PGRN device", "runtime.internal": "internal",
    "runtime.external": "external", "runtime.unknown": "unknown device",
    "runtime.ready": "Required native components and disk reserve verified.",
    "runtime.incomplete": "Native runtime incomplete — repair the app bundle before Start.",
    "runtime.diskUnsafe": "Disk reserve is too low for a safe Start.",
    "runtime.externalPgrn": "PGRN is on external storage. It works, but internal NVMe is usually much faster.",
    "lbl.modelsRoot": "Models folder",
    "hint.modelsRoot": "One folder on the Mac’s internal disk: GGUF + .pgrn here, MLX under mlx/. Default: ~/Modelle. External drives are Advanced overflow only.",
    "tip.modelsRoot": "Primary catalog on internal SSD. Metal looks for GGUF + .pgrn under this folder. MLX uses <folder>/mlx/<model>/. Put models on an external drive only when internal is full (Advanced overflow) — Start still defaults here.",
    "adv.paths": "Advanced: optional overflow / path overrides",
    "adv.p2pL3": "Advanced: L3 expert mirror",
    "adv.p2pHelp": "How to use",
    "hint.pathOverrides": "Only if GGUF or MLX live outside the Models folder (second SSD / custom layout).",
    "hint.mlxPgrnDefaults": "MLX defaults: touch · cold_io=0 · balanced · keep-hot. Rarely need changing.",
    "hint.mlxLevers": "Tools / JSON / Schema use the same Chat contract on both engines. Compact + Grammar drafts are Metal-only; choose Metal or Auto for those.",
    "adv.mlxPgrn": "Advanced: profile / residency / L3 / MCP",
    "chat.stop": "Stop", "chat.clear": "Clear history", "chat.thinking": "Thinking",
    "chat.model": "Model", "chat.tools": "Tools", "chat.json": "JSON", "chat.vlm": "VLM",
    "chat.schema": "Schema",
    "chat.schemaPh": "{\"type\":\"object\",\"properties\":{\"answer\":{\"type\":\"string\"}},\"required\":[\"answer\"]}",
    "chat.schemaOk": "json_schema ready",
    "chat.schemaEmpty": "Empty → json_object (no schema)",
    "chat.schemaBad": "Invalid schema JSON",
    "chat.attachClear": "Remove attachment", "chat.reasoning": "Thinking",
    "chat.toolCall": "Tool call", "chat.toolResult": "Tool result",
    "chat.serverHint": "The server must be running (top right → Start).",
    "chat.viaP2p": "Reply via P2P",
    "tip.chatAttach": "Attach an image (VLM models only). Sent as OpenAI image_url.",
    "tip.chatDocAttach": "Attach a document (PDF/MD/TXT/DOCX/PPTX). MLX only — oMLX MarkItDown file parts.",
    "tip.chatSchema": "Optional JSON Schema for either local engine. Paste a raw schema, {name,schema}, or full response_format. Empty = json_object.",
    "lbl.chatTools": "Enable tools",
    "hint.chatTools": "When on, Chat sends standard OpenAI tools to the active local engine (time + calculator). The Chat toolbar stays in sync.",
    "chat.toolsPriming": "warming tools", "chat.toolsPrimed": "tools warm", "chat.toolsPrimeFailed": "warm-up failed",
    "chat.toolsPrimeWait": "The local tool schema is still warming — send again in a moment.",
    "tip.chatTools": "OpenAI tool calling for both MLX and Metal. Off by default. On MLX, Start selects a bounded contract-safe cache profile.",
    "toast.mlxContractProfile": "MLX contract-safe cache profile active",
    "err.mlxContractRestart": "Tools/JSON need the bounded MLX contract profile on this Mac. Stop and Start once; Slipstream will select it automatically.",
    "sec.mlxPgrn": "Slipstream Settings (MLX)",
    "tip.mlxPgrn": "oMLX streaming profile + residency. Applied as SLIPSTREAM_PGRN_* when you Start with Backend=MLX. Metal/llama.cpp ignores these. Default residency=touch (safe). mlock is opt-in for short measured runs — dual mlock/keep-hot can hard-freeze the Mac. One heavy PGRN server at a time. Overnight / sleep: leave touch or off.",
    "hint.mlxPgrn": "Passed as SLIPSTREAM_PGRN_* on MLX start. Default: touch + keep-hot + io16 (no cold boost). mlock opt-in (never overnight). Sticky Settings may still show an old mlock choice — switch to touch before sleep. Prefetch stays off unless set outside the app.",
    "hint.mlxMemGuard": "Memory guard: default is --memory-guard-gb = total RAM − 3 GiB headroom. Long coding/full-PGRN can hit the Metal wired cap (~28 GiB) and fail with prefill_memory_exceeded — close apps, shrink context, enable “Memory guard off” below, or raise iogpu.wired_limit_mb (sudo). Cold-io stays 0 (profile io16). Start refuses if free+inactive < 4 GiB; warns below 8 GiB.",
    "lbl.memoryGuardOff": "Memory guard off",
    "tip.memoryGuardOff": "Opt-in escape for Metal wired ~28 GiB (prefill_memory_exceeded). Passes --memory-guard off instead of --memory-guard-gb. Default off (safer). Only for long coding/full-PGRN when the wired cap binds. Does not bypass the free-RAM start floor.",
    "warn.mlxFreeCritical": "RAM too low for a safe MLX start (free+inactive < 4 GiB). Close browsers/IDEs and retry.",
    "warn.mlxFreeLow": "Low free RAM (< 8 GiB). MLX may stall or hit the wired cap — close apps first. Prefer residency=touch; avoid mlock overnight.",
    "warn.mlxMlockLow": "Residency is mlock and free RAM looks tight. mlock overnight / dual serve can hard-freeze the Mac. Switch to touch, or continue only for a short measured run?",
    "lbl.pgrnProfile": "Profile",
    "pgrn.profile.balanced": "balanced (default)",
    "pgrn.profile.quality": "quality",
    "pgrn.profile.fast": "fast",
    "hint.pgrnProfileBalanced": "Caps: capacity 4096 · hot 2048 · io_width 16 (preferred for stable warm; no cold-io=32 boost)",
    "hint.pgrnProfileQuality": "Caps: capacity 4096 · hot 2048 · io_width 16 (A/B naming; same io as balanced)",
    "hint.pgrnProfileFast": "Caps: capacity 512 · hot 256 · io_width 4 (tight headroom)",
    "lbl.pgrnResidency": "Residency",
    "tip.pgrnResidency": "Host-owned stream slots. Default touch (fault-in, interactive-safe). mlock is opt-in for short measured runs only (historical quiet peak ~18.9 tok/s — not a current product claim) — never overnight, never two servers, tear down after. off = no residency wiring. Metal arenas no-op. Launcher refuses serve if free+inactive < 8 GiB.",
    "pgrn.residency.mlock": "mlock (opt-in)",
    "pgrn.residency.touch": "touch (default)",
    "pgrn.residency.off": "off",
    "lbl.pgrnKeepHot": "Keep hot (MX protect)",
    "tip.pgrnKeepHot": "Protect the post-prefill MX expert set from LRU. Default on (safe with touch). Pair with mlock only for short measured runs; never two mlock/keep-hot servers; prefer keep-hot+touch overnight over mlock.",
    "lbl.pgrnWarmup": "Warmup on load",
    "tip.pgrnWarmup": "Short greedy generate after load so cold first-token is closer to warm. Default on.",
    "lbl.pgrnPeerBase": "L3 peer base (optional)",
    "tip.pgrnPeerBase": "LAN expert mirror URL. Non-empty enables SLIPSTREAM_PGRN_L3=peer. Separate from sealed P2P jobs. Default OFF.",
    "ph.pgrnPeerBase": "http://192.168.1.10:8765",
    "hint.pgrnPeerBase": "Empty = L3 OFF. Non-empty sets SLIPSTREAM_PGRN_L3=peer + PEER_BASE on MLX start.",
    "lbl.mcpConfig": "MCP config (optional)",
    "tip.mcpConfig": "Absolute path to oMLX MCP JSON/YAML. Non-empty → OMLX_MCP_CONFIG + --mcp-config on MLX start; server tools auto-merge into chat/messages/responses. After serve: GET /v1/mcp/tools and /api/status. Default OFF.",
    "ph.mcpConfig": "/path/to/mcp.json",
    "hint.mcpConfig": "Empty = MCP OFF. Absolute path only. Non-empty sets OMLX_MCP_CONFIG + --mcp-config; verify with GET /v1/mcp/tools when the server is up.",
    "warn.mcpConfigRel": "MCP config path should be absolute (e.g. /Users/…/mcp.json).",
    "lbl.quality": "Quality ↔ speed", "tip.quality": "Higher quant = better answers, more disk + RAM, a little slower. Q4 is the fast default; trade speed for quality anytime.",
    "btn.selectModel": "Select model…", "pill.stopped": "○ Stopped", "pill.starting": "◐ Starting…", "pill.running": "● Running",
    "btn.start": "Start", "btn.stop": "Stop", "tile.usability": "Usability",
    "amp.smooth": "Smooth", "amp.borderline": "Borderline", "amp.pressure": "Pressure", "srv.noModel": "no model", "reco.for": "For your Mac", "reco.free": "free", "reco.with": "with", "reco.pgrnFast": "PGRN on fastest SSD (streamed)", "sec.settings": "Settings", "sec.selectModel": "Choose model",
    "sec.connectAgent": "Connect coding agent", "sec.indexing": "Indexing", "sec.test": "Test", "sec.logs": "Logs & diagnostics",
    "reco.title": "Best settings for your Mac", "btn.applyBest": "Apply best", "btn.applyPeak": "Metal Peak", "btn.goodTokens": "Good tokens",
    "lbl.cache": "Cache size", "lbl.context": "Context", "lbl.io": "I/O threads", "lbl.thinking": "Thinking", "lbl.mtp": "MTP speed", "lbl.compact": "Compact (faster)", "lbl.grammar": "Grammar drafts (JSON/tools)", "lbl.model": "Model",
    "lbl.extBase": "GGUF on second SSD (optional overflow)",
    "hint.extBase": "Optional overflow. Empty = GGUF next to PGRN in the Models folder.",
    "sec.cluster": "Cluster / P2P",
    "sec.p2p": "P2P node", "lbl.p2pEnable": "Enable Slipstream P2P (experimental)",
    "lbl.p2pMode": "Node mode", "lbl.p2pDonate": "Donate bounded capacity to community peers",
    "lbl.p2pRemoteChat": "Allow Chat fallback to a remote peer",
    "hint.p2pWorkerDisclosure": "Traffic is encrypted, but the selected worker decrypts and sees the plaintext prompt for inference. Sensitive and Secret requests stay local by default.",
    "hint.p2p": "Off by default. Local Metal/MLX path unchanged.",
    "hint.p2pL3": "L3 expert peer mirror (separate from sealed jobs; default OFF): host — l3_expert_mirror.py export-mirror then serve-mirror; consumer — SLIPSTREAM_PGRN_L3=peer + SLIPSTREAM_PGRN_PEER_BASE=http://host:8765. See docs/P2P_MVP.md § L3.",
    "hint.p2pSettings": "Start/stop, peers, jobs, and credits are on the Cluster tab.",
    "hint.p2pEngine": "mock = in-process demo (safe default). mlx/llama/auto = HTTP attach to an already-running Slipstream on :8080 — Start the app serve first; UI never spawns engines.",
    "hint.p2pMulti": "Two machines: Start node on each (different listen ports), put the other host:port in Bootstrap/Peer, Probe, then Ask. Keep engine=mock until :8080 is already up.",
    "hint.p2pRecentEmpty": "No peers yet — probe an address or send a job.",
    "btn.p2pGotoCluster": "Open Cluster",
    "lbl.p2pListen": "Listen address", "lbl.p2pListenShort": "Listen",
    "lbl.p2pNodeId": "Node ID", "lbl.p2pCredits": "Credits", "lbl.p2pEngine": "Engine",
    "lbl.p2pState": "Listener", "lbl.p2pSettlement": "Last settle",
    "lbl.p2pBootstrap": "Bootstrap peers", "lbl.p2pPeer": "Peer address",
    "lbl.p2pModel": "Model", "lbl.p2pMaxTokens": "Max tokens", "lbl.p2pAskPrompt": "Prompt",
    "lbl.p2pRecent": "Recent peers",
    "lbl.mlxStream": "MLX streaming",
    "mlx.stream.ready": "streaming ready",
    "mlx.stream.resident": "resident only",
    "mlx.stream.unavailable": "unavailable",
    "mlx.stream.metal": "Metal backend (PGRN SSD)",
    "ph.p2pBootstrap": "127.0.0.1:9001 (comma-separated)",
    "ph.p2pPeer": "empty = this node / in-process loopback",
    "ph.p2pAsk": "hello slipstream p2p",
    "btn.p2pStart": "Start node", "btn.p2pStop": "Stop",
    "btn.p2pProbe": "Probe peers", "btn.p2pSendJob": "Send job", "btn.p2pAsk": "Ask",
    "btn.p2pCredits": "Refresh",
    "p2p.statusOffline": "offline", "p2p.statusListening": "listening", "p2p.statusError": "error",
    "hint.p2pAsk": "When the local server is stopped, Chat also routes here if P2P is enabled.",
    "lang.partial": "{pct}% translated — the rest falls back to English.",
    "btn.download": "Download", "btn.convert": "Convert", "btn.cancel": "Cancel", "btn.send": "Send", "btn.setupStart": "Set up & start",
  },
  de: {
    "header.sub": "Große Coding-Modelle lokal auf dem Mac — von SSD gestreamt",
    "nav.chat": "Chat", "nav.models": "Modelle", "nav.downloads": "Downloads", "nav.benchmarks": "Benchmarks", "nav.cluster": "Cluster",
    "nav.logs": "Logs", "nav.settings": "Einstellungen",
    "nav.regions": "Hauptbereiche",
    "chat.empty": "Starte eine Unterhaltung mit dem lokalen Modell.", "chat.placeholder": "Nachricht an das lokale Modell…",
    "journey.title": "Modelle größer als RAM streamen von der SSD",
    "journey.step1": "Modellordner wählen",
    "journey.step2": "Server starten",
    "journey.step3": "Code-Prompt senden",
    "journey.codingTip": "Coding: Thinking aus, greedy (temp 0).",
    "journey.optional": "optional",
    "journey.promptSample": "Schreibe eine Python-Funktion is_prime(n) mit kurzem Docstring und zwei Tests.",
    "journey.needModel": "Zuerst Modellordner wählen (Tab Modelle).",
    "hint.ssdStream": "Modelle größer als RAM streamen von der SSD.",
    "hint.codingPreset": "Coding-Preset: Thinking aus, greedy (temp 0).",
    "hint.startPath": "Modellordner → Backend Auto → Start",
    "runtime.title": "Native Runtime & SSD",
    "runtime.subtitle": "Verifizierte Slipstream-Komponenten und echte Speichergeräte.",
    "runtime.llama": "llama.cpp/PGRN", "runtime.convert": "PGRN-Converter",
    "runtime.omlx": "oMLX/PGRN", "runtime.version": "Runtime",
    "runtime.modelDevice": "Modell-Gerät", "runtime.pgrnDevice": "PGRN-Gerät",
    "runtime.diskFree": "Frei auf PGRN-Gerät", "runtime.internal": "intern",
    "runtime.external": "extern", "runtime.unknown": "Gerät unbekannt",
    "runtime.ready": "Native Pflichtkomponenten und Disk-Reserve verifiziert.",
    "runtime.incomplete": "Native Runtime unvollständig — App-Bundle vor Start reparieren.",
    "runtime.diskUnsafe": "Disk-Reserve ist für einen sicheren Start zu klein.",
    "runtime.externalPgrn": "PGRN liegt extern. Das funktioniert, interne NVMe ist meist deutlich schneller.",
    "lbl.modelsRoot": "Modellordner",
    "hint.modelsRoot": "Ein Ordner auf der internen SSD: GGUF + .pgrn hier, MLX unter mlx/. Standard: ~/Modelle. Externe Platten nur als Advanced-Overflow.",
    "tip.modelsRoot": "Primärer Katalog auf der internen SSD. Metal sucht GGUF + .pgrn hier. MLX nutzt <Ordner>/mlx/<Modell>/. Externe Platte nur wenn intern voll (Advanced-Overflow) — Start bleibt standardmäßig hier.",
    "adv.paths": "Advanced: optionaler Overflow / Pfad-Overrides",
    "adv.p2pL3": "Advanced: L3-Experten-Mirror",
    "adv.p2pHelp": "So geht’s",
    "hint.pathOverrides": "Nur wenn GGUF oder MLX außerhalb des Modellordners liegen (zweite SSD / eigenes Layout).",
    "hint.mlxPgrnDefaults": "MLX-Defaults: touch · cold_io=0 · balanced · keep-hot. Selten nötig zu ändern.",
    "hint.mlxLevers": "Tools / JSON / Schema nutzen bei beiden Engines denselben Chat-Vertrag. Compact + Grammar-Drafts sind Metal-only; dafür Metal oder Auto wählen.",
    "adv.mlxPgrn": "Advanced: Profil / Residency / L3 / MCP",
    "chat.stop": "Stopp", "chat.clear": "Verlauf löschen", "chat.thinking": "Thinking",
    "chat.model": "Modell", "chat.tools": "Tools", "chat.json": "JSON", "chat.vlm": "VLM",
    "chat.schema": "Schema",
    "chat.schemaPh": "{\"type\":\"object\",\"properties\":{\"answer\":{\"type\":\"string\"}},\"required\":[\"answer\"]}",
    "chat.schemaOk": "json_schema bereit",
    "chat.schemaEmpty": "Leer → json_object (kein Schema)",
    "chat.schemaBad": "Ungültiges Schema-JSON",
    "chat.attachClear": "Anhang entfernen", "chat.reasoning": "Thinking",
    "chat.toolCall": "Tool-Aufruf", "chat.toolResult": "Tool-Ergebnis",
    "chat.serverHint": "Server muss laufen (oben rechts → Start).",
    "chat.viaP2p": "Antwort via P2P",
    "tip.chatAttach": "Bild anhängen (nur VLM). Wird als OpenAI image_url gesendet.",
    "tip.chatDocAttach": "Dokument anhängen (PDF/MD/TXT/DOCX/PPTX). Nur MLX — oMLX MarkItDown file-Parts.",
    "tip.chatSchema": "Optionales JSON Schema für beide lokalen Engines. Rohes Schema, {name,schema} oder volles response_format. Leer = json_object.",
    "lbl.chatTools": "Tools aktivieren",
    "hint.chatTools": "Wenn an, sendet Chat Standard-OpenAI-Tools an die aktive lokale Engine (Zeit + Taschenrechner). Die Chat-Leiste bleibt synchron.",
    "chat.toolsPriming": "Tools wärmen", "chat.toolsPrimed": "Tools warm", "chat.toolsPrimeFailed": "Warm-up fehlgeschlagen",
    "chat.toolsPrimeWait": "Das lokale Tool-Schema wird noch geladen – gleich noch einmal senden.",
    "tip.chatTools": "OpenAI-Tool-Calling für MLX und Metal. Standard aus. Bei MLX wählt Start ein begrenztes, vertragssicheres Cache-Profil.",
    "toast.mlxContractProfile": "MLX-Profil für Tools/JSON aktiv",
    "err.mlxContractRestart": "Tools/JSON brauchen auf diesem Mac das begrenzte MLX-Vertragsprofil. Einmal Stoppen und Starten; Slipstream wählt es automatisch.",
    "sec.mlxPgrn": "Slipstream-Einstellungen (MLX)",
    "tip.mlxPgrn": "oMLX-Streaming-Profil + Residency. Werden als SLIPSTREAM_PGRN_* gesetzt, wenn du mit Backend=MLX startest. Metal/llama.cpp ignoriert sie. Standard residency=touch (sicher). mlock nur opt-in für kurze gemessene Läufe — dual mlock/keep-hot kann den Mac hart einfrieren. Nur ein schwerer PGRN-Server gleichzeitig. Über Nacht / Sleep: touch oder off lassen.",
    "hint.mlxPgrn": "Beim MLX-Start als SLIPSTREAM_PGRN_* übergeben. Standard: touch + keep-hot + io16 (kein cold boost). mlock opt-in (nie über Nacht). Sticky Settings können noch eine alte mlock-Wahl zeigen — vor Sleep auf touch stellen. Prefetch bleibt aus, außer außerhalb der App gesetzt.",
    "hint.mlxMemGuard": "Memory-Guard: Standard ist --memory-guard-gb = Gesamt-RAM − 3 GiB Headroom. Langes Coding/volles PGRN kann an der Metal-Wired-Cap (~28 GiB) scheitern (prefill_memory_exceeded) — Apps schließen, Kontext verkleinern, unten „Memory-Guard aus“ aktivieren oder iogpu.wired_limit_mb erhöhen (sudo). Cold-io bleibt 0 (Profil io16). Start verweigert bei free+inactive < 4 GiB; warnt unter 8 GiB.",
    "lbl.memoryGuardOff": "Memory-Guard aus",
    "tip.memoryGuardOff": "Opt-in-Escape für Metal-Wired ~28 GiB (prefill_memory_exceeded). Setzt --memory-guard off statt --memory-guard-gb. Standard aus (sicherer). Nur bei langem Coding/vollem PGRN, wenn die Wired-Cap greift. Umgeht nicht den Free-RAM-Start-Floor.",
    "warn.mlxFreeCritical": "Zu wenig RAM für sicheren MLX-Start (free+inactive < 4 GiB). Browser/IDEs schließen und erneut versuchen.",
    "warn.mlxFreeLow": "Wenig freier RAM (< 8 GiB). MLX kann stallen oder an die Wired-Cap stoßen — zuerst Apps schließen. Bevorzuge residency=touch; mlock nicht über Nacht.",
    "warn.mlxMlockLow": "Residency ist mlock und freier RAM wirkt knapp. mlock über Nacht / dual Serve kann den Mac einfrieren. Auf touch wechseln — oder nur für einen kurzen gemessenen Lauf fortfahren?",
    "lbl.pgrnProfile": "Profil",
    "pgrn.profile.balanced": "balanced (Standard)",
    "pgrn.profile.quality": "quality",
    "pgrn.profile.fast": "fast",
    "hint.pgrnProfileBalanced": "Caps: capacity 4096 · hot 2048 · io_width 16 (bevorzugt für stabiles Warm; kein cold-io=32 Boost)",
    "hint.pgrnProfileQuality": "Caps: capacity 4096 · hot 2048 · io_width 16 (A/B-Benennung; gleiches io wie balanced)",
    "hint.pgrnProfileFast": "Caps: capacity 512 · hot 256 · io_width 4 (knappere Reserve)",
    "lbl.pgrnResidency": "Residency",
    "tip.pgrnResidency": "Host-eigene Stream-Slots. Standard touch (fault-in, interaktiv sicher). mlock opt-in nur für kurze Messläufe (historischer Quiet-Peak ~18.9 tok/s — kein aktueller Produktclaim) — nie über Nacht, nie zwei Server, danach tear-down. off = keine Residency-Verdrahtung. Metal-Arenen no-op. Launcher lehnt Serve ab wenn free+inactive < 8 GiB.",
    "pgrn.residency.mlock": "mlock (opt-in)",
    "pgrn.residency.touch": "touch (Standard)",
    "pgrn.residency.off": "off",
    "lbl.pgrnKeepHot": "Keep-hot (MX schützen)",
    "tip.pgrnKeepHot": "Schützt das MX-Expertenset nach Prefill vor LRU. Standard an (sicher mit touch). Mit mlock nur für kurze gemessene Läufe; nie zwei mlock/keep-hot Server; über Nacht lieber keep-hot+touch statt mlock.",
    "lbl.pgrnWarmup": "Warmup beim Laden",
    "tip.pgrnWarmup": "Kurzes greedy Generate nach dem Laden, damit das erste Token näher am Warm-Pfad liegt. Standard an.",
    "lbl.pgrnPeerBase": "L3-Peer-Base (optional)",
    "tip.pgrnPeerBase": "LAN-Experten-Mirror-URL. Nicht leer → SLIPSTREAM_PGRN_L3=peer. Getrennt von versiegelten P2P-Jobs. Standard AUS.",
    "ph.pgrnPeerBase": "http://192.168.1.10:8765",
    "hint.pgrnPeerBase": "Leer = L3 AUS. Nicht leer setzt SLIPSTREAM_PGRN_L3=peer + PEER_BASE beim MLX-Start.",
    "lbl.mcpConfig": "MCP-Config (optional)",
    "tip.mcpConfig": "Absoluter Pfad zu oMLX-MCP JSON/YAML. Nicht leer → OMLX_MCP_CONFIG + --mcp-config beim MLX-Start; Server-Tools werden in chat/messages/responses gemerged. Nach Serve: GET /v1/mcp/tools und /api/status. Standard AUS.",
    "ph.mcpConfig": "/path/to/mcp.json",
    "hint.mcpConfig": "Leer = MCP AUS. Nur absoluter Pfad. Nicht leer setzt OMLX_MCP_CONFIG + --mcp-config; prüfen mit GET /v1/mcp/tools wenn der Server läuft.",
    "warn.mcpConfigRel": "MCP-Config-Pfad sollte absolut sein (z. B. /Users/…/mcp.json).",
    "lbl.quality": "Qualität ↔ Speed", "tip.quality": "Höherer Quant = bessere Antworten, mehr Disk + RAM, etwas langsamer. Q4 ist der schnelle Standard; Speed jederzeit für Qualität tauschbar.",
    "btn.selectModel": "Modell wählen…", "pill.stopped": "○ Gestoppt", "pill.starting": "◐ Startet…", "pill.running": "● Läuft",
    "btn.start": "Start", "btn.stop": "Stopp", "tile.usability": "Bedienbarkeit",
    "amp.smooth": "Flüssig", "amp.borderline": "Grenzwertig", "amp.pressure": "Druck", "srv.noModel": "kein Modell",
    "reco.for": "Für deinen Mac", "reco.free": "frei", "reco.with": "mit", "reco.pgrnFast": "PGRN auf die schnellste SSD (gestreamt)", "sec.settings": "Einstellungen", "sec.selectModel": "Modell wählen",
    "sec.connectAgent": "Coding-Agent verbinden", "sec.indexing": "Indexierung", "sec.test": "Test", "sec.logs": "Logs & Diagnose",
    "reco.title": "Auto-Empfehlung für deinen Mac", "btn.applyBest": "Beste anwenden", "btn.applyPeak": "Metal Peak", "btn.goodTokens": "Gute Tokens",
    "lbl.cache": "Cache-Größe", "lbl.context": "Kontext", "lbl.io": "I/O-Threads", "lbl.thinking": "Thinking", "lbl.mtp": "MTP-Speed", "lbl.compact": "Compact (schneller)", "lbl.grammar": "Grammar-Drafts (JSON/Tools)", "lbl.model": "Modell",
    "lbl.extBase": "GGUF auf zweiter SSD (optionaler Overflow)",
    "hint.extBase": "Optionaler Overflow. Leer = GGUF neben PGRN im Modellordner.",
    "sec.cluster": "Cluster / P2P",
    "sec.p2p": "P2P-Node", "lbl.p2pEnable": "Slipstream P2P aktivieren (experimentell)",
    "lbl.p2pMode": "Node-Modus", "lbl.p2pDonate": "Begrenzte Leistung für Community-Peers spenden",
    "lbl.p2pRemoteChat": "Chat-Fallback an einen Remote-Peer erlauben",
    "hint.p2pWorkerDisclosure": "Die Übertragung ist verschlüsselt, aber der ausgewählte Worker entschlüsselt und sieht den Klartext-Prompt für die Inferenz. Sensible und geheime Anfragen bleiben standardmäßig lokal.",
    "hint.p2p": "Standard aus. Lokaler Metal/MLX-Pfad unverändert.",
    "hint.p2pL3": "L3-Experten-Peer-Mirror (getrennt von versiegelten Jobs; Standard AUS): Host — l3_expert_mirror.py export-mirror, dann serve-mirror; Consumer — SLIPSTREAM_PGRN_L3=peer + SLIPSTREAM_PGRN_PEER_BASE=http://host:8765. Siehe docs/P2P_MVP.md § L3.",
    "hint.p2pSettings": "Start/Stop, Peers, Jobs und Credits sind im Cluster-Tab.",
    "hint.p2pEngine": "mock = In-Prozess-Demo (sicherer Default). mlx/llama/auto = HTTP-Attach an schon laufendes Slipstream auf :8080 — Serve zuerst starten; UI spawnt keine Engines.",
    "hint.p2pMulti": "Zwei Macs: auf jedem Node starten (andere Listen-Ports), die andere host:port in Bootstrap/Peer, Peers prüfen, dann Fragen. engine=mock lassen, bis :8080 schon läuft.",
    "hint.p2pRecentEmpty": "Noch keine Peers — Adresse prüfen oder Job senden.",
    "btn.p2pGotoCluster": "Cluster öffnen",
    "lbl.mlxStream": "MLX-Streaming",
    "mlx.stream.ready": "Streaming bereit",
    "mlx.stream.resident": "nur resident",
    "mlx.stream.unavailable": "nicht verfügbar",
    "mlx.stream.metal": "Metal-Backend (PGRN SSD)",
    "lbl.p2pListen": "Listen-Adresse", "lbl.p2pListenShort": "Listen",
    "lbl.p2pNodeId": "Node-ID", "lbl.p2pCredits": "Credits", "lbl.p2pEngine": "Engine",
    "lbl.p2pState": "Listener", "lbl.p2pSettlement": "Letzte Abrechnung",
    "lbl.p2pBootstrap": "Bootstrap-Peers", "lbl.p2pPeer": "Peer-Adresse",
    "lbl.p2pModel": "Modell", "lbl.p2pMaxTokens": "Max. Tokens", "lbl.p2pAskPrompt": "Prompt",
    "lbl.p2pRecent": "Letzte Peers",
    "ph.p2pBootstrap": "127.0.0.1:9001 (kommagetrennt)",
    "ph.p2pPeer": "leer = dieser Node / In-Process-Loopback",
    "ph.p2pAsk": "hello slipstream p2p",
    "btn.p2pStart": "Node starten", "btn.p2pStop": "Stopp",
    "btn.p2pProbe": "Peers prüfen", "btn.p2pSendJob": "Job senden", "btn.p2pAsk": "Fragen",
    "btn.p2pCredits": "Aktualisieren",
    "p2p.statusOffline": "offline", "p2p.statusListening": "hört", "p2p.statusError": "Fehler",
    "hint.p2pAsk": "Wenn der lokale Server gestoppt ist, nutzt Chat denselben Weg (P2P an).",
    "lang.partial": "{pct}% übersetzt — der Rest fällt auf Englisch zurück.",
    "btn.download": "Herunterladen", "btn.convert": "Konvertieren", "btn.cancel": "Abbrechen", "btn.send": "Senden", "btn.setupStart": "Einrichten & starten",
  },
  zh: {
    "header.sub": "在 Mac 上本地运行大型编程模型 — 从 SSD 流式加载",
    "nav.chat": "聊天", "nav.models": "模型", "nav.downloads": "下载", "nav.benchmarks": "基准测试", "nav.cluster": "集群",
    "nav.logs": "日志", "nav.settings": "设置",
    "btn.selectModel": "选择模型…", "pill.stopped": "○ 已停止", "pill.starting": "◐ 启动中…", "pill.running": "● 运行中",
    "btn.start": "启动", "btn.stop": "停止", "tile.usability": "可用性", "sec.settings": "设置", "sec.selectModel": "选择模型",
    "sec.connectAgent": "连接编程助手", "sec.indexing": "索引", "sec.test": "测试", "sec.logs": "日志与诊断",
    "reco.title": "为你的 Mac 推荐最佳设置", "btn.applyBest": "应用最佳",
    "lbl.cache": "缓存大小", "lbl.context": "上下文", "lbl.io": "I/O 线程", "lbl.thinking": "思考", "lbl.mtp": "MTP 加速", "lbl.model": "模型",
    "btn.download": "下载", "btn.convert": "转换", "btn.cancel": "取消", "btn.send": "发送", "btn.setupStart": "设置并启动",
  },
  es: {
    "header.sub": "Modelos de código grandes, locales en tu Mac — transmitidos desde el SSD",
    "nav.chat": "Chat", "nav.models": "Modelos", "nav.downloads": "Descargas", "nav.benchmarks": "Benchmarks", "nav.cluster": "Clúster",
    "nav.logs": "Registros", "nav.settings": "Ajustes",
    "btn.selectModel": "Elegir modelo…", "pill.stopped": "○ Detenido", "pill.starting": "◐ Iniciando…", "pill.running": "● En marcha",
    "btn.start": "Iniciar", "btn.stop": "Parar", "tile.usability": "Usabilidad", "sec.settings": "Ajustes", "sec.selectModel": "Elegir modelo",
    "sec.connectAgent": "Conectar agente", "sec.indexing": "Indexado", "sec.test": "Prueba", "sec.logs": "Registros y diagnóstico",
    "reco.title": "Mejores ajustes para tu Mac", "btn.applyBest": "Aplicar",
    "lbl.cache": "Tamaño de caché", "lbl.context": "Contexto", "lbl.io": "Hilos de E/S", "lbl.thinking": "Razonamiento", "lbl.mtp": "MTP", "lbl.model": "Modelo",
    "btn.download": "Descargar", "btn.convert": "Convertir", "btn.cancel": "Cancelar", "btn.send": "Enviar", "btn.setupStart": "Configurar e iniciar",
  },
};
// Additional keys (EN + DE complete; zh/es fall back to EN for these).
const I18N_EXT = {
  en: {
    "chart.ssd": "SSD throughput", "chart.ssdNote": "experts streamed from SSD",
    "chart.arena": "Expert cache — live", "arena.resident": "resident hit", "arena.stream": "streaming from SSD",
    "chart.hit": "Cache hit-rate", "chart.hitNote": "served resident (no SSD read)",
    "reco.computing": "Computing…", "toast.noSys": "No system data yet", "toast.applied": "Best settings applied", "toast.peakApplied": "Metal Peak: cache 14 · io 4 · headroom 3", "toast.goodTokens": "Good tokens: thinking off · temp 0 on send", "warn.peakNeeds17": "Metal Peak needs ≥17 GiB free (prefer ≥22; admit ≥21.5). Close apps or use Apply best.", "confirm.peakMarginal": "Free RAM is under 21.5 GiB — peak is marginal (admission/stall risk). Preferred quiet is ≥22. Continue with cache=14 anyway?", "hint.p2pFreeze": "Does not spawn a second heavy serve. Start Slipstream first for mlx/llama/auto; one Metal or oMLX process only.",
    "path.gguf": "GGUF folder", "path.pgrn": "PGRN path (streamed)",
    "path.source": "Download source (HuggingFace repo/URL)", "path.binary": "Server binary (our llama.cpp engine)",
    "adv.mirror": "Advanced: 2nd SSD mirror (dual-SSD)", "adv.mirrorPh": "path to a byte-identical .pgrn copy on a 2nd fast disk", "adv.mirrorWarn": "⚠ Only helps with two equally-fast SSDs (2× NVMe or TB4). On internal + slow USB it's slower — leave empty.", "adv.buffered": "Advanced: buffered reads (non-NVMe drives)", "adv.predict": "Advanced: predictive prefetch (experimental)",
    "path.summary": "Source & location",
    "picker.folder": "Choose folder…", "picker.pgrn": "Choose PGRN…", "picker.binary": "Choose binary…",
    "compat.text": "<b>Compatibility:</b> our engine streams <b>experts</b> from SSD — compatible are <b>MoE models</b> with <b>Q4_K/Q5_K/Q6_K</b> experts whose architecture llama.cpp knows (Qwen3-MoE, DeepSeek, Mixtral, GLM-4.5-MoE, Laguna). Not: dense models, IQ/Q2/Q3/Q8_0/MXFP4.",
    "agent.intro": "Slipstream is OpenAI-compatible. One click per agent: the config is patched directly (Kilo/OpenCode) or placed on your clipboard. Prefer the live Chat model id (GET /v1/models); MLX may also expose alias slipstream via model_settings. Raise agent read timeouts (≥300s) for cold PGRN prefills. Indexing embeddings stay on :8090 (nomic) — chat base :8080 is not the embedder. Optional: stream_options.include_usage=true.",
    "agent.patch": "Patch in", "agent.copy": "Copy config", "agent.tagPatch": "1-click patch", "agent.tagCopy": "copy values / config",
    "idx.setup": "Set up & start", "idx.stop": "Stop all",
    "idx.intro": "One click: download the embed model (~100 MB) + Qdrant (~30 MB), start both and patch the agent.",
    "idx.valuesFor": "Values for your agent (Codebase Indexing -> OpenAI Compatible)",
    "idx.hint": "Then enable 'Codebase Indexing' in your agent once, enter the values above, hit 'Start Indexing'. The index stays local in Qdrant.",
    "idx.doneStep": "Done — enable Codebase Indexing in your agent, then restart it.", "idx.doneToast": "Indexing set up",
    "test.placeholder": "Prompt, e.g.: Write a Python function is_prime(n).", "test.noThink": "without thinking (faster)", "test.reasoning": "Reasoning",
    "logs.autoscroll": "Auto-scroll", "logs.diag": "Copy diagnostics",
    "logs.server": "Server", "logs.download": "Download", "logs.convert": "Conversion",
    "logs.none": "(no log)", "logs.empty": "(empty)",
    "btn.copy": "Copy", "toast.copyFail": "Copy failed",
    "qual.fast": "fast (default)", "qual.more": "more quality", "qual.best": "best quality",
    "conv.sha256": "Checksum", "conv.write": "Writing experts", "conv.verify": "Verifying",
    "conv.done": "done", "conv.error": "failed",
    "conv.resume": "Re-checking what's done", "conv.cancelled": "paused",
    "resume.title": "Conversion paused", "resume.of": "of", "resume.experts": "experts",
    "resume.orphan": "An unusable leftover file is in the way — start over to clear it.",
    "btn.resume": "Continue", "btn.restart": "Start over",
    "toast.resumed": "Conversion continued", "toast.discarded": "Paused conversion discarded",
    "badge.ready": "ready", "badge.partial": "partial", "badge.missing": "not set up", "badge.notLoaded": "not loaded",
    "st.ready": "Ready — can be started.", "st.loaded": "loaded — ready", "st.needConvert": "convert needed",
    "st.notThere": "not present — download it.", "st.needGguf": "GGUF missing — check Models folder, or set optional second-SSD overflow under Settings → Advanced.",
    "st.dlRunning": "Download running…", "st.convRunning": "Converting… (GGUF -> PGRN)",
    "reco.cons": "Conservative — runs on 16 GiB Macs, more SSD reads.", "reco.rec": "<b>Recommended</b> for interactive coding on 36 GiB — Mac stays smooth.",
    "models.group.rec": "Recommended for coding", "models.group.xl": "XL giants (advanced)",
    "reco.peak": "<b>Metal Peak (qualified)</b> — 14 GiB + io=4; needs ≥17 GiB free (prefer ≥22; admit ≥21.5). Measured ~18–19 tok/s warm.", "reco.fast": "Fast — needs lots of free RAM; close apps first.", "reco.aggr": "Aggressive — only with lots of free RAM, else swapping.",
    "act.prefill": "Prefill — reading the prompt", "act.decode": "Generating answer", "act.idleReady": "Ready — waiting for a request", "act.stopped": "Server stopped", "act.running": "running…", "act.tokens": "tokens",
    "note.qwen36": "Strongest compatible coder with MTP speed.", "note.qwen30": "Smaller, no MTP — good for weaker Macs.",
    "note.deepseek": "Small & fast, lowest RAM need.", "note.glm": "Large, lots of disk — strong quality.",
    "note.laguna": "Strongest model. Tip: PGRN on the fastest SSD (streamed), GGUF anywhere (load only).",
    "note.qwencoder": "Coding MoE, 35B active. XL — big fast SSD + PGRN sidecar needed.", "note.maverick": "Fastest decode of the giants (17B active). XL — big SSD + PGRN.",
    "note.dsv3": "671B/37B all-rounder. XL — big SSD + PGRN sidecar.", "note.dsr1": "Reasoning model — thinking tokens slow agent use. XL — big SSD.",
    "note.glm52": "Top-tier quality, 466 GB — needs a large SSD or a lower quant.", "note.minimax": "23B active, 264 GB — arrives once merged into llama.cpp.", "note.v4flash": "Ideal fit (13B active, 146 GB) — arrives with mainline llama.cpp.",
    "badge.soon": "soon", "btn.send": "Send",
    "installed.title": "Installed models", "speed.title": "Expected speed", "speed.external": "external SSD - slower",
    "sec.downloads": "Download & conversion",
    "dl.intro": "Fetch the GGUF, then convert it into the streamable PGRN format. Multi-part (XL) models are downloaded shard by shard and converted in one pass.",
    "sec.bench": "Benchmark",
    "bench.intro": "One click: measures prefill (reading the prompt) and decode (writing the answer) against the running server — real numbers, not an estimate.",
    "bench.tokens": "Tokens per run", "bench.runs": "Runs", "bench.run": "Run benchmark", "bench.running": "Benchmarking…",
    "bench.col.run": "Run", "bench.col.prefill": "Prefill", "bench.col.decode": "Decode", "bench.col.hit": "Hit-rate", "bench.col.tokens": "Tokens",
    "bench.mean": "Mean", "bench.needServer": "Start the server first — the benchmark measures the live process.",
    "bench.note": "Run 1 fills the expert cache (cold), later runs show the warm state — that is where a coding session lives. Hit-rate comes from the engine log.",
    "bench.failed": "Benchmark failed", "bench.hint": "Temperature 0, seed 42, prompt cache off — comparable between runs.",
    "sec.memPlan": "Memory plan",
    "mem.resident": "Model resident (attention + embeddings)", "mem.cache": "Expert cache (configured)",
    "mem.kv": "KV cache", "mem.kvPending": "after start", "mem.reserve": "Reserve for macOS",
    "mem.sum": "Total / free RAM",
    "mem.fits": "Fits — the Mac stays usable.",
    "mem.tight": "Tight — close a few apps or shrink the cache by 2 GiB.",
    "mem.over": "Too big for the free RAM — shrink the cache in Settings, otherwise macOS starts swapping.",
    "mem.note": "<b>Why this matters:</b> expert cache + KV cache + a reserve for macOS must fit in RAM. Slipstream keeps a <b>3 GiB reserve</b> — measured: 1.5 GiB leads to Metal residency stalls. If the traffic light goes yellow or red, shrink the cache in Settings.",
    "experts.note": "<b>What you see:</b> each cell is a slot of the bounded expert arena. Green = the expert was already resident (no SSD read), amber = it was streamed in from SSD. The engine partitions the arena per layer — <b>width-weighted</b> when a <code>partition-weights.txt</code> sits next to the PGRN, which measured +11% decode on a warm cache.", "sec.streaming": "SSD streaming", "adv.summary": "Advanced: I/O levers",
    "partition.weighted": "width-weighted (sidecar found)", "partition.equal": "equal split (no sidecar)", "partition.unknown": "–",
    "streaming.note": "<b>Rule of thumb:</b> the PGRN belongs on the fastest SSD — it is read during every single token. The GGUF is only touched at load time and may live on a slow external drive. More I/O threads mainly speed up prefill of long agent prompts.",
    "tip.cluster": "Slipstream P2P (LAN TCP): enable → Start node (mock) → Probe/Ask. Empty peer = loopback. Multi-node: put the other host:port in Bootstrap/Peer. L3 expert HTTP mirror is separate (opt-in; not sealed jobs).",
    "sec.test": "Single test", "sec.serving": "Serving stats", "sec.host": "System",
    "nav.live": "Live",
    "anchor.serving": "Serving", "anchor.streaming": "Streaming",
    "anchor.memory": "Memory", "anchor.system": "System",
    "stat.model": "Model", "stream.fetched": "experts fetched from SSD",
    "host.swap": "Swap in use", "host.noSwap": "none",
    "scope.session": "Session", "scope.alltime": "All-time",
    "stats.clear": "Reset", "stats.sure": "Sure?", "stats.yes": "Yes", "stats.no": "Cancel",
    "stat.tokens": "Total tokens", "stat.cached": "From cache", "stat.efficiency": "Cache efficiency",
    "stat.requests": "Requests", "stat.prefill": "Prefill", "stat.decode": "Decode (avg)",
    "stat.lastTps": "Last decode",
    "stat.rss": "RSS / model mem",
    "stat.pgrnHw": "PGRN high-water",
    "stat.cachedNote": "prompt tokens that did not have to be computed",
    "stats.noServer": "No reading yet — the server is not answering /metrics (Metal) or /api/status (MLX). Start it in Chat; totals below stay as they were.",
    "scope.sessionNote": "since the server started (or your last reset)",
    "scope.alltimeNote": "across all runs, kept on disk",
    "stats.requestNote": "completed generations",
    "stats.noExperts": "– (no streaming run)",
    "chat.metaLive": "live",
    "chat.metaLast": "last",
    "obs.tps": "tok/s", "obs.cache": "cache", "obs.rss": "RSS", "obs.cfg": "cfg",
    "obs.downTip": "Server down — tok/s, cache hit, and RSS show – until Start",
    "obs.upTip": "Last decode tok/s · cache hit (expert or KV) · process RSS (or model memory)",
    "lbl.backend": "Backend", "backend.metal": "llama.cpp · Metal + PGRN (SSD streaming)",
    "backend.mlx": "MLX + SSD PGRN (when sidecar bundled)",
    "backend.auto": "Auto (hybrid)",
    "tip.backend": "Metal + PGRN default. MLX streams experts from SSD when sidecar + experts.pgrn are present. Auto (hybrid): short/warm → MLX if experts.pgrn exists; long prefill (≥8k chars) → Metal. Explicit metal/mlx never overridden. Metal-class ~15 tok/s warm on internal NVMe (measured).",
    "hint.backend": "Auto: short/warm → MLX when experts.pgrn exists, long prefill → Metal. Explicit choice stays sticky.",
    "lbl.mlxDir": "MLX directory (override)",
    "hint.mlxDir": "Catalog parent only — e.g. ~/Modelle/mlx — not the Qwen…-4bit folder inside it. Default is <Models folder>/mlx. SSD streaming needs experts.pgrn beside each model.",
    "err.noMlxModel": "This catalog entry has no MLX twin yet — pick Qwen3.6-35B or switch back to Metal.",
    "err.mlxDirLeaf": "MLX directory must be the catalog parent (…/mlx), not the model folder. Set Advanced → MLX directory to ~/Modelle/mlx, or switch Backend to Metal.",
    "mlx.cap.missingExperts": "experts.pgrn missing — Start will be resident-only (no SSD expert streaming).",
    "mlx.cap.noRuntime": "MLX runtime not installed — one-time wheel download (~0.5–1 GiB).",
    "mlx.cap.noOmlx": "MLX runtime not installed — one-time wheel download (~0.5–1 GiB).",
    "mlx.cap.noLauncher": "PGRN launcher not in this app build — resident oMLX only.",
    "btn.mlxRuntime": "Install MLX runtime",
    "btn.mlxRuntimeBusy": "Installing MLX runtime…",
    "toast.mlxRuntimeStarted": "MLX runtime install started",
    "toast.mlxRuntimeReady": "MLX runtime ready",
    "host.ecores": "E-cores", "host.pcores": "P-cores", "host.gpu": "Utilisation", "host.gpuMem": "GPU memory",
    "host.memory": "Memory", "host.wired": "Wired", "host.active": "Active", "host.compressed": "Compressed", "host.free": "Free",
    "host.section": "Host", "host.thermal": "Thermal", "host.load": "Load", "host.uptime": "Uptime",
    "unit.days": "d", "unit.hours": "h", "unit.minutes": "min",
    "thermal.Nominal": "Nominal", "thermal.Fair": "Fair", "thermal.Serious": "Serious", "thermal.Critical": "Critical",
    "status.note": "<b>Where these come from:</b> serving counters come from Metal <code>/metrics</code> or MLX <code>/api/status</code>, polled every three seconds — same source as the menubar. <b>Last decode</b> is the engine-log eval line (Metal) or <code>avg_generation_tps</code> (MLX). <b>Session</b> restarts with the server; <b>all-time</b> is kept on disk. Prefill speed counts only tokens actually processed. <b>Expert hit-rate</b> comes from the Metal PGRN log or MLX <code>pgrn</code> block on <code>/api/status</code>. RSS / high-water appear when oMLX exposes them.",
  },
  de: {
    "chart.ssd": "SSD-Durchsatz", "chart.ssdNote": "Experten von SSD gestreamt",
    "chart.arena": "Experten-Cache — live", "arena.resident": "resident (Treffer)", "arena.stream": "streamt von SSD",
    "chart.hit": "Cache-Hit-Rate", "chart.hitNote": "resident bedient (kein SSD-Read)",
    "reco.computing": "Ermittle Werte…", "toast.noSys": "Noch keine Systemdaten", "toast.applied": "Beste Einstellungen angewendet", "toast.peakApplied": "Metal Peak: Cache 14 · io 4 · Headroom 3", "toast.goodTokens": "Gute Tokens: Thinking aus · temp 0 beim Senden", "warn.peakNeeds17": "Metal Peak braucht ≥17 GiB frei (besser ≥22; Zulassung ≥21.5). Apps schließen oder Beste anwenden.", "confirm.peakMarginal": "Weniger als 21.5 GiB frei — Peak ist grenzwertig (Admission/Stall-Risiko). Bevorzugtes ruhiges Fenster ≥22. Trotzdem Cache=14?", "hint.p2pFreeze": "Startet keinen zweiten Heavy-Serve. Für mlx/llama/auto zuerst Slipstream starten; nur ein Metal- oder oMLX-Prozess.",
    "path.gguf": "GGUF-Ordner", "path.pgrn": "PGRN-Pfad (gestreamt)",
    "path.source": "Download-Quelle (HuggingFace-Repo/URL)", "path.binary": "Server-Binary (unsere llama.cpp-Engine)",
    "adv.mirror": "Advanced: 2. SSD-Mirror (Dual-SSD)", "adv.mirrorPh": "Pfad zu einer byte-identischen .pgrn-Kopie auf einer 2. schnellen Disk", "adv.mirrorWarn": "⚠ Nur bei zwei gleich schnellen SSDs (2× NVMe oder TB4). Auf intern + langsamer USB ist es langsamer — dann leer lassen.", "adv.buffered": "Advanced: gepufferte Reads (Nicht-NVMe-Laufwerke)", "adv.predict": "Advanced: Predictive Prefetch (experimentell)",
    "path.summary": "Quelle & Speicherort",
    "picker.folder": "Ordner wählen…", "picker.pgrn": "PGRN wählen…", "picker.binary": "Binary wählen…",
    "compat.text": "<b>Kompatibilität:</b> unsere Engine streamt <b>Experten</b> von SSD — kompatibel sind <b>MoE-Modelle</b> mit <b>Q4_K/Q5_K/Q6_K</b>-Experten, deren Architektur llama.cpp kennt (Qwen3-MoE, DeepSeek, Mixtral, GLM-4.5-MoE, Laguna). Nicht: Dense-Modelle, IQ/Q2/Q3/Q8_0/MXFP4.",
    "agent.intro": "Slipstream ist OpenAI-kompatibel. Ein Klick pro Agent: Config wird direkt gepatcht (Kilo/OpenCode) oder in die Zwischenablage gelegt. Bevorzuge die Live-Chat-Modell-ID (GET /v1/models); MLX kann zusätzlich Alias slipstream via model_settings anbieten. Agenten-Read-Timeouts (≥300s) für kalte PGRN-Prefills. Index-Embeddings bleiben :8090 (nomic) — Chat-Base :8080 ist nicht der Embedder. Optional: stream_options.include_usage=true.",
    "agent.patch": "Einpatchen", "agent.copy": "Config kopieren", "agent.tagPatch": "1-Klick-Patch", "agent.tagCopy": "Werte / Config kopieren",
    "idx.setup": "Einrichten & starten", "idx.stop": "Alles stoppen",
    "idx.intro": "Ein Klick: Embed-Modell (~100 MB) + Qdrant (~30 MB) laden, beide starten und Agent patchen.",
    "idx.valuesFor": "Werte für deinen Agenten (Codebase Indexing -> OpenAI Compatible)",
    "idx.hint": "Danach im Agenten einmalig 'Codebase Indexing' aktivieren, obige Werte eintragen, 'Start Indexing'. Der Index bleibt lokal in Qdrant.",
    "idx.doneStep": "Fertig — im Agenten Codebase Indexing aktivieren, dann neu starten.", "idx.doneToast": "Indexierung eingerichtet",
    "test.placeholder": "Prompt, z. B.: Schreibe eine Python-Funktion is_prime(n).", "test.noThink": "ohne Thinking (schneller)", "test.reasoning": "Reasoning (Denkspur)",
    "logs.autoscroll": "Auto-Scroll", "logs.diag": "Diagnose kopieren",
    "logs.server": "Server", "logs.download": "Download", "logs.convert": "Konvertierung",
    "logs.none": "(kein Log)", "logs.empty": "(leer)",
    "btn.copy": "Kopieren", "toast.copyFail": "Kopieren fehlgeschlagen",
    "qual.fast": "schnell (Standard)", "qual.more": "mehr Qualität", "qual.best": "beste Qualität",
    "conv.sha256": "Prüfsumme", "conv.write": "Schreibe Experten", "conv.verify": "Verifiziere",
    "conv.done": "fertig", "conv.error": "fehlgeschlagen",
    "conv.resume": "Prüfe Bisheriges", "conv.cancelled": "angehalten",
    "resume.title": "Konvertierung angehalten", "resume.of": "von", "resume.experts": "Experten",
    "resume.orphan": "Eine unbrauchbare Restdatei blockiert — mit „Neu beginnen\" wegräumen.",
    "btn.resume": "Fortsetzen", "btn.restart": "Neu beginnen",
    "toast.resumed": "Konvertierung fortgesetzt", "toast.discarded": "Angehaltene Konvertierung verworfen",
    "badge.ready": "bereit", "badge.partial": "teilweise", "badge.missing": "nicht eingerichtet", "badge.notLoaded": "nicht geladen",
    "st.ready": "Bereit — kann gestartet werden.", "st.loaded": "geladen — bereit", "st.needConvert": "konvertieren nötig",
    "st.notThere": "nicht vorhanden — herunterladen.", "st.needGguf": "GGUF fehlt — Modellordner prüfen, oder optionalen Overflow unter Einstellungen → Advanced setzen.",
    "st.dlRunning": "Download läuft…", "st.convRunning": "Konvertierung läuft… (GGUF -> PGRN)",
    "reco.cons": "Konservativ — läuft auf 16 GiB Macs, mehr SSD-Reads.", "reco.rec": "<b>Empfohlen</b> für interaktives Coding auf 36 GiB — Mac bleibt flüssig.",
    "models.group.rec": "Empfohlen fürs Coding", "models.group.xl": "XL-Riesen (fortgeschritten)",
    "reco.peak": "<b>Metal Peak (qualifiziert)</b> — 14 GiB + io=4; braucht ≥17 GiB frei (besser ≥22; Zulassung ≥21.5). Gemessen ~18–19 tok/s warm.", "reco.fast": "Schnell — braucht viel freien RAM; erst Apps schließen.", "reco.aggr": "Aggressiv — nur bei viel freiem RAM, sonst Swapping.",
    "act.prefill": "Prefill — Prompt wird gelesen", "act.decode": "Antwort wird generiert", "act.idleReady": "Bereit — wartet auf Anfrage", "act.stopped": "Server gestoppt", "act.running": "läuft…", "act.tokens": "Tokens",
    "note.qwen36": "Stärkster kompatibler Coder mit MTP-Speed.", "note.qwen30": "Kleiner, ohne MTP — gut für schwächere Macs.",
    "note.deepseek": "Klein & schnell, geringster RAM-Bedarf.", "note.glm": "Groß, viel Disk — starke Qualität.",
    "note.laguna": "Stärkstes Modell. Tipp: PGRN auf die schnellste SSD (gestreamt), GGUF egal (nur Load).",
    "note.qwencoder": "Coding-MoE, 35B aktiv. XL — große schnelle SSD + PGRN-Sidecar nötig.", "note.maverick": "Schnellster Decode der Riesen (17B aktiv). XL — große SSD + PGRN.",
    "note.dsv3": "671B/37B Allrounder. XL — große SSD + PGRN-Sidecar.", "note.dsr1": "Reasoning-Modell — Thinking-Tokens bremsen Agent. XL — große SSD.",
    "note.glm52": "Top-Qualität, 466 GB — braucht große SSD oder kleineren Quant.", "note.minimax": "23B aktiv, 264 GB — kommt, sobald in llama.cpp gemerged.", "note.v4flash": "Idealer Fit (13B aktiv, 146 GB) — kommt mit Mainline-llama.cpp.",
    "badge.soon": "bald", "btn.send": "Senden",
    "installed.title": "Installierte Modelle", "speed.title": "Erwartete Geschwindigkeit", "speed.external": "externe SSD - langsamer",
    "sec.downloads": "Download & Konvertierung",
    "dl.intro": "GGUF laden, dann in das gestreamte PGRN-Format konvertieren. Mehrteilige (XL-)Modelle werden Shard für Shard geladen und in einem Durchgang konvertiert.",
    "sec.bench": "Benchmark",
    "bench.intro": "Ein Klick: misst Prefill (Prompt lesen) und Decode (Antwort schreiben) am laufenden Server — echte Zahlen, keine Schätzung.",
    "bench.tokens": "Tokens je Lauf", "bench.runs": "Durchläufe", "bench.run": "Benchmark starten", "bench.running": "Messe…",
    "bench.col.run": "Lauf", "bench.col.prefill": "Prefill", "bench.col.decode": "Decode", "bench.col.hit": "Hit-Rate", "bench.col.tokens": "Tokens",
    "bench.mean": "Mittel", "bench.needServer": "Erst den Server starten — der Benchmark misst den laufenden Prozess.",
    "bench.note": "Lauf 1 füllt den Experten-Cache (kalt), spätere Läufe zeigen den warmen Zustand — dort lebt eine echte Coding-Session. Die Hit-Rate kommt aus dem Engine-Log.",
    "bench.failed": "Benchmark fehlgeschlagen", "bench.hint": "Temperatur 0, Seed 42, Prompt-Cache aus — vergleichbar zwischen Läufen.",
    "sec.memPlan": "Speicherplan",
    "mem.resident": "Modell resident (Attention + Embeddings)", "mem.cache": "Experten-Cache (konfiguriert)",
    "mem.kv": "KV-Cache", "mem.kvPending": "nach dem Start", "mem.reserve": "Reserve für macOS",
    "mem.sum": "Summe / freier RAM",
    "mem.fits": "Passt — der Mac bleibt bedienbar.",
    "mem.tight": "Knapp — ein paar Apps schließen oder den Cache um 2 GiB verkleinern.",
    "mem.over": "Zu groß für den freien RAM — Cache in den Einstellungen verkleinern, sonst swappt macOS.",
    "mem.note": "<b>Warum das zählt:</b> Experten-Cache + KV-Cache + eine Reserve für macOS müssen in den RAM passen. Slipstream hält <b>3 GiB Reserve</b> — gemessen: 1,5 GiB führen zu Metal-Residency-Stalls. Wird die Ampel gelb oder rot, den Cache in den Einstellungen verkleinern.",
    "experts.note": "<b>Was du siehst:</b> jede Zelle ist ein Slot der begrenzten Experten-Arena. Grün = der Experte war schon resident (kein SSD-Read), Orange = er wurde von der SSD gestreamt. Die Engine partitioniert die Arena pro Layer — <b>width-gewichtet</b>, wenn eine <code>partition-weights.txt</code> neben der PGRN liegt (gemessen +11 % Decode bei warmem Cache).", "sec.streaming": "SSD-Streaming", "adv.summary": "Advanced: I/O-Hebel",
    "partition.weighted": "width-gewichtet (Sidecar gefunden)", "partition.equal": "Equal-Split (kein Sidecar)", "partition.unknown": "–",
    "streaming.note": "<b>Daumenregel:</b> die PGRN gehört auf die schnellste SSD — sie wird bei jedem einzelnen Token gelesen. Das GGUF wird nur beim Laden angefasst und darf auf einer langsamen externen Disk liegen. Mehr I/O-Threads beschleunigen vor allem den Prefill langer Agenten-Prompts.",
    "tip.cluster": "Slipstream P2P (LAN-TCP): aktivieren → Node starten (mock) → Peers prüfen / Fragen. Leerer Peer = Loopback. Multi-Node: andere host:port in Bootstrap/Peer. L3-Experten-HTTP-Mirror ist getrennt (opt-in; keine versiegelten Jobs).",
    "sec.test": "Einzel-Test", "sec.serving": "Serving-Statistik", "sec.host": "System",
    "nav.live": "Live",
    "anchor.serving": "Serving", "anchor.streaming": "Streaming",
    "anchor.memory": "Speicher", "anchor.system": "System",
    "stat.model": "Modell", "stream.fetched": "Experten von SSD geholt",
    "host.swap": "Swap in Benutzung", "host.noSwap": "keiner",
    "scope.session": "Sitzung", "scope.alltime": "Gesamt",
    "stats.clear": "Zurücksetzen", "stats.sure": "Sicher?", "stats.yes": "Ja", "stats.no": "Abbrechen",
    "stat.tokens": "Tokens gesamt", "stat.cached": "Aus dem Cache", "stat.efficiency": "Cache-Effizienz",
    "stat.requests": "Anfragen", "stat.prefill": "Prefill", "stat.decode": "Decode (Ø)",
    "stat.lastTps": "Letztes Decode",
    "stat.rss": "RSS / Modell-RAM",
    "stat.pgrnHw": "PGRN High-Water",
    "stat.cachedNote": "Prompt-Tokens, die nicht gerechnet werden mussten",
    "stats.noServer": "Noch keine Messung — der Server antwortet nicht auf /metrics (Metal) oder /api/status (MLX). In Chat starten; die Werte unten bleiben, wie sie waren.",
    "scope.sessionNote": "seit dem Serverstart (oder deinem letzten Zurücksetzen)",
    "scope.alltimeNote": "über alle Läufe, auf der Platte gehalten",
    "stats.requestNote": "abgeschlossene Antworten",
    "stats.noExperts": "– (kein Streaming-Lauf)",
    "chat.metaLive": "live",
    "chat.metaLast": "zuletzt",
    "obs.tps": "tok/s", "obs.cache": "cache", "obs.rss": "RSS", "obs.cfg": "cfg",
    "obs.downTip": "Server aus — tok/s, Cache-Hit und RSS zeigen – bis Start",
    "obs.upTip": "Letztes Decode tok/s · Cache-Hit (Experte oder KV) · Prozess-RSS (oder Modell-RAM)",
    "lbl.backend": "Backend", "backend.metal": "llama.cpp · Metal + PGRN (SSD-Streaming)",
    "backend.mlx": "MLX + SSD PGRN (wenn Sidecar gebündelt)",
    "backend.auto": "Auto (hybrid)",
    "tip.backend": "Metal + PGRN Default. MLX streamt Experten von SSD wenn Sidecar + experts.pgrn. Auto (hybrid): kurz/warm → MLX wenn experts.pgrn; langer Prefill (≥8k Zeichen) → Metal. Explizites metal/mlx wird nie überschrieben. Metal-Klasse ~15 tok/s warm auf interner NVMe (gemessen).",
    "hint.backend": "Auto: kurz/warm → MLX wenn experts.pgrn, langer Prefill → Metal. Explizite Wahl bleibt sticky.",
    "lbl.mlxDir": "MLX-Verzeichnis (Override)",
    "hint.mlxDir": "Nur der Katalog-Ordner — z. B. ~/Modelle/mlx — nicht der Qwen…-4bit-Unterordner. Standard ist <Modellordner>/mlx. SSD-Streaming braucht experts.pgrn neben jedem Modell.",
    "err.noMlxModel": "Dieser Katalogeintrag hat noch kein MLX-Zwilling — Qwen3.6-35B wählen oder zurück auf Metal.",
    "err.mlxDirLeaf": "MLX-Verzeichnis muss der Katalog-Parent sein (…/mlx), nicht der Modellordner. Unter Erweitert → MLX-Verzeichnis auf ~/Modelle/mlx setzen — oder Backend auf Metal.",
    "mlx.cap.missingExperts": "experts.pgrn fehlt — Start ist nur resident (kein SSD-Experten-Streaming).",
    "mlx.cap.noRuntime": "MLX-Runtime fehlt — einmaliger Wheel-Download (~0,5–1 GiB).",
    "mlx.cap.noOmlx": "MLX-Runtime fehlt — einmaliger Wheel-Download (~0,5–1 GiB).",
    "mlx.cap.noLauncher": "PGRN-Launcher fehlt in diesem App-Build — nur resident oMLX.",
    "btn.mlxRuntime": "MLX-Runtime installieren",
    "btn.mlxRuntimeBusy": "MLX-Runtime wird installiert…",
    "toast.mlxRuntimeStarted": "MLX-Runtime-Installation gestartet",
    "toast.mlxRuntimeReady": "MLX-Runtime bereit",
    "host.ecores": "E-Kerne", "host.pcores": "P-Kerne", "host.gpu": "Auslastung", "host.gpuMem": "GPU-Speicher",
    "host.memory": "Speicher", "host.wired": "Wired", "host.active": "Aktiv", "host.compressed": "Komprimiert", "host.free": "Frei",
    "host.section": "Host", "host.thermal": "Thermik", "host.load": "Last", "host.uptime": "Laufzeit",
    // Wortgleich mit der Menüleiste, die dieselbe Laufzeit in Rust formatiert.
    "unit.days": "T", "unit.hours": "Std", "unit.minutes": "Min",
    "thermal.Nominal": "Nominal", "thermal.Fair": "Erhöht", "thermal.Serious": "Ernst", "thermal.Critical": "Kritisch",
    "status.note": "<b>Woher die Zahlen kommen:</b> Serving-Zähler aus Metal <code>/metrics</code> oder MLX <code>/api/status</code>, alle drei Sekunden — dieselbe Quelle wie die Menüleiste. <b>Letztes Decode</b> aus der Engine-Log-eval-Zeile (Metal) oder <code>avg_generation_tps</code> (MLX). <b>Sitzung</b> beginnt mit dem Server neu; <b>Gesamt</b> liegt auf der Platte. Prefill zählt nur gerechnete Tokens. <b>Experten-Trefferquote</b> aus dem Metal-PGRN-Log oder dem MLX-<code>pgrn</code>-Block. RSS / High-Water nur wenn oMLX sie liefert.",
  },
  zh: {}, es: {},
};
Object.keys(I18N).forEach((l) => Object.assign(I18N[l], I18N_EXT[l] || {}));

// Hover tooltips (EN + DE; zh/es fall back to EN).
const TIPS = {
  en: {
    "tip.usability": "Green = Mac stays smooth. Yellow/red = memory pressure / swapping — shrink the cache.",
    "tip.liveMonitor": "Real values from the streaming kernel log — not an estimate.",
    "tip.cache": "How many experts stay resident in RAM. Bigger = faster (more hits) but needs more free RAM. Measured Metal peak: 14 GiB + io=4 (~18–19 tok/s) when free ≥22 GiB preferred (admit ≥21.5); safe warm: 10 GiB + io=4.",
    "tip.ctx": "Window for prompt + history. Coding agents send ~30k+ tokens, so 40k is the best compromise. 32k can overflow; 64k causes memory pressure.",
    "tip.io": "Parallel SSD read threads for fetching experts. Default 4 = 2×-qualified Metal balanced recipe. 1 = serial path; 8/16 = optional wider fetch (not requalified as product default).",
    "tip.mirror": "Stripe expert reads across two disks, split by measured bandwidth. Wins only with two comparably-fast, independent SSDs; a slow USB drive makes it slower. Parity is CRC-checked.",
    "tip.predict": "Learns which experts co-fire between layers and warms the next layer's likely experts (cache only — parity-safe). Metal: PGRN_ONLINE_PREDICT. MLX/oMLX: SLIPSTREAM_PGRN_ONLINE. Default off; experimental — measure A/B before leaving on.",
    "tip.mtp": "Speculative decoding via multi-token prediction. Only for models with an MTP/DFlash draft; off otherwise.",
    "tip.compact": "Zero-copy expert slots: the GPU reads experts straight from the cache — no re-upload copy. Measured +13–24% decode at moderate cache, neutral at high, swap-safe. On by default.",
    "tip.extBase": "Where the large GGUF files live, if that is a different disk than the streamed PGRN. The PGRN belongs on the fastest disk because it is read continuously; a GGUF is read once at load, so it can sit on a slower external drive (measured 2.7× on Laguna). Empty means both live in the model folder.",
    "tip.grammar": "For structured output (JSON / tool calls): where the grammar forces the next character, inject it as a pre-accepted, target-verified draft — fewer forwards, fewer SSD expert fetches. Lossless (output byte-identical). Adaptive-guarded: only engages when it beats the draft model, so easy JSON never regresses. Measured +45% tok/s on rigid schemas (fetch-bound), neutral on simple JSON. On by default.",
    "tip.thinking": "The model's reasoning mode. Leave OFF for agentic coding: otherwise it loops in endless thinking and burns the token budget before answering. ON only for hard single questions.",
    "tip.model": "Compatible = MoE architecture + Q4_K/Q5_K/Q6_K experts + known to llama.cpp.",
    "tip.gguf": "The GGUF is read only at load — it may live on the slow external SSD.",
    "tip.pgrn": "The PGRN is streamed continuously during every answer — put it on the FASTEST SSD (internal NVMe). Measured: 2.7x faster than external.",
    "tip.indexing": "Semantic code search: instead of stuffing everything into the prompt, your agent fetches only relevant code. Needs an embedding model (runs on the GPU) + a vector DB (Qdrant) — both startable here.",
    "tip.embedder": "Our llama-server in --embedding mode with Nomic-Embed (~100 MB). Turns code into vectors.",
    "tip.qdrant": "Local vector database (release binary, ~30 MB). Stores your code's index.",
    "tip.test": "Send a prompt to the model and check answer + speed.",
    "tip.serving": "The running server's own counters, polled every three seconds — the same reading the menubar shows.",
    "tip.efficiency": "Share of the submitted prompt that came from the KV cache instead of being computed. High is good: in an agent session the history repeats, so a warm cache saves the whole prefill.",
    "tip.prefillRate": "Tokens processed per second, counting only the tokens that were actually computed — cache hits cost no prefill time and must not inflate the rate.",
    "tip.lastTps": "Last completed decode: Metal/oMLX from the engine log (eval-time or output=N tok/s); MLX also falls back to /api/status avg_generation_tps. Chat shows a live wall-clock rate while streaming.",
    "tip.rss": "Process RSS: oMLX process_rss_bytes, or ps on the child Slipstream started (Metal). Falls back to model_memory_used from /api/status. Shows – when the server is down or neither source answers.",
    "tip.pgrnHw": "PGRN arena high-water bytes from oMLX store.cache_stats when experts.pgrn is attached. Metal expert hit-rate still comes from the engine log.",
    "tip.thermal": "Coarse thermal pressure from macOS. Nominal = no throttling. Die temperature would need private frameworks, so this level stands in for it.",
  },
  de: {
    "tip.usability": "Grün = Mac bleibt flüssig. Gelb/Rot = Speicherdruck / Swapping — Cache verkleinern.",
    "tip.liveMonitor": "Echte Werte aus dem Streaming-Kernel-Log — keine Schätzung.",
    "tip.cache": "Wie viele Experten resident im RAM bleiben. Größer = schneller (mehr Treffer), braucht aber mehr freien RAM. Gemessenes Metal-Peak: 14 GiB + io=4 (~18–19 tok/s) bei ≥22 GiB frei bevorzugt (Zulassung ≥21.5); sicheres Warm: 10 GiB + io=4.",
    "tip.ctx": "Fenster für Prompt + Verlauf. Coding-Agenten schicken ~30k+ Tokens, daher ist 40k der beste Kompromiss. 32k kann überlaufen; 64k erzeugt Speicherdruck.",
    "tip.io": "Parallele SSD-Lesethreads beim Experten-Holen. Standard 4 = 2×-qualifiziertes Metal-balanced-Rezept. 1 = serieller Pfad; 8/16 = optional breiter (nicht als Produktdefault requalifiziert).",
    "tip.mirror": "Verteilt Experten-Reads bandbreiten-proportional über zwei Disks. Gewinnt nur mit zwei ähnlich schnellen, unabhängigen SSDs; eine langsame USB-Disk macht es langsamer. Parity ist CRC-geprüft.",
    "tip.predict": "Lernt, welche Experten zwischen Layern gemeinsam feuern, und wärmt die nächsten wahrscheinlichen Experten (nur Cache — parity-safe). Metal: PGRN_ONLINE_PREDICT. MLX/oMLX: SLIPSTREAM_PGRN_ONLINE. Standard aus; experimentell — vor Dauerbetrieb A/B messen.",
    "tip.mtp": "Spekulatives Decoding via Multi-Token-Prediction. Nur bei Modellen mit MTP/DFlash-Draft; sonst automatisch aus.",
    "tip.compact": "Zero-Copy-Experten-Slots: die GPU liest Experten direkt aus dem Cache — keine Re-Upload-Kopie. Gemessen +13–24% Decode bei moderatem Cache, neutral bei hohem, swap-safe. Standardmäßig an.",
    "tip.extBase": "Wo die großen GGUF-Dateien liegen, falls das eine andere Platte ist als die gestreamte PGRN. Die PGRN gehört auf die schnellste Platte, weil sie laufend gelesen wird; eine GGUF wird nur einmal beim Laden gelesen und darf auf einer langsameren externen liegen (gemessen 2,7× bei Laguna). Leer heißt: beides im Modellordner.",
    "tip.grammar": "Für strukturierte Ausgabe (JSON / Tool-Calls): wo die Grammatik das nächste Zeichen erzwingt, wird es als vorab-akzeptierter, vom Target verifizierter Draft injiziert — weniger Forwards, weniger SSD-Experten-Fetches. Verlustfrei (Output byte-identisch). Adaptiv abgesichert: greift nur, wenn es das Draft-Model schlägt, einfaches JSON regressiert also nie. Gemessen +45% tok/s bei rigiden Schemas (fetch-bound), neutral bei einfachem JSON. Standardmäßig an.",
    "tip.thinking": "Reasoning-Modus des Modells. Für Agenten-Coding AUS lassen: sonst verheddert es sich im Endlos-Denken und verbraucht das Token-Budget, bevor es antwortet. AN nur für schwere Einzelfragen.",
    "tip.model": "Kompatibel = MoE-Architektur + Q4_K/Q5_K/Q6_K-Experten + von llama.cpp unterstützt.",
    "tip.gguf": "Das GGUF wird nur beim Laden gelesen — darf auf der langsamen externen SSD liegen.",
    "tip.pgrn": "Die PGRN wird während jeder Antwort ständig gestreamt — auf die SCHNELLSTE SSD legen (interne NVMe). Gemessen: 2,7x schneller als extern.",
    "tip.indexing": "Semantische Codesuche: statt alles in den Prompt zu stopfen, holt der Agent nur relevante Code-Stellen. Braucht ein Embedding-Modell (läuft auf der GPU) + eine Vektor-DB (Qdrant) — beide hier startbar.",
    "tip.embedder": "Unser llama-server im --embedding-Modus mit Nomic-Embed (~100 MB). Wandelt Code in Vektoren.",
    "tip.qdrant": "Lokale Vektor-Datenbank (Release-Binary, ~30 MB). Speichert den Index deines Codes.",
    "tip.test": "Prompt an das Modell schicken und Antwort + Speed prüfen.",
    "tip.serving": "Die eigenen Zähler des laufenden Servers, alle drei Sekunden abgefragt — dieselbe Lesung, die auch die Menüleiste zeigt.",
    "tip.efficiency": "Anteil des eingereichten Prompts, der aus dem KV-Cache kam statt gerechnet zu werden. Hoch ist gut: in einer Agenten-Sitzung wiederholt sich der Verlauf, ein warmer Cache spart dann den ganzen Prefill.",
    "tip.prefillRate": "Verarbeitete Tokens pro Sekunde, gezählt werden nur die tatsächlich gerechneten — Cache-Treffer kosten keine Prefill-Zeit und dürfen die Rate nicht aufblähen.",
    "tip.lastTps": "Letztes abgeschlossenes Decode: Metal/oMLX aus dem Engine-Log (eval-time oder output=N tok/s); MLX fällt auch auf /api/status avg_generation_tps zurück. Chat zeigt eine Live-Wanduhr-Rate während des Streamings.",
    "tip.rss": "Prozess-RSS: oMLX process_rss_bytes, oder ps auf dem von Slipstream gestarteten Kind (Metal). Fallback: model_memory_used aus /api/status. Zeigt – wenn der Server aus ist oder keine Quelle antwortet.",
    "tip.pgrnHw": "PGRN-Arena-High-Water in Bytes aus oMLX store.cache_stats wenn experts.pgrn angehängt ist. Metal-Experten-Hit-Rate kommt weiter aus dem Engine-Log.",
    "tip.thermal": "Grober thermischer Druck von macOS. Nominal = keine Drosselung. Die Chip-Temperatur bräuchte private Frameworks, dieser Wert steht dafür ein.",
  },
};
Object.keys(TIPS).forEach((l) => Object.assign(I18N[l], TIPS[l]));

// Toasts, statuses, setup steps (EN + DE; zh/es fall back to EN).
const MISC = {
  en: {
    "toast.canceled": "Canceled", "toast.pickCanceled": "Selection canceled", "toast.noDialog": "File dialog unavailable",
    "toast.diagCopied": "Diagnostics copied", "toast.dlStarted": "Download started", "toast.convStarted": "Conversion started",
    "toast.convDone": "Conversion finished — PGRN ready",
    "toast.idxStopped": "Indexing stopped", "toast.serverStopped": "Server stopped",
    "toast.noUrl": "No URL for this model — place it manually.", "toast.embDl": "Downloading embed model (~100 MB)",
    "toast.qInstall": "Installing Qdrant (~30 MB)", "toast.embStarted": "Embedder started", "toast.embStopped": "Embedder stopped",
    "toast.qStarted": "Qdrant started", "toast.qStopped": "Qdrant stopped", "toast.copied": "Copied", "badge.loading": "loading", "idx.running": "Running…",
    "err.prefix": "Error: ", "btn.notReady": "Model not ready", "srv.startedLoading": "Server started — loading model (~60s)…",
    "idx.step1": "1/4 Downloading embed model (~100 MB)…", "idx.step2": "2/4 Installing Qdrant (~30 MB)…",
    "idx.step3emb": "3/4 Starting embedder…", "idx.step3q": "3/4 Starting Qdrant…", "idx.step4": "4/4 Waiting for both to be ready…",
    "idx.dlTimeout": "Download timeout (embed model)", "idx.installTimeout": "Install timeout (Qdrant)",
    "st.notConverted": "GGUF present — not converted yet.",
    "st.embRunning": "running — ready", "st.embStart": "starting…", "st.embDl2": "downloading…", "st.embLoaded": "loaded — ready", "st.embNone": "not downloaded (~100 MB)",
    "st.qRunning": "running — port 6333", "st.qInstalling": "installing… (~30 MB)", "st.qInstalled": "installed — ready", "st.qNone": "not installed",
    "test.fail": "Error: could not reach the server. Is it running? (Start above)",
    "how.cline": "In VS Code: Cline -> Settings -> API Provider = \"OpenAI Compatible\", enter the values. Raise read timeout for cold prefills.",
    "how.roo": "In VS Code: Roo Code -> Settings -> API Provider = \"OpenAI Compatible\", enter the values. Raise read timeout for cold prefills.",
    "how.cursor": "In Cursor: Settings -> Models -> set OpenAI API Key + \"Override OpenAI Base URL\" = the Base URL. Model id from Chat / GET /v1/models.",
    "how.codex": "Codex / Responses-native clients: use the Responses URL (not chat-only). Server: POST /v1/responses on the same host. Chat agents keep using /v1.",
    "how.anthropic": "Anthropic-compatible clients: Messages URL is POST /v1/messages on the same host (oMLX). Chat agents can stay on OpenAI /v1.",
    "how.continue": "Paste into ~/.continue/config.yaml under models:. requestOptions.timeoutMs helps cold prefills.",
    "how.aider": "Paste into ~/.aider.conf.yml (or as CLI flags). Raise timeouts for cold PGRN prefills.",
  },
  de: {
    "toast.canceled": "Abgebrochen", "toast.pickCanceled": "Auswahl abgebrochen", "toast.noDialog": "Datei-Dialog nicht verfügbar",
    "toast.diagCopied": "Diagnose kopiert", "toast.dlStarted": "Download gestartet", "toast.convStarted": "Konvertierung gestartet",
    "toast.convDone": "Konvertierung fertig — PGRN bereit",
    "toast.idxStopped": "Indexierung gestoppt", "toast.serverStopped": "Server gestoppt",
    "toast.noUrl": "Für dieses Modell keine URL — manuell ablegen.", "toast.embDl": "Embed-Modell wird geladen (~100 MB)",
    "toast.qInstall": "Qdrant wird installiert (~30 MB)", "toast.embStarted": "Embedder gestartet", "toast.embStopped": "Embedder gestoppt",
    "toast.qStarted": "Qdrant gestartet", "toast.qStopped": "Qdrant gestoppt", "toast.copied": "Kopiert", "badge.loading": "lädt", "idx.running": "Läuft…",
    "err.prefix": "Fehler: ", "btn.notReady": "Modell nicht bereit", "srv.startedLoading": "Server gestartet — lädt Modell (~60s)…",
    "idx.step1": "1/4 Embed-Modell wird geladen (~100 MB)…", "idx.step2": "2/4 Qdrant wird installiert (~30 MB)…",
    "idx.step3emb": "3/4 Embedder startet…", "idx.step3q": "3/4 Qdrant startet…", "idx.step4": "4/4 warte bis beide bereit…",
    "idx.dlTimeout": "Download-Timeout (Embed-Modell)", "idx.installTimeout": "Install-Timeout (Qdrant)",
    "st.notConverted": "GGUF vorhanden — noch nicht konvertiert.",
    "st.embRunning": "läuft — bereit", "st.embStart": "startet…", "st.embDl2": "lädt…", "st.embLoaded": "geladen — bereit", "st.embNone": "nicht heruntergeladen (~100 MB)",
    "st.qRunning": "läuft — Port 6333", "st.qInstalling": "installiert… (~30 MB)", "st.qInstalled": "installiert — bereit", "st.qNone": "nicht installiert",
    "test.fail": "Fehler: Server nicht erreichbar. Läuft er? (Start oben)",
    "how.cline": "In VS Code: Cline -> Settings -> API Provider = \"OpenAI Compatible\", Werte einfügen. Read-Timeout für kalte Prefills erhöhen.",
    "how.roo": "In VS Code: Roo Code -> Settings -> API Provider = \"OpenAI Compatible\", Werte einfügen. Read-Timeout für kalte Prefills erhöhen.",
    "how.cursor": "In Cursor: Settings -> Models -> OpenAI API Key setzen + \"Override OpenAI Base URL\" = die Base URL. Modell-ID aus Chat / GET /v1/models.",
    "how.codex": "Codex / Responses-Clients: Responses-URL nutzen (nicht nur Chat). Server: POST /v1/responses auf demselben Host. Chat-Agents bleiben bei /v1.",
    "how.anthropic": "Anthropic-kompatible Clients: Messages-URL ist POST /v1/messages auf demselben Host (oMLX). Chat-Agents können bei OpenAI /v1 bleiben.",
    "how.continue": "In ~/.continue/config.yaml unter models: einfügen. requestOptions.timeoutMs hilft bei kalten Prefills.",
    "how.aider": "In ~/.aider.conf.yml einfügen (oder als CLI-Flags). Timeouts für kalte PGRN-Prefills erhöhen.",
  },
};
Object.keys(MISC).forEach((l) => Object.assign(I18N[l], MISC[l]));

let LANG = localStorage.getItem("slipstream.lang") ||
  (navigator.language || "en").slice(0, 2);
if (!I18N[LANG]) LANG = "en";
function t(key) { return (I18N[LANG] && I18N[LANG][key]) || I18N.en[key] || key; }

// Offering a language the app only half speaks is the same dishonesty as a fake
// panel, so the selector says how far each one gets. Measured, not asserted: a
// language that gets completed loses its marker without anyone editing this.
function langCoverage(lang) {
  const total = Object.keys(I18N.en).length;
  if (!total || !I18N[lang]) return 0;
  return Object.keys(I18N[lang]).length / total;
}
function markLangCoverage() {
  const sel = $("lang");
  if (!sel) return;
  for (const opt of sel.options) {
    const pct = Math.round(langCoverage(opt.value) * 100);
    const base = opt.dataset.label || (opt.dataset.label = opt.textContent);
    opt.textContent = pct >= 95 ? base : `${base} · ${pct}%`;
    opt.title = pct >= 95 ? "" : t("lang.partial").replace("{pct}", String(pct));
  }
}
function applyLang(lang) {
  if (I18N[lang]) LANG = lang;
  localStorage.setItem("slipstream.lang", LANG);
  document.documentElement.lang = LANG;
  document.querySelectorAll("[data-i18n]").forEach((el) => { el.textContent = t(el.dataset.i18n); });
  document.querySelectorAll("[data-i18n-tip]").forEach((el) => { el.setAttribute("data-tip", t(el.dataset.i18nTip)); });
  document.querySelectorAll("[data-i18n-html]").forEach((el) => { el.innerHTML = t(el.dataset.i18nHtml); });
  document.querySelectorAll("[data-i18n-ph]").forEach((el) => { el.setAttribute("placeholder", t(el.dataset.i18nPh)); });
  document.querySelectorAll("[data-i18n-aria]").forEach((el) => { el.setAttribute("aria-label", t(el.dataset.i18nAria)); });
  const ls = $("lang"); if (ls) ls.value = LANG;
  try { markLangCoverage(); } catch {}
  try { renderModelNote(); } catch {}
  try { updateReco(); } catch {}
  try { renderAgents(); } catch {}
  try { updateCacheRec(); } catch {}
  try { renderSpeed(); } catch {}
  try { renderInstalled(); } catch {}
  try { renderMemPlan(); } catch {}
  try { renderPartitionNote(); } catch {}
  try { if (state.mstatus) renderResume(state.mstatus); } catch {}
  try { renderQualitySelector(); } catch {}
  try { updatePgrnProfileHint(); } catch {}
  try { fillModels(); } catch {}   // options carry a translated "soon" suffix
  // The pill and the power button carry live text set by setPill() — repaint them
  // in whatever mode we're currently in, otherwise they keep the old language.
  try { setPill(state.pillMode || "off"); } catch {}
  try { refreshP2pStatus(); } catch {}
  // Same for the indexing button, whose label depends on the live setup state.
  const idxBtn = $("idxSetup");
  if (idxBtn) idxBtn.textContent = t(idxBtn.dataset.mode === "stop" ? "idx.stop" : "idx.setup");
  if (state.bench) { try { $("benchOut").innerHTML = renderBenchTable(state.bench, state.bench.length) + `<div class="bench-note">${t("bench.note")}</div>`; } catch {} }
}

// ---- tiny helpers ----------------------------------------------------------
function toast(msg, err) {
  const t = $("toast");
  t.textContent = msg;
  t.className = "toast show" + (err ? " err" : "");
  clearTimeout(t._h);
  t._h = setTimeout(() => (t.className = "toast"), 2600);
}
// ---- number formatting -----------------------------------------------------
// One decimal separator for the whole window. The menubar writes its figures
// through Rust's German formatter ("4,25", "26,8 GiB"), so anything here that
// spelled them "4.25" would put one reading in two spellings — sometimes two
// lines apart, as the memory plan and the memory bar were.
const dec = (value, digits) => value.toLocaleString(LANG, {
  minimumFractionDigits: digits, maximumFractionDigits: digits,
});
const tps = (rate) => `${dec(rate, 1)} tok/s`;
const giB = (bytes) => `${dec(bytes / 1073741824, 1)} GiB`;
const fmtGiB = (b) => dec(b / 1073741824, 1);
function fmtEta(s) {
  s = Math.round(s);
  return s >= 60 ? `${Math.floor(s / 60)}m ${s % 60}s` : `${s}s`;
}
function pgrnOf(gguf) { return gguf.replace(/\.gguf$/i, ".pgrn"); }
function modelDir() { return $("pDir").value.trim(); }
function ggufPath() { return modelDir() + "/" + currentFile(); }
function urlFor(m) {
  if (!m.repo) return "";
  return `https://huggingface.co/${m.repo}/resolve/main/${m.file}`;
}

// ---- tabs ------------------------------------------------------------------
function showTab(name) {
  const btn = document.querySelector(`.tab[data-tab="${name}"]`);
  if (!btn || btn.disabled) return false;
  // aria-selected must follow the visual state, or the tab strip reads as eight
  // plain buttons with no current item.
  document.querySelectorAll(".tab").forEach((t) => {
    t.classList.remove("tab-active");
    t.setAttribute("aria-selected", "false");
  });
  btn.classList.add("tab-active");
  btn.setAttribute("aria-selected", "true");
  document.querySelectorAll(".panel").forEach((p) => (p.hidden = p.dataset.panel !== name));
  localStorage.setItem("slipstream.tab", name);
  // Canvases have no width while their panel is hidden — repaint on reveal.
  try { drawAll(); } catch {}
  try { sizeChat(); } catch {}
  // Skipped while hidden, so it would otherwise show stale values for a tick.
  try { refreshStatus(); } catch {}
  if (name === "cluster") {
    try { refreshP2pStatus(); } catch {}
    try { refreshClusterMlxCap(); } catch {}
  }
  return true;
}
document.querySelectorAll(".tab[data-tab]").forEach((tab) => {
  tab.onclick = () => showTab(tab.dataset.tab);
});
// Memory, Experts and Streaming were folded into the live view. Whoever left the
// app on one of them should land where that content went, not back on Chat.
const ABSORBED = { memory: "status", experts: "status", streaming: "status" };
const lastTab = localStorage.getItem("slipstream.tab") || "chat";
showTab(ABSORBED[lastTab] || lastTab) || showTab("chat");

// ---- info tooltips ---------------------------------------------------------
const tip = $("tooltip");
document.addEventListener("mouseover", (e) => {
  if (!tip) return;
  const el = e.target.closest("[data-tip]");
  if (!el) return;
  tip.textContent = el.dataset.tip;
  tip.classList.add("show");
  const r = el.getBoundingClientRect();
  tip.style.left = "0px"; tip.style.top = "0px";
  const tw = tip.offsetWidth;
  let x = Math.min(r.left, window.innerWidth - tw - 12);
  tip.style.left = Math.max(8, x) + "px";
  tip.style.top = (r.bottom + 8) + "px";
});
document.addEventListener("mouseout", (e) => {
  if (!tip) return;
  if (e.target.closest("[data-tip]")) tip.classList.remove("show");
});

// ---- copy buttons ----------------------------------------------------------
document.querySelectorAll(".copy").forEach((b) => {
  b.onclick = async () => {
    const txt = b.dataset.copyText || $(b.dataset.copy).textContent;
    try { await navigator.clipboard.writeText(txt); toast(t("toast.copied") + ": " + txt); }
    catch { toast(t("toast.pickCanceled"), true); }
  };
});

// ---- model dropdown --------------------------------------------------------
function fillModels() {
  const sel = $("modelSel");
  sel.innerHTML = "";
  const recGrp = document.createElement("optgroup");
  recGrp.label = t("models.group.rec");
  const xlGrp = document.createElement("optgroup");
  xlGrp.label = t("models.group.xl");
  MODELS.forEach((m) => {
    const o = document.createElement("option");
    o.value = m.id; o.textContent = m.soon ? `${m.name} — ${t("badge.soon")}` : m.name;
    if (m.soon) o.disabled = true;
    (m.xl ? xlGrp : recGrp).appendChild(o);
  });
  sel.appendChild(recGrp);
  if (xlGrp.children.length) sel.appendChild(xlGrp);
  sel.value = state.model.id;
}
function selectModel(id) {
  state.model = MODELS.find((m) => m.id === id) || defaultModel();
  state.quantIdx = 0;
  renderQualitySelector();
  applyModelPaths();
  localStorage.setItem("pgrn.model", id);
  applyBackendUi();
}
// The streamed file depends on the chosen quant; keep paths/url/note in sync.
function applyModelPaths() {
  const m = state.model;
  const base = defaultModelsRoot() || (state.def ? state.def.model_dir : "/Users/Modelle");
  const file = currentFile();
  const sub = currentSubdir();
  $("pDir").value = `${m.extGguf ? ggufBase() : base}/${sub}`;
  $("pPgrn").value = `${base}/${sub}/${file.replace(/\.gguf$/i, ".pgrn")}`;
  $("pUrl").value = m.repo ? `https://huggingface.co/${m.repo}/resolve/main/${file}` : "";
  renderModelNote();
  state.remoteBytes = 0;
  refreshModel();
}
function currentFile() {
  const m = state.model;
  if (m.quants && m.quants[state.quantIdx || 0]) return m.quants[state.quantIdx || 0].file;
  return m.file;
}
function currentQuant() {
  const m = state.model;
  if (m.quants && m.quants[state.quantIdx || 0]) return m.quants[state.quantIdx || 0];
  return null;
}
function currentSubdir() {
  const q = currentQuant();
  return (q && q.subdir) || state.model.subdir;
}
/** Effective speculative decode: per-quant override (UD-Q5/Q6 = none). */
function effectiveSpec() {
  const q = currentQuant();
  if (q && Object.prototype.hasOwnProperty.call(q, "spec")) return q.spec || "none";
  return state.model.spec || "none";
}
function effectiveMtp() {
  const q = currentQuant();
  if (q && Object.prototype.hasOwnProperty.call(q, "mtp")) return !!q.mtp;
  return !!state.model.mtp;
}

function renderQualitySelector() {
  const wrap = $("qualityWrap"), sel = $("qualitySel");
  const m = state.model;
  if (!wrap || !sel) return;
  if (!m.quants || m.quants.length < 2) { wrap.style.display = "none"; return; }
  wrap.style.display = "";
  sel.innerHTML = "";
  m.quants.forEach((q, i) => {
    const o = document.createElement("option");
    o.value = String(i); o.textContent = `${q.label} · ${t(q.tier)} (~${q.gb} GiB)`;
    sel.appendChild(o);
  });
  sel.value = String(state.quantIdx || 0);
}
function renderModelNote() {
  try { renderSpeed(); } catch {}
  const m = state.model, el = $("modelNote");
  if (!el) return;
  const gb = (m.quants && m.quants[state.quantIdx || 0]) ? m.quants[state.quantIdx || 0].gb : m.gb;
  const canMtp = effectiveMtp();
  el.innerHTML = `${t(m.note)} &nbsp;&middot;&nbsp; ~${gb} GiB Download` +
    (canMtp ? " &nbsp;&middot;&nbsp; MTP/DFlash-Speed" : "");
  // The Downloads tab acts on this selection — name it there too.
  const dn = $("dlModelName");
  if (dn) dn.textContent = m.quants ? `${m.name} · ${m.quants[state.quantIdx || 0].label}` : m.name;
  // Models/quants without an MTP head (e.g. UD-Q5_K_XL) can't speculate — don't offer it.
  const sp = $("specMtp");
  if (sp) {
    const canSpec = effectiveSpec() !== "none";
    sp.disabled = !canSpec;
    if (!canSpec) sp.checked = false;
    if (sp.parentElement) sp.parentElement.style.opacity = canSpec ? "" : ".45";
  }
}
$("modelSel").onchange = (e) => selectModel(e.target.value);
if ($("qualitySel")) $("qualitySel").onchange = (e) => { state.quantIdx = +e.target.value || 0; applyModelPaths(); };

// ---- file pickers ----------------------------------------------------------
async function pickInto(inputId, opts) {
  if (!dialog || !dialog.open) { toast(t("toast.noDialog"), true); return; }
  try {
    const cur = $(inputId).value.trim();
    let prefer = "";
    if (inputId === "pMlx") prefer = state.preferMlxDir || preferredMlxModelDir();
    else if (inputId === "pModelsRoot" || inputId === "pDir" || inputId === "extBase") {
      prefer = state.preferMetalDir || preferredMetalModelDir();
    }
    const picked = await dialog.open(Object.assign({
      defaultPath: cur || prefer || undefined,
    }, opts || {}));
    if (picked) {
      $(inputId).value = picked;
      // A programmatic assignment fires nothing, so inputs that persist their
      // value would silently miss a pick.
      $(inputId).dispatchEvent(new Event("change", { bubbles: true }));
    }
  } catch (e) { toast(t("toast.pickCanceled"), true); }
}
function addPicker(inputId, opts, key) {
  const inp = $(inputId);
  const btn = document.createElement("button");
  // data-i18n, not a baked-in label: applyLang() has to be able to re-label it.
  btn.className = "btn btn-ghost"; btn.dataset.i18n = key; btn.textContent = t(key);
  btn.style.marginTop = "6px";
  btn.onclick = () => pickInto(inputId, opts);
  inp.insertAdjacentElement("afterend", btn);
}

// ---- model status / download / convert -------------------------------------
/// Interrupted conversion (R1.5). The strip only exists while there is work to
/// pick up, and it hides while a conversion is running so the two progress bars
/// can never contradict each other.
function renderResume(st) {
  const row = $("resumeRow");
  if (!row) return;
  const r = st.resume || {};
  const show = !st.converting && !st.downloading && (r.resumable || r.orphan_partial);
  row.hidden = !show;
  if (!show) return;
  const pct = r.records_total ? Math.min(100, (r.records_done / r.records_total) * 100) : 0;
  $("resumeBar").style.width = pct.toFixed(0) + "%";
  $("resumeBtn").disabled = !r.resumable;
  $("resumeNote").textContent = r.resumable
    ? `${pct.toFixed(0)} % · ${r.records_done.toLocaleString(LANG)} ${t("resume.of")} `
      + `${r.records_total.toLocaleString(LANG)} ${t("resume.experts")} · ${fmtGiB(r.partial_bytes)} GiB`
    : t("resume.orphan");
}

async function refreshModel() {
  const gguf = ggufPath(), pgrn = ($("pPgrn").value.trim() || pgrnOf(gguf)), dir = modelDir();
  let st;
  try { st = await invoke("model_status", { gguf, pgrn, dir }); }
  catch { return; }
  state.mstatus = st;
  renderPartitionNote();
  renderResume(st);

  // Conversion just ended: surface the converter's final JSONL verdict once.
  if (state.wasConverting && !st.converting) {
    state.wasConverting = false;
    try {
      const cp = await invoke("convert_progress");
      if (cp.phase === "error") toast(t("err.prefix") + cp.message, true);
      else if (cp.phase === "done") toast(t("toast.convDone"));
      else if (cp.phase === "cancelled") toast(t("resume.title"));
    } catch {}
  }

  const badge = $("modelBadge");
  const dlStatus = $("dlStatus"), dlBar = $("dlBar"), dlNote = $("dlNote");

  if (st.downloading) {
    if (!state.remoteBytes) {
      try { state.remoteBytes = await invoke("remote_size", { url: $("pUrl").value.trim() }); } catch {}
    }
    const pct = state.remoteBytes ? Math.min(100, (st.gguf_bytes / state.remoteBytes) * 100) : 0;
    badge.className = "badge partial"; badge.textContent = t("badge.loading") + " " + pct.toFixed(0) + "%";
    dlStatus.textContent = t("st.dlRunning");
    dlBar.style.width = pct + "%";
    dlNote.textContent = `${fmtGiB(st.gguf_bytes)} / ${state.remoteBytes ? fmtGiB(state.remoteBytes) : "?"} GiB · ${t("reco.free")}: ${st.disk_free_gib.toFixed(0)} GiB`;
  } else if (st.converting) {
    state.wasConverting = true;
    let cp = null;
    try { cp = await invoke("convert_progress"); } catch {}
    if (cp && cp.total_bytes > 0 && cp.phase && cp.phase !== "error") {
      // Real progress from the native converter's JSONL protocol.
      const pct = Math.min(100, (cp.done_bytes / cp.total_bytes) * 100);
      const phase = t("conv." + cp.phase);
      badge.className = "badge partial"; badge.textContent = phase + " " + pct.toFixed(0) + "%";
      dlStatus.textContent = t("st.convRunning") + " — " + phase;
      dlBar.style.width = pct + "%";
      const speed = cp.mb_s > 0 ? ` · ${Math.round(cp.mb_s)} MB/s` : "";
      const eta = cp.eta_s >= 1 ? ` · ETA ${fmtEta(cp.eta_s)}` : "";
      dlNote.textContent = `${fmtGiB(cp.done_bytes)} / ${fmtGiB(cp.total_bytes)} GiB${speed}${eta}`;
    } else {
      badge.className = "badge partial"; badge.textContent = t("st.convRunning");
      dlStatus.textContent = t("st.convRunning");
      dlBar.style.width = "2%";
      dlNote.textContent = `PGRN: ${fmtGiB(st.pgrn_bytes)} GiB`;
    }
  } else if (st.pgrn_bytes > 0 && st.gguf_bytes > 0) {
    badge.className = "badge ready"; badge.textContent = t("badge.ready");
    dlStatus.textContent = t("st.ready");
    dlBar.style.width = "100%";
    dlNote.textContent = `GGUF ${fmtGiB(st.gguf_bytes)} GiB · PGRN ${fmtGiB(st.pgrn_bytes)} GiB · ${t("reco.free")} ${st.disk_free_gib.toFixed(0)} GiB`;
  } else if (st.gguf_bytes > 0) {
    badge.className = "badge partial"; badge.textContent = t("st.needConvert");
    dlStatus.textContent = t("st.notConverted");
    dlBar.style.width = "50%";
    dlNote.textContent = `GGUF ${fmtGiB(st.gguf_bytes)} GiB · ${t("reco.free")} ${st.disk_free_gib.toFixed(0)} GiB`;
  } else if (st.pgrn_bytes > 0) {
    // PGRN on the fast disk, but GGUF path points nowhere — almost always the
    // unset second-SSD (extBase) folder when the GGUF lives on an external volume.
    badge.className = "badge partial"; badge.textContent = t("badge.partial");
    dlStatus.textContent = t("st.needGguf");
    dlBar.style.width = "50%";
    dlNote.textContent = `PGRN ${fmtGiB(st.pgrn_bytes)} GiB · ${t("reco.free")} ${st.disk_free_gib.toFixed(0)} GiB`;
  } else {
    badge.className = "badge missing"; badge.textContent = t("badge.notLoaded");
    dlStatus.textContent = t("st.notThere");
    dlBar.style.width = "0%";
    dlNote.textContent = `${t("reco.free")}: ${st.disk_free_gib.toFixed(0)} GiB`;
  }

  // power button reflects readiness when server is stopped
  if (!state.running) {
    const ready = st.pgrn_bytes > 0 && st.gguf_bytes > 0;
    $("powerBtn").disabled = !ready;
    $("powerBtn").textContent = ready ? t("btn.start") : t("btn.notReady");
  }
}

$("dlBtn").onclick = async () => {
  const url = $("pUrl").value.trim();
  if (!url) { toast(t("toast.noUrl"), true); return; }
  try {
    await invoke("start_download", { url, dest: ggufPath(), dir: modelDir() });
    toast(t("toast.dlStarted")); state.remoteBytes = 0; refreshModel();
  } catch (e) { toast(e, true); }
};
$("convBtn").onclick = async () => {
  // Same PGRN resolution as status/start: the streamed sidecar belongs on the
  // fast internal SSD (pPgrn), not necessarily next to the (external) GGUF.
  const gguf = ggufPath();
  const pgrn = $("pPgrn").value.trim() || pgrnOf(gguf);
  try {
    await invoke("start_convert", { gguf, pgrn, ioThreads: +$("io").value || 8, resume: false });
    toast(t("toast.convStarted")); refreshModel();
  } catch (e) { toast(e, true); }
};
$("resumeBtn").onclick = async () => {
  const pgrn = $("pPgrn").value.trim() || pgrnOf(ggufPath());
  try {
    await invoke("start_convert", { gguf: ggufPath(), pgrn, ioThreads: +$("io").value || 8, resume: true });
    toast(t("toast.resumed")); refreshModel();
  } catch (e) { toast(e, true); }
};
$("resumeDiscard").onclick = async () => {
  const pgrn = $("pPgrn").value.trim() || pgrnOf(ggufPath());
  try {
    await invoke("discard_convert", { pgrn });
    toast(t("toast.discarded")); refreshModel();
  } catch (e) { toast(e, true); }
};
$("dlCancel").onclick = async () => {
  await invoke("cancel_download"); await invoke("cancel_convert");
  toast(t("toast.canceled")); refreshModel();
};

// ---- server start/stop -----------------------------------------------------
function runtimeComponent(report, name) {
  return ((report && report.components) || []).find((c) => c.name === name) || null;
}

function runtimeComponentText(component) {
  if (!component || !component.applicable) return "–";
  return component.ready ? "✓" : `✗ ${component.detail || "missing"}`;
}

function formatStorageDevice(report) {
  if (!report) return "–";
  const device = report.internal === true ? t("runtime.internal")
    : report.internal === false ? t("runtime.external")
    : t("runtime.unknown");
  const medium = report.solid_state === true ? " SSD" : "";
  const link = report.is_symlink ? " · symlink" : "";
  return `${device}${medium}${link}`;
}

function renderNativeRuntimeStatus() {
  const runtime = state.nativeRuntime;
  const modelStorage = state.nativeStorage.model;
  const pgrnStorage = state.nativeStorage.pgrn;
  const badge = $("runtimeBadge");
  if (!badge) return;

  const llama = runtimeComponent(runtime, "llama_server");
  const convert = runtimeComponent(runtime, "pgrn_convert");
  const omlxComponents = ((runtime && runtime.components) || []).filter((c) => c.applicable && c.required
    && (c.name.startsWith("omlx_") || c.name.startsWith("pgrn_host")));
  const failedOmlx = omlxComponents.find((c) => !c.ready) || null;
  $("runtimeLlama").textContent = runtimeComponentText(llama);
  $("runtimeConvert").textContent = runtimeComponentText(convert);
  $("runtimeOmlx").textContent = omlxComponents.length > 0 && !failedOmlx
    ? "✓" : runtimeComponentText(failedOmlx);
  $("runtimeVersion").textContent = runtime
    ? `schema ${runtime.schema} · MLX ${(runtime.mlx_packages || {}).mlx || "–"}` : "–";
  $("runtimeModelDevice").textContent = formatStorageDevice(modelStorage);
  $("runtimePgrnDevice").textContent = formatStorageDevice(pgrnStorage);
  $("runtimeDiskFree").textContent = pgrnStorage
    ? `${fmtGiB(pgrnStorage.available_bytes)} GiB` : "–";

  const diskReady = [modelStorage, pgrnStorage].filter(Boolean).every((storage) => storage.admitted);
  const ready = !!(runtime && runtime.ready && diskReady);
  badge.className = `badge ${ready ? "ready" : "missing"}`;
  badge.textContent = ready ? t("badge.ready") : t("badge.missing");
  let note = !runtime || !runtime.ready ? t("runtime.incomplete")
    : !diskReady ? t("runtime.diskUnsafe")
    : pgrnStorage && !pgrnStorage.placement_ok ? t("runtime.externalPgrn")
    : t("runtime.ready");
  const failed = ((runtime && runtime.components) || [])
    .filter((c) => c.applicable && c.required && !c.ready)
    .map((c) => `${c.name}: ${c.detail}`);
  if (failed.length) note += ` ${failed.join(" · ")}`;
  $("runtimeNote").textContent = note;
}

function nativeStoragePaths(resolvedBackend) {
  if (resolvedBackend === "mlx" && state.model.mlx) {
    const catalog = ($("pMlx") && $("pMlx").value.trim())
      || state.model.mlx.dir || defaultMlxDir();
    const model = `${catalog}/${state.model.mlx.id}`;
    return { model, pgrn: `${model}/experts.pgrn` };
  }
  const model = ggufPath();
  return { model, pgrn: ($("pPgrn").value.trim() || pgrnOf(model)) };
}

async function storageReport(path, role) {
  try {
    return await invoke("inspect_storage", {
      path,
      role,
      plannedBytes: 0,
      reserveBytes: 3 * 1024 * 1024 * 1024,
    });
  } catch (e) {
    return {
      path, role, admitted: false, placement_ok: false,
      detail: String(e), internal: null, solid_state: null,
    };
  }
}

async function refreshNativeRuntimeStatus(resolvedBackend) {
  try {
    state.nativeRuntime = await invoke("runtime_preflight");
  } catch (e) {
    state.nativeRuntime = { ready: false, components: [], mlx_packages: {}, detail: String(e) };
  }
  const paths = nativeStoragePaths(resolvedBackend || effectiveBackend());
  const [model, pgrn] = await Promise.all([
    storageReport(paths.model, "model"),
    storageReport(paths.pgrn, "pgrn"),
  ]);
  state.nativeStorage = { model, pgrn };
  renderNativeRuntimeStatus();
  return { runtime: state.nativeRuntime, storageReports: [model, pgrn] };
}

async function ensureNativeStartReady(resolvedBackend) {
  const status = await refreshNativeRuntimeStatus(resolvedBackend);
  const runtime = status.runtime;
  if (!runtime.ready) {
    toast(t("runtime.incomplete"), true);
    return false;
  }
  for (const storage of status.storageReports) {
    if (!storage.admitted) {
      toast(`${t("runtime.diskUnsafe")} ${storage.detail || ""}`, true);
      return false;
    }
  }
  const pgrnStorage = status.storageReports.find((storage) => storage.role === "pgrn");
  if (pgrnStorage && !pgrnStorage.placement_ok) toast(t("runtime.externalPgrn"), true);
  return true;
}

/** Cluster tab: surface mlx_capability streaming readiness (reuse Settings probe). */
async function refreshClusterMlxCap() {
  const code = $("clusterMlxCap");
  const badge = $("clusterMlxCapBadge");
  if (!code) return;
  // Probe when MLX is possible (explicit or Auto); Metal-only shows metal badge.
  if (effectiveBackend() !== "mlx" && state.backend !== "mlx" && !isAutoBackend()) {
    code.textContent = t("mlx.stream.metal");
    if (badge) {
      badge.hidden = false;
      badge.className = "badge ready";
      badge.textContent = "metal";
    }
    return;
  }
  const mlxDir = ($("pMlx") && $("pMlx").value.trim())
    || (state.model.mlx && state.model.mlx.dir)
    || "";
  try {
    const cap = await invoke("mlx_capability", { mlxDir });
    const mode = cap.mode || "unavailable";
    const label = mode === "streaming" ? t("mlx.stream.ready")
      : mode === "resident" ? t("mlx.stream.resident")
      : t("mlx.stream.unavailable");
    const short = mode === "streaming"
      ? `${label}${cap.models_with_experts_pgrn != null ? ` · ${cap.models_with_experts_pgrn}/${cap.models || 0} pgrn` : ""}`
      : (cap.detail || label);
    code.textContent = short;
    if (badge) {
      badge.hidden = false;
      badge.className = "badge " + (mode === "streaming" ? "ready" : mode === "resident" ? "partial" : "missing");
      badge.textContent = mode;
    }
  } catch (e) {
    code.textContent = String(e);
    if (badge) {
      badge.hidden = false;
      badge.className = "badge missing";
      badge.textContent = "error";
    }
  }
}

async function refreshMlxCapability() {
  const note = $("mlxCapNote");
  const btn = $("mlxRuntimeBtn");
  refreshClusterMlxCap();
  if (!note) return;
  // Show capability for explicit MLX and Auto (needs experts.pgrn for short/warm pick).
  if (state.backend !== "mlx" && !isAutoBackend()) {
    note.hidden = true;
    note.textContent = "";
    if (btn) btn.hidden = true;
    return;
  }
  const mlxDir = ($("pMlx") && $("pMlx").value.trim())
    || (state.model.mlx && state.model.mlx.dir)
    || "";
  try {
    const cap = await invoke("mlx_capability", { mlxDir });
    note.hidden = false;
    note.textContent = cap.detail || "";
    if (cap.mode === "streaming") note.style.color = "";
    else if (cap.mode === "resident") note.style.color = "var(--warn, #b8860b)";
    else note.style.color = "var(--danger, #c0392b)";
    if (btn) {
      const needInstall = !cap.runtime_ready && cap.runtime_state !== "installing"
        && (cap.mode === "unavailable" || (!cap.omlx_app && !cap.runtime_ready));
      const installing = cap.runtime_state === "installing";
      btn.hidden = !(needInstall || installing);
      btn.disabled = installing;
      btn.textContent = installing ? t("btn.mlxRuntimeBusy") : t("btn.mlxRuntime");
    }
  } catch (e) {
    note.hidden = false;
    note.textContent = String(e);
    note.style.color = "var(--danger, #c0392b)";
    if (btn) btn.hidden = false;
  }
}

/** Caps hint for SLIPSTREAM_PGRN_PROFILE (docs/PGRN_ON_MLX.md). */
const PGRN_PROFILE_HINT = {
  balanced: "hint.pgrnProfileBalanced",
  quality: "hint.pgrnProfileQuality",
  fast: "hint.pgrnProfileFast",
};

function pgrnProfileFromUi() {
  const v = ($("pgrnProfile") && $("pgrnProfile").value) || localStorage.getItem("slipstream.pgrn.profile") || "balanced";
  return (v === "quality" || v === "fast") ? v : "balanced";
}

/** Keep expanded MLX tool/schema prompts below the Apple Metal hard cap. */
function pgrnProfileForStart(resolved, pendingText) {
  const selected = pgrnProfileFromUi();
  const commonContract = chatToolsPreference()
    || chatJsonPreference()
    || messageAsksForTools(pendingText);
  if (resolved === "mlx" && commonContract) return "contract";
  return selected;
}
/** Product residency allow-list. Unknown / sticky garbage → touch (mlock is opt-in). */
function normalizePgrnResidency(v) {
  return (v === "mlock" || v === "off" || v === "touch") ? v : "touch";
}
function pgrnResidencyFromUi() {
  const v = ($("pgrnResidency") && $("pgrnResidency").value) || localStorage.getItem("slipstream.pgrn.residency") || "touch";
  return normalizePgrnResidency(v);
}
/** Cold-start floors — keep in sync with mlx.rs MLX_MIN/WARN + Track D mlock confirm. */
const MLX_MIN_FREE_GIB = 4;
const MLX_WARN_FREE_GIB = 8;
const MLX_MLOCK_CONFIRM_GIB = 12;
/**
 * Pure cold-start mem-guard decision (no DOM / no toast).
 * Mirrored in scripts/test_memory_guard_ux.mjs.
 * @returns {{ action: "refuse"|"allow", reason: string, warnKey?: string|null, confirmKey?: string|null }}
 */
function coldStartMemGuard(freeGib, residency) {
  if (freeGib == null || !Number.isFinite(freeGib)) {
    return { action: "allow", reason: "unknown_free", warnKey: null, confirmKey: null };
  }
  if (freeGib < MLX_MIN_FREE_GIB) {
    return { action: "refuse", reason: "critical_free", warnKey: "warn.mlxFreeCritical", confirmKey: null };
  }
  let warnKey = null;
  let confirmKey = null;
  let reason = "ok";
  if (freeGib < MLX_WARN_FREE_GIB) {
    warnKey = "warn.mlxFreeLow";
    reason = "warn_free";
  }
  if (normalizePgrnResidency(residency) === "mlock" && freeGib < MLX_MLOCK_CONFIRM_GIB) {
    confirmKey = "warn.mlxMlockLow";
    reason = warnKey ? "warn_and_confirm_mlock" : "confirm_mlock";
  }
  return { action: "allow", reason, warnKey, confirmKey };
}
function pgrnKeepHotFromUi() {
  if ($("pgrnKeepHot")) return !!$("pgrnKeepHot").checked;
  const s = localStorage.getItem("slipstream.pgrn.keepHot");
  return s === null ? true : s === "1";
}
function pgrnWarmupFromUi() {
  if ($("pgrnWarmup")) return !!$("pgrnWarmup").checked;
  const s = localStorage.getItem("slipstream.pgrn.warmup");
  return s === null ? true : s === "1";
}
/** Opt-in --memory-guard off (Metal wired ~28 GiB). Default false. */
function memoryGuardOffFromUi() {
  if ($("memoryGuardOff")) return !!$("memoryGuardOff").checked;
  return localStorage.getItem("slipstream.pgrn.memoryGuardOff") === "1";
}
function pgrnPeerBaseFromUi() {
  if ($("pgrnPeerBase")) return $("pgrnPeerBase").value.trim();
  return (localStorage.getItem("slipstream.pgrn.peerBase") || "").trim();
}
/** Opt-in MCP JSON/YAML path → OMLX_MCP_CONFIG. Empty = MCP OFF. */
function mcpConfigFromUi() {
  if ($("mcpConfig")) return $("mcpConfig").value.trim();
  return (localStorage.getItem("slipstream.mlx.mcpConfig") || "").trim();
}
/** Absolute path (or empty=OFF). Relative paths are invalid for serve cwd. */
function mcpConfigPathOk(p) {
  const s = String(p == null ? "" : p).trim();
  if (!s) return true;
  if (s.startsWith("/")) return true;
  return /^[A-Za-z]:[\\/]/.test(s);
}

function updatePgrnProfileHint() {
  const el = $("pgrnProfileHint");
  if (!el) return;
  const key = PGRN_PROFILE_HINT[pgrnProfileFromUi()] || PGRN_PROFILE_HINT.balanced;
  el.dataset.i18n = key;
  el.textContent = t(key);
}

function persistPgrnMlxSettings() {
  localStorage.setItem("slipstream.pgrn.profile", pgrnProfileFromUi());
  localStorage.setItem("slipstream.pgrn.residency", pgrnResidencyFromUi());
  localStorage.setItem("slipstream.pgrn.keepHot", pgrnKeepHotFromUi() ? "1" : "0");
  localStorage.setItem("slipstream.pgrn.warmup", pgrnWarmupFromUi() ? "1" : "0");
  localStorage.setItem("slipstream.pgrn.memoryGuardOff", memoryGuardOffFromUi() ? "1" : "0");
  localStorage.setItem("slipstream.pgrn.peerBase", pgrnPeerBaseFromUi());
  const mcpPath = mcpConfigFromUi();
  localStorage.setItem("slipstream.mlx.mcpConfig", mcpPath);
  if (!mcpConfigPathOk(mcpPath) && typeof toast === "function") {
    toast(t("warn.mcpConfigRel"), true);
  }
  updatePgrnProfileHint();
}

function loadPgrnMlxSettings() {
  if ($("pgrnProfile")) {
    const p = localStorage.getItem("slipstream.pgrn.profile") || "balanced";
    $("pgrnProfile").value = (p === "quality" || p === "fast") ? p : "balanced";
  }
  if ($("pgrnResidency")) {
    const r = localStorage.getItem("slipstream.pgrn.residency") || "touch";
    $("pgrnResidency").value = normalizePgrnResidency(r);
  }
  if ($("pgrnKeepHot")) {
    const s = localStorage.getItem("slipstream.pgrn.keepHot");
    $("pgrnKeepHot").checked = s === null ? true : s === "1";
  }
  if ($("pgrnWarmup")) {
    const s = localStorage.getItem("slipstream.pgrn.warmup");
    $("pgrnWarmup").checked = s === null ? true : s === "1";
  }
  if ($("memoryGuardOff")) {
    $("memoryGuardOff").checked = localStorage.getItem("slipstream.pgrn.memoryGuardOff") === "1";
  }
  if ($("pgrnPeerBase")) {
    $("pgrnPeerBase").value = localStorage.getItem("slipstream.pgrn.peerBase") || "";
  }
  if ($("mcpConfig")) {
    $("mcpConfig").value = localStorage.getItem("slipstream.mlx.mcpConfig") || "";
  }
  updatePgrnProfileHint();
}

function wirePgrnMlxSettings() {
  ["pgrnProfile", "pgrnResidency"].forEach((id) => {
    if ($(id)) $(id).onchange = persistPgrnMlxSettings;
  });
  ["pgrnKeepHot", "pgrnWarmup", "memoryGuardOff"].forEach((id) => {
    if ($(id)) $(id).onchange = persistPgrnMlxSettings;
  });
  if ($("pgrnPeerBase")) {
    $("pgrnPeerBase").addEventListener("change", persistPgrnMlxSettings);
    $("pgrnPeerBase").addEventListener("blur", persistPgrnMlxSettings);
  }
  if ($("mcpConfig")) {
    $("mcpConfig").addEventListener("change", persistPgrnMlxSettings);
    $("mcpConfig").addEventListener("blur", persistPgrnMlxSettings);
  }
  loadPgrnMlxSettings();
}

function applyBackendUi() {
  const prefMlx = state.backend === "mlx";
  const showMlxDir = prefMlx || isAutoBackend();
  // Chat levers follow the effective engine (after Auto resolve); Auto alone shows dir.
  const mlx = effectiveBackend() === "mlx";
  if ($("backendSel")) $("backendSel").value = isAutoBackend() ? "auto" : state.backend;
  if ($("mlxDirWrap")) $("mlxDirWrap").hidden = !showMlxDir;
  // PGRN levers only apply to Metal. Explicit MLX gets its own short note instead
  // of a stack of disabled Metal knobs; Auto keeps them because it may resolve Metal.
  ["metalCacheCtrl", "metalDraftCtrl", "metalStreamingCard"].forEach((id) => {
    const el = $(id);
    if (el) el.hidden = prefMlx;
  });
  if ($("mlxLeverNote")) $("mlxLeverNote").hidden = !prefMlx;
  ["cache", "io", "compactSlots", "grammarDraft", "pMirror", "bufferedReads", "onlinePredict"]
    .forEach((id) => {
      const el = $(id);
      if (!el) return;
      el.disabled = prefMlx;
    });
  if (showMlxDir && state.model.mlx) {
    if ($("pMlx") && !$("pMlx").value.trim()) {
      $("pMlx").value = state.model.mlx.dir || defaultMlxDir();
    }
  }
  if (mlx && state.model.mlx) {
    if (!state.chatModelsLive) state.chatModel = state.model.mlx.id;
  } else if (!state.chatModelsLive && !isAutoBackend()) {
    state.chatModel = "slipstream";
  }
  // Tools / JSON are the common OpenAI contract. Attachments remain scoped below.
  ["chatToolsWrap", "chatJsonWrap", "chatToolsSettingsWrap"].forEach((id) => {
    const el = $(id);
    if (el) el.hidden = false;
  });
  // Slipstream PGRN settings: show for explicit MLX or Auto (may resolve to MLX).
  if ($("mlxPgrnSettingsWrap")) $("mlxPgrnSettingsWrap").hidden = !showMlxDir;
  syncChatToolsUi(chatToolsPreference());
  syncChatJsonUi(chatJsonPreference());
  updateChatSchemaVisibility();
  updatePgrnProfileHint();
  refreshMlxCapability();
  refreshChatModels();
  updateChatCapabilityControls();
}

async function startServer() {
  const draft = state.model.draft ? `${modelDir()}/${state.model.draft}` : "";
  let mlxDir = ($("pMlx") && $("pMlx").value.trim()) || (state.model.mlx && state.model.mlx.dir) || "";
  const coerced = coerceMlxCatalogDir(mlxDir);
  if (coerced && coerced !== normalizeRoot(mlxDir)) {
    mlxDir = coerced;
    if ($("pMlx")) $("pMlx").value = mlxDir;
    localStorage.setItem("slipstream.mlxDir", mlxDir);
    if (state.model && state.model.mlx) state.model.mlx.dir = mlxDir;
    MODELS.forEach((m) => { if (m.mlx) m.mlx.dir = mlxDir; });
  }
  const pending = ($("chatInput") && $("chatInput").value) || "";
  const promptChars = estimatePromptChars(pending);
  let hasExperts = false;
  if (isAutoBackend() || state.backend === "mlx") {
    try {
      const cap = await invoke("mlx_capability", { mlxDir });
      hasExperts = !!(cap && (cap.models_with_experts_pgrn > 0 || cap.mode === "streaming"));
    } catch {}
  }
  const resolved = isAutoBackend()
    ? resolveAutoBackend(promptChars, hasExperts)
    : (state.backend === "mlx" ? "mlx" : "metal");
  if (!(await ensureNativeStartReady(resolved))) return;
  const cfg = {
    server: $("pServer").value.trim(),
    model: ggufPath(),
    pgrn: $("pPgrn").value.trim() || pgrnOf(ggufPath()),
    cache_gb: +$("cache").value,
    headroom_gb: 3,
    ctx: +$("ctx").value,
    io_threads: +$("io").value,
    port: PORT,
    thinking: $("thinking").checked,
    // The MTP toggle has to actually gate speculation — it was decorative before.
    spec_type: ($("specMtp") && !$("specMtp").checked) ? "none" : effectiveSpec(),
    draft_model: draft,
    pgrn_mirror: ($("pMirror") && $("pMirror").value.trim()) || "",
    pgrn_buffered: !!($("bufferedReads") && $("bufferedReads").checked),
    pgrn_online: !!($("onlinePredict") && $("onlinePredict").checked),
    pgrn_compact: $("compactSlots") ? $("compactSlots").checked : true,
    grammar_draft: $("grammarDraft") ? $("grammarDraft").checked : true,
    kv_quant: state.model.kv || "q8_0",
    backend: state.backend,
    mlx_dir: mlxDir,
    omlx_bin: "",
    prompt_chars: promptChars,
    // MLX-only SLIPSTREAM_PGRN_* (Metal start ignores these fields).
    pgrn_profile: pgrnProfileForStart(resolved, pending),
    pgrn_residency: pgrnResidencyFromUi(),
    pgrn_keep_hot: pgrnKeepHotFromUi(),
    pgrn_warmup: pgrnWarmupFromUi(),
    pgrn_l3_peer_base: pgrnPeerBaseFromUi(),
    mcp_config: mcpConfigFromUi(),
    memory_guard_off: memoryGuardOffFromUi(),
  };
  persistPgrnMlxSettings();
  const goingMlx = state.backend === "mlx" || (isAutoBackend() && resolved === "mlx");
  if (goingMlx && !state.model.mlx) {
    toast(t("err.noMlxModel"), true);
    return;
  }
  // Cold-start UX: warn / confirm before MLX when free RAM is tight (backend also
  // refuses < MLX_MIN_FREE_GIB). Prefer touch; mlock stays opt-in for short measured runs.
  if (goingMlx) {
    const free = state.sys && typeof state.sys.free_gib === "number" ? state.sys.free_gib : null;
    const guard = coldStartMemGuard(free, cfg.pgrn_residency);
    if (guard.action === "refuse") {
      toast(t(guard.warnKey || "warn.mlxFreeCritical"), true);
      return;
    }
    if (guard.warnKey) toast(t(guard.warnKey), true);
    if (guard.confirmKey && !confirm(t(guard.confirmKey))) return;
  }
  // Metal peak gate: cache≥14 needs ≥17 GiB free (prefer ≥22; admit ≥21.5 = 22−0.5 tol).
  if (!goingMlx && cfg.cache_gb >= 14) {
    const freeM = state.sys && typeof state.sys.free_gib === "number" ? state.sys.free_gib : null;
    const peakAdmit = 22 - 0.5; // PEAK_FREE_GIB − PEAK_FREE_TOLERANCE_GIB
    if (freeM != null && freeM < 17) {
      toast(t("warn.peakNeeds17"), true);
      return;
    }
    if (freeM != null && freeM < peakAdmit && !confirm(t("confirm.peakMarginal"))) return;
  }
  try {
    resetLlamaToolPrime();
    const msg = await invoke("start_server", { cfg });
    state.resolvedBackend = isAutoBackend() ? resolved : null;
    state.runningPgrnProfile = goingMlx ? cfg.pgrn_profile : null;
    const profileNote = goingMlx && cfg.pgrn_profile === "contract"
      ? ` · ${t("toast.mlxContractProfile")}`
      : "";
    toast(`${msg || t("srv.startedLoading")}${profileNote}`);
    setPill("loading");
    applyBackendUi();
  } catch (e) {
    const msg = String(e || "");
    if (/MLX model directory is empty or missing config\.json/i.test(msg)) {
      toast(t("err.mlxDirLeaf"), true);
    } else {
      toast(e, true);
    }
  }
}
async function stopServer() {
  resetLlamaToolPrime();
  await invoke("stop_server");
  state.resolvedBackend = null;
  state.runningPgrnProfile = null;
  setPill("off");
  toast(t("toast.serverStopped"));
  applyBackendUi();
}
$("powerBtn").onclick = () => (state.running ? stopServer() : startServer());

function setPill(mode) {
  const p = $("statusPill"), b = $("powerBtn");
  state.pillMode = mode;
  if (mode === "on") { p.className = "pill pill-on"; p.textContent = t("pill.running"); b.textContent = t("btn.stop"); b.className = "btn btn-danger"; b.disabled = false; }
  else if (mode === "loading") { p.className = "pill pill-loading"; p.textContent = t("pill.starting"); b.textContent = t("btn.stop"); b.className = "btn btn-danger"; b.disabled = false; }
  else { p.className = "pill pill-off"; p.textContent = t("pill.stopped"); b.className = "btn btn-primary"; }
}

// ---- cache slider ----------------------------------------------------------
$("cache").oninput = () => { $("cacheVal").textContent = $("cache").value; updateCacheRec(); renderSpeed(); };
function updateCacheRec() {
  const c = +$("cache").value;
  let rec = "";
  if (c <= 6) rec = t("reco.cons");
  else if (c <= 12) rec = t("reco.rec");
  else if (c === 14) rec = t("reco.peak");
  else if (c <= 18) rec = t("reco.fast");
  else rec = t("reco.aggr");
  $("cacheRec").innerHTML = rec;
}

// ---- auto recommendation (best settings for THIS machine) ------------------
const GiB = 1073741824;
function computeReco() {
  const s = state.sys, st = state.mstatus, m = state.model;
  if (!s || !s.total_gib) return null;
  // resident (non-expert) weights ~= gguf - pgrn; fall back to a rough fraction.
  const resident = (st && st.gguf_bytes && st.pgrn_bytes && st.gguf_bytes > st.pgrn_bytes)
    ? (st.gguf_bytes - st.pgrn_bytes) / GiB : m.gb * 0.12;
  const pgrnGb = (st && st.pgrn_bytes) ? st.pgrn_bytes / GiB : m.gb * 0.88;
  const draftGb = m.draft ? 2.2 : 0;
  const free = typeof s.free_gib === "number" ? s.free_gib : 0;
  // Qualified Metal bands (HANDOVER / METAL_PEAK_VS_SMOKE). Headroom fixed at 3 GiB;
  // keep post-load free estimate ≥ 2 GiB. io=4 is the 2×-qualified balanced width.
  // Peak preferred quiet ≥22; admit band ≥21.5 (0.5 GiB tolerance) so 21.81 is not refused.
  const headroom = 3;
  const postFloor = 2;
  const peakAdmit = 22 - 0.5;
  let cache, band;
  if (free >= peakAdmit) { cache = 14; band = "peak"; }
  else if (free >= 17) { cache = 10; band = "warm"; }
  else if (free >= 12) { cache = 8; band = "cons"; }
  else if (free >= 8) { cache = 6; band = "cons"; }
  else { cache = 4; band = "tight"; }
  const maxByFloor = Math.floor(free - resident - draftGb - headroom - postFloor);
  if (Number.isFinite(maxByFloor)) cache = Math.min(cache, Math.max(4, maxByFloor));
  cache = Math.max(4, Math.min(cache, 24, Math.ceil(pgrnGb) || cache));
  // context: derived from BOTH total and free RAM (KV cache grows with context), snapped to options.
  const ctxOpts = [16384, 32768, 40960, 65536];
  const ctxCeil = s.total_gib >= 48 ? 65536 : s.total_gib >= 32 ? 40960 : s.total_gib >= 20 ? 32768 : 16384;
  const freeCap = free >= 20 ? 65536 : free >= 12 ? 40960 : free >= 7 ? 32768 : 16384;
  let ctx = Math.min(ctxCeil, freeCap);
  if (!ctxOpts.includes(ctx)) ctx = ctxOpts.filter((o) => o <= ctx).pop() || 16384;
  const io = 4;
  return { cache, ctx, io, resident, pgrnGb, band, headroom };
}
function updateReco() {
  const r = computeReco();
  const el = $("recoText");
  if (!el) return;
  if (!r) { el.textContent = t("reco.computing"); return; }
  const s = state.sys;
  el.innerHTML =
    `${t("reco.for")} (<b>${s.total_gib.toFixed(0)} GiB</b> RAM, ${s.free_gib.toFixed(0)} ${t("reco.free")}) ${t("reco.with")} <b>${state.model.name.split(" ")[0]}</b>: ` +
    `Cache <b>${r.cache} GiB</b> &middot; ${t("lbl.context")} <b>${r.ctx / 1024}k</b> &middot; io <b>${r.io}</b> &middot; ` +
    `${t("reco.pgrnFast")}` +
    (state.model.draft ? ` &middot; <b>DFlash</b>` : (effectiveSpec() === "draft-mtp" ? ` &middot; <b>MTP</b>` : ""));
  state.reco = r;
}
// Memory panel: what actually has to fit in RAM. Every line is either measured
// (resident share from the real file sizes, KV from the engine's own log) or a
// configured value — no invented KV formula.
function renderMemPlan() {
  const el = $("memPlan"); if (!el) return;
  const s = state.sys, r = computeReco();
  if (!s || !r) { el.textContent = t("reco.computing"); return; }
  const cache = +$("cache").value || 10;
  const kvGb = state.kvMiB ? state.kvMiB / 1024 : null;
  const reserve = 3;
  const sum = r.resident + cache + (kvGb || 0) + reserve;
  const free = s.free_gib;
  const kvType = state.model.kv || "q8_0";
  const row = (label, val) => `<tr><td>${label}</td><td>${val}</td></tr>`;
  el.innerHTML =
    `<table class="bench-table">` +
    row(t("mem.resident"), `${dec(r.resident, 1)} GiB`) +
    row(t("mem.cache"), `${cache} GiB`) +
    row(`${t("mem.kv")} (${(+$("ctx").value || 40960) / 1024}k · ${kvType})`,
        kvGb != null ? `${dec(kvGb, 1)} GiB` : t("mem.kvPending")) +
    row(t("mem.reserve"), `${reserve} GiB`) +
    `<tr class="bench-mean"><td>${t("mem.sum")}</td><td>${dec(sum, 1)} / ${dec(free, 1)} GiB</td></tr>` +
    `</table>` +
    `<div class="bench-note">${sum <= free ? t("mem.fits") : sum <= free + 2 ? t("mem.tight") : t("mem.over")}</div>`;
}

// Streaming panel: does the engine get a width-weighted partition or the equal
// split? Driven by the sidecar that start_server auto-detects — not a guess.
function renderPartitionNote() {
  const el = $("partitionNote"); if (!el) return;
  const st = state.mstatus;
  if (!st) { el.textContent = t("partition.unknown"); return; }
  el.innerHTML = st.weights
    ? `<b style="color:var(--green)">${t("partition.weighted")}</b>`
    : t("partition.equal");
}

function applyReco() {
  const r = state.reco || computeReco();
  if (!r) { toast(t("toast.noSys"), true); return; }
  $("cache").value = r.cache; $("cacheVal").textContent = r.cache; updateCacheRec();
  $("ctx").value = String(r.ctx);
  $("io").value = String(r.io);
  if ($("thinking")) $("thinking").checked = false;
  if ($("chatThink")) $("chatThink").checked = false;
  toast(`${t("toast.applied")}: Cache ${r.cache} · ${r.ctx / 1024}k · io ${r.io}`); renderSpeed();
  renderObsStrip(state.live || {});
}
function applyPeak() {
  // Qualified Metal Peak: cache=14 · headroom=3 · io=4 (~18–19 tok/s warm when free allows).
  // Preferred quiet ≥22; admit ≥21.5 (0.5 tol) without confirm; confirm only below admit band.
  const free = state.sys && typeof state.sys.free_gib === "number" ? state.sys.free_gib : null;
  const peakAdmit = 22 - 0.5;
  if (free != null && free < 17) {
    toast(t("warn.peakNeeds17"), true);
    return;
  }
  if (free != null && free < peakAdmit && !confirm(t("confirm.peakMarginal"))) return;
  $("cache").value = 14; $("cacheVal").textContent = "14"; updateCacheRec();
  if ($("io")) $("io").value = "4";
  if ($("compactSlots")) $("compactSlots").checked = true;
  if ($("thinking")) $("thinking").checked = false;
  if ($("chatThink")) $("chatThink").checked = false;
  toast(t("toast.peakApplied"));
  renderSpeed();
  renderObsStrip(state.live || {});
}
function applyGoodTokens() {
  // Shared coding path: temperature 0 + thinking off (Metal + oMLX).
  if ($("thinking")) $("thinking").checked = false;
  if ($("chatThink")) $("chatThink").checked = false;
  toast(t("toast.goodTokens"));
}
$("applyReco").onclick = applyReco;
if ($("applyPeak")) $("applyPeak").onclick = applyPeak;
if ($("applyGoodTokens")) $("applyGoodTokens").onclick = applyGoodTokens;

// ---- expected speed (calibrated to our measurements) -----------------------
// Anchors (internal PGRN, decode tok/s): 3B-active @cache10 ~= 13, @cache2 ~= 5.5;
// 8B-active @cache10 ~= 2.8. Power law over active params + linear cache scaling +
// disk factor (external USB SSD measured ~0.37x internal). An estimate, not a promise.
function speedEstimate() {
  const m = state.model, cache = +$("cache").value || 10;
  const a = m.activeB || 3;
  const base10 = 72.8 / Math.pow(a, 1.566);
  const cacheScale = Math.max(0.35, 0.42 + 0.0725 * (cache - 2));
  const external = /\/Volumes\//.test($("pPgrn").value || "");
  const est = Math.max(0.3, base10 * cacheScale * (external ? 0.37 : 1.0));
  return { est, external };
}
function renderSpeed() {
  const el = $("speedText"); if (!el) return;
  const s = speedEstimate();
  el.innerHTML = `≈ <b>${s.est < 10 ? dec(s.est, 1) : Math.round(s.est)} tok/s</b>` +
    (s.external ? ` <span class="agent-tag">(${t("speed.external")})</span>` : "");
}

// ---- installed models (what's already on disk) -----------------------------
async function renderInstalled() {
  const host = $("installedList"); if (!host || !state.def) return;
  const base = defaultModelsRoot() || state.def.model_dir;
  const rows = await Promise.all(MODELS.map(async (m) => {
    if (m.soon) return { m, cls: "soon", txt: `~${m.gb} GiB · ${m.activeB}B active` };
    const ggufDir = `${m.extGguf ? ggufBase() : base}/${currentSubdir()}`;
    const gguf = `${ggufDir}/${m.file}`;
    const pgrn = `${base}/${currentSubdir()}/${currentFile().replace(/\.gguf$/i, ".pgrn")}`;
    let st; try { st = await invoke("model_status", { gguf, pgrn, dir: ggufDir }); } catch { st = null; }
    const has = st && st.pgrn_bytes > 0 && st.gguf_bytes > 0;
    const part = st && st.gguf_bytes > 0;
    return { m, cls: has ? "ready" : part ? "partial" : "missing",
      txt: has ? `${(st.gguf_bytes / 1073741824).toFixed(0)}+${(st.pgrn_bytes / 1073741824).toFixed(0)} GiB` : part ? "GGUF only" : "—" };
  }));
  host.innerHTML = "";
  rows.forEach((r) => {
    const el = document.createElement("div");
    el.className = "agent";
    el.innerHTML = `<div class="agent-info"><div class="agent-name">${r.m.name}</div><div class="agent-tag">${r.txt}</div></div><span class="badge ${r.cls}">${t("badge." + (r.cls === "ready" ? "ready" : r.cls === "partial" ? "partial" : r.cls === "soon" ? "soon" : "notLoaded"))}</span>`;
    host.appendChild(el);
  });
}

// ---- log parsing + live monitor -------------------------------------------
function parseLog(text) {
  const lines = text.split("\n");
  let hitPct = null, hits = null, misses = null, tps = null, loaded = false;
  let lastPrompt = null, lastComp = null, sumComp = 0, kvMiB = null;
  for (const l of lines) {
    let m = l.match(/hits = (\d+), misses = (\d+) \(([\d.]+)%\)/);
    if (m) { hits = +m[1]; misses = +m[2]; hitPct = +m[3]; }
    // The engine reports its own KV allocation at load ("KV self size = N MiB" /
    // "Metal KV buffer size = N MiB") — the only honest KV number we have.
    m = l.match(/KV (?:self size|buffer size)\s*=\s*([\d.]+)\s*MiB/);
    if (m) kvMiB = Math.max(kvMiB || 0, +m[1]);
    // Format: "eval time = ... ( 73.44 ms per token, 13.62 tokens per second)"
    // so match the number right before "tokens per second" (not right after "(").
    // Last "eval time" line per request block is the decode speed.
    m = l.match(/eval time =.*?([\d.]+)\s*tokens per second/);
    if (m) tps = +m[1];
    // token counts: "prompt eval time = X ms / N tokens" and "eval time = X ms / N tokens"
    m = l.match(/prompt eval time =\s*[\d.]+ ms \/\s*(\d+) tokens/);
    if (m) lastPrompt = +m[1];
    else { m = l.match(/[ |]eval time =\s*[\d.]+ ms \/\s*(\d+) tokens/); if (m) { lastComp = +m[1]; sumComp += +m[1]; } }
    if (l.includes("HTTP server listening") || l.includes("server is listening") || l.includes("main loop")) loaded = true;
  }
  return { hitPct, hits, misses, tps, loaded, lastPrompt, lastComp, sumComp, kvMiB };
}

// Detect the server's current request phase from the log tail so we can show
// a live "Prefill 42% / ETA" banner during the (slow) prompt read.
function parseActivity(text) {
  const lines = text.split("\n");
  let phase = "idle", pp = null, dec = null;
  for (const l of lines) {
    let m = l.match(/prompt processing, n_tokens =\s*(\d+), progress =\s*([\d.]+)(?:, t =\s*([\d.]+) s \/\s*([\d.]+) tokens per second)?/);
    if (m) { phase = "prefill"; pp = { tokens: +m[1], progress: +m[2], t: m[3] ? +m[3] : null, rate: m[4] ? +m[4] : null }; continue; }
    m = l.match(/n_decoded =\s*(\d+), tg =\s*([\d.]+)/);
    if (m) { phase = "decode"; dec = { tokens: +m[1], tps: +m[2] }; continue; }
    if (/prompt eval time =/.test(l) && phase === "prefill") { phase = "decode"; }
    if (/release:|stop processing|total time =/.test(l)) { phase = "idle"; pp = null; dec = null; }
  }
  return { phase, pp, dec };
}

function updateActivity(a) {
  const el = $("activity");
  if (a.phase === "prefill" && a.pp) {
    const total = a.pp.progress > 0 ? Math.round(a.pp.tokens / a.pp.progress) : a.pp.tokens;
    const p = Math.round(a.pp.progress * 100);
    const eta = (a.pp.rate && a.pp.progress > 0) ? Math.max(0, Math.round((total - a.pp.tokens) / a.pp.rate)) : null;
    el.hidden = false;
    el.className = "activity prefill";
    $("actTitle").textContent = t("act.prefill");
    $("actSub").textContent = `${a.pp.tokens.toLocaleString()} / ${total.toLocaleString()} Tokens`
      + (a.pp.rate ? ` · ${a.pp.rate.toFixed(0)} tok/s` : "")
      + (eta != null ? ` · ETA ~${eta}s` : "");
    $("actBar").style.width = p + "%"; $("actPct").textContent = p + "%";
  } else if (a.phase === "decode") {
    el.hidden = false;
    el.className = "activity decode";
    $("actTitle").textContent = t("act.decode");
    $("actSub").textContent = a.dec ? `${a.dec.tokens} ${t("act.tokens")} · ${tps(a.dec.tps)}` : t("act.running");
    $("actBar").style.width = "100%"; $("actPct").textContent = "";
  } else {
    // Idle status already lives in the topbar pill — keep this strip out of the
    // chrome so it cannot read as a second, empty navbar.
    el.hidden = true;
    el.className = "activity idle";
    $("actTitle").textContent = "";
    $("actSub").textContent = ""; $("actBar").style.width = "0%"; $("actPct").textContent = "";
  }
  sizeChat();
}

// The chat pane fills the content scrollport below the header (+ activity).
// Measure against `.content`, not the window — body itself no longer scrolls.
function sizeChat() {
  const w = document.querySelector(".chat-wrap");
  if (!w || w.closest(".panel").hidden) return;
  const top = w.getBoundingClientRect().top;
  const content = document.querySelector(".content");
  const bottom = content ? content.getBoundingClientRect().bottom : window.innerHeight;
  w.style.height = Math.max(320, bottom - top - 20) + "px";
}
window.addEventListener("resize", sizeChat);

function push(buf, v) { buf.push(v); if (buf.length > 60) buf.shift(); }

function drawSpark(id, buf, color, max) {
  const c = $(id); if (!c) return;
  const dpr = window.devicePixelRatio || 1;
  const w = c.clientWidth, h = c.clientHeight;
  c.width = w * dpr; c.height = h * dpr;
  const ctx = c.getContext("2d"); ctx.scale(dpr, dpr);
  ctx.clearRect(0, 0, w, h);
  if (buf.length < 2) return;
  const top = max || Math.max(...buf, 1);
  const x = (i) => (i / (buf.length - 1)) * w;
  const y = (v) => h - (v / top) * (h - 6) - 3;
  ctx.beginPath(); ctx.moveTo(0, h);
  buf.forEach((v, i) => ctx.lineTo(x(i), y(v)));
  ctx.lineTo(w, h); ctx.closePath();
  const g = ctx.createLinearGradient(0, 0, 0, h);
  g.addColorStop(0, color + "44"); g.addColorStop(1, color + "05");
  ctx.fillStyle = g; ctx.fill();
  ctx.beginPath();
  buf.forEach((v, i) => (i ? ctx.lineTo(x(i), y(v)) : ctx.moveTo(x(i), y(v))));
  ctx.strokeStyle = color; ctx.lineWidth = 1.6; ctx.stroke();
  ctx.beginPath(); ctx.arc(x(buf.length - 1), y(buf[buf.length - 1]), 2.5, 0, 7); ctx.fillStyle = color; ctx.fill();
}
// Both sparklines sit in the live view's streaming section. The token-rate trace
// that used to be here is gone: the strip shows the momentary rate and the serving
// section the average, so a third drawing of it was the duplication we removed.
function drawAll() {
  drawSpark("ssdChart", state.ssd, "#ff9d2f");
  drawSpark("hitChart", state.hit, "#35d07f", 100);
}

// ---- polling loop ----------------------------------------------------------
// ---- signature viz: expert-cache arena -------------------------------------
// Honest aggregate of live metrics: the green fraction tracks the REAL cache
// hit-rate; amber flickers are experts streaming from SSD (rate ~ throughput).
// No per-expert claim — it visualizes the bounded arena's hit-rate + activity.
const ARENA = { cols: 30, rows: 8, hit: 0, ssd: 0, active: false, cells: null };
function initArena() {
  ARENA.cells = Array.from({ length: ARENA.cols * ARENA.rows }, () => ({ rank: Math.random(), flick: 0 }));
  requestAnimationFrame(drawArena);
}
function drawArena() {
  const cv = $("expertGrid");
  if (cv && cv.clientWidth) {
    const ctx = cv.getContext("2d");
    const w = cv.clientWidth, h = cv.clientHeight, dpr = Math.min(window.devicePixelRatio || 1, 2);
    if (cv.width !== Math.round(w * dpr)) { cv.width = Math.round(w * dpr); cv.height = Math.round(h * dpr); }
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    const { cols, rows, cells } = ARENA, cw = w / cols, ch = h / rows, pad = 2;
    const residentFrac = ARENA.hit / 100;
    const streamProb = ARENA.active ? Math.min(0.05 + ARENA.ssd / 5000, 0.22) : 0.003;
    for (let i = 0; i < cells.length; i++) {
      const c = cells[i];
      const x = (i % cols) * cw + pad, y = ((i / cols) | 0) * ch + pad, tw = cw - 2 * pad, th = ch - 2 * pad;
      if (Math.random() < streamProb) c.flick = 1;
      c.flick *= 0.86;
      if (c.flick > 0.04) ctx.fillStyle = `rgba(255,168,58,${0.3 + 0.62 * c.flick})`;   // streaming from SSD
      else if (c.rank < residentFrac) ctx.fillStyle = "rgba(60,200,120,0.5)";            // resident hit
      else ctx.fillStyle = "rgba(120,132,155,0.10)";                                     // cold slot
      ctx.beginPath();
      if (ctx.roundRect) ctx.roundRect(x, y, tw, th, 2); else ctx.rect(x, y, tw, th);
      ctx.fill();
    }
  }
  requestAnimationFrame(drawArena);
}

async function poll() {
  let sstate = "down";
  try { sstate = await invoke("server_state"); } catch {}
  state.running = sstate !== "down";

  // The log still answers two things the collector cannot: which phase the engine
  // is in right now (for the strip) and how large the KV cache turned out (for
  // the plan). Everything it used to also supply here — hit rate, misses, token
  // rate — now comes from `live_stats`, so no figure has two readers.
  if (state.running) {
    try {
      const log = await invoke("read_log", { maxLines: 400 });
      const p = parseLog(log);
      const act = parseActivity(log);
      updateActivity(act);
      ARENA.active = act.phase === "decode" || act.phase === "prefill";
      // Load-time line: keep it, the 400-line tail window scrolls past it later.
      if (p.kvMiB) state.kvMiB = p.kvMiB;
    } catch {}
    if (sstate === "ready") maybePrimeLlamaTools();
    setPill(sstate === "ready" && state.toolPrimeStatus !== "warming" ? "on" : "loading");
    // Refresh model list when the API is ready (throttled; GET /v1/models).
    if (sstate === "ready") {
      const now = Date.now();
      if (!state._modelsAt || now - state._modelsAt > 8000 || !state.chatModelsLive) {
        state._modelsAt = now;
        refreshChatModels();
      }
    }
  } else {
    resetLlamaToolPrime();
    setPill("off");
    state.lastMisses = null;
    state.kvMiB = null;
    ARENA.active = false;
    if (state.resolvedBackend) {
      state.resolvedBackend = null;
      applyBackendUi();
    }
    if (state.chatModelsLive) {
      state.chatModelsLive = false;
      state.chatModels = [];
      refreshChatModels(); // falls back to slipstream / mlx catalog id
    }
  }

  drawAll();
  refreshModel();
}

// ---- coding agents (one-click connect) ------------------------------------
const BASE_URL = "http://127.0.0.1:8080/v1";
const EMBEDDINGS_BASE_URL = "http://127.0.0.1:8090/v1";
/** Model id for agent snippets — live Chat selection, not a hardcoded alias. */
function agentModelId() {
  return state.chatModel || chatFallbackModelId() || "slipstream";
}
/** Responses API URL from OpenAI chat base (never doubles /v1). */
function responsesUrl(base) {
  const b = String(base == null ? BASE_URL : base);
  return b.replace(/\/v1\/?$/, "") + "/v1/responses";
}
/** Anthropic Messages URL from OpenAI chat base. */
function messagesUrl(base) {
  const b = String(base == null ? BASE_URL : base);
  return b.replace(/\/v1\/?$/, "") + "/v1/messages";
}
/** Indexing embedder note — not the chat base. */
function embeddingsNote() {
  return `Embeddings (indexing): ${EMBEDDINGS_BASE_URL}  model nomic-embed-text`;
}
/** Cold-prefill timeout hint for agent paste configs. */
function agentTimeoutNote() {
  return "Read timeout: 300s  # cold PGRN prefill";
}
/** Shared footer for copy snippets (timeout + embeddings + usage tip). */
function agentSnippetFooter() {
  return `${agentTimeoutNote()}\n${embeddingsNote()}\n# Optional: stream_options.include_usage=true`;
}
/** OpenAI-compatible paste block (Cline / Roo / Cursor). */
function openaiCompatSnippet() {
  return `Base URL: ${BASE_URL}\nModel: ${agentModelId()}\nAPI Key: sk-local\n${agentSnippetFooter()}`;
}
const AGENTS = [
  { id: "kilo", name: "Kilo Code", tag: "1-Klick-Patch · VS Code neu starten", action: "patch", target: "kilo", restart: "VS Code" },
  { id: "opencode", name: "OpenCode", tag: "1-Klick-Patch · OpenCode neu starten", action: "patch", target: "opencode", restart: "OpenCode" },
  { id: "cline", name: "Cline", tag: "Werte kopieren -> OpenAI-Compatible-Provider", action: "copy",
    how: "how.cline",
    snippet: () => openaiCompatSnippet() },
  { id: "roo", name: "Roo Code", tag: "Werte kopieren -> OpenAI-Compatible-Provider", action: "copy",
    how: "how.roo",
    snippet: () => openaiCompatSnippet() },
  { id: "cursor", name: "Cursor", tag: "Werte kopieren -> Override OpenAI Base URL", action: "copy",
    how: "how.cursor",
    snippet: () => openaiCompatSnippet() },
  { id: "continue", name: "Continue", tag: "config.yaml-Snippet kopieren", action: "copy",
    how: "how.continue",
    snippet: () => `models:\n  - name: Slipstream Local\n    provider: openai\n    model: ${agentModelId()}\n    apiBase: ${BASE_URL}\n    apiKey: sk-local\n    requestOptions:\n      timeoutMs: 300000\n# ${embeddingsNote()}` },
  { id: "aider", name: "aider", tag: ".aider.conf.yml-Snippet kopieren", action: "copy",
    how: "how.aider",
    snippet: () => `openai-api-base: ${BASE_URL}\nopenai-api-key: sk-local\nmodel: openai/${agentModelId()}\n# Raise timeouts for cold PGRN prefills (300s+)\n# ${embeddingsNote()}` },
  { id: "codex", name: "Codex / Responses", tag: "Werte kopieren -> Responses API", action: "copy",
    how: "how.codex",
    snippet: () => `Responses URL: ${responsesUrl()}\nChat base (fallback): ${BASE_URL}\nModel: ${agentModelId()}\nAPI Key: sk-local\n${agentTimeoutNote()}\n# Example: POST {model,input:[{role:"user",content:"hi"}]}` },
  { id: "anthropic", name: "Anthropic / Messages", tag: "Werte kopieren -> Messages API", action: "copy",
    how: "how.anthropic",
    snippet: () => `Messages URL: ${messagesUrl()}\nChat base (fallback): ${BASE_URL}\nModel: ${agentModelId()}\nAPI Key: sk-local\n${agentTimeoutNote()}\n# Example: POST {model,max_tokens:256,messages:[{role:"user",content:"hi"}]}` },
];

function renderAgents() {
  const host = $("agentList");
  if (!host) return;
  host.innerHTML = "";
  AGENTS.forEach((a) => {
    const el = document.createElement("div");
    el.className = "agent";
    el.innerHTML = `<div class="agent-info"><div class="agent-name">${a.name}</div><div class="agent-tag">${t(a.action === "patch" ? "agent.tagPatch" : "agent.tagCopy")}</div></div>`;
    const btn = document.createElement("button");
    btn.className = "btn " + (a.action === "patch" ? "btn-primary" : "btn-ghost");
    btn.textContent = a.action === "patch" ? t("agent.patch") : t("agent.copy");
    btn.onclick = () => doAgent(a);
    el.appendChild(btn);
    host.appendChild(el);
  });
}
async function doAgent(a) {
  if (a.action === "patch") {
    try {
      const path = await invoke("patch_kilo_config", {
        cfg: { home: state.def ? state.def.home : "", base_url: BASE_URL, model: agentModelId(),
               api_key: "sk-local", ctx: +$("ctx").value, max_out: 4096, target: a.target },
      });
      $("agentNote").innerHTML = `<b>${a.name}</b>: gepatcht -> <code>${path}</code> &middot; ${a.restart} neu starten.`;
      toast(`In ${a.name} gepatcht - ${a.restart} neu starten`);
    } catch (e) { toast(String(e), true); }
  } else {
    try {
      await navigator.clipboard.writeText(a.snippet());
      $("agentNote").innerHTML = `<b>${a.name}</b>: ${t("toast.copied")}. ${t(a.how)}`;
      toast(`${a.name}-Config kopiert`);
    } catch { toast(t("toast.copyFail"), true); }
  }
}

// ---- indexing (embedder + qdrant) -----------------------------------------
function embModel() { return `${state.def.model_dir}/embed/${state.def.embed_file}`; }
function embDir() { return `${state.def.model_dir}/embed`; }
function qDir() { return `${state.def.home}/.peregrine/qdrant`; }
function qBin() { return `${qDir()}/qdrant`; }

const EMB_BYTES = 99588928; // nomic Q5_K_M size, for download progress
let idxBusy = false;        // true while the one-click setup runs

async function refreshIndex() {
  if (!state.def) return;
  let st;
  try { st = await invoke("index_status", { embModel: embModel(), qdrantBin: qBin() }); }
  catch { return; }

  const embUp = st.emb_state === "ready", embLoad = st.emb_state === "loading";
  const embDl = st.emb_bytes > 0 && st.emb_bytes < EMB_BYTES * 0.99;
  $("embStatus").textContent = embUp ? t("st.embRunning") : embLoad ? t("st.embStart") : embDl ? `${t("st.embDl2")} ${(st.emb_bytes / 1048576).toFixed(0)} MB` : st.emb_bytes > 0 ? t("st.embLoaded") : t("st.embNone");
  $("embBar").style.width = embUp ? "100%" : st.emb_bytes > 0
    ? Math.min(95, (st.emb_bytes / EMB_BYTES) * 100).toFixed(0) + "%" : "0%";

  $("qStatus").textContent = st.installing ? t("st.qInstalling") : st.qdrant_running ? t("st.qRunning") : st.qdrant_installed ? t("st.qInstalled") : t("st.qNone");
  $("qBar").style.width = st.qdrant_installed ? (st.qdrant_running ? "100%" : "60%") : (st.installing ? "40%" : "0%");

  const b = $("idxBadge"), both = embUp && st.qdrant_running;
  if (both) { b.className = "badge ready"; b.textContent = t("badge.ready"); }
  else if (st.emb_bytes > 0 || st.qdrant_installed) { b.className = "badge partial"; b.textContent = t("badge.partial"); }
  else { b.className = "badge missing"; b.textContent = t("badge.missing"); }

  if (!idxBusy) {
    const setup = $("idxSetup");
    setup.disabled = false;
    setup.textContent = both ? t("idx.stop") : t("idx.setup");
    setup.className = "btn " + (both ? "btn-danger" : "btn-primary");
    setup.dataset.mode = both ? "stop" : "start";
  }
}

const idxStatus = () => invoke("index_status", { embModel: embModel(), qdrantBin: qBin() });
const idxStep = (t) => { $("idxStep").textContent = t; };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitUntil(cond, tries) {
  for (let i = 0; i < tries; i++) { try { if (await cond()) return true; } catch {} await sleep(1000); }
  return false;
}

async function setupIndexing() {
  idxBusy = true;
  const btn = $("idxSetup");
  btn.disabled = true; btn.textContent = t("idx.running");
  try {
    let st = await idxStatus();
    if (st.emb_bytes < EMB_BYTES * 0.99) {
      idxStep(t("idx.step1"));
      await invoke("start_download", { url: state.def.embed_url, dest: embModel(), dir: embDir() });
      if (!(await waitUntil(async () => (await idxStatus()).emb_bytes >= EMB_BYTES * 0.99, 300)))
        throw t("idx.dlTimeout");
    }
    st = await idxStatus();
    if (!st.qdrant_installed) {
      idxStep(t("idx.step2"));
      await invoke("install_qdrant", { dir: qDir() });
      if (!(await waitUntil(async () => (await idxStatus()).qdrant_installed, 180)))
        throw t("idx.installTimeout");
    }
    st = await idxStatus();
    if (st.emb_state === "down") {
      idxStep(t("idx.step3emb"));
      await invoke("start_embedder", { cfg: { server: $("pServer").value.trim(), model: embModel() } });
    }
    if (!st.qdrant_running) {
      idxStep(t("idx.step3q"));
      await invoke("start_qdrant", { bin: qBin(), storage: `${qDir()}/storage` });
    }
    idxStep(t("idx.step4"));
    await waitUntil(async () => { const s = await idxStatus(); return s.emb_state === "ready" && s.qdrant_running; }, 60);
    await invoke("patch_kilo_config", {
      cfg: { home: state.def.home, base_url: "http://127.0.0.1:8080/v1", model: "slipstream", api_key: "sk-local", ctx: +$("ctx").value, max_out: 4096 },
    });
    idxStep(t("idx.doneStep"));
    toast(t("idx.doneToast"));
  } catch (e) { idxStep(""); toast(String(e), true); }
  idxBusy = false;
  refreshIndex();
}

async function stopIndexing() {
  try { await invoke("stop_embedder"); await invoke("stop_qdrant"); idxStep(""); toast(t("toast.idxStopped")); }
  catch (e) { toast(e, true); }
  refreshIndex();
}

$("idxSetup").onclick = () => ($("idxSetup").dataset.mode === "stop" ? stopIndexing() : setupIndexing());

// ---- test panel ------------------------------------------------------------
$("sendBtn").onclick = async () => {
  const prompt = $("prompt").value.trim();
  if (!prompt) return;
  const btn = $("sendBtn"); btn.disabled = true; btn.textContent = "…";
  $("answer").textContent = ""; $("reasoning").textContent = "";
  $("reasoningWrap").style.display = "none"; $("testMeta").textContent = "sende…";
  const t0 = performance.now();
  const messages = [];
  if ($("noThink").checked) messages.push({ role: "system", content: "/no_think" });
  messages.push({ role: "user", content: prompt });
  try {
    const r = await fetch(`http://127.0.0.1:${PORT}/v1/chat/completions`, {
      method: "POST", headers: { "Content-Type": "application/json", Authorization: "Bearer sk-local" },
      body: JSON.stringify({ model: "slipstream", messages, max_tokens: 1024, temperature: 0 }),
    });
    const j = await r.json();
    const msg = (j.choices && j.choices[0] && j.choices[0].message) || {};
    if (msg.reasoning_content) { $("reasoning").textContent = msg.reasoning_content; $("reasoningWrap").style.display = "block"; }
    $("answer").textContent = msg.content || JSON.stringify(j, null, 2);
    const dt = (performance.now() - t0) / 1000;
    const tok = j.usage ? j.usage.completion_tokens : 0;
    $("testMeta").textContent = `${dec(dt, 1)} s · ${tok} tokens · ${tps(tok / dt)}`;
  } catch (e) {
    $("answer").textContent = t("test.fail");
    $("testMeta").textContent = "";
  }
  btn.disabled = false; btn.textContent = t("btn.send");
};

// ---- benchmark -------------------------------------------------------------
// Measures the running server through llama.cpp's own /completion timings
// (prompt_per_second = prefill, predicted_per_second = decode) instead of
// wall-clock guessing. Prompt cache is disabled and each run gets a distinct
// prompt, so prefill is really re-done every time. Run 1 is the cold one — the
// expert cache fills during it; the warm runs are what a coding session feels.
const BENCH_UNIT =
  "Note %n: fn fetch_expert(layer: usize, idx: usize) -> Result<Slot, Error> { " +
  "let slot = arena.reserve(layer)?; io.read_at(slot.buf_mut(), dir.offset(layer, idx))?; Ok(slot) } ";
function benchPrompt(run) {
  let s = `Benchmark run ${run}. Read these review notes and summarise them in three sentences.\n`;
  for (let i = 0; i < 90; i++) s += BENCH_UNIT.replace("%n", String(run * 1000 + i));
  return s;
}
async function runBenchmark() {
  const btn = $("benchRun"), out = $("benchOut"), badge = $("benchBadge");
  if (!btn || !out) return;
  if (!state.running) { toast(t("bench.needServer"), true); return; }
  const nTok = +$("benchTokens").value || 300;
  const runs = +$("benchRuns").value || 2;
  btn.disabled = true; btn.textContent = t("bench.running");
  const rows = [];
  try {
    for (let i = 1; i <= runs; i++) {
      badge.className = "badge partial"; badge.textContent = `${i}/${runs}`;
      out.innerHTML = `<div class="bench-note bench-run">${t("bench.running")} ${i}/${runs}</div>`
        + renderBenchTable(rows, runs);
      const res = await fetch(`http://127.0.0.1:${PORT}/completion`, {
        method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          prompt: benchPrompt(i), n_predict: nTok, temperature: 0, seed: 42,
          cache_prompt: false, stream: false,
        }),
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const j = await res.json();
      const tm = j.timings || {};
      rows.push({
        run: i,
        prefill: tm.prompt_per_second || null,
        decode: tm.predicted_per_second || null,
        pTok: tm.prompt_n || 0,
        dTok: tm.predicted_n || 0,
        hit: state.hit.length ? state.hit[state.hit.length - 1] : null,
      });
      out.innerHTML = renderBenchTable(rows, runs);
    }
    state.bench = rows;
    badge.className = "badge ready"; badge.textContent = t("badge.ready");
    out.innerHTML = renderBenchTable(rows, runs) + `<div class="bench-note">${t("bench.note")}</div>`;
  } catch (e) {
    badge.className = "badge missing"; badge.textContent = t("conv.error");
    out.innerHTML = renderBenchTable(rows, runs)
      + `<div class="bench-note" style="color:#ff9d92">${t("bench.failed")}: ${String(e)}</div>`;
  }
  btn.disabled = false; btn.textContent = t("bench.run");
}
function renderBenchTable(rows, runs) {
  if (!rows.length) return "";
  const num = (v, d) => (v == null ? "–" : dec(v, d));
  let html = `<table class="bench-table"><tr>` +
    `<th>${t("bench.col.run")}</th><th>${t("bench.col.prefill")}</th>` +
    `<th>${t("bench.col.decode")}</th><th>${t("bench.col.hit")}</th><th>${t("bench.col.tokens")}</th></tr>`;
  rows.forEach((r) => {
    html += `<tr><td>${r.run}${r.run === 1 && runs > 1 ? " (cold)" : ""}</td>` +
      `<td>${num(r.prefill, 0)} tok/s</td><td>${num(r.decode, 2)} tok/s</td>` +
      `<td>${r.hit == null ? "–" : dec(r.hit, 1) + " %"}</td>` +
      `<td>${r.pTok} + ${r.dTok}</td></tr>`;
  });
  // Mean over the warm runs only — averaging the cold run in would understate it.
  const warm = runs > 1 && rows.length > 1 ? rows.slice(1) : rows;
  const mean = (k) => {
    const vals = warm.map((r) => r[k]).filter((v) => v != null);
    return vals.length ? vals.reduce((a, b) => a + b, 0) / vals.length : null;
  };
  html += `<tr class="bench-mean"><td>${t("bench.mean")}${warm.length !== rows.length ? " (warm)" : ""}</td>` +
    `<td>${num(mean("prefill"), 0)} tok/s</td><td>${num(mean("decode"), 2)} tok/s</td><td></td><td></td></tr>`;
  return html + `</table>`;
}
if ($("benchRun")) $("benchRun").onclick = runBenchmark;

// ---- logs ------------------------------------------------------------------
const LOG_FILES = {
  download: "/tmp/peregrine-download.log",
  convert: "/tmp/peregrine-convert.log",
};
async function refreshLogs() {
  const src = $("logSrc").value;
  let txt = "";
  try {
    if (src === "server") txt = await invoke("read_log", { maxLines: 300 });
    else txt = await invoke("tail_file", { path: LOG_FILES[src], maxLines: 300 });
  } catch { txt = t("logs.none"); }
  const el = $("logs");
  const atBottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 30;
  el.textContent = txt || t("logs.empty");
  if ($("autoscroll").checked && atBottom) el.scrollTop = el.scrollHeight;
}
$("diagBtn").onclick = async () => {
  try {
    // The same reading the live view drew, so a pasted report and a screenshot of
    // the window cannot show two different machines.
    const live = await invoke("live_stats");
    const memory = live.system.memory;
    const log = await invoke("read_log", { maxLines: 60 });
    const diag = `Peregrine Control - Diagnose
Modell: ${state.model.name}
Pfad: ${ggufPath()}
Server-Binary: ${$("pServer").value}
Cache: ${$("cache").value} GiB · Ctx: ${$("ctx").value} · IO: ${$("io").value}
RAM frei: ${giB(memory.free_bytes)} von ${giB(memory.total_bytes)} · Swap: ${giB(memory.swap_used_bytes)}
Experten-Cache: ${live.experts ? dec(live.experts.hit_rate, 1) + " % Treffer, " + live.experts.misses + " von SSD" : "kein Streaming-Lauf"}
Laeuft: ${state.running}
--- letzte Server-Logs ---
${log}`;
    await navigator.clipboard.writeText(diag);
    toast(t("toast.diagCopied"));
  } catch (e) { toast(e, true); }
};

// ---- live view -------------------------------------------------------------
// Four sections, one reading each: serving from /metrics, streaming from the
// engine log, memory and system from the host sampler. It draws what the menubar
// poller already collected, so a figure here and the same figure in the menu are
// one reading rather than two samples seconds apart. The panels this replaced
// showed decode speed in four places from two chains; that is what merging them
// was for, so nothing here may draw the same quantity twice.
const pctOf = (fraction) => (fraction == null ? "–" : `${Math.round(fraction * 100)} %`);

function setMeter(id, fraction) {
  $(id).style.width = `${Math.min(1, Math.max(0, fraction || 0)) * 100}%`;
}

function renderScope() {
  $("scopeSession").classList.toggle("seg-on", statsScope === "session");
  $("scopeAlltime").classList.toggle("seg-on", statsScope === "alltime");
  $("stScopeNote").textContent = t(statsScope === "alltime" ? "scope.alltimeNote" : "scope.sessionNote");
}

function statusVisible() {
  const panel = document.querySelector('.panel[data-panel="status"]');
  return panel && !panel.hidden;
}

async function refreshStatus() {
  let live;
  try { live = await invoke("live_stats"); } catch { return; }
  absorbLive(live);
  // Drawing while the view is hidden is the only part worth skipping; the
  // history and the plan above must keep up either way.
  if (statusVisible()) renderLive(live);
}

// Everything that has to keep running with the view closed: the sparklines' own
// memory, and the planner's inputs. The planner used to read a second memory
// source (a `vm_stat` subprocess on its own timer) and could therefore contradict
// the bar it sits next to; it takes this tick now.
function absorbLive(live) {
  const cache = live.experts;
  const now = performance.now() / 1000;
  let ssd = 0;
  if (cache && state.lastMisses != null && state.lastT != null) {
    const grown = cache.misses - state.lastMisses, seconds = now - state.lastT;
    if (grown >= 0 && seconds > 0) ssd = (grown * EXPERT_MIB) / seconds;
  }
  state.lastMisses = cache ? cache.misses : null;
  state.lastT = now;
  state.ssdNow = ssd;
  push(state.ssd, Math.max(0, ssd));
  if (cache) push(state.hit, cache.hit_rate);
  ARENA.hit = cache ? cache.hit_rate : 0;
  ARENA.ssd = ssd;

  const memory = live.system.memory;
  if (memory.total_bytes) {
    state.sys = {
      ...(state.sys || {}),
      free_gib: memory.free_bytes / 1073741824,
      total_gib: memory.total_bytes / 1073741824,
      swap_used_mb: memory.swap_used_bytes / 1048576,
    };
    updateReco();
    renderMemPlan();
  }
  // Topbar strip must update even when the Live panel is closed.
  renderObsStrip(live);
}

/** Fields for the always-visible topbar strip. Pure — mirrored in scripts/test_obs_strip.mjs. */
function pickObsStrip(live, running) {
  if (!live || !running) {
    return { tps: null, cachePct: null, rssBytes: null, rssSource: null };
  }
  let tpsVal = null;
  if (live.last_tps != null && live.last_tps > 0) tpsVal = live.last_tps;
  else if (live.serving_available && live.session && live.session.avg_decode_tps > 0) {
    tpsVal = live.session.avg_decode_tps;
  }
  let cachePct = null;
  if (live.experts && live.experts.hit_rate != null) cachePct = live.experts.hit_rate;
  else if (live.serving_available && live.session && (live.session.cached_tokens > 0 || live.session.total_tokens > 0)) {
    cachePct = live.session.cache_efficiency;
  }
  // Treat non-positive rss_bytes as missing so process/model fallbacks still work.
  const rssBytes = (live.rss_bytes != null && live.rss_bytes > 0)
    ? live.rss_bytes
    : (live.process_rss_bytes || live.model_memory_bytes || null);
  let rssSource = live.rss_source || null;
  if (!rssSource && rssBytes != null) {
    rssSource = live.process_rss_bytes ? "process" : "model_memory";
  }
  return { tps: tpsVal, cachePct, rssBytes, rssSource };
}

function formatObsTps(v) {
  if (v == null || !Number.isFinite(v) || !(v > 0)) return "–";
  return v < 10 ? dec(v, 1) : String(Math.round(v));
}
function formatObsCache(v) {
  if (v == null || !Number.isFinite(v)) return "–";
  return `${dec(v, 0)}%`;
}
function formatObsRss(bytes) {
  if (bytes == null || !Number.isFinite(bytes) || !(bytes > 0)) return "–";
  const gib = bytes / 1073741824;
  return gib >= 10 ? `${Math.round(gib)} GiB` : `${dec(gib, 1)} GiB`;
}

function formatObsCfg() {
  const c = $("cache") ? $("cache").value : null;
  const io = $("io") ? $("io").value : null;
  if (c == null || io == null) return "–";
  return `${c}/${io}/3`;
}
function renderObsStrip(live) {
  const pick = pickObsStrip(live, state.running);
  const paint = (id, text) => {
    const el = $(id);
    if (!el) return;
    el.textContent = text;
    el.classList.toggle("obs-empty", text === "–");
  };
  paint("obsTps", formatObsTps(pick.tps));
  paint("obsCache", formatObsCache(pick.cachePct));
  paint("obsRss", formatObsRss(pick.rssBytes));
  paint("obsCfg", formatObsCfg());
  const strip = $("obsStrip");
  if (strip) {
    const cfg = formatObsCfg();
    strip.title = (state.running ? t("obs.upTip") : t("obs.downTip")) + (cfg !== "–" ? ` · ${cfg}` : "");
  }
}

function renderLive(live) {
  const s = statsScope === "alltime" ? live.alltime : live.session;
  $("stTokens").textContent = s.total_tokens.toLocaleString(LANG);
  $("stCached").textContent = s.cached_tokens.toLocaleString(LANG);
  $("stEff").textContent = dec(s.cache_efficiency, 1);
  setMeter("stEffBar", s.cache_efficiency / 100);
  $("stRequests").textContent = s.requests.toLocaleString(LANG);
  $("stRequestNote").textContent = t("stats.requestNote");
  // Last decode (Metal/oMLX log / MLX avg) vs session averages from /metrics or /api/status.
  const lastEl = $("stLastTps");
  if (lastEl) lastEl.textContent = live.last_tps != null ? tps(live.last_tps) : "–";
  $("stPrefill").textContent = tps(s.avg_prefill_tps);
  $("stDecode").textContent = tps(s.avg_decode_tps);
  $("activeModelNote").textContent = state.model?.name || t("srv.noModel");
  const rssEl = $("stRss");
  if (rssEl) {
    const rss = live.rss_bytes || live.process_rss_bytes || live.model_memory_bytes;
    rssEl.textContent = rss ? giB(rss) : "–";
  }
  const hwEl = $("stPgrnHw");
  if (hwEl) {
    hwEl.textContent = live.pgrn_high_water_bytes ? giB(live.pgrn_high_water_bytes) : "–";
  }
  $("stNoServer").hidden = live.serving_available;
  renderObsStrip(live);
  renderScope();

  // 2 — streaming. Both figures come from one parse of the engine log: the rate
  // as printed, the throughput from how fast misses grow between two readings.
  const cache = live.experts;
  $("stExperts").textContent = cache ? `${dec(cache.hit_rate, 1)} %` : "–";
  $("stSsdNow").textContent = dec(state.ssdNow || 0, 0);
  $("stStreamState").textContent = cache
    ? `${cache.misses.toLocaleString(LANG)} ${t("stream.fetched")}`
    : t("stats.noExperts");

  const sys = live.system;
  $("stECores").textContent = pctOf(sys.e_core_usage);
  setMeter("stECoreBar", sys.e_core_usage);
  $("stPCores").textContent = pctOf(sys.p_core_usage);
  setMeter("stPCoreBar", sys.p_core_usage);
  $("stGpu").textContent = pctOf(sys.gpu_usage);
  setMeter("stGpuBar", sys.gpu_usage);
  $("stGpuMem").textContent = sys.gpu_memory_bytes == null ? "–" : giB(sys.gpu_memory_bytes);

  const m = sys.memory;
  const used = m.total_bytes - m.free_bytes;
  $("stMemUsed").textContent = giB(used);
  $("stMemTotal").textContent = `${Math.round(m.total_bytes / 1073741824).toLocaleString(LANG)} GiB`;
  $("stMemPct").textContent = m.total_bytes ? `${Math.round(used / m.total_bytes * 100)} %` : "–";
  const share = (bytes) => (m.total_bytes ? `${bytes / m.total_bytes * 100}%` : "0%");
  $("stMemWired").style.width = share(m.wired_bytes);
  $("stMemActive").style.width = share(m.active_bytes);
  $("stMemComp").style.width = share(m.compressed_bytes);
  $("stMemFree").style.width = share(m.free_bytes);
  $("stWired").textContent = giB(m.wired_bytes);
  $("stActive").textContent = giB(m.active_bytes);
  $("stCompressed").textContent = giB(m.compressed_bytes);
  $("stFree").textContent = giB(m.free_bytes);
  $("stSwap").textContent = m.swap_used_bytes ? giB(m.swap_used_bytes) : t("host.noSwap");

  // Same thresholds the memory panel used, now fed by the tick that drew the bar
  // above rather than by a second reading taken at another moment.
  const freePct = m.total_bytes ? (m.free_bytes / m.total_bytes) * 100 : 0;
  const swapMb = m.swap_used_bytes / 1048576;
  let lamp = "green", verdict = "amp.smooth";
  if (swapMb > 800 || freePct < 12) { lamp = "red"; verdict = "amp.pressure"; }
  else if (swapMb > 150 || freePct < 22) { lamp = "yellow"; verdict = "amp.borderline"; }
  $("ampel").className = "ampel ampel-" + lamp;
  $("ampelText").textContent = t(verdict);

  const thermal = $("stThermal");
  thermal.textContent = t(`thermal.${sys.thermal}`);
  thermal.className = "val " + (sys.thermal === "Nominal" ? "ampel-green"
    : sys.thermal === "Critical" ? "ampel-red" : "ampel-yellow");
  $("stLoad").textContent = sys.load_average.length
    ? sys.load_average.map((v) => dec(v, 2)).join(" · ") : "–";
  const hours = Math.floor(sys.uptime_seconds / 3600);
  $("stUptime").textContent = hours >= 24
    ? `${Math.floor(hours / 24)} ${t("unit.days")} ${hours % 24} ${t("unit.hours")}`
    : `${hours} ${t("unit.hours")} ${Math.floor((sys.uptime_seconds % 3600) / 60)} ${t("unit.minutes")}`;
}

function initStatus() {
  document.querySelectorAll(".seg button[data-scope]").forEach((btn) => {
    btn.onclick = () => {
      statsScope = btn.dataset.scope;
      localStorage.setItem("slipstream.statsScope", statsScope);
      hideStatsConfirm();
      renderScope();
      refreshStatus();
    };
  });
  // Resetting throws away numbers, so it asks first — the same two-step the
  // reference dashboard uses.
  $("statsClear").onclick = () => {
    $("statsClear").hidden = true;
    $("statsConfirm").hidden = false;
  };
  $("statsNo").onclick = hideStatsConfirm;
  $("statsYes").onclick = async () => {
    hideStatsConfirm();
    try {
      await invoke("clear_stats", { scope: statsScope });
      refreshStatus();
    } catch (e) { toast(e, true); }
  };
  initAnchors();
  renderScope();
}

// The four sections are one scroll, so the rail both jumps and follows. Without
// the second half it would highlight Serving while you read System.
function initAnchors() {
  const rail = $("statusAnchors");
  const scroller = document.querySelector(".content");
  const buttons = [...rail.querySelectorAll(".anchor")];
  buttons.forEach((button) => {
    button.onclick = () => {
      const target = document.getElementById(button.dataset.anchor);
      if (target) target.scrollIntoView({ behavior: "smooth", block: "start" });
    };
  });
  const mark = () => {
    if (!statusVisible()) return;
    // The section whose top has last passed under the rail is the one being read.
    const line = rail.getBoundingClientRect().bottom + 8;
    let current = buttons[0];
    buttons.forEach((button) => {
      const section = document.getElementById(button.dataset.anchor);
      if (section && section.getBoundingClientRect().top <= line) current = button;
    });
    // At the end of the scroll the last section is on screen but its top never
    // passed the rail, so by the rule above it would never light up.
    const atBottom = scroller
      ? scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 4
      : window.innerHeight + window.scrollY >= document.body.scrollHeight - 4;
    if (atBottom) current = buttons[buttons.length - 1];
    buttons.forEach((button) => button.classList.toggle("anchor-on", button === current));
  };
  (scroller || window).addEventListener("scroll", mark, { passive: true });
  mark();
}

function hideStatsConfirm() {
  $("statsConfirm").hidden = true;
  $("statsClear").hidden = false;
}

// ---- chat (streaming against the local OpenAI-compatible server) -----------
const chat = { history: [], streaming: false, abort: null };

/** Read a common preference, falling back to the pre-parity MLX-only key. */
function migratedChatPreference(key, legacyKey) {
  const current = localStorage.getItem(key);
  return (current == null ? localStorage.getItem(legacyKey) : current) === "1";
}

function chatToolsPreference() {
  return migratedChatPreference("slipstream.chatTools", "slipstream.mlxTools");
}

function chatJsonPreference() {
  return migratedChatPreference("slipstream.chatJson", "slipstream.mlxJson");
}

/** Persist + sync the common Settings ↔ Chat toolbar tool preference. */
function syncChatToolsUi(on) {
  const enabled = !!on;
  localStorage.setItem("slipstream.chatTools", enabled ? "1" : "0");
  if ($("settingsChatTools")) $("settingsChatTools").checked = enabled;
  if ($("chatTools")) $("chatTools").checked = enabled;
}

function syncChatJsonUi(on) {
  const enabled = !!on;
  localStorage.setItem("slipstream.chatJson", enabled ? "1" : "0");
  if ($("chatJson")) $("chatJson").checked = enabled;
  updateChatSchemaVisibility();
}

/** Sanitize OpenAI json_schema.name (alphanumeric / _ / -). */
function schemaNameFrom(value, fallback) {
  const s = String(value || fallback || "response")
    .replace(/[^a-zA-Z0-9_-]/g, "_")
    .replace(/^_+|_+$/g, "")
    .slice(0, 64);
  return s || "response";
}

/**
 * Parse pasted schema text into an OpenAI-style json_schema wrapper.
 * Accepts: raw JSON Schema, {name,schema,strict?}, or full response_format.
 * Pure — unit-tested via scripts/test_schema_paste.mjs.
 */
function parseSchemaPaste(text) {
  const raw = String(text || "").trim();
  if (!raw) return { ok: true, empty: true };
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    return { ok: false, error: (e && e.message) || "invalid JSON" };
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    return { ok: false, error: "schema must be a JSON object" };
  }
  if (parsed.type === "json_object" && !parsed.json_schema) {
    return { ok: true, empty: true };
  }
  let wrap = parsed;
  if (parsed.type === "json_schema" && parsed.json_schema && typeof parsed.json_schema === "object") {
    wrap = parsed.json_schema;
  }
  if (wrap.schema && typeof wrap.schema === "object" && !Array.isArray(wrap.schema)) {
    const out = {
      name: schemaNameFrom(wrap.name, "response"),
      schema: wrap.schema,
    };
    if (wrap.strict === false) out.strict = false;
    else out.strict = true;
    return { ok: true, json_schema: out };
  }
  // Raw JSON Schema document.
  return {
    ok: true,
    json_schema: {
      name: schemaNameFrom(wrap.title || wrap.$id || wrap.name, "response"),
      schema: wrap,
      strict: true,
    },
  };
}

/**
 * Build response_format for oMLX chat completions when JSON mode is on.
 * Empty paste → json_object; valid schema → json_schema; invalid → { error }.
 */
function buildResponseFormat(jsonEnabled, schemaText) {
  if (!jsonEnabled) return null;
  const parsed = parseSchemaPaste(schemaText);
  if (!parsed.ok) return { error: parsed.error };
  if (parsed.empty) return { response_format: { type: "json_object" } };
  return {
    response_format: {
      type: "json_schema",
      json_schema: parsed.json_schema,
    },
  };
}

function chatSchemaText() {
  if ($("chatSchema")) return $("chatSchema").value;
  return localStorage.getItem("slipstream.chatJsonSchema")
    || localStorage.getItem("slipstream.mlxJsonSchema")
    || "";
}

function persistChatSchema(text) {
  localStorage.setItem("slipstream.chatJsonSchema", String(text || ""));
}

function updateChatSchemaStatus() {
  const ta = $("chatSchema");
  const st = $("chatSchemaStatus");
  if (!ta || !st) return;
  const parsed = parseSchemaPaste(ta.value);
  ta.classList.toggle("schema-bad", !parsed.ok);
  st.classList.remove("is-err", "is-ok");
  if (!parsed.ok) {
    st.textContent = `${t("chat.schemaBad")}: ${parsed.error}`;
    st.classList.add("is-err");
  } else if (parsed.empty) {
    st.textContent = t("chat.schemaEmpty");
    st.classList.add("is-ok");
  } else {
    st.textContent = `${t("chat.schemaOk")} · ${parsed.json_schema.name}`;
    st.classList.add("is-ok");
  }
}

/** Show schema paste when the common JSON contract is enabled.
 *  Collapsed by default so the composer stays calm until the user opens it. */
function updateChatSchemaVisibility() {
  const wrap = $("chatSchemaWrap");
  if (!wrap) return;
  const jsonOn = !!($("chatJson") && $("chatJson").checked);
  wrap.hidden = !jsonOn;
  if (!jsonOn) {
    if (wrap instanceof HTMLDetailsElement) wrap.open = false;
    return;
  }
  const ta = $("chatSchema");
  if (ta && !ta.dataset.hydrated) {
    ta.value = localStorage.getItem("slipstream.chatJsonSchema")
      || localStorage.getItem("slipstream.mlxJsonSchema")
      || "";
    ta.dataset.hydrated = "1";
  }
  updateChatSchemaStatus();
}

/** Heuristic: user is asking for a tool-backed answer (time / calc / "use tools"). */
function messageAsksForTools(text) {
  const s = String(text || "").toLowerCase();
  if (!s.trim()) return false;
  return /\b(use\s+tools?|tool\s*call|function\s*call|get_current_time|calculator|what\s+time|current\s+time|uhrzeit|rechner|berechne|calculate|compute)\b/i.test(s)
    || /[0-9]+\s*[\+\-\*\/×÷]\s*[0-9]+/.test(s);
}

function toolsEnabledForRequest(userText) {
  const checked = !!(
    ($("chatTools") && $("chatTools").checked)
    || ($("settingsChatTools") && $("settingsChatTools").checked)
    || chatToolsPreference()
  );
  if (checked) return true;
  // Soft path: if the user clearly asks, attach the common local demo tools.
  return messageAsksForTools(userText);
}

/** Local demo tools for oMLX tool-calling roundtrips (no MCP required). */
const DEMO_TOOLS = [
  {
    type: "function",
    function: {
      name: "get_current_time",
      description: "Return the current local date and time as an ISO-8601 string.",
      parameters: { type: "object", properties: {}, additionalProperties: false },
    },
  },
  {
    type: "function",
    function: {
      name: "calculator",
      description: "Evaluate a simple arithmetic expression (numbers and + - * / parentheses).",
      parameters: {
        type: "object",
        properties: {
          expression: { type: "string", description: "e.g. (2+3)*4" },
        },
        required: ["expression"],
      },
    },
  },
];

function renderLlamaToolPrime() {
  const badge = $("chatToolsPrime");
  if (!badge) return;
  const status = state.toolPrimeStatus || "idle";
  const applicable = chatToolsPreference() && effectiveBackend() !== "mlx" && status !== "idle";
  badge.hidden = !applicable;
  badge.classList.toggle("is-ready", status === "ready");
  badge.classList.toggle("is-failed", status === "failed");
  badge.textContent = status === "warming"
    ? t("chat.toolsPriming")
    : status === "ready"
      ? t("chat.toolsPrimed")
      : status === "failed" ? t("chat.toolsPrimeFailed") : "";
}

function resetLlamaToolPrime() {
  if (state.toolPrimeController) state.toolPrimeController.abort();
  state.toolPrimeController = null;
  state.toolPrimePromise = null;
  state.toolPrimeStatus = "idle";
  renderLlamaToolPrime();
}

/**
 * Prime only Slipstream's fixed llama.cpp tool prefix. A real Qwen/PGRN gate
 * measured 23.36 s hidden prefill -> 4.73 s first visible calculator TTFT with
 * 337 cached tokens and zero swap growth. oMLX is excluded because its short
 * prefix cache produced no hit in the corresponding qualification.
 */
function maybePrimeLlamaTools() {
  if (!state.running || !chatToolsPreference() || effectiveBackend() === "mlx") {
    renderLlamaToolPrime();
    return state.toolPrimePromise;
  }
  if (state.toolPrimeStatus === "warming" || state.toolPrimeStatus === "ready" || state.toolPrimeStatus === "failed") {
    renderLlamaToolPrime();
    return state.toolPrimePromise;
  }

  const controller = new AbortController();
  state.toolPrimeController = controller;
  state.toolPrimeStatus = "warming";
  renderLlamaToolPrime();
  const timeout = setTimeout(() => controller.abort(), 120000);
  const primeBody = {
    model: chatFallbackModelId(),
    messages: [{ role: "user", content: "Initialize the local tool contract." }],
    tools: DEMO_TOOLS,
    tool_choice: "auto",
    // One discarded token completes the OpenAI request and leaves the prefix hot.
    max_tokens: 1,
    temperature: 0,
    stream: false,
    cache_prompt: true,
    chat_template_kwargs: { enable_thinking: false },
  };
  const promise = fetch(`http://127.0.0.1:${PORT}/v1/chat/completions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(primeBody),
    signal: controller.signal,
  }).then(async (response) => {
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    await response.arrayBuffer();
    if (state.toolPrimeController === controller) state.toolPrimeStatus = "ready";
    return true;
  }).catch(() => {
    if (state.toolPrimeController === controller && state.running) state.toolPrimeStatus = "failed";
    return false;
  }).finally(() => {
    clearTimeout(timeout);
    if (state.toolPrimeController === controller) {
      state.toolPrimeController = null;
      state.toolPrimePromise = null;
      renderLlamaToolPrime();
    }
  });
  state.toolPrimePromise = promise;
  return promise;
}

function onChatToolsToggle(enabled) {
  syncChatToolsUi(enabled);
  if (!enabled && state.toolPrimeStatus === "warming") {
    resetLlamaToolPrime();
    return;
  }
  if (enabled && state.toolPrimeStatus === "failed") state.toolPrimeStatus = "idle";
  if (enabled) maybePrimeLlamaTools();
  renderLlamaToolPrime();
}

function runDemoTool(name, args) {
  if (name === "get_current_time") return new Date().toISOString();
  if (name === "calculator") {
    const expr = String((args && args.expression) || "").replace(/[^0-9+\-*/().\s]/g, "");
    if (!expr.trim()) return "Error: empty expression";
    try {
      // eslint-disable-next-line no-new-func
      const v = Function(`"use strict"; return (${expr});`)();
      if (typeof v !== "number" || !Number.isFinite(v)) return "Error: not a finite number";
      return String(v);
    } catch (e) {
      return `Error: ${e.message || e}`;
    }
  }
  return `Error: unknown tool ${name}`;
}

/** Heuristic VLM detection — oMLX /v1/models omits model_type. */
function looksLikeVlm(id) {
  const s = String(id || "").toLowerCase();
  if (!s) return false;
  return /vlm|vision|llava|pixtral|minicpm-v|internvl|qwen2-vl|qwen2\.5-vl|qwen3-vl|qwen3\.5|qwen3\.6|gemma-?3|phi-3\.5-vision|molmo|idefics/.test(s);
}

function chatFallbackModelId() {
  if (effectiveBackend() === "mlx" && state.model && state.model.mlx) return state.model.mlx.id;
  return "slipstream";
}

function selectedChatModelMeta() {
  const id = state.chatModel || chatFallbackModelId();
  const hit = (state.chatModels || []).find((m) => m.id === id);
  return hit || { id, vlm: looksLikeVlm(id) || (effectiveBackend() === "mlx" && looksLikeVlm(chatFallbackModelId())) };
}

function fillChatModelSelect(ids) {
  const sel = $("chatModelSel");
  if (!sel) return;
  const prev = state.chatModel || chatFallbackModelId();
  sel.innerHTML = "";
  const list = ids.length ? ids : [{ id: chatFallbackModelId(), vlm: false }];
  for (const m of list) {
    const opt = document.createElement("option");
    opt.value = m.id;
    opt.textContent = m.id + (m.vlm ? " · VLM" : "");
    sel.appendChild(opt);
  }
  const prefer = list.some((m) => m.id === prev) ? prev : list[0].id;
  sel.value = prefer;
  state.chatModel = prefer;
  sel.title = state.chatModelsLive
    ? "GET /v1/models · live"
    : "GET /v1/models · fallback (server down or list empty)";
  const live = $("chatModelsLive");
  if (live) {
    live.hidden = !state.chatModelsLive;
    live.textContent = state.chatModelsLive ? "live" : "";
  }
  updateChatCapabilityControls();
}

async function refreshChatModels() {
  const fallback = [{ id: chatFallbackModelId(), vlm: looksLikeVlm(chatFallbackModelId()) || effectiveBackend() === "mlx" }];
  if (!state.running) {
    state.chatModelsLive = false;
    state.chatModels = fallback;
    fillChatModelSelect(fallback);
    return;
  }
  try {
    const ac = new AbortController();
    const to = setTimeout(() => ac.abort(), 2500);
    let res;
    try {
      res = await fetch(`http://127.0.0.1:${PORT}/v1/models`, { signal: ac.signal });
    } finally {
      clearTimeout(to);
    }
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const j = await res.json();
    const raw = Array.isArray(j.data) ? j.data : (Array.isArray(j) ? j : []);
    const ids = [];
    const seen = new Set();
    for (const row of raw) {
      const id = (row && (row.id || row.model)) || "";
      if (!id || seen.has(id)) continue;
      seen.add(id);
      const vlm = !!(row.model_type === "vlm" || row.vlm || looksLikeVlm(id));
      ids.push({ id, vlm });
    }
    if (!ids.length) ids.push(...fallback);
    // Catalog mlx twin: mark VLM when name matches even if server omitted type.
    if (effectiveBackend() === "mlx") {
      for (const m of ids) {
        if (!m.vlm && looksLikeVlm(m.id)) m.vlm = true;
      }
    }
    state.chatModelsLive = true;
    state.chatModels = ids;
    fillChatModelSelect(ids);
  } catch {
    state.chatModelsLive = false;
    state.chatModels = fallback;
    fillChatModelSelect(fallback);
  }
}

function updateChatCapabilityControls() {
  const mlx = effectiveBackend() === "mlx";
  const meta = selectedChatModelMeta();
  const vlm = !!(meta.vlm || (mlx && looksLikeVlm(meta.id)));
  if ($("chatVlmBadge")) $("chatVlmBadge").hidden = !vlm;
  if ($("chatAttach")) $("chatAttach").hidden = !vlm;
  if ($("chatDocAttach")) $("chatDocAttach").hidden = !mlx;
  // Metal never sends multimodal / file parts — drop pending attaches when leaving the path.
  if (!vlm && state.chatAttach) {
    state.chatAttach = null;
    renderChatAttachPreview();
  }
  if (!mlx && state.chatDoc) {
    state.chatDoc = null;
    renderChatAttachPreview();
  }
  ["chatToolsWrap", "chatJsonWrap", "chatToolsSettingsWrap"].forEach((id) => {
    const el = $(id);
    if (el) el.hidden = false;
  });
  if (!chat.streaming) {
    const toolsOn = chatToolsPreference();
    const jsonOn = chatJsonPreference();
    if ($("settingsChatTools") && $("settingsChatTools").checked !== toolsOn) {
      $("settingsChatTools").checked = toolsOn;
    }
    if ($("chatTools") && $("chatTools").checked !== toolsOn) $("chatTools").checked = toolsOn;
    if ($("chatJson") && $("chatJson").checked !== jsonOn) $("chatJson").checked = jsonOn;
  }
  updateChatSchemaVisibility();
}

function mimeFromDataUrl(dataUrl) {
  const m = /^data:([^;,]+)/i.exec(dataUrl || "");
  return (m && m[1]) || "application/octet-stream";
}

/** Build OpenAI/oMLX user `content` (string or content-parts array). Pure — unit-tested. */
function buildUserContentParts(text, imageAttach, docAttach) {
  const hasImg = !!(imageAttach && imageAttach.dataUrl);
  const hasDoc = !!(docAttach && docAttach.dataUrl);
  if (!hasImg && !hasDoc) return text || "";
  const parts = [];
  if (text) parts.push({ type: "text", text });
  if (hasImg) {
    parts.push({ type: "image_url", image_url: { url: imageAttach.dataUrl } });
  }
  if (hasDoc) {
    const filename = docAttach.filename
      || (docAttach.path && String(docAttach.path).split("/").pop())
      || "document";
    const mime = docAttach.mime || mimeFromDataUrl(docAttach.dataUrl);
    parts.push({
      type: "file",
      file: {
        filename,
        mime_type: mime,
        file_data: docAttach.dataUrl,
      },
    });
  }
  return parts;
}

function renderChatAttachPreview() {
  const row = $("chatAttachRow");
  const prev = $("chatAttachPreview");
  if (!prev) return;
  prev.innerHTML = "";
  const hasImg = !!(state.chatAttach && state.chatAttach.dataUrl);
  const hasDoc = !!(state.chatDoc && state.chatDoc.dataUrl);
  if (!hasImg && !hasDoc) {
    if (row) row.hidden = true;
    return;
  }
  if (row) row.hidden = false;
  if (hasImg) {
    const img = document.createElement("img");
    img.src = state.chatAttach.dataUrl;
    img.alt = (state.chatAttach.path || "").split("/").pop() || "image";
    prev.appendChild(img);
  }
  if (hasDoc) {
    const chip = document.createElement("span");
    chip.className = "chat-attach-chip";
    const name = state.chatDoc.filename
      || (state.chatDoc.path || "").split("/").pop()
      || "document";
    chip.textContent = "📎 " + name;
    chip.title = state.chatDoc.path || name;
    prev.appendChild(chip);
  }
}

function clearChatAttach() {
  state.chatAttach = null;
  state.chatDoc = null;
  renderChatAttachPreview();
}

async function pickChatImage() {
  if (!dialog || !dialog.open) { toast(t("toast.noDialog"), true); return; }
  try {
    const picked = await dialog.open({
      multiple: false,
      filters: [{ name: "Images", extensions: ["png", "jpg", "jpeg", "gif", "webp", "bmp"] }],
    });
    if (!picked) return;
    const path = typeof picked === "string" ? picked : (picked.path || picked);
    const dataUrl = await invoke("read_file_data_url", { path });
    state.chatAttach = { path, dataUrl };
    renderChatAttachPreview();
  } catch (e) {
    toast(e, true);
  }
}

async function pickChatDoc() {
  if (effectiveBackend() !== "mlx") {
    toast(t("tip.chatDocAttach"), true);
    return;
  }
  if (!dialog || !dialog.open) { toast(t("toast.noDialog"), true); return; }
  try {
    const picked = await dialog.open({
      multiple: false,
      filters: [{
        name: "Documents",
        extensions: ["pdf", "md", "markdown", "txt", "docx", "pptx"],
      }],
    });
    if (!picked) return;
    const path = typeof picked === "string" ? picked : (picked.path || picked);
    const dataUrl = await invoke("read_file_data_url", { path });
    const filename = String(path).split("/").pop() || "document";
    state.chatDoc = {
      path,
      dataUrl,
      filename,
      mime: mimeFromDataUrl(dataUrl),
    };
    renderChatAttachPreview();
  } catch (e) {
    toast(e, true);
  }
}

function chatAddBubble(role, text) {
  const empty = $("chatEmpty"); if (empty) empty.style.display = "none";
  const wrap = document.createElement("div");
  wrap.className = `chat-msg chat-${role}`;
  const col = document.createElement("div");
  col.className = "chat-msg-col";
  const body = document.createElement("div");
  body.className = "chat-bubble";
  const pending = role === "assistant" && !String(text || "").trim();
  if (pending) body.classList.add("chat-pending");
  body.textContent = text || "";
  col.appendChild(body);
  wrap.appendChild(col);
  $("chatMessages").appendChild(wrap);
  $("chatMessages").scrollTop = $("chatMessages").scrollHeight;
  return { wrap, col, body };
}

/** Drop empty assistant shells (abort / no tokens) so Chat stays clean. */
function chatFinalizeAssistantBubble(ui) {
  if (!ui || !ui.body || !ui.wrap) return;
  ui.body.classList.remove("streaming");
  const hasText = !!(ui.body.textContent && ui.body.textContent.trim());
  const hasExtras = !!(ui.col && ui.col.querySelector(".chat-tool, .chat-reasoning"));
  if (!hasText && !hasExtras && !ui.body.classList.contains("chat-err")) {
    ui.wrap.remove();
    return;
  }
  ui.body.classList.remove("chat-pending");
}

function chatAddReasoning(col, text) {
  if (!text) return;
  let el = col.querySelector(".chat-reasoning");
  if (!el) {
    el = document.createElement("details");
    el.className = "chat-reasoning";
    el.open = true;
    const lab = document.createElement("summary");
    lab.className = "chat-reasoning-label";
    lab.textContent = t("chat.reasoning");
    el.appendChild(lab);
    const body = document.createElement("div");
    body.className = "chat-reasoning-body";
    el.appendChild(body);
    col.insertBefore(el, col.firstChild);
    el._body = body;
  }
  const body = el._body || el.querySelector(".chat-reasoning-body");
  if (body) body.textContent = text;
  else el.textContent = text;
  $("chatMessages").scrollTop = $("chatMessages").scrollHeight;
}

function chatAddToolNote(col, label, text) {
  const el = document.createElement("div");
  el.className = "chat-tool";
  el.textContent = `${label}: ${text}`;
  col.appendChild(el);
  $("chatMessages").scrollTop = $("chatMessages").scrollHeight;
  return el;
}

function setChatBusy(busy) {
  chat.streaming = busy;
  $("chatSend").style.display = busy ? "none" : "";
  $("chatStop").style.display = busy ? "" : "none";
  $("chatInput").disabled = busy;
  if ($("chatAttach")) $("chatAttach").disabled = busy;
  if ($("chatDocAttach")) $("chatDocAttach").disabled = busy;
  if ($("chatModelSel")) $("chatModelSel").disabled = busy;
  if ($("chatTools")) $("chatTools").disabled = busy;
  if ($("chatJson")) $("chatJson").disabled = busy;
  if ($("chatThink")) $("chatThink").disabled = busy;
}

function p2pPeerFromUi() {
  return ($("p2pPeer") && $("p2pPeer").value.trim()) || "";
}
function p2pModelFromUi() {
  return ($("p2pModel") && $("p2pModel").value.trim()) || "mock";
}
function p2pMaxTokensFromUi(fallback) {
  const n = parseInt(($("p2pMaxTokens") && $("p2pMaxTokens").value) || "", 10);
  return Number.isFinite(n) && n > 0 ? n : fallback;
}
function p2pProbeAddrs() {
  const peer = p2pPeerFromUi();
  const boot = ($("p2pBootstrap") && $("p2pBootstrap").value.trim()) || "";
  const parts = [peer, ...boot.split(",")].map((s) => s.trim()).filter(Boolean);
  return [...new Set(parts)].join(",");
}
function setP2pNote(msg) {
  if ($("p2pNote")) $("p2pNote").textContent = msg || "";
}
function setP2pPill(mode) {
  const el = $("p2pPill");
  if (!el) return;
  el.classList.remove("p2p-offline", "p2p-listening", "p2p-error");
  const key = mode === "listening" ? "p2p.statusListening"
    : mode === "error" ? "p2p.statusError" : "p2p.statusOffline";
  el.classList.add(mode === "listening" ? "p2p-listening" : mode === "error" ? "p2p-error" : "p2p-offline");
  el.textContent = t(key);
}

/** Local server down + P2P flag on → sealed job (never used when Metal/MLX is ready). */
async function sendChatViaP2p(text) {
  const input = $("chatInput");
  if (input) { input.value = ""; input.style.height = "auto"; }
  chatAddBubble("user", text);
  chat.history.push({ role: "user", content: text });
  const ui = chatAddBubble("assistant", "");
  ui.body.classList.add("streaming");
  setChatBusy(true);
  const meta = $("chatMeta"); if (meta) meta.textContent = "";
  const t0 = performance.now();
  try {
    const out = await invoke("p2p_chat", {
      prompt: text,
      peer: p2pPeerFromUi() || null,
      model: p2pModelFromUi(),
      maxTokens: p2pMaxTokensFromUi(64),
    });
    if (!out || !out.ok) throw new Error((out && out.error) || "P2P job failed");
    const answer = out.text || "";
    ui.body.classList.remove("chat-pending");
    ui.body.textContent = answer;
    chat.history.push({ role: "assistant", content: answer });
    const secs = (performance.now() - t0) / 1000;
    const tokens = out.tokens || 0;
    if (meta && secs > 0 && tokens > 0) meta.textContent = `${tps(tokens / secs)} · ${tokens} tokens · P2P`;
    toast(t("chat.viaP2p"));
  } catch (e) {
    ui.body.classList.remove("chat-pending");
    ui.body.textContent = t("err.prefix") + e;
    ui.body.classList.add("chat-err");
  } finally {
    chatFinalizeAssistantBubble(ui);
    setChatBusy(false);
  }
}

/**
 * Build POST /v1/chat/completions body for Slipstream Chat.
 * Common OpenAI contract: both local engines receive tools / response_format.
 * Multimodal and file content parts remain capability-scoped by their builders.
 * Returns { error } when JSON schema paste is invalid.
 */
function buildChatRequestBody(messages, opts) {
  const think = !!($("chatThink") && $("chatThink").checked);
  const body = {
    model: state.chatModel || chatFallbackModelId(),
    messages,
    stream: true,
    stream_options: { include_usage: true },
    temperature: think ? 0.6 : 0,
    chat_template_kwargs: { enable_thinking: think },
  };
  const userText = (opts && opts.userText) || "";
  if (toolsEnabledForRequest(userText)) {
    body.tools = DEMO_TOOLS;
    body.tool_choice = "auto";
  }
  if ($("chatJson") && $("chatJson").checked) {
    const rf = buildResponseFormat(true, chatSchemaText());
    if (rf && rf.error) return { error: rf.error };
    if (rf && rf.response_format) body.response_format = rf.response_format;
  }
  return body;
}

/** One SSE chat/completions round; mutates ui + returns {answer, reasoning, toolCalls, tokens}. */
async function streamChatCompletion(messages, ui, opts) {
  const body = buildChatRequestBody(messages, opts);
  if (body.error) throw new Error(body.error);
  const needsContractProfile = !!(body.tools || body.response_format);
  if (
    effectiveBackend() === "mlx"
    && needsContractProfile
    && state.runningPgrnProfile
    && state.runningPgrnProfile !== "contract"
    && state.runningPgrnProfile !== "fast"
  ) {
    throw new Error(t("err.mlxContractRestart"));
  }
  const res = await fetch(`http://127.0.0.1:${PORT}/v1/chat/completions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
    signal: chat.abort.signal,
  });
  if (!res.ok || !res.body) {
    let detail = `HTTP ${res.status}`;
    try { const err = await res.json(); if (err.error) detail += `: ${err.error.message || JSON.stringify(err.error)}`; } catch {}
    throw new Error(detail);
  }
  const reader = res.body.getReader();
  const dec = new TextDecoder();
  let buf = "", answer = "", reasoning = "", tokens = 0, usageCompletion = 0;
  const toolCallsMap = {};
  const t0 = (opts && opts.t0) || performance.now();
  const meta = $("chatMeta");
  let lastMetaAt = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    const lines = buf.split("\n");
    buf = lines.pop() || "";
    for (const line of lines) {
      const s = line.trim();
      if (!s.startsWith("data:")) continue;
      const payload = s.slice(5).trim();
      if (payload === "[DONE]") continue;
      try {
        const j = JSON.parse(payload);
        if (j.usage && typeof j.usage.completion_tokens === "number") {
          usageCompletion = j.usage.completion_tokens;
        }
        const delta = j.choices?.[0]?.delta || {};
        if (delta.reasoning_content) {
          reasoning += delta.reasoning_content;
          chatAddReasoning(ui.col, reasoning);
        }
        if (delta.content) {
          answer += delta.content;
          tokens++;
          ui.body.classList.remove("chat-pending");
          ui.body.textContent = answer;
          $("chatMessages").scrollTop = $("chatMessages").scrollHeight;
          // Live wall tok/s while streaming (throttle DOM writes).
          const now = performance.now();
          if (meta && tokens >= 2 && now - lastMetaAt > 250) {
            const secs = (now - t0) / 1000;
            if (secs > 0) {
              meta.textContent = `${tps(tokens / secs)} · ${tokens} tokens · ${t("chat.metaLive")}`;
              lastMetaAt = now;
            }
          }
        }
        if (delta.tool_calls) {
          for (const tc of delta.tool_calls) {
            const i = tc.index ?? 0;
            if (!toolCallsMap[i]) {
              toolCallsMap[i] = { id: "", type: "function", function: { name: "", arguments: "" } };
            }
            if (tc.id) toolCallsMap[i].id = tc.id;
            if (tc.function?.name) toolCallsMap[i].function.name += tc.function.name;
            if (tc.function?.arguments) toolCallsMap[i].function.arguments += tc.function.arguments;
          }
        }
      } catch {}
    }
  }
  return {
    answer,
    reasoning,
    toolCalls: Object.values(toolCallsMap),
    tokens: usageCompletion > 0 ? usageCompletion : tokens,
  };
}

async function sendChat() {
  const input = $("chatInput");
  const text = input.value.trim();
  const hasImg = !!(state.chatAttach && state.chatAttach.dataUrl);
  const hasDoc = !!(state.chatDoc && state.chatDoc.dataUrl);
  // Docs are oMLX MarkItDown-only — never send file parts on Metal.
  if (hasDoc && effectiveBackend() !== "mlx") {
    toast(t("tip.chatDocAttach"), true);
    return;
  }
  if ((!text && !hasImg && !hasDoc) || chat.streaming) return;
  if (toolsEnabledForRequest(text) && state.toolPrimeStatus === "warming") {
    toast(t("chat.toolsPrimeWait"));
    return;
  }
  // Prefer local Metal/MLX whenever the server is up; P2P only as offline fallback.
  if (!state.running) {
    if (state.p2p && state.p2pRemoteChat) {
      await sendChatViaP2p(text || (hasDoc ? "(document)" : "(image)"));
      return;
    }
    toast(t("chat.serverHint"), true);
    return;
  }
  input.value = ""; input.style.height = "auto";

  const userContent = buildUserContentParts(text, state.chatAttach, state.chatDoc);
  if (hasImg || hasDoc) {
    const bits = [];
    if (text) bits.push(text);
    if (hasImg) bits.push("🖼 " + ((state.chatAttach.path || "").split("/").pop() || "image"));
    if (hasDoc) bits.push("📎 " + (state.chatDoc.filename || (state.chatDoc.path || "").split("/").pop() || "document"));
    chatAddBubble("user", bits.join("\n"));
    clearChatAttach();
  } else {
    chatAddBubble("user", text);
  }
  chat.history.push({ role: "user", content: userContent });

  const ui = chatAddBubble("assistant", "");
  ui.body.classList.add("streaming");
  setChatBusy(true);
  const meta = $("chatMeta"); if (meta) meta.textContent = "";
  const t0 = performance.now();
  let tokens = 0;
  chat.abort = new AbortController();
  try {
    // Tool roundtrips: stream → execute demo tools → stream again (max 3 loops).
    let messages = chat.history.slice();
    let finalAnswer = "", finalReasoning = "";
    const reqOpts = { userText: text };
    for (let round = 0; round < 4; round++) {
      const out = await streamChatCompletion(messages, ui, reqOpts);
      tokens += out.tokens;
      if (out.reasoning) finalReasoning = out.reasoning;
      if (out.toolCalls && out.toolCalls.length && out.toolCalls.some((tc) => tc.function && tc.function.name)) {
        const assistantMsg = {
          role: "assistant",
          content: out.answer || null,
          tool_calls: out.toolCalls,
        };
        if (out.reasoning) assistantMsg.reasoning_content = out.reasoning;
        messages.push(assistantMsg);
        chat.history.push(assistantMsg);
        for (const tc of out.toolCalls) {
          const name = tc.function.name;
          let args = {};
          try { args = JSON.parse(tc.function.arguments || "{}"); } catch {}
          chatAddToolNote(ui.col, t("chat.toolCall"), `${name}(${JSON.stringify(args)})`);
          const result = runDemoTool(name, args);
          chatAddToolNote(ui.col, t("chat.toolResult"), `${name} → ${result}`);
          const toolMsg = {
            role: "tool",
            tool_call_id: tc.id || name,
            name,
            content: result,
          };
          messages.push(toolMsg);
          chat.history.push(toolMsg);
        }
        ui.body.textContent = "";
        ui.body.classList.add("chat-pending");
        continue;
      }
      finalAnswer = out.answer;
      break;
    }
    if (finalReasoning && !ui.col.querySelector(".chat-reasoning")) {
      chatAddReasoning(ui.col, finalReasoning);
    }
    if (finalAnswer) ui.body.classList.remove("chat-pending");
    ui.body.textContent = finalAnswer;
    const asst = { role: "assistant", content: finalAnswer };
    if (finalReasoning) asst.reasoning_content = finalReasoning;
    // Avoid duplicating the last assistant message if a tool loop already pushed it.
    const last = chat.history[chat.history.length - 1];
    if (!(last && last.role === "assistant" && last.content === finalAnswer && !last.tool_calls)) {
      chat.history.push(asst);
    }
    const secs = (performance.now() - t0) / 1000;
    if (meta && secs > 0 && tokens > 0) {
      meta.textContent = `${tps(tokens / secs)} · ${tokens} tokens · ${t("chat.metaLast")}`;
    }
  } catch (e) {
    if (e.name !== "AbortError") {
      ui.body.classList.remove("chat-pending");
      ui.body.textContent = ui.body.textContent || (t("err.prefix") + (e.message || e));
      ui.body.classList.add("chat-err");
    }
  } finally {
    chatFinalizeAssistantBubble(ui);
    setChatBusy(false);
    chat.abort = null;
  }
}

/** Calm first-launch empty state — 3-step CTA (see PRODUCT_CLICK_JOURNEY.md). */
function chatEmptyHtml() {
  return '<div class="chat-empty" id="chatEmpty">'
    + '<p class="chat-empty-title" data-i18n="journey.title"></p>'
    + '<ol class="journey-steps" id="journeySteps">'
    + '<li><button type="button" class="journey-step" id="journeyStep1" data-journey="folder">'
    + '<span class="journey-n" aria-hidden="true">1</span><span data-i18n="journey.step1"></span></button></li>'
    + '<li><button type="button" class="journey-step" id="journeyStep2" data-journey="start">'
    + '<span class="journey-n" aria-hidden="true">2</span><span data-i18n="journey.step2"></span></button></li>'
    + '<li><button type="button" class="journey-step" id="journeyStep3" data-journey="prompt">'
    + '<span class="journey-n" aria-hidden="true">3</span><span data-i18n="journey.step3"></span></button></li>'
    + '</ol>'
    + '<p class="chat-empty-tip" data-i18n="journey.codingTip"></p>'
    + '</div>';
}

async function journeyChooseFolder() {
  showTab("models");
  try { await preferDefaultModelPaths(); } catch {}
  const prefer = state.preferMetalDir || preferredMetalModelDir();
  if ($("pModelsRoot") && prefer && !$("pModelsRoot").value.trim()) $("pModelsRoot").value = prefer;
  if ($("pModelsRoot")) {
    await pickInto("pModelsRoot", { directory: true });
    return;
  }
  // Fallback for older DOM: Metal → GGUF folder, MLX → mlx dir.
  const useMlx = effectiveBackend() === "mlx";
  if (useMlx) {
    const mlxPrefer = state.preferMlxDir || preferredMlxModelDir();
    if ($("pMlx") && mlxPrefer && !$("pMlx").value.trim()) $("pMlx").value = mlxPrefer;
    if ($("pMlx")) await pickInto("pMlx", { directory: true });
    else showTab("settings");
    return;
  }
  if ($("pDir")) {
    try { applyModelPaths(); } catch {}
    await pickInto("pDir", { directory: true });
  }
}
function journeyStartServer() {
  if (state.running) return;
  const btn = $("powerBtn");
  if (btn && !btn.disabled) { btn.click(); return; }
  toast(t("journey.needModel"), true);
  showTab("models");
}
function journeySendPrompt() {
  showTab("chat");
  const input = $("chatInput");
  if (!input) return;
  if (!input.value.trim()) input.value = t("journey.promptSample");
  input.focus();
  input.dispatchEvent(new Event("input"));
}
function wireJourney() {
  const s1 = $("journeyStep1");
  if (s1) s1.onclick = () => { journeyChooseFolder(); };
  const s2 = $("journeyStep2");
  if (s2) s2.onclick = () => { journeyStartServer(); };
  const s3 = $("journeyStep3");
  if (s3) s3.onclick = () => { journeySendPrompt(); };
}
function wireJourneyCta() { wireJourney(); }
function renderChatEmpty() {
  const wrap = $("chatMessages");
  if (!wrap) return;
  wrap.innerHTML = chatEmptyHtml();
  applyLang(LANG);
  wireJourney();
}

function initChat() {
  const send = $("chatSend"); if (!send) return;
  send.onclick = sendChat;
  $("chatStop").onclick = () => { if (chat.abort) chat.abort.abort(); };
  $("chatClear").onclick = () => {
    chat.history = [];
    clearChatAttach();
    renderChatEmpty();
  };
  wireJourney();
  if ($("chatModelSel")) {
    $("chatModelSel").onchange = (e) => {
      state.chatModel = e.target.value;
      updateChatCapabilityControls();
    };
  }
  if ($("chatAttach")) $("chatAttach").onclick = pickChatImage;
  if ($("chatDocAttach")) $("chatDocAttach").onclick = pickChatDoc;
  if ($("chatAttachClear")) $("chatAttachClear").onclick = clearChatAttach;
  // Settings ↔ Chat toolbar: common OpenAI tools / structured output contract.
  if ($("chatTools")) $("chatTools").onchange = (e) => onChatToolsToggle(e.target.checked);
  if ($("settingsChatTools")) $("settingsChatTools").onchange = (e) => onChatToolsToggle(e.target.checked);
  if ($("chatJson")) $("chatJson").onchange = (e) => syncChatJsonUi(e.target.checked);
  const schema = $("chatSchema");
  if (schema) {
    schema.addEventListener("input", () => {
      persistChatSchema(schema.value);
      updateChatSchemaStatus();
    });
  }
  const input = $("chatInput");
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); sendChat(); }
  });
  input.addEventListener("input", () => {
    input.style.height = "auto";
    input.style.height = Math.min(160, input.scrollHeight) + "px";
  });
  // Restore the common contract before its first paint, independent of backend.
  syncChatToolsUi(chatToolsPreference());
  syncChatJsonUi(chatJsonPreference());
  refreshChatModels();
  updateChatCapabilityControls();
}

// ---- Slipstream P2P (Cluster tab; localStorage slipstream.p2p) --------------
function p2pEngineFromUi() {
  const el = $("p2pEngine");
  return (el && el.value) || localStorage.getItem("slipstream.p2p.engine") || "mock";
}
function p2pModeFromUi() {
  const el = $("p2pMode");
  return (el && el.value) || localStorage.getItem("slipstream.p2p.mode") || "local";
}
function p2pBootstrapFromUi() {
  return ($("p2pBootstrap") && $("p2pBootstrap").value.trim()) || "";
}
function p2pModelFromUi() {
  const el = $("p2pModel");
  return (el && el.value.trim()) || "mock";
}
function p2pMaxTokensFromUi(fallback) {
  const el = $("p2pMaxTokens");
  const n = el ? parseInt(el.value, 10) : NaN;
  return Number.isFinite(n) && n > 0 ? n : fallback;
}
function p2pPeerFromUi() {
  return ($("p2pPeer") && $("p2pPeer").value.trim()) || "";
}

function applyP2pUi() {
  const on = !!state.p2p;
  if ($("p2pEnable")) $("p2pEnable").checked = on;
  if ($("p2pPanel")) $("p2pPanel").hidden = !on;
  if ($("p2pEngine")) {
    const saved = localStorage.getItem("slipstream.p2p.engine") || "mock";
    $("p2pEngine").value = saved;
  }
  if ($("p2pMode")) {
    $("p2pMode").value = localStorage.getItem("slipstream.p2p.mode") || "local";
  }
  if ($("p2pDonate")) {
    const community = p2pModeFromUi() === "community";
    $("p2pDonate").disabled = !community;
    $("p2pDonate").checked = community && localStorage.getItem("slipstream.p2p.donate") === "1";
  }
  if ($("p2pRemoteChat")) {
    $("p2pRemoteChat").checked = !!state.p2pRemoteChat;
  }
  if ($("p2pBootstrap")) {
    $("p2pBootstrap").value = localStorage.getItem("slipstream.p2p.bootstrap") || "";
  }
  if ($("p2pPeer")) {
    const peer = localStorage.getItem("slipstream.p2p.peer");
    if (peer != null) $("p2pPeer").value = peer;
  }
}

function setP2pPill(mode) {
  const pill = $("p2pPill");
  if (!pill) return;
  pill.classList.remove("p2p-offline", "p2p-listening", "p2p-error");
  if (mode === "listening") {
    pill.classList.add("p2p-listening");
    pill.textContent = t("p2p.statusListening") || "listening";
  } else if (mode === "error") {
    pill.classList.add("p2p-error");
    pill.textContent = "error";
  } else {
    pill.classList.add("p2p-offline");
    pill.textContent = t("p2p.statusOffline") || "offline";
  }
}

async function refreshP2pCredits(jobId) {
  if (!state.p2p) return;
  try {
    const c = await invoke("p2p_credits", {
      account: null,
      jobId: jobId || null,
    });
    if ($("p2pCredits")) $("p2pCredits").textContent = String(c.balance ?? c.credits ?? 0);
    if ($("p2pSettlement")) {
      if (c.settlement) {
        const s = c.settlement;
        $("p2pSettlement").textContent =
          `${s.credits} cr · ${s.tokens} tok · ${(s.job_id || "").slice(0, 16)}`;
      } else if (jobId) {
        $("p2pSettlement").textContent = "–";
      }
    }
  } catch (e) {
    if ($("p2pNote")) $("p2pNote").textContent = String(e);
  }
}

async function refreshP2pRecent() {
  if (!state.p2p) return;
  const list = $("p2pRecentList");
  const empty = $("p2pRecentEmpty");
  if (!list) return;
  try {
    const peers = await invoke("p2p_recent_peers");
    list.innerHTML = "";
    const rows = peers || [];
    if (empty) empty.hidden = rows.length > 0;
    for (const p of rows) {
      const li = document.createElement("li");
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "btn btn-ghost p2p-peer-btn";
      const label = p.ok
        ? `${p.addr} · ${(p.node_id || "").slice(0, 10)}… · ${p.backend || "?"}`
        : (p.addr || String(p));
      btn.textContent = typeof p === "string" ? p : label;
      const addr = typeof p === "string" ? p : p.addr;
      btn.onclick = () => {
        if ($("p2pPeer")) {
          $("p2pPeer").value = addr;
          localStorage.setItem("slipstream.p2p.peer", addr);
        }
      };
      li.appendChild(btn);
      list.appendChild(li);
    }
  } catch {}
}

async function refreshP2pStatus() {
  if (!state.p2p) { setP2pPill("offline"); return; }
  try {
    const st = await invoke("p2p_status");
    if ($("p2pNodeId")) $("p2pNodeId").textContent = st.node_id || "–";
    if ($("p2pCredits")) $("p2pCredits").textContent = String(st.credits ?? 0);
    if ($("p2pEngineDisp")) $("p2pEngineDisp").textContent = st.engine || "mock";
    if ($("p2pEngine") && st.engine) $("p2pEngine").value = st.engine;
    if ($("p2pMode") && st.mode) {
      $("p2pMode").value = st.mode;
      localStorage.setItem("slipstream.p2p.mode", st.mode);
    }
    if ($("p2pDonate")) {
      $("p2pDonate").disabled = st.mode !== "community";
      $("p2pDonate").checked = st.mode === "community" && !!st.donate_capacity;
      localStorage.setItem("slipstream.p2p.donate", $("p2pDonate").checked ? "1" : "0");
    }
    if ($("p2pListenDisp")) $("p2pListenDisp").textContent = st.listen_addr || "–";
    if ($("p2pState")) {
      $("p2pState").textContent = st.running
        ? ("listening " + (st.listen_addr || ""))
        : "stopped";
    }
    if (st.running && st.listen_addr && $("p2pListen")) $("p2pListen").value = st.listen_addr;
    setP2pPill(st.running ? "listening" : "offline");
    if (st.last_job_id) await refreshP2pCredits(st.last_job_id);
    else await refreshP2pCredits(null);
    await refreshP2pRecent();
  } catch (e) {
    setP2pPill("error");
    if ($("p2pNote")) $("p2pNote").textContent = String(e);
  }
}

function updateChatP2pHint() {
  const note = $("chatServerNote");
  if (!note) return;
  if (!state.running && state.p2p && state.p2pRemoteChat) {
    note.textContent = t("chat.p2pHint") || t("chat.serverHint");
  } else {
    note.textContent = t("chat.serverHint");
  }
}

function initP2p() {
  applyP2pUi();
  updateChatP2pHint();
  if ($("p2pEnable")) {
    $("p2pEnable").onchange = () => {
      state.p2p = !!$("p2pEnable").checked;
      localStorage.setItem("slipstream.p2p", state.p2p ? "1" : "0");
      applyP2pUi();
      updateChatP2pHint();
      if (state.p2p) refreshP2pStatus();
      else {
        setP2pPill("offline");
        if ($("p2pNote")) $("p2pNote").textContent = "";
      }
    };
  }
  if ($("p2pEngine")) {
    $("p2pEngine").onchange = () => {
      localStorage.setItem("slipstream.p2p.engine", p2pEngineFromUi());
      if ($("p2pModel") && p2pEngineFromUi() !== "mock" && $("p2pModel").value === "mock") {
        $("p2pModel").value = "slipstream";
      }
    };
  }
  if ($("p2pMode")) {
    $("p2pMode").onchange = () => {
      const mode = p2pModeFromUi();
      localStorage.setItem("slipstream.p2p.mode", mode);
      if ($("p2pDonate")) {
        $("p2pDonate").disabled = mode !== "community";
        if (mode !== "community") {
          $("p2pDonate").checked = false;
          localStorage.setItem("slipstream.p2p.donate", "0");
        }
      }
    };
  }
  if ($("p2pDonate")) {
    $("p2pDonate").onchange = () => {
      localStorage.setItem("slipstream.p2p.donate", $("p2pDonate").checked ? "1" : "0");
    };
  }
  if ($("p2pRemoteChat")) {
    $("p2pRemoteChat").onchange = () => {
      state.p2pRemoteChat = !!$("p2pRemoteChat").checked;
      localStorage.setItem("slipstream.p2p.remoteChat", state.p2pRemoteChat ? "1" : "0");
      updateChatP2pHint();
    };
  }
  if ($("p2pBootstrap")) {
    $("p2pBootstrap").onchange = () => {
      localStorage.setItem("slipstream.p2p.bootstrap", p2pBootstrapFromUi());
    };
  }
  if ($("p2pPeer")) {
    $("p2pPeer").onchange = () => {
      localStorage.setItem("slipstream.p2p.peer", p2pPeerFromUi());
    };
  }
  if ($("p2pGotoCluster")) {
    $("p2pGotoCluster").onclick = () => showTab("cluster");
  }
  if ($("p2pCopyId")) {
    $("p2pCopyId").onclick = async () => {
      const id = ($("p2pNodeId") && $("p2pNodeId").textContent) || "";
      if (!id || id === "–") return;
      try { await navigator.clipboard.writeText(id); toast(t("btn.copy")); } catch {}
    };
  }
  if ($("p2pStart")) {
    $("p2pStart").onclick = async () => {
      if ($("p2pNote")) $("p2pNote").textContent = "Starting…";
      try {
        const st = await invoke("p2p_start", {
          listen: ($("p2pListen") && $("p2pListen").value) || "",
          engine: p2pEngineFromUi(),
          bootstrap: p2pBootstrapFromUi() || null,
          mode: p2pModeFromUi(),
          donateCapacity: !!($("p2pDonate") && $("p2pDonate").checked),
        });
        if ($("p2pNote")) $("p2pNote").textContent = st.running
          ? ("Listening " + st.listen_addr + " · " + (st.engine || "mock") + " · " + (st.mode || "local"))
          : "Stopped";
        await refreshP2pStatus();
      } catch (e) {
        setP2pPill("error");
        if ($("p2pNote")) $("p2pNote").textContent = String(e);
      }
    };
  }
  if ($("p2pStop")) {
    $("p2pStop").onclick = async () => {
      try {
        await invoke("p2p_stop");
        if ($("p2pNote")) $("p2pNote").textContent = "Stopped";
        await refreshP2pStatus();
      } catch (e) {
        if ($("p2pNote")) $("p2pNote").textContent = String(e);
      }
    };
  }
  if ($("p2pProbe")) {
    $("p2pProbe").onclick = async () => {
      const addrs = p2pPeerFromUi() || p2pBootstrapFromUi();
      if (!addrs) {
        if ($("p2pNote")) $("p2pNote").textContent = "Enter a peer or bootstrap address";
        return;
      }
      if ($("p2pNote")) $("p2pNote").textContent = "Probing…";
      try {
        const list = await invoke("p2p_peers", { addrs });
        const out = $("p2pPeersOut");
        if (out) {
          out.hidden = false;
          out.textContent = (list || []).map((p) =>
            p.ok
              ? `✓ ${p.addr}  id=${(p.node_id || "").slice(0, 16)}…  ${p.backend}  models=${(p.models || []).join(",")}`
              : `✗ ${p.addr}  ${p.error || "fail"}`
          ).join("\n");
        }
        if ($("p2pNote")) {
          const ok = (list || []).filter((p) => p.ok).length;
          $("p2pNote").textContent = `Probed ${list.length} · ${ok} ok`;
        }
        await refreshP2pRecent();
      } catch (e) {
        if ($("p2pNote")) $("p2pNote").textContent = String(e);
      }
    };
  }
  if ($("p2pRefreshCredits")) {
    $("p2pRefreshCredits").onclick = () => refreshP2pCredits(
      ($("p2pSettlement") && $("p2pSettlement").dataset.jobId) || null
    );
  }
  async function runClusterJob(kind) {
    const prompt = ($("p2pAskPrompt") && $("p2pAskPrompt").value.trim())
      || (kind === "test" ? "hello slipstream p2p" : "");
    if (kind === "ask" && !prompt) {
      if ($("p2pNote")) $("p2pNote").textContent = "Enter a prompt";
      return;
    }
    if ($("p2pNote")) $("p2pNote").textContent = kind === "test" ? "Sending job…" : "P2P ask…";
    try {
      const args = {
        peer: p2pPeerFromUi() || (kind === "ask" ? null : ""),
        prompt,
        model: p2pModelFromUi(),
        system: null,
        maxTokens: p2pMaxTokensFromUi(kind === "test" ? 8 : 64),
        jobId: null,
      };
      const out = kind === "test"
        ? await invoke("p2p_send_test_job", { ...args, peer: p2pPeerFromUi() })
        : await invoke("p2p_chat", args);
      if ($("p2pNote")) {
        $("p2pNote").textContent = out.ok
          ? (`OK ${out.tokens} tok · job=${out.job_id} · ${(out.text || "").slice(0, 100)}`)
          : ("FAIL: " + (out.error || "unknown"));
      }
      if (out && out.ok) {
        toast(t("chat.viaP2p"));
        if ($("p2pSettlement")) $("p2pSettlement").dataset.jobId = out.job_id || "";
        await refreshP2pCredits(out.job_id);
      }
      await refreshP2pStatus();
    } catch (e) {
      if ($("p2pNote")) $("p2pNote").textContent = String(e);
    }
  }
  if ($("p2pSendJob")) $("p2pSendJob").onclick = () => runClusterJob("test");
  if ($("p2pAsk")) $("p2pAsk").onclick = () => runClusterJob("ask");
  if (state.p2p) refreshP2pStatus();
  // Keep Cluster status fresh while the tab is open.
  setInterval(() => {
    if (state.p2p && document.querySelector('.tab[data-tab="cluster"].tab-active')) {
      refreshP2pStatus();
    }
    updateChatP2pHint();
  }, 4000);
}

// ---- boot ------------------------------------------------------------------
async function boot() {
  try { state.def = await invoke("defaults"); } catch {}
  // One primary Models folder; sync MLX to <root>/mlx unless Advanced override.
  applyModelsRoot(defaultModelsRoot());
  ensureMlxCatalogDirs();
  try { await preferDefaultModelPaths(); } catch {}
  const saved = localStorage.getItem("pgrn.model");
  if (saved && MODELS.some((m) => m.id === saved)) state.model = MODELS.find((m) => m.id === saved);
  // Only probe an external GGUF base when the Models folder has no
  // GGUF yet. Prefer everything under the primary folder when both exist.
  const modelsRoot = defaultModelsRoot();
  if (!extBase && modelsRoot) {
    try {
      const m = state.model;
      const file = (m.quants && m.quants[0]) ? m.quants[0].file : m.file;
      const sub = (m.quants && m.quants[state.quantIdx||0] && m.quants[state.quantIdx||0].subdir) || m.subdir;
      const localGguf = `${modelsRoot}/${sub}/${file}`;
      const pgrn = `${modelsRoot}/${sub}/${String(file).replace(/\.gguf$/i, ".pgrn")}`;
      const local = await invoke("model_status", { gguf: localGguf, pgrn, dir: `${modelsRoot}/${sub}` });
      if (!(local && local.gguf_bytes > 0)) {
        const bases = await invoke("list_ext_model_bases");
        for (const base of bases || []) {
          const gguf = `${base}/${sub}/${file}`;
          const st = await invoke("model_status", { gguf, pgrn, dir: `${base}/${sub}` });
          if (st && st.gguf_bytes > 0) {
            extBase = base;
            localStorage.setItem("pgrn.extBase", extBase);
            break;
          }
        }
      }
    } catch {}
  }
  fillModels();
  $("pServer").value = state.def ? state.def.server_bin : "";
  selectModel(state.model.id);
  updateCacheRec();

  // file pickers under the path inputs
  addPicker("pModelsRoot", { directory: true }, "picker.folder");
  addPicker("pDir", { directory: true }, "picker.folder");
  addPicker("pPgrn", { directory: false }, "picker.pgrn");
  addPicker("pServer", { directory: false }, "picker.binary");
  addPicker("pMirror", { directory: false }, "picker.pgrn");
  addPicker("extBase", { directory: true }, "picker.folder");
  addPicker("pMlx", { directory: true }, "picker.folder");
  if ($("pModelsRoot")) $("pModelsRoot").value = defaultModelsRoot();
  $("extBase").value = extBase;
  if ($("backendSel")) {
    $("backendSel").value = state.backend;
    $("backendSel").onchange = () => {
      state.backend = parseBackendPref($("backendSel").value);
      state.resolvedBackend = null;
      localStorage.setItem("slipstream.backend", state.backend);
      applyBackendUi();
    };
  }
  if ($("pMlx") && state.model.mlx) $("pMlx").value = state.model.mlx.dir || defaultMlxDir();
  if ($("pModelsRoot")) {
    const persistRoot = () => {
      const v = normalizeRoot($("pModelsRoot").value);
      if (!v) return;
      applyModelsRoot(v);
      applyModelPaths();
      renderInstalled();
      refreshMlxCapability();
    };
    $("pModelsRoot").addEventListener("change", persistRoot);
    $("pModelsRoot").addEventListener("blur", persistRoot);
  }
  if ($("pMlx")) {
    const persistMlx = () => {
      let v = coerceMlxCatalogDir($("pMlx").value);
      if (v && $("pMlx").value.trim() && normalizeRoot($("pMlx").value) !== v) {
        $("pMlx").value = v;
      }
      if (v) {
        localStorage.setItem("slipstream.mlxDir", v);
        if (state.model && state.model.mlx) state.model.mlx.dir = v;
        MODELS.forEach((m) => { if (m.mlx) m.mlx.dir = v; });
      }
      refreshMlxCapability();
    };
    $("pMlx").addEventListener("change", persistMlx);
    $("pMlx").addEventListener("blur", persistMlx);
  }
  wirePgrnMlxSettings();
  if ($("mlxRuntimeBtn")) {
    $("mlxRuntimeBtn").onclick = async () => {
      try {
        const msg = await invoke("install_mlx_runtime");
        toast(msg || t("toast.mlxRuntimeStarted"));
        refreshMlxCapability();
        const poll = setInterval(async () => {
          await refreshMlxCapability();
          try {
            const raw = await invoke("mlx_runtime_status");
            const st = typeof raw === "string" ? JSON.parse(raw) : raw;
            if (st && st.state === "ready") {
              clearInterval(poll);
              toast(t("toast.mlxRuntimeReady"));
              refreshMlxCapability();
            } else if (st && st.state === "failed") {
              clearInterval(poll);
              toast(st.detail || "MLX runtime install failed", true);
            }
          } catch (_) { /* keep polling */ }
        }, 3000);
        setTimeout(() => clearInterval(poll), 45 * 60 * 1000);
      } catch (e) { toast(e, true); }
    };
  }
  applyBackendUi();
  await refreshNativeRuntimeStatus(effectiveBackend());
  $("extBase").onchange = (e) => {
    extBase = e.target.value.trim().replace(/\/+$/, "");
    localStorage.setItem("pgrn.extBase", extBase);
    applyModelPaths();
    renderInstalled();
  };
  renderAgents();
  $("lang").onchange = (e) => { applyLang(e.target.value); setPill(state.running ? "loading" : "off"); };
  applyLang(LANG);
  renderSpeed();
  renderInstalled();
  initArena();
  initChat();
  initStatus();
  initP2p();

  // Core counts are hardware, so they are read once; every changing number the
  // planner needs rides along with the live reading instead.
  try {
    const hardware = await invoke("system_stats");
    state.sys = { ...(state.sys || {}), cores: hardware.cores, perf_cores: hardware.perf_cores };
  } catch {}

  setInterval(poll, 1000);
  setInterval(refreshLogs, 1500);
  setInterval(refreshIndex, 1500);
  setInterval(renderInstalled, 4000);
  // Once a second, because the SSD-throughput trace is a delta between two of
  // these readings and a three-second step would flatten every burst. The serving
  // and system figures inside it change on the collector's own three-second beat.
  setInterval(refreshStatus, 1000);
  poll();
  refreshIndex();
  refreshStatus();
}
boot();
