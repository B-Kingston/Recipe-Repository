# Ask end-to-end corpus

Drives the recipe chat (the **Ask** drawer) on a running deployment over plain HTTP, the way the drawer's forms do, and checks what each step leaves behind: the 303 and its one-time notice, the drawer, the recipe's stored content (read from the card's edit forms, with block ids), its version tabs, and its notes.

It has no dependencies beyond Node 22 (`fetch`, `FormData`, and type stripping run `.ts` directly). Run it in the project's Node image, like everything else here.

## What a run does to the deployment

- Signs in as the test account (and, for the privacy case, a second account), and signs both out at the end.
- Makes **one throwaway recipe**. Recipes can only be added through AI, so this costs one generation (`POST /ai/generate`, then Save). The run then rewrites the recipe by hand into a fixed test recipe (`TEST_RECIPE` in `cases.ts`), so the cases know what is in it.
- Adds a note with a small photo to that recipe, edits it by hand, and asks the chat about 16 questions, accepting or denying the proposals.
- Deletes the recipe at the end, which removes its chat thread, notes, photos, and versions too. If the drawer was closed for the account before the run, it closes it again. Pass `E2E_KEEP=1` to keep the recipe for a look.
- If a run is killed, delete the recipe titled `E2E Ask <run id>` by hand.

Credentials come only from the environment or a file. They are never printed, and the client never logs request bodies or cookies.

## Running it

```sh
# Signed-out checks only (no credentials needed): /healthz, sign-in redirects, every chat route protected.
docker run --rm --network host -v "$PWD":/app:ro -w /app \
  -e E2E_BASE_URL=http://minibuntu:3000 \
  node:22.23.4-bookworm-slim node e2e/ask/run.ts

# Everything: put the credentials in a file only you can read.
#   E2E_USERNAME=…        E2E_PASSWORD=…
#   E2E_USERNAME_2=…      E2E_PASSWORD_2=…      (optional: the privacy case)
docker run --rm --network host -v "$PWD":/app:ro -v "$HOME/.ask-e2e.env":/creds.env:ro -w /app \
  -e E2E_BASE_URL=http://minibuntu:3000 -e E2E_CREDENTIALS_FILE=/creds.env \
  node:22.23.4-bookworm-slim node e2e/ask/run.ts

# A few cases (setup still makes the recipe):
  … -e E2E_CASES=plain-question,edit-scale,deny …
```

Type check: `docker run --rm -v "$PWD":/app -v recipe-node-modules:/app/node_modules -w /app node:22.23.4-bookworm-slim npx tsc -p e2e/ask`.

| Variable | Default | |
|---|---|---|
| `E2E_BASE_URL` | `http://minibuntu:3000` | The deployment. |
| `E2E_USERNAME`, `E2E_PASSWORD` | | The test account. Without them only the signed-out checks run. |
| `E2E_USERNAME_2`, `E2E_PASSWORD_2` | | A second account, for `privacy`; skipped without. |
| `E2E_CREDENTIALS_FILE` | | A file of `KEY=VALUE` lines with the names above, or JSON `{username, password, username2, password2}`. The environment wins. |
| `E2E_CASES` / `E2E_SKIP` | all / none | Comma lists of case names. Opt-in cases run only when named. |
| `E2E_EXPECT_SEARCH` | `1` | `web-search` requires a source URL. Set `0` on a deployment with no web search for its model (a pi-ai endpoint with no search key, or `PI_SEARCH_ENABLED=false`). |
| `E2E_ANSWER_TIMEOUT_MS` | `240000` | How long one answer may take. |
| `E2E_GENERATE_TIMEOUT_MS` | `360000` | How long making the test recipe may take. |
| `E2E_KEEP` | `0` | Keep the test recipe. |
| `E2E_SCRATCH_RECIPE_ID` | | Use this recipe instead of generating one. **It is overwritten and then deleted.** For local runs without an AI login. |
| `E2E_REPORT` | | Also write the outcomes as JSON to this path. |

Exit code: 0 when every check passed, 1 when any failed (including cleanup), 2 for a refused sign-in or bad configuration.

## Reading the results

Model output varies, so the checks are split:

- **Hard checks** fail the case. They cover what the app guarantees, or what the system prompt requires: statuses, notices, Accept/Deny forms, the recipe saved exactly once, one divider and one `Chat: …` version per Accept, denied and stale proposals changing nothing, kept rows, and 404s for another account. They also cover facts the model can only know by using its tools. For example, the note holds a made-up word (`Kalamansi`), and the previous version's spice was replaced by hand, so an answer that names it must have read the note or opened the version.
- **Soft checks** print `warn:` and do not fail. They cover what a good answer would very likely say, such as 160°F for eggs, the photo being red, or 800 g of tomatoes becoming 1200 g.

The chat's tool calls do not show over HTTP. To see which tools ran, read the app log: `docker compose logs recipes | grep "chat tool call"`. Each call is logged with its name and arguments, never the search key or photo bytes.

## The cases

| Case | Model | What it proves |
|---|---|---|
| (signed out) | no | `/healthz` says `ok`. A signed-out GET goes to `/login?next=…` (htmx gets `HX-Redirect`). Every chat and search-key POST route goes to `/login`. |
| `routes` | no | Signed in, every chat route exists: a GET on a POST-only route answers 405, an unknown path 404. Unknown message ids are quiet no-ops. Over-long and empty questions answer 400. A foreign `back` is not followed. Settings never prints a search key. |
| `no-ai-login` | no (opt-in) | Without an AI login, the question is stored and its answer fails as `needs_settings` with a Settings link. Polling stops and nothing is proposed. |
| `plain-question` | yes | The question is stored at once. The drawer polls with the box disabled. The answer lands as text with Keep as note and no proposal. |
| `no-edit-for-a-question` | yes | "Can I use olive oil?" gets no proposal, and the recipe is unchanged. |
| `keep-as-note` | no | Keep as note makes one whole-recipe note marked From the chat. The answer shows Kept in notes. Keeping it twice adds nothing. |
| `notes-and-photo` | yes | `recipe_notes` is read (the answer names the note's made-up word). `note_photo` is soft-checked (red), since it needs a vision model. |
| `version-lookup` | yes | `recipe_version` is read: after a hand edit, the answer names the spice the previous version used. |
| `web-search` | yes | The model searches and ends with a source URL (hosted search, or the search-key tool on pi-ai models). |
| `edit-scale` | yes | The proposal changes servings and amounts. A second question meanwhile gets `chat_busy`. Accept saves servings 6, keeps the ingredient rows, and adds one divider and one `Chat:` version. A second Accept is a no-op. |
| `edit-vegan` | yes | The proposal and Accept leave no unqualified animal products. Untouched rows are kept. |
| `edit-remove-step` | yes | One fewer step, step 2's row is gone, and the others keep their rows. |
| `edit-add-step-end` | yes | One more step, at the end. |
| `edit-add-step-start` | yes | A step added before the others saves, and the others move down. **Fails today**: Accept answers 500 (UNIQUE `recipe_blocks(recipe_id, section, position)`). |
| `edit-reorder` | yes | A pure move saves the new order on the same rows. **Fails today**, same cause. |
| `search-then-edit` | yes | A turn that searches and then edits completes, so the provider accepts the follow-up request after a hosted-search round. pi-ai drops `web_search_call` and `server_tool_use` items from that request, and this case is the live check of whether Codex or Anthropic reject it. |
| `deny` | yes | Deny shows the notice and marks the proposal denied. The recipe, versions, and dividers are untouched, and a later Accept does nothing. |
| `stale` | yes | After a hand edit, Accept is hidden and Dismiss offered. A forced Accept gets `change_stale`. The hand edit stands. |
| `privacy` | yes | A second account gets 404 for the page and for asking, accept, deny, keep, clear, and the drawer, and does not see the recipe in its library. The owner's proposal stays ready. |
| `clear` | no | The thread is emptied, and the drawer shows its empty state and notice. |

## Cost

One recipe generation plus about 16 chat turns. A turn can take several model rounds (lookups, edits, up to 8) and a few searches, so a full run is roughly 20 to 60 provider requests and 10 to 25 minutes. Codex or Claude subscriptions count against their usage windows. API keys are billed per token, and the search key per query (`web-search`, `search-then-edit`, and any search the model chooses). Run single cases with `E2E_CASES` while iterating.
