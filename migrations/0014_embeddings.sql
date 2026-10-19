-- EmbeddingGemma 2 features. The vectors come from the embedding sidecar
-- (llama.cpp llama-server) and are stored as little-endian f32 BLOBs.

-- Library search: one document vector per recipe. `document` is the exact
-- text that was embedded, so a changed recipe is detected by comparing the
-- freshly built document with the stored one.
CREATE TABLE recipe_embeddings (
    recipe_id TEXT PRIMARY KEY REFERENCES recipes(id) ON DELETE CASCADE,
    document TEXT NOT NULL,
    vector BLOB NOT NULL,
    updated_at TEXT NOT NULL
);

-- Video moments. A video import embeds 12 s audio windows and sampled
-- frames, and keeps small stills every two seconds. The pool rides with the
-- expiring draft, moves onto the recipe when the draft is applied, and lets
-- later step edits and AI alterations re-match their steps to the video.
CREATE TABLE draft_moment_vectors (
    draft_id TEXT NOT NULL REFERENCES ai_drafts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('audio', 'frame')),
    seconds REAL NOT NULL,
    vector BLOB NOT NULL
);
CREATE INDEX draft_moment_vectors_draft ON draft_moment_vectors(draft_id);

CREATE TABLE draft_moment_stills (
    draft_id TEXT NOT NULL REFERENCES ai_drafts(id) ON DELETE CASCADE,
    seconds INTEGER NOT NULL,
    jpeg BLOB NOT NULL,
    PRIMARY KEY (draft_id, seconds)
);

-- The matched moment of each draft step (0-based step index).
CREATE TABLE draft_step_moments (
    draft_id TEXT NOT NULL REFERENCES ai_drafts(id) ON DELETE CASCADE,
    step_index INTEGER NOT NULL,
    seconds INTEGER NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('audio', 'frame')),
    PRIMARY KEY (draft_id, step_index)
);

CREATE TABLE recipe_moment_vectors (
    recipe_id TEXT NOT NULL REFERENCES recipes(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('audio', 'frame')),
    seconds REAL NOT NULL,
    vector BLOB NOT NULL
);
CREATE INDEX recipe_moment_vectors_recipe ON recipe_moment_vectors(recipe_id);

CREATE TABLE recipe_moment_stills (
    recipe_id TEXT NOT NULL REFERENCES recipes(id) ON DELETE CASCADE,
    seconds INTEGER NOT NULL,
    jpeg BLOB NOT NULL,
    PRIMARY KEY (recipe_id, seconds)
);

-- The matched moment of each saved step. Keyed by step block, so moving a
-- step keeps its moment and deleting it removes the moment.
CREATE TABLE recipe_step_moments (
    step_id TEXT PRIMARY KEY REFERENCES recipe_blocks(id) ON DELETE CASCADE,
    seconds INTEGER NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('audio', 'frame'))
);
