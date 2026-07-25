"use strict";
const T = window.__TAURI__;
const invoke = T.core.invoke;
const dialog = T.dialog;
const $ = (id) => document.getElementById(id);
const PORT = 8080;
const EXPERT_MIB = 1.83; // avg bytes per streamed expert (35B geometry)
const EXT_BASE = "/Volumes/Crucial X10/Modelle"; // external SSD (big, slow) - editable in UI
// Storage split (measured 2.7x on Laguna): PGRN is streamed continuously -> fastest
// disk (internal NVMe); GGUF is read once at load -> can live on the slow external.

// ---- compatible models (seed the dropdown) --------------------------------
// url: HF repo "owner/name" -> resolve URL is built; or a full https URL.
const MODELS = [
  { id: "qwen3.6-35b", name: "Qwen3.6-35B-A3B (MTP)", subdir: "qwen3.6-35b-a3b-q4",
    repo: "unsloth/Qwen3.6-35B-A3B-Instruct-GGUF", file: "Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf",
    gb: 21, mtp: true, activeB: 3, spec: "draft-mtp", draft: "", extGguf: true,
    note: "note.qwen36" },
  { id: "qwen3-30b", name: "Qwen3-30B-A3B", subdir: "qwen3-30b-a3b-q4",
    repo: "unsloth/Qwen3-30B-A3B-GGUF", file: "Qwen3-30B-A3B-UD-Q4_K_XL.gguf",
    gb: 18, mtp: false, activeB: 3, spec: "none", draft: "",
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
    gb: 290, mtp: false, activeB: 35, spec: "none", draft: "", extGguf: true,
    note: "note.qwencoder" },
  { id: "llama4-maverick", name: "Llama 4 Maverick (400B-A17B, XL)", subdir: "llama4-maverick-q4",
    repo: "unsloth/Llama-4-Maverick-17B-128E-Instruct-GGUF", file: "Llama-4-Maverick-17B-128E-Instruct-Q4_K_M-00001-of-00005.gguf",
    gb: 243, mtp: false, activeB: 17, spec: "none", draft: "", extGguf: true,
    note: "note.maverick" },
  { id: "deepseek-v3", name: "DeepSeek V3 (671B-A37B, XL)", subdir: "deepseek-v3-q4",
    repo: "unsloth/DeepSeek-V3-GGUF", file: "DeepSeek-V3-Q4_K_M-00001-of-00009.gguf",
    gb: 340, mtp: false, activeB: 37, spec: "none", draft: "", extGguf: true,
    note: "note.dsv3" },
  { id: "deepseek-r1", name: "DeepSeek R1 (671B-A37B, Reasoning, XL)", subdir: "deepseek-r1-q4",
    repo: "unsloth/DeepSeek-R1-GGUF", file: "DeepSeek-R1-Q4_K_M-00001-of-00009.gguf",
    gb: 404, mtp: false, activeB: 37, spec: "none", draft: "", extGguf: true,
    note: "note.dsr1" },
  { id: "glm-5.2", name: "GLM-5.2 (744B-A40B, XL)", subdir: "glm-5.2-q4",
    repo: "unsloth/GLM-5.2-GGUF", file: "GLM-5.2-Q4_K_M-00001-of-00010.gguf",
    gb: 466, mtp: false, activeB: 40, spec: "none", draft: "", extGguf: true,
    note: "note.glm52" },
  // --- coming soon: real MoE, but not yet merged into mainline llama.cpp (fork/PR only) ---
  { id: "minimax-m3", name: "MiniMax M3 (428B-A23B)", subdir: "minimax-m3-q4",
    repo: "unsloth/MiniMax-M3-GGUF", file: "", gb: 264, mtp: false, activeB: 23,
    spec: "none", draft: "", extGguf: true, soon: true, note: "note.minimax" },
  { id: "deepseek-v4-flash", name: "DeepSeek V4 Flash (284B-A13B)", subdir: "deepseek-v4-flash-q4",
    repo: "", file: "", gb: 146, mtp: false, activeB: 13,
    spec: "none", draft: "", extGguf: true, soon: true, note: "note.v4flash" },
];

const state = {
  def: null,          // defaults() from backend
  model: MODELS[0],
  running: false,
  lastMisses: null, lastT: null,
  ssd: [], hit: [], tps: [],  // rolling buffers
  remoteBytes: 0,
};

