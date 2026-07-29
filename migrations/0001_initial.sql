PRAGMA foreign_keys = ON;

CREATE TABLE recipes (
  id TEXT PRIMARY KEY NOT NULL,
  title TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  servings INTEGER,
  prep_minutes INTEGER,
  cook_minutes INTEGER,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE recipe_blocks (
  id TEXT PRIMARY KEY NOT NULL,
  recipe_id TEXT NOT NULL REFERENCES recipes(id) ON DELETE CASCADE,
  section TEXT NOT NULL CHECK(section IN ('ingredient', 'step')),
  position INTEGER NOT NULL,
  text TEXT NOT NULL DEFAULT '',
  quantity TEXT NOT NULL DEFAULT '',
  unit TEXT NOT NULL DEFAULT '',
  optional INTEGER NOT NULL DEFAULT 0 CHECK(optional IN (0, 1)),
  UNIQUE(recipe_id, section, position)
);
CREATE INDEX recipe_blocks_recipe_section_position ON recipe_blocks(recipe_id, section, position);

CREATE TABLE recipe_sources (
  id TEXT PRIMARY KEY NOT NULL,
  recipe_id TEXT NOT NULL REFERENCES recipes(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  url TEXT NOT NULL,
  UNIQUE(recipe_id, position),
  UNIQUE(recipe_id, url)
);

CREATE TABLE ai_drafts (
  id TEXT PRIMARY KEY NOT NULL,
  recipe_id TEXT REFERENCES recipes(id) ON DELETE CASCADE,
  operation TEXT NOT NULL CHECK(operation IN ('generate', 'alter')),
  recipe_json TEXT NOT NULL,
  sources_json TEXT NOT NULL,
  search_suggestions TEXT NOT NULL DEFAULT '',
  base_updated_at TEXT,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL
);
CREATE INDEX ai_drafts_expires_at ON ai_drafts(expires_at);
