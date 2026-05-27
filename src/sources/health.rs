//! Per-source liveness state. Updated on every search fan-out and PDF
//! fetch. **In-memory only** — container restarts (which happen on every
//! deploy) reset it. The durable record is `audit_log`; see
//! `/admin/sources` for the audit-log-backed activity view that complements
//! this in-process snapshot.
//!
//! The map itself is built once at startup with a fixed set of source ids
//! and never mutated after — only the per-entry `RwLock`s are written.
//! That keeps the hot path lock-free at the map level while still allowing
//! concurrent updates per source.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::Serialize;
use time::OffsetDateTime;

/// Heuristic: source is "degraded" once this many fan-out failures pile up
/// without an intervening success. Picked to be high enough to ride out
/// one or two transient hiccups but low enough that a real outage flips
/// the public banner within a single user's search session (each search
/// produces up to 4 variants per source).
const DEGRADED_THRESHOLD: u32 = 4;

#[derive(Debug, Clone, Default, Serialize)]
pub struct SourceHealth {
    pub last_ok: Option<OffsetDateTime>,
    pub last_error_at: Option<OffsetDateTime>,
    pub last_error_msg: Option<String>,
    pub consecutive_fails: u32,
    pub consecutive_oks: u32,
    pub total_ok: u64,
    pub total_fail: u64,
}

impl SourceHealth {
    pub fn record_ok(&mut self) {
        self.last_ok = Some(OffsetDateTime::now_utc());
        self.consecutive_fails = 0;
        self.consecutive_oks = self.consecutive_oks.saturating_add(1);
        self.total_ok = self.total_ok.saturating_add(1);
    }

    pub fn record_err(&mut self, err: &str) {
        self.last_error_at = Some(OffsetDateTime::now_utc());
        // Truncate to a sane size so a giant Cloudflare-challenge HTML
        // body smuggled into the error string doesn't bloat the map.
        let mut msg: String = err.chars().take(500).collect();
        if err.chars().count() > 500 {
            msg.push('…');
        }
        self.last_error_msg = Some(msg);
        self.consecutive_oks = 0;
        self.consecutive_fails = self.consecutive_fails.saturating_add(1);
        self.total_fail = self.total_fail.saturating_add(1);
    }

    /// True when the source has piled up enough consecutive failures that
    /// the public UI should surface it (Phase G banner + "Open on
    /// <source>" link swap on result cards).
    pub fn is_degraded(&self) -> bool {
        self.consecutive_fails >= DEGRADED_THRESHOLD
    }
}

/// Read-after-init map: built once in `main.rs` with the set of source
/// ids and never resized. Lookups are O(N) on a 3-entry HashMap — i.e.,
/// effectively free — and avoid pulling in a `DashMap` dependency.
pub type HealthMap = Arc<HashMap<&'static str, RwLock<SourceHealth>>>;

pub fn new(ids: &[&'static str]) -> HealthMap {
    let mut map = HashMap::with_capacity(ids.len());
    for id in ids {
        map.insert(*id, RwLock::new(SourceHealth::default()));
    }
    Arc::new(map)
}

pub fn record_ok(map: &HealthMap, source_id: &str) {
    if let Some(lock) = map.get(source_id) {
        if let Ok(mut h) = lock.write() {
            h.record_ok();
        }
    }
}

pub fn record_err(map: &HealthMap, source_id: &str, err: &str) {
    if let Some(lock) = map.get(source_id) {
        if let Ok(mut h) = lock.write() {
            h.record_err(err);
        }
    }
}

/// Snapshot the whole map. Sorted by source id for stable rendering.
pub fn snapshot(map: &HealthMap) -> Vec<(&'static str, SourceHealth)> {
    let mut out: Vec<_> = map
        .iter()
        .map(|(k, v)| (*k, v.read().map(|g| g.clone()).unwrap_or_default()))
        .collect();
    out.sort_by_key(|(k, _)| *k);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_then_err_resets_consecutive() {
        let mut h = SourceHealth::default();
        h.record_ok();
        h.record_ok();
        assert_eq!(h.consecutive_oks, 2);
        h.record_err("boom");
        assert_eq!(h.consecutive_oks, 0);
        assert_eq!(h.consecutive_fails, 1);
        assert!(!h.is_degraded());
    }

    #[test]
    fn degraded_after_threshold_fails() {
        let mut h = SourceHealth::default();
        for _ in 0..DEGRADED_THRESHOLD {
            h.record_err("boom");
        }
        assert!(h.is_degraded());
        h.record_ok();
        assert!(!h.is_degraded());
    }

    #[test]
    fn err_message_truncated() {
        let mut h = SourceHealth::default();
        let long = "x".repeat(10_000);
        h.record_err(&long);
        let stored = h.last_error_msg.unwrap();
        assert!(stored.chars().count() <= 501); // 500 + ellipsis
        assert!(stored.ends_with('…'));
    }

    #[test]
    fn snapshot_is_sorted_and_stable() {
        let map = new(&["zeta", "alpha", "mu"]);
        let snap = snapshot(&map);
        let ids: Vec<&str> = snap.iter().map(|(k, _)| *k).collect();
        assert_eq!(ids, vec!["alpha", "mu", "zeta"]);
    }
}