// ---- i18n ------------------------------------------------------------------
const I18N = {
  en: {
    "header.sub": "Large coding models, local on your Mac — streamed from SSD",
    "nav.dashboard": "Dashboard", "nav.models": "Models", "nav.agent": "Coding Agent", "nav.debug": "Debug",
    "btn.selectModel": "Select model…", "pill.stopped": "○ Stopped", "pill.starting": "◐ Starting…", "pill.running": "● Running",
    "btn.start": "Start", "btn.stop": "Stop",
    "tile.ram": "Free RAM", "tile.usability": "Usability", "tile.decode": "Decode", "tile.server": "Server",
    "amp.smooth": "Flüssig", "amp.borderline": "Grenzwertig", "amp.pressure": "Druck", "srv.ready": "bereit", "srv.loading": "lädt Modell…", "srv.stopped": "gestoppt", "srv.noModel": "kein Modell", "ram.of": "von", "ram.total": "gesamt", "tile.decodeNote": "aus letztem Request", "reco.for": "Für deinen Mac", "reco.free": "frei", "reco.with": "mit", "reco.pgrnFast": "PGRN auf schnellste SSD (gestreamt)",
    "amp.smooth": "Smooth", "amp.borderline": "Borderline", "amp.pressure": "Pressure", "srv.ready": "ready", "srv.loading": "loading model…", "srv.stopped": "stopped", "srv.noModel": "no model", "ram.of": "of", "ram.total": "total", "tile.decodeNote": "from last request", "reco.for": "For your Mac", "reco.free": "free", "reco.with": "with", "reco.pgrnFast": "PGRN on fastest SSD (streamed)",
    "sec.liveMonitor": "Live monitor", "sec.settings": "Settings", "sec.selectModel": "Choose model",
    "sec.connectAgent": "Connect coding agent", "sec.indexing": "Indexing", "sec.test": "Test", "sec.logs": "Logs & diagnostics",
    "reco.title": "Best settings for your Mac", "btn.applyBest": "Apply best",
    "lbl.cache": "Cache size", "lbl.context": "Context", "lbl.io": "I/O threads", "lbl.thinking": "Thinking", "lbl.mtp": "MTP speed", "lbl.compact": "Compact (faster)", "lbl.model": "Model",
    "btn.download": "Download", "btn.convert": "Convert", "btn.cancel": "Cancel", "btn.send": "Send", "btn.setupStart": "Set up & start",
  },
  de: {
    "header.sub": "Große Coding-Modelle lokal auf dem Mac — von SSD gestreamt",
    "nav.dashboard": "Dashboard", "nav.models": "Modelle", "nav.agent": "Coding-Agent", "nav.debug": "Debug",
    "btn.selectModel": "Modell wählen…", "pill.stopped": "○ Gestoppt", "pill.starting": "◐ Startet…", "pill.running": "● Läuft",
    "btn.start": "Start", "btn.stop": "Stop",
    "tile.ram": "Freier RAM", "tile.usability": "Usability", "tile.decode": "Decode", "tile.server": "Server",
    "sec.liveMonitor": "Live-Monitor", "sec.settings": "Einstellungen", "sec.selectModel": "Modell wählen",
    "sec.connectAgent": "Coding-Agent verbinden", "sec.indexing": "Indexierung", "sec.test": "Test", "sec.logs": "Logs & Diagnose",
    "reco.title": "Auto-Empfehlung für deinen Mac", "btn.applyBest": "Beste anwenden",
    "lbl.cache": "Cache-Größe", "lbl.context": "Kontext", "lbl.io": "I/O-Threads", "lbl.thinking": "Thinking", "lbl.mtp": "MTP-Speed", "lbl.compact": "Compact (schneller)", "lbl.model": "Modell",
    "btn.download": "Herunterladen", "btn.convert": "Konvertieren", "btn.cancel": "Abbrechen", "btn.send": "Senden", "btn.setupStart": "Einrichten & starten",
  },
  zh: {
    "header.sub": "在 Mac 上本地运行大型编程模型 — 从 SSD 流式加载",
    "nav.dashboard": "仪表盘", "nav.models": "模型", "nav.agent": "编程助手", "nav.debug": "调试",
    "btn.selectModel": "选择模型…", "pill.stopped": "○ 已停止", "pill.starting": "◐ 启动中…", "pill.running": "● 运行中",
    "btn.start": "启动", "btn.stop": "停止",
    "tile.ram": "可用内存", "tile.usability": "可用性", "tile.decode": "解码", "tile.server": "服务",
    "sec.liveMonitor": "实时监控", "sec.settings": "设置", "sec.selectModel": "选择模型",
    "sec.connectAgent": "连接编程助手", "sec.indexing": "索引", "sec.test": "测试", "sec.logs": "日志与诊断",
    "reco.title": "为你的 Mac 推荐最佳设置", "btn.applyBest": "应用最佳",
    "lbl.cache": "缓存大小", "lbl.context": "上下文", "lbl.io": "I/O 线程", "lbl.thinking": "思考", "lbl.mtp": "MTP 加速", "lbl.model": "模型",
    "btn.download": "下载", "btn.convert": "转换", "btn.cancel": "取消", "btn.send": "发送", "btn.setupStart": "设置并启动",
  },
  es: {
    "header.sub": "Modelos de código grandes, locales en tu Mac — transmitidos desde el SSD",
    "nav.dashboard": "Panel", "nav.models": "Modelos", "nav.agent": "Agente", "nav.debug": "Depurar",
    "btn.selectModel": "Elegir modelo…", "pill.stopped": "○ Detenido", "pill.starting": "◐ Iniciando…", "pill.running": "● En marcha",
    "btn.start": "Iniciar", "btn.stop": "Parar",
    "tile.ram": "RAM libre", "tile.usability": "Usabilidad", "tile.decode": "Decodif.", "tile.server": "Servidor",
    "sec.liveMonitor": "Monitor en vivo", "sec.settings": "Ajustes", "sec.selectModel": "Elegir modelo",
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
    "tok.prompt": "prompt", "tok.answer": "answer", "tok.session": "tokens (session)",
    "chart.hit": "Cache hit-rate", "chart.hitNote": "served resident (no SSD read)",
    "chart.tokens": "Tokens", "chart.tokensNote": "prefill + decode",
    "reco.computing": "Computing…", "toast.noSys": "No system data yet", "toast.applied": "Best settings applied",
    "path.gguf": "GGUF folder", "path.pgrn": "PGRN path (streamed)",
    "path.source": "Download source (HuggingFace repo/URL)", "path.binary": "Server binary (our llama.cpp engine)",
    "adv.mirror": "Advanced: 2nd SSD mirror (dual-SSD)", "adv.mirrorPh": "path to a byte-identical .pgrn copy on a 2nd fast disk", "adv.mirrorWarn": "⚠ Only helps with two equally-fast SSDs (2× NVMe or TB4). On internal + slow USB it's slower — leave empty.", "adv.buffered": "Advanced: buffered reads (non-NVMe drives)", "adv.predict": "Advanced: predictive prefetch (experimental)",
    "path.summary": "Source & location",
    "picker.folder": "Choose folder…", "picker.pgrn": "Choose PGRN…", "picker.binary": "Choose binary…",
    "compat.text": "<b>Compatibility:</b> our engine streams <b>experts</b> from SSD — compatible are <b>MoE models</b> with <b>Q4_K/Q5_K/Q6_K</b> experts whose architecture llama.cpp knows (Qwen3-MoE, DeepSeek, Mixtral, GLM-4.5-MoE, Laguna). Not: dense models, IQ/Q2/Q3/Q8_0/MXFP4.",
    "agent.intro": "Slipstream is OpenAI-compatible. One click per agent: the config is patched directly (Kilo/OpenCode) or placed on your clipboard.",
    "agent.patch": "Patch in", "agent.copy": "Copy config", "agent.tagPatch": "1-click patch", "agent.tagCopy": "copy values / config",
    "idx.setup": "Set up & start", "idx.stop": "Stop all",
    "idx.intro": "One click: download the embed model (~100 MB) + Qdrant (~30 MB), start both and patch the agent.",
    "idx.valuesFor": "Values for your agent (Codebase Indexing -> OpenAI Compatible)",
    "idx.hint": "Then enable 'Codebase Indexing' in your agent once, enter the values above, hit 'Start Indexing'. The index stays local in Qdrant.",
    "idx.doneStep": "Done — enable Codebase Indexing in your agent, then restart it.", "idx.doneToast": "Indexing set up",
    "test.placeholder": "Prompt, e.g.: Write a Python function is_prime(n).", "test.noThink": "without thinking (faster)", "test.reasoning": "Reasoning",
    "logs.autoscroll": "Auto-scroll", "logs.diag": "Copy diagnostics",
    "logs.server": "Server", "logs.download": "Download", "logs.convert": "Conversion",
    "badge.ready": "ready", "badge.partial": "partial", "badge.missing": "not set up", "badge.notLoaded": "not loaded",
    "st.ready": "Ready — can be started.", "st.loaded": "loaded — ready", "st.needConvert": "convert needed",
    "st.notThere": "not present — download it.", "st.dlRunning": "Download running…", "st.convRunning": "Converting… (GGUF -> PGRN)",
    "reco.cons": "Conservative — runs on 16 GiB Macs, more SSD reads.", "reco.rec": "<b>Recommended</b> for interactive coding on 36 GiB — Mac stays smooth.",
    "reco.fast": "Fast — needs lots of free RAM; close apps first.", "reco.aggr": "Aggressive — only with lots of free RAM, else swapping.",
    "act.prefill": "Prefill — reading the prompt", "act.decode": "Generating answer", "act.idleReady": "Ready — waiting for a request", "act.stopped": "Server stopped", "act.running": "running…",
    "note.qwen36": "Strongest compatible coder with MTP speed.", "note.qwen30": "Smaller, no MTP — good for weaker Macs.",
    "note.deepseek": "Small & fast, lowest RAM need.", "note.glm": "Large, lots of disk — strong quality.",
    "note.laguna": "Strongest model. Tip: PGRN on the fastest SSD (streamed), GGUF anywhere (load only).",
    "note.qwencoder": "Coding MoE, 35B active. XL — big fast SSD + PGRN sidecar needed.", "note.maverick": "Fastest decode of the giants (17B active). XL — big SSD + PGRN.",
    "note.dsv3": "671B/37B all-rounder. XL — big SSD + PGRN sidecar.", "note.dsr1": "Reasoning model — thinking tokens slow agent use. XL — big SSD.",
    "note.glm52": "Top-tier quality, 466 GB — needs a large SSD or a lower quant.", "note.minimax": "23B active, 264 GB — arrives once merged into llama.cpp.", "note.v4flash": "Ideal fit (13B active, 146 GB) — arrives with mainline llama.cpp.",
    "badge.soon": "soon", "btn.send": "Send",
    "installed.title": "Installed models", "speed.title": "Expected speed", "speed.external": "external SSD - slower",
  },
  de: {
    "chart.ssd": "SSD-Durchsatz", "chart.ssdNote": "Experten von SSD gestreamt",
    "chart.arena": "Experten-Cache — live", "arena.resident": "resident (Treffer)", "arena.stream": "streamt von SSD",
    "tok.prompt": "Prompt", "tok.answer": "Antwort", "tok.session": "Tokens (Session)",
    "chart.hit": "Cache-Hit-Rate", "chart.hitNote": "resident bedient (kein SSD-Read)",
    "chart.tokens": "Tokens", "chart.tokensNote": "Prefill + Decode",
    "reco.computing": "Ermittle Werte…", "toast.noSys": "Noch keine Systemdaten", "toast.applied": "Beste Einstellungen angewendet",
    "path.gguf": "GGUF-Ordner", "path.pgrn": "PGRN-Pfad (gestreamt)",
    "path.source": "Download-Quelle (HuggingFace-Repo/URL)", "path.binary": "Server-Binary (unsere llama.cpp-Engine)",
    "adv.mirror": "Advanced: 2. SSD-Mirror (Dual-SSD)", "adv.mirrorPh": "Pfad zu einer byte-identischen .pgrn-Kopie auf einer 2. schnellen Disk", "adv.mirrorWarn": "⚠ Nur bei zwei gleich schnellen SSDs (2× NVMe oder TB4). Auf intern + langsamer USB ist es langsamer — dann leer lassen.", "adv.buffered": "Advanced: gepufferte Reads (Nicht-NVMe-Laufwerke)", "adv.predict": "Advanced: Predictive Prefetch (experimentell)",
    "path.summary": "Quelle & Speicherort",
    "picker.folder": "Ordner wählen…", "picker.pgrn": "PGRN wählen…", "picker.binary": "Binary wählen…",
    "compat.text": "<b>Kompatibilität:</b> unsere Engine streamt <b>Experten</b> von SSD — kompatibel sind <b>MoE-Modelle</b> mit <b>Q4_K/Q5_K/Q6_K</b>-Experten, deren Architektur llama.cpp kennt (Qwen3-MoE, DeepSeek, Mixtral, GLM-4.5-MoE, Laguna). Nicht: Dense-Modelle, IQ/Q2/Q3/Q8_0/MXFP4.",
    "agent.intro": "Slipstream ist OpenAI-kompatibel. Ein Klick pro Agent: Config wird direkt gepatcht (Kilo/OpenCode) oder in die Zwischenablage gelegt.",
    "agent.patch": "Einpatchen", "agent.copy": "Config kopieren", "agent.tagPatch": "1-Klick-Patch", "agent.tagCopy": "Werte / Config kopieren",
    "idx.setup": "Einrichten & starten", "idx.stop": "Alles stoppen",
    "idx.intro": "Ein Klick: Embed-Modell (~100 MB) + Qdrant (~30 MB) laden, beide starten und Agent patchen.",
    "idx.valuesFor": "Werte für deinen Agenten (Codebase Indexing -> OpenAI Compatible)",
    "idx.hint": "Danach im Agenten einmalig 'Codebase Indexing' aktivieren, obige Werte eintragen, 'Start Indexing'. Der Index bleibt lokal in Qdrant.",
    "idx.doneStep": "Fertig — im Agenten Codebase Indexing aktivieren, dann neu starten.", "idx.doneToast": "Indexierung eingerichtet",
    "test.placeholder": "Prompt, z. B.: Schreibe eine Python-Funktion is_prime(n).", "test.noThink": "ohne Thinking (schneller)", "test.reasoning": "Reasoning",
    "logs.autoscroll": "Auto-Scroll", "logs.diag": "Diagnose kopieren",
    "logs.server": "Server", "logs.download": "Download", "logs.convert": "Konvertierung",
    "badge.ready": "bereit", "badge.partial": "teilweise", "badge.missing": "nicht eingerichtet", "badge.notLoaded": "nicht geladen",
    "st.ready": "Bereit — kann gestartet werden.", "st.loaded": "geladen — bereit", "st.needConvert": "konvertieren nötig",
    "st.notThere": "nicht vorhanden — herunterladen.", "st.dlRunning": "Download läuft…", "st.convRunning": "Konvertierung läuft… (GGUF -> PGRN)",
    "reco.cons": "Konservativ — läuft auf 16 GiB Macs, mehr SSD-Reads.", "reco.rec": "<b>Empfohlen</b> für interaktives Coding auf 36 GiB — Mac bleibt flüssig.",
    "reco.fast": "Schnell — braucht viel freien RAM; erst Apps schließen.", "reco.aggr": "Aggressiv — nur bei viel freiem RAM, sonst Swapping.",
    "act.prefill": "Prefill — Prompt wird gelesen", "act.decode": "Antwort wird generiert", "act.idleReady": "Bereit — wartet auf Anfrage", "act.stopped": "Server gestoppt", "act.running": "läuft…",
    "note.qwen36": "Stärkster kompatibler Coder mit MTP-Speed.", "note.qwen30": "Kleiner, ohne MTP — gut für schwächere Macs.",
    "note.deepseek": "Klein & schnell, geringster RAM-Bedarf.", "note.glm": "Groß, viel Disk — starke Qualität.",
    "note.laguna": "Stärkstes Modell. Tipp: PGRN auf die schnellste SSD (gestreamt), GGUF egal (nur Load).",
    "note.qwencoder": "Coding-MoE, 35B aktiv. XL — große schnelle SSD + PGRN-Sidecar nötig.", "note.maverick": "Schnellster Decode der Riesen (17B aktiv). XL — große SSD + PGRN.",
    "note.dsv3": "671B/37B Allrounder. XL — große SSD + PGRN-Sidecar.", "note.dsr1": "Reasoning-Modell — Thinking-Tokens bremsen Agent. XL — große SSD.",
    "note.glm52": "Top-Qualität, 466 GB — braucht große SSD oder kleineren Quant.", "note.minimax": "23B aktiv, 264 GB — kommt, sobald in llama.cpp gemerged.", "note.v4flash": "Idealer Fit (13B aktiv, 146 GB) — kommt mit Mainline-llama.cpp.",
    "badge.soon": "bald", "btn.send": "Senden",
    "installed.title": "Installierte Modelle", "speed.title": "Erwartete Geschwindigkeit", "speed.external": "externe SSD - langsamer",
  },
  zh: {}, es: {},
};
Object.keys(I18N).forEach((l) => Object.assign(I18N[l], I18N_EXT[l] || {}));

