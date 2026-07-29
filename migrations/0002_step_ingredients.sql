CREATE TABLE recipe_step_ingredients (
  id TEXT PRIMARY KEY NOT NULL,
  step_id TEXT NOT NULL REFERENCES recipe_blocks(id) ON DELETE CASCADE,
  position INTEGER NOT NULL,
  text TEXT NOT NULL,
  UNIQUE(step_id, position)
);

CREATE INDEX recipe_step_ingredients_step_position
  ON recipe_step_ingredients(step_id, position);
