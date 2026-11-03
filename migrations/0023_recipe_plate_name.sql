-- A one- or two-word name for the dish ("Roti", "Pad thai") that the recipe
-- page's plate shows in its well when there is no dish photo. The model
-- writes it; empty falls back to the title's initial.
ALTER TABLE recipes ADD COLUMN plate_name TEXT NOT NULL DEFAULT '';