// Hover tooltips (EN + DE; zh/es fall back to EN).
const TIPS = {
  en: {
    "tip.ram": "RAM available to the OS. Cache + KV + reserve must fit so the Mac stays usable.",
    "tip.usability": "Green = Mac stays smooth. Yellow/red = memory pressure / swapping — shrink the cache.",
    "tip.decode": "Generation speed (tokens/s) from the last run.",
    "tip.liveMonitor": "Real values from the streaming kernel log — not an estimate.",
    "tip.cache": "How many experts stay resident in RAM. Bigger = faster (more hits) but needs more free RAM.",
    "tip.ctx": "Window for prompt + history. Coding agents send ~30k+ tokens, so 40k is the best compromise. 32k can overflow; 64k causes memory pressure.",
    "tip.io": "Parallel SSD read threads for fetching experts. 8 = much faster prefill (large agent prompts); 1 = conservative.",
    "tip.mirror": "Stripe expert reads across two disks, split by measured bandwidth. Wins only with two comparably-fast, independent SSDs; a slow USB drive makes it slower. Parity is CRC-checked.",
    "tip.predict": "Learns which experts co-fire between layers, live, and prefetches the next layer's likely experts during compute. Experimental — verify with an A/B; parity-safe (warms cache only).",
    "tip.mtp": "Speculative decoding via multi-token prediction. Only for models with an MTP/DFlash draft; off otherwise.",
    "tip.compact": "Zero-copy expert slots: the GPU reads experts straight from the cache — no re-upload copy. Measured +13–24% decode at moderate cache, neutral at high, swap-safe. On by default.",
    "tip.thinking": "The model's reasoning mode. Leave OFF for agentic coding: otherwise it loops in endless thinking and burns the token budget before answering. ON only for hard single questions.",
    "tip.model": "Compatible = MoE architecture + Q4_K/Q5_K/Q6_K experts + known to llama.cpp.",
    "tip.gguf": "The GGUF is read only at load — it may live on the slow external SSD.",
    "tip.pgrn": "The PGRN is streamed continuously during every answer — put it on the FASTEST SSD (internal NVMe). Measured: 2.7x faster than external.",
    "tip.indexing": "Semantic code search: instead of stuffing everything into the prompt, your agent fetches only relevant code. Needs an embedding model (runs on the GPU) + a vector DB (Qdrant) — both startable here.",
    "tip.embedder": "Our llama-server in --embedding mode with Nomic-Embed (~100 MB). Turns code into vectors.",
    "tip.qdrant": "Local vector database (release binary, ~30 MB). Stores your code's index.",
    "tip.test": "Send a prompt to the model and check answer + speed.",
  },
  de: {
    "tip.ram": "Vom Betriebssystem verfügbarer Speicher. Cache + KV + Reserve müssen reinpassen, damit der Mac nutzbar bleibt.",
    "tip.usability": "Grün = Mac bleibt flüssig. Gelb/Rot = Speicherdruck / Swapping — Cache verkleinern.",
    "tip.decode": "Generierungs-Geschwindigkeit (Tokens/s) aus dem letzten Lauf.",
    "tip.liveMonitor": "Echte Werte aus dem Streaming-Kernel-Log — keine Schätzung.",
    "tip.cache": "Wie viele Experten resident im RAM bleiben. Größer = schneller (mehr Treffer), braucht aber mehr freien RAM.",
    "tip.ctx": "Fenster für Prompt + Verlauf. Coding-Agenten schicken ~30k+ Tokens, daher ist 40k der beste Kompromiss. 32k kann überlaufen; 64k erzeugt Speicherdruck.",
    "tip.io": "Parallele SSD-Lesethreads beim Experten-Holen. 8 = deutlich schnellerer Prefill (große Agenten-Prompts); 1 = konservativ.",
    "tip.mirror": "Verteilt Experten-Reads bandbreiten-proportional über zwei Disks. Gewinnt nur mit zwei ähnlich schnellen, unabhängigen SSDs; eine langsame USB-Disk macht es langsamer. Parity ist CRC-geprüft.",
    "tip.predict": "Lernt live, welche Experten zwischen Layern gemeinsam feuern, und lädt die wahrscheinlichen nächsten Experten während des Rechnens vor. Experimentell — per A/B prüfen; parity-safe (wärmt nur den Cache).",
    "tip.mtp": "Spekulatives Decoding via Multi-Token-Prediction. Nur bei Modellen mit MTP/DFlash-Draft; sonst automatisch aus.",
    "tip.compact": "Zero-Copy-Experten-Slots: die GPU liest Experten direkt aus dem Cache — keine Re-Upload-Kopie. Gemessen +13–24% Decode bei moderatem Cache, neutral bei hohem, swap-safe. Standardmäßig an.",
    "tip.thinking": "Reasoning-Modus des Modells. Für Agenten-Coding AUS lassen: sonst verheddert es sich im Endlos-Denken und verbraucht das Token-Budget, bevor es antwortet. AN nur für schwere Einzelfragen.",
    "tip.model": "Kompatibel = MoE-Architektur + Q4_K/Q5_K/Q6_K-Experten + von llama.cpp unterstützt.",
    "tip.gguf": "Das GGUF wird nur beim Laden gelesen — darf auf der langsamen externen SSD liegen.",
    "tip.pgrn": "Die PGRN wird während jeder Antwort ständig gestreamt — auf die SCHNELLSTE SSD legen (interne NVMe). Gemessen: 2,7x schneller als extern.",
    "tip.indexing": "Semantische Codesuche: statt alles in den Prompt zu stopfen, holt der Agent nur relevante Code-Stellen. Braucht ein Embedding-Modell (läuft auf der GPU) + eine Vektor-DB (Qdrant) — beide hier startbar.",
    "tip.embedder": "Unser llama-server im --embedding-Modus mit Nomic-Embed (~100 MB). Wandelt Code in Vektoren.",
    "tip.qdrant": "Lokale Vektor-Datenbank (Release-Binary, ~30 MB). Speichert den Index deines Codes.",
    "tip.test": "Prompt an das Modell schicken und Antwort + Speed prüfen.",
  },
};
Object.keys(TIPS).forEach((l) => Object.assign(I18N[l], TIPS[l]));

