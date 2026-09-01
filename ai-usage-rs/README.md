# ai-usage-rs

Rust CLI tracking usage across AI subscriptions. Single static binary, zero
cloud dependency.

## Commands

```bash
cargo build --release
./target/release/ai-usage status
./target/release/ai-usage collect                 # Claude + Z.ai collectors, then status
./target/release/ai-usage recommend --json        # machine-readable routing payload
./target/release/ai-usage observe <provider> <pct> [--note TEXT]
./target/release/ai-usage limit-hit <provider> [--note TEXT]
./target/release/ai-usage start-window            # start Claude's 5h clock (haiku ping)
./target/release/ai-usage alert                   # Telegram alerts on level transitions
./target/release/ai-usage budget check <provider> <estimate>   # refuse over-ceiling dispatches
./target/release/ai-usage budget record <provider> <tokens> [--at-unix TS]
./target/release/ai-usage credit record <provider> <dollars-used> [--at-unix TS]
./target/release/ai-usage credit status <provider>
```

Data: `~/.local/share/ai-usage-optimizer/usage.sqlite3`
Config: `~/.config/ai-usage-optimizer/config.json` (auto-created on first run)

## Providers

| Provider | Tracking | Real signal? |
|---|---|---|
| Claude Pro | Server cache from `~/.claude.json` + JSONL fallback | Server-reported 5h/weekly %, auto-calibrated token estimate between refreshes |
| Z.ai CodePlus | Quota endpoint (needs `GLM_API_KEY` or `ZAI_API_KEY`) | Real, polled live |
| ChatGPT Plus | Manual only | No consumer usage API exists |
| Ollama Pro | Monthly credit balance (dashboard readings) | Real dollars via `credit record`; burn rate + projection |
| Ollama Local | `/api/tags` reachability + local model count | Unmetered capacity, verified live |

## Credit-pool model (Ollama Pro)

Ollama Pro is a **$60/month credit balance**, not a rate limit — a percentage
of a rate limit and dollars of a monthly credit pool are different quantities.
The tool keeps them separate:

- `credit record ollama-pro 5.05` records a cumulative dashboard reading.
  `credit status` derives percent-of-pool, remaining dollars, burn rate
  ($/h from the delta between readings) and the projected month-end spend.
- `limit-hit <provider>` records a 429/session limit as a TRANSIENT
  `rate_events` row (TTL 15 min, per-provider `rate_limit_backoff_secs`).
  It never writes a 100% observation and never touches the balance — a
  subagent 429 renders as `backoff`, self-clearing, never as plan death.
- Legacy sticky `limit-hit` observations (percent=100, stuck forever) are
  migrated automatically on DB open: moved into `rate_events` with their
  original timestamp, marked `limit-hit-consumed` in the observation stream.
- `budget check ollama-pro <dollars-cents>` REFUSES a dispatch whose
  estimated dollar cost would cross the remaining pool (exit 1), and warns
  (without refusing) when the estimate crosses the daily soft cap.
  A burn projection that overruns the pool before reset refuses
  speculative dispatches outright.
- `alert` pushes a Telegram burn warning the first time a credit provider's
  projection crosses the pool (transition-latched; clears when the
  projection returns inside the pool).

## Alerts

`ai-usage alert` pushes Telegram messages on level transitions (ok / warning
90% / critical 95%) with a 30-min per-provider cooldown. Messages follow
cause / consequence / action. `AI_USAGE_DRY_RUN=1` prints instead of sending.

## Daemon

`scripts/collect-cron.sh` runs `collect` + `alert` — intended for launchd
scheduling every 5 min. Sources `~/.hermes/.env` for API keys.

## Claude calibration

Anthropic doesn't publish a numeric token cap for Pro/Max. Config ships with
a placeholder (`225000`). Calibrate manually (see root README) or let the tool
auto-calibrate from fresh cache snapshots.

## Design reference

Full provider research: `../DESIGN.md`.
Earlier Python MVP attempts: `../experiments/`.