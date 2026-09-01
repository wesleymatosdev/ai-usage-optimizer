//! AI Usage Optimizer — combines exact Hermes OAuth account quotas, direct
//! provider APIs, local estimates, and unmetered local capacity to recommend
//! which route has headroom.
//!
//! Zero cloud dependency: SQLite storage under ~/.local/share/ai-usage-optimizer/,
//! JSON config under ~/.config/ai-usage-optimizer/. Hermes supplies exact
//! ChatGPT Codex and Anthropic OAuth windows without exposing credentials;
//! Z.ai has a direct quota endpoint; local Ollama models are unmetered. Ollama
//! cloud remains manual because it exposes no documented subscription quota.

mod alert;
mod budget;
mod collectors;
mod config;
mod credit;
mod db;
mod lanes;
mod route;
mod routing;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

const PROVIDERS: [&str; 5] = [
    "claude-pro",
    "zai-codeplus",
    "chatgpt-plus",
    "ollama-pro",
    "ollama-local",
];

fn usage() -> &'static str {
    "ai-usage — AI subscription usage tracker\n\n\
Commands:\n  \
  init                              Print config path (creates default config if missing)\n  \
  status                            Show latest known state + recommendation for all providers\n  \
  collect                           Run automatic collectors (Claude, Z.ai API), then show status\n  \
  recommend [--json]                Machine-readable routing recommendation (optional JSON)\n  \
  route [task-class] [--runtime-secs N] [--json]  Session-open routing decision (reasoning/extraction/classifier)\n  \
  observe <provider> <percent> [--note TEXT]   Record a manual usage observation (0-100)\n  \
  credit observe <provider> <used_dollars> [--note TEXT]   Record monthly credit dollars consumed\n  \
  credit status [provider]          Credit-balance detail: remaining, burn rate, projection\n  \
  limit-hit <provider> [--note TEXT] [--ttl-secs N]   Record a transient 429 (backoff; never touches the balance)\n  \
  start-window                      Start Claude's 5h limit clock now (cheap haiku ping)\n  \
  alert                             Push Telegram alerts on level transitions (ok/warning/critical)\n  \
  budget check <provider> <estimate>          Refuse dispatches crossing the ceiling (dollars for credit providers, tokens otherwise)\n  \
  budget record <provider> <tokens> [--at-unix TS]   Record actual tokens a dispatch consumed\n\n\
Providers: claude-pro, zai-codeplus, chatgpt-plus, ollama-pro, ollama-local\n\n\
Credit providers (ollama-pro) model a monthly DOLLAR pool, not a rate limit:\nrecord dollars consumed with `credit observe`; 429s are transient backoff\nand never touch the balance. Note: --note TEXT is visible via `ps` and shell\nhistory. Do not include secrets, tokens, or sensitive context in note arguments.\n"
}

fn default_config_path() -> PathBuf {
    // Exact file path when overridden; otherwise the HOME-based default.
    if let Some(p) = env::var_os("AI_USAGE_CONFIG_PATH") {
        return PathBuf::from(p);
    }
    dirs_config().join("ai-usage-optimizer").join("config.json")
}

fn default_db_path() -> PathBuf {
    if let Some(p) = env::var_os("AI_USAGE_DB_PATH") {
        return PathBuf::from(p);
    }
    dirs_data().join("ai-usage-optimizer").join("usage.sqlite3")
}

fn dirs_config() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".config")
}

