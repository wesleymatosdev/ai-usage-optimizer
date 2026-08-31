//! Claude Code usage collector.
//!
//! Primary signal: `~/.claude.json` → `cachedUsageUtilization` — the server's own
//! five_hour/seven_day utilization percentages (with reset timestamps). The cache
//! only refreshes every few hours (verified: /usage and messages do NOT force a
//! refresh), so:
//!   - fresh cache (≤90m): trust its 5h percentage directly
//!   - stale cache: fall back to the local JSONL token estimate, using the
//!     five_hour_token_budget auto-calibrated from the freshest usable snapshot
//!     (window tokens ÷ utilization %), so the estimate stays grounded in real
//!     server numbers instead of a placeholder.
//!
//! Window semantics: the 5h clock starts on the FIRST message after a reset, not
//! at the reset itself. A window past its resets_at is spent and reads 0% until
//! the next message starts a new one (`ai-usage start-window` fires that ping).

use crate::config::Config;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Cache younger than this (minutes) is trusted verbatim for the 5h figure.
const FRESH_MIN: f64 = 90.0;
/// Server 5h utilization below this % is too small to back-solve a budget from.
const CALIB_MIN_PCT: f64 = 3.0;

struct CacheInfo {
    fetched_ms: f64,
    five_raw: f64,
    five_reset: Option<f64>,
    week_raw: f64,
    week_reset: Option<f64>,
}

fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn read_cache() -> Option<CacheInfo> {
    let text = fs::read_to_string(home().join(".claude.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let cached = v.get("cachedUsageUtilization")?;
    let util = cached.get("utilization")?;
    let window = |name: &str| -> Option<(f64, Option<f64>)> {
        let w = util.get(name)?;
        let pct = w.get("utilization")?.as_f64()?;
        let reset = w
            .get("resets_at")
            .and_then(|x| x.as_str())
            .and_then(parse_iso_to_unix);
        Some((pct, reset))
    };
    let (five_raw, five_reset) = window("five_hour")?;
    let (week_raw, week_reset) = window("seven_day").unwrap_or((0.0, None));
    Some(CacheInfo {
        fetched_ms: cached.get("fetchedAtMs")?.as_f64()?,
        five_raw,
        five_reset,
        week_raw,
        week_reset,
    })
}

fn now_unix() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn calib_path() -> PathBuf {
    dirs_data().join("claude-budget-calibration.json")
}

fn dirs_data() -> PathBuf {
    home()
        .join(".local")
        .join("share")
        .join("ai-usage-optimizer")
}

fn write_calibration(budget: u64, now: f64) {
    let json = format!(
        "{{\"five_hour_token_budget\": {budget}, \"calibrated_at\": {:.0}}}\n",
        now
    );
    let _ = fs::create_dir_all(dirs_data());
    let _ = fs::write(calib_path(), json);
}

fn read_calibration() -> Option<u64> {
    let text = fs::read_to_string(calib_path()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("five_hour_token_budget")?.as_u64()
}

fn configured_budget(cfg: &Config) -> u64 {
    cfg.providers
        .get("claude-pro")
        .and_then(|p| p.five_hour_token_budget)
        .unwrap_or(225_000)
}

/// Recursively find files matching a suffix under root, without a walkdir dependency.
/// Uses `DirEntry::file_type()` (does not follow symlinks) rather than
/// `Path::is_dir()`, so a symlink cycle under root can't cause unbounded recursion.
fn find_files(root: &PathBuf, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            find_files(&path, suffix, out);
        } else if file_type.is_file() && path.to_string_lossy().ends_with(suffix) {
            out.push(path);
        }
    }
}

/// Parse an RFC3339-ish timestamp (with Z or +HH:MM offset) to unix seconds.
/// Minimal parser — good enough for Claude's ISO8601 timestamps, no chrono dep.
pub(crate) fn parse_iso_to_unix(ts: &str) -> Option<f64> {
    // Expected shape: 2026-08-31T02:24:36.532688Z or with +00:00 offset.
    let ts = ts.trim();
    let (date_part, time_part) = ts.split_once('T')?;
    let mut parts = date_part.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let mo: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;

    // Strip timezone: assume Z or +00:00 (Claude timestamps are UTC/ISO with offset).
    let time_clean = time_part
        .trim_end_matches('Z')
        .split('+')
        .next()
        .unwrap_or(time_part);
    let mut tparts = time_clean.split(':');
    let h: i64 = tparts.next()?.parse().ok()?;
    let mi: i64 = tparts.next()?.parse().ok()?;
    let s: f64 = tparts.next()?.parse().ok()?;

    // days since epoch (days_from_civil, Howard Hinnant algorithm)
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
    let keys = [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ];
    keys.iter()
        .filter_map(|k| usage.get(k).and_then(|v| v.as_u64()))
        .sum()
}

/// Total JSONL usage tokens recorded in [from, to] (unix seconds).
fn jsonl_tokens(from: f64, to: f64) -> u64 {
    let root = home().join(".claude").join("projects");
    let mut files = Vec::new();
    find_files(&root, ".jsonl", &mut files);

    let mut total: u64 = 0;
    for file in &files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines() {
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let message = record.get("message");
            let Some(usage) = message.and_then(|m| m.get("usage")) else {
                continue;
            };
            let stamp = record
                .get("timestamp")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    message
                        .and_then(|m| m.get("timestamp"))
                        .and_then(|v| v.as_str())
                });
            let Some(stamp) = stamp else { continue };
            let Some(parsed) = parse_iso_to_unix(stamp) else {
                continue;
            };
            if parsed >= from && parsed <= to {
                total += sum_usage_tokens(usage);
            }
        }
    }
    total
}

