//! Serving counters behind the menubar's Serving Stats submenu.
//!
//! `llama-server` exposes cumulative counters on `/metrics`, but they start at
//! zero with every server process. That makes the raw reading the *session*
//! number and leaves all-time totals to us: each poll adds the delta since the
//! last one to a persisted running total, and a counter that went backwards means
//! the server restarted, so the whole current reading is the delta.
//!
//! The derived figures follow the same definitions the reference menubar uses, so
//! a number here means what a number there means: prefill speed counts only the
//! tokens actually processed (a cache hit costs no prefill time and must not
//! flatter the rate), while cache efficiency and total tokens count the full
//! submitted prompt.

use std::path::PathBuf;

/// Raw cumulative counters as `/metrics` reports them.
#[derive(Default, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Counters {
    /// Prompt tokens the server actually had to process.
    pub prompt_processed: u64,
    /// Prompt tokens served from the KV cache instead.
    pub prompt_cached: u64,
    pub predicted: u64,
    pub prompt_seconds: f64,
    pub predicted_seconds: f64,
    pub requests: u64,
}

impl Counters {
    /// True if any counter is below `earlier`, which only happens when the
    /// server process was replaced.
    fn went_backwards_from(&self, earlier: &Counters) -> bool {
        self.prompt_processed < earlier.prompt_processed
            || self.prompt_cached < earlier.prompt_cached
            || self.predicted < earlier.predicted
            || self.requests < earlier.requests
            || self.prompt_seconds < earlier.prompt_seconds
            || self.predicted_seconds < earlier.predicted_seconds
    }

    /// This reading minus an earlier one, floored at zero.
    fn minus(&self, earlier: &Counters) -> Counters {
        Counters {
            prompt_processed: self.prompt_processed.saturating_sub(earlier.prompt_processed),
            prompt_cached: self.prompt_cached.saturating_sub(earlier.prompt_cached),
            predicted: self.predicted.saturating_sub(earlier.predicted),
            prompt_seconds: (self.prompt_seconds - earlier.prompt_seconds).max(0.0),
            predicted_seconds: (self.predicted_seconds - earlier.predicted_seconds).max(0.0),
            requests: self.requests.saturating_sub(earlier.requests),
        }
    }

    fn add_delta(&mut self, current: &Counters, previous: &Counters) {
        self.prompt_processed += current.prompt_processed.saturating_sub(previous.prompt_processed);
        self.prompt_cached += current.prompt_cached.saturating_sub(previous.prompt_cached);
        self.predicted += current.predicted.saturating_sub(previous.predicted);
        self.requests += current.requests.saturating_sub(previous.requests);
        self.prompt_seconds += (current.prompt_seconds - previous.prompt_seconds).max(0.0);
        self.predicted_seconds += (current.predicted_seconds - previous.predicted_seconds).max(0.0);
    }
}

/// What the submenu shows, for one scope.
#[derive(Default, Clone, serde::Serialize)]
pub struct ServeSnapshot {
    /// Full submitted prompt plus generated tokens.
    pub total_tokens: u64,
    pub cached_tokens: u64,
    /// Share of the submitted prompt that came from the cache, in percent.
    pub cache_efficiency: f64,
    /// Prefill rate over the tokens that were actually processed.
    pub avg_prefill_tps: f64,
    pub avg_decode_tps: f64,
    pub requests: u64,
}

impl ServeSnapshot {
    pub fn from(counters: &Counters) -> Self {
        let submitted = counters.prompt_processed + counters.prompt_cached;
        Self {
            total_tokens: submitted + counters.predicted,
            cached_tokens: counters.prompt_cached,
            cache_efficiency: if submitted > 0 {
                counters.prompt_cached as f64 / submitted as f64 * 100.0
            } else {
                0.0
            },
            avg_prefill_tps: if counters.prompt_seconds > 0.0 {
                counters.prompt_processed as f64 / counters.prompt_seconds
            } else {
                0.0
            },
            avg_decode_tps: if counters.predicted_seconds > 0.0 {
                counters.predicted as f64 / counters.predicted_seconds
            } else {
                0.0
            },
            requests: counters.requests,
        }
    }
}

