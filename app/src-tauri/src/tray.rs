//! The macOS menubar item: a status line, two live stat submenus, and the
//! actions that do not need the window open.
//!
//! Every stat row is a disabled menu item whose title is rewritten in place by
//! the poller, which is how a plain `NSMenu` shows live numbers without custom
//! views. The poll runs on the same three-second cadence that already refreshed
//! the tooltip, rather than only while a submenu is open: the server's counters
//! are cumulative and reset with its process, so a gap in polling wide enough to
//! contain a whole server session would lose those tokens from the all-time
//! total for good.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{Manager, Wry};

use crate::servestats::{ServeSnapshot, Store};
use crate::sysstats::{SysSnapshot, Sampler};
use crate::{show_main, stop_server_impl, tray_status_line, AppState, LOG_PATH, SERVER_PORT};

const TRAY_ID: &str = "slipstream-tray";
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The rows the poller rewrites. Held as handles because `set_text` is the only
/// way to make a menu item show a new value.
struct Rows {
    status: MenuItem<Wry>,
    e_cores: MenuItem<Wry>,
    p_cores: MenuItem<Wry>,
    gpu: MenuItem<Wry>,
    gpu_memory: MenuItem<Wry>,
    memory_total: MenuItem<Wry>,
    wired: MenuItem<Wry>,
    active: MenuItem<Wry>,
    compressed: MenuItem<Wry>,
    free: MenuItem<Wry>,
    thermal: MenuItem<Wry>,
    load: MenuItem<Wry>,
    uptime: MenuItem<Wry>,
    experts: MenuItem<Wry>,
    session: Vec<MenuItem<Wry>>,
    alltime: Vec<MenuItem<Wry>>,
}

/// A greyed-out, unclickable row. `None::<&str>` is the accelerator.
fn row(app: &tauri::App, id: &str, text: &str) -> tauri::Result<MenuItem<Wry>> {
    MenuItem::with_id(app, id, text, false, None::<&str>)
}

fn german(value: f64, decimals: usize) -> String {
    format!("{value:.decimals$}").replace('.', ",")
}

fn gib(bytes: u64) -> String {
    format!("{} GB", german(bytes as f64 / GIB, 2))
}

fn percent(fraction: Option<f64>) -> String {
    match fraction {
        Some(v) => format!("{} %", german(v * 100.0, 0)),
        None => "–".into(),
    }
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push('.');
        }
        out.push(ch);
    }
    out
}

fn uptime(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let days = total / 86_400;
    let hours = (total % 86_400) / 3600;
    if days > 0 {
        format!("{days} T {hours} Std")
    } else {
        format!("{hours} Std {} Min", (total % 3600) / 60)
    }
}

/// The expert cache as the engine last reported it — the numbers that explain
/// streamed decode speed, and the ones no stock server reports. Misses are handed
/// out too because the window derives SSD throughput from how fast they grow;
/// deriving it from a second parse of the same file is how two panels come to
/// disagree about one cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct ExpertCache {
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
}

/// What the log says, but only while we still own the server that wrote it. The
/// file is truncated when we start a server and then outlives it, so ungated
/// this would put a two-day-old number next to live ones with nothing on it to
/// say so.
pub fn expert_cache(state: &AppState) -> Option<ExpertCache> {
    crate::alive(&state.server)
        .then(expert_cache_from_log)
        .flatten()
}

fn expert_cache_from_log() -> Option<ExpertCache> {
    parse_expert_cache(&log_tail(LOG_PATH, 256 * 1024)?)
}

/// The last `bytes` of a file, cut back to a line boundary. The window asks for
/// this every second, and an engine log runs to megabytes over an afternoon —
/// reading the whole thing at 1 Hz to find its last line would be silly. The
/// cache line is printed after every request, so a quarter-megabyte of tail
/// always contains one.
pub fn log_tail(path: &str, bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = len.saturating_sub(bytes);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut raw = Vec::with_capacity(bytes as usize);
    file.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    // A seek lands mid-line unless it landed at the start of the file; that
    // fragment could otherwise be parsed as a whole line.
    Some(match (from > 0, text.find('\n')) {
        (true, Some(first)) => text[first + 1..].to_owned(),
        _ => text,
    })
}

