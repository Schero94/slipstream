//! What the engine is doing right now, read from llama-server's `/slots`.
//!
//! The Slipstream engine adds `"phase"` (idle | waiting | prompt | generating) to every slot
//! and, while a prompt is read, `"prompt_progress"` {total, cache, processed, time_ms}. During
//! a layer-major prefill `/slots` is answered while the decode runs, so the window and the
//! menubar can show "Reading your prompt 12,000 of 39,500" for requests that come from an
//! editor (Kilo, OpenCode), not only for the app's own chat. A stock llama-server has no
//! `"phase"`; then this yields nothing and the window falls back to the log lines.

use serde::Serialize;
use serde_json::Value;

/// One reading of the busiest slot.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SlotActivity {
    /// "idle" | "waiting" | "prompt" | "generating"
    pub phase: String,
    pub slot: i64,
    /// Engine task id, -1 when the slot has none (matches the log's `task N`).
    pub task: i64,
    /// Prompt tokens of the request (prompt phase only).
    pub total: Option<i64>,
    /// Tokens reused from the cache (prompt phase only).
    pub cache: Option<i64>,
    /// Tokens done so far, cached ones included (prompt phase only).
    pub processed: Option<i64>,
    /// Time since the prompt started, in ms (prompt phase only).
    pub time_ms: Option<i64>,
    /// Answer tokens written so far (generating phase, when the server reports it).
    pub n_decoded: Option<i64>,
}

fn rank(phase: &str) -> u8 {
    match phase {
        "prompt" => 3,
        "generating" => 2,
        "waiting" => 1,
        _ => 0,
    }
}

/// Parse a `/slots` body. None when it is not a slot list or no slot carries `"phase"`
/// (stock server): the caller then has no reading rather than a made-up "idle".
pub fn parse_slots(body: &str) -> Option<SlotActivity> {
    let v: Value = serde_json::from_str(body).ok()?;
    let slots = v.as_array()?;
    let mut best: Option<SlotActivity> = None;
    for s in slots {
        let Some(phase) = s.get("phase").and_then(Value::as_str) else { continue };
        let pp = s.get("prompt_progress");
        let field = |k: &str| pp.and_then(|p| p.get(k)).and_then(Value::as_i64);
        // Upstream has used both an object and a one-element array for next_token.
        let next = s.get("next_token").map(|n| n.get(0).unwrap_or(n));
        let a = SlotActivity {
            phase: phase.to_string(),
            slot: s.get("id").and_then(Value::as_i64).unwrap_or(0),
            task: s.get("id_task").and_then(Value::as_i64).unwrap_or(-1),
            total: field("total"),
            cache: field("cache"),
            processed: field("processed"),
            time_ms: field("time_ms"),
            n_decoded: if phase == "generating" {
                next.and_then(|n| n.get("n_decoded")).and_then(Value::as_i64)
            } else {
                None
            },
        };
        if best.as_ref().map_or(true, |b| rank(&a.phase) > rank(&b.phase)) {
            best = Some(a);
        }
    }
    best
}

/// Menubar status for an active slot (the menubar is German like the rest of its rows).
/// None when idle, so the caller keeps its usual "bereit / tok/s" line.
pub fn status_text(a: &SlotActivity) -> Option<String> {
    match a.phase.as_str() {
        "prompt" => match (a.processed, a.total) {
            (Some(p), Some(t)) if t > 0 => {
                let pct = ((p as f64 / t as f64) * 100.0).clamp(0.0, 100.0).floor() as i64;
                Some(format!("Slipstream · liest Prompt {pct} %"))
            }
            _ => Some("Slipstream · liest Prompt…".into()),
        },
        "generating" => Some("Slipstream · schreibt Antwort…".into()),
        "waiting" => Some("Slipstream · wartet…".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_phase_carries_the_progress_numbers() {
        let body = r#"[{"id":0,"id_task":7,"is_processing":true,"phase":"prompt",
            "prompt_progress":{"total":39517,"cache":0,"processed":12000,"time_ms":41000}}]"#;
        let a = parse_slots(body).unwrap();
        assert_eq!(a.phase, "prompt");
        assert_eq!(a.task, 7);
        assert_eq!(a.total, Some(39517));
        assert_eq!(a.processed, Some(12000));
        assert_eq!(a.time_ms, Some(41000));
        assert_eq!(status_text(&a).unwrap(), "Slipstream · liest Prompt 30 %");
    }

    #[test]
    fn the_busiest_slot_wins_and_generating_reads_n_decoded() {
        let body = r#"[{"id":0,"id_task":-1,"phase":"idle"},
            {"id":1,"id_task":9,"phase":"generating","next_token":[{"n_decoded":42}]}]"#;
        let a = parse_slots(body).unwrap();
        assert_eq!((a.slot, a.task, a.phase.as_str()), (1, 9, "generating"));
        assert_eq!(a.n_decoded, Some(42));
        assert_eq!(a.total, None);
        let obj = r#"[{"id":0,"id_task":3,"phase":"generating","next_token":{"n_decoded":5}}]"#;
        assert_eq!(parse_slots(obj).unwrap().n_decoded, Some(5));
    }

    #[test]
    fn a_stock_server_without_phase_yields_no_reading() {
        assert_eq!(parse_slots(r#"[{"id":0,"is_processing":true}]"#), None);
        assert_eq!(parse_slots("not json"), None);
        assert_eq!(parse_slots(r#"{"error":"x"}"#), None);
    }

    #[test]
    fn idle_has_no_menubar_text() {
        let a = parse_slots(r#"[{"id":0,"phase":"idle"}]"#).unwrap();
        assert_eq!(status_text(&a), None);
    }
}