/// Parses the Prometheus text exposition `/metrics` returns. Unknown lines and
/// the `# HELP`/`# TYPE` comments are ignored, so upstream adding counters
/// cannot break this.
pub fn parse_metrics(body: &str) -> Option<Counters> {
    let mut counters = Counters::default();
    let mut saw_any = false;
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = name.strip_prefix("llamacpp:").unwrap_or(name);
        let Ok(value) = value.trim().parse::<f64>() else {
            continue;
        };
        match name {
            "prompt_tokens_total" => counters.prompt_processed = value as u64,
            "prompt_tokens_cached_total" => counters.prompt_cached = value as u64,
            "tokens_predicted_total" => counters.predicted = value as u64,
            "prompt_seconds_total" => counters.prompt_seconds = value,
            "tokens_predicted_seconds_total" => counters.predicted_seconds = value,
            "requests_total" => counters.requests = value as u64,
            _ => continue,
        }
        saw_any = true;
    }
    saw_any.then_some(counters)
}

/// Optional MLX/PGRN fields from `GET /api/status` that sit beside the serving
/// counters. Absent on Metal `/metrics` and on older oMLX builds.
#[derive(Default, Clone, PartialEq, serde::Serialize)]
pub struct ApiExtras {
    /// Lifetime average generation tok/s from the oMLX metrics snapshot.
    pub avg_generation_tps: Option<f64>,
    /// Engine-reported model memory (Metal heap / resident estimate).
    pub model_memory_bytes: Option<u64>,
    /// Best-effort process RSS when the server exposes it.
    pub process_rss_bytes: Option<u64>,
    pub pgrn_hits: Option<u64>,
    pub pgrn_misses: Option<u64>,
    pub pgrn_hit_rate: Option<f64>,
    pub pgrn_high_water_bytes: Option<u64>,
    pub pgrn_mx_size: Option<u64>,
}

impl ApiExtras {
    pub fn has_pgrn(&self) -> bool {
        self.pgrn_hits.is_some() || self.pgrn_misses.is_some()
    }
}

/// Parses the lightweight extras from oMLX `GET /api/status` (PGRN + RSS).
/// Returns `None` when the body is not JSON status (e.g. Prometheus `/metrics`).
pub fn parse_omlx_extras(body: &str) -> Option<ApiExtras> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    // Refuse Prometheus text / unrelated JSON.
    if value.get("status").and_then(|v| v.as_str()) != Some("ok")
        && value.get("avg_generation_tps").is_none()
        && value.get("pgrn").is_none()
    {
        return None;
    }
    let mut extras = ApiExtras {
        avg_generation_tps: value.get("avg_generation_tps").and_then(|v| v.as_f64()),
        model_memory_bytes: value
            .get("model_memory_used")
            .and_then(|v| v.as_u64())
            .filter(|&n| n > 0),
        process_rss_bytes: value
            .get("process_rss_bytes")
            .and_then(|v| v.as_u64())
            .filter(|&n| n > 0),
        ..Default::default()
    };
    if let Some(pgrn) = value.get("pgrn").filter(|v| !v.is_null()) {
        extras.pgrn_hits = pgrn.get("hits").and_then(|v| v.as_u64());
        extras.pgrn_misses = pgrn.get("misses").and_then(|v| v.as_u64());
        extras.pgrn_hit_rate = pgrn.get("hit_rate").and_then(|v| v.as_f64());
        extras.pgrn_high_water_bytes = pgrn
            .get("high_water_bytes")
            .and_then(|v| v.as_u64())
            .filter(|&n| n > 0);
        extras.pgrn_mx_size = pgrn.get("mx_size").and_then(|v| v.as_u64());
        // Derive hit rate when the server omitted it but counters are present.
        // Cold start (0 hits + 0 misses) → Some(0.0) so the strip can show an
        // honest expert 0% instead of falling through to KV or inventing null.
        if extras.pgrn_hit_rate.is_none() {
            let hits = extras.pgrn_hits.unwrap_or(0);
            let misses = extras.pgrn_misses.unwrap_or(0);
            let total = hits + misses;
            if total > 0 {
                extras.pgrn_hit_rate = Some(hits as f64 / total as f64 * 100.0);
            } else if extras.pgrn_hits.is_some() || extras.pgrn_misses.is_some() {
                extras.pgrn_hit_rate = Some(0.0);
            }
        }
    }
    Some(extras)
}

