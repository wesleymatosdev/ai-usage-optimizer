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
mod burn;
mod collectors;
mod config;
mod credits;
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
  limit-hit <provider> [--note TEXT]           Record that a provider just returned a 429/limit error\n  \
  start-window                      Start Claude's 5h limit clock now (cheap haiku ping)\n  \
  alert                             Push Telegram alerts on level transitions (ok/warning/critical)\n  \
  budget check <provider> <estimate>          Refuse dispatches that would cross hard daily/weekly ceilings\n  \
  budget record <provider> <tokens> [--at-unix TS]   Record actual tokens a dispatch consumed\\n  \\
  credit record <provider> <dollars-used> [--at-unix TS]   Record a cumulative credit reading\\n  \\
  credit status <provider>                    Show dollars used/remaining, burn rate, projection\\n\
\nProviders: claude-pro, zai-codeplus, chatgpt-plus, ollama-pro, ollama-local\n\nNote: --note TEXT is visible via `ps` and shell history. Do not include\nsecrets, tokens, or sensitive context in note arguments.\n"
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
                let classified = routing::classify_all(&conn, &cfg, &states);
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
                eprintln!("usage: ai-usage limit-hit <provider> [--note TEXT]");
                return ExitCode::FAILURE;
            }
            let provider = &args[2];
            if !PROVIDERS.contains(&provider.as_str()) {
                eprintln!("unknown provider: {provider}. Valid: {:?}", PROVIDERS);
                return ExitCode::FAILURE;
            }
            let note = extract_note(&args[3..])
                .unwrap_or_else(|| "provider reported limit/rate exhaustion".to_string());
            // A 429/session limit is TRANSIENT: record a timestamped rate
            // event (TTL backoff, default 15m) — never a percent=100
            // observation. The old sticky behavior rendered one subagent 429
            // as plan exhaustion and corrupted every routing decision that
            // read it. Monthly consumption lives in credit_events and is
            // untouched here.
            if let Err(e) = credits::record_rate_event(&conn, provider, &note, budget::now_unix()) {
                eprintln!("db error: {e}");
                return ExitCode::FAILURE;
            }
            let state = credits::backoff_state(&conn, &cfg, provider, budget::now_unix());
            println!(
                "recorded: transient backoff on {provider} for {}s (clears in {}s) — \
                 monthly consumption untouched",
                state.ttl_secs, state.clear_in_secs
            );
            ExitCode::SUCCESS
        }
        "credit" => {
            let mut rest = args[2..].iter();
            let sub = rest.next().map(String::as_str).unwrap_or("");
            match sub {
                "record" => {
                    // usage: credit record <provider> <dollars-used> [--at-unix TS]
                    let provider = match rest.next() {
                        Some(p) => p.clone(),
                        None => {
                            eprintln!(
                                "usage: ai-usage credit record <provider> <dollars-used> [--at-unix TS]"
                            );
                            return ExitCode::FAILURE;
                        }
                    };
                    if !PROVIDERS.contains(&provider.as_str()) {
                        eprintln!("unknown provider: {provider}. Valid: {:?}", PROVIDERS);
                        return ExitCode::FAILURE;
                    }
                    let dollars: f64 = match rest.next().and_then(|t| t.parse().ok()) {
                        Some(d) if d >= 0.0 => d,
                        _ => {
                            eprintln!("dollars-used must be a non-negative number (cumulative, from the provider dashboard)");
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
                    if let Err(e) = credits::record_credit(&conn, &provider, dollars, at_unix) {
                        eprintln!("db error: {e}");
                        return ExitCode::FAILURE;
                    }
                    println!("recorded ${dollars:.2} cumulative for {provider}");
                    ExitCode::SUCCESS
                }
                "status" => {
                    let provider = match rest.next() {
                        Some(p) => p.clone(),
                        None => {
                            eprintln!("usage: ai-usage credit status <provider>");
                            return ExitCode::FAILURE;
                        }
                    };
                    if !PROVIDERS.contains(&provider.as_str()) {
                        eprintln!("unknown provider: {provider}. Valid: {:?}", PROVIDERS);
                        return ExitCode::FAILURE;
                    }
                    let state = credits::credit_state(&conn, &cfg, &provider, budget::now_unix());
                    print_credit_state(&state);
                    ExitCode::SUCCESS
                }
                other => {
                    eprintln!("unknown credit subcommand: {other}");
                    eprintln!("usage: ai-usage credit record <provider> <dollars-used> [--at-unix TS] | credit status <provider>");
                    ExitCode::FAILURE
                }
            }
        }
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
                    // usage: budget check <provider> [estimate]
                    // Token estimates gate the token ledger; credit-pool
                    // providers (monthly_credit_dollars configured) are
                    // gated in DOLLARS: a dispatch whose estimated cost
                    // would cross the pool is refused, not reported.
                    let provider = match rest.next() {
                        Some(p) => p.clone(),
                        None => {
                            eprintln!("usage: ai-usage budget check <provider> [estimate]");
                            return ExitCode::FAILURE;
                        }
                    };
                    if !PROVIDERS.contains(&provider.as_str()) {
                        eprintln!("unknown provider: {provider}. Valid: {:?}", PROVIDERS);
                        return ExitCode::FAILURE;
                    }
                    let estimate: Option<u64> = match rest.next() {
                        Some(t) => match t.parse() {
                            Ok(v) => Some(v),
                            Err(_) => {
                                eprintln!("estimate must be a non-negative integer");
                                return ExitCode::FAILURE;
                            }
                        },
                        None => None,
                    };
                    let Some(estimate) = estimate else {
                        eprintln!(
                            "refused: no cost estimate — every dispatch must declare its \
                             estimated cost before running (budget check <provider> <tokens|dollars>)"
                        );
                        return ExitCode::FAILURE;
                    };
                    // Credit-pool providers take the estimate as DOLLARS
                    // (their ceiling is a dollar pool, not tokens).
                    let now = budget::now_unix();
                    if credit_configured(&cfg, &provider) {
                        let estimated_dollars = estimate as f64 / 100.0;
                        match burn::check_credits(&conn, &cfg, &provider, estimated_dollars, now) {
                            Ok(decision) => {
                                if let Some(warning) = &decision.soft_cap_warning {
                                    eprintln!("warning: {warning}");
                                }
                                if decision.allowed {
                                    println!("{}", decision.message);
                                    ExitCode::SUCCESS
                                } else {
                                    eprintln!("REFUSED: {}", decision.message);
                                    ExitCode::FAILURE
                                }
                            }
                            Err(e) => {
                                eprintln!("refused: {e}");
                                ExitCode::FAILURE
                            }
                        }
                    } else {
                        let decision = budget::check(&conn, &cfg, &provider, estimate, now);
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

    // Credit-pool providers: mirror the credit percent-of-pool into the
    // observation stream so route/route-hint consumers see the REAL balance
    // (10.2%), never a stale limit-hit 100%. No network probe — the figure
    // comes from recorded credit readings only.
    for provider in crate::PROVIDERS {
        let pool = cfg
            .providers
            .get(provider)
            .and_then(|p| p.monthly_credit_dollars)
            .unwrap_or(0.0);
        if pool <= 0.0 {
            continue;
        }
        let now = budget::now_unix();
        let state = credits::credit_state(conn, cfg, provider, now);
        if let (Some(used), Some(pct)) = (state.used_dollars, state.percent) {
            let note = format!(
                "credit pool ${:.2}: ${:.2} used, ${:.2} left, resets {}",
                state.pool_dollars,
                used,
                state.remaining_dollars,
                crate::db::unix_to_iso(state.reset_at_unix)
            );
            if let Err(e) = db::observe(conn, provider, Some(pct), "credit-pool", &note) {
                eprintln!("db error recording {provider} credit state: {e}");
            }
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
        // Credit-pool providers report dollars + percent-of-pool, not the
        // last observation's percent (a 429 note must never read as 100%).
        if credit_configured(cfg, provider) {
            print_credit_line(conn, cfg, provider);
            continue;
        }
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

/// A provider is credit-modeled when its config declares a monthly pool.
fn credit_configured(cfg: &config::Config, provider: &str) -> bool {
    cfg.providers
        .get(provider)
        .and_then(|p| p.monthly_credit_dollars)
        .unwrap_or(0.0)
        > 0.0
}

/// One status line for a credit-modeled provider: dollars, percent of pool,
/// burn rate, projected month-end — plus the transient backoff when active.
fn print_credit_line(conn: &rusqlite::Connection, cfg: &config::Config, provider: &str) {
    let now = budget::now_unix();
    let state = credits::credit_state(conn, cfg, provider, now);
    let backoff = credits::backoff_state(conn, cfg, provider, now);
    if state.used_dollars.is_none() {
        println!(
            "{provider:14}  unset  credits — no dashboard reading yet \
             (record one: ai-usage credit record {provider} <dollars-used>)"
        );
        if backoff.active {
            println!(
                "{provider:14}  [BACKOFF {}s: {}]",
                backoff.clear_in_secs, backoff.note
            );
        }
        return;
    }
    let used = state.used_dollars.unwrap_or(0.0);
    let mut line = format!(
        "{provider:14} {:5.1}%  credits ${used:.2} of ${:.2} pool (${:.2} left)",
        state.percent.unwrap_or(0.0),
        state.pool_dollars,
        state.remaining_dollars
    );
    if let Some(burn) = &state.burn {
        line.push_str(&format!(
            " — burn ${:.2}/h, projects ${:.2} by reset (+${:.2} over)",
            burn.dollars_per_hour, burn.projected_at_reset, burn.projected_overrun
        ));
    }
    if backoff.active {
        line.push_str(&format!(
            "  [BACKOFF {}s: {}]",
            backoff.clear_in_secs, backoff.note
        ));
    }
    println!("{line}");
}

fn print_credit_state(state: &credits::CreditState) {
    println!(
        "{}: ${:.2} of ${:.2} used ({:.1}%), ${:.2} remaining",
        state.provider,
        state.used_dollars.unwrap_or(0.0),
        state.pool_dollars,
        state.percent.unwrap_or(0.0),
        state.remaining_dollars
    );
    match &state.burn {
        Some(burn) => println!(
            "burn rate ${:.2}/h over the last {:.1}h — projected ${:.2} by reset (+${:.2} past the pool)",
            burn.dollars_per_hour,
            burn.window_secs as f64 / 3600.0,
            burn.projected_at_reset,
            burn.projected_overrun
        ),
        None => println!("burn rate: need at least two credit readings"),
    }
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
    let classified = routing::classify_all(conn, cfg, states);
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
