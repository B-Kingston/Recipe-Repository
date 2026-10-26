-- Recipe chat: one private conversation per (user, recipe). A row is a
-- question ('user'), the assistant's answer ('assistant'), or a divider
-- ('change') marking where a change from the chat was saved to the recipe.
-- Rows read in insertion order (rowid), with created_at for display.
CREATE TABLE recipe_chat_messages (
  id TEXT PRIMARY KEY NOT NULL,
  user_id TEXT NOT NULL,
  recipe_id TEXT NOT NULL REFERENCES recipes(id) ON DELETE CASCADE,
  role TEXT NOT NULL CHECK(role IN ('user', 'assistant', 'change')),
  -- The question, the answer, an error to show, or the divider's label.
  content TEXT NOT NULL DEFAULT '',
  -- An answer is 'pending' while the model runs; 'failed' shows content as
  -- the error, and 'needs_settings' also links to Settings (no AI login, or
  -- a refused one). Questions and dividers are always 'done'.
  status TEXT NOT NULL DEFAULT 'done'
    CHECK(status IN ('pending', 'done', 'failed', 'needs_settings')),
  -- A change the cook asked for: the instruction the draft is made from, and
  -- the model's 1-2 word name for it, which labels the divider once saved.
  change_request TEXT NOT NULL DEFAULT '',
  change_label TEXT NOT NULL DEFAULT '',
  -- '' (no change), 'drafting', 'ready' (a draft awaits review), 'applied',
  -- 'discarded', or 'failed' (the draft could not be made).
  change_state TEXT NOT NULL DEFAULT ''
    CHECK(change_state IN ('', 'drafting', 'ready', 'applied', 'discarded', 'failed')),
  created_at TEXT NOT NULL
);
CREATE INDEX recipe_chat_thread ON recipe_chat_messages(user_id, recipe_id);

-- The chat answer a draft was made for, so saving it marks the thread. Chat
-- drafts are ordinary 'alter' drafts otherwise. Alteration chains carry the
-- link along; clearing the chat leaves the draft.
ALTER TABLE ai_drafts ADD COLUMN chat_message_id TEXT
  REFERENCES recipe_chat_messages(id) ON DELETE SET NULL;