/// Last reading wins: the engine prints cumulative counters after every request,
/// so the final line is the current state of the cache, not a sum to be added up.
fn parse_expert_cache(log: &str) -> Option<ExpertCache> {
    let mut latest = None;
    for line in log.lines() {
        let Some(rest) = line.split("hits = ").nth(1) else {
            continue;
        };
        let Some((hits, rest)) = rest.split_once(", misses = ") else {
            continue;
        };
        let misses: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let (Ok(hits), Ok(misses)) = (hits.trim().parse::<u64>(), misses.parse::<u64>()) {
            if hits + misses > 0 {
                latest = Some(ExpertCache {
                    hits,
                    misses,
                    hit_rate: hits as f64 / (hits + misses) as f64 * 100.0,
                });
            }
        }
    }
    latest
}

fn serve_labels(snapshot: &ServeSnapshot) -> [String; 6] {
    [
        format!("Tokens gesamt: {}", thousands(snapshot.total_tokens)),
        format!("Aus dem Cache: {}", thousands(snapshot.cached_tokens)),
        format!("Cache-Effizienz: {} %", german(snapshot.cache_efficiency, 1)),
        format!("Prefill: {} tok/s", german(snapshot.avg_prefill_tps, 1)),
        format!("Decode: {} tok/s", german(snapshot.avg_decode_tps, 1)),
        format!("Anfragen: {}", thousands(snapshot.requests)),
    ]
}

fn apply(
    rows: &Rows,
    status: &str,
    system: &SysSnapshot,
    store: &Store,
    experts: Option<ExpertCache>,
) {
    // Status text is computed *before* any Live mutex is held. Calling
    // tray_status_line() (curl) or set_text while holding `live.serving` deadlocks
    // the app: set_text hops to the main thread, and the UI's live_stats waits on
    // the same mutex — classic AppKit hang (psynch_mutexwait).
    let _ = rows.status.set_text(status);

    let _ = rows.e_cores.set_text(format!("E-Kerne: {}", percent(system.e_core_usage)));
    let _ = rows.p_cores.set_text(format!("P-Kerne: {}", percent(system.p_core_usage)));
    let _ = rows.gpu.set_text(format!("GPU: {}", percent(system.gpu_usage)));
    let _ = rows.gpu_memory.set_text(match system.gpu_memory_bytes {
        Some(bytes) => format!("GPU-Speicher: {}", gib(bytes)),
        None => "GPU-Speicher: –".into(),
    });

    let memory = &system.memory;
    let used = memory.wired_bytes + memory.active_bytes + memory.compressed_bytes;
    let share = if memory.total_bytes > 0 {
        used as f64 / memory.total_bytes as f64
    } else {
        0.0
    };
    let _ = rows.memory_total.set_text(format!(
        "{} / {} ({})",
        german(used as f64 / GIB, 1),
        gib(memory.total_bytes),
        percent(Some(share))
    ));
    let _ = rows.wired.set_text(format!("Wired: {}", gib(memory.wired_bytes)));
    let _ = rows.active.set_text(format!("Aktiv: {}", gib(memory.active_bytes)));
    let _ = rows.compressed.set_text(format!("Komprimiert: {}", gib(memory.compressed_bytes)));
    let _ = rows.free.set_text(format!("Frei: {}", gib(memory.free_bytes)));

    let _ = rows.thermal.set_text(format!("Thermik: {}", system.thermal));
    let _ = rows.load.set_text(if system.load_average.is_empty() {
        "Last: –".into()
    } else {
        format!(
            "Last: {}",
            system
                .load_average
                .iter()
                .map(|v| german(*v, 2))
                .collect::<Vec<_>>()
                .join(" · ")
        )
    });
    let _ = rows.uptime.set_text(format!("Laufzeit: {}", uptime(system.uptime_seconds)));

    let _ = rows.experts.set_text(match experts {
        Some(cache) => format!("Trefferquote: {} %", german(cache.hit_rate, 1)),
        None => "Trefferquote: – (kein Streaming-Lauf)".into(),
    });

    // Without --metrics there is nothing to show rather than zeroes to invent.
    if store.has_reading() {
        for (item, text) in rows.session.iter().zip(serve_labels(&store.session())) {
            let _ = item.set_text(text);
        }
    }
    for (item, text) in rows.alltime.iter().zip(serve_labels(&store.alltime())) {
        let _ = item.set_text(text);
    }
}

