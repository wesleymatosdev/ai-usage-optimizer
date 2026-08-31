# Experiment: Two AI agents independently build the same tool in Python — same night, same brief, zero coordination

**Date:** 2026-08-30/31
**Trigger:** Wesley hit an Ollama Cloud 429 mid-session; realized he had no visibility into subscription usage across 4 AI providers (Claude Pro, ChatGPT Plus, Ollama Pro, Z.ai CodePlus) until it was too late.

## The setup

Hermes dispatched 4 parallel crewmates via `delegate_task` (GLM-5.3 Flash on Z.ai) to work
different threads simultaneously — website reviews, unf.it investigation, and one tasked with
*researching and designing* (not building) an AI usage optimizer. That crewmate produced
`DESIGN.md` — 671 lines, 5 providers researched, real API endpoints documented, architecture
proposed. Explicitly scoped as design-only.

Mid-session, Z.ai itself hit its rate limit (429) — ironic, given the tool being designed was
meant to prevent exactly that surprise. Delegation switched providers (glm-5.2:cloud via ollama).

Later, unprompted (or under a different steer — worth checking session logs), **a sibling agent
went ahead and built the design into working code anyway** — committed as `e698c50 feat: add AI
subscription usage MVP`. Separately, in a *new* session with a different agent (Claude Sonnet 5,
this one), Wesley asked to build the MVP. That agent (me) had no visibility into the sibling's
work, read the same DESIGN.md, and built a second, independent implementation from scratch.

Neither agent knew the other existed until I ran `git status` and found untracked/committed
files that weren't mine.

## What each produced (both Python, both working, both from the same design doc)

### Attempt 1 — sibling agent (glm-5.2 or glm-5.3, unclear which — check git author)
- `ai_usage/__main__.py` — single-file CLI, ~160 lines, argparse-based
- SQLite via raw `sqlite3`, JSON config (not YAML)
- Commands: `init`, `status`, `collect`, `observe`, `limit-hit`
- Has actual unit tests (`tests/test_mvp.py`, 2 tests, passing)
- Calibrated Claude limit already set: `five_hour_token_budget: 225000` (mine left it `null`)
- Ran cleanly first try: `python3 -m ai_usage collect` → real 100% alert against real JSONL data
- Tighter, more idiomatic — one file, no unnecessary package structure

### Attempt 2 — this agent (Claude Sonnet 5, Anthropic)
- Multi-file: `src/db.py`, `src/collectors/claude_code.py`, `cli.py`, `config.yaml`
- SQLite via a small `db.py` module, PyYAML config (had to `pip install` — sibling avoided this
  entirely with stdlib JSON, which is the better call for a "zero-dependency" tool)
- Commands: `status`, `check`, `mark`, `recommend`
- No tests
- Needed a venv + pip install to run (sibling's ran with bare `python3`, zero setup)
- More "designed" (separate collector modules, docstrings) but objectively worse for a tool
  whose whole pitch is "zero-dependency, run anywhere"

## The interesting failure mode

I almost overwrote the sibling's `README.md` without ever reading it first — the write_file
tool caught it: *"was modified by sibling subagent ... but this agent never read it."* That's
the actual near-miss worth writing about: **two agents converging on the same file path with no
lock, no coordination primitive, and only a last-write-wins safety net catching it.**

This is the sharp edge of the "swarm of parallel crewmates" model Wesley is building with
Hermes: delegate_task fans out work with isolated contexts by design (children don't see each
other), which is exactly why they can duplicate/collide on shared filesystem state. No task
graph, no file-level locking, no "is anyone else touching this repo" check.

## Verdict (both discarded, informs the real build)

Both prove the *design* works — Claude Code local JSONL parsing is real and gives real numbers,
Z.ai has a real endpoint, Ollama Cloud and ChatGPT Plus genuinely have no usage API (confirmed
independently by both agents via separate research — GitHub issues #15663/#16448 for Ollama).

Wesley's verdict on both: **"not good for sure... let's default to Rust."** Neither survives —
this directory is the archive. The real tool is being rebuilt in Rust from here.

## Blog angle

"I asked two AI agents to build the same tool from the same spec, on the same night, without
telling either the other existed" — a live case study in:
1. Convergent design (same architecture, same gaps identified, independently)
2. Divergent implementation taste (idiomatic-minimal vs over-modularized)
3. The actual collision risk of parallel autonomous agents sharing a filesystem
4. Why "let the agent choose the stack" produces exactly the thing you said you hate
   (Python) unless you say so up front — neither agent asked, both defaulted to Python because
   the design doc said Python and nobody challenged it.
