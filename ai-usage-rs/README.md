# AI Usage Optimizer — Rust MVP

**Status:** Working MVP (2026-08-31), replaces two earlier Python attempts (see `experiments/`).
Zero-cloud, zero-config-framework, single static binary.

## Why Rust, not Python

Wesley's explicit call after seeing two independent Python MVPs: Python defaults are "bloated,
odd quirks" for a tool whose whole value proposition is a lightweight background daemon. Two
agents (a GLM sibling and this Claude Sonnet agent) built the same design in Python
independently the same night — see `experiments/README.md` for the full story. Both are archived,
neither is the real tool.

## What works today

| Provider | Tracking | Real signal? |
|---|---|---|
| Claude Pro | Automatic — parses `~/.claude/projects/**/*.jsonl` | Real token counts; % needs calibration (Anthropic publishes no numeric limit — see below) |
| Z.ai CodePlus | Automatic, needs `ZAI_API_KEY` env var | Real quota endpoint, polled live |
| ChatGPT Plus | **Manual only** | No usage API exists for consumer plans |
| Ollama Pro | **Manual only** | Confirmed no usage API exists (GitHub ollama/ollama#15663, #16448, both open/duplicate) |

## Build & run

```bash
cd ai-usage-rs
cargo build --release
./target/release/ai-usage status
./target/release/ai-usage collect                 # runs Claude + Z.ai collectors, then status
./target/release/ai-usage observe chatgpt-plus 35 --note "portal showed 35%"
./target/release/ai-usage limit-hit ollama-pro --note "429 during session"
```

Data lives in `~/.local/share/ai-usage-optimizer/usage.sqlite3`. Config in
`~/.config/ai-usage-optimizer/config.json` (auto-created on first run, edit
`rotation_order`, thresholds, or `claude-pro.five_hour_token_budget` there).

## Calibrating the Claude Pro limit

Anthropic doesn't publish a numeric token cap for Pro/Max. Config ships with a
placeholder (`225000`). To calibrate:
1. Run `/usage` in Claude Code, note the 5h window %.
2. At the same moment, run `ai-usage collect` — note the raw token count printed.
3. `limit = raw_tokens / (pct_shown / 100)`, update `five_hour_token_budget` in config.

## Not yet built (next iteration)

- Telegram push + Hermes memory write on alert (currently prints only)
- launchd scheduling for periodic `collect`
- Rotation auto-switch wired into Hermes delegate_task routing
- Ollama Cloud proxy-logger to estimate token velocity locally (no server-side counter exists)

## Design reference

Full provider research: `../DESIGN.md` (2026-08-30).
