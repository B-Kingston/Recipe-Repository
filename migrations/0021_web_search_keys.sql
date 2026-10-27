-- A search API key the recipe chat uses to search the web for models that
-- have no web search of their own. One per account; it belongs to the AI
-- setup, so an account sharing another's AI logins uses that account's key.
CREATE TABLE web_search_keys (
  user_id TEXT PRIMARY KEY NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  -- A search API the worker has an adapter for (WEB_SEARCH_PROVIDERS in
  -- main.rs, SEARCH_PROVIDERS in pi/web-search.ts); checked by the app, so a
  -- new provider needs no migration.
  provider TEXT NOT NULL,
  api_key TEXT NOT NULL CHECK(api_key <> ''),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