// Toasts, statuses, setup steps (EN + DE; zh/es fall back to EN).
const MISC = {
  en: {
    "toast.canceled": "Canceled", "toast.pickCanceled": "Selection canceled", "toast.noDialog": "File dialog unavailable",
    "toast.diagCopied": "Diagnostics copied", "toast.dlStarted": "Download started", "toast.convStarted": "Conversion started",
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
    "how.cline": "In VS Code: Cline -> Settings -> API Provider = \"OpenAI Compatible\", enter the values.",
    "how.roo": "In VS Code: Roo Code -> Settings -> API Provider = \"OpenAI Compatible\", enter the values.",
    "how.cursor": "In Cursor: Settings -> Models -> set OpenAI API Key + \"Override OpenAI Base URL\" = the Base URL.",
    "how.continue": "Paste into ~/.continue/config.yaml under models:.",
    "how.aider": "Paste into ~/.aider.conf.yml (or as CLI flags).",
  },
  de: {
    "toast.canceled": "Abgebrochen", "toast.pickCanceled": "Auswahl abgebrochen", "toast.noDialog": "Datei-Dialog nicht verfügbar",
    "toast.diagCopied": "Diagnose kopiert", "toast.dlStarted": "Download gestartet", "toast.convStarted": "Konvertierung gestartet",
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
    "how.cline": "In VS Code: Cline -> Settings -> API Provider = \"OpenAI Compatible\", Werte einfügen.",
    "how.roo": "In VS Code: Roo Code -> Settings -> API Provider = \"OpenAI Compatible\", Werte einfügen.",
    "how.cursor": "In Cursor: Settings -> Models -> OpenAI API Key setzen + \"Override OpenAI Base URL\" = die Base URL.",
    "how.continue": "In ~/.continue/config.yaml unter models: einfügen.",
    "how.aider": "In ~/.aider.conf.yml einfügen (oder als CLI-Flags).",
  },
};
Object.keys(MISC).forEach((l) => Object.assign(I18N[l], MISC[l]));

