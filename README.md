# Recipe Repository

A deliberately small recipe library for Kindle browsers and ordinary desktop browsers. It is a single Rust/Axum process: Askama renders every page on the server, SQLite holds the data, and the only browser script (`static/app.js`) is an optional ES5 progressive enhancement.

## Start it

1. Copy `.env.example` to `.env`.
2. Run `docker compose up --build`.
3. Open `http://localhost:3000`. The first visit asks you to create a username and password and signs you in. After that, the app shows its own sign-in page when a browser has no session; a sign-in lasts 30 days from the last visit. Change the password later at **Settings → Reset password** (other devices then sign out), and use **Settings → Sign out** to end the session on one device. The account avatar at the top right opens the account menu: create another account (each has its own recipes; new accounts use the first account's AI logins until they choose their own in Settings), switch between the accounts this browser is signed in to, sign in to another one, or sign out.
4. Open `http://localhost:3000/settings` and authorise a subscription:
   - **Authorise Codex**: a one-time sign-in code opens at chatgpt.com/codex/device; the app waits for the ChatGPT Plus/Pro sign-in and stores the OAuth credential in the recipes database. (A credential previously saved by the pi CLI in `~/.pi/agent/auth.json` is imported into the database automatically on the first start after upgrading.)
   - **Authorise Claude**: sign in with your Claude Pro/Max account (the login Claude Code uses) on claude.ai, then paste the code Anthropic shows back into the app. The app stores the OAuth credential in the recipes database.

   The same flows are reachable from the gear menu. The same page also registers OpenAI-spec or Anthropic-spec API endpoints with their keys (stored in the database and shown masked) that can be selected as the active provider.
5. Open `http://localhost:3000` (or the machine's LAN address from the Kindle).

Recipe data is kept in Docker's `recipe-data` volume at `/data/recipes.sqlite3`. It survives container rebuilds and restarts. To use a local database instead, set `DATABASE_URL=sqlite:recipes.sqlite3` before starting the binary.

`docker compose up` also starts the `embeddings` sidecar: llama.cpp's `llama-server` with Google's EmbeddingGemma 2 (Q8_0 text model plus its vision/audio projector). The first start downloads about 870 MB into the `embedding-models` volume; the service then uses about 2.5 GB of RAM and runs on the CPU. It powers library search, video step moments, and the dish-photo ranking. Everything stays on the host. Set `EMBEDDING_URL=` (empty) in `.env` to turn those features off; the rest of the app works unchanged.

`PI_MODEL` sets the initial Pi OpenAI Codex model; it defaults to `gpt-6-luna`. After startup, the provider and model can be changed and persisted from the Settings page; the model list is discovered live from the selected subscription's catalogue. `PI_SEARCH_ENABLED` controls the provider's hosted native `web_search` tool (OpenAI or Anthropic) and defaults to `true`. Set it to `false` to generate without web research.

The Docker image includes the local video-analysis runtime. `MEDIA_WHISPER_MODEL=base` is a good CPU default; the first import downloads that open-source model into `/data/media-models`, then later imports reuse it. Whisper decodes the speech chunks in batches (`MEDIA_WHISPER_BATCH_SIZE=8`, beam size 1) and splits them into timed sentences with its word timings. `MEDIA_WHISPER_TASK` defaults to `transcribe`, which keeps the spoken language; set it to `translate` to have Whisper render non-English audio as English. The benchmarked production OCR model is fixed to `PP-OCRv6_small_det` plus `PP-OCRv6_small_rec`, downloaded there on the first OCR import. ffmpeg extracts candidate frames at 4 Hz. The planner reads a 1 Hz grid of them plus every shot change (`MEDIA_OCR_BASE_HZ`, `MEDIA_OCR_CUT_RATIO`), and a fast detection-only pass skips planned frames that show no text (`MEDIA_OCR_TEXT_GATE`). Transcription and OCR run at the same time, each in a long-lived helper process (`pi/local-media.py serve`), so later imports skip the Python imports and model loads. A helper exits after `MEDIA_WORKER_IDLE_SECONDS` (600) idle seconds. `MEDIA_YTDLP_PATH`, `MEDIA_FFMPEG_PATH`, `MEDIA_OCR_SCRIPT`, `MEDIA_PYTHON`, and `MEDIA_TRANSCRIBE_SCRIPT` can point at local replacements. If a public post requires login, `MEDIA_COOKIES_FILE` may point at a locally mounted yt-dlp cookies file; never commit or expose that file. Video imports also need a Vercel AI Gateway API key, saved once in **Settings**: the worker sends the caption, the timestamped local transcript, and the OCR cards to Vercel AI Gateway's `poolside/laguna-s-2.1-free` cleaner (override with `AI_GATEWAY_CLEANER_MODEL`) using the OpenAI-compatible chat-completions API. The key is stored in the recipes database, reaches the worker only through its environment, and is never passed in process arguments or logged.

## Using it

- Open **Add recipe**. Switch between researching a dish idea, named recipe, or recipe URLs with the active AI provider, and importing a public Facebook Reel or Instagram post/reel. The video importer uses `yt-dlp` for the caption and a temporary video copy, `ffmpeg` for audio/frequent screenshots, local CPU `faster-whisper` for speech-to-text, and batched PaddleOCR PP-OCRv6 for on-screen text. A dedicated Vercel AI Gateway cleaner (`poolside/laguna-s-2.1-free`) then keeps only dish, ingredients, quantities, method, timing, temperature, serving, and relevant cooking-note facts; unknown fields and social chatter are discarded before the final recipe-generation request. Reasoning is disabled on the cleaner by default, and a single retry absorbs transient malformed responses. Media final generation runs in gap-fill mode: the final recipe model receives only that cleaned evidence and may use web search to fill whatever gaps it leaves open, with the original post kept as the attribution source.
- Video imports end with a **Dish photo** picker on the draft preview: a local ffmpeg pass scores sampled frames for colour, texture, sharpness, and centre composition, skips intros/outros and OCR caption cards, and crops square candidates around the food. With the embedding sidecar, twelve crops are nominated and EmbeddingGemma 2 ranks them by how much more each looks like "a finished, plated dish" than like the usual non-dish shots (the cook talking to camera, raw ingredients, caption cards, utensils); the best four are offered. The best shot is pre-selected; saving copies your choice onto the recipe so the library card shows the dish. Set `THUMBNAILS_ENABLED=false` to skip this entirely.
- Video imports also match every method step to its **moment in the video**. The importer embeds the reel's audio in 12-second windows and samples a still every two seconds. When EmbeddingGemma 2 finds that the narration clearly talks about the steps, each step takes the moment it is described; otherwise (music-only reels) the steps are matched to the frames. Each step then shows the moment's time and three stills from that moment, on the draft, the recipe card, and the chart view. One tap on a still opens it in a full-page viewer; the arrows step through that step's other stills, and Escape, the × button, or a tap outside the image closes it. Editing, adding, or moving steps re-matches them; AI alterations keep the moments. A recipe saved before this feature shows **Find step moments in the video** under its method: the app downloads the video again and matches the steps in the background. Switch the feature off under **Settings → Video step moments**.
- Search the library from the box on the home page. Each recipe's description, ingredients, and method are embedded once (and again after an edit); a search matches by meaning, so "something creamy with eggplant" finds the coconut curry. Titles containing the search text rank first. When the embedding service is missing or down, search falls back to BM25 word matching over every word of each recipe; Settings → Developer tools can make word search the default (other accounts follow the first account's choice unless they pick their own).
- Saved recipes keep a **Dish photo** panel on the card: upload any image (normalised to a square 600 px JPEG locally), remove the photo, or — when the recipe carries a social-video source — re-run just the frame-sampling half of the pipeline to get four fresh candidates to choose from.
- Recipes keep one compact ingredient list at the top, then repeat the exact amounts used in small boxes beneath the relevant method step. Tap an ingredient or step to edit it; step ingredients are entered one exact amount per line. With JavaScript off, the same tap opens an ordinary edit form.
- Saved recipes can switch between the conventional card and an optional cooking-flow chart. Chart stages are directly selectable, support Back/Next and keyboard navigation, highlight their ingredient inputs, and start a countdown when the generated step has a timer.
- Add, remove, or move ingredient and method blocks. A block has a stable ID; its `section` and `position` only determine its display order.
- Use **Add recipe** for a fresh grounded recipe, or **Alter with AI** from a recipe page for a complete replacement draft. Nothing is saved until the preview is confirmed.
- The Rust app stores the Pi OAuth credential in the database, materialises it into a private per-request `auth.json` for the Pi SDK worker, and captures any token refresh back into the database. The worker exposes no filesystem or shell tools. When `PI_SEARCH_ENABLED=true`, the worker injects OpenAI's hosted `web_search` declaration into the Codex Responses request, verifies that a native search event occurred, and accepts source URLs only from that search-backed response. Those sources appear beneath the recipe timings. Each generated step includes its ingredient amounts, prior-step inputs, concise chart label, and timer metadata. Drafts expire after 24 hours.

The social importer is intentionally single-URL and best-effort: Meta may require login, block datacenter IPs, or change its page format. Public Facebook Reels are less reliable to download than Instagram posts. The UI reports missing local audio/OCR tools or cleaner configuration instead of silently sending raw social chatter to the final model. To keep the local process bounded, it analyzes at most the first five minutes, downloads at most a 60 MiB media file, limits the temporary workdir, and runs one import at a time.

To debug the reel pipeline itself, open **Settings → Media extraction debugger**. It runs the same extractor on up to five URLs (one per line), shows each phase as it completes — post description capture, audio transcription, per-frame OCR — and ends with a review page showing every OCR frame thumbnail next to its raw engine reading, the cleaner's verdict for that reading, the caption chains that became evidence cards, and the full description/audio captures.

## Deployment note

Access is by account: the first is created on first visit, and any signed-in user can add more from the account menu. Passwords are stored only as Argon2 hashes in the database. Each sign-in gets a random session token in an `HttpOnly` cookie, and the database stores only the token's hash. The cookie has no `Secure` flag, because the app serves plain HTTP. Every signed-in user can see the other usernames and create accounts. Expose it only on a trusted LAN or behind a VPN/reverse proxy with access control. Do not place it directly on the public internet.

## Verification

The project has Rust unit tests (recipe validation, source deduplication, auth, media and thumbnail pipelines) and Node tests for the Pi worker. Run them in throwaway containers so the host needs no toolchain:

```sh
docker run --rm -v "$PWD":/src:ro -e CARGO_TARGET_DIR=/tmp/target rust:1.88-slim sh -c \
  'apt-get update -qq && apt-get install -y -qq ffmpeg >/dev/null && rustup component add rustfmt clippy >/dev/null && cp -r /src /work && cd /work && cargo fmt --check && cargo test && cargo clippy -- -D warnings'
docker run --rm -v "$PWD":/src:ro node:22-bookworm-slim sh -c \
  'cp -r /src /work && cd /work && npm ci && npm test'
```

The intended complete validation command is:

```sh
docker compose up --build
```

- Then save your Vercel AI Gateway API key in **Settings**, visit `/healthz`, open **Add recipe**, switch to **Generate from video**, paste a public video URL, wait for local extraction/model warm-up and the cleaner pass, and review the cleaned evidence panel before saving. The example URLs can be tried directly:

- `https://www.facebook.com/reel/2921942621481069`
- `https://www.instagram.com/p/DZNQT3Pt3Ja/`

Meta can require a current yt-dlp cookies file for either URL; this is an upstream access limitation, not a recipe-parser failure. Then edit and reorder blocks and test one ordinary AI preview as well.