/// Back-solve the true 5h token budget from a usable cache snapshot:
/// tokens in the snapshot's 5h window ÷ (utilization/100). Writes the result
/// to disk so later (stale-cache) collects stay calibrated. Skipped when the
/// utilization is too small to be precise or the window already expired.
fn calibrate_from_cache(c: &CacheInfo, now: f64) -> Option<u64> {
    if c.five_raw < CALIB_MIN_PCT {
        return None;
    }
    if c.five_reset.map_or(false, |r| now >= r) {
        return None; // window expired; its percentage no longer maps to tokens
    }
    let to = c.fetched_ms / 1000.0;
    let tokens = jsonl_tokens(to - 5.0 * 3600.0, to);
    if tokens == 0 {
        return None;
    }
    let budget = (tokens as f64 / (c.five_raw / 100.0)) as u64;
    if budget == 0 {
        return None;
    }
    write_calibration(budget, now);
    Some(budget)
}

/// Effective Claude usage. Returns (percent, source, note).
/// effective = max(5h, weekly) — either limit blocks dispatch.
pub fn collect(cfg: &Config) -> (f64, String, String) {
    let now = now_unix();
    let Some(c) = read_cache() else {
        // No snapshot at all: JSONL estimate against calibrated-or-placeholder budget.
        let budget = read_calibration().unwrap_or_else(|| configured_budget(cfg));
        let tokens = jsonl_tokens(now - 5.0 * 3600.0, now);
        let pct = (tokens as f64 * 100.0 / budget as f64).min(100.0);
        let note = format!("no server cache; JSONL estimate {tokens}/{budget} tokens");
        return (pct, "local-jsonl".to_string(), note);
    };

    let age_min = (now - c.fetched_ms / 1000.0) / 60.0;
    let week_live = if c.week_reset.map_or(false, |r| now >= r) {
        0.0
    } else {
        c.week_raw
    };

    let (five_pct, five_note) = if age_min <= FRESH_MIN {
        let pct = if c.five_reset.map_or(false, |r| now >= r) {
            0.0
        } else {
            c.five_raw
        };
        (pct, format!("5h {:.0}% (server, fresh)", pct))
    } else {
        // Stale snapshot: re-derive 5h from live local tokens. Use the budget
        // auto-calibrated from the freshest eligible snapshot; label honestly
        // when only the placeholder is available (real budget likely higher).
        let calibrated = calibrate_from_cache(&c, now).or_else(read_calibration);
        let budget = calibrated.unwrap_or_else(|| configured_budget(cfg));
        let tokens = jsonl_tokens(now - 5.0 * 3600.0, now);
        let pct = (tokens as f64 * 100.0 / budget as f64).min(100.0);
        let quality = if calibrated.is_some() {
            "calibrated"
        } else {
            "UNCALIBRATED placeholder budget"
        };
        let note = format!("5h ~{pct:.0}% (JSONL {tokens}/{budget}, {quality})");
        (pct, note)
    };

    let effective = five_pct.max(week_live).min(100.0);
    let reset_txt = |ts: Option<f64>| {
        ts.map(|r| {
            let mins_left = ((r - now) / 60.0) as i64;
            if mins_left > 0 {
                format!("resets in {mins_left}m")
            } else {
                "expired — starts on next message".to_string()
            }
        })
        .unwrap_or_else(|| "resets ?".to_string())
    };
    let note = format!(
        "{}; weekly {:.0}% ({}); cache {:.0}m old",
        five_note,
        week_live,
        reset_txt(c.week_reset),
        age_min
    );
    (effective, "server-cache".to_string(), note)
}

/// Start a fresh 5h window with a minimal request. Claude's clock only starts
/// counting on the first message after a reset, so a cheap ping ("runs the
/// limit clock") buys the full 5h from NOW instead of letting it idle-expire.
/// Uses the same OAuth auth Claude Code already has; model is the cheapest
/// available alias. Returns a human-readable result note.
pub fn start_window() -> Result<String, String> {
    // Bare "claude" is resolved via PATH, which a tampered environment could
    // hijack; CLAUDE_BIN lets a caller pin a verified absolute path instead.
    let bin = env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());

    let mut child = std::process::Command::new(&bin)
        .arg("-p")
        .arg("--model")
        .arg("haiku")
        .arg("Reply with exactly: window-started")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn {bin}: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{bin} ping timed out after 30s"));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => return Err(format!("failed to wait on {bin}: {e}")),
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to collect {bin} output: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{bin} ping failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let body = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(format!(
        "5h window started now (reply: {body}). The window runs until 5h from this moment."
    ))
}