pub fn install(app: &tauri::App) -> tauri::Result<()> {
    let status = row(app, "status", "Server: …")?;
    let stop = MenuItem::with_id(app, "stop", "Server stoppen", true, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Slipstream öffnen", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Beenden", true, None::<&str>)?;

    let cpu_head = row(app, "cpu_head", "CPU")?;
    let e_cores = row(app, "e_cores", "E-Kerne: –")?;
    let p_cores = row(app, "p_cores", "P-Kerne: –")?;
    let gpu_head = row(app, "gpu_head", "GPU")?;
    let gpu = row(app, "gpu", "GPU: –")?;
    let gpu_memory = row(app, "gpu_mem", "GPU-Speicher: –")?;
    let mem_head = row(app, "mem_head", "SPEICHER")?;
    let memory_total = row(app, "mem_total", "–")?;
    let wired = row(app, "wired", "Wired: –")?;
    let active = row(app, "active", "Aktiv: –")?;
    let compressed = row(app, "compressed", "Komprimiert: –")?;
    let free = row(app, "free", "Frei: –")?;
    let host_head = row(app, "host_head", "HOST")?;
    let thermal = row(app, "thermal", "Thermik: –")?;
    let load = row(app, "load", "Last: –")?;
    let uptime_row = row(app, "uptime", "Laufzeit: –")?;

    let system_menu = Submenu::with_items(
        app,
        "System Stats",
        true,
        &[
            &cpu_head, &e_cores, &p_cores,
            &PredefinedMenuItem::separator(app)?,
            &gpu_head, &gpu, &gpu_memory,
            &PredefinedMenuItem::separator(app)?,
            &mem_head, &memory_total, &wired, &active, &compressed, &free,
            &PredefinedMenuItem::separator(app)?,
            &host_head, &thermal, &load, &uptime_row,
        ],
    )?;

    let session_head = row(app, "session_head", "SITZUNG")?;
    let session: Vec<MenuItem<Wry>> = serve_labels(&ServeSnapshot::default())
        .iter()
        .enumerate()
        .map(|(index, text)| row(app, &format!("session_{index}"), text))
        .collect::<tauri::Result<_>>()?;
    let expert_head = row(app, "expert_head", "EXPERTEN-CACHE (SSD-Streaming)")?;
    let experts = row(app, "experts", "Trefferquote: –")?;
    let alltime_head = row(app, "alltime_head", "GESAMT")?;
    let alltime: Vec<MenuItem<Wry>> = serve_labels(&ServeSnapshot::default())
        .iter()
        .enumerate()
        .map(|(index, text)| row(app, &format!("alltime_{index}"), text))
        .collect::<tauri::Result<_>>()?;

    let mut serving_items: Vec<&dyn tauri::menu::IsMenuItem<Wry>> = vec![&session_head];
    for item in &session {
        serving_items.push(item);
    }
    let separator_a = PredefinedMenuItem::separator(app)?;
    serving_items.push(&separator_a);
    serving_items.push(&expert_head);
    serving_items.push(&experts);
    let separator_b = PredefinedMenuItem::separator(app)?;
    serving_items.push(&separator_b);
    serving_items.push(&alltime_head);
    for item in &alltime {
        serving_items.push(item);
    }
    let serving_menu = Submenu::with_items(app, "Serving Stats", true, &serving_items)?;

    let menu = Menu::with_items(
        app,
        &[
            &status,
            &stop,
            &PredefinedMenuItem::separator(app)?,
            &system_menu,
            &serving_menu,
            &PredefinedMenuItem::separator(app)?,
            &open,
            &quit,
        ],
    )?;

    let rows = Rows {
        status,
        e_cores,
        p_cores,
        gpu,
        gpu_memory,
        memory_total,
        wired,
        active,
        compressed,
        free,
        thermal,
        load,
        uptime: uptime_row,
        experts,
        session,
        alltime,
    };

    let mut tray = tauri::tray::TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("Slipstream")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "stop" => stop_server_impl(&app.state::<AppState>()),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            use tauri::tray::TrayIconEvent;
            if let TrayIconEvent::Click { button, .. } = event {
                if button == tauri::tray::MouseButton::Left {
                    show_main(tray.app_handle());
                }
            }
        });
    if let Some(icon) = app.default_window_icon().cloned() {
        tray = tray.icon(icon);
    }
    let _tray = tray.build(app)?;

    let handle = app.handle().clone();
    std::thread::spawn(move || {
        let mut sampler = Sampler::default();
        let metrics_url = format!("http://127.0.0.1:{SERVER_PORT}/metrics");
        let omlx_status_url = format!("http://127.0.0.1:{SERVER_PORT}/api/status");
        loop {
            // All I/O and status probes happen with *no* Live mutex held. Holding
            // `serving` across curl / set_text is what froze Slipstream under load.
            let system = sampler.sample();
            // Metal answers /metrics; resident MLX answers /api/status. Try the
            // Prometheus path first so a stock llama-server stays unchanged.
            let body = crate::http_body(&metrics_url)
                .or_else(|| crate::http_body(&omlx_status_url));
            let extras = body
                .as_deref()
                .and_then(crate::servestats::parse_omlx_extras)
                .unwrap_or_default();
            let status = tray_status_line();
            let experts = expert_cache(&handle.state::<AppState>()).or_else(|| {
                extras.has_pgrn().then(|| ExpertCache {
                    hits: extras.pgrn_hits.unwrap_or(0),
                    misses: extras.pgrn_misses.unwrap_or(0),
                    hit_rate: extras.pgrn_hit_rate.unwrap_or(0.0),
                })
            });

            // Snapshot under the lock, then release before touching AppKit menus.
            let store_snapshot = {
                let live = handle.state::<crate::Live>();
                let mut store = live
                    .serving
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(ref body) = body {
                    store.observe(body);
                }
                *live
                    .system
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = system.clone();
                *live
                    .api_extras
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = extras;
                // A no-op unless the total moved, so an idle server writes nothing.
                store.persist();
                store.clone()
            };

            apply(&rows, &status, &system, &store_snapshot, experts);
            if let Some(tray) = handle.tray_by_id(TRAY_ID) {
                let _ = tray.set_tooltip(Some(status.as_str()));
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_formatted_the_way_a_german_menu_reads() {
        assert_eq!(german(4.25, 2), "4,25");
        assert_eq!(gib(1_610_612_736), "1,50 GB");
        assert_eq!(percent(Some(0.294)), "29 %");
        assert_eq!(percent(None), "–");
        assert_eq!(thousands(1_234_567), "1.234.567");
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
    }

    #[test]
    fn uptime_reads_as_days_then_falls_back_to_hours() {
        assert_eq!(uptime(6.0 * 86_400.0 + 18.0 * 3600.0), "6 T 18 Std");
        assert_eq!(uptime(3600.0 * 5.0 + 120.0), "5 Std 2 Min");
        assert_eq!(uptime(0.0), "0 Std 0 Min");
    }

    /// Lines copied from a real 35B run, in the order the engine wrote them: two
    /// requests, and a tiers line in between that carries a zero hit count and
    /// must not be mistaken for the cache's.
    #[test]
    fn the_hit_rate_is_the_last_cache_line_not_the_last_hits_word() {
        let log = "\
8.26.243.389 I slot print_timing: PGRN cache = 8190.71 MiB, hits = 91002, misses = 68827 (56.94%)
8.26.243.391 I slot print_timing: PGRN tiers HOT/WARM slots = 0/3731, hits = 0/159829, promotions = 0
8.39.600.342 I slot print_timing: PGRN cache = 8190.71 MiB, hits = 181403, misses = 130459 (58.17%)
8.39.600.344 I slot print_timing: PGRN tiers HOT/WARM slots = 0/3731, hits = 0/181403, promotions = 0
";
        let cache = parse_expert_cache(log).expect("a reading");
        assert_eq!(format!("{:.2}", cache.hit_rate), "58.17");
        // Both counters travel, since the window derives SSD throughput from how
        // fast misses grow between two of these readings.
        assert_eq!((cache.hits, cache.misses), (181_403, 130_459));
    }

    #[test]
    fn a_log_without_a_streaming_run_yields_no_reading() {
        assert_eq!(parse_expert_cache(""), None);
        assert_eq!(parse_expert_cache("main: server is listening on 127.0.0.1:8080\n"), None);
        // A finished run that never fetched an expert: no denominator, no rate.
        assert_eq!(parse_expert_cache("PGRN cache = 0.00 MiB, hits = 0, misses = 0 (0.00%)\n"), None);
    }

    /// Reading only the tail must not invent a line. A seek into the middle of a
    /// file lands mid-line, and that fragment is dropped rather than parsed —
    /// otherwise a cut like "…hits = 91002, misses = 6" would read as a real
    /// reading with a truncated number.
    #[test]
    fn the_tail_reader_drops_the_line_it_cut_and_keeps_the_rest() {
        let dir = std::env::temp_dir().join(format!("slipstream-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("engine.log");
        let noise = "x".repeat(4096);
        std::fs::write(
            &path,
            format!(
                "{noise}\nPGRN cache = 1.00 MiB, hits = 91002, misses = 68827 (56.94%)\n\
                 PGRN cache = 1.00 MiB, hits = 181403, misses = 130459 (58.17%)\n"
            ),
        )
        .unwrap();
        let name = path.to_str().unwrap();

        // A window smaller than the noise cuts into it: the fragment goes, both
        // readings survive, and the last one wins.
        let tail = log_tail(name, 256).expect("a tail");
        assert!(!tail.starts_with('x'), "the cut line must be dropped");
        assert_eq!(parse_expert_cache(&tail).unwrap().misses, 130_459);

        // A window larger than the file returns all of it, first line included.
        let whole = log_tail(name, 1 << 20).expect("a tail");
        assert!(whole.starts_with('x'), "no cut, so nothing to drop");
        assert_eq!(parse_expert_cache(&whole).unwrap().misses, 130_459);

        // A cut that lands inside the numbers of the last line must not parse it.
        let cut = log_tail(name, 20).expect("a tail");
        assert_eq!(parse_expert_cache(&cut), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The gate, in its closed position: with no server of ours running, the log
    /// on disk belongs to some earlier process and is not reported at all.
    #[test]
    fn no_server_of_ours_means_no_rate_however_old_the_log_is() {
        let state = AppState::default();
        assert_eq!(expert_cache(&state), None);
    }

    #[test]
    fn serving_rows_carry_the_measured_figures() {
        let snapshot = ServeSnapshot {
            total_tokens: 176,
            cached_tokens: 63,
            cache_efficiency: 49.21875,
            avg_prefill_tps: 314.0,
            avg_decode_tps: 159.46,
            requests: 2,
        };
        let labels = serve_labels(&snapshot);
        assert_eq!(labels[0], "Tokens gesamt: 176");
        assert_eq!(labels[2], "Cache-Effizienz: 49,2 %");
        assert_eq!(labels[3], "Prefill: 314,0 tok/s");
        assert_eq!(labels[5], "Anfragen: 2");
    }
}
