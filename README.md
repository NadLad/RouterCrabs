# iziRouter

**Mini proxy OpenAI-compatible qui route automatiquement vers le modèle le plus approprié selon la complexité du prompt.**

Un prompt simple (`« Bonjour »`) → modèle léger et économique.  
Un prompt complexe (`« Débug ce race condition dans mon code Rust »`) → modèle puissant.

Le tout est transparent pour le client (OpenCrabs, ChatGPT CLI, n'importe quel client OpenAI-compatible) — il ne voit qu'un seul endpoint.

## Architecture

```
Client (OpenCrabs) ──→ iziRouter (localhost:8001)
                           │
                     ┌─────┴──────┐
                     ▼             ▼
                 FLASH           PRO
              (modèle cheap)  (modèle puissant)
```

## Installation

```bash
git clone https://github.com/NadLad/iziRouter.git
cd iziRouter
cp .env.example .env
# Édite .env avec tes clés API et modèles
cargo build --release
./target/release/izi-router
```

Prérequis : Rust 1.80+, ~4 Mo de RAM, zéro GPU.

## Variables d'environnement

| Variable | Obligatoire | Défaut | Description |
|---|---|---|---|
| `FLASH_API_KEY` | ✅ | — | Clé API pour le tier FLASH |
| `FLASH_API_BASE` | ✅ | — | Base URL de l'API FLASH |
| `FLASH_MODEL` | ✅ | — | Nom du modèle FLASH |
| `FLASH_AUTH_HEADER` | ❌ | `Bearer` | Header d'authentification (voir section dédiée) |
| `PRO_API_KEY` | ✅ | — | Clé API pour le tier PRO |
| `PRO_API_BASE` | ✅ | — | Base URL de l'API PRO |
| `PRO_MODEL` | ✅ | — | Nom du modèle PRO |
| `PRO_AUTH_HEADER` | ❌ | `Bearer` | Header d'authentification (voir section dédiée) |
| `PORT` | ❌ | `8001` | Port d'écoute |
| `COMPLEXITY_THRESHOLD` | ❌ | `3` | Score seuil pour basculer vers PRO (1–10) |
| `RUST_LOG` | ❌ | `info,izi_router=debug` | Niveau de log (tracing) |

## Header d'authentification

Par défaut, iziRouter envoie `Authorization: Bearer <clé>`, le standard OpenAI/DeepSeek/Groq/Mistral/OpenRouter/Together.

Pour les APIs qui utilisent un autre header (ex: Anthropic utilise `x-api-key`), définis `FLASH_AUTH_HEADER` / `PRO_AUTH_HEADER` :

```env
# Anthropic natif
FLASH_AUTH_HEADER=x-api-key
PRO_AUTH_HEADER=x-api-key
```

| Comportement | Valeur |
|---|---|
| `Authorization: Bearer <key>` | `Bearer` (défaut) |
| `x-api-key: <key>` | `x-api-key` (Anthropic natif) |
| `X-Goog-Api-Key: <key>` | `X-Goog-Api-Key` (Gemini) |
| Header personnalisé | N'importe quelle chaîne |

## Seuil de complexité

`COMPLEXITY_THRESHOLD` contrôle à quel point iziRouter est *gourmand* :

| Valeur | Comportement |
|---|---|
| `1` | Presque tout part vers PRO (ultra-prudent) |
| `3` | Équilibré **(défaut)** |
| `6` | Seuls les prompts très complexes vont vers PRO |
| `10` | Presque tout reste sur FLASH (ultra-économe) |

## Règles de classification

Le classifieur score chaque prompt sur 5 règles et additionne :

| Règle | +1 | +2 | +3 | +5 |
|---|---|---|---|---|
| Longueur | >300 car. | >800 car. | >2000 car. | — |
| Code | — | ≥1 marqueur | ≥3 marqueurs | — |
| Mots-clés tech | 1 trouvé | 2–3 trouvés | ≥4 trouvés | — |
| Image | — | — | — | image détectée |
| Question ouverte | `?` + mot-clé | — | — | — |

Score ≥ seuil → PRO, sinon → FLASH.

## Exemples de configuration

### DeepSeek (Flash + Pro)

```env
FLASH_API_KEY=sk-votre-cle
FLASH_API_BASE=https://api.deepseek.com
FLASH_MODEL=deepseek-v4-flash
PRO_API_KEY=sk-votre-cle
PRO_API_BASE=https://api.deepseek.com
PRO_MODEL=deepseek-v4-pro
```

### OpenAI (GPT-4o-mini + GPT-4o)

```env
FLASH_API_KEY=sk-votre-cle
FLASH_API_BASE=https://api.openai.com
FLASH_MODEL=gpt-4o-mini
PRO_API_KEY=sk-votre-cle
PRO_API_BASE=https://api.openai.com
PRO_MODEL=gpt-4o
```

### Groq (Llama 8B + 70B)

```env
FLASH_API_KEY=gsk_votre-cle
FLASH_API_BASE=https://api.groq.com/openai
FLASH_MODEL=llama-3.1-8b-instant
PRO_API_KEY=gsk_votre-cle
PRO_API_BASE=https://api.groq.com/openai
PRO_MODEL=llama-3.3-70b-versatile
```

### Mixte : Flash chez DeepSeek, Pro chez Anthropic

```env
FLASH_API_KEY=sk-votre-cle-deepseek
FLASH_API_BASE=https://api.deepseek.com
FLASH_MODEL=deepseek-v4-flash

PRO_API_KEY=sk-ant-votre-cle
PRO_API_BASE=https://api.anthropic.com
PRO_MODEL=claude-sonnet-4-6-20251101
PRO_AUTH_HEADER=x-api-key
```

### OpenRouter (tout via un seul provider)

```env
FLASH_API_KEY=sk-or-v1-votre-cle
FLASH_API_BASE=https://openrouter.ai/api
FLASH_MODEL=deepseek/deepseek-v4-flash
PRO_API_KEY=sk-or-v1-votre-cle
PRO_API_BASE=https://openrouter.ai/api
PRO_MODEL=anthropic/claude-sonnet-4.6
```

## Utilisation avec OpenCrabs

Dans `~/.opencrabs/config.toml` :

```toml
[providers.custom.deepseek]
base_url = "http://localhost:8001/v1"
api_key = "not-needed"
default_model = "izi-router"
```

OpenCrabs parle à iziRouter, iziRouter choisit le bon modèle. Rien d'autre à configurer.

## Debug

Chaque réponse inclut deux headers :

| Header | Contenu |
|---|---|
| `X-iziRouter-Model` | Modèle choisi (ex: `deepseek-v4-flash`) |
| `X-iziRouter-Reason` | Raison (ex: `long, contient du code`) |

```bash
curl -s -D - http://localhost:8001/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"messages":[{"role":"user","content":"Bonjour"}]}' \
  | grep X-iziRouter
# → X-iziRouter-Model: deepseek-v4-flash
# → X-iziRouter-Reason: simple
```

## Licence

MIT — fais-en ce que tu veux.
