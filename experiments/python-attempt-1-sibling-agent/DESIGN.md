# AI Subscription Usage Optimizer — Design Document

**Status:** Design (not implemented)
**Owner:** Wesley Matos
**Date:** 2026-08-30
**Goals:** Self-hosted, local-first, zero-cloud, OSS. Track usage across multiple AI providers, alert at 90%/95% thresholds, recommend model switches before limits are hit.

---

## 1. Problem

Wesley uses five AI subscriptions/providers — **Claude (Pro/Max via Claude Code)**, **OpenAI**, **GLM via Z.ai**, **DeepSeek**, and **Ollama (local)** — and tracks usage manually via bookmarks. Limits and billing models differ per provider: rolling 5-hour windows, weekly quotas, prepaid token packages, cash balances, and unlimited-but-local. On 2026-08-30, GLM-5.3 Flash on Z.ai returned a 429 mid-session because the Coding Plan quota was exhausted with no warning. The goal is a local daemon that polls each provider's usage signal, computes percentage-of-limit, fires alerts at 90% and 95%, and recommends switching to a provider with headroom.

---

## 2. Provider Research — Usage APIs Exposed

Each provider was researched for what usage/billing data is programmatically accessible. Summary table first, then per-provider detail.

### 2.1 Summary Matrix

| Provider | Usage Signal | Access Method | Auth | Freshness | Limit Type | Key Gap |
|---|---|---|---|---|---|---|
| **Claude Code (Pro/Max)** | Plan usage % (5h window + weekly), per-session tokens | Local JSONL parse (`~/.claude/projects/`); `/usage` is in-CLI only | None (local files) | Real-time (local) | Rolling 5h + weekly | No HTTP API for subscription %; must parse local files or screen-scrape |
| **Claude (API / Console)** | Token usage + cost, per model/workspace | Admin API `GET /v1/organizations/usage_report/messages` | `sk-ant-admin01-` key | ~delayed (hours) | Spend $ | Requires org/admin key; not for Pro/Max subs |
| **OpenAI** | Token usage + cost, per model/project/user | `GET /v1/organization/usage/completions` + `GET /v1/organization/costs` | Admin API key (`Authorization: Bearer`) | Near real-time | Spend $ / rate tier | Requires admin key; per-project filtering available |
| **Z.ai (GLM Coding Plan)** | Token usage % (5h), MCP usage % (monthly), per-model 24h tokens | `GET /api/monitor/usage/quota/limit`, `GET /api/monitor/usage/model-usage` | API key (no Bearer prefix) | Real-time | Rolling 5h + monthly | **Standard API (non-Coding-Plan) has NO billing API** — only Coding Plan |
| **DeepSeek** | Cash balance (total, granted, topped-up) | `GET /user/balance` | `Bearer` API key | Near real-time | Cash balance | **No spend/quota API** — balance only; no period-to-date cost |
| **Ollama** | Per-response tokens (`prompt_eval_count`, `eval_count`) | `/api/generate`, `/api/chat` response fields; `/api/ps` for loaded models | None (local) | Real-time | Unlimited (local) | **No aggregated stats endpoint** — must intercept/log per-call |

### 2.2 Claude Code (Pro/Max subscription) — Detail

**What's available:**
- `/usage` slash command (in-CLI, interactive only): shows plan usage bars (5-hour window %, weekly limit %), activity stats, per-skill/subagent/plugin attribution. **Not an HTTP API** — it's a TUI render.
- Local session JSONL files at `~/.claude/projects/<encoded-path>/<session-id>.jsonl`. Each line with `message.usage` contains `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`.
- `~/.claude/.credentials.json` has subscription type and rate-limit tier. `~/.claude.json` has extra usage status.
- Cost is computed locally by Claude Code from token counts × list prices (not authoritative billing).

**What's NOT available:**
- No public HTTP API to fetch "current 5h window usage %" for a Pro/Max subscription. The `/usage` command fetches from an internal endpoint that is rate-limited and not documented.
- Usage from other devices / claude.ai is not included in local files.

