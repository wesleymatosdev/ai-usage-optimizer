//! Claude Code usage collector — estimates 5h token usage from local JSONL
//! session logs at ~/.claude/projects/**/*.jsonl. Zero API key needed.
//!
//! Limitation: undercounts usage from other devices (JSONL is local-only).

use crate::config::Config;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Recursively find files matching a suffix under root, without a walkdir dependency.
fn find_files(root: &PathBuf, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_files(&path, suffix, out);
        } else if path.to_string_lossy().ends_with(suffix) {
            out.push(path);
        }
    }
}

/// Parse an RFC3339-ish timestamp (with Z or +HH:MM offset) to unix seconds.
/// Minimal parser — good enough for Claude's ISO8601 timestamps, no chrono dep.
fn parse_iso_to_unix(ts: &str) -> Option<f64> {
    // Expected shape: 2026-08-31T02:24:36.532688Z or with +00:00 offset.
    let ts = ts.trim();
    let (date_part, time_part) = ts.split_once('T')?;
    let mut parts = date_part.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let mo: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;

    // Strip timezone: assume Z or +00:00 (Claude logs are UTC).
    let time_clean = time_part
        .trim_end_matches('Z')
        .split('+')
        .next()
        .unwrap_or(time_part)
        .split('-')
        .next()
        .unwrap_or(time_part);
    let mut tparts = time_clean.split(':');
    let h: i64 = tparts.next()?.parse().ok()?;
    let mi: i64 = tparts.next()?.parse().ok()?;
    let s: f64 = tparts.next()?.parse().ok()?;

    // days since epoch (civil_from_days inverse — days_from_civil, Howard Hinnant algorithm)
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = (yy - era * 400) as i64;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;

    Some(days as f64 * 86400.0 + (h * 3600 + mi * 60) as f64 + s)
}

fn sum_usage_tokens(usage: &Value) -> u64 {
    let keys = ["input_tokens", "output_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"];
    keys.iter()
        .filter_map(|k| usage.get(k).and_then(|v| v.as_u64()))
        .sum()
}

pub fn collect(cfg: &Config) -> (f64, String) {
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let root = home.join(".claude").join("projects");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    let since = now - 5.0 * 3600.0;

    let mut files = Vec::new();
    find_files(&root, ".jsonl", &mut files);

    let mut total: u64 = 0;
    for file in &files {
        let Ok(text) = fs::read_to_string(file) else { continue };
        for line in text.lines() {
            let Ok(record) = serde_json::from_str::<Value>(line) else { continue };
            let message = record.get("message");
            let usage = message.and_then(|m| m.get("usage"));
            let Some(usage) = usage else { continue };
            let stamp = record
                .get("timestamp")
                .and_then(|v| v.as_str())
                .or_else(|| message.and_then(|m| m.get("timestamp")).and_then(|v| v.as_str()));
            let Some(stamp) = stamp else { continue };
            let Some(parsed) = parse_iso_to_unix(stamp) else { continue };
            if parsed >= since {
                total += sum_usage_tokens(usage);
            }
        }
    }

    let budget = cfg
        .providers
        .get("claude-pro")
        .and_then(|p| p.five_hour_token_budget)
        .unwrap_or(225_000);
    let pct = (total as f64 * 100.0 / budget as f64).min(100.0);
    let note = format!("local Claude JSONL estimate: {total}/{budget} tokens (may undercount other devices)");
    (pct, note)
}
