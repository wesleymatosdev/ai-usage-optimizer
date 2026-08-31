# ai-usage

Local-first usage tracker for AI subscriptions. Polls real usage signals from
providers that expose them, accepts manual observations for those that don't,
fires Telegram alerts on threshold transitions, and recommends which provider
has headroom for your next task.

Self-hosted, zero-cloud, single static Rust binary. No API keys leave your
machine.

## Why

When you're rotating between Claude, Z.ai, ChatGPT, and Ollama to stay under
rolling 5-hour limits, hitting a 429 blind is annoying. This tool gives you
verified headroom visibility so you can route work before limits bite.

## Supported providers

| Provider | Tracking | Real signal? |
|---|---|---|
| Claude Pro | Automatic — server cache from `~/.claude.json` + local JSONL fallback | Yes (server-reported 5h/weekly %, auto-calibrated token estimate between refreshes) |
| Z.ai CodePlus | Automatic, needs `GLM_API_KEY` or `ZAI_API_KEY` env var | Yes (real quota endpoint, polled live) |
| ChatGPT Plus | Manual only | No usage API exists for consumer plans |
| Ollama Pro | Manual only | No usage API exists (confirmed via GitHub ollama/ollama#15663, #16448) |

## Build & run

```bash
cd ai-usage-rs
cargo build --release
./target/release/ai-usage status          # show latest known state + recommendation
./target/release/ai-usage collect         # run automatic collectors, then status
./target/release/ai-usage recommend --json  # machine-readable routing payload
./target/release/ai-usage observe chatgpt-plus 35 --note "portal showed 35%"
./target/release/ai-usage limit-hit ollama-pro --note "429 during session"
./target/release/ai-usage start-window    # start Claude's 5h clock now (cheap haiku ping)
./target/release/ai-usage alert           # push Telegram alerts on level transitions
```

Data lives in `~/.local/share/ai-usage-optimizer/usage.sqlite3`.
Config in `~/.config/ai-usage-optimizer/config.json` (auto-created on first run;
edit `rotation_order`, thresholds, or `five_hour_token_budget` there).

## Alerts

Telegram alerts fire only on level transitions (ok / warning 90% / critical
95%), with a 30-minute per-provider cooldown to prevent flapping. Every alert
follows a cause / consequence / action structure:

```
ai-usage: ollama-pro just returned a hard limit (429/session cap) — delegate
hit session usage limit — dispatches there will fail outright. Route work to
chatgpt-plus (20% used).
```

Recovery messages confirm the provider can take dispatches again (no action
needed).

`AI_USAGE_DRY_RUN=1` prints messages instead of sending them — the only
sanctioned way to verify the pipeline without disturbing anyone.

## Daemon (macOS)

A launchd agent runs `collect` + `alert` every 5 minutes:

```bash
# plist at ~/Library/LaunchAgents/com.wesleymatos.ai-usage-collect.plist
# wrapper at ai-usage-rs/scripts/collect-cron.sh (sources ~/.hermes/.env for keys)
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.wesleymatos.ai-usage-collect.plist
```

The wrapper sources environment variables for API keys — tokens are never
copied or hardcoded.

## Calibrating Claude Pro

Anthropic doesn't publish a numeric token cap for Pro/Max. The config ships
with a placeholder (`225000`). To calibrate:

1. Run `/usage` in Claude Code, note the 5h window %.
2. At the same moment, run `ai-usage collect` — note the raw token count.
3. `limit = raw_tokens / (pct_shown / 100)`, update `five_hour_token_budget`
   in config.

The tool also auto-calibrates from fresh `~/.claude.json` cache snapshots when
nonzero 5h utilization is present.

## Architecture

```
ai-usage-rs/
  src/
    main.rs        CLI entry point, command dispatch
    config.rs      JSON config (thresholds, rotation order, providers)
    db.rs          SQLite storage (observations, alerts)
    alert.rs       Telegram alerter (transitions, cooldown, cause/consequence/action)
    collectors/
      claude.rs    ~/.claude.json cache + JSONL fallback + window start
      zai.rs       Z.ai quota endpoint
  scripts/
    collect-cron.sh  env-sourcing + collect + alert (for launchd)
```

Earlier Python MVP attempts are archived in `experiments/` — two independent
agents built the same design in Python the same night, both were scrapped in
favor of Rust. See `experiments/README.md` for the full story.

Full provider research and design rationale: `DESIGN.md`.

## License

MIT