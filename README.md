# Kindle Recipes

A deliberately small recipe library for Kindle browsers and ordinary desktop browsers. It is a single Rust/Axum process: Askama renders every page on the server, SQLite holds the data, and the only browser script is an optional ES5 inline-edit convenience.

## Start it

1. Copy `.env.example` to `.env`.
2. Run `docker compose up --build`.
3. Open `http://localhost:3000/settings` and choose **Authorise Codex**. A one-time sign-in code opens at chatgpt.com/codex/device; the app waits for the ChatGPT Plus/Pro sign-in and stores the OAuth credential in the recipes database. (The same flow is reachable from the gear menu. A credential previously saved by the pi CLI in `~/.pi/agent/auth.json` is imported into the database automatically on the first start after upgrading.)
4. Open `http://localhost:3000` (or the machine's LAN address from the Kindle).

Recipe data is kept in Docker's `recipe-data` volume at `/data/recipes.sqlite3`. It survives container rebuilds and restarts. To use a local database instead, set `DATABASE_URL=sqlite:recipes.sqlite3` before starting the binary.

`PI_MODEL` sets the initial Pi OpenAI Codex model; it defaults to `gpt-5.4-mini`. After startup, the model can be changed and persisted from the Settings page. `PI_SEARCH_ENABLED` controls OpenAI's hosted native `web_search` tool and defaults to `true`. Set it to `false` to generate without web research.

## Using it

- Create a recipe from a dish idea, a named recipe, or one or more recipe URLs. Pi uses your ChatGPT subscription and web search to research the request before preparing a draft for review.
- Recipes keep one compact ingredient list at the top, then repeat the exact amounts used in small boxes beneath the relevant method step. Tap an ingredient or step to edit it; step ingredients are entered one exact amount per line. With JavaScript off, the same tap opens an ordinary edit form.
- Saved recipes can switch between the conventional card and an optional cooking-flow chart. Chart stages are directly selectable, support Back/Next and keyboard navigation, highlight their ingredient inputs, and start a countdown when the generated step has a timer.
- Add, remove, or move ingredient and method blocks. A block has a stable ID; its `section` and `position` only determine its display order.
- Use **New Recipe** for a fresh grounded recipe, or **Alter with AI** from a recipe page for a complete replacement draft. Nothing is saved until the preview is confirmed.
- The Rust app stores the Pi OAuth credential in the database, materialises it into a private per-request `auth.json` for the Pi SDK worker, and captures any token refresh back into the database. The worker exposes no filesystem or shell tools. When `PI_SEARCH_ENABLED=true`, the worker injects OpenAI's hosted `web_search` declaration into the Codex Responses request, verifies that a native search event occurred, and accepts source URLs only from that search-backed response. Those sources appear beneath the recipe timings. Each generated step includes its ingredient amounts, prior-step inputs, concise chart label, and timer metadata. Drafts expire after 24 hours.

## Deployment note

This first release deliberately has no login. Only expose it on a trusted LAN or behind a VPN/reverse proxy with access control. Do not place it directly on the public internet.

## Verification

The project has Rust unit tests for generated recipe validation, source deduplication, and the structured recipe schema. When Rust is available locally, run:

```sh
cargo fmt --check
cargo test
cargo clippy -- -D warnings
```

The intended complete validation command is:

```sh
docker compose up --build
```

Then visit `/healthz`, create a recipe, edit and reorder blocks, and test one AI preview with a real API key.
