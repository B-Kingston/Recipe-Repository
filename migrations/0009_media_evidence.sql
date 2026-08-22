-- Bounded textual provenance from a social-video import. The downloaded media
-- is temporary and never stored in SQLite; this lets an expiring draft show
-- which description, local transcript, and OCR evidence informed it.
ALTER TABLE ai_drafts ADD COLUMN evidence_json TEXT NOT NULL DEFAULT '';
