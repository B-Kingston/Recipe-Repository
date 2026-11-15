# Repository Guidelines

## Project Overview

Kindle Recipes (Recipe Repository) is a deliberately small recipe library for Kindle and desktop browsers. One Rust/Axum process: Askama renders every page on the server, SQLite holds all data, htmx 1.9 makes navigation feel like an app, and a Node worker on the Pi SDK (`pi/`) generates recipes through a ChatGPT (Codex) or Claude subscription or a provider's API key. A Python helper (`pi/local-media.py`) transcribes and OCRs social videos locally, and an EmbeddingGemma 2 sidecar (llama.cpp, compose service `embeddings`) powers search, step moments, folders, and dish-photo ranking.

Accounts use a native password sign-in: the first visit goes to `/setup`, which creates the first account; after that every library route needs a session. Each account has its own library, drafts, settings, and AI logins. Accounts can form a **group** (household, `src/groups.rs`) through email invites: members share chosen recipes read-only and may let each other generate with their AI logins.

## Architecture

Binary crate `kindle-recipes` (edition 2024, no `lib.rs`):

| File | What lives there |
|---|---|
| `src/main.rs` | Bootstrap, env config, `routes()`, `AppState`, `AppError`, shared serde and template structs, form helpers, the Settings and sign-in-flow handlers, `PROVIDER_PRESETS`, `image_response` / `not_modified` (ETag + 304 for every locally produced JPEG). |
| `src/auth.rs` | Accounts and cookie sessions: `auth_middleware`, `AuthUser`, setup, sign-in/out, multi-account switching, password change, account deletion (`delete_user`), brute-force limits (`RateLimits`), `same_origin_only`. |
| `src/settings.rs` | All queries on `app_settings`, `pi_credentials`, `ai_endpoints`, `web_search_keys`, and AI grants; `ai_account` resolution. Re-exported from `main.rs`, so callers use `crate::ai_provider` etc. |
| `src/ai.rs` | Add-recipe card, drafts, generation (`pi_recipe`), chat and cleaner calls, worker runners (`run_node`, `run_worker`, `run_subscription_worker`), `run_with_retry`, login tracking, model catalogue, recipe prompts and `validate_generated`, the video import and media-debugger runs. |
| `src/chat.rs` | The recipe chat (Ask drawer): questions, answers, proposals with Accept/Deny, Keep as note. |
| `src/notes.rs` | The cook's notes (text and/or photo, whole recipe or one version). |
| `src/recipes.rs` | Recipe handlers and most SQL (recipes, blocks, versions, drafts, thumbnails, moments, search index, chat threads, notes); `begin_write`. |
| `src/groups.rs` | Groups, invites (Resend mail or a shown link), AI grants, the Share toggle. |
| `src/scale.rs`, `src/chart.rs` | Pure servings scaling and cooking-flow chart building. |
| `src/media.rs`, `src/media_worker.rs` | Social video import (yt-dlp, ffmpeg, Whisper and OCR lanes, OCR cleaning), and the persistent Python worker client. |
| `src/thumbs.rs`, `src/moments.rs` | Dish-photo candidates and video step moments. |
| `src/embeddings.rs`, `src/search.rs`, `src/folders.rs` | Sidecar client, library search (vectors with BM25 fallback), folders and cuisines. |
| `src/htmx.rs`, `src/display.rs` | Cookie helpers, one-time flash notices, htmx response glue; colour/e-ink display mode. |
| `pi/` | Node ESM worker in TypeScript run by Node's type stripping (no build): `recipe-worker.ts` (protocol types and entry), `codex-auth.ts`, `claude-auth.ts`, `native-search.ts`, `web-search.ts`, `recipe-edits.ts`, `cli.ts`, plus `*.test.ts`; `local-media.py`. |
| `web/src/` | Browser TypeScript compiled to one ES5 `static/app.js` (order: `files` in `web/tsconfig.json`, `main.ts` last). |
| `templates/` | Askama templates; `base.html` is the parent. |
| `static/` | `app.css` (Enamelware look), `eink.css` (e-ink overrides), SVGs, self-hosted fonts. `app.js` and `htmx.js` are build outputs (gitignored). |
| `migrations/` | sqlx migrations, `NNNN_name.sql`. |
| `bench/` | OCR benchmark harness and fixtures the Rust tests read. `e2e/ask/` is a manual end-to-end run of the recipe chat against a deployment. |