**How the tool will get data:**
- **Primary:** Parse `~/.claude/projects/**/*.jsonl` locally (same approach as `ccusage`). Aggregate tokens per 5-hour window and per rolling 7-day week. Compare against plan limits from config.
- **Secondary (optional):** Read `~/.claude/.credentials.json` for plan tier to auto-detect limits.
- **API users (if Wesley ever uses API key):** Anthropic Admin API `GET /v1/organizations/usage_report/messages` with `sk-ant-admin01-` key, `anthropic-version: 2023-06-01`. Supports `bucket_width=1m|1h|1d`, `group_by[]=model`, `workspace_ids[]`, `service_tier`. Cost endpoint: `GET /v1/organizations/cost_report`.

**Reference endpoints:**
```
# Admin API (API/org users only)
GET https://api.anthropic.com/v1/organizations/usage_report/messages
    ?starting_at=2026-08-30T00:00:00Z&ending_at=2026-08-30T05:00:00Z
    &bucket_width=1h&group_by[]=model
Headers: x-api-key: sk-ant-admin01-..., anthropic-version: 2023-06-01

GET https://api.anthropic.com/v1/organizations/cost_report
    ?starting_at=...&ending_at=...&bucket_width=1d
```

### 2.3 OpenAI — Detail

**What's available:**
- Completions Usage API: `GET https://api.openai.com/v1/organization/usage/completions`
  - Params: `start_time` (Unix seconds, required), `end_time` (optional), `bucket_width` (`1m`/`1h`/`1d`), `group_by` (`model`, `project_id`, `user_id`, `api_key_id`), `models[]`, `project_ids[]`, `limit`, `page` (cursor).
  - Returns: `input_tokens`, `output_tokens`, `input_cached_tokens`, `input_audio_tokens`, `output_audio_tokens`, `num_model_requests`, per group.
- Costs API: `GET https://api.openai.com/v1/organization/costs`
  - Params: `start_time`, `end_time`, `bucket_width` (only `1d`), `group_by` (`project_id`, `line_item`), `limit` (1-180, default 7), `page`.
  - Returns: `amount.value` + `amount.currency`, `line_item`, `project_id`.
- Auth: `Authorization: Bearer $OPENAI_ADMIN_KEY` (admin key, not project key). Requires "Usage Dashboard" permission or org owner.
- Additional endpoints exist for `audio_speeches`, `audio_transcriptions`, `embeddings`, `images`, `code_interpreter_sessions`, `vector_stores`.

**What's NOT available:**
- No "subscription limit" or "ChatGPT Plus/Pro quota %" — this is API spend only. ChatGPT consumer subscriptions have no usage API.

**How the tool will get data:**
- Poll `/organization/costs` daily for spend tracking. Poll `/organization/usage/completions` with `bucket_width=1h` for token velocity. Compare against configured monthly budget.

**Reference:**
```bash
curl "https://api.openai.com/v1/organization/usage/completions?start_time=$(date -u +%s)&bucket_width=1h&group_by[]=model&limit=24" \
  -H "Authorization: Bearer $OPENAI_ADMIN_KEY"
curl "https://api.openai.com/v1/organization/costs?start_time=$(date -u +%s)&bucket_width=1d&limit=30" \
  -H "Authorization: Bearer $OPENAI_ADMIN_KEY"
```

### 2.4 Z.ai / GLM (Coding Plan) — Detail

**What's available (Coding Plan only):**
- Quota limits: `GET /api/monitor/usage/quota/limit`
  - Returns: `tokenUsage5Hour` (0-100%), `mcpUsage1Month` (0-100%).
- Model usage (24h): `GET /api/monitor/usage/model-usage`
  - Returns: `totalTokens`, `totalCalls`, per-model breakdown (`modelName`, `tokens`, `calls`).
- Tool/MCP usage (24h): `GET /api/monitor/usage/tool-usage`
- Base URLs: `https://api.z.ai/api/coding/paas/v4` (international) or `https://open.bigmodel.cn/api/coding/paas/v4` (China).
- Auth: `Authorization: <api-key>` (NO "Bearer" prefix). Headers: `Accept-Language: en-US,en;q=0.9`.
- Rate limit on monitoring endpoints: 60 req/min.
- Data retention: token usage = 5h rolling, MCP = 1 month, model/tool = 24h.

