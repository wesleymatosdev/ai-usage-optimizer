#!/usr/bin/env python3
"""ai-usage Telegram alerter — pushes only on level TRANSITIONS per provider.

Alert design (Wesley's error-interface rule): a non-happy-path message must
answer three things —
  1. what happened (provider, percent, level, and the actual cause),
  2. what it means for you (dispatches there will fail / work again),
  3. what to do (switch to the provider with real headroom, or wait).
Never just a level name. Token comes from TELEGRAM_BOT_TOKEN in the
environment — never hardcoded.
"""

import json
import os
import sqlite3
import sys
import time
import urllib.request
from urllib.parse import urlencode

DB = os.path.expanduser("~/.local/share/ai-usage-optimizer/usage.sqlite3")
STATE = os.path.expanduser("~/.local/share/ai-usage-optimizer/alert-state.json")
WARNING, CRITICAL = 90.0, 95.0
# Minimum seconds between pushes for the same provider — prevents alert flapping.
COOLDOWN_SECS = 30 * 60
# AI_USAGE_DRY_RUN=1 prints messages instead of sending them (pipeline testing
# must never disturb the user; this is the only sanctioned way to verify).
DRY_RUN = os.environ.get("AI_USAGE_DRY_RUN") == "1"


def level_for(pct):
    if pct is None:
        return "unknown"
    if pct >= CRITICAL:
        return "critical"
    if pct >= WARNING:
        return "warning"
    return "ok"


def latest_state():
    """{provider: (percent, source, note)} for the newest observation of each."""
    conn = sqlite3.connect(DB)
    rows = conn.execute(
        "SELECT o.provider, o.percent, o.source, o.note FROM observations o "
        "JOIN (SELECT provider, MAX(id) AS id FROM observations GROUP BY provider) x "
        "ON o.id = x.id"
    ).fetchall()
    conn.close()
    return {p: (pct, src, note or "") for p, pct, src, note in rows}


def action_line(state, exclude):
    """The 'what to do' half: route to the provider with the most headroom."""
    candidates = sorted(
        (pct, p) for p, (pct, _, _) in state.items()
        if p != exclude and pct is not None and pct < WARNING
    )
    if candidates:
        pct, p = candidates[0]
        return f"Route work to {p} ({pct:.0f}% used)."
    return "No provider has verified headroom — use Ollama local models or wait for a window reset."


def compose(provider, pct, source, note, level, state):
    cause = {
        "limit-hit": f"{provider} just returned a hard limit (429/session cap) — {note or 'no details recorded'}",
        "manual": f"{provider} observation: {pct:.0f}% used ({note or 'no source noted'})",
    }.get(source, f"{provider} is at {pct:.0f}% used ({note or source})")

    if level == "ok":
        impact = "it can take dispatches again."
        return f"ai-usage: {cause} — {impact}"

    impact = {
        "critical": "dispatches there will fail outright.",
        "warning": "dispatches there may start failing soon.",
    }[level]
    return f"ai-usage: {cause} — {impact} {action_line(state, provider)}"


def send_telegram(text):
    token = os.environ.get("TELEGRAM_BOT_TOKEN")
    chat = os.environ.get("TELEGRAM_HOME_CHANNEL") or os.environ.get("TELEGRAM_ALLOWED_USERS")
    if not token or not chat:
        print("telegram: missing token/chat, skipping push", file=sys.stderr)
        return False
    url = f"https://api.telegram.org/bot{token}/sendMessage"
    data = urlencode({"chat_id": chat, "text": text}).encode()
    try:
        with urllib.request.urlopen(url, data=data, timeout=15) as r:
            return r.status == 200
    except Exception as e:
        print(f"telegram send failed: {e}", file=sys.stderr)
        return False


def main():
    state = latest_state()
    current = {p: level_for(pct) for p, (pct, _, _) in state.items()}
    try:
        with open(STATE) as f:
            previous = json.load(f)
    except (OSError, ValueError):
        previous = None

    if previous is None:
        with open(STATE, "w") as f:
            json.dump(current, f, indent=2)
        print("baseline recorded:", current)
        return

    changes = {
        p: lvl for p, lvl in current.items() if previous.get(p) != lvl and lvl != "unknown"
    }
    if not changes:
        print("no level changes")
        return

    for provider, lvl in sorted(changes.items()):
        pct, source, note = state[provider]
        msg = compose(provider, pct, source, note, lvl, state)
        last_push = previous.get(f"__pushed_{provider}", 0)
        now = time.time()
        if now - last_push < COOLDOWN_SECS:
            print(f"cooldown: skipping push for {provider} (last {int(now - last_push)}s ago)")
            previous[provider] = lvl
            continue
        if DRY_RUN:
            print(f"[dry-run] would push: {msg}")
        elif send_telegram(msg):
            print(f"pushed: {msg}")
            previous[f"__pushed_{provider}"] = now
        previous[provider] = lvl

    with open(STATE, "w") as f:
        json.dump(previous, f, indent=2)


if __name__ == "__main__":
    main()
