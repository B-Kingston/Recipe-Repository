CREATE TABLE recipe_versions (
  id TEXT PRIMARY KEY NOT NULL,
  recipe_id TEXT NOT NULL REFERENCES recipes(id) ON DELETE CASCADE,
  version INTEGER NOT NULL CHECK(version > 0),
  label TEXT NOT NULL DEFAULT '',
  snapshot TEXT NOT NULL,
  created_at TEXT NOT NULL,
  UNIQUE(recipe_id, version)
);

-- Keep public version numbers monotonic even when the latest visible version
-- is deleted, so an old version URL can never later point at different data.
CREATE TABLE recipe_version_counters (
  recipe_id TEXT PRIMARY KEY NOT NULL REFERENCES recipes(id) ON DELETE CASCADE,
  last_version INTEGER NOT NULL CHECK(last_version > 0)
);
