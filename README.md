# Recipe Repository

A deliberately small recipe library for Kindle browsers and ordinary desktop browsers. It is a single Rust/Axum process: Askama renders every page on the server, SQLite holds the data, and htmx swaps pages in place so navigation and saves feel instant without a client-side app. The browser scripts (htmx and `static/app.js`, both ES5 for Kindle browsers) are optional: with JavaScript off, every page is plain HTML.

## Start it

1. Optionally copy `.env.example` to `.env` and change what you need; every value there is a default.
2. Run `docker compose up --build`.
3. Open `http://localhost:3000` (or the machine's LAN address from the Kindle). The first visit asks for a username and password and signs you in; a sign-in lasts 30 days from the last visit. The avatar at the top right opens the account menu: create more accounts (each has its own recipes and AI logins), switch between the accounts this browser is signed in to, or sign out. **Settings** has the password and account deletion pages.
4. In **Settings → AI provider**, connect one of:
   - **Codex (ChatGPT)**: a one-time code for chatgpt.com/codex/device signs in a ChatGPT Plus/Pro account.
   - **Claude (Pro/Max)**: sign in on claude.ai, then paste the code it shows back into the app.
   - An API key for OpenAI, Anthropic, Google Gemini, Mistral, xAI, DeepSeek, OpenRouter, Groq, Together AI, Fireworks, Cerebras, or another OpenAI- or Anthropic-compatible server.

   Credentials and keys are stored in the database (keys are shown masked), reach the worker only on its stdin, and are never logged.

Data lives in the `recipe-data` volume at `/data/recipes.sqlite3`. `docker compose up` also starts the `embeddings` sidecar (llama.cpp with EmbeddingGemma 2; the first start downloads about 870 MB, then it uses about 2.5 GB of RAM on the CPU). It powers library search, video step moments, library folders, and dish-photo ranking, all on the host. Set `EMBEDDING_URL=` (empty) to turn those off; everything else keeps working.

## Using it

- **Add recipe**: write a dish idea, a recipe to recreate, recipe links, or a public Facebook Reel or Instagram post, in any mix. Without a video the AI provider researches the prompt (web search when `PI_SEARCH_ENABLED=true` and the provider has it). With a video, the importer reads it locally: `yt-dlp` fetches the caption and video, `ffmpeg` extracts audio and frames, `faster-whisper` transcribes the speech, and PaddleOCR PP-OCRv6 reads on-screen text. The **video cleaner** (the active provider unless Settings picks another) keeps only recipe facts before the recipe model sees them. Nothing is saved until you confirm the draft preview, which you can alter as often as you like.
- **Video extras**: imports offer dish-photo candidates scored from the video's frames, and match every step to its moment in the video (time and stills on the draft, card, and chart). `THUMBNAILS_ENABLED=false` skips the photos; **Settings → Video step moments** switches the moments off.
- **Recipes**: one ingredient list at the top, with each step's exact amounts under it. Tap any block to edit it; add, remove, or move blocks; scale servings; switch to the cooking-flow chart with timers. Every edit is kept as a version, and notes (text or a photo) can be added to the recipe or one version.
- **Ask**: the recipe chat answers questions about a recipe and proposes changes, listed exactly under **Accept** and **Deny**; a recipe changes only when you accept.
- **Search and folders**: search matches by meaning (BM25 word search when the sidecar is missing or chosen under Developer tools), and the library can be filed A to Z, by dish folder, or by cuisine.
- **Groups**: in **Settings → Group**, invite someone by email (the link works once, for 7 days; without `RESEND_API_KEY`, Settings shows the link to send yourself). Members share chosen recipes read-only and can save a copy, and each member can let others generate with their AI logins while those have none of their own.
- **Display**: **Settings → Display** switches a browser between colour and e-ink; Automatic picks e-ink on a Kindle.

Video import is best-effort: Meta may require a login or block datacenter addresses. `MEDIA_COOKIES_FILE` can point at a locally mounted yt-dlp cookies file (never commit it). Imports analyse at most the first five minutes, download at most 60 MiB, and run one at a time. Whisper uses the `base` model (`MEDIA_WHISPER_MODEL`); `MEDIA_WHISPER_TASK=translate` renders non-English speech as English. Models download once into `/data/media-models`. **Settings → Media extraction debugger** runs the extractor on up to five URLs and shows every phase, OCR frame, and cleaner verdict.

## Deployment note

Passwords are stored only as Argon2 hashes, and the database keeps only the hash of each session token. Session cookies are `HttpOnly` but not `Secure`, because the app serves plain HTTP: put a TLS proxy in front of anything beyond a trusted LAN, and set `TRUST_PROXY_HEADERS=true` behind a proxy that sets `X-Forwarded-For`, so the sign-in limits see client addresses. Failed sign-ins are limited per address and per username, and any signed-in user can create accounts (10 an hour per address). Finish setup before exposing the server: `/setup` is open until the first account exists.

## Verification

Run the tests in throwaway containers so the host needs no toolchain:

```sh
docker run --rm -v "$PWD":/src:ro -e CARGO_TARGET_DIR=/tmp/target rust:1.88-slim sh -c \
  'apt-get update -qq && apt-get install -y -qq ffmpeg >/dev/null && rustup component add rustfmt clippy >/dev/null && cp -r /src /work && cd /work && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test'
docker run --rm -v "$PWD":/src:ro node:22.23.4-bookworm-slim sh -c \
  'cp -r /src /work && cd /work && npm ci && npm test && npm run typecheck && npm run build:web'
```

Then run the stack, connect a provider, visit `/healthz`, add a recipe, edit and reorder its blocks, and import a public video such as `https://www.instagram.com/p/DZNQT3Pt3Ja/`.