/// Parses oMLX `GET /api/status`. Lifetime averages are turned back into total
/// seconds so the same `ServeSnapshot` math that llama's counters use still
/// applies: rates = tokens / seconds.
pub fn parse_omlx_status(body: &str) -> Option<Counters> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    // Refuse Prometheus text accidentally fed here.
    if value.get("total_prompt_tokens").is_none() && value.get("total_completion_tokens").is_none()
    {
        return None;
    }
    let prompt_total = value.get("total_prompt_tokens")?.as_u64()?;
    let prompt_cached = value
        .get("total_cached_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let predicted = value
        .get("total_completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let requests = value
        .get("total_requests")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let prompt_processed = prompt_total.saturating_sub(prompt_cached);
    let avg_prefill = value
        .get("avg_prefill_tps")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let avg_decode = value
        .get("avg_generation_tps")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    Some(Counters {
        prompt_processed,
        prompt_cached,
        predicted,
        prompt_seconds: if avg_prefill > 0.0 && prompt_processed > 0 {
            prompt_processed as f64 / avg_prefill
        } else {
            0.0
        },
        predicted_seconds: if avg_decode > 0.0 && predicted > 0 {
            predicted as f64 / avg_decode
        } else {
            0.0
        },
        requests,
    })
}

/// Last decode tok/s from an engine log tail.
///
/// Metal/llama.cpp: `eval time = … tokens per second` (prefill lines ignored).
/// oMLX: `output=N tok/s` or `Completion: … (N tok/s)`.
pub fn parse_last_tps(log: &str) -> Option<f64> {
    let mut best = None;
    for line in log.lines() {
        if line.contains("eval time") && line.contains("tokens per second") {
            if let Some(v) = number_before(line, "tokens per second") {
                if sane_tps(v) {
                    best = Some(v);
                }
            }
            continue;
        }
        if let Some(rest) = line.split("output=").nth(1) {
            let token = rest.split(|c: char| c.is_whitespace() || c == 't').next();
            if let Some(Ok(v)) = token.map(str::parse::<f64>) {
                if sane_tps(v) {
                    best = Some(v);
                }
            }
            continue;
        }
        if line.contains("Completion:") && line.contains("tok/s") {
            if let Some(v) = number_before(line, "tok/s") {
                if sane_tps(v) {
                    best = Some(v);
                }
            }
        }
    }
    best
}

fn sane_tps(v: f64) -> bool {
    v.is_finite() && v > 0.0 && v < 10_000.0
}

/// Number immediately before `marker` in `line` (skipping spaces and `(`).
fn number_before(line: &str, marker: &str) -> Option<f64> {
    let idx = line.find(marker)?;
    let head = line[..idx].trim_end();
    let num = head.rsplit([' ', '(']).next()?;
    num.trim().trim_end_matches(')').parse().ok()
}

/// Session counters plus a persisted all-time total.
#[derive(Default, Clone)]
pub struct Store {
    session: Counters,
    alltime: Counters,
    /// The reading the all-time total already contains. Persisted, because the
    /// app may be restarted while the same server keeps running — without it,
    /// that server's counters would be added a second time on every app start.
    counted_through: Option<Counters>,
    /// Subtracted from the session reading. The server's own totals cannot be
    /// reset from outside, so an offset is the only honest way to offer a
    /// "clear" that leaves the running server alone.
    session_offset: Counters,
    have_reading: bool,
    dirty: bool,
}

/// On-disk shape. Both fields matter: the total alone cannot say whether the
/// server it was measured from is still the one answering.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Persisted {
    alltime: Counters,
    counted_through: Option<Counters>,
}

/// Reads the file's contents, accepting the earlier format that stored the
/// all-time total on its own.
fn parse_persisted(text: &str) -> Persisted {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Persisted::default();
    };
    if value.get("alltime").is_some() {
        return serde_json::from_value(value).unwrap_or_default();
    }
    // Pre-0.2.6 file: a bare Counters object, with no record of what it counted.
    Persisted {
        alltime: serde_json::from_value(value).unwrap_or_default(),
        counted_through: None,
    }
}

