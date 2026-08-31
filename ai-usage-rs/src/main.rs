//! AI Usage Optimizer — tracks usage across AI subscriptions (Claude Pro, Z.ai
//! CodePlus, ChatGPT Plus, Ollama Pro) and recommends which one has headroom.
//!
//! Zero cloud dependency: SQLite storage under ~/.local/share/ai-usage-optimizer/,
//! JSON config under ~/.config/ai-usage-optimizer/. Two providers (ChatGPT Plus,
//! Ollama Pro) are manual-only — neither exposes a usage API for consumer plans
//! (confirmed via GitHub issues ollama/ollama#15663, #16448 — both open/duplicate
//! as of Aug 2026). Claude Pro is estimated from local JSONL session logs. Z.ai
//! CodePlus has a real quota endpoint, polled when ZAI_API_KEY is set.

mod config;
mod db;
mod collectors;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

const PROVIDERS: [&str; 4] = ["claude-pro", "zai-codeplus", "chatgpt-plus", "ollama-pro"];

fn usage() -> &'static str {
    "ai-usage — AI subscription usage tracker\n\n\
Commands:\n  \
  init                              Print config path (creates default config if missing)\n  \
  status                            Show latest known state + recommendation for all providers\n  \
  collect                           Run automatic collectors (Claude JSONL, Z.ai API), then show status\n  \
  observe <provider> <percent> [--note TEXT]   Record a manual usage observation (0-100)\n  \
  limit-hit <provider> [--note TEXT]           Record that a provider just returned a 429/limit error\n\n\
Providers: claude-pro, zai-codeplus, chatgpt-plus, ollama-pro\n"
}

fn default_config_path() -> PathBuf {
    dirs_config().join("ai-usage-optimizer").join("config.json")
}

fn default_db_path() -> PathBuf {
    dirs_data().join("ai-usage-optimizer").join("usage.sqlite3")
}

fn dirs_config() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".config")
}

fn dirs_data() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".local").join("share")
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
        other => {
            eprintln!("unknown command: {other}\n");
            eprint!("{}", usage());
            ExitCode::FAILURE
        }
    }
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
    let (pct, note) = collectors::claude::collect(cfg);
    if let Err(e) = db::observe(conn, "claude-pro", Some(pct), "local-jsonl", &note) {
        eprintln!("db error recording claude-pro: {e}");
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
}

fn print_status(conn: &rusqlite::Connection, cfg: &config::Config) {
    let states = db::latest(conn);
    for provider in PROVIDERS {
        match states.get(provider) {
            Some(s) => {
                let pct = s.percent.map(|p| format!("{p:5.1}%")).unwrap_or_else(|| " unset".to_string());
                println!("{provider:14} {pct}  {}  {}", s.source, s.note);
            }
            None => println!("{provider:14} unknown — no observation"),
        }
    }
    let rec = recommendation(cfg, &states);
    println!("{rec}");
    fire_alerts(conn, cfg, &states, &rec);
}

fn recommendation(cfg: &config::Config, states: &std::collections::HashMap<String, db::Observation>) -> String {
    let mut candidates: Vec<(f64, &str)> = cfg
        .rotation_order
        .iter()
        .filter_map(|p| {
            states.get(p).and_then(|s| s.percent).filter(|&pct| pct < 90.0).map(|pct| (pct, p.as_str()))
        })
        .collect();
    candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    match candidates.first() {
        Some((pct, provider)) => format!("Recommended next: {provider} ({:.0}% verified headroom).", 100.0 - pct),
        None => "No provider has verified headroom. Record a current consumer-subscription observation before switching.".to_string(),
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
        let level = if pct >= cfg.thresholds.critical { "critical" } else { "warning" };
        let message = format!("{provider} at {pct:.0}% — {rec}");
        if let Err(e) = db::alert(conn, provider, level, pct, &message) {
            eprintln!("db error recording alert: {e}");
            continue;
        }
        println!("ALERT: {message}");
    }
}
