# Repository Guidelines

## Project Overview

Kindle Recipes is a deliberately small recipe library for Kindle browsers and ordinary desktop browsers. It is a single Rust/Axum process: Askama renders every page server-side, SQLite holds all data, and an OpenAI Codex (Pi) Node worker generates recipe drafts through the user's ChatGPT subscription. There is deliberately **no login** — deploy only on a trusted LAN or behind access control.

## Architecture & Data Flow

Single-process Axum 0.8 app (crate `kindle-recipes`, edition 2024, binary-only — no `lib.rs`):

- `src/main.rs` — the hub: `#[tokio::main]` bootstrap, env config, `routes()` router, `AppState`, `AppError`, shared serde types, Askama template structs, form/validation helpers, Codex device-code auth handlers, `run_node_script`.
- `src/ai.rs` — Pi/Codex generation pipeline: draft create/alter/apply/cancel handlers, `pi_recipe`, `run_pi_worker(_with_credential)`, model catalogue, `recipe_schema` / `validate_generated` / `normalize_generated` / `dedupe_sources`.
- `src/recipes.rs` — recipe CRUD handlers **and all SQL accessor helpers** (`find_recipe`, `blocks`, `step_ingredients`, `sources`, `find_draft`, `recipe_snapshot`, …).
- `src/chart.rs` — pure cooking-flow chart builder (`build_chart`, `selected_chart_step`), no I/O.

**AppState** is `Arc`-shared into every handler: `db: SqlitePool`, `model`, `pi_worker_path`, `auth_script_path`, `search_grounding: bool`, and `codex_flows: Arc<Mutex<HashMap<String, CodexFlow>>>` for in-flight device-code flows.

Data flow: HTTP request → handler (extractors `State<Arc<AppState>>`, `Path`, `Form`, `Query`) → sqlx helpers or the AI pipeline → Askama template struct → HTML. Every page is a server-rendered template in `templates/`; forms post `application/x-www-form-urlencoded` via axum `Form`.

AI flow (load-bearing): `POST /ai/generate` → `create_draft` → `spawn_blocking` runs `node pi/recipe-worker.mjs` (one JSON request on stdin, one JSON line on stdout) with a **per-request `0o600` temp `auth.json`** materialized from the DB credential; the worker's token refresh is read back and persisted via `store_codex_credential`; result lands in `ai_drafts` with `expires_at = now + 24h`; the preview page shows it and `apply_draft` persists in one transaction under a `base_updated_at` staleness guard (rejects if the recipe changed meanwhile).

Key invariants:

- AI output must pass `validate_generated` (every ingredient used exactly once, `inputSteps` only reference earlier steps, …) before any draft is saved.
- `recipes.chart_json` is AI-derived only; any manual block edit MUST call `invalidate_chart` so the viewer falls back to linear inference.
- The Codex credential is DB-only; Pi SDK invocations MUST use `run_pi_worker_with_credential`, never a fixed-path credential file.

## Key Directories

| Path | Purpose |
|---|---|
| `src/` | Rust binary crate: `main.rs` (bootstrap/router/shared types), `ai.rs`, `recipes.rs`, `chart.rs` |
| `src/tests/` | Rust unit-test tree (wired via `#[cfg(test)] mod tests;` in `main.rs`) |
| `pi/` | Node ESM sidecar: `recipe-worker.mjs` (entry), `codex-auth.mjs` (device-flow CLI), `codex-native-search.mjs` (pure lib) + `*.test.mjs` |
| `templates/` | Askama HTML templates, `base.html` parent + `{% block %}` children |
| `static/` | `app.css`, `app.js` (strict ES5 IIFE, progressive enhancement) |
| `migrations/` | sqlx SQL migrations, `NNNN_name.sql` naming |

## Development Commands

```sh
cargo build / cargo run          # local Rust build (needs DATABASE_URL default sqlite:recipes.sqlite3)
cargo test                        # Rust unit tests
cargo fmt --check && cargo clippy -- -D warnings
npm test                          # Node worker tests: node --test pi/*.test.mjs
docker compose up --build         # full stack, port 3000, sqlite in recipe-data volume
./deploy.sh [ssh-target]          # ssh → git pull --ff-only && docker compose up --build -d (default bailee@192.168.8.223)
```

The binary accepts `--healthcheck` (probes `GET /healthz` without starting the server; used by the compose healthcheck).

## Code Conventions & Common Patterns