let LANG = localStorage.getItem("slipstream.lang") ||
  (navigator.language || "en").slice(0, 2);
if (!I18N[LANG]) LANG = "en";
function t(key) { return (I18N[LANG] && I18N[LANG][key]) || I18N.en[key] || key; }
function applyLang(lang) {
  if (I18N[lang]) LANG = lang;
  localStorage.setItem("slipstream.lang", LANG);
  document.documentElement.lang = LANG;
  document.querySelectorAll("[data-i18n]").forEach((el) => { el.textContent = t(el.dataset.i18n); });
  document.querySelectorAll("[data-i18n-tip]").forEach((el) => { el.setAttribute("data-tip", t(el.dataset.i18nTip)); });
  document.querySelectorAll("[data-i18n-html]").forEach((el) => { el.innerHTML = t(el.dataset.i18nHtml); });
  document.querySelectorAll("[data-i18n-ph]").forEach((el) => { el.setAttribute("placeholder", t(el.dataset.i18nPh)); });
  const ls = $("lang"); if (ls) ls.value = LANG;
  try { renderModelNote(); } catch {}
  try { updateReco(); } catch {}
  try { renderAgents(); } catch {}
  try { updateCacheRec(); } catch {}
  try { renderSpeed(); } catch {}
  try { renderInstalled(); } catch {}
}

// ---- tiny helpers ----------------------------------------------------------
function toast(msg, err) {
  const t = $("toast");
  t.textContent = msg;
  t.className = "toast show" + (err ? " err" : "");
  clearTimeout(t._h);
  t._h = setTimeout(() => (t.className = "toast"), 2600);
}
const fmtGiB = (b) => (b / 1073741824).toFixed(1);
function pgrnOf(gguf) { return gguf.replace(/\.gguf$/i, ".pgrn"); }
function modelDir() { return $("pDir").value.trim(); }
function ggufPath() { return modelDir() + "/" + state.model.file; }
function urlFor(m) {
  if (!m.repo) return "";
  return `https://huggingface.co/${m.repo}/resolve/main/${m.file}`;
}

