-- The cook's own filing of a recipe in the library's Folders and Cuisines
-- views: a dish folder slug and a cuisine slug ("other" for none). Empty
-- leaves the recipe to EmbeddingGemma, which files it automatically.
ALTER TABLE recipes ADD COLUMN folder TEXT NOT NULL DEFAULT '';
ALTER TABLE recipes ADD COLUMN cuisine TEXT NOT NULL DEFAULT '';
