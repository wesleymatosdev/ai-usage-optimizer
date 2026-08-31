# AI Usage Optimizer — MVP

Local, stdlib-only proof of concept for preventing subscription-limit surprises.

## What works now

- Records **Ollama Pro**, **ChatGPT Plus**, **Claude Pro**, and **Z.ai CodePlus** observations in local SQLite.
- Polls the documented Z.ai Coding Plan quota endpoint when `ZAI_API_KEY` is present.
- Estimates Claude Code's current five-hour usage from local `~/.claude/projects/**/*.jsonl` files.
- Treats ChatGPT Plus and Ollama Pro honestly as manual observations: neither consumer subscription exposes a supported quota API.
- Records a 429/limit hit as 100%, then recommends the configured provider with verified headroom.

## Run

```bash
cd ~/projects/personal/ai-usage-optimizer
python3 -m ai_usage init
python3 -m ai_usage collect
python3 -m ai_usage observe chatgpt-plus 35 --note 'portal showed 35%'
python3 -m ai_usage limit-hit ollama-pro --note 'Ollama cloud session quota exhausted'
python3 -m ai_usage status
python3 -m unittest discover -s tests -v
```

All data is local under `~/.local/share/ai-usage-optimizer/`. The generated config is `~/.config/ai-usage-optimizer/config.json`; edit `rotation_order`, thresholds, and Claude calibration there.

## Deliberate MVP boundary

This does not pretend it can read consumer-subscription quota data that providers do not expose. A future adapter can ingest a browser-exported status or provider-approved endpoint. Telegram and Hermes-memory delivery remain the next integration slice; alert events are already stored locally in SQLite.
