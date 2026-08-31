#!/bin/bash
# ai-usage collect + alert wrapper — sources Hermes env for API keys/tokens.
env_file="$HOME/.hermes/.env"
if [ -f "$env_file" ]; then
    owner=$(stat -f "%u" "$env_file" 2>/dev/null || stat -c "%u" "$env_file" 2>/dev/null)
    mode=$(stat -f "%OLp" "$env_file" 2>/dev/null || stat -c "%a" "$env_file" 2>/dev/null)
    # Sourcing runs whatever is in this file as shell code — refuse unless it's
    # owned by us and closed to group/other (no group or other bits set).
    if [ "$owner" = "$(id -u)" ] && [ $(( 8#$mode & 8#077 )) -eq 0 ]; then
        set -a
        source "$env_file"
        set +a
    else
        echo "collect-cron: refusing to source $env_file — must be owned by uid $(id -u) with mode 600 (got owner=$owner mode=$mode)" >&2
    fi
fi
"$HOME/projects/personal/ai-usage-optimizer/ai-usage-rs/target/release/ai-usage" collect >/dev/null 2>&1
"$HOME/projects/personal/ai-usage-optimizer/ai-usage-rs/target/release/ai-usage" alert