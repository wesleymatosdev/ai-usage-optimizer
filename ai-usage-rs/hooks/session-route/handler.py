"""Session-open routing hook — logs ai-usage's routing recommendation.

Read-only: never mutates ~/.hermes config, model_override, or any Hermes
runtime state. It shells out to the ai-usage `route` command (pure function
over already-collected provider data, no live network probing) and appends
the recommendation to a local JSONL log under
~/.local/share/ai-usage-optimizer/session-route-hook.jsonl so the coordinator
(or a human) can see what the fleet's own tool would have picked, and decide
whether to apply it. Applying the recommendation (switching model_override,
updating a MoA preset, etc.) is a separate, explicit coordinator action —
this hook does not do it automatically per the task's hard guardrail.
"""

import json
import os
import subprocess
import time
from pathlib import Path

AI_USAGE_BIN = str(
    Path.home()
    / "projects/personal/ai-usage-optimizer/ai-usage-rs/target/release/ai-usage"
)
LOG_PATH = Path.home() / ".local/share/ai-usage-optimizer/session-route-hook.jsonl"


def _log(entry: dict) -> None:
    try:
        LOG_PATH.parent.mkdir(parents=True, exist_ok=True)
        entry["logged_at"] = time.time()
        with LOG_PATH.open("a") as f:
            f.write(json.dumps(entry) + "\n")
    except Exception:
        # Logging must never raise — worst case we lose one line of history.
        pass


async def handle(event_type: str, context: dict):
    try:
        if event_type != "session:start":
            return

        task_class = os.environ.get("AI_USAGE_ROUTE_TASK_CLASS", "reasoning")

        if not Path(AI_USAGE_BIN).exists():
            _log(
                {
                    "event": event_type,
                    "session_id": context.get("session_id"),
                    "error": f"ai-usage binary not found at {AI_USAGE_BIN}",
                }
            )
            return

        result = subprocess.run(
            [AI_USAGE_BIN, "route", task_class, "--json"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode != 0:
            _log(
                {
                    "event": event_type,
                    "session_id": context.get("session_id"),
                    "error": f"ai-usage route exited {result.returncode}: {result.stderr.strip()}",
                }
            )
            return

        payload = json.loads(result.stdout)
        _log(
            {
                "event": event_type,
                "session_id": context.get("session_id"),
                "platform": context.get("platform"),
                "task_class": task_class,
                "recommended": payload.get("recommended"),
                "reason": payload.get("reason"),
                "confidence": payload.get("confidence"),
            }
        )
    except Exception as exc:  # noqa: BLE001 — swallow, log, never crash the gateway
        _log({"event": event_type, "error": f"{type(exc).__name__}: {exc}"})
