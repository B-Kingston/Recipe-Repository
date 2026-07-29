-- Rich cooking-flow metadata is deliberately separate from the editable recipe
-- blocks.  Editing ingredients or method blocks clears it and uses the safe
-- linear fallback until an AI draft creates a new graph.
ALTER TABLE recipes ADD COLUMN chart_json TEXT NOT NULL DEFAULT '';
