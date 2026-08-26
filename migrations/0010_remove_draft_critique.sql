-- The optional flavour review pass was removed; discard its persisted draft metadata.
ALTER TABLE ai_drafts DROP COLUMN critique_json;