// ---- tabs ------------------------------------------------------------------
document.querySelectorAll(".tab").forEach((tab) => {
  tab.onclick = () => {
    document.querySelectorAll(".tab").forEach((t) => t.classList.remove("tab-active"));
    tab.classList.add("tab-active");
    const name = tab.dataset.tab;
    document.querySelectorAll(".panel").forEach((p) => (p.hidden = p.dataset.panel !== name));
  };
});

// ---- info tooltips ---------------------------------------------------------
const tip = $("tooltip");
document.addEventListener("mouseover", (e) => {
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
  MODELS.forEach((m) => {
    const o = document.createElement("option");
    o.value = m.id; o.textContent = m.soon ? `${m.name} — ${t("badge.soon")}` : m.name;
    if (m.soon) o.disabled = true;
    sel.appendChild(o);
  });
  sel.value = state.model.id;
}
function selectModel(id) {
  state.model = MODELS.find((m) => m.id === id) || MODELS[0];
  const m = state.model;
  const base = state.def ? state.def.model_dir : "/Users/Modelle";
  // GGUF (load-only) can sit on the slow external; PGRN (streamed) on fast internal.
  $("pDir").value = (m.extGguf ? `${EXT_BASE}/${m.subdir}` : `${base}/${m.subdir}`);
  $("pPgrn").value = `${base}/${m.subdir}/${m.file.replace(/\.gguf$/i, ".pgrn")}`;
  $("pUrl").value = urlFor(state.model);
  renderModelNote();
  state.remoteBytes = 0;
  localStorage.setItem("pgrn.model", id);
  refreshModel();
}
function renderModelNote() {
  try { renderSpeed(); } catch {}
  const m = state.model, el = $("modelNote");
  if (!el) return;
  el.innerHTML = `${t(m.note)} &nbsp;&middot;&nbsp; ~${m.gb} GiB Download` +
    (m.mtp ? " &nbsp;&middot;&nbsp; MTP/DFlash-Speed" : "");
}
$("modelSel").onchange = (e) => selectModel(e.target.value);

// ---- file pickers ----------------------------------------------------------
async function pickInto(inputId, opts) {
  if (!dialog || !dialog.open) { toast(t("toast.noDialog"), true); return; }
  try {
    const cur = $(inputId).value.trim();
    const picked = await dialog.open(Object.assign({ defaultPath: cur || undefined }, opts));
    if (picked) $(inputId).value = picked;
  } catch (e) { toast(t("toast.pickCanceled"), true); }
}
function addPicker(inputId, opts, label) {
  const inp = $(inputId);
  const btn = document.createElement("button");
  btn.className = "btn btn-ghost"; btn.textContent = label || "Waehlen…";
  btn.style.marginTop = "6px";
  btn.onclick = () => pickInto(inputId, opts);
  inp.insertAdjacentElement("afterend", btn);
}

// ---- model status / download / convert -------------------------------------
async function refreshModel() {
  const gguf = ggufPath(), pgrn = ($("pPgrn").value.trim() || pgrnOf(gguf)), dir = modelDir();
  let st;
  try { st = await invoke("model_status", { gguf, pgrn, dir }); }
  catch { return; }
  state.mstatus = st;

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
    dlNote.textContent = `${fmtGiB(st.gguf_bytes)} / ${state.remoteBytes ? fmtGiB(state.remoteBytes) : "?"} GiB · frei: ${st.disk_free_gib.toFixed(0)} GiB`;
  } else if (st.converting) {
    const est = st.gguf_bytes * 0.9;
    const pct = est ? Math.min(100, (st.pgrn_bytes / est) * 100) : 0;
    badge.className = "badge partial"; badge.textContent = "konvertiert " + pct.toFixed(0) + "%";
    dlStatus.textContent = t("st.convRunning");
    dlBar.style.width = pct + "%";
    dlNote.textContent = `PGRN: ${fmtGiB(st.pgrn_bytes)} GiB`;
  } else if (st.pgrn_bytes > 0 && st.gguf_bytes > 0) {
    badge.className = "badge ready"; badge.textContent = t("badge.ready");
    dlStatus.textContent = t("st.ready");
    dlBar.style.width = "100%";
    dlNote.textContent = `GGUF ${fmtGiB(st.gguf_bytes)} GiB · PGRN ${fmtGiB(st.pgrn_bytes)} GiB · frei ${st.disk_free_gib.toFixed(0)} GiB`;
  } else if (st.gguf_bytes > 0) {
    badge.className = "badge partial"; badge.textContent = t("st.needConvert");
    dlStatus.textContent = t("st.notConverted");
    dlBar.style.width = "50%";
    dlNote.textContent = `GGUF ${fmtGiB(st.gguf_bytes)} GiB · frei ${st.disk_free_gib.toFixed(0)} GiB`;
  } else {
    badge.className = "badge missing"; badge.textContent = t("badge.notLoaded");
    dlStatus.textContent = t("st.notThere");
    dlBar.style.width = "0%";
    dlNote.textContent = `frei: ${st.disk_free_gib.toFixed(0)} GiB`;
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
  try {
    await invoke("start_convert", { repo: state.def ? state.def.repo : "", gguf: ggufPath(), pgrn: pgrnOf(ggufPath()) });
    toast(t("toast.convStarted")); refreshModel();
  } catch (e) { toast(e, true); }
};
$("dlCancel").onclick = async () => {
  await invoke("cancel_download"); await invoke("cancel_convert");
  toast(t("toast.canceled")); refreshModel();
};

// ---- server start/stop -----------------------------------------------------
async function startServer() {
  const draft = state.model.draft ? `${modelDir()}/${state.model.draft}` : "";
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
    spec_type: state.model.spec || "none",
    draft_model: draft,
    pgrn_mirror: ($("pMirror") && $("pMirror").value.trim()) || "",
    pgrn_buffered: !!($("bufferedReads") && $("bufferedReads").checked),
    pgrn_online: !!($("onlinePredict") && $("onlinePredict").checked),
    pgrn_compact: $("compactSlots") ? $("compactSlots").checked : true,
  };
  try {
    const msg = await invoke("start_server", { cfg }); toast(t("srv.startedLoading"));
    setPill("loading");
  } catch (e) { toast(e, true); }
}
async function stopServer() {
  await invoke("stop_server");
  setPill("off");
  toast(t("toast.serverStopped"));
}
$("powerBtn").onclick = () => (state.running ? stopServer() : startServer());