Request flow: handler (`State<Arc<AppState>>`, `AuthUser`, `Path`, `Form`, `Query`) → sqlx accessors or the AI pipeline → Askama struct → HTML. Forms post `application/x-www-form-urlencoded`.

## Front end (load-bearing)

- Every page is a whole server-rendered document. `base.html` loads `/static/htmx.js` and `/static/app.js` (`defer`) and puts `hx-boost="true"` on `<body>`. Without JavaScript every page works as plain HTML; when htmx fails to load, `app.js` still wires the page.
- htmx config (`htmx-config` meta): `historyCacheSize: 0` (another account may use the browser), `allowEval` and `allowScriptTags` off. No inline scripts anywhere; behaviour lives in `web/src`.
- `hx-boost="false"` where a whole page must load: account menu, sign-out, account creation and deletion, the display form, multipart uploads (photo, add-note), edit-block links and their Cancel links, still links.
- Status notices never go in the query string: mutators answer `flash_redirect("/page#block", "key")` and the page maps the key to text (Settings shows each in its own block). The invite form is the exception: it answers with the Settings page itself so a link made without mail shows once.
- Background work is polled with hidden `hx-trigger="every 3s"` elements; `data-poll-unless-editing` skips a tick while an edit form is open, and `data-swap-when-gone="<selector>"` holds the swap until the marker leaves the response.
- Errors (4xx/5xx) swap into `.app-shell`; a non-GET htmx request answered with a page gets `HX-Push-Url: false` (`htmx::htmx_layer`).
- Browser code is ES5 for old Kindle browsers: only the ES5 lib is visible to the type checker, so use the `dom.ts` helpers (`closest`, `each`, …). A feature registers with `onPage(root => …)` and must find its elements inside `root`. htmx is pinned at 1.9.12.
- `web/src/add-recipe.ts` sorts links the same way as `ai::route_prompt` / `media::canonical_social_url`; keep them in step.

## Accounts and auth (load-bearing)

- Every library route sits behind `auth_middleware` on the `protected` router. Outside it: `/setup`, `/login`, `/logout`, `/logout/all`, `/healthz`, `/display.css`, `/static`, and the invite routes `GET /invites/{token}`, `POST /invites/{token}/decline`, `POST /invites/{token}/signup`. No session and no users → `/setup`; otherwise → `/login` (GETs carry `?next=`, accepted only as a local path by `local_target`).
- Passwords are Argon2 hashes only; an unknown username costs the same dummy check. `reset_password` verifies the current password first, ends every session of the user, and issues a new one. Never log or return password material.
- Sessions: the cookie holds 32 random bytes (base64url); the `sessions` table keeps only their SHA-256. A session ends 30 days after last use, extended at most daily. Cookies are `Path=/; HttpOnly; SameSite=Lax`, no `Secure` (plain HTTP on the LAN), and are all built by `htmx::set_cookie`. A cookie the middleware sends is dropped when the handler already set that cookie.
- Multi-account: `recipe_session` holds the active token and `recipe_accounts` up to 7 others. The account menu lists only this browser's accounts, never the server's other users. Switching to any id not among them goes to plain `/login`.
- `same_origin_only` refuses non-GET/HEAD/OPTIONS requests from another origin (`Sec-Fetch-Site`, else `Origin` vs `Host`) with `AppError::CrossOrigin` (403).
- Brute-force limits (`auth::RateLimits`, in memory, sliding windows): sign-in per client address (30/15 min) and per username (10/15 min, case-folded, known or not); wrong current passwords per user (10/15 min); new accounts per address (10/hour). A limited form re-renders with 429 and "Try again in N minutes." The client address is the peer address, or with `TRUST_PROXY_HEADERS` the left-most `X-Forwarded-For`; IPv6 counts per /64. Tests have no connect info, so per-address limits are skipped there unless a test inserts `ConnectInfo`.
- Account deletion (`delete_user`) is one transaction: `groups::leave_group_in` first, then every per-account table, then `users`; cascades take the rest. **A new per-user table without `ON DELETE CASCADE` on `users(id)` MUST be added to `delete_user` and to `OWNED_TABLES` / `seed_account` in `src/tests/auth.rs`.** The handler also drops the account's in-memory state (model catalogues, job sets, sign-in flows, media runs).