- **Handlers**: `pub(crate) async fn` free functions, `State<Arc<AppState>>` first, returning the crate alias `type Result<T> = std::result::Result<T, AppError>`. Page-render handlers are named `*_page`; mutators follow `update_/delete_/add_/move_/alter_/apply_/cancel_*`.
- **Errors**: one `AppError` enum (thiserror) in `main.rs` with a single `IntoResponse` mapping — `NotFound`→404, `BadRequest`→400, everything else→500. Extend `AppError`, never introduce per-module error types. Log once at the boundary: `error!(error = %self, "request failed")`.
- **SQL**: sqlx 0.8 raw queries (no `query!` macros), row types via `#[derive(FromRow)]`, multi-step writes inside `sqlx::Transaction` passed as `&mut sqlx::Transaction<'_, sqlx::Sqlite>`. All DB accessors live in `src/recipes.rs`.
- **IDs & timestamps**: `Uuid::new_v4().to_string()` TEXT PKs; timestamps are RFC3339 strings via `stamp()`. Do not introduce INTEGER ids or epoch timestamps.
- **Config**: `dotenvy::dotenv().ok()` with defaults read in `main()`; boolean switches via `env_bool(name, default)`; user-visible settings persist in the `app_settings` key/value table, not env.
- **Async**: `#[tokio::main]`; blocking Node subprocess work inside `tokio::task::spawn_blocking` with plain `std::process::Command`.
- **Logging**: `tracing` + env-filter; `info!`/`warn!`/`error!`; HTTP via `TraceLayer`.
- **Templates**: Askama structs with `#[template(path = "...")]`; new pages extend `templates/base.html`; `{% match %}` for Rust Option/enum rendering.
- **Shared types** (serde, camelCase renames for AI wire types) live in `main.rs` — keep them there, not in feature modules.
- Form validation helpers in `main.rs`: `required(s, name)`, `trim`, `number(s)`.

## Important Files

- `src/main.rs` — entry point; `routes()` (router table at `main.rs:419`), `AppState`, `AppError`, `anyhowless` module (startup error alias deliberately avoiding an anyhow dep).
- `Cargo.toml` / `Dockerfile` / `compose.yaml` / `deploy.sh` / `.env.example` — build, deploy, env contract.
- `pi/recipe-worker.mjs` — the worker protocol contract: stdin JSON `{prompt, systemPrompt, model, searchEnabled, authPath}` or `{command: "listModels"}`; stdout `{recipe, sources}` or `{error, code}` (`code: "configuration"` → `AiNotConfigured`).
- `migrations/0001_initial.sql` — core schema (`recipes`, `recipe_blocks`, `recipe_sources`, `ai_drafts`); later migrations add `recipe_step_ingredients`, `recipes.chart_json`, `pi_credentials`, `app_settings`.
- `templates/recipe.html` — largest template: inline edit forms, block move/delete, chart view.

## Runtime/Tooling Preferences

- **Rust**: edition 2024 (needs ≥ 1.85; Docker pins `rust:1.88-slim`). glibc, not musl. `sqlx` 0.8 no-default-features (tokio-rustls, sqlite, migrate); `reqwest` with rustls (no native-tls).
- **Node**: ≥ 22 (`node:22-bookworm-slim` runtime image), ESM (`"type": "module"`), npm; `@earendil-works/pi-ai` and `@earendil-works/pi-coding-agent` pinned together at `0.83.0` — keep them in lockstep.
- **Env vars** (`.env.example`): `DATABASE_URL` (`sqlite:/data/recipes.sqlite3`), `APP_BIND` (`0.0.0.0:3000`), `PI_MODEL` (`gpt-5.4-mini`), `PI_SEARCH_ENABLED` (default `true`), `RUST_LOG` (`kindle_recipes=info,tower_http=info`), optional `PI_WORKER_PATH` / `PI_AUTH_SCRIPT_PATH` / `PI_CODING_AGENT_DIR`. No OAuth tokens in `.env` — the Codex credential lives in the DB.
- Migrations run via compile-time embedded `sqlx::migrate!()`; any schema change is a new numbered `migrations/000N_*.sql`.
- `.agents/` skills are coding-agent tooling, excluded from the Docker image.

## Testing & QA

- **Rust** (`cargo test`): unit tests in `src/tests/` (`ai.rs`, `auth.rs`, `chart.rs`, `config.rs`, `recipes.rs`) plus inline `mod model_tests` in `main.rs`. Use `#[tokio::test]`; handler tests in `src/tests/recipes.rs` drive axum `State`/`Path` directly against in-memory SQLite with `sqlx::migrate!()` — reuse its `database()` / `state()` helpers. Env-var tests MUST take the static `ENV_LOCK: Mutex<()>` in `src/tests/config.rs` (and note `unsafe std::env::set_var` in edition 2024).
- **Node** (`npm test`): built-in `node:test` across `pi/*.test.mjs`. Mocking is HTTP-level: swap `globalThis.fetch` (codex-auth), inject a `fetchImpl` parameter (codex-native-search), or test pure functions (recipe-worker). No test framework deps, no coverage thresholds.
- Touch the worker protocol → run the `pi/*.test.mjs` suites; touch validation/schema → run `cargo test`.
- Intended full validation: `docker compose up --build`, then visit `/healthz`, create a recipe, edit and reorder blocks, and test one AI preview with a real API key.
