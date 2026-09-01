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
mod collectors;
mod config;
mod db;
mod route;

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
  start-window                     Start Claude's 5h limit clock now (cheap haiku ping)\n  \
  alert                            Push Telegram alerts on level transitions (ok/warning/critical)\n\
\nProviders: claude-pro, zai-codeplus, chatgpt-plus, ollama-pro, ollama-local\n\nNote: --note TEXT is visible via `ps` and shell history. Do not include\nsecrets, tokens, or sensitive context in note arguments.\n"
}

fn default_config_path() -> PathBuf {
    dirs_config().join("ai-usage-optimizer").join("config.json")
}

fn default_db_path() -> PathBuf {
    dirs_data().join("ai-usage-optimizer").join("usage.sqlite3")
}

fn dirs_config() -> PathBuf {
    env::var_os("AI_USAGE_CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".config")
        })
}

fn dirs_data() -> PathBuf {
    env::var_os("AI_USAGE_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".local")
                .join("share")
        })
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
            let rec = recommendation(&cfg, &states);
            if json_mode {
                let candidates: Vec<serde_json::Value> = cfg
                    .rotation_order
                    .iter()
                    .filter_map(|p| {
                        states.get(p).map(|s| {
                            serde_json::json!({
                                "provider": p,
                                "percent": s.percent,
                                "source": s.source,
                                "note": s.note,
                                "has_headroom": s.percent.map_or(false, |pct| pct < cfg.thresholds.warning),
                            })
                        })
                    })
                    .collect();
                let out = serde_json::json!({
                    "recommended": rec.provider,
                    "headroom_percent": rec.headroom,
                    "message": rec.text,
                    "candidates": candidates,
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
            if let Err(e) = db::observe(&conn, provider, Some(100.0), "limit-hit", &note) {
                eprintln!("db error: {e}");
                return ExitCode::FAILURE;
            }
            println!("recorded");
            ExitCode::SUCCESS
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
        Err(error) => eprintln!("Hermes ChatGPT quota unavailable: {error}"),
    }

    match collectors::zai::collect(cfg) {
        Ok(Some((pct, note))) => {
            if let Err(e) = db::observe(conn, "zai-codeplus", Some(pct), "direct-api", &note) {
                eprintln!("db error recording zai-codeplus: {e}");
            }
        }
        Ok(None) => eprintln!("ZAI_API_KEY is not set"),
        Err(e) => eprintln!("Z.ai quota request failed: {e}"),
    }

    match collectors::ollama::collect() {
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
        Err(error) => eprintln!("Ollama local capacity unavailable: {error}"),
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
    let rec = recommendation(cfg, &states);
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
    cfg: &config::Config,
    states: &std::collections::HashMap<String, db::Observation>,
) -> Recommendation {
    let mut candidates: Vec<(f64, &str)> = cfg
        .rotation_order
        .iter()
        .filter_map(|p| {
            states
                .get(p)
                .and_then(|s| s.percent)
                .filter(|&pct| pct < cfg.thresholds.warning)
                .map(|pct| (pct, p.as_str()))
        })
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
