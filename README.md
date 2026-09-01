# RouterCrabs 🧭

A lightweight OpenAI-compatible proxy that automatically picks the right model based on **domain** or **complexity** of your prompt. Two routing modes, combinable:

- 🏷️ **Domain routing** — keywords → specialized model (agri → AgriLLM, code → Pro…)
- 🧠 **Complexity routing** — local heuristics → Flash (simple) or Pro (complex)
- 🔌 **Multi-provider** — each tier can point to a different provider
- ⚡ **Zero latency** — local classification, substring match + heuristics, <1ms
- 📦 **Single binary** — ~4 MB, no Docker, no database
- 🎯 **Native SSE streaming**

```mermaid
flowchart LR
    A[Prompt] --> B[RouterCrabs]
    B -->|"agriculture, soil…"| C[AgriLLM]
    B -->|"code, Rust, debug…"| D[Pro]
    B -->|"no keyword + low complexity"| E[Flash]
    B -->|"no keyword + high complexity"| F[Pro]
```

---

## Quick Start

```bash
git clone https://github.com/NadLad/RouterCrabs
cd RouterCrabs
cp tiers.yaml.example tiers.yaml
# Edit tiers.yaml → uncomment the [fallback] section + your domains
cp .env.example .env
# Edit .env → add your API keys
cargo run --release
```

Then in OpenCrabs (`~/.opencrabs/config.toml`):

```toml
[providers.custom.deepseek]
base_url = "http://localhost:8001/v1"
api_key = "not-needed"
default_model = "router-crabs"
```

---

## How It Works

### 1. Domain Routing (keywords)

Each tier defines a list of keywords. RouterCrabs scans the prompt for these keywords (substring, case-insensitive) and computes a score:

```
Score = match_count × tier_weight
```

The tier with the highest score wins. Ties are broken by `weight`, then `default: true`.

```
"Compare wheat and corn yields in organic agriculture"
  → "agriculture" matched → agri tier → AgriLLM ✅
```

### 2. Complexity Routing (fallback)

When **no domain keywords** match, RouterCrabs computes a **complexity score** — a *signed* integer that can go negative — as the sum of objective heuristics plus **per-keyword signed weights**:

| Component | Scoring |
|---|---|
| **Prompt length** | >2000 chars: +3<br>>800 chars: +2<br>>300 chars: +1 |
| **Code presence** | ≥3 markers (```, `fn`, `class`, `SELECT`…): +3<br>≥1: +2 |
| **Images** | +5 (always → Pro) |
| **Technical keywords** | +1 each (explain, architecture, algorithm, compare…) |
| **Simple keywords** | **−3 each** (salut, hello, thanks, merci…) — counterweight |
| **Question words** | 0 (neutral markers — pourquoi, why, comment…) |

If score ≥ `threshold` → **complex** model. Otherwise → **simple** model. A negative score (several simple words) reliably stays on the simple model.

```
"salut merci"          → score −6 < 3 → DeepSeek Flash ✅
"Hello"                → score −3 < 3 → DeepSeek Flash ✅
"Explain microservices → score 5 ≥ 3 → DeepSeek Pro   ✅
 architecture, compare
 performance tradeoffs"
```

#### Customizing Keywords — `keywords.yaml`

The technical keywords, **simple keywords**, question words, and code markers used for scoring are loaded from a separate YAML file. This file is **trilingual** by default (French, English, Arabic) and organized by language for easy maintenance. Each list carries a signed weight: technical `+1`, simple `−3`, question `0`.

```yaml
# keywords.yaml — v0.3+ language-based format

code_markers:
  - "```"
  - "fn "
  - "class "
  - "SELECT "
  # ...

languages:
  french:
    technical_keywords:
      - "explique"
      - "algorithme"
      - "sécurité"
      # ...
    question_words:
      - "pourquoi"
      - "comment"
      # ...

  english:
    technical_keywords:
      - "explain"
      - "algorithm"
      - "security"
      # ...
    question_words:
      - "why"
      - "how"
      # ...

  arabic:
    technical_keywords:
      - "اشرح"
      - "خوارزمية"
      - "أمان"
      # ...
    question_words:
      - "لماذا"
      - "كيف"
      # ...
```

All languages are merged into one pool at runtime — add as many as you need. If the file is missing or a section is empty, built-in defaults are used. Edit and restart the service — no recompilation needed.

##### Adding a New Language

To add Spanish, German, Italian, or any other language, copy one of the existing language blocks and translate the keywords:

```yaml
languages:
  # ... existing french, english, arabic ...

  spanish:
    technical_keywords:
      - "explica"
      - "analiza"
      - "compara"
      - "arquitectura"
      - "algoritmo"
      - "optimiza"
      - "seguridad"
      - "implementa"
      - "configura"
      - "despliega"
      - "compila"
      - "concurrente"
      - "memoria"
      - "cache"
      - "latencia"
      - "escalabilidad"
      - "contenedor"
      - "prueba unitaria"
      - "cifra"
      - "protocolo"
      # ... add more as needed
    question_words:
      - "por qué"
      - "cómo"
      - "qué es"
      - "puedes"
      - "qué"
      - "quién"
      - "dónde"
      - "cuándo"
      - "cuál"
```

That's it. Restart RouterCrabs and your new language is active — a `"¿cómo implementar un middleware?"` will now go to Pro, while `"¿cómo estás?"` stays on Flash.

> **Tip:** A commented Spanish template is already included at the bottom of `keywords.yaml`. Uncomment and customize.

### 3. Full Algorithm (hybrid)

```
1. Domain keywords → if match → specialized tier
2. Otherwise → complexity score → ≥ threshold → complex model
                                 → < threshold → simple model
3. Otherwise (no fallback section) → tier with default: true
```

---

## Configuration — `tiers.yaml`

```yaml
port: 8001
# host: "0.0.0.0"       # uncomment to bind to LAN
# keywords_path: "keywords.yaml"  # custom scoring keyword file

# ── Domain tiers (keywords) ──────────────────────────────
tiers:
  - model: "agrillm-v2"
    api_base: "https://api.agrillm.com/v1"
    api_key: "${AGRI_API_KEY}"
    keywords: [agriculture, agronomy, soil, plant, harvest, livestock]
    weight: 20

  - model: "deepseek-v4-pro"
    api_base: "https://api.deepseek.com"
    api_key: "${DEEPSEEK_API_KEY}"
    keywords: [code, Rust, Python, API, database, SQL, Docker, deployment]
    weight: 10

# ── Complexity routing (fallback) ──────────────────────
fallback:
  threshold: 3          # switch threshold: simple → complex
  simple:
    model: "deepseek-v4-flash"
    api_base: "https://api.deepseek.com"
    api_key: "${DEEPSEEK_API_KEY}"
  complex:
    model: "deepseek-v4-pro"
    api_base: "https://api.deepseek.com"
    api_key: "${DEEPSEEK_API_KEY}"
```

### Global Fields

| Field | Required | Default | Description |
|---|---|---|---|
| `port` | ❌ | `8001` | Listening port |
| `host` | ❌ | `127.0.0.1` | Bind address (`0.0.0.0` = LAN) |
| `keywords_path` | ❌ | `keywords.yaml` | Path to scoring keywords file |
| `journal_path` | ❌ | `journal.jsonl` | Path to the append-only request journal |

### Tier Fields

| Field | Required | Default | Description |
|---|---|---|---|
| `model` | ✅ | — | Model to call |
| `api_base` | ✅ | — | API base URL |
| `api_key` | ✅ | — | Key (`${VAR}` = environment variable) |
| `auth_header` | ❌ | `Bearer` | Auth header (`x-api-key` for native Anthropic) |
| `keywords` | ❌ | `[]` | Keywords (lowercase, substring match) |
| `weight` | ❌ | `1` | Priority in case of a tie |
| `default` | ❌ | `false` | Ultimate fallback if no keywords nor `fallback` |

### Fallback Section Fields

| Field | Required | Default | Description |
|---|---|---|---|
| `threshold` | ❌ | `3` | Minimum complexity score to switch to `complex` |
| `simple.model` | ✅ | — | Model for simple requests |
| `simple.api_base` | ✅ | — | Base URL |
| `simple.api_key` | ✅ | — | API key |
| `complex.model` | ✅ | — | Model for complex requests |
| `complex.api_base` | ✅ | — | Base URL |
| `complex.api_key` | ✅ | — | API key |

---

## Adding a Provider / Model

RouterCrabs is **provider-agnostic**. Nothing in the code knows the names
`deepseek`, `pro` or `flash` — routing, journaling, feedback and the RSI loop
all operate on *configured tiers* and the semantic `simple`/`complex` axis.
Adding a model, a provider, or a whole variant family is **a YAML edit only** —
no Rust, no recompile, no new slash commands.

A tier is just a block with a model identifier, an OpenAI-compatible endpoint,
a key and (optionally) routing keywords:

```yaml
- model: "llama-3.3-70b-versatile"     # any identifier you like
  api_base: "https://api.groq.com/openai"   # OpenAI-compatible base
  api_key: "${GROQ_API_KEY}"           # env var, never hardcoded
  auth_header: "Bearer"                # default; "x-api-key" for some providers
  keywords: [resume, cv, cover letter] # domain routing
  weight: 20
```

### Complete multi-provider example — `tiers.yaml`

This example shows every feature at once: two **domain tiers** (a Groq
endpoint for resume-writing, a local vLLM for code), a DeepSeek fallback pair,
a custom auth header, and the journal enabled.

```yaml
# RouterCrabs — complete multi-provider configuration
port: 8001
host: "127.0.0.1"

# Keywords scoring file + request journal (both relative to this config dir)
keywords_path: "keywords.yaml"
journal_path: "journal.jsonl"

# ── Domain tiers (keywords) ──────────────────────────────────────────
# Phase 1 of routing: keywords match → this tier wins (score × weight).
tiers:

  # Groq (Llama) — handles anything about resumes / cover letters.
  - model: "llama-3.3-70b-versatile"
    api_base: "https://api.groq.com/openai"
    api_key: "${GROQ_API_KEY}"
    auth_header: "Bearer"
    keywords: ["resume", "cv", "cover letter", "motivation letter"]
    weight: 20

  # Local vLLM server — code reviews, refactors, SQL.
  - model: "qwen2.5-coder-32b"
    api_base: "http://127.0.0.1:8000/v1"
    api_key: "${VLLM_API_KEY}"            # vLLM ignores it, but keep the key
    keywords: ["refactor", "debug", "sql", "rust", "python", "docker"]
    weight: 15

# ── Complexity routing (fallback) ────────────────────────────────────
# Phase 2: when no domain keyword matches, score the prompt complexity and
# pick simple (fast/cheap) or complex (strong) from the fallback pair.
fallback:
  threshold: 2
  simple:
    model: "deepseek-v4-flash"
    api_base: "https://api.deepseek.com"
    api_key: "${DEEPSEEK_API_KEY}"
  complex:
    model: "deepseek-v4-pro"
    api_base: "https://api.deepseek.com"
    api_key: "${DEEPSEEK_API_KEY}"
```

### Rules of thumb

| Want | Change |
|---|---|
| New model on an existing provider | Add a `tiers[]` block (keywords) **or** swap a `fallback.simple/complex` model |
| New provider (OpenAI-compatible) | Add a tier block with its `api_base` + `${VAR}` key |
| Provider with non-`Bearer` auth | Set `auth_header` (e.g. `x-api-key`) |
| Route a *domain* to a specific model | Add `keywords` + `weight` to its tier block |
| Make a tier the ultimate default | `default: true` on exactly one tier |

### What does NOT change

- **Feedback** (`POST /v1/feedback`, `/pro` `/flash`) — the value is
  classified against the configured tiers; a new provider works with **zero
  new commands**.
- **`router-crabs analyze` / `apply`** — proposals are generated per term and
  filed by language, independent of which provider produced the correction.
- **Journaling** — `routed` records the actual model name, whatever it is.

> **Compatibility note:** every tier is expected to speak the
> **OpenAI-compatible** protocol (`POST {api_base}/chat/completions`, SSE
> streaming). DeepSeek, OpenAI, Mistral, Groq, OpenRouter, Together, vLLM,
> Ollama, LM Studio and llama.cpp all do. A provider with a proprietary,
> non-OpenAI API (e.g. Anthropic's native `/v1/messages`) needs a small
> request/response adapter — a new layer, not a rewrite.

---

## Request Journal & Feedback

Every request is appended to an **append-only JSONL journal** (`journal.jsonl`,
chmod 600) — one JSON object per line. This is the raw material that makes
later learning possible (feedback → analysis → keyword updates).

**`req` line** — written on every request:
```json
{"type":"req","id":"ab3b89c6-…","ts":"2026-08-27T21:16:20Z","profile":"unknown","prompt":"explique l'architecture des microservices","prompt_truncated":false,"score":3,"matched":["explique","architecture","microservice"],"weights":[["explique",1],["architecture",1],["microservice",1]],"routed":"deepseek-v4-pro","reason":"complexity: high (score: 3, threshold: 3)"}
```

| Field | Type | Description |
|---|---|---|
| `type` | string | `"req"` or `"fb"` |
| `id` | uuid | Unique request id (only on `req`) |
| `ts` | string | ISO 8601 UTC timestamp |
| `profile` | string | Emitting profile, from the `X-RouterCrabs-Profile` header (default `unknown`) |
| `prompt` | string | Last user message, truncated to 2000 chars |
| `prompt_truncated` | bool | `true` if the prompt was truncated |
| `score` | int | Complexity score — a **signed** integer (can be negative) |
| `matched` | array | Technical keywords that matched |
| `weights` | array | Matched keywords with their signed weight (`[term, weight]` pairs) |
| `routed` | string | The **actual model** that served the request (e.g. `deepseek-v4-pro`, `qwen2.5-72b`) |
| `reason` | string | Human-readable routing reason |

**`fb` line** — written when explicit feedback arrives via
`POST /v1/feedback`. The `correct_tier` value is stored as its normalized
**semantic kind** — `"simple"` or `"complex"` — regardless of which model
name (or alias) the client sent:
```json
{"type":"fb","req_id":"ab3b89c6-…","ts":"2026-08-27T21:16:39Z","correct_tier":"complex","source":"slash"}
```

Feedback endpoint — `correction` is **model-agnostic**: send any configured
model name, or a generic alias (`pro`/`complex`/`strong` → complex tier,
`flash`/`simple`/`fast` → simple tier). Unknown values return HTTP 400:
```bash
# "The last request should have gone to the simple tier" (it went complex)
curl -s -X POST http://localhost:8001/v1/feedback \
  -H "Content-Type: application/json" \
  -d '{"correction":"flash"}'

# "The last request should have gone to the complex tier" (it went simple)
curl -s -X POST http://localhost:8001/v1/feedback \
  -H "Content-Type: application/json" \
  -d '{"correction":"deepseek-v4-pro"}'

# Full model names work too — matched against the configured tiers
curl -s -X POST http://localhost:8001/v1/feedback \
  -H "Content-Type: application/json" \
  -d '{"correction":"qwen2.5-72b"}'
```

`source` defaults to `"slash"` (explicit `/pro` and `/flash` commands) — pass
`"agent"` for agent self-assessment. The `/pro` `/flash` commands are just
shortcuts for "complex"/"simple" — adding a new provider requires **zero** new
commands.

---

## Self-Improvement Loop — `analyze` / `apply`

RouterCrabs ships a two-command loop that turns the journal + feedback into
**proposed** keyword edits — but **never applies anything automatically**.
The workflow is strictly: observe → propose → human approve → apply.

### `router-crabs analyze`

Scans the journal, joins each `fb` line to its originating `req`, aggregates
feedback per term, and writes **edit proposals** as YAML files in
`<config-dir>/proposals/`. It only *reads* the config — it never modifies it.
The config is resolved from `$HOME/.config/routercrabs/tiers.yaml` when
`TIERS_CONFIG` is unset, so the CLI works from any working directory.

```bash
router-crabs analyze
# Propositions : 0
# Répertoire : ~/.config/routercrabs/proposals/
```

A proposal looks like:

```yaml
id: 20260827T213000Z-a1b2c3d4
created_at: 2026-08-27T21:30:00Z
type: add            # add | adjust | remove
target: technical_keywords   # technical_keywords | simple_keywords | question_words
language: french     # french | english | arabic
term: refactoriser
weight: 1
reason: "4 corrections vers flash sur 6 requêtes"
evidence:
  req_ids: ["…", "…", "…"]
  correct_tier_ratio: 0.67
```

Proposals are only generated once a term crosses a threshold (by default
5 promotions for `add`, 3 corrections for `adjust`/`remove`). With a small
journal the output is honestly **0 propositions** — the thresholds are a
deliberate anti-runaway guard.

### `router-crabs apply <proposal.yaml>`

Applies an **already-approved** proposal to `keywords.yaml`, with guards:

1. Backs up `keywords.yaml` to a timestamped `.bak` file (`cp -a`).
2. Applies the edit (add / adjust weight / remove term).
3. Re-validates the resulting YAML — on parse failure it **aborts** and
   leaves the backup untouched.
4. Restarts the service via `systemctl --user restart routercrabs.service`.

```bash
router-crabs apply ~/.config/routercrabs/proposals/20260827T213000Z-a1b2c3d4.yaml
```

Because `apply` edits a real config file, only run it on a proposal a human
has explicitly approved.

---

## Debug

Every response includes headers to trace routing:

```
X-RouterCrabs-Tier:   complex-fallback
X-RouterCrabs-Model:  deepseek-v4-pro
X-RouterCrabs-Reason: complexity: high (score: 5, threshold: 3)
```

To see detailed scores:

```bash
RUST_LOG=debug cargo run --release
```

---

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `TIERS_CONFIG` | `~/.config/routercrabs/tiers.yaml` | Path to the YAML config (when unset, the CLI and the server fall back to the real config location — `tiers.yaml` relative only if `HOME` is empty) |
| `PORT` | `8001` | Listening port |
| `RUST_LOG` | `info,router_crabs=debug` | Log level |
| `*_API_KEY` | — | API keys (referenced in `tiers.yaml` via `${VAR}`) |

---

## Usage as a Rust Library

```rust
use router_crabs::{TiersConfig, Message, ScoringKeywords, select_tier, score_complexity, forward_request};

let config = TiersConfig::load("tiers.yaml")?;
let (tier, reason) = select_tier(&config, &messages);
let complexity = score_complexity(&messages, &config.keywords);
```

---

## License

MIT