**What's NOT available:**
- **Standard API (non-Coding-Plan) has NO billing/quota API.** GitHub issue #71 (zai-org/z-ai-sdk-python) is an open feature request for standard API balance, resource packages, and per-model usage. The `/api/monitor/usage/quota/limit` endpoint returns an error for standard API keys — it only works for Coding Plan.
- Resource package balance/expiry is not exposed.
- Deduction source (cash vs. free quota vs. voucher vs. resource package) is not exposed.

**How the tool will get data:**
- Poll `quota/limit` every 2-5 min for the 5h and monthly percentages. This is the direct signal for the 90%/95% alert. Poll `model-usage` hourly for trend data.
- The community tool `zai-quota` (SeeYangZhi/zai-quota) validates this approach — it calls the same endpoint and parses `percentage`, `remaining`, `nextResetTime`, per-model `usageDetails`.

**Reference:**
```bash
curl -sS \
  -H "Authorization: $ZAI_API_KEY" \
  -H "Accept-Language: en-US,en;q=0.9" \
  -H "Content-Type: application/json" \
  "https://api.z.ai/api/coding/paas/v4/api/monitor/usage/quota/limit" | jq '.'
```

**The 429 that triggered this project:** Z.ai returns HTTP 429 with business code `1302` for rate/quota limits. The tool should detect 429s from any provider and correlate with the quota polling data.

### 2.5 DeepSeek — Detail

**What's available:**
- Balance API: `GET https://api.deepseek.com/user/balance` (also works at `https://api.deepseek.com/v1/user/balance`)
  - Auth: `Authorization: Bearer $DEEPSEEK_API_KEY`
  - Returns: `is_available` (boolean — whether balance is sufficient), `balance_infos[]` with `total_balance`, `granted_balance`, `topped_up_balance`, `currency` (CNY by default).
- Rate-limit headers from `GET /v1/models`: RPM and TPM limits in response headers.
- Per-response `usage` field in `/chat/completions`: `prompt_tokens`, `completion_tokens`, `total_tokens`, `prompt_cache_hit_tokens`, `prompt_cache_miss_tokens`.

**What's NOT available:**
- **No spend/quota API.** The balance endpoint shows cash balance only — no period-to-date spend, no usage quota %, no rate window remaining.
- Usage/cost endpoints at `platform.deepseek.com/api/v0/usage/amount` and `/usage/cost` exist on the web console but only authenticate via session cookies, not Bearer API keys. With Bearer auth they return `{"code": 40003, "msg": "Authorization Failed"}` (a fake-200 application error).
- Granted credit expiry dates are not exposed.

**How the tool will get data:**
- Poll `/user/balance` every 2-5 min. Track `total_balance` delta over time to infer spend rate. Alert when balance drops below configured thresholds (e.g., ¥10, ¥5).
- Optionally: intercept per-call `usage` from a local proxy to accumulate token counts (see §2.6 approach — same pattern as Ollama).

### 2.6 Ollama — Detail

**What's available:**
- Per-response usage in `/api/generate` and `/api/chat` responses:
  - `prompt_eval_count` (input tokens), `eval_count` (output tokens), `total_duration`, `load_duration`, `prompt_eval_duration`, `eval_duration` (all nanoseconds).
  - For streaming: usage fields in the final chunk where `done: true`.
- `/api/ps`: lists currently loaded models with name, size, VRAM usage, processor. Not usage stats.
- `/api/tags`: lists all available models.
- No auth (local).