## AI flow (load-bearing)

- Add recipe is one card: `POST /ai/generate` → `route_prompt`. One Facebook/Instagram video link starts a video import (the rest of the text becomes notes); no video → `create_draft` → `pi_recipe`. Drafts live 24 h in `ai_drafts`; `apply_draft` always creates a new recipe and deletes the draft in the same transaction. Drafts never change a saved recipe; only the recipe chat's Accept does.
- `ai_account(db, user)` resolves whose AI setup serves a user: its own when configured, else a group member who granted it theirs (the one picked in `ai_source`, then the earliest), else its own (which fails as `AiNotConfigured`). Grants count only while both accounts are members of the grant's group.
- `resolve_backend` turns `app_settings.ai_provider` (`"pi"` Codex, `"claude"`, or an `ai_endpoints` id) into `RecipeBackend::{Subscription, Endpoint}`. Endpoint specs: `openai` / `anthropic` (worker's native code; web search only at the native base URL, `endpoint_has_web_search`), pi-ai provider ids, or `openai-compatible`. An endpoint without web search, or `PI_SEARCH_ENABLED=false`, generates with search off.
- Workers run on blocking threads, at most `AI_WORKER_CONCURRENCY` (2) at once; one JSON request on stdin, one JSON line on stdout (`pi/recipe-worker.ts` holds the protocol types). Subscription runs get a per-run `0o600` temp `auth.json` (`TempAuthFile`) from the DB credential; a refreshed token is written back only over the credential the run started from. Never use a fixed-path credential file.
- `run_with_retry` is the one decode-and-retry loop (generation, chat, cleaner): one retry for a retryable failure or a reply the caller refuses; a missing or refused login is never retried. A refused login (`code: "auth"`) stamps `auth_failed_at` and returns `AppError::AiLoginRefused`.
- Generated recipes must pass `validate_generated` before any draft is saved.
- Every worker call logs the full request and response at `info`, but never `apiKey`/`authPath`. Endpoint and search keys live only in the database, reach templates only through `mask_key`, and must pass `header_safe_key`.
- Recipe chat: the worker's edit tools change a copy; the reply's `proposal` is stored and shown as a diff; Accept runs `recipes::accept_chat_proposal` in one transaction, refusing a stale proposal (`updated_at` moved). Answers left pending by a restart are failed at startup.

## Key invariants

- Owner scoping: every recipe query is `user_id`-scoped (`find_recipe`), except the read-only shared view (`recipes::READABLE_BY_VIEWER` in `find_readable_recipe`, dish photo and still handlers, the Group view, `copy_recipe`). Notes, chat threads, photos, stills, and media runs are served only to their owner (another account gets 404).
- Every change to a recipe's title, description, or blocks MUST bump `recipes.updated_at` (chat staleness and the search index compare it). Any manual block edit MUST also call `invalidate_chart`; `chart_json` is AI-derived only. Every block mutation calls `moments::spawn_recipe_refresh`. Toggling `shared` or a folder pin keeps `updated_at` and records no version.
- The newest version cannot be deleted, so it always matches the live recipe.
- Dish photos and note photos come only from the local ffmpeg pipeline or an ffmpeg-normalised upload, never a cloud service, and are never logged. Draft and recipe thumbnail candidates die when consumed or expired.
- Groups: an account is in at most one group; a group exists only with two or more members. Invite tokens are stored only as SHA-256, addresses only masked (`mask_email`) and hashed; `RESEND_API_KEY` is never logged or rendered. Leaving a group deletes the leaver's grants both ways and unshares its recipes.
- Embedding features degrade without the sidecar: search falls back to BM25, imports skip moments, dish photos keep the heuristic order, folders hold only pinned recipes.
- Media: never feed the caption to Whisper as `initial_prompt` or `hotwords` (measured: the transcript collapses). The work directory is removed before the media permit is released; `MAX_MEDIA_SECONDS` (300) caps every channel.
- In SQL, never mix `?` and `?N` placeholders in one query (sqlx binds the unnamed ones out of order).

## Conventions

- Handlers are `pub(crate) async fn` with `State<Arc<AppState>>` first, returning `crate::Result<T>`. Page handlers are `*_page`; mutators `update_/delete_/add_/…`.
- One `AppError` enum in `main.rs` with one `IntoResponse`; `user_message()` is the reader-facing text (`GENERIC_FAILURE` for internal errors). Never add per-module error types. Log once at the boundary.
- SQL: sqlx 0.8 raw queries (no `query!`), `#[derive(FromRow)]`, multi-step writes in `recipes::begin_write` (`BEGIN IMMEDIATE`, never `pool.begin()`). Put new queries with their table's accessors (`recipes.rs`, `settings.rs`; `auth.rs` owns `users`/`sessions`).
- IDs are `Uuid::new_v4().to_string()`; timestamps RFC3339 via `stamp()`.
- Config: env read once in `main()` (`env_bool` for switches); user-visible settings live in `app_settings`, not env. Shared serde types stay in `main.rs`.
- Migrations: any schema change is a new numbered file. NEVER edit an existing migration (sqlx checksums them).
- `pi/*.ts` use erasable syntax only (no enums, namespaces, parameter properties); relative imports end in `.ts`. `@earendil-works/pi-ai` and `pi-coding-agent` stay pinned together.
- Pinned toolchains: `rust:1.88-slim`, `node:22.23.4-bookworm-slim` (never lower), TypeScript 5.9 (browser code targets ES5), htmx 1.9.12.

## Commands

All builds, tests, and installs run in Docker, never on the host.

```sh
# Rust: format, lint, test (the test image needs ffmpeg)
docker run --rm -v "$PWD":/src -v kr-target:/target -e CARGO_TARGET_DIR=/target -w /src <rust-image-with-ffmpeg> \
  sh -c 'cargo fmt --check && cargo clippy --all-targets -q -- -D warnings && cargo test -q'
# Node: worker tests, type checks, browser build
docker run --rm -v "$PWD":/app -w /app node:22.23.4-bookworm-slim \
  sh -c 'npm ci && npm test && npm run typecheck && npm run build:web'
docker build .
```

Do not start the stack by hand. Deploy with `./deploy.sh [target]` (gitignored, host-specific: builds, backs up the live database, `docker compose up -d`, waits for `/healthz`). `AGENTS.local.md`, when present, has this machine's rules.

The binary also accepts `--healthcheck` (compose healthcheck) and `--clean-ocr` (reads an OCR envelope on stdin and prints the cleaned snippets; `bench/ocr/benchmark.py` uses it).

## Testing

- Rust tests live in `src/tests/` (one file per module) plus inline modules in `main.rs`, `media.rs`, `media_worker.rs`, `recipes.rs`, and `thumbs.rs`. `src/tests/mod.rs` has the shared helpers: `memory_db()`, `test_state(db)`, `user(id)`, `body_text(response)`. Handler tests drive handlers directly or through `routes()`; router tests sign in through `POST /login` or the cookie `setup_create` sets.
- Embedding code is tested with synthetic vectors (no sidecar); media tests use `sh` stub workers and the fixtures in `bench/`; photo tests need ffmpeg. Env-var tests MUST take `ENV_LOCK` in `src/tests/config.rs`.
- Node: `npm test` runs `pi/*.test.ts` with `node:test`; mock at the HTTP level (`fetchImpl` parameters or `globalThis.fetch`). `npm run typecheck` checks `pi` and `web`.
- Touching `web/src` or htmx attributes in templates: typecheck, build, and check the pages in a browser. Touching the worker protocol: run the Node suites. Touching validation or SQL: run `cargo test`.
