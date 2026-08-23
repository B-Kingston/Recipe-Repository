# Kindle Recipes

A deliberately small recipe library for Kindle browsers and ordinary desktop browsers. It is a single Rust/Axum process: Askama renders every page on the server, SQLite holds the data, and the only browser script is an optional ES5 inline-edit convenience.

## Start it

1. Copy `.env.example` to `.env`.
2. Run `docker compose up --build`.
3. Open `http://localhost:3000/settings` and choose **Authorise Codex**. A one-time sign-in code opens at chatgpt.com/codex/device; the app waits for the ChatGPT Plus/Pro sign-in and stores the OAuth credential in the recipes database. (The same flow is reachable from the gear menu. A credential previously saved by the pi CLI in `~/.pi/agent/auth.json` is imported into the database automatically on the first start after upgrading.)
4. Open `http://localhost:3000` (or the machine's LAN address from the Kindle).

Recipe data is kept in Docker's `recipe-data` volume at `/data/recipes.sqlite3`. It survives container rebuilds and restarts. To use a local database instead, set `DATABASE_URL=sqlite:recipes.sqlite3` before starting the binary.

`PI_MODEL` sets the initial Pi OpenAI Codex model; it defaults to `gpt-5.4-mini`. After startup, the model can be changed and persisted from the Settings page. `PI_SEARCH_ENABLED` controls OpenAI's hosted native `web_search` tool and defaults to `true`. Set it to `false` to generate without web research.

The Docker image includes the local video-analysis runtime. `MEDIA_WHISPER_MODEL=base` is a good CPU default; the first import downloads that open-source model into `/data/media-models`, then later imports reuse it. The benchmarked production OCR model is fixed to `PP-OCRv6_small_det` plus `PP-OCRv6_small_rec`, downloaded there on the first OCR import. Frames are sampled frequently with ffmpeg and sent to one PaddleOCR process in batches (`MEDIA_OCR_BATCH_SIZE=8` by default), so the OCR model is not instantiated once per frame. `MEDIA_YTDLP_PATH`, `MEDIA_FFMPEG_PATH`, `MEDIA_OCR_SCRIPT`, `MEDIA_PYTHON`, and `MEDIA_TRANSCRIBE_SCRIPT` can point at local replacements. If a public post requires login, `MEDIA_COOKIES_FILE` may point at a locally mounted yt-dlp cookies file; never commit or expose that file. Video imports also require `AI_GATEWAY_API_KEY`: the worker sends the caption, local transcript, and OCR to Vercel AI Gateway's `poolside/laguna-s-2.1-free` cleaner (override with `AI_GATEWAY_CLEANER_MODEL`) using the OpenAI-compatible chat-completions API. The key is inherited by the worker and is never stored in SQLite, passed in process arguments, or logged.

## Using it

- Create a recipe from a dish idea, a named recipe, or one or more recipe URLs. Pi uses your ChatGPT subscription and web search to research the request before preparing a draft for review.
- Choose **From Video** and paste a public Facebook Reel or Instagram post/reel URL. The importer uses `yt-dlp` for the caption and a temporary video copy, `ffmpeg` for audio/frequent screenshots, local CPU `faster-whisper` for speech-to-text, and batched PaddleOCR PP-OCRv6 for on-screen text. A dedicated Vercel AI Gateway cleaner (`poolside/laguna-s-2.1-free`) then keeps only dish, ingredients, quantities, method, timing, temperature, serving, and relevant cooking-note facts; unknown fields and social chatter are discarded before the normal recipe-generation request. Reasoning is disabled on the cleaner by default, and a single retry absorbs transient malformed responses. Media final generation explicitly disables web search, so the final recipe model receives only that cleaned evidence (plus attribution and any user notes), while the raw bounded channels remain in the expiring draft for audit. The media is deleted after extraction; audio and OCR themselves never use an audio/vision API.
- Recipes keep one compact ingredient list at the top, then repeat the exact amounts used in small boxes beneath the relevant method step. Tap an ingredient or step to edit it; step ingredients are entered one exact amount per line. With JavaScript off, the same tap opens an ordinary edit form.
- Saved recipes can switch between the conventional card and an optional cooking-flow chart. Chart stages are directly selectable, support Back/Next and keyboard navigation, highlight their ingredient inputs, and start a countdown when the generated step has a timer.
- Add, remove, or move ingredient and method blocks. A block has a stable ID; its `section` and `position` only determine its display order.
- Use **New Recipe** for a fresh grounded recipe, or **Alter with AI** from a recipe page for a complete replacement draft. Nothing is saved until the preview is confirmed.
- The Rust app stores the Pi OAuth credential in the database, materialises it into a private per-request `auth.json` for the Pi SDK worker, and captures any token refresh back into the database. The worker exposes no filesystem or shell tools. When `PI_SEARCH_ENABLED=true`, the worker injects OpenAI's hosted `web_search` declaration into the Codex Responses request, verifies that a native search event occurred, and accepts source URLs only from that search-backed response. Those sources appear beneath the recipe timings. Each generated step includes its ingredient amounts, prior-step inputs, concise chart label, and timer metadata. Drafts expire after 24 hours.

The social importer is intentionally single-URL and best-effort: Meta may require login, block datacenter IPs, or change its page format. Public Facebook Reels are less reliable to download than Instagram posts. The UI reports missing local audio/OCR tools or cleaner configuration instead of silently sending raw social chatter to the final model. To keep the local process bounded, it analyzes at most the first five minutes, downloads at most a 60 MiB media file, limits the temporary workdir, and runs one import at a time.

To debug the reel pipeline itself, open **Settings → Media extraction debugger**. It runs the same extractor on up to five URLs (one per line), streams each phase live — post description capture, audio transcription, per-frame OCR — and finishes with a review page showing every OCR frame thumbnail next to its raw engine reading, the cleaner's verdict for that reading, the caption chains that became evidence cards, and the full description/audio captures.

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

Then save your Vercel AI Gateway API key in **Settings**, visit `/healthz`, create a recipe, paste a public video URL into **From Video**, wait for local extraction/model warm-up and the cleaner pass, and review the cleaned evidence panel before saving. The example URLs can be tried directly:

- `https://www.facebook.com/reel/2921942621481069`
- `https://www.instagram.com/p/DZNQT3Pt3Ja/`

Meta can require a current yt-dlp cookies file for either URL; this is an upstream access limitation, not a recipe-parser failure. Then edit and reorder blocks and test one ordinary AI preview as well.