**What's NOT available:**
- **No aggregated stats endpoint.** GitHub issue #11118 requests `ollama stats` / `GET /api/stats` — not implemented. There is no server-side cumulative token counter.
- No billing (it's local/free), so no limit to track — but token throughput is useful for cost comparison and capacity planning.

**How the tool will get data:**
- **Proxy/interceptor pattern:** Run a thin local reverse proxy (e.g., on port 11435) that fronts Ollama's 11434. Every `/api/generate` and `/api/chat` response passes through; the proxy extracts `prompt_eval_count` + `eval_count` and logs to SQLite. Existing clients point at the proxy URL instead of 11434.
- **Alternative (no proxy):** Parse Ollama server logs if verbose logging is enabled, or use a wrapper script. The proxy approach is cleaner and captures 100% of calls.
- Since Ollama is "unlimited," its usage is tracked for **cost-equivalent comparison** (what would this have cost on a paid provider?) and **throughput monitoring**, not for limit alerts.

---

## 3. Architecture

### 3.1 Design Principles

1. **Local-first, zero-cloud.** All data stays on Wesley's machine. No telemetry, no SaaS dashboard. SQLite + local files.
2. **Zero-API-key for local providers.** Claude Code (local JSONL) and Ollama (local proxy) require no keys. Remote providers (OpenAI, Z.ai, DeepSeek) need keys stored in a local env file or OS keychain — never transmitted anywhere except the provider's own API.
3. **Pluggable providers.** Each provider is a "collector" module implementing a common interface. Adding a new provider = adding one module.
4. **Poll-based, not push.** A local daemon polls each provider at its own interval. No webhooks to expose, no inbound ports.
5. **OSS, single binary or Python package.** Preference for Python (matches Hermes ecosystem) with optional systemd/launchd integration.

### 3.2 Components

```
┌─────────────────────────────────────────────────────────┐
│                    ai-usage-optimizer                    │
│                                                          │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐ │
│  │ Collector │  │ Collector │  │ Collector │  │ Collector │ │
│  │  Claude   │  │  OpenAI   │  │   Z.ai    │  │ DeepSeek  │ │
│  │ (JSONL)   │  │  (Admin   │  │ (Monitor  │  │ (Balance  │ │
│  │           │  │   API)    │  │   API)    │  │   API)    │ │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘ │
│       │              │              │              │      │
│  ┌────┴──────────────┴──────────────┴──────────────┴────┐ │
│  │              Collector Registry / Scheduler           │ │
│  │  (per-provider poll interval, retry, backoff)         │ │
│  └───────────────────────┬──────────────────────────────┘ │
│                          │                                │
│  ┌───────────────────────▼──────────────────────────────┐ │
│  │                  Storage (SQLite)                      │ │
│  │  snapshots, tokens, costs, alerts, config             │ │
│  └───────────────────────┬──────────────────────────────┘ │
│                          │                                │
│  ┌──────────┐  ┌─────────┴────────┐  ┌──────────────────┐ │
│  │  Alert   │  │  Threshold Engine │  │  Recommendation  │ │
│  │  Engine  │  │  (90% / 95%)      │  │  Engine          │ │
│  └────┬─────┘  └──────────────────┘  └────────┬─────────┘ │
│       │                                        │          │
│  ┌────▼────┐                          ┌────────▼─────────┐ │
│  │ Notifier│                          │  CLI / TUI /     │ │
│  │ (notif) │                          │  Status Line     │ │
│  └─────────┘                          └──────────────────┘ │
│                                                          │
│  ┌──────────────────────────────────────────────────────┐ │
│  │  Ollama Proxy (optional, port 11435 → 11434)         │ │
│  │  Logs token counts from /api/generate, /api/chat     │ │
│  └──────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────┘
```

### 3.3 Collector Interface

Every collector implements:

```python
class Collector(Protocol):
    provider_id: str          # "claude", "openai", "zai", "deepseek", "ollama"
    poll_interval_s: int      # provider-specific (30s–300s)

    def collect(self) -> UsageSnapshot:
        """Poll the provider and return a normalized snapshot."""
        ...

    def health_check(self) -> bool:
        """Verify auth/config is valid. Called on startup."""
        ...
```

```python
@dataclass
class UsageSnapshot:
    provider_id: str
    timestamp: datetime
    # Normalized fields (all optional — not every provider has every signal)
    limit_percent: float | None       # 0-100, how close to limit
    limit_type: str | None            # "5h_window", "weekly", "monthly", "balance", "spend_budget"
    resets_at: datetime | None        # when the current window resets
    tokens_in: int | None
    tokens_out: int | None
    cost_usd: float | None
    balance: float | None
    balance_currency: str | None
    models: list[ModelUsage]          # per-model breakdown if available
    raw: dict                         # original provider response for debugging
```

### 3.4 Threshold & Alert Engine

**Configuration (YAML):**
```yaml
providers:
  claude:
    enabled: true
    plan: max                   # pro | max — determines 5h/weekly limits
    poll_interval_s: 60         # parse JSONL every minute
    limits:
      five_hour_window: 100000  # tokens (configurable per plan tier)
      weekly: 700000
  openai:
    enabled: true
    poll_interval_s: 300
    admin_key_env: OPENAI_ADMIN_KEY
    limits:
      monthly_budget_usd: 50.00
  zai:
    enabled: true
    poll_interval_s: 120        # quota endpoint: 60 req/min limit, poll every 2 min
    api_key_env: ZAI_API_KEY
    endpoint: intl              # intl (api.z.ai) | cn (open.bigmodel.cn)
    limits:
      five_hour_percent: 100    # provider returns % directly
      monthly_mcp_percent: 100
  deepseek:
    enabled: true
    poll_interval_s: 120
    api_key_env: DEEPSEEK_API_KEY
    limits:
      balance_warning_cny: 10.0
      balance_critical_cny: 5.0
  ollama:
    enabled: true
    poll_interval_s: 30
    proxy_port: 11435
    upstream: http://localhost:11434
    # no limits — unlimited local

alerts:
  thresholds:
    warning: 90    # percent of any limit
    critical: 95   # percent of any limit
  cooldown_min: 15  # don't re-fire the same alert for 15 min
  notifications:
    - type: macos_notification   # osascript
    - type: terminal-notifier
    # - type: webhook             # optional, for Discord/Slack (future)
    # - type: hermes_memory       # write alert to Hermes memory for agent visibility

recommendations:
  enabled: true
  strategy: headroom             # headroom | cost | performance
  fallback_chain:                # ordered preference for switching
    - ollama                     # always available, free
    - deepseek                   # cheap, when balance is healthy
    - zai                        # when 5h window has headroom
    - openai                     # when budget remains
    - claude                     # last resort (most expensive overage)
```

**Alert logic:**
1. After each `collect()`, the threshold engine checks `limit_percent` (or computed % for balance/budget types) against configured thresholds.
2. If ≥90% and no alert fired in cooldown window → fire **warning** notification: `"[ai-usage] Z.ai 5h window at 92% — resets in 47 min. Consider switching to DeepSeek."`
3. If ≥95% → fire **critical** notification: `"[ai-usage] Z.ai 5h window at 96% — CRITICAL. Switch to Ollama/deepseek now."`
4. Alerts are deduplicated by `(provider_id, limit_type)` within cooldown.
5. Alert events are logged to SQLite for history.

### 3.5 Recommendation Engine

When any provider crosses 90%, the recommendation engine evaluates all other providers and suggests the best switch target:

**Scoring (per candidate provider):**
- **Headroom score (40%):** How far from limit? `100 - limit_percent`. Higher = better.
- **Cost score (30%):** Cost per 1M tokens (from a maintained pricing table). Lower = better. Ollama = 0 (free).
- **Capability match (20%):** Does the model support the current task type? (coding, reasoning, chat). Simple tag matching.
- **Latency score (10%):** Historical response time from the storage DB. Lower = better.

**Output example:**
```
⚠️  Z.ai 5h window at 92% (resets in 47 min)

Recommended switch: DeepSeek (deepseek-chat)
  Headroom: 100% (balance: ¥87.50)
  Cost: $0.27/1M tokens (vs GLM-5.3 Flash $0.15/1M)
  Capability: coding ✓, reasoning ✓

Fallback: Ollama (qwen3-coder:32b)
  Headroom: ∞ (local, unlimited)
  Cost: $0.00
  Capability: coding ✓, reasoning △ (slower)
```

### 3.6 Storage Schema (SQLite)

```sql
CREATE TABLE snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    provider_id TEXT NOT NULL,
    timestamp DATETIME NOT NULL,
    limit_percent REAL,
    limit_type TEXT,
    resets_at DATETIME,
    tokens_in INTEGER,
    tokens_out INTEGER,
    cost_usd REAL,
    balance REAL,
    balance_currency TEXT,
    raw_json TEXT,
    UNIQUE(provider_id, timestamp)
);

CREATE TABLE model_usage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    snapshot_id INTEGER REFERENCES snapshots(id),
    model_name TEXT NOT NULL,
    tokens_in INTEGER,
    tokens_out INTEGER,
    calls INTEGER,
    cost_usd REAL
);

CREATE TABLE alerts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    provider_id TEXT NOT NULL,
    limit_type TEXT NOT NULL,
    level TEXT NOT NULL,           -- 'warning' | 'critical'
    percent REAL NOT NULL,
    message TEXT NOT NULL,
    fired_at DATETIME NOT NULL,
    acknowledged BOOLEAN DEFAULT 0
);

CREATE TABLE recommendations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    triggered_by_alert INTEGER REFERENCES alerts(id),
    from_provider TEXT NOT NULL,
    to_provider TEXT NOT NULL,
    to_model TEXT,
    score REAL,
    rationale TEXT,
    created_at DATETIME NOT NULL
);

CREATE TABLE config_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    changed_at DATETIME NOT NULL,
    config_yaml TEXT NOT NULL
);
```

Retention: snapshots purged after 90 days, alerts/recommendations kept indefinitely (small).

---

## 4. Per-Provider Collector Designs

### 4.1 Claude Collector

```
Source: ~/.claude/projects/**/*.jsonl (top-level only, skip subagents/)
Parse: Each line → if message.usage exists, extract:
  - input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens
  - timestamp from message.timestamp or file line
  - model from message.model
Aggregate:
  - 5h rolling window: sum tokens where timestamp > now() - 5h
  - 7d rolling window: sum tokens where timestamp > now() - 7d
  - Apply cache pricing: cache_read = 0.1× input, cache_write = 1.25× input
Compare against plan limits (from config or ~/.claude/.credentials.json):
  - Max 5h: ~225K effective tokens (configurable — actual limit varies by tier/load)
  - Max weekly: ~700K effective tokens (configurable)
Limit %: max(five_hour_pct, weekly_pct)
```

**Note:** Claude Code's 5h/weekly limits are not published as fixed token counts — they're dynamic and depend on model mix, context size, and server-side load. The config file lets Wesley calibrate from observed `/usage` bars. The tool can also parse the `/usage` TUI output if Claude Code is run in a controlled terminal (fragile, not recommended).

### 4.2 OpenAI Collector

```
Source: GET /v1/organization/costs (daily buckets) + GET /v1/organization/usage/completions (hourly)
Auth: Bearer $OPENAI_ADMIN_KEY
Compute:
  - Monthly spend = sum(costs.amount.value) for current billing period
  - Limit % = monthly_spend / monthly_budget_usd × 100
  - Token velocity from completions API for trend prediction
Poll: costs every 5 min (1d buckets, last 30 days), completions every 5 min (1h buckets, last 24h)
```

### 4.3 Z.ai Collector

```
Source: GET /api/monitor/usage/quota/limit + GET /api/monitor/usage/model-usage
Auth: Authorization: <key> (NO Bearer), Accept-Language: en-US
Base: https://api.z.ai/api/coding/paas/v4 (intl) or https://open.bigmodel.cn/api/coding/paas/v4 (cn)
Compute:
  - limit_percent = max(tokenUsage5Hour, mcpUsage1Month)  ← provider returns % directly!
  - This is the most direct signal of all providers.
Poll: every 2 min (well under 60 req/min rate limit)
Rate limit handling: if 429 with code 1302, back off to 5 min and flag as "rate-limited"
```

**This is the provider that triggered the project.** The quota endpoint returns exact percentages — the tool just needs to poll it and fire at 90/95.

### 4.4 DeepSeek Collector

```
Source: GET /user/balance
Auth: Bearer $DEEPSEEK_API_KEY
Compute:
  - balance = balance_infos[0].total_balance (float)
  - is_available = response.is_available
  - Limit % = (1 - balance / balance_warning_threshold) × 100  (inverted: low balance = high %)
  - Or absolute: alert when balance < warning_cny (¥10) and critical_cny (¥5)
  - Spend rate = (previous_balance - current_balance) / time_delta → projected depletion time
Poll: every 2 min
Gap: No usage/cost breakdown. Balance delta is the only spend signal.
```

### 4.5 Ollama Collector (Proxy)

```
Component: Local reverse proxy on port 11435 → upstream localhost:11434
Intercept: /api/generate, /api/chat responses
Extract from response JSON:
  - prompt_eval_count (input tokens)
  - eval_count (output tokens)
  - model
  - total_duration, eval_duration
Log to SQLite, pass response through to client unmodified.
Compute:
  - No limit (unlimited local). Track for cost-equivalent: what would these tokens cost on each paid provider?
  - Throughput: tokens/sec from eval_count / (eval_duration / 1e9)
Poll: proxy logs in real-time, no polling needed. Status endpoint on the proxy itself.
```

**Client configuration:** Change `OLLAMA_HOST` from `localhost:11434` to `localhost:11435` (or set in Hermes config). Transparent passthrough.

---

## 5. CLI & TUI Design

### 5.1 Commands

```bash
ai-usage status              # One-shot: print current usage across all providers
ai-usage status --json       # Machine-readable output
ai-usage daemon              # Start the polling daemon (foreground)
ai-usage daemon --background # Start as background process (launchd/systemd)
ai-usage alerts              # Show recent alerts
ai-usage history --provider zai --days 7  # Historical usage chart
ai-usage recommend           # Force a recommendation check now
ai-usage config edit         # Open config in $EDITOR
ai-usage config validate     # Validate config.yaml
ai-usage provider list       # List configured providers + health
ai-usage provider add        # Interactive provider setup wizard
```

### 5.2 Status Output (TUI)

```
╔══════════════════════════════════════════════════════════════╗
║  AI Usage Optimizer — 2026-08-30 23:45:01 -03               ║
╠══════════════════════════════════════════════════════════════╣
║  PROVIDER    LIMIT TYPE      USED    RESETS             STATUS║
║  ────────    ──────────      ────    ───────             ─────║
║  Claude      5h window       34%     in 2h 15m          ●   ║
║  Claude      weekly          12%     in 4d               ●   ║
║  OpenAI      monthly $       23%     Sep 1               ●   ║
║  Z.ai        5h window       92% ⚠   in 47m              ⚠   ║
║  Z.ai        monthly MCP     18%     Sep 1               ●   ║
║  DeepSeek    balance ¥       14% ↓   n/a                 ●   ║
║  Ollama      (unlimited)     —       —                   ●   ║
╠══════════════════════════════════════════════════════════════╣
║  ⚠  Z.ai 5h window at 92% — switch to DeepSeek recommended  ║
║  → ai-usage recommend                                       ║
╚══════════════════════════════════════════════════════════════╝
```

### 5.3 Status Line Integration

For Claude Code / Hermes Agent status line:
```
[C:34%] [Z:92%⚠] [D:¥87] [O:∞]
```
Configured via Claude Code's `statusLine` hook or Hermes status line config.

---

## 6. Notifications

### 6.1 macOS Notification (default)

```bash
osascript -e 'display notification "Z.ai 5h window at 92%. Switch to DeepSeek." with title "AI Usage Optimizer" subtitle "Warning: 90% threshold"'
```

### 6.2 terminal-notifier (fallback)

```bash
terminal-notifier -title "AI Usage Optimizer" -subtitle "Critical: 95%" -message "Z.ai 5h window at 96%. Switch now." -sound "Basso"
```

### 6.3 Hermes Memory (optional)

Write alert to Hermes memory so it appears in agent context:
```
[ALERT] Z.ai 5h window at 92% — recommend switching to DeepSeek (balance ¥87.50).
```
This lets Hermes Agent proactively suggest switches during coding sessions.

---

## 7. Technology Choices

| Component | Choice | Rationale |
|---|---|---|
| Language | Python 3.11+ | Matches Hermes ecosystem; stdlib sqlite3, json, urllib; easy to extend |
| Config | YAML (`~/.config/ai-usage/config.yaml`) | Human-readable, familiar |
| Storage | SQLite (`~/.local/share/ai-usage/usage.db`) | Zero-config, local, fast, durable |
| Daemon | Python + launchd (macOS) / systemd (Linux) | No extra runtime; native OS integration |
| HTTP | `httpx` (async) or stdlib `urllib` | Minimal deps; stdlib preferred for zero-API-key simplicity |
| TUI | `rich` / `textual` | Already in Hermes stack; nice tables/status bars |
| Notifications | `osascript` (macOS built-in) | Zero deps; native |
| Ollama proxy | `aiohttp` reverse proxy or `mitmproxy` addon | Lightweight; transparent passthrough |

**Dependency budget:** Target <5 non-stdlib packages (httpx, rich, pyyaml, click). Everything else stdlib.

---

## 8. Deployment

### 8.1 macOS (Wesley's setup)

```bash
# Install
pip install ai-usage-optimizer   # or: uvx ai-usage-optimizer

# Configure
ai-usage config edit             # opens ~/.config/ai-usage/config.yaml

# Set API keys (in shell profile or keychain)
export ZAI_API_KEY="..."
export OPENAI_ADMIN_KEY="..."
export DEEPSEEK_API_KEY="..."

# Run as launchd agent
ai-usage daemon --background      # installs ~/Library/LaunchAgents/com.wesley.ai-usage.plist

# Or foreground for debugging
ai-usage daemon
```

### 8.2 LaunchAgent plist (generated)

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.wesley.ai-usage</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/ai-usage</string>
    <string>daemon</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>/tmp/ai-usage.log</string>
  <key>StandardErrorPath</key>
  <string>/tmp/ai-usage.error.log</string>
</dict>
</plist>
```

---

## 9. Limitations & Risks

| Risk | Impact | Mitigation |
|---|---|---|
| Z.ai monitor endpoint is unofficial (GitHub issue #71) | Could break without notice | Defensive parsing; fall back to per-response token counting; monitor the upstream issue |
| Claude Code 5h/weekly limits are not fixed token counts | Limit % may be inaccurate | Configurable calibration; parse `/usage` output as ground truth when available; conservative default thresholds |
| DeepSeek has no spend API, only balance | Can't track spend rate precisely | Balance-delta inference; intercept per-call usage if precision needed |
| Ollama has no stats endpoint | Must run proxy | Proxy is optional; if not running, Ollama shows as "untracked" with no impact on other providers |
| OpenAI admin key required (not project key) | May not have access | Health check on startup; graceful disable if key invalid |
| Claude Code local JSONL doesn't include other devices | Under-counts usage | Document clearly; supplement with Anthropic Admin API if API key user |
| Polling intervals could hit rate limits | 429 from monitoring APIs | Per-collector backoff; Z.ai monitoring is 60 req/min (poll at 2 min = 0.5 req/min, safe) |

---

## 10. Future Enhancements (Out of Scope for V1)

- **Webhook notifications** (Discord, Slack, ntfy.sh) for remote alerts.
- **Automatic model switching** — integrate with Hermes Agent routing to auto-redirect to recommended provider when threshold crossed.
- **Per-project usage attribution** — track which project/task consumed which tokens.
- **Cost optimization suggestions** — "You spent $12 on Claude this week; the same work on DeepSeek would have cost $3.40."
- **Budget forecasting** — project monthly spend from current velocity.
- **Provider reliability tracking** — track 429s, 500s, latency per provider for switch decisions.
- **OpenTelemetry export** — for integration with existing observability stacks.
- **Multi-machine sync** — sync usage data across machines via Syncthing (no cloud).

---

## 11. Research References

| Provider | Source | URL |
|---|---|---|
| Claude Code /usage | Anthropic docs | https://docs.anthropic.com/en/docs/claude-code/costs |
| Claude Code local JSONL | GitHub issue #33978 | https://github.com/anthropics/claude-code/issues/33978 |
| Claude Code JSONL structure | Milvus blog | https://milvus.io/blog/why-claude-code-feels-so-stable-... |
| ccusage tool | GitHub | https://github.com/ccusage/ccusage |
| Anthropic Admin API | Anthropic docs | https://docs.anthropic.com/en/api/data-usage-cost-api |
| OpenAI Usage API | OpenAI cookbook | https://developers.openai.com/cookbook/examples/completions_usage_api |
| OpenAI Costs API | OpenAI docs | https://help.openai.com/en/articles/10478918-api-usage-dashboard |
| Z.ai Coding Plan monitor | OpenClaw skills | https://github.com/openclaw/skills/blob/main/skills/.../api-endpoints.md |
| Z.ai quota CLI (community) | GitHub | https://github.com/SeeYangZhi/zai-quota |
| Z.ai standard API billing gap | GitHub issue #71 | https://github.com/zai-org/z-ai-sdk-python/issues/71 |
| DeepSeek balance API | DeepSeek docs | https://api-docs.deepseek.com/api/get-user-balance/ |
| DeepSeek usage API gap | GitHub issue #654 | https://github.com/deepseek-ai/awesome-deepseek-integration/issues/654 |
| Ollama usage fields | Ollama docs | https://docs.ollama.com/api/usage |
| Ollama stats feature request | GitHub issue #11118 | https://github.com/ollama/ollama/issues/11118 |
| Ollama cloud usage scraper | sourcehut | https://git.sr.ht/~hrbrmstr/ollama-usage |