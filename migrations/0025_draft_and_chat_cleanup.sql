-- Indexes on foreign-key columns whose parent rows get deleted. Without one,
-- every deleted recipe or chat message makes SQLite scan the whole child
-- table, note photos included. (The (user_id, recipe_id) indexes do not
-- serve a lookup by recipe_id alone.)
CREATE INDEX recipe_notes_chat_message ON recipe_notes(chat_message_id);
CREATE INDEX recipe_notes_recipe_id ON recipe_notes(recipe_id);
CREATE INDEX recipe_chat_messages_recipe ON recipe_chat_messages(recipe_id);

-- The chat proposes edits for the cook to accept; it no longer drafts them.
-- A change left drafting by an older version failed, and the instruction a
-- draft was made from is never read.
UPDATE recipe_chat_messages SET change_state = 'failed' WHERE change_state = 'drafting';
ALTER TABLE recipe_chat_messages DROP COLUMN change_request;

-- A draft only ever becomes a new recipe: an alteration makes another draft,
-- and the chat edits saved recipes itself. So ai_drafts loses the columns for
-- altering a saved recipe (recipe_id, base_updated_at), the operation, the
-- search suggestions nothing wrote, and the chat link nothing reads. Drafts
-- that altered a saved recipe go with them, and their dish-photo candidates
-- (draft_thumbnails has no foreign key).
DELETE FROM draft_thumbnails
  WHERE draft_id IN (SELECT id FROM ai_drafts WHERE recipe_id IS NOT NULL);

-- SQLite cannot drop a foreign-key column, so the table is rebuilt. Dropping
-- the old table first deletes its rows, which cascades into each draft's
-- moment pool and step moments, so those are set aside and put back.
CREATE TEMP TABLE kept_draft_moment_vectors AS SELECT * FROM draft_moment_vectors;
CREATE TEMP TABLE kept_draft_moment_stills AS SELECT * FROM draft_moment_stills;
CREATE TEMP TABLE kept_draft_step_moments AS SELECT * FROM draft_step_moments;

CREATE TABLE ai_drafts_new (
  id TEXT PRIMARY KEY NOT NULL,
  user_id TEXT NOT NULL DEFAULT '',
  recipe_json TEXT NOT NULL,
  sources_json TEXT NOT NULL,
  evidence_json TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL
);
INSERT INTO ai_drafts_new (id, user_id, recipe_json, sources_json, evidence_json, created_at, expires_at)
  SELECT id, user_id, recipe_json, sources_json, evidence_json, created_at, expires_at
  FROM ai_drafts
  WHERE recipe_id IS NULL;
DROP TABLE ai_drafts;
ALTER TABLE ai_drafts_new RENAME TO ai_drafts;
CREATE INDEX ai_drafts_expires_at ON ai_drafts(expires_at);

-- Emptied first in case foreign keys were off and nothing cascaded.
DELETE FROM draft_moment_vectors;
DELETE FROM draft_moment_stills;
DELETE FROM draft_step_moments;
INSERT INTO draft_moment_vectors
  SELECT * FROM kept_draft_moment_vectors WHERE draft_id IN (SELECT id FROM ai_drafts);
INSERT INTO draft_moment_stills
  SELECT * FROM kept_draft_moment_stills WHERE draft_id IN (SELECT id FROM ai_drafts);
INSERT INTO draft_step_moments
  SELECT * FROM kept_draft_step_moments WHERE draft_id IN (SELECT id FROM ai_drafts);
DROP TABLE kept_draft_moment_vectors;
DROP TABLE kept_draft_moment_stills;
DROP TABLE kept_draft_step_moments;
