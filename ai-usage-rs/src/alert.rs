//! Telegram alerting — pushes only on level TRANSITIONS per provider.
//!
//! Alert design (non-happy-path rule): a non-happy-path message must
//! answer three things —
//!   1. what happened (provider, percent, level, and the actual cause),
//!   2. what it means for you (dispatches there will fail / work again),
//!   3. what to do (switch to the provider with real headroom, or wait).
//!
//! Never just a level name. Token comes from TELEGRAM_BOT_TOKEN in the
//! environment — never hardcoded.
//!
//! `AI_USAGE_DRY_RUN=1` prints messages instead of sending them (pipeline
//! testing must never disturb the user; this is the only sanctioned way to
//! verify).

use crate::config::Config;
use crate::db::{self, Observation};
use rusqlite::Connection;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Minimum seconds between pushes for the same provider — prevents alert flapping.
const COOLDOWN_SECS: f64 = 30.0 * 60.0;

fn state_path() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".local/share/ai-usage-optimizer/alert-state.json")
}

fn level_for(pct: Option<f64>, cfg: &Config) -> &'static str {
    match pct {
        None => "unknown",
        Some(p) if p >= cfg.thresholds.critical => "critical",
        Some(p) if p >= cfg.thresholds.warning => "warning",
        _ => "ok",
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// The "what to do" half: route to the provider with the most headroom.
fn action_line(states: &HashMap<String, Observation>, exclude: &str, cfg: &Config) -> String {
    let mut candidates: Vec<(f64, &str)> = states
        .iter()
        .filter_map(|(p, s)| {
            if p == exclude {
                return None;
            }
            s.percent
                .filter(|&pct| pct < cfg.thresholds.warning)
                .map(|pct| (pct, p.as_str()))
        })
        .collect();
    candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    match candidates.first() {
        Some((pct, p)) => format!("Route work to {p} ({:.0}% used).", pct),
        None => "No provider has verified headroom — use Ollama local models or wait for a window reset.".to_string(),
    }
}

/// Compose a single alert message following cause → consequence → action.
fn compose(
    provider: &str,
    pct: f64,
    source: &str,
    note: &str,
    level: &str,
    states: &HashMap<String, Observation>,
    cfg: &Config,
) -> String {
    let cause = match source {
        "limit-hit" => {
            format!("{provider} just returned a hard limit (429/session cap) — {note}")
        }
        "manual" => {
            format!("{provider} observation: {:.0}% used ({note})", pct)
        }
        _ => {
            let detail = if note.is_empty() { source } else { note };
            format!("{provider} is at {:.0}% used ({})", pct, detail)
        }
    };

    if level == "ok" {
        return format!("ai-usage: {cause} — it can take dispatches again.");
    }

    let impact = match level {
        "critical" => "dispatches there will fail outright.",
        "warning" => "dispatches there may start failing soon.",
        _ => "",
    };
    format!(
        "ai-usage: {cause} — {impact} {}",
        action_line(states, provider, cfg)
    )
}

fn send_telegram(text: &str) -> bool {
    let token = match env::var("TELEGRAM_BOT_TOKEN") {
        Ok(t) => t,
        Err(_) => {
            eprintln!("telegram: missing token, skipping push");
            return false;
        }
    };
    let chat = env::var("TELEGRAM_HOME_CHANNEL")
        .or_else(|_| env::var("TELEGRAM_ALLOWED_USERS"))
        .unwrap_or_default();
    if chat.is_empty() {
        eprintln!("telegram: missing chat, skipping push");
        return false;
    }
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let payload = serde_json::json!({
        "chat_id": chat,
        "text": text,
    });
    match ureq::post(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send_json(payload)
    {
        Ok(r) => r.status() == 200,
        Err(e) => {
            // ureq's Display includes the request URL, which embeds the bot
            // token (Telegram puts it in the path) — redact before logging.
            let msg = e.to_string().replace(&token, "***");
            eprintln!("telegram send failed: {msg}");
            false
        }
    }
}

/// Run the alert loop: compare latest DB observations against the previous
/// state file, push on transitions, respect per-provider cooldown.
pub fn run(conn: &Connection, cfg: &Config) {
    let dry_run = env::var("AI_USAGE_DRY_RUN").ok().as_deref() == Some("1");
    let states = db::latest(conn);
    let current: HashMap<String, &str> = states
        .iter()
        .map(|(p, s)| (p.clone(), level_for(s.percent, cfg)))
        .collect();

    let path = state_path();
    let previous: Map<String, Value> = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Map::new(),
    };

    // First run — record baseline and exit.
    if previous.is_empty() && !path.exists() {
        let baseline: Map<String, Value> = current
            .iter()
            .map(|(k, v)| (k.clone(), Value::String((*v).to_string())))
            .collect();
        let _ = fs::write(
            &path,
            serde_json::to_string_pretty(&baseline).unwrap_or_default() + "\n",
        );
        let keys: Vec<&String> = current.keys().collect();
        println!("baseline recorded: {keys:?}");
        return;
    }

    // Detect level changes (skip unknowns).
    let changes: Vec<(&String, &str)> = current
        .iter()
        .filter(|(p, lvl)| {
            **lvl != "unknown"
                && previous
                    .get(*p)
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    != **lvl
        })
        .map(|(p, lvl)| (p, *lvl))
        .collect();

    if changes.is_empty() {
        println!("no level changes");
        // Still save the state to capture any new providers.
        let mut merged = previous.clone();
        for (p, lvl) in &current {
            merged.insert(p.clone(), Value::String((*lvl).to_string()));
        }
        let _ = fs::write(
            &path,
            serde_json::to_string_pretty(&merged).unwrap_or_default() + "\n",
        );
        return;
    }

    let now = now_secs();
    let mut merged = previous.clone();

    let mut changes_sorted = changes.clone();
    changes_sorted.sort_by(|a, b| a.0.cmp(b.0));

    for (provider, level) in &changes_sorted {
        let s = match states.get(*provider) {
            Some(s) => s,
            None => continue,
        };
        let pct = match s.percent {
            Some(p) => p,
            None => continue,
        };
        let msg = compose(provider, pct, &s.source, &s.note, level, &states, cfg);

        let push_key = format!("__pushed_{provider}");
        let last_push = merged
            .get(&push_key)
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let elapsed = now - last_push;

        if elapsed < COOLDOWN_SECS {
            println!("cooldown: skipping push for {provider} (last {elapsed:.0}s ago)");
            merged.insert(provider.to_string(), Value::String((*level).to_string()));
            continue;
        }

        if dry_run {
            println!("[dry-run] would push: {msg}");
        } else if send_telegram(&msg) {
            println!("pushed: {msg}");
            merged.insert(push_key, Value::from(now));
        }
        merged.insert(provider.to_string(), Value::String((*level).to_string()));
    }

    let _ = fs::write(
        &path,
        serde_json::to_string_pretty(&merged).unwrap_or_default() + "\n",
    );

    // Burn-rate awareness (Part B): a cumulative-percent reading hides
    // velocity. Check every credit-pool provider's projection and push when
    // the projected month-end first crosses the pool — transition-gated via
    // credit_alerts, so a sustained overrun does not spam.
    check_burn_alerts(conn, cfg, dry_run);
}

/// Push a Telegram warning when a credit provider's burn projection first
/// overruns its monthly pool (or when the projection changes materially).
/// Fires at most once per state (kind = "burn-overrun"), with its own
/// cooldown column so repeated `alert` runs never flap.
fn check_burn_alerts(conn: &Connection, cfg: &Config, dry_run: bool) {
    let now = crate::budget::now_unix();
    for provider in crate::PROVIDERS {
        let configured = cfg
            .providers
            .get(provider)
            .and_then(|p| p.monthly_credit_dollars)
            .unwrap_or(0.0);
        if configured <= 0.0 {
            continue;
        }
        let state = crate::credits::credit_state(conn, cfg, provider, now);
        let Some(burn) = &state.burn else { continue };
        let over = burn.projected_overrun > 0.0;
        let last: Option<i64> = conn
            .query_row(
                "SELECT fired_at FROM credit_alerts
                 WHERE provider = ?1 AND kind = 'burn-overrun'
                 ORDER BY id DESC LIMIT 1",
                rusqlite::params![provider],
                |row| row.get(0),
            )
            .ok();
        if over && last.is_none() {
            let message = format!(
                "ai-usage: {} burn rate ${:.2}/h projects ${:.2} by reset — \
                 ${:.2} PAST the ${:.2} monthly pool. Impact: the plan exhausts before \
                 its reset and paid dispatches start failing. Action: move heavy loops to \
                 subscription-seat or local routes now.",
                provider,
                burn.dollars_per_hour,
                burn.projected_at_reset,
                burn.projected_overrun,
                state.pool_dollars
            );
            if dry_run {
                println!("[dry-run] would push: {message}");
            } else if send_telegram(&message) {
                println!("pushed: {message}");
            }
            let _ = conn.execute(
                "INSERT INTO credit_alerts (provider, kind, message, fired_at) \
                 VALUES (?1, 'burn-overrun', ?2, ?3)",
                rusqlite::params![provider, message, now],
            );
        } else if !over && last.is_some() {
            // Projection returned inside the pool → clear the latch so a
            // future overrun can fire again.
            let _ = conn.execute(
                "DELETE FROM credit_alerts WHERE provider = ?1 AND kind = 'burn-overrun'",
                rusqlite::params![provider],
            );
        }
    }
}
