-- The cook's notes on a recipe: text, an optional photo, or both. A note
-- belongs to the recipe as a whole and may point at one saved version.
CREATE TABLE recipe_notes (
  id TEXT PRIMARY KEY NOT NULL,
  user_id TEXT NOT NULL,
  recipe_id TEXT NOT NULL REFERENCES recipes(id) ON DELETE CASCADE,
  -- The recipe version the note is about, or NULL for the whole recipe. A
  -- number rather than a key: version numbers are never reused, so a note
  -- still names the version after that version is deleted.
  version INTEGER CHECK(version IS NULL OR version > 0),
  body TEXT NOT NULL DEFAULT '',
  -- A photo normalised to a JPEG of at most 1280 px on its longer side.
  photo_jpeg BLOB,
  -- The chat answer the note was kept from, if any.
  chat_message_id TEXT REFERENCES recipe_chat_messages(id) ON DELETE SET NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  CHECK(body <> '' OR photo_jpeg IS NOT NULL)
);
CREATE INDEX recipe_notes_recipe ON recipe_notes(user_id, recipe_id);