impl Store {
    pub fn load() -> Self {
        let text = std::fs::read_to_string(alltime_path()).unwrap_or_default();
        let saved = parse_persisted(&text);
        Self {
            alltime: saved.alltime,
            counted_through: saved.counted_through,
            ..Default::default()
        }
    }

    /// Folds one `/metrics` reading in. Returns false when the body carried no
    /// counters, which is what a server without `--metrics` looks like.
    pub fn observe(&mut self, body: &str) -> bool {
        // Metal exposes Prometheus `/metrics`; resident MLX exposes JSON
        // `/api/status`. Same Store, same session/all-time math.
        let Some(current) = parse_metrics(body).or_else(|| parse_omlx_status(body)) else {
            return false;
        };
        let previously_counted = self.counted_through;
        let baseline = match previously_counted {
            // Same server still counting up: only the delta is new.
            Some(counted) if !current.went_backwards_from(&counted) => counted,
            // A fresh server process, or nothing counted yet: all of it is new.
            _ => {
                if previously_counted.is_some() {
                    // The counters the offset was taken from no longer exist.
                    self.session_offset = Counters::default();
                }
                Counters::default()
            }
        };
        self.alltime.add_delta(&current, &baseline);
        self.session = current;
        self.counted_through = Some(current);
        self.have_reading = true;
        // Only a reading that actually moved is worth a write, which keeps an
        // idle server from touching the disk at all.
        self.dirty |= previously_counted != Some(current);
        true
    }

    /// True while a changed all-time total has not been written yet. Exists so
    /// the write policy can be asserted instead of inferred from disk timing.
    #[cfg(test)]
    pub fn pending_write(&self) -> bool {
        self.dirty
    }

    pub fn session(&self) -> ServeSnapshot {
        ServeSnapshot::from(&self.session.minus(&self.session_offset))
    }

    pub fn alltime(&self) -> ServeSnapshot {
        ServeSnapshot::from(&self.alltime)
    }

    pub fn has_reading(&self) -> bool {
        self.have_reading
    }

    /// Zeroes the session figures without touching the server or the all-time
    /// total: everything counted so far becomes the new starting point.
    pub fn clear_session(&mut self) {
        self.session_offset = self.session;
    }

    /// Drops the all-time total. Marks it for writing rather than writing here,
    /// so the store stays free of disk access and the caller keeps one place
    /// where persistence happens.
    pub fn clear_alltime(&mut self) {
        self.alltime = Counters::default();
        self.dirty = true;
    }

    /// Writes the all-time total if it changed. Meant to be called after every
    /// fold: the file is barely a hundred bytes, and waiting instead would mean
    /// losing everything counted since the last write whenever the app quits.
    pub fn persist(&mut self) {
        if !self.dirty {
            return;
        }
        let path = alltime_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let saved = Persisted { alltime: self.alltime, counted_through: self.counted_through };
        if let Ok(text) = serde_json::to_string(&saved) {
            let _ = std::fs::write(path, text);
        }
        self.dirty = false;
    }
}