function setPill(mode) {
  const p = $("statusPill"), b = $("powerBtn");
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
  const avail = (s.free_gib && s.free_gib > 6 ? s.free_gib : s.total_gib) - resident - draftGb - 3 /*headroom*/ - 2 /*reserve*/;
  let cache = Math.max(4, Math.min(Math.floor(avail), 24, Math.ceil(pgrnGb)));
  // context: derived from BOTH total and free RAM (KV cache grows with context), snapped to options.
  const ctxOpts = [16384, 32768, 40960, 65536];
  const ctxCeil = s.total_gib >= 48 ? 65536 : s.total_gib >= 32 ? 40960 : s.total_gib >= 20 ? 32768 : 16384;
  const freeCap = s.free_gib >= 20 ? 65536 : s.free_gib >= 12 ? 40960 : s.free_gib >= 7 ? 32768 : 16384;
  let ctx = Math.min(ctxCeil, freeCap);
  if (!ctxOpts.includes(ctx)) ctx = ctxOpts.filter((o) => o <= ctx).pop() || 16384;
  // io threads: parallel cold-reads per layer stream -> scale with performance (P-)cores.
  const pcores = s.perf_cores || Math.round((s.cores || 8) * 0.6);
  const io = pcores <= 4 ? 4 : 8;
  return { cache, ctx, io, resident, pgrnGb };
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
    (state.model.draft ? ` &middot; <b>DFlash</b>` : (state.model.spec === "draft-mtp" ? ` &middot; <b>MTP</b>` : ""));
  state.reco = r;
}
function applyReco() {
  const r = state.reco || computeReco();
  if (!r) { toast(t("toast.noSys"), true); return; }
  $("cache").value = r.cache; $("cacheVal").textContent = r.cache; updateCacheRec();
  $("ctx").value = String(r.ctx);
  $("io").value = String(r.io);
  toast(`${t("toast.applied")}: Cache ${r.cache} · ${r.ctx / 1024}k · io ${r.io}`); renderSpeed();
}
$("applyReco").onclick = applyReco;

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
  el.innerHTML = `≈ <b>${s.est < 10 ? s.est.toFixed(1) : Math.round(s.est)} tok/s</b>` +
    (s.external ? ` <span class="agent-tag">(${t("speed.external")})</span>` : "");
}