fn dirs_data() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".local")
        .join("share")
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprint!("{}", usage());
        return ExitCode::FAILURE;
    }

    let cfg_path = default_config_path();
    let db_path = default_db_path();

    let cfg = match config::load_or_init(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let conn = match db::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("db error: {e}");
            return ExitCode::FAILURE;
        }
    };

    match args[1].as_str() {
        "init" => {
            println!("{}", cfg_path.display());
            ExitCode::SUCCESS
        }
        "status" => {
            print_status(&conn, &cfg);
            ExitCode::SUCCESS
        }
        "collect" => {
            run_collect(&conn, &cfg);
            print_status(&conn, &cfg);
            ExitCode::SUCCESS
        }
        "recommend" => {
            let json_mode = args[2..].iter().any(|a| a == "--json");
            let states = db::latest(&conn);
            let rec = recommendation(&conn, &cfg, &states);
            if json_mode {
                // Explicit verdicts for EVERY rotation-order provider — the
                // silent filter_map that dropped unknown providers is what
                // made "unknown" and "exhausted" indistinguishable. The
                // local-first policy is applied inside classify_all, so a
                // policy-refused provider carries the `local-first` verdict
                // in `candidates` AND the `excluded` map — a machine consumer
                // never sees a suppressed provider as dispatchable.
                let backoff = backoff_providers(&conn);
                let classified = routing::classify_all(&cfg, &states, &backoff);
                let candidates: Vec<serde_json::Value> = classified
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "provider": c.provider,
                            "percent": c.percent,
                            "source": c.source,
                            "note": c.note,
                            "verdict": c.verdict.as_str(),
                            "has_headroom": c.has_headroom,
                            "reset_in_secs": c.reset_in_secs,
                        })
                    })
                    .collect();
                let excluded: serde_json::Map<String, serde_json::Value> = classified
                    .iter()
                    .filter(|c| c.verdict != routing::Verdict::Eligible)
                    .map(|c| (c.provider.clone(), serde_json::json!(c.verdict.as_str())))
                    .collect();
                let out = serde_json::json!({
                    "recommended": rec.provider,
                    "headroom_percent": rec.headroom,
                    "message": rec.text,
                    "candidates": candidates,
                    "excluded": excluded,
                });
                println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
            } else {
                println!("{}", rec.text);
            }
            ExitCode::SUCCESS
        }
        "route" => {
            let task_class_arg = args
                .get(2)
                .and_then(|s| route::TaskClass::parse(s))
                .unwrap_or(route::TaskClass::Reasoning);
            let runtime_secs: i64 = args
                .iter()
                .position(|a| a == "--runtime-secs")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(1800);
            let json_mode = args[2..].iter().any(|a| a == "--json");

            let states = db::latest(&conn);
            let provider_states =
                route::states_from_observations(&cfg.rotation_order, &states, &route_hints());
            let thresholds = route::Thresholds {
                warning: cfg.thresholds.warning,
                critical: cfg.thresholds.critical,
            };
            let req = route::RouteRequest {
                task_class: task_class_arg,
                expected_runtime_secs: runtime_secs,
            };
            let decision = route::decide(&provider_states, &req, &thresholds);

            if json_mode {
                let ranked: Vec<serde_json::Value> = decision
                    .ranked
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "provider": c.provider,
                            "reason": c.reason,
                            "confidence": c.confidence,
                        })
                    })
                    .collect();
                let out = serde_json::json!({
                    "recommended": decision.recommended.as_ref().map(|c| &c.provider),
                    "reason": decision.recommended.as_ref().map(|c| &c.reason),
                    "confidence": decision.recommended.as_ref().map(|c| c.confidence),
                    "task_class": task_class_arg.as_str(),
                    "ranked": ranked,
                });
                println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
            } else {
                match &decision.recommended {
                    Some(c) => println!(
                        "Route to {} (confidence {:.2}): {}",
                        c.provider, c.confidence, c.reason
                    ),
                    None => println!(
                        "No provider has a verified observation — run `ai-usage collect` first."
                    ),
                }
            }
            ExitCode::SUCCESS
        }
        "observe" => {
            if args.len() < 4 {
                eprintln!("usage: ai-usage observe <provider> <percent> [--note TEXT]");
                return ExitCode::FAILURE;
            }
            let provider = &args[2];
            if !PROVIDERS.contains(&provider.as_str()) {
                eprintln!("unknown provider: {provider}. Valid: {:?}", PROVIDERS);
                return ExitCode::FAILURE;
            }
            let percent: f64 = match args[3].parse() {
                Ok(p) => p,
                Err(_) => {
                    eprintln!("percent must be a number 0-100");
                    return ExitCode::FAILURE;
                }
            };
            if !(0.0..=100.0).contains(&percent) {
                eprintln!("percent must be between 0 and 100");
                return ExitCode::FAILURE;
            }
            let note = extract_note(&args[4..]).unwrap_or_else(|| "manual observation".to_string());
            if let Err(e) = db::observe(&conn, provider, Some(percent), "manual", &note) {
                eprintln!("db error: {e}");
                return ExitCode::FAILURE;
            }
            println!("recorded");
            ExitCode::SUCCESS
        }
        "limit-hit" => {
            if args.len() < 3 {
                eprintln!("usage: ai-usage limit-hit <provider> [--note TEXT] [--ttl-secs N]");
                return ExitCode::FAILURE;
            }
            let provider = &args[2];
            if !PROVIDERS.contains(&provider.as_str()) {
                eprintln!("unknown provider: {provider}. Valid: {:?}", PROVIDERS);
                return ExitCode::FAILURE;
            }
            let note = extract_note(&args[3..])
                .unwrap_or_else(|| "provider returned 429/session rate limit".to_string());
            let ttl = args[3..]
                .iter()
                .position(|a| a == "--ttl-secs")
                .and_then(|i| args[3..].get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(db::DEFAULT_RATE_LIMIT_TTL_SECS);
            let at_unix = args[3..]
                .iter()
                .position(|a| a == "--at-unix")
                .and_then(|i| args[3..].get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(db::now_unix());
            // A 429 is TRANSIENT: a rate_limit_event with a TTL, never a
            // percent=100 observation. The credit balance is untouched.
            if let Err(e) = db::record_rate_limit(&conn, provider, &note, at_unix, ttl) {
                eprintln!("db error: {e}");
                return ExitCode::FAILURE;
            }
            println!("recorded (transient backoff, ttl {ttl}s)");
            ExitCode::SUCCESS
        }
        "credit" => match cmd_credit(&args[2..], &conn, &cfg) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        "start-window" => {
            // Start Claude's 5h clock NOW with a cheap haiku ping, then observe
            // the fresh window so the tracker reflects it immediately.
            match collectors::claude::start_window() {
                Ok(note) => {
                    if let Err(e) =
                        db::observe(&conn, "claude-pro", Some(0.0), "window-start", &note)
                    {
                        eprintln!("db error: {e}");
                    }
                    println!("{note}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        "alert" => {
            alert::run(&conn, &cfg);
            ExitCode::SUCCESS
        }
        "budget" => {
            let mut rest = args[2..].iter();
            let sub = rest.next().map(String::as_str).unwrap_or("");
            match sub {
                "record" => {
                    let provider = match rest.next() {
                        Some(p) => p.clone(),
                        None => {
                            eprintln!(
                                "usage: ai-usage budget record <provider> <tokens> [--at-unix TS]"
                            );
                            return ExitCode::FAILURE;
                        }
                    };
                    if !PROVIDERS.contains(&provider.as_str()) {
                        eprintln!("unknown provider: {provider}. Valid: {:?}", PROVIDERS);
                        return ExitCode::FAILURE;
                    }
                    let tokens: u64 = match rest.next().and_then(|t| t.parse().ok()) {
                        Some(t) => t,
                        None => {
                            eprintln!("tokens must be a non-negative integer");
                            return ExitCode::FAILURE;
                        }
                    };
                    let at_unix = rest
                        .next()
                        .zip(rest.next())
                        .and_then(|(flag, val)| {
                            (flag == "--at-unix")
                                .then_some(())
                                .and_then(|_| val.parse().ok())
                        })
                        .unwrap_or_else(budget::now_unix);
                    if let Err(e) = db::record_spend(&conn, &provider, tokens, at_unix) {
                        eprintln!("db error: {e}");
                        return ExitCode::FAILURE;
                    }
                    println!("recorded");
                    ExitCode::SUCCESS
                }
                "check" => {
                    // usage: budget check <provider> [estimate_tokens]
                    let provider = match rest.next() {
                        Some(p) => p.clone(),
                        None => {
                            eprintln!("usage: ai-usage budget check <provider> [estimate_tokens]");
                            return ExitCode::FAILURE;
                        }
                    };
                    if !PROVIDERS.contains(&provider.as_str()) {
                        eprintln!("unknown provider: {provider}. Valid: {:?}", PROVIDERS);
                        return ExitCode::FAILURE;
                    }
                    let estimate: Option<String> = rest.next().cloned();
                    let Some(estimate_str) = estimate else {
                        eprintln!(
                            "refused: no cost estimate — every dispatch must declare its \
                             estimated cost before running (budget check <provider> <tokens|dollars>)"
                        );
                        return ExitCode::FAILURE;
                    };
                    let now = budget::now_unix();
                    let is_credit = cfg
                        .providers
                        .get(&provider)
                        .map(|p| p.kind == "credit_balance")
                        .unwrap_or(false);
                    if is_credit {
                        // Credit providers budget in DOLLARS against the
                        // monthly pool — a token count is the wrong quantity.
                        let Ok(estimate_dollars) = estimate_str.parse::<f64>() else {
                            eprintln!("estimate must be dollars for credit providers (e.g. 0.05)");
                            return ExitCode::FAILURE;
                        };
                        if estimate_dollars < 0.0 {
                            eprintln!("estimate must be non-negative");
                            return ExitCode::FAILURE;
                        }
                        let d = budget::check_credit(&conn, &cfg, &provider, estimate_dollars, now);
                        if d.allowed {
                            println!("{}", d.message);
                            ExitCode::SUCCESS
                        } else {
                            eprintln!("{}", d.message);
                            ExitCode::FAILURE
                        }
                    } else {
                        let Ok(estimate) = estimate_str.parse::<u64>() else {
                            eprintln!("estimate must be a non-negative integer");
                            return ExitCode::FAILURE;
                        };
                        let decision =
                            budget::check(&conn, &cfg, &provider, estimate, budget::now_unix());
                        if decision.allowed {
                            println!("{}", decision.message);
                            ExitCode::SUCCESS
                        } else {
                            eprintln!("{}", decision.message);
                            ExitCode::FAILURE
                        }
                    }
                }
                other => {
                    eprintln!("unknown budget subcommand: {other}");
                    eprintln!("usage: ai-usage budget check <provider> <estimate> | budget record <provider> <tokens> [--at-unix TS]");
                    ExitCode::FAILURE
                }
            }
        }
        "lane" => {
            let mut rest = args[2..].iter();
            let sub = rest.next().map(String::as_str).unwrap_or("");
            let provider = rest.next().cloned().unwrap_or_default();
            if !PROVIDERS.contains(&provider.as_str()) {
                eprintln!("unknown provider: {provider}. Valid: {:?}", PROVIDERS);
                ExitCode::FAILURE
            } else {
                match sub {
                    "claim" => match lanes::claim(&conn, &cfg, &provider, lanes::now_unix()) {
                        Ok(active) => {
                            println!("lane claimed ({active} active)");
                            ExitCode::SUCCESS
                        }
                        Err(e) => {
                            eprintln!("{e}");
                            ExitCode::FAILURE
                        }
                    },
                    "release" => match lanes::release(&conn, &provider) {
                        Ok(removed) => {
                            println!(
                                "{}",
                                if removed {
                                    "lane released"
                                } else {
                                    "no active lane to release"
                                }
                            );
                            ExitCode::SUCCESS
                        }
                        Err(e) => {
                            eprintln!("{e}");
                            ExitCode::FAILURE
                        }
                    },
                    other => {
                        eprintln!("unknown lane subcommand: {other}");
                        eprintln!(
                            "usage: ai-usage lane claim <provider> | lane release <provider>"
                        );
                        ExitCode::FAILURE
                    }
                }
            }
        }
        other => {
            eprintln!("unknown command: {other}\n");
            eprint!("{}", usage());
            ExitCode::FAILURE
        }
    }
}

fn route_hints() -> std::collections::HashMap<String, (route::CostTier, Vec<route::TaskClass>)> {
    use route::{CostTier, TaskClass};
    let mut hints = std::collections::HashMap::new();
    hints.insert(
        "claude-pro".to_string(),
        (CostTier::Subscription, vec![TaskClass::Reasoning]),
    );
    hints.insert(
        "chatgpt-plus".to_string(),
        (
            CostTier::Subscription,
            vec![TaskClass::Reasoning, TaskClass::Extraction],
        ),
    );
    hints.insert(
        "zai-codeplus".to_string(),
        (
            CostTier::Subscription,
            vec![TaskClass::Extraction, TaskClass::Classifier],
        ),
    );
    hints.insert(
        "ollama-pro".to_string(),
        (CostTier::Metered, vec![TaskClass::Extraction]),
    );
    hints.insert(
        "ollama-local".to_string(),
        (CostTier::Local, vec![TaskClass::Classifier]),
    );
    hints
}

fn extract_note(rest: &[String]) -> Option<String> {
    let mut iter = rest.iter();
    while let Some(a) = iter.next() {
        if a == "--note" {
            return iter.next().cloned();
        }
    }
    None
}

/// `credit` subcommands — the dollar-form surface for credit_balance providers.
fn cmd_credit(
    args: &[String],
    conn: &rusqlite::Connection,
    cfg: &config::Config,
) -> Result<ExitCode, String> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "observe" => {
            let provider = args.get(1).ok_or_else(|| {
                "usage: ai-usage credit observe <provider> <used_dollars> [--note TEXT]".to_string()
            })?;
            if !PROVIDERS.contains(&provider.as_str()) {
                return Err(format!("unknown provider: {provider}. Valid: {PROVIDERS:?}"));
            }
            let used: f64 = args
                .get(2)
                .and_then(|s| s.parse().ok())
                .ok_or("used_dollars must be a number (e.g. 5.10)")?;
            if !(0.0..=100_000.0).contains(&used) {
                return Err("used_dollars must be >= 0".to_string());
            }
            let note = extract_note(&args[3..]).unwrap_or_else(|| "dashboard reading".to_string());
            let at_unix = args[3..]
                .iter()
                .position(|a| a == "--at-unix")
                .and_then(|i| args[3..].get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(db::now_unix());
            db::record_credit(conn, provider, used, &note, at_unix)
                .map_err(|e| format!("db error: {e}"))?;
            // Mirror into observations (derived percent) so percent-based
            // routing/alerts see credit providers on the same surface.
            if let Some(p) = cfg.providers.get(provider) {
                if p.kind == "credit_balance" {
                    let pool = p.monthly_pool_dollars.unwrap_or(60.0);
                    let pct = if pool > 0.0 {
                        (used / pool * 100.0).clamp(0.0, 100.0)
                    } else {
                        0.0
                    };
                    let note = format!("${used:.2} of ${pool:.2} monthly pool");
                    db::observe(conn, provider, Some(pct), "credit", &note)
                        .map_err(|e| format!("db error: {e}"))?;
                }
            }
            println!("recorded");
            Ok(ExitCode::SUCCESS)
        }
        "status" => {
            let provider = args.get(1).cloned().unwrap_or_else(|| "ollama-pro".into());
            if !PROVIDERS.contains(&provider.as_str()) {
                return Err(format!("unknown provider: {provider}. Valid: {PROVIDERS:?}"));
            }
            print_credit_status(conn, cfg, &provider, db::now_unix());
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!(
            "unknown credit subcommand: {other}\nusage: ai-usage credit observe <provider> <used_dollars> | credit status [provider]"
        )),
    }
}

/// Credit detail: dollars consumed / remaining, burn rate, projected spend at
/// reset. Pure reporting — never probes any endpoint.
fn print_credit_status(
    conn: &rusqlite::Connection,
    cfg: &config::Config,
    provider: &str,
    now: i64,
) {
    let Some(p) = cfg.providers.get(provider) else {
        println!("{provider}: not configured");
        return;
    };
    if p.kind != "credit_balance" {
        println!(
            "{provider}: not a credit_balance provider (kind {})",
            p.kind
        );
        return;
    }
    let reset_at = p
        .reset_at
        .as_deref()
        .and_then(crate::collectors::claude::parse_iso_to_unix)
        .map(|f| f as i64);
    let plan = credit::CreditPlan {
        monthly_pool_dollars: p.monthly_pool_dollars.unwrap_or(60.0),
        reset_at_unix: reset_at,
    };
    let latest = db::latest_credit(conn, provider);
    let Some(latest) = latest else {
        println!(
            "{provider}: no credit readings — run `ai-usage credit observe {provider} <dollars>`"
        );
        return;
    };
    // Earliest reading still inside the current period = burn baseline.
    let baseline = period_baseline(conn, provider, latest.at_unix);
    let state = credit::credit_state(&plan, &latest, baseline.as_ref(), now);
    println!(
        "{provider}: ${:.2} used of ${:.2} pool ({:.1}%), ${:.2} remaining",
        state.used_dollars, state.pool_dollars, state.percent_used, state.remaining_dollars
    );
    if let Some(reload) = p.reload_monthly_max_dollars {
        println!("  safety net: auto-reload monthly max ${reload:.2} (balance $0)");
    }
    match state.burn_per_hour {
        Some(rate) => println!("  burn rate: ${rate:.2}/h (from recorded readings)"),
        None => println!("  burn rate: need 2+ readings in this period"),
    }
    match (state.projected_at_reset, plan.reset_at_unix) {
        (Some(projected), Some(reset)) => println!(
            "  projected at reset ({}): ${:.2} of ${:.2} pool",
            human_duration(reset.saturating_sub(now)),
            projected,
            plan.monthly_pool_dollars
        ),
        _ => println!("  projection: needs both a burn rate and a configured reset date"),
    }
    let backoffs = db::active_rate_limits(conn, provider, now);
    if !backoffs.is_empty() {
        let e = &backoffs[0];
        println!(
            "  transient 429 backoff active for {} (balance unaffected)",
            human_duration(e.expires_at().saturating_sub(now))
        );
    }
}

fn period_baseline(
    conn: &rusqlite::Connection,
    provider: &str,
    latest_at: i64,
) -> Option<db::CreditObservation> {
    // Second-latest reading overall is the simplest honest baseline; a
    // period-aware query can come later once resets are recorded.
    conn.query_row(
        "SELECT used_dollars, COALESCE(note, ''), observed_at_unix
         FROM credit_observations WHERE provider = ?1 AND observed_at_unix < ?2
         ORDER BY id DESC LIMIT 1",
        rusqlite::params![provider, latest_at],
        |row| {
            Ok(db::CreditObservation {
                used_dollars: row.get(0)?,
                note: row.get(1)?,
                at_unix: row.get(2)?,
            })
        },
    )
    .ok()
}

fn human_duration(secs: i64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m")
    }
}

/// Providers with a currently-active transient 429/rate-limit event — the
/// `backoff` verdict input for routing. Kept tiny and conn-taking so both
/// `recommend --json` and `recommendation()` share one source of truth.
fn backoff_providers(conn: &rusqlite::Connection) -> std::collections::HashSet<String> {
    backoff_providers_conn(conn)
}

fn backoff_providers_conn(conn: &rusqlite::Connection) -> std::collections::HashSet<String> {
    let now = db::now_unix();
    let mut out = std::collections::HashSet::new();
    for provider in PROVIDERS {
        if !db::active_rate_limits(conn, provider, now).is_empty() {
            out.insert(provider.to_string());
        }
    }
    out
}

fn run_collect(conn: &rusqlite::Connection, cfg: &config::Config) {
    // Collectors that hard-fail must leave an explicit UNAVAILABLE marker
    // (percent NULL) — a silent eprintln made "quota source down" and
    // "never checked" indistinguishable downstream.
    fn record_failure(conn: &rusqlite::Connection, provider: &str, error: &str) {
        let note: String = error.chars().take(200).collect();
        if let Err(e) = db::observe(conn, provider, None, "unavailable", &note) {
            eprintln!("db error recording {provider} unavailability: {e}");
        }
    }

    match collectors::hermes::collect("anthropic") {
        Ok(snapshot) => {
            if let Err(e) = db::observe(
                conn,
                "claude-pro",
                Some(snapshot.percent),
                &snapshot.source,
                &snapshot.note,
            ) {
                eprintln!("db error recording claude-pro: {e}");
            }
        }
        Err(error) => {
            eprintln!("Hermes Anthropic quota unavailable ({error}); using local fallback");
            let (pct, source, note) = collectors::claude::collect(cfg);
            if let Err(e) = db::observe(conn, "claude-pro", Some(pct), &source, &note) {
                eprintln!("db error recording claude-pro: {e}");
            }
        }
    }

    match collectors::hermes::collect("openai-codex") {
        Ok(snapshot) => {
            if let Err(e) = db::observe(
                conn,
                "chatgpt-plus",
                Some(snapshot.percent),
                &snapshot.source,
                &snapshot.note,
            ) {
                eprintln!("db error recording chatgpt-plus: {e}");
            }
        }
        Err(error) => {
            eprintln!("Hermes ChatGPT quota unavailable: {error}");
            record_failure(conn, "chatgpt-plus", &error);
        }
    }

    match collectors::zai::collect(cfg) {
        Ok(Some((pct, note))) => {
            if let Err(e) = db::observe(conn, "zai-codeplus", Some(pct), "direct-api", &note) {
                eprintln!("db error recording zai-codeplus: {e}");
            }
        }
        Ok(None) => eprintln!("ZAI_API_KEY is not set"),
        Err(e) => {
            eprintln!("Z.ai quota request failed: {e}");
            record_failure(conn, "zai-codeplus", &e);
        }
    }

    match collectors::ollama::collect(cfg) {
        Ok(snapshot) => {
            if let Err(e) = db::observe(
                conn,
                "ollama-local",
                Some(snapshot.percent),
                &snapshot.source,
                &snapshot.note,
            ) {
                eprintln!("db error recording ollama-local: {e}");
            }
        }
        Err(error) => {
            eprintln!("Ollama local capacity unavailable: {error}");
            record_failure(conn, "ollama-local", &error);
        }
    }
}

fn print_status(conn: &rusqlite::Connection, cfg: &config::Config) {
    let states = db::latest(conn);
    for provider in PROVIDERS {
        match states.get(provider) {
            Some(s) => {
                let pct = s
                    .percent
                    .map(|p| format!("{p:5.1}%"))
                    .unwrap_or_else(|| " unset".to_string());
                println!("{provider:14} {pct}  {}  {}", s.source, s.note);
            }
            None => println!("{provider:14} unknown — no observation"),
        }
    }
    let rec = recommendation(conn, cfg, &states);
    println!("{}", rec.text);
    fire_alerts(conn, cfg, &states, &rec.text);
}

/// A routing recommendation: which provider to send the next task to.
struct Recommendation {
    provider: Option<String>,
    headroom: f64,
    text: String,
}

fn recommendation(
    conn: &rusqlite::Connection,
    cfg: &config::Config,
    states: &std::collections::HashMap<String, db::Observation>,
) -> Recommendation {
    // Select among ELIGIBLE-with-headroom candidates only, using the same
    // classify_all surface that `recommend --json` exposes. The local-first
    // policy is applied inside classify_all (single source of truth), so a
    // provider the policy refuses carries the `local-first` verdict there
    // and can never be recommended here.
    let backoff = backoff_providers_conn(conn);
    let classified = routing::classify_all(cfg, states, &backoff);
    let mut candidates: Vec<(f64, &str)> = classified
        .iter()
        .filter(|c| c.verdict == routing::Verdict::Eligible && c.has_headroom)
        .filter_map(|c| c.percent.map(|pct| (pct, c.provider.as_str())))
        .collect();
    candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    match candidates.first() {
        Some((pct, provider)) => Recommendation {
            provider: Some(provider.to_string()),
            headroom: 100.0 - pct,
            text: format!("Recommended next: {provider} ({:.0}% verified headroom).", 100.0 - pct),
        },
        None => Recommendation {
            provider: None,
            headroom: 0.0,
            text: "No provider has verified headroom. Record a current consumer-subscription observation before switching.".to_string(),
        },
    }
}

fn fire_alerts(
    conn: &rusqlite::Connection,
    cfg: &config::Config,
    states: &std::collections::HashMap<String, db::Observation>,
    rec: &str,
) {
    for (provider, s) in states {
        let Some(pct) = s.percent else { continue };
        if pct < cfg.thresholds.warning {
            continue;
        }
        let level = if pct >= cfg.thresholds.critical {
            "critical"
        } else {
            "warning"
        };
        let message = format!("{provider} at {pct:.0}% — {rec}");
        if let Err(e) = db::alert(conn, provider, level, pct, &message) {
            eprintln!("db error recording alert: {e}");
            continue;
        }
        println!("ALERT: {message}");
    }
}