/// All-time totals belong in Application Support, not `/tmp` where the rest of
/// this app's scratch state lives — they are supposed to outlive a reboot.
fn alltime_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join("Library/Application Support/Slipstream")
        .join("serving-stats.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim shape of a real `/metrics` response, trimmed to the lines we use.
    const BODY: &str = "\
# HELP llamacpp:prompt_tokens_total Number of prompt tokens processed.
# TYPE llamacpp:prompt_tokens_total counter
llamacpp:prompt_tokens_total 65
llamacpp:prompt_seconds_total 0.207
llamacpp:prompt_tokens_cached_total 63
llamacpp:requests_total 2
llamacpp:tokens_predicted_total 48
llamacpp:tokens_predicted_seconds_total 0.301
llamacpp:n_decode_total 49
";

    #[test]
    fn parses_the_counters_a_real_server_returns() {
        let c = parse_metrics(BODY).expect("counters present");
        assert_eq!(c.prompt_processed, 65);
        assert_eq!(c.prompt_cached, 63);
        assert_eq!(c.predicted, 48);
        assert_eq!(c.requests, 2);
        assert!((c.prompt_seconds - 0.207).abs() < 1e-9);
        assert!((c.predicted_seconds - 0.301).abs() < 1e-9);
    }

    #[test]
    fn omlx_status_json_becomes_the_same_counters() {
        let body = r#"{
          "total_prompt_tokens": 100,
          "total_cached_tokens": 40,
          "total_completion_tokens": 50,
          "total_requests": 3,
          "avg_prefill_tps": 200.0,
          "avg_generation_tps": 50.0
        }"#;
        let c = parse_omlx_status(body).expect("omlx status");
        assert_eq!(c.prompt_processed, 60);
        assert_eq!(c.prompt_cached, 40);
        assert_eq!(c.predicted, 50);
        assert_eq!(c.requests, 3);
        assert!((c.prompt_seconds - 0.3).abs() < 1e-9);
        assert!((c.predicted_seconds - 1.0).abs() < 1e-9);
        let snap = ServeSnapshot::from(&c);
        assert!((snap.avg_prefill_tps - 200.0).abs() < 1e-6);
        assert!((snap.avg_decode_tps - 50.0).abs() < 1e-6);
    }

    #[test]
    fn a_body_without_counters_is_not_a_reading() {
        // A server started without --metrics answers 501 with a plain message.
        assert!(parse_metrics("").is_none());
        assert!(parse_metrics("# HELP only\n# TYPE only\n").is_none());
        assert!(parse_metrics("Not Implemented").is_none());
    }

    #[test]
    fn the_derived_figures_match_the_measured_run() {
        // 128 prompt tokens submitted across two requests, 63 of them cached.
        let snapshot = ServeSnapshot::from(&parse_metrics(BODY).unwrap());
        assert_eq!(snapshot.total_tokens, 65 + 63 + 48);
        assert_eq!(snapshot.cached_tokens, 63);
        assert!((snapshot.cache_efficiency - 63.0 / 128.0 * 100.0).abs() < 1e-9);
        // Prefill counts processed tokens only: 65 / 0.207 s.
        assert!((snapshot.avg_prefill_tps - 65.0 / 0.207).abs() < 1e-6);
        assert!((snapshot.avg_decode_tps - 48.0 / 0.301).abs() < 1e-6);
        assert_eq!(snapshot.requests, 2);
    }

    #[test]
    fn an_empty_server_reports_zeroes_rather_than_dividing_by_zero() {
        let snapshot = ServeSnapshot::from(&Counters::default());
        assert_eq!(snapshot.total_tokens, 0);
        assert_eq!(snapshot.cache_efficiency, 0.0);
        assert_eq!(snapshot.avg_prefill_tps, 0.0);
        assert_eq!(snapshot.avg_decode_tps, 0.0);
    }

    fn body(processed: u64, cached: u64, predicted: u64, requests: u64) -> String {
        format!(
            "llamacpp:prompt_tokens_total {processed}\n\
             llamacpp:prompt_tokens_cached_total {cached}\n\
             llamacpp:tokens_predicted_total {predicted}\n\
             llamacpp:requests_total {requests}\n\
             llamacpp:prompt_seconds_total 1\n\
             llamacpp:tokens_predicted_seconds_total 1\n"
        )
    }

    #[test]
    fn all_time_accumulates_across_a_server_restart() {
        let mut store = Store::default();
        assert!(store.observe(&body(100, 10, 50, 4)));
        assert!(store.observe(&body(150, 20, 80, 6)));
        assert_eq!(store.alltime().requests, 6, "growing counters must not double count");

        // The server restarts: counters drop back towards zero.
        assert!(store.observe(&body(20, 5, 10, 1)));
        assert_eq!(store.session().requests, 1, "session follows the live server");
        assert_eq!(store.alltime().requests, 7, "all-time keeps the earlier six");
        assert_eq!(store.alltime().cached_tokens, 25);

        assert!(store.observe(&body(30, 8, 15, 2)));
        assert_eq!(store.alltime().requests, 8);
        assert_eq!(store.alltime().cached_tokens, 28);
    }

    #[test]
    fn the_first_reading_of_an_app_run_counts_in_full() {
        // A server that was already running before the app started still has to
        // contribute its totals once, not be treated as a zero baseline.
        let mut store = Store::default();
        store.observe(&body(500, 100, 200, 20));
        assert_eq!(store.alltime().requests, 20);
        assert_eq!(store.session().requests, 20);
    }

    #[test]
    fn an_unchanged_reading_needs_no_write() {
        let mut store = Store::default();
        store.observe(&body(100, 10, 50, 4));
        assert!(store.pending_write(), "the first fold must be written");
        store.dirty = false;
        // An idle server answers with the same counters every three seconds.
        store.observe(&body(100, 10, 50, 4));
        assert!(!store.pending_write(), "an idle server must not touch the disk");
        store.observe(&body(101, 10, 50, 4));
        assert!(store.pending_write(), "a moved total must be written");
    }

    /// A store as it comes back from disk, which is where recounting went wrong.
    fn reloaded(store: &Store) -> Store {
        let saved = Persisted { alltime: store.alltime, counted_through: store.counted_through };
        let text = serde_json::to_string(&saved).unwrap();
        let read_back = parse_persisted(&text);
        Store {
            alltime: read_back.alltime,
            counted_through: read_back.counted_through,
            ..Default::default()
        }
    }

    #[test]
    fn restarting_the_app_does_not_recount_a_server_that_kept_running() {
        let mut store = Store::default();
        store.observe(&body(100, 10, 50, 4));
        assert_eq!(store.alltime().requests, 4);

        // Quit, reopen — the server never stopped, so its counters are already in.
        let mut store = reloaded(&store);
        store.observe(&body(100, 10, 50, 4));
        assert_eq!(store.alltime().requests, 4, "the same work must not count twice");
        store.observe(&body(130, 15, 70, 6));
        assert_eq!(store.alltime().requests, 6, "new work after the restart still counts");
    }

    #[test]
    fn restarting_the_app_after_a_server_restart_counts_the_new_server_in_full() {
        let mut store = Store::default();
        store.observe(&body(100, 10, 50, 4));
        let mut store = reloaded(&store);
        // Lower counters can only mean a different server process.
        store.observe(&body(20, 5, 10, 1));
        assert_eq!(store.alltime().requests, 5);
        assert_eq!(store.session().requests, 1);
    }

    #[test]
    fn a_file_from_before_the_fix_keeps_its_total() {
        // The old format was a bare Counters object with no record of its source.
        let old = r#"{"prompt_processed":31,"prompt_cached":39,"predicted":127,
            "prompt_seconds":0.5,"predicted_seconds":2.5,"requests":4}"#;
        let saved = parse_persisted(old);
        assert_eq!(saved.alltime.requests, 4);
        assert!(saved.counted_through.is_none());

        let saved = parse_persisted("not json at all");
        assert_eq!(saved.alltime.requests, 0);
    }

    #[test]
    fn clearing_the_session_leaves_the_server_and_the_total_alone() {
        let mut store = Store::default();
        store.observe(&body(100, 10, 50, 4));
        store.clear_session();
        assert_eq!(store.session().requests, 0, "the session starts over");
        assert_eq!(store.alltime().requests, 4, "the total is untouched");

        // Work after the clear counts from the new starting point.
        store.observe(&body(140, 30, 90, 7));
        assert_eq!(store.session().requests, 3);
        assert_eq!(store.session().cached_tokens, 20);
        assert_eq!(store.alltime().requests, 7);
    }

    #[test]
    fn a_restart_forgets_the_session_offset() {
        // Otherwise the offset from the old process would be subtracted from the
        // new one's small counters and the session would read zero for a while.
        let mut store = Store::default();
        store.observe(&body(100, 10, 50, 4));
        store.clear_session();
        store.observe(&body(20, 5, 10, 1));
        assert_eq!(store.session().requests, 1, "the fresh server is shown in full");
        assert_eq!(store.alltime().requests, 5);
    }

    #[test]
    fn clearing_the_total_marks_it_for_writing() {
        let mut store = Store::default();
        store.observe(&body(100, 10, 50, 4));
        store.dirty = false;
        store.clear_alltime();
        assert_eq!(store.alltime().requests, 0);
        assert_eq!(store.session().requests, 4, "the live server keeps counting");
        assert!(store.pending_write(), "a cleared total must reach the disk");
    }

    #[test]
    fn an_unreadable_body_leaves_the_totals_alone() {
        let mut store = Store::default();
        store.observe(&body(100, 10, 50, 4));
        assert!(!store.observe("Not Implemented"));
        assert_eq!(store.alltime().requests, 4);
        assert_eq!(store.session().requests, 4);
    }

    #[test]
    fn omlx_status_extras_carry_pgrn_and_rss() {
        let body = r#"{
          "status": "ok",
          "avg_generation_tps": 14.2,
          "model_memory_used": 17179869184,
          "process_rss_bytes": 18253611008,
          "pgrn": {
            "hits": 900,
            "misses": 100,
            "hit_rate": 90.0,
            "high_water_bytes": 4294967296,
            "mx_size": 2048
          }
        }"#;
        let x = parse_omlx_extras(body).expect("extras");
        assert!((x.avg_generation_tps.unwrap() - 14.2).abs() < 1e-9);
        assert_eq!(x.model_memory_bytes, Some(17_179_869_184));
        assert_eq!(x.process_rss_bytes, Some(18_253_611_008));
        assert_eq!(x.pgrn_hits, Some(900));
        assert_eq!(x.pgrn_misses, Some(100));
        assert!((x.pgrn_hit_rate.unwrap() - 90.0).abs() < 1e-9);
        assert_eq!(x.pgrn_high_water_bytes, Some(4_294_967_296));
        assert_eq!(x.pgrn_mx_size, Some(2048));
        assert!(x.has_pgrn());
    }

    #[test]
    fn omlx_extras_derive_hit_rate_when_omitted() {
        let body = r#"{"status":"ok","pgrn":{"hits":3,"misses":1}}"#;
        let x = parse_omlx_extras(body).expect("extras");
        assert!((x.pgrn_hit_rate.unwrap() - 75.0).abs() < 1e-9);
    }

    #[test]
    fn prometheus_metrics_are_not_omlx_extras() {
        assert!(parse_omlx_extras(BODY).is_none());
        assert!(parse_omlx_extras("Not Implemented").is_none());
    }

    #[test]
    fn last_tps_reads_decode_eval_not_prefill() {
        let log = "\
prompt eval time = 200.00 ms / 64 tokens ( 3.12 ms per token, 320.00 tokens per second)
eval time =   301.00 ms /  48 tokens ( 6.27 ms per token, 13.62 tokens per second)
";
        assert!((parse_last_tps(log).unwrap() - 13.62).abs() < 1e-9);
        assert!(parse_last_tps("server listening\n").is_none());
    }

    #[test]
    fn last_tps_reads_omlx_output_and_completion() {
        let omlx = "\
INFO Completion: 64 tokens in 3.40s (18.9 tok/s), prompt: 40
INFO request done output=17.5 tok/s e2e
";
        assert!((parse_last_tps(omlx).unwrap() - 17.5).abs() < 1e-9);
        assert!(parse_last_tps("prompt only 500.0 tok/s prefill").is_none());
    }

    #[test]
    fn omlx_extras_empty_pgrn_and_missing_rss_are_honest() {
        let bare = r#"{"status":"ok","avg_generation_tps":0.0}"#;
        let x = parse_omlx_extras(bare).expect("extras");
        assert_eq!(x.process_rss_bytes, None);
        assert_eq!(x.model_memory_bytes, None);
        assert!(!x.has_pgrn());
        let empty_pgrn = r#"{"status":"ok","pgrn":{"hits":0,"misses":0}}"#;
        let y = parse_omlx_extras(empty_pgrn).expect("extras");
        assert!(y.has_pgrn());
        assert!((y.pgrn_hit_rate.unwrap_or(-1.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn last_tps_ignores_zero_and_garbage() {
        // sane_tps rejects ≤0 — zero decode must not surface as a strip value.
        assert!(parse_last_tps(
            "eval time = 0.00 ms / 0 tokens ( 0.00 ms per token, 0.00 tokens per second)"
        )
        .is_none());
        assert!(parse_last_tps("INFO request done output=0.0 tok/s e2e").is_none());
        assert!(parse_last_tps("").is_none());
        assert!(parse_last_tps("tokens per second\n").is_none());
        // Prefer latest sane decode when earlier lines are junk.
        let mixed = "\
eval time = 0.00 ms / 0 tokens ( 0.00 ms per token, 0.00 tokens per second)
eval time = 200.00 ms / 40 tokens ( 5.00 ms per token, 14.20 tokens per second)
";
        assert!((parse_last_tps(mixed).unwrap() - 14.20).abs() < 1e-9);
    }
}
