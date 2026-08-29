-- Pending dish-photo candidates for an already-saved recipe. A "pick new
-- frames" run stores its choices here until the user picks one (which copies
-- the JPEG onto recipes.thumbnail_jpeg), discards them, or replaces the set
-- with a newer run.
CREATE TABLE recipe_thumbnail_candidates (
    recipe_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    seconds INTEGER NOT NULL,
    jpeg BLOB NOT NULL,
    PRIMARY KEY (recipe_id, idx)
);
