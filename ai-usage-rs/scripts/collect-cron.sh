#!/bin/bash
# ai-usage collect + alert wrapper — sources Hermes env for API keys/tokens.
set -a
[ -f "$HOME/.hermes/.env" ] && source "$HOME/.hermes/.env"
set +a
"$HOME/projects/personal/ai-usage-optimizer/ai-usage-rs/target/release/ai-usage" collect >/dev/null 2>&1
"$HOME/projects/personal/ai-usage-optimizer/ai-usage-rs/target/release/ai-usage" alert