// ---- installed models (what's already on disk) -----------------------------
async function renderInstalled() {
  const host = $("installedList"); if (!host || !state.def) return;
  const base = state.def.model_dir;
  const rows = await Promise.all(MODELS.map(async (m) => {
    if (m.soon) return { m, cls: "soon", txt: `~${m.gb} GiB · ${m.activeB}B active` };
    const ggufDir = m.extGguf ? `${EXT_BASE}/${m.subdir}` : `${base}/${m.subdir}`;
    const gguf = `${ggufDir}/${m.file}`;
    const pgrn = `${base}/${m.subdir}/${m.file.replace(/\.gguf$/i, ".pgrn")}`;
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
  let lastPrompt = null, lastComp = null, sumComp = 0;
  for (const l of lines) {
    let m = l.match(/hits = (\d+), misses = (\d+) \(([\d.]+)%\)/);
    if (m) { hits = +m[1]; misses = +m[2]; hitPct = +m[3]; }
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
  return { hitPct, hits, misses, tps, loaded, lastPrompt, lastComp, sumComp };
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
    el.className = "activity prefill";
    $("actTitle").textContent = t("act.prefill");
    $("actSub").textContent = `${a.pp.tokens.toLocaleString()} / ${total.toLocaleString()} Tokens`
      + (a.pp.rate ? ` · ${a.pp.rate.toFixed(0)} tok/s` : "")
      + (eta != null ? ` · ETA ~${eta}s` : "");
    $("actBar").style.width = p + "%"; $("actPct").textContent = p + "%";
  } else if (a.phase === "decode") {
    el.className = "activity decode";
    $("actTitle").textContent = t("act.decode");
    $("actSub").textContent = a.dec ? `${a.dec.tokens} Tokens · ${a.dec.tps.toFixed(1)} tok/s` : "läuft…";
    $("actBar").style.width = "100%"; $("actPct").textContent = "";
  } else {
    el.className = "activity idle";
    $("actTitle").textContent = state.running ? t("act.idleReady") : t("act.stopped");
    $("actSub").textContent = ""; $("actBar").style.width = "0%"; $("actPct").textContent = "";
  }
}

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
  try {
    const s = await invoke("system_stats");
    state.sys = s;
    $("ramValue").textContent = s.free_gib.toFixed(1);
    $("swapValue").textContent = s.swap_used_mb.toFixed(0);
    const pct = s.total_gib ? (s.free_gib / s.total_gib) * 100 : 0;
    $("ramBar").style.width = pct + "%";
    $("ramNote").textContent = `${t("ram.of")} ${s.total_gib.toFixed(0)} GiB ${t("ram.total")}`;
    let amp = "green", txt = t("amp.smooth");
    if (s.swap_used_mb > 800 || pct < 12) { amp = "red"; txt = t("amp.pressure"); }
    else if (s.swap_used_mb > 150 || pct < 22) { amp = "yellow"; txt = t("amp.borderline"); }
    $("ampel").className = "ampel ampel-" + amp;
    $("ampelText").textContent = txt;
    updateReco();
  } catch {}

  if (state.running) {
    try {
      const log = await invoke("read_log", { maxLines: 400 });
      const p = parseLog(log);
      const act = parseActivity(log);
      updateActivity(act);
      ARENA.active = act.phase === "decode" || act.phase === "prefill";
      const now = performance.now() / 1000;
      let ssd = 0;
      if (p.misses != null && state.lastMisses != null && state.lastT != null) {
        const dM = p.misses - state.lastMisses, dT = now - state.lastT;
        if (dM >= 0 && dT > 0) ssd = (dM * EXPERT_MIB) / dT;
      }
      state.lastMisses = p.misses; state.lastT = now;
      push(state.ssd, Math.max(0, ssd));
      if (p.hitPct != null) push(state.hit, p.hitPct);
      if (p.tps != null) push(state.tps, p.tps);
      ARENA.hit = p.hitPct != null ? p.hitPct : ARENA.hit;
      ARENA.ssd = ssd;

      $("ssdNow").textContent = ssd.toFixed(0);
      $("hitNow").textContent = p.hitPct != null ? p.hitPct.toFixed(0) : "0";
      $("tpsNow").textContent = p.tps != null ? p.tps.toFixed(1) : "0";
      $("tokValue").textContent = p.tps != null ? p.tps.toFixed(1) : "-";
      if (p.lastComp != null) {
        $("tokNote").textContent = `${p.lastPrompt != null ? p.lastPrompt + " " + t("tok.prompt") + " + " : ""}${p.lastComp} ${t("tok.answer")} · ${p.sumComp} ${t("tok.session")}`;
      }
      setPill(sstate === "ready" ? "on" : "loading");
      $("healthText").textContent = sstate === "ready" ? t("srv.ready") : t("srv.loading");
      $("activeModelNote").textContent = state.model.name;
    } catch {}
  } else {
    setPill("off");
    $("healthText").textContent = t("srv.stopped");
    $("activeModelNote").textContent = t("srv.noModel");
    state.lastMisses = null;
    ARENA.active = false;
  }

  drawSpark("ssdChart", state.ssd, "#ff9d2f");
  drawSpark("hitChart", state.hit, "#35d07f", 100);
  drawSpark("tpsChart", state.tps, "#4aa8ff");

  refreshModel();
}

// ---- coding agents (one-click connect) ------------------------------------
const BASE_URL = "http://127.0.0.1:8080/v1";
const AGENTS = [
  { id: "kilo", name: "Kilo Code", tag: "1-Klick-Patch · VS Code neu starten", action: "patch", target: "kilo", restart: "VS Code" },
  { id: "opencode", name: "OpenCode", tag: "1-Klick-Patch · OpenCode neu starten", action: "patch", target: "opencode", restart: "OpenCode" },
  { id: "cline", name: "Cline", tag: "Werte kopieren -> OpenAI-Compatible-Provider", action: "copy",
    how: "how.cline",
    snippet: () => `Base URL: ${BASE_URL}\nModel: slipstream\nAPI Key: sk-local` },
  { id: "roo", name: "Roo Code", tag: "Werte kopieren -> OpenAI-Compatible-Provider", action: "copy",
    how: "how.roo",
    snippet: () => `Base URL: ${BASE_URL}\nModel: slipstream\nAPI Key: sk-local` },
  { id: "cursor", name: "Cursor", tag: "Werte kopieren -> Override OpenAI Base URL", action: "copy",
    how: "how.cursor",
    snippet: () => `Base URL: ${BASE_URL}\nModel: slipstream\nAPI Key: sk-local` },
  { id: "continue", name: "Continue", tag: "config.yaml-Snippet kopieren", action: "copy",
    how: "how.continue",
    snippet: () => `models:\n  - name: Slipstream Local\n    provider: openai\n    model: slipstream\n    apiBase: ${BASE_URL}\n    apiKey: sk-local` },
  { id: "aider", name: "aider", tag: ".aider.conf.yml-Snippet kopieren", action: "copy",
    how: "how.aider",
    snippet: () => `openai-api-base: ${BASE_URL}\nopenai-api-key: sk-local\nmodel: openai/slipstream` },
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
        cfg: { home: state.def ? state.def.home : "", base_url: BASE_URL, model: "slipstream",
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
    } catch { toast("Kopieren fehlgeschlagen", true); }
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
  else if (st.emb_bytes > 0 || st.qdrant_installed) { b.className = "badge partial"; b.textContent = "teilweise"; }
  else { b.className = "badge missing"; b.textContent = "nicht eingerichtet"; }

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
    $("testMeta").textContent = `${dt.toFixed(1)} s · ${tok} tokens · ${(tok / dt).toFixed(1)} tok/s`;
  } catch (e) {
    $("answer").textContent = t("test.fail");
    $("testMeta").textContent = "";
  }
  btn.disabled = false; btn.textContent = t("btn.send");
};

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
  } catch { txt = "(kein Log)"; }
  const el = $("logs");
  const atBottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 30;
  el.textContent = txt || "(leer)";
  if ($("autoscroll").checked && atBottom) el.scrollTop = el.scrollHeight;
}
$("diagBtn").onclick = async () => {
  try {
    const s = await invoke("system_stats");
    const log = await invoke("read_log", { maxLines: 60 });
    const diag = `Peregrine Control - Diagnose
Modell: ${state.model.name}
Pfad: ${ggufPath()}
Server-Binary: ${$("pServer").value}
Cache: ${$("cache").value} GiB · Ctx: ${$("ctx").value} · IO: ${$("io").value}
RAM frei: ${s.free_gib.toFixed(1)}/${s.total_gib.toFixed(0)} GiB · Swap: ${s.swap_used_mb.toFixed(0)} MB
Laeuft: ${state.running}
--- letzte Server-Logs ---
${log}`;
    await navigator.clipboard.writeText(diag);
    toast(t("toast.diagCopied"));
  } catch (e) { toast(e, true); }
};

// ---- boot ------------------------------------------------------------------
async function boot() {
  try { state.def = await invoke("defaults"); } catch {}
  const saved = localStorage.getItem("pgrn.model");
  if (saved && MODELS.some((m) => m.id === saved)) state.model = MODELS.find((m) => m.id === saved);
  fillModels();
  $("pServer").value = state.def ? state.def.server_bin : "";
  selectModel(state.model.id);
  updateCacheRec();

  // file pickers under the path inputs
  addPicker("pDir", { directory: true }, t("picker.folder"));
  addPicker("pPgrn", { directory: false }, t("picker.pgrn"));
  addPicker("pServer", { directory: false }, t("picker.binary"));
  addPicker("pMirror", { directory: false }, t("picker.pgrn"));
  renderAgents();
  $("lang").onchange = (e) => { applyLang(e.target.value); setPill(state.running ? "loading" : "off"); };
  applyLang(LANG);
  renderSpeed();
  renderInstalled();
  initArena();

  setInterval(poll, 1000);
  setInterval(refreshLogs, 1500);
  setInterval(refreshIndex, 1500);
  setInterval(renderInstalled, 4000);
  poll();
  refreshIndex();
}
boot();
