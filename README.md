# Kindle Recipes

A deliberately small recipe library for Kindle browsers and ordinary desktop browsers. It is a single Rust/Axum process: Askama renders every page on the server, SQLite holds the data, and the only browser script is an optional ES5 inline-edit convenience.

## Start it

1. Copy `.env.example` to `.env` and add `GEMINI_API_KEY` if you want AI features.
2. Run `docker compose up --build`.
3. Open `http://localhost:3000` (or the machine's LAN address from the Kindle).

Recipe data is kept in Docker's `recipe-data` volume at `/data/recipes.sqlite3`. It survives container rebuilds and restarts. To use a local database instead, set `DATABASE_URL=sqlite:recipes.sqlite3` before starting the binary.

`GEMINI_BASE_URL` normally stays at its default Interactions endpoint. It is available for a local mock server in integration tests; do not use it to send your API key to an untrusted endpoint.
`GEMINI_SEARCH_GROUNDING` controls whether Gemini may use Google Search. Set it to `true` or `false`; it defaults to `true`.

## Using it

- Create a recipe from a dish idea, a named recipe, or one or more recipe URLs. Gemini uses Google Search and URL Context to research the request before preparing a draft for review.
- Recipes keep one compact ingredient list at the top, then repeat the exact amounts used in small boxes beneath the relevant method step. Tap an ingredient or step to edit it; step ingredients are entered one exact amount per line. With JavaScript off, the same tap opens an ordinary edit form.
- Saved recipes can switch between the conventional card and an optional cooking-flow chart. Chart stages are directly selectable, support Back/Next and keyboard navigation, highlight their ingredient inputs, and start a countdown when the generated step has a timer.
- Add, remove, or move ingredient and method blocks. A block has a stable ID; its `section` and `position` only determine its display order.
- Use **New Recipe** for a fresh grounded recipe, or **Alter with AI** from a recipe page for a complete replacement draft. Nothing is saved until the preview is confirmed.
- Gemini calls use the Interactions API with `gemini-3.6-flash`, `store: false`, and strict JSON output. Each generated step includes its ingredient amounts, prior-step inputs, concise chart label, and timer metadata. When `GEMINI_SEARCH_GROUNDING=true`, requests include the `google_search` and `url_context` tools; citations appear in the expandable source count beneath the recipe timings, and the preview shows any returned Google search-suggestions widget. Drafts expire after 24 hours.

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